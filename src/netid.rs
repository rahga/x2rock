//! Identifying which network we are attached to.
//!
//! A cached player address is only meaningful on the network it was found on, and
//! this laptop moves. SSIDs and RFC1918 subnets collide constantly - half the world
//! is `192.168.1.0/24` on an SSID called `guest` - so the default gateway's MAC is
//! used instead. It is stable per site and effectively unique.

use std::fs;
use std::net::Ipv4Addr;

use anyhow::{Result, anyhow};

/// Default IPv4 gateway, parsed from a routing table in `/proc/net/route` format.
///
/// `/proc/net/route` is little-endian hex, one route per line.
fn default_gateway_from(table: &str) -> Result<Ipv4Addr> {
    for line in table.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (_iface, destination, gateway, flags) =
            (fields.next(), fields.next(), fields.next(), fields.next());
        let (Some(destination), Some(gateway)) = (destination, gateway) else {
            continue;
        };
        // Destination 0.0.0.0 marks a default route.
        // A gateway of 0.0.0.0 is an unrouted/on-link entry, not a reachable gateway.
        if destination == "00000000" && gateway != "00000000" {
            // RTF_GATEWAY flag is 0x0002. If flags are present, ensure it's marked as a gateway.
            if let Some(flags) = flags {
                let flags = u16::from_str_radix(flags, 16).unwrap_or(0);
                if flags & 0x0002 == 0 {
                    continue;
                }
            }
            let raw = u32::from_str_radix(gateway, 16)?;
            return Ok(Ipv4Addr::from(raw.swap_bytes()));
        }
    }
    Err(anyhow!("no default route"))
}

fn default_gateway() -> Result<Ipv4Addr> {
    let table = fs::read_to_string("/proc/net/route")?;
    default_gateway_from(&table)
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
    fn skips_dummy_default_routes_and_finds_gateway() {
        let table = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
dummy0\t00000000\t00000000\t0001\t0\t0\t50\t00000000\t0\t0\t0
tun0\t00000000\t0100A8C0\t0001\t0\t0\t50\t00000000\t0\t0\t0
wlan0\t00000000\t0156A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
";
        let gw = default_gateway_from(table).unwrap();
        assert_eq!(gw, Ipv4Addr::new(192, 168, 86, 1));
    }

    /// Reads the real routing table, so it needs a machine with a default
    /// route; a network-less container or CI runner has none. Run with
    /// `cargo test -- --ignored` on a connected machine.
    #[test]
    #[ignore = "needs a real default route; fails in a network-less container"]
    fn finds_this_machines_gateway() {
        // Not asserting a value - just that parsing the real table does not error.
        assert!(default_gateway().is_ok(), "should find a default route");
    }
}
