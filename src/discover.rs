//! Finding a player.
//!
//! Multicast discovery (SSDP/mDNS) is not dependable here: a default-deny inbound
//! firewall - which Omarchy ships - silently drops the replies, because they arrive
//! from the player's unicast address and match no conntrack entry. So this scans
//! outbound TCP instead, which is unaffected.
//!
//! Reaching any single player is enough: `getGroups` then reports every other
//! player's address.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::net::TcpStream;

use crate::sonos::local::PORT;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
/// Deliberately modest: this runs on other people's networks.
const CONCURRENCY: usize = 32;
/// Refuse to sweep more than this many addresses unattended. A /22 is already
/// ~13s; a /16 would be a quarter of an hour and would look like a port scan.
const MAX_HOSTS: u32 = 1024;

/// The network this machine is attached to.
pub struct LocalNetwork {
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
}

impl LocalNetwork {
    pub fn prefix_len(&self) -> u32 {
        u32::from(self.netmask).count_ones()
    }

    /// Usable addresses in the subnet, without materialising them. Zero for
    /// /31 and /32, which have no host range worth walking.
    fn host_count(&self) -> u32 {
        let total = (!u32::from(self.netmask)).wrapping_add(1);
        if total < 4 { 0 } else { total - 2 }
    }

    /// Addresses in the subnet, excluding network, broadcast and ourselves.
    fn hosts(&self) -> Vec<Ipv4Addr> {
        let mask = u32::from(self.netmask);
        let base = u32::from(self.ip) & mask;
        let total = (!mask).wrapping_add(1);

        if self.host_count() == 0 {
            return vec![];
        }
        (1..total - 1)
            .map(|offset| Ipv4Addr::from(base + offset))
            .filter(|&ip| ip != self.ip)
            .collect()
    }

    /// The /24 around this machine, used when the real subnet is too large to sweep.
    fn narrowed_to_24(&self) -> Self {
        Self {
            ip: self.ip,
            netmask: Ipv4Addr::new(255, 255, 255, 0),
        }
    }
}

/// Our address on the network we are attached to, learned without sending anything.
fn local_ipv4() -> Result<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("203.0.113.1:80")?;
    match socket.local_addr()?.ip() {
        IpAddr::V4(v4) => Ok(v4),
        IpAddr::V6(_) => Err(anyhow!("no IPv4 address on this network")),
    }
}

/// Read the real netmask from the interface holding our address, rather than
/// assuming a /24 - corporate networks are frequently /22 or wider.
pub fn local_network() -> Result<LocalNetwork> {
    let ip = local_ipv4()?;
    for interface in if_addrs::get_if_addrs()? {
        if let if_addrs::IfAddr::V4(v4) = interface.addr
            && v4.ip == ip
        {
            return Ok(LocalNetwork {
                ip,
                netmask: v4.netmask,
            });
        }
    }
    Err(anyhow!("no interface found holding our address {ip}"))
}

async fn responds(ip: Ipv4Addr) -> Option<Ipv4Addr> {
    let addr = SocketAddr::from((ip, PORT));
    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(_)) => Some(ip),
        _ => None,
    }
}

/// What a scan actually covered, so callers can be honest about it.
pub struct Scan {
    pub found: Vec<Ipv4Addr>,
    pub scanned: u32,
    /// Set when the real subnet was too large and the sweep was narrowed.
    pub narrowed_from: Option<u32>,
}

/// Sweep the local subnet for anything listening on the Sonos LAN API port.
///
/// `stop_early` returns after the first hit, which is all that is needed to
/// bootstrap - the rest come from `getGroups`.
pub async fn scan_local_subnet(stop_early: bool) -> Result<Scan> {
    let network = local_network()?;
    let full_prefix = network.prefix_len();

    let (network, narrowed_from) = if network.host_count() > MAX_HOSTS {
        (network.narrowed_to_24(), Some(full_prefix))
    } else {
        (network, None)
    };

    let hosts = network.hosts();
    let scanned = hosts.len() as u32;
    let mut found = Vec::new();

    for chunk in hosts.chunks(CONCURRENCY) {
        let probes: Vec<_> = chunk.iter().map(|&ip| tokio::spawn(responds(ip))).collect();
        for probe in probes {
            if let Ok(Some(ip)) = probe.await {
                found.push(ip);
                if stop_early {
                    return Ok(Scan {
                        found,
                        scanned,
                        narrowed_from,
                    });
                }
            }
        }
    }
    found.sort();
    Ok(Scan {
        found,
        scanned,
        narrowed_from,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ip: [u8; 4], mask: [u8; 4]) -> LocalNetwork {
        LocalNetwork {
            ip: Ipv4Addr::from(ip),
            netmask: Ipv4Addr::from(mask),
        }
    }

    #[test]
    fn prefix_len_from_netmask() {
        assert_eq!(net([192, 168, 77, 27], [255, 255, 255, 0]).prefix_len(), 24);
        assert_eq!(net([10, 1, 2, 3], [255, 255, 252, 0]).prefix_len(), 22);
        assert_eq!(net([10, 1, 2, 3], [255, 255, 0, 0]).prefix_len(), 16);
    }

    #[test]
    fn slash_24_excludes_network_broadcast_and_self() {
        let hosts = net([192, 168, 77, 27], [255, 255, 255, 0]).hosts();
        assert_eq!(hosts.len(), 253, "254 usable minus ourselves");
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 77, 1)));
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 77, 254)));
        assert!(
            !hosts.contains(&Ipv4Addr::new(192, 168, 77, 0)),
            "network address"
        );
        assert!(
            !hosts.contains(&Ipv4Addr::new(192, 168, 77, 255)),
            "broadcast"
        );
        assert!(
            !hosts.contains(&Ipv4Addr::new(192, 168, 77, 27)),
            "ourselves"
        );
    }

    #[test]
    fn slash_22_spans_all_four_c_blocks() {
        let network = net([10, 1, 5, 9], [255, 255, 252, 0]);
        let hosts = network.hosts();
        assert_eq!(hosts.len(), 1021, "1022 usable minus ourselves");
        assert!(
            hosts.contains(&Ipv4Addr::new(10, 1, 4, 1)),
            "starts at .4.1"
        );
        assert!(
            hosts.contains(&Ipv4Addr::new(10, 1, 7, 254)),
            "ends at .7.254"
        );
        assert!(!hosts.contains(&Ipv4Addr::new(10, 1, 4, 0)));
        assert!(!hosts.contains(&Ipv4Addr::new(10, 1, 7, 255)));
    }

    #[test]
    fn oversized_networks_narrow_to_a_24() {
        let wide = net([10, 1, 2, 3], [255, 255, 0, 0]);
        assert!(wide.host_count() > MAX_HOSTS);
        assert_eq!(wide.host_count(), 65_534);

        let narrowed = wide.narrowed_to_24();
        assert_eq!(narrowed.prefix_len(), 24);
        assert_eq!(narrowed.hosts().len(), 253);
        assert!(narrowed.hosts().contains(&Ipv4Addr::new(10, 1, 2, 200)));
        assert!(!narrowed.hosts().contains(&Ipv4Addr::new(10, 1, 3, 1)));
    }

    #[test]
    fn tiny_networks_yield_no_hosts() {
        assert!(
            net([10, 0, 0, 1], [255, 255, 255, 254]).hosts().is_empty(),
            "/31"
        );
        assert!(
            net([10, 0, 0, 1], [255, 255, 255, 255]).hosts().is_empty(),
            "/32"
        );
    }
}
