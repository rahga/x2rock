//! What x2rock remembers between runs: which players live on which network.
//!
//! Stored under `$XDG_STATE_HOME/x2rock/` rather than the config directory - it
//! is regenerable, machine-discovered state, not something the user wrote. Keyed by
//! network fingerprint, because this laptop moves and a player address is only
//! meaningful on the network it was found on.

use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::sonos::proto::Groups;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Network fingerprint -> household id -> players seen there.
    #[serde(default)]
    pub networks: BTreeMap<String, BTreeMap<String, Vec<KnownPlayer>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownPlayer {
    pub id: String,
    pub name: String,
    pub ip: IpAddr,
}

fn path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "x2rock")
        .ok_or_else(|| anyhow!("no home directory"))?;
    let dir = dirs
        .state_dir()
        .ok_or_else(|| anyhow!("no XDG state directory on this platform"))?;
    Ok(dir.join("networks.json"))
}

impl State {
    /// Load, treating a missing file as empty state.
    pub fn load() -> Result<Self> {
        let path = path()?;
        match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically, so a crash mid-write cannot leave a truncated file.
    pub fn save(&self) -> Result<()> {
        let path = path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }

    /// Record what a household reported about itself. Returns whether anything changed,
    /// so callers can skip the write on the common no-op path.
    pub fn remember(&mut self, fingerprint: &str, household: &str, groups: &Groups) -> bool {
        let mut players: Vec<KnownPlayer> = groups
            .players
            .iter()
            .filter_map(|p| {
                Some(KnownPlayer {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    ip: p.ip()?,
                })
            })
            .collect();
        players.sort_by(|a, b| a.name.cmp(&b.name));

        let slot = self
            .networks
            .entry(fingerprint.to_string())
            .or_default()
            .entry(household.to_string())
            .or_default();
        if *slot == players {
            return false;
        }
        *slot = players;
        true
    }

    /// Every player remembered on a network, across households.
    pub fn players_on(&self, fingerprint: &str) -> Vec<KnownPlayer> {
        self.networks
            .get(fingerprint)
            .map(|households| households.values().flatten().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonos::proto::{Group, Player};

    // `load` and `save` read and write the real XDG state directory, so they are
    // left alone here rather than pointed at a temporary one - the interesting
    // behaviour is `remember` and `players_on`, which are pure over `self`.

    /// A household as `getGroups` would report it - only the players matter
    /// here, since that is all `remember` reads.
    fn household(players: &[(&str, &str, &str)]) -> Groups {
        Groups {
            groups: Vec::<Group>::new(),
            players: players
                .iter()
                .map(|(id, name, url)| Player {
                    id: (*id).into(),
                    name: (*name).into(),
                    websocket_url: (*url).into(),
                    capabilities: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn a_household_is_new_once_and_unchanged_after_that() {
        let mut state = State::default();
        let seen = household(&[("RINCON_1", "Media Room", "wss://192.168.77.94:1443/x")]);

        // The return value is what lets a caller skip the write, so the first
        // sighting has to say "changed" and the identical second must not.
        assert!(state.remember("net-a", "hh:1", &seen));
        assert!(!state.remember("net-a", "hh:1", &seen));
    }

    #[test]
    fn the_order_a_household_reports_in_does_not_count_as_a_change() {
        let mut state = State::default();
        let one = household(&[
            ("RINCON_1", "Media Room", "wss://192.168.77.94:1443/x"),
            ("RINCON_2", "Kitchen", "wss://192.168.77.95:1443/x"),
        ]);
        // The same two players, reported the other way round. Sonos does not
        // promise an order, so without the sort this would rewrite the file on
        // every run and never settle.
        let other = household(&[
            ("RINCON_2", "Kitchen", "wss://192.168.77.95:1443/x"),
            ("RINCON_1", "Media Room", "wss://192.168.77.94:1443/x"),
        ]);

        assert!(state.remember("net-a", "hh:1", &one));
        assert!(!state.remember("net-a", "hh:1", &other));
    }

    #[test]
    fn a_moved_address_is_a_change_worth_writing() {
        let mut state = State::default();
        assert!(state.remember(
            "net-a",
            "hh:1",
            &household(&[("RINCON_1", "Media Room", "wss://192.168.77.94:1443/x")]),
        ));
        // DHCP handed the same speaker a different address - the whole reason
        // this file exists is to be right about that.
        assert!(state.remember(
            "net-a",
            "hh:1",
            &household(&[("RINCON_1", "Media Room", "wss://192.168.77.99:1443/x")]),
        ));
        assert_eq!(
            state.players_on("net-a")[0].ip,
            "192.168.77.99".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn a_player_with_no_usable_address_is_not_remembered() {
        let mut state = State::default();
        state.remember(
            "net-a",
            "hh:1",
            &household(&[
                ("RINCON_1", "Media Room", "wss://192.168.77.94:1443/x"),
                // A hostname rather than an address: nothing to reconnect to
                // later, so remembering it would only produce a slow failure.
                ("RINCON_2", "Kitchen", "wss://kitchen.local:1443/x"),
            ]),
        );

        let known = state.players_on("net-a");
        assert_eq!(known.len(), 1);
        assert_eq!(known[0].name, "Media Room");
    }

    #[test]
    fn one_network_can_hold_several_households() {
        let mut state = State::default();
        state.remember(
            "net-a",
            "hh:1",
            &household(&[("RINCON_1", "Media Room", "wss://192.168.77.94:1443/x")]),
        );
        // A second Sonos household on the same LAN - two systems in one office.
        state.remember(
            "net-a",
            "hh:2",
            &household(&[("RINCON_9", "Studio", "wss://192.168.77.20:1443/x")]),
        );

        let mut names: Vec<_> = state
            .players_on("net-a")
            .into_iter()
            .map(|p| p.name)
            .collect();
        names.sort();
        assert_eq!(names, ["Media Room", "Studio"]);
    }

    #[test]
    fn an_unvisited_network_knows_nothing_which_is_how_it_is_recognised() {
        let mut state = State::default();
        state.remember(
            "net-a",
            "hh:1",
            &household(&[("RINCON_1", "Media Room", "wss://192.168.77.94:1443/x")]),
        );

        // session.rs reads this emptiness as "unregistered network" and refuses
        // to scan on it, so an unvisited fingerprint must come back empty rather
        // than fall through to another network's players.
        assert!(state.players_on("cafe-wifi").is_empty());
    }

    #[test]
    fn state_survives_a_round_trip_and_an_empty_file_is_empty_state() {
        let mut state = State::default();
        state.remember(
            "net-a",
            "hh:1",
            &household(&[("RINCON_1", "Media Room", "wss://192.168.77.94:1443/x")]),
        );

        let text = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&text).unwrap();
        assert_eq!(back.players_on("net-a"), state.players_on("net-a"));

        // `networks` defaults, so a file written by an older version - or an
        // empty object - loads as no players rather than as a parse error the
        // user would have to delete the file to escape.
        let bare: State = serde_json::from_str("{}").unwrap();
        assert!(bare.networks.is_empty());
    }
}
