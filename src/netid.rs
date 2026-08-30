//! Identifying which network we are attached to.
//!
//! A cached player address is only meaningful on the network it was found on, and
//! this laptop moves. SSIDs and RFC1918 subnets collide constantly - half the world
//! is `192.168.1.0/24` on an SSID called `guest` - so the default gateway's MAC is
//! used instead. It is stable per site and effectively unique.

use std::fs;
use std::net::Ipv4Addr;

use anyhow::{Result, anyhow};

/// Default IPv4 gateway, from the kernel routing table.
///
/// `/proc/net/route` is little-endian hex, one route per line.
fn default_gateway() -> Result<Ipv4Addr> {
    let table = fs::read_to_string("/proc/net/route")?;
    for line in table.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (_iface, destination, gateway) = (fields.next(), fields.next(), fields.next());
        let (Some(destination), Some(gateway)) = (destination, gateway) else {
            continue;
        };
        // Destination 0.0.0.0 marks the default route.
        if destination == "00000000" {
            let raw = u32::from_str_radix(gateway, 16)?;
            return Ok(Ipv4Addr::from(raw.swap_bytes()));
        }
    }
    Err(anyhow!("no default route"))
}

/// MAC address of the default gateway, as a stable fingerprint for this network.
///
/// Returns `None` rather than failing: an unidentifiable network is a normal
/// condition, and simply means nothing is cached for it.
pub fn network_fingerprint() -> Option<String> {
    let gateway = default_gateway().ok()?;
    let arp = fs::read_to_string("/proc/net/arp").ok()?;

    for line in arp.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        // IP address, HW type, flags, HW address, ...
        if fields.len() >= 4 && fields[0] == gateway.to_string() {
            let mac = fields[3];
            if mac != "00:00:00:00:00:00" {
                return Some(mac.to_ascii_lowercase());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_parses_as_little_endian_hex() {
        // 0100A8C0 little-endian is 192.168.0.1
        let raw = u32::from_str_radix("0100A8C0", 16).unwrap();
        assert_eq!(
            Ipv4Addr::from(raw.swap_bytes()),
            Ipv4Addr::new(192, 168, 0, 1)
        );
    }

    #[test]
    fn finds_this_machines_gateway() {
        // Not asserting a value - just that parsing the real table does not error.
        assert!(default_gateway().is_ok(), "should find a default route");
    }
}
