//! `network` - each player's link, read from the players' own `/support/review`.
//!
//! A read-only, local dump of how every speaker is on the network: wired or
//! SonosNet or WiFi, at what link speed, on what firmware. It exists mostly for
//! an agent to ingest - `--json` is the whole parsed view - so x2rock reports
//! rather than diagnoses: it pulls the stable fields out of the diagnostics page
//! and leaves the judgement to whoever reads them.
//!
//! **Masked by default.** Because this output is made to be handed off, the
//! hardware identifiers are masked unless `--unredacted` is passed (the caller
//! hands [`run`] `redact = !unredacted`). The household id is dropped earlier,
//! at parse, and is never shown either way.

use anyhow::Result;

use crate::commands::household::{masked, masked_uuid};
use crate::session::Session;
use crate::sonos::upnp::{PlayerNetwork, Upnp};

/// `ZPInfo` keys whose values are hardware identifiers, masked by `--redact` the
/// same way `system` masks them - still comparable, not publishable. The
/// household id is already gone (dropped at parse), so it is not here.
const IDENTIFIERS: &[&str] = &["IPAddress", "MACAddress", "SerialNumber"];

/// The household's network view: one GET to the reached player answers for all.
pub async fn run(session: &Session, json: bool, redact: bool) -> Result<()> {
    let mut players = Upnp::new(session.connection.ip()).network().await?;
    players.sort_by(|a, b| a.zone.cmp(&b.zone));
    if redact {
        players = players.into_iter().map(redacted).collect();
    }
    if json {
        println!("{}", serde_json::to_value(&players)?);
    } else {
        print!("{}", text(&players));
    }
    Ok(())
}

/// Mask the hardware identifiers in place; the household id is never present to
/// mask. `LocalUID` is a `RINCON_…` uuid and takes the uuid mask, which keeps a
/// different tail than a serial would.
fn redacted(mut player: PlayerNetwork) -> PlayerNetwork {
    for key in IDENTIFIERS {
        if let Some(value) = player.info.get_mut(*key) {
            *value = masked(value);
        }
    }
    if let Some(uid) = player.info.get_mut("LocalUID") {
        *uid = masked_uuid(uid);
    }
    player
}

/// One readable block per player; the JSON is the machine view.
fn text(players: &[PlayerNetwork]) -> String {
    if players.is_empty() {
        return "no players answered the network review\n".to_string();
    }
    let get = |p: &PlayerNetwork, key: &str| p.info.get(key).cloned().unwrap_or_default();
    let mut out = String::new();
    for p in players {
        let connection = p
            .wireless
            .get("ConnectionTypeString")
            .or_else(|| p.wireless.get("WifiModeString"))
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        let wired = if p.ports.is_empty() {
            "none".to_string()
        } else {
            p.ports
                .iter()
                .map(|port| {
                    // A down port reports a speed of 0; the word "down" already
                    // says it, so the speed is shown only for an up link.
                    let speed = match (port.up, port.speed.as_deref()) {
                        (true, Some(s)) => format!(" {s}"),
                        _ => String::new(),
                    };
                    format!(
                        "port {} {}{speed}",
                        port.port,
                        if port.up { "up" } else { "down" },
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let firmware = {
            let version = get(p, "SoftwareVersion");
            let series = get(p, "SeriesID");
            let hw = get(p, "HardwareVersion");
            let mut parts = Vec::new();
            if !series.is_empty() {
                parts.push(series);
            }
            if !hw.is_empty() {
                parts.push(format!("hw {hw}"));
            }
            if parts.is_empty() {
                version
            } else {
                format!("{version} ({})", parts.join(", "))
            }
        };
        out.push_str(&format!("{}\n", p.zone));
        out.push_str(&format!("  connection: {connection}\n"));
        out.push_str(&format!("  wired:      {wired}\n"));
        out.push_str(&format!("  firmware:   {firmware}\n"));
        let address = get(p, "IPAddress");
        let mac = get(p, "MACAddress");
        let serial = get(p, "SerialNumber");
        if !address.is_empty() || !mac.is_empty() || !serial.is_empty() {
            out.push_str(&format!(
                "  address:    {address}  mac {mac}  serial {serial}\n"
            ));
        }
    }
    out
}
