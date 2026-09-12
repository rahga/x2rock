//! What x2rock remembers between runs: which players live on which network.
//!
//! Stored under `$XDG_STATE_HOME/x2rock/` rather than the config directory - it
//! is regenerable, machine-discovered state, not something the user wrote. Keyed by
//! network fingerprint, because this laptop moves and a player address is only
//! meaningful on the network it was found on.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::sonos::proto::Groups;
use crate::store;

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
    store::path("networks.json")
}

impl State {
    /// Load, treating a missing file as empty state.
    pub fn load() -> Result<Self> {
        let path = path()?;
        match store::read_optional(&path)? {
            Some(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            None => Ok(Self::default()),
        }
    }

    /// Write atomically, so a crash mid-write cannot leave a truncated file.
    ///
    /// No lock, unlike bookmarks: the daemon and the CLI both write this, but
    /// each writes what a player just told it about the household, so two
    /// writers moments apart agree, and the loser of the race loses nothing
    /// the next `attach` will not put back.
    pub fn save(&self) -> Result<()> {
        store::write_atomically(&path()?, &serde_json::to_string_pretty(self)?, store::PLAIN)
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

    /// Point a remembered player at its new name after a rename.
    ///
    /// Returns whether anything changed, so the caller can skip the write.
    ///
    /// Keyed on the player id rather than the old name, and applied across
    /// every network and household, because the id is what is actually stable:
    /// the same speaker is remembered once per network this machine has seen it
    /// on, and leaving the others holding the old name would offer it as a
    /// completion again the next time the laptop moved.
    ///
    /// [`Self::remember`] would fix this by itself on the next run - it
    /// overwrites the whole list from live state - so this exists only to close
    /// the window in between, during which completions would offer a name that
    /// no longer resolves.
    pub fn rename_player(&mut self, id: &str, new_name: &str) -> bool {
        let mut changed = false;
        for households in self.networks.values_mut() {
            for players in households.values_mut() {
                // An id appears at most once per household, and only the list
                // that actually moved needs re-sorting - `changed` is the
                // return value, so using it to decide that too re-sorted every
                // later household as well.
                let Some(player) = players.iter_mut().find(|p| p.id == id) else {
                    continue;
                };
                if player.name == new_name {
                    continue;
                }
                player.name = new_name.to_string();
                players.sort_by(|a, b| a.name.cmp(&b.name));
                changed = true;
            }
        }
        changed
    }

    /// Every player remembered on a network, across households.
    pub fn players_on(&self, fingerprint: &str) -> Vec<KnownPlayer> {
        self.networks
            .get(fingerprint)
            .map(|households| households.values().flatten().cloned().collect())
            .unwrap_or_default()
    }

    /// Forget, on one network, each remembered household that a completed scan
    /// has just shown to be gone. Returns whether anything changed.
    ///
    /// The evidence required is deliberately specific: a household is
    /// forgotten only when **every address it was remembered at answered the
    /// scan for a different household** - `answering` is every address the
    /// scan got a session from, `seen` every household id it found. That is
    /// exactly a factory reset, a replaced system, or DHCP handing the same
    /// addresses on: the old id would otherwise sit beside the new one with
    /// identical room names and turn every command into `multiple_households`
    /// until someone hand-edits the state file.
    ///
    /// It is *not* "forget whatever did not answer". A household that is
    /// powered off during a rescan has given no evidence of anything, and
    /// forgetting it on a two-household network would leave the next command
    /// attaching to the other household and reporting `unknown_room`, rather
    /// than rescanning when it comes back. Unanswered addresses keep a
    /// household remembered.
    pub fn forget_superseded_households(
        &mut self,
        fingerprint: &str,
        seen: &[&str],
        answering: &[IpAddr],
    ) -> bool {
        let Some(households) = self.networks.get_mut(fingerprint) else {
            return false;
        };
        let before = households.len();
        households.retain(|id, players| {
            seen.contains(&id.as_str())
                || players.is_empty()
                || !players.iter().all(|p| answering.contains(&p.ip))
        });
        households.len() != before
    }

    /// Every household remembered on a network, each with its own players -
    /// the unflattened form of [`Self::players_on`].
    ///
    /// Exists for the sites that must not silently merge two households
    /// sharing a network (an office with two Sonos systems): `players_on`
    /// is fine for "what could this name resolve to," but picking *which*
    /// player to attach to needs to know the households stayed separate.
    pub fn households_on(&self, fingerprint: &str) -> Vec<(String, Vec<KnownPlayer>)> {
        self.networks
            .get(fingerprint)
            .map(|households| {
                households
                    .iter()
                    .map(|(id, players)| (id.clone(), players.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Room / player names remembered for this machine, deduplicated and sorted.
    /// Prefers the current network when a fingerprint is provided and matches;
    /// otherwise returns all players across all networks.
    pub fn room_names(&self, current_network: Option<&str>) -> Vec<String> {
        let mut names: Vec<String> = if let Some(fp) = current_network {
            let on_net = self.players_on(fp);
            if !on_net.is_empty() {
                on_net.into_iter().map(|p| p.name).collect()
            } else {
                self.all_room_names()
            }
        } else {
            self.all_room_names()
        };
        names.sort();
        names.dedup();
        names
    }

    fn all_room_names(&self) -> Vec<String> {
        self.networks
            .values()
            .flat_map(|households| households.values())
            .flatten()
            .map(|p| p.name.clone())
            .collect()
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

    /// The two cases the rule has to tell apart: a household whose addresses
    /// now all belong to someone else (gone - a reset, a replacement, DHCP
    /// moving on) and one that merely did not answer (maybe off - keep it).
    #[test]
    fn a_household_is_forgotten_only_when_its_addresses_answer_for_another() {
        let mut state = State::default();
        let old = household(&[
            ("RINCON_1", "Kitchen", "wss://192.168.77.94:1443/x"),
            ("RINCON_2", "Bedroom", "wss://192.168.77.95:1443/x"),
        ]);
        state.remember("net-a", "hh:OLD", &old);

        // The same two speakers, factory-reset, now report a new household id
        // at the same addresses. A scan sees only hh:NEW.
        let answering: Vec<IpAddr> = ["192.168.77.94", "192.168.77.95"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        assert!(state.forget_superseded_households("net-a", &["hh:NEW"], &answering));
        assert!(
            state.players_on("net-a").is_empty(),
            "hh:OLD should be gone"
        );

        // But a household with one address unanswered is kept: it may be off.
        state.remember("net-a", "hh:OLD", &old);
        let only_one: Vec<IpAddr> = vec!["192.168.77.94".parse().unwrap()];
        assert!(!state.forget_superseded_households("net-a", &["hh:NEW"], &only_one));
        assert_eq!(state.players_on("net-a").len(), 2, "hh:OLD should survive");

        // A household the scan saw is never a candidate, whatever answered.
        assert!(!state.forget_superseded_households("net-a", &["hh:OLD"], &answering));
        // And an unknown network changes nothing rather than erroring.
        assert!(!state.forget_superseded_households("net-none", &["hh:NEW"], &answering));
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

    /// A rename has to reach every network the speaker is remembered on, not
    /// just the one in front of us: a laptop that has seen the same household
    /// from home and from a guest network holds two copies, and the one left
    /// behind would offer the old name back as a completion.
    #[test]
    fn renaming_a_player_reaches_every_network_it_is_remembered_on() {
        let mut state = State::default();
        let same_speaker = household(&[("RINCON_1", "Kitchen", "wss://192.168.77.94:1443/x")]);
        state.remember("net-home", "hh:1", &same_speaker);
        state.remember("net-guest", "hh:1", &same_speaker);

        assert!(state.rename_player("RINCON_1", "Galley"));
        for net in ["net-home", "net-guest"] {
            let names: Vec<_> = state.players_on(net).into_iter().map(|p| p.name).collect();
            assert_eq!(names, ["Galley"], "{net} kept the old name");
        }

        // Idempotent, so a caller can skip the disk write.
        assert!(!state.rename_player("RINCON_1", "Galley"));
        // And an id nobody holds changes nothing rather than erroring.
        assert!(!state.rename_player("RINCON_NOPE", "Somewhere"));
    }

    /// Renamed entries stay sorted, because `remember` writes them sorted and
    /// compares the whole vector to decide whether anything changed - an
    /// out-of-order list would look like a change on every single run.
    #[test]
    fn a_rename_keeps_the_remembered_list_sorted() {
        let mut state = State::default();
        state.remember(
            "net-a",
            "hh:1",
            &household(&[
                ("RINCON_1", "Attic", "wss://192.168.77.94:1443/x"),
                ("RINCON_2", "Basement", "wss://192.168.77.95:1443/x"),
            ]),
        );
        assert!(state.rename_player("RINCON_1", "Zebra"));
        let names: Vec<_> = state
            .players_on("net-a")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Basement", "Zebra"]);
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

        // Unlike `players_on`, `households_on` keeps the two apart - the only
        // way a caller can tell it is looking at two systems, not one.
        let mut households = state.households_on("net-a");
        households.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(households.len(), 2);
        assert_eq!(households[0].0, "hh:1");
        assert_eq!(households[0].1[0].name, "Media Room");
        assert_eq!(households[1].0, "hh:2");
        assert_eq!(households[1].1[0].name, "Studio");
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

    #[test]
    fn room_names_prefers_the_current_network_and_falls_back_to_all() {
        let mut state = State::default();
        state.remember(
            "home",
            "hh:1",
            &household(&[
                ("RINCON_1", "Media Room", "wss://192.168.1.10:1443/x"),
                ("RINCON_2", "Kitchen", "wss://192.168.1.11:1443/x"),
                // Stereo pair or satellite sharing room name:
                ("RINCON_3", "Media Room", "wss://192.168.1.12:1443/x"),
            ]),
        );
        state.remember(
            "office",
            "hh:2",
            &household(&[("RINCON_4", "Conference Room", "wss://10.0.0.5:1443/x")]),
        );

        // Matching current network gives deduplicated, sorted names on that network:
        assert_eq!(
            state.room_names(Some("home")),
            vec!["Kitchen".to_string(), "Media Room".to_string()]
        );

        // Unknown network falls back to all remembered rooms across all networks:
        assert_eq!(
            state.room_names(Some("unknown")),
            vec![
                "Conference Room".to_string(),
                "Kitchen".to_string(),
                "Media Room".to_string()
            ]
        );

        // None falls back to all:
        assert_eq!(
            state.room_names(None),
            vec![
                "Conference Room".to_string(),
                "Kitchen".to_string(),
                "Media Room".to_string()
            ]
        );
    }
}
