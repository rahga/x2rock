//! Getting from "the user ran a command" to a live connection and a known household.
//!
//! Shared by the CLI and the daemon, so both find players the same way.

use std::net::IpAddr;

use anyhow::{Result, bail};

use crate::discover;
use crate::netid;
use crate::sonos::local::Connection;
use crate::sonos::proto::{Groups, Player};
use crate::state::State;

/// A live connection together with the household topology it reported.
pub struct Session {
    pub connection: Connection,
    pub groups: Groups,
}

/// Connect to a player, learn the household from it, and remember what it said.
pub async fn attach(ip: IpAddr, state: &mut State, fingerprint: Option<&str>) -> Result<Session> {
    let connection = Connection::open(ip).await?;
    let groups = connection.groups().await?;
    if let Some(fingerprint) = fingerprint {
        let household = connection.household_id().await?;
        if state.remember(fingerprint, &household, &groups) {
            state.save()?;
        }
    }
    Ok(Session { connection, groups })
}

/// Find a player to talk to: an explicit address, then whatever is remembered for
/// this network, then - only on a network we already know has players - a rescan.
///
/// An unrecognised network is never scanned automatically. This runs on hotel and
/// client-site WiFi, where an unprompted subnet sweep is bad manners at best.
pub async fn connect(explicit: Option<IpAddr>, state: &mut State) -> Result<Session> {
    let fingerprint = netid::network_fingerprint();
    if let Some(ip) = explicit {
        return attach(ip, state, fingerprint.as_deref()).await;
    }

    let Some(fingerprint) = fingerprint.as_deref() else {
        bail!("could not identify this network (no default gateway); pass --ip explicitly");
    };
    let known = state.players_on(fingerprint);
    if known.is_empty() {
        // An unregistered network: this gateway has never been discovered on.
        // `players_on` is empty only for a fingerprint not in state, because a
        // network is remembered only once it has players - so this branch *is*
        // the unregistered-network case, and says so by name. The constructor
        // owns the never-hand-out-a-scan rationale (and its pinning test).
        return Err(crate::hint::unregistered_network(fingerprint));
    }

    for player in &known {
        if let Ok(session) = attach(player.ip, state, Some(fingerprint)).await {
            return Ok(session);
        }
    }

    // A known network where nothing answers: addresses have most likely moved.
    eprintln!("Remembered players did not answer; rescanning...");
    let scan = discover::scan_local_subnet(true).await?;
    match scan.found.first() {
        Some(ip) => attach(IpAddr::V4(*ip), state, Some(fingerprint)).await,
        None => {
            let names: Vec<_> = known.iter().map(|p| p.name.as_str()).collect();
            Err(crate::hint::no_players_answered(&names))
        }
    }
}

/// The group a command applies to.
#[derive(Debug)]
pub struct Target {
    pub group_id: String,
    pub name: String,
    /// The coordinator owns the group's queue, so UPnP calls go to it.
    pub coordinator_id: String,
    pub coordinator_ip: Option<IpAddr>,
}

pub fn target(groups: &Groups, room: Option<&str>) -> Result<Target> {
    let group = groups.resolve(room)?;
    Ok(Target {
        group_id: group.id.clone(),
        name: group.name.clone(),
        coordinator_id: group.coordinator_id.clone(),
        coordinator_ip: groups.player(&group.coordinator_id).and_then(Player::ip),
    })
}

/// Group commands go to the coordinator, which may not be the player we reached.
pub async fn coordinator(session: &Session, target: &Target) -> Result<Connection> {
    match target.coordinator_ip {
        Some(ip) if ip != session.connection.ip() => Connection::open(ip).await,
        _ => Ok(session.connection.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonos::proto::Group;

    // `attach`, `connect` and `coordinator` all open sockets, so what is unit
    // testable here is `target` - the pure step that turns a room name into the
    // group a command applies to. It is also the step with the most room to be
    // quietly wrong: every grouping rule the CLI documents passes through it.

    fn player(id: &str, name: &str, ip: &str) -> Player {
        Player {
            id: id.into(),
            name: name.into(),
            websocket_url: format!("wss://{ip}:1443/websocket/api"),
            capabilities: vec![],
        }
    }

    fn group(id: &str, name: &str, coordinator: &str, members: &[&str]) -> Group {
        Group {
            id: id.into(),
            name: name.into(),
            coordinator_id: coordinator.into(),
            playback_state: String::new(),
            player_ids: members.iter().map(|id| (*id).into()).collect(),
        }
    }

    /// Media Room on its own, plus Dining Room and Kitchen playing together with
    /// Dining Room coordinating - the shape the grouping rules are written for.
    fn household() -> Groups {
        Groups {
            groups: vec![
                group("g:media", "Media Room", "RINCON_1", &["RINCON_1"]),
                group(
                    "g:dining",
                    "Dining Room + 1",
                    "RINCON_2",
                    &["RINCON_2", "RINCON_3"],
                ),
            ],
            players: vec![
                player("RINCON_1", "Media Room", "192.168.77.94"),
                player("RINCON_2", "Dining Room", "192.168.77.95"),
                player("RINCON_3", "Kitchen", "192.168.77.96"),
            ],
        }
    }

    #[test]
    fn a_member_name_targets_the_group_and_sends_upnp_to_the_coordinator() {
        let target = target(&household(), Some("Kitchen")).unwrap();

        // Naming any member addresses the whole group - "pause the kitchen"
        // while the kitchen is grouped pauses the group, which is what people
        // mean and what the CLI promises.
        assert_eq!(target.group_id, "g:dining");
        assert_eq!(target.name, "Dining Room + 1");
        // But the queue lives on the coordinator, so the address a UPnP call
        // goes to is Dining Room's - not the room that was named. Getting this
        // backwards would edit the wrong queue while looking like it worked.
        assert_eq!(target.coordinator_id, "RINCON_2");
        assert_eq!(target.coordinator_ip, "192.168.77.95".parse().ok());
    }

    #[test]
    fn the_composite_group_label_is_not_a_room_name() {
        // "Dining Room + 1" is a display label built from the group; no player
        // is called that. Passing it back as -r is the documented trap, and it
        // has to fail as a room-resolution error rather than resolve to
        // something plausible.
        let error = target(&household(), Some("Dining Room + 1")).unwrap_err();
        assert_eq!(crate::hint::of(&error).0, "unknown_room");
    }

    #[test]
    fn a_room_name_matches_however_it_is_capitalised() {
        // Rooms are addressed by name from a shell, so the name a user types is
        // not the name Sonos stores.
        let target = target(&household(), Some("kITCHEN")).unwrap();
        assert_eq!(target.group_id, "g:dining");
    }

    #[test]
    fn with_one_group_no_room_is_needed_and_with_several_it_is() {
        let mut groups = household();
        groups.groups.truncate(1);
        assert_eq!(target(&groups, None).unwrap().group_id, "g:media");

        // With more than one there is no defensible default: picking for the
        // user means music in a room they did not ask for.
        assert!(target(&household(), None).is_err());
    }

    #[test]
    fn a_coordinator_with_no_usable_address_still_targets_the_group() {
        let mut groups = household();
        // The coordinator is missing from the player list - a topology that
        // moved between the two reads that built it.
        groups.players.retain(|p| p.id != "RINCON_2");

        let target = target(&groups, Some("Kitchen")).unwrap();
        assert_eq!(target.coordinator_id, "RINCON_2");
        // None rather than an error: `coordinator()` reads this as "use the
        // connection already open", which is the right fallback and a worse
        // outcome to turn into a failed command.
        assert_eq!(target.coordinator_ip, None);
    }
}
