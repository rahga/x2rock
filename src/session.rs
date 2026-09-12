//! Getting from "the user ran a command" to a live connection and a known household.
//!
//! Shared by the CLI and the daemon, so both find players the same way.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};

use anyhow::{Error, Result, bail};

use crate::discover;
use crate::netid;
use crate::sonos::local::Connection;
use crate::sonos::proto::{Groups, Player};
use crate::state::{KnownPlayer, State};

/// A live connection together with the household topology it reported.
pub struct Session {
    pub connection: Connection,
    pub groups: Groups,
}

/// One household found during a scan: its id and a ready-to-use session.
pub struct Discovered {
    pub household_id: String,
    pub session: Session,
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
///
/// `household` is `--household`: which Sonos household to use when more than
/// one is reachable here (an office running two systems, a guest property on
/// the same LAN). It is read only in that case - a network with one household,
/// which is every ordinary home forever, never consults it and behaves exactly
/// as before. See [`resolve_household`].
pub async fn connect(
    explicit: Option<IpAddr>,
    state: &mut State,
    household: Option<&str>,
) -> Result<Session> {
    let fingerprint = netid::network_fingerprint();
    if let Some(ip) = explicit {
        return attach(ip, state, fingerprint.as_deref()).await;
    }

    let Some(fingerprint) = fingerprint.as_deref() else {
        bail!("could not identify this network (no default gateway); pass --ip explicitly");
    };
    let households = state.households_on(fingerprint);
    if households.is_empty() {
        // An unregistered network: this gateway has never been discovered on.
        // `households_on` is empty only for a fingerprint not in state, because
        // a network is remembered only once it has players - so this branch
        // *is* the unregistered-network case, and says so by name. The
        // constructor owns the never-hand-out-a-scan rationale (and its
        // pinning test).
        return Err(crate::hint::unregistered_network(fingerprint));
    }
    let (household_id, players) = resolve_household(&households, household)?;

    for player in players {
        let Ok(session) = attach(player.ip, state, Some(fingerprint)).await else {
            continue;
        };
        // A remembered address can now belong to a different household - DHCP
        // handed it to someone else's speaker, or the household itself moved -
        // and treating whatever answers there as the household that was asked
        // for is exactly the mistake this whole feature exists to rule out.
        // Free to check: `attach` already fetched and cached this id.
        if session.connection.household_id().await.ok().as_deref() == Some(household_id.as_str()) {
            return Ok(session);
        }
        // Wrong household at a remembered address: not ours to keep, and not
        // ours to leak either.
        session.connection.close();
    }

    // The chosen household's remembered players did not answer: addresses have
    // most likely moved. Rescan and re-partition by household id - a second
    // household answering first must never silently stand in for the one that
    // was actually asked for, which ruled out the old `attach_any`-over-the-
    // raw-scan fallback (first responder wins, whoever that is).
    eprintln!("Remembered players did not answer; rescanning...");
    let scan = discover::scan_local_subnet().await?;
    if scan.found.is_empty() {
        let names: Vec<_> = players.iter().map(|p| p.name.as_str()).collect();
        return Err(crate::hint::no_players_answered(&names));
    }

    let (discovered, last_error) = discover_households(&scan.found).await;
    if discovered.is_empty() {
        // Devices answered on the Sonos port but none completed a session -
        // mid-reboot, or a Boost, which listens there and never will. Distinct
        // from `no_players_answered` above: something is out there, just not
        // yet talking.
        let last = last_error.unwrap_or_else(|| anyhow::anyhow!("no address reported a household"));
        return Err(crate::hint::none_completed_a_session(
            scan.found.len(),
            &last,
        ));
    }
    let mut changed = false;
    for found in &discovered {
        changed |= state.remember(fingerprint, &found.household_id, &found.session.groups);
    }
    if changed {
        // Propagated, as `attach` does a few lines up: a save that fails here
        // silently would leave every following command re-sweeping the subnet
        // with nothing to say why.
        state.save()?;
    }

    // Keep the household that was asked for; close the others rather than
    // drop them (see `discover_households` for why dropping is a leak).
    let mut selected = None;
    for found in discovered {
        if selected.is_none() && &found.household_id == household_id {
            selected = Some(found.session);
        } else {
            found.session.connection.close();
        }
    }
    selected.ok_or_else(|| {
        let names: Vec<_> = players.iter().map(|p| p.name.as_str()).collect();
        crate::hint::no_players_answered(&names)
    })
}

/// Every household visible among already-Sonos-port-reachable addresses, each
/// with the live connection and topology it reported.
///
/// One player per household is enough to name it - `groups()` from any member
/// reports the whole household - so every address is probed for its household
/// id first (cheap: a player's cheapest reply already carries it, see
/// [`Connection::household_id`]) and grouped by it, before topology is
/// fetched. But "enough to name it" is not "safe to stop at the first one
/// that answered": that address can go on to fail its own `groups()` call
/// (mid-reboot between the two round trips), and dropping the whole household
/// because *that one* member had trouble would make an otherwise-healthy
/// household disappear from a scan. So every member found for a household is
/// tried, in order, until one completes - the same fallback `attach_any` used
/// to give a single household, now given to each one. Failures are logged
/// exactly as `attach_any` logged them, so "found N but none would talk" still
/// has something above it explaining why.
///
/// This is the one place that looks at every responder instead of stopping at
/// the first: `discover`, `households` and a `connect` rescan all need to know
/// when more than one household is out there, not merely that *a* household
/// is.
///
/// The second half of the return is the last failure seen, if any - not
/// `attach_any`'s whole running commentary, but enough that a caller reporting
/// "found N but none would talk" can say what actually went wrong, the way
/// `attach_any` used to, rather than a placeholder.
pub async fn discover_households(found: &[Ipv4Addr]) -> (Vec<Discovered>, Option<Error>) {
    let probes = found.iter().map(|&ip| async move {
        match Connection::open(IpAddr::V4(ip)).await {
            Ok(connection) => match connection.household_id().await {
                Ok(id) => Ok((id, connection)),
                Err(e) => {
                    eprintln!("{ip}: {e:#}");
                    Err(e)
                }
            },
            Err(e) => {
                eprintln!("{ip}: {e:#}");
                Err(e)
            }
        }
    });

    let mut last_error = None;
    let mut by_household: BTreeMap<String, Vec<Connection>> = BTreeMap::new();
    for result in futures_util::future::join_all(probes).await {
        match result {
            Ok((household_id, connection)) => {
                by_household
                    .entry(household_id)
                    .or_default()
                    .push(connection);
            }
            Err(e) => last_error = Some(e),
        }
    }

    let mut discovered = Vec::new();
    for (household_id, connections) in by_household {
        let mut completed = None;
        let mut connections = connections.into_iter();
        for connection in connections.by_ref() {
            match connection.groups().await {
                Ok(groups) => {
                    completed = Some(Session { connection, groups });
                    break;
                }
                Err(e) => {
                    eprintln!("{}: {e:#}", connection.ip());
                    last_error = Some(e);
                    connection.close();
                }
            }
        }
        // One session per household is kept; every other connection opened by
        // the probe is closed rather than dropped. `Connection` has no `Drop`
        // - `open` spawns a read loop and a keepalive holding strong `Arc`s,
        // and a healthy socket refreshes its own liveness on every pong - so a
        // dropped one lives, and pings, for the rest of the process. The
        // daemon rescans on every reconnect, which made this unbounded.
        for spare in connections {
            spare.close();
        }
        if let Some(session) = completed {
            discovered.push(Discovered {
                household_id,
                session,
            });
        }
    }
    (discovered, last_error)
}

/// Which of several households a `--household` selector names: by any room
/// name that belongs to it, or - only when a room name is not enough because
/// two households share it - an unambiguous prefix of the household id.
///
/// A single household is returned regardless of the selector; there is
/// nothing to choose, and this is what keeps an ordinary one-household network
/// from ever needing to know this exists. `households` is never empty here -
/// callers check that first, since an empty household list and "no household
/// selected" are different problems with different fixes.
fn resolve_household<'a>(
    households: &'a [(String, Vec<KnownPlayer>)],
    selector: Option<&str>,
) -> Result<&'a (String, Vec<KnownPlayer>)> {
    let [first, rest @ ..] = households else {
        bail!("no household known on this network");
    };
    if rest.is_empty() {
        return Ok(first);
    }

    let summarise = || -> Vec<(String, Vec<String>)> {
        households
            .iter()
            .map(|(id, players)| (id.clone(), players.iter().map(|p| p.name.clone()).collect()))
            .collect()
    };
    let Some(selector) = selector else {
        return Err(crate::hint::multiple_households(&summarise()));
    };

    let wanted = selector.to_lowercase();
    let by_room: Vec<&(String, Vec<KnownPlayer>)> = households
        .iter()
        .filter(|(_, players)| players.iter().any(|p| p.name.to_lowercase() == wanted))
        .collect();
    match by_room.as_slice() {
        [one] => return Ok(one),
        [] => {}
        _ => {
            return Err(crate::hint::ambiguous_household(
                &format!(
                    "\"{selector}\" is a room in {} households; pass --household <id> instead \
                     (see `x2rock households`)",
                    by_room.len()
                ),
                &summarise(),
            ));
        }
    }

    // Not a room name anywhere: an id, or an unambiguous prefix of one - the
    // fallback for the one case a room name cannot resolve, two households
    // naming a room the same thing. Exact match first, then unique prefix -
    // the same order `Catalogue::find` resolves a service name in, so an id
    // copied verbatim from `x2rock households` is never refused merely
    // because a longer id happens to start with it.
    if let Some(exact) = households.iter().find(|(id, _)| id == selector) {
        return Ok(exact);
    }
    let by_id: Vec<&(String, Vec<KnownPlayer>)> = households
        .iter()
        .filter(|(id, _)| id.starts_with(selector))
        .collect();
    match by_id.as_slice() {
        [one] => Ok(one),
        [] => Err(crate::hint::unknown_household(selector, &summarise())),
        _ => Err(crate::hint::ambiguous_household(
            &format!("--household {selector:?} matches more than one household id"),
            &summarise(),
        )),
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
    // testable here is `target` and `resolve_household` - the pure steps that
    // turn a name into, respectively, the group a command applies to and the
    // household it belongs to. Both are the step with the most room to be
    // quietly wrong: every grouping and disambiguation rule the CLI documents
    // passes through one of them.

    fn known(id: &str, name: &str, ip: &str) -> KnownPlayer {
        KnownPlayer {
            id: id.into(),
            name: name.into(),
            ip: ip.parse().unwrap(),
        }
    }

    #[test]
    fn a_single_household_is_returned_regardless_of_the_selector() {
        let households = vec![(
            "hh:1".to_string(),
            vec![known("RINCON_1", "Media Room", "192.168.77.94")],
        )];
        // No selector, an irrelevant one, even a wrong one - one household
        // never needs disambiguating, which is what keeps every ordinary
        // single-household home from ever seeing `--household`.
        assert_eq!(resolve_household(&households, None).unwrap().0, "hh:1");
        assert_eq!(
            resolve_household(&households, Some("nonsense")).unwrap().0,
            "hh:1"
        );
    }

    #[test]
    fn several_households_with_no_selector_is_the_ambiguous_error() {
        let households = vec![
            (
                "hh:1".to_string(),
                vec![known("RINCON_1", "Media Room", "192.168.77.94")],
            ),
            (
                "hh:2".to_string(),
                vec![known("RINCON_9", "Studio", "192.168.77.20")],
            ),
        ];
        let error = resolve_household(&households, None).unwrap_err();
        assert_eq!(crate::hint::of(&error).0, "multiple_households");
    }

    #[test]
    fn a_room_name_unique_to_one_household_resolves_it_without_any_id() {
        let households = vec![
            (
                "hh:1".to_string(),
                vec![known("RINCON_1", "Media Room", "192.168.77.94")],
            ),
            (
                "hh:2".to_string(),
                vec![known("RINCON_9", "Studio", "192.168.77.20")],
            ),
        ];
        // Case-insensitive, matching every other room-name lookup in the CLI.
        let chosen = resolve_household(&households, Some("stUDIo")).unwrap();
        assert_eq!(chosen.0, "hh:2");
    }

    #[test]
    fn a_room_name_in_both_households_needs_the_id_instead() {
        let households = vec![
            (
                "hh:1".to_string(),
                vec![known("RINCON_1", "Kitchen", "192.168.77.94")],
            ),
            (
                "hh:2".to_string(),
                vec![known("RINCON_9", "Kitchen", "192.168.77.20")],
            ),
        ];
        // The room name alone cannot pick one - this is the one case a
        // household id is the only remaining way to choose.
        let by_name = resolve_household(&households, Some("Kitchen")).unwrap_err();
        assert!(format!("{by_name:#}").contains("--household <id>"));

        assert_eq!(
            resolve_household(&households, Some("hh:2")).unwrap().0,
            "hh:2"
        );
    }

    #[test]
    fn an_id_prefix_resolves_when_it_is_unambiguous() {
        let households = vec![
            (
                "hh:1abc".to_string(),
                vec![known("RINCON_1", "Media Room", "192.168.77.94")],
            ),
            (
                "hh:2xyz".to_string(),
                vec![known("RINCON_9", "Studio", "192.168.77.20")],
            ),
        ];
        assert_eq!(
            resolve_household(&households, Some("hh:1")).unwrap().0,
            "hh:1abc"
        );
    }

    #[test]
    fn an_id_copied_verbatim_resolves_even_when_it_prefixes_another() {
        // A selector that is itself a complete, exact id must not be refused
        // as ambiguous just because some other household's id happens to
        // start with the same characters - exact match outranks prefix
        // matching, the same order `Catalogue::find` resolves a service name.
        let households = vec![
            (
                "hh:1abc".to_string(),
                vec![known("RINCON_1", "Media Room", "192.168.77.94")],
            ),
            (
                "hh:1abcxyz".to_string(),
                vec![known("RINCON_9", "Studio", "192.168.77.20")],
            ),
        ];
        assert_eq!(
            resolve_household(&households, Some("hh:1abc")).unwrap().0,
            "hh:1abc"
        );
    }

    #[test]
    fn a_selector_matching_nothing_is_unknown_household_not_multiple_households() {
        let households = vec![
            (
                "hh:1".to_string(),
                vec![known("RINCON_1", "Media Room", "192.168.77.94")],
            ),
            (
                "hh:2".to_string(),
                vec![known("RINCON_9", "Studio", "192.168.77.20")],
            ),
        ];
        let error = resolve_household(&households, Some("Bedroom")).unwrap_err();
        assert_eq!(crate::hint::of(&error).0, "unknown_household");
    }

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
