//! The household as a whole: finding it on a network and remembering it,
//! naming which one when a LAN carries two, what hardware it is made of, what
//! firmware each speaker runs, and grouping - `group`, `party`, `ungroup`.
//! The grouping commands resolve their own rooms because `ungroup` names its
//! room positionally and must work without `--room`.

use std::net::IpAddr;

use anyhow::{Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::json;

use super::Report;
use crate::session::{self, Session};
use crate::sonos::proto::{Group, Groups, Player};
use crate::sonos::upnp::{self, Upnp};
use crate::state::State;
use crate::{discover, netid};

/// A group after `group`, `party` or `ungroup` touched it: named by the rooms
/// in it, so the result of a change is visible rather than merely reported as
/// having happened.
#[derive(Debug, Serialize)]
pub struct GroupOutcome {
    /// The group's own name - "Kitchen + 1" - which is for display only.
    pub group: String,
    pub coordinator: String,
    /// Every room in it, in the order the player lists them - which is not
    /// coordinator first; `coordinator` says which one that is.
    pub members: Vec<String>,
    /// The room `ungroup` took out; null for the other two.
    pub left: Option<String>,
    /// `ungroup` on a room already alone: nothing was asked of the player.
    #[serde(skip)]
    already_alone: bool,
    #[serde(skip)]
    notes: Vec<String>,
}

impl GroupOutcome {
    fn of(group: &Group, groups: &Groups) -> Self {
        let name = |id: &String| groups.player(id).map(|p| p.name.clone());
        Self {
            group: group.name.clone(),
            coordinator: name(&group.coordinator_id).unwrap_or_default(),
            members: group.player_ids.iter().filter_map(name).collect(),
            left: None,
            already_alone: false,
            notes: Vec::new(),
        }
    }

    fn line(&self) -> String {
        if self.members.len() > 1 {
            format!("{:<24} [{}]", self.group, self.members.join(" + "))
        } else {
            format!("{:<24} on its own", self.group)
        }
    }
}

impl Report for GroupOutcome {
    fn text(&self) -> String {
        if self.already_alone {
            return format!("{:<24} was already on its own", self.group);
        }
        match &self.left {
            Some(room) => format!("{room:<24} left {}\n{}", self.group, self.line()),
            None => self.line(),
        }
    }
    fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// `party off`: how many groups were broken up, and which rooms left one.
#[derive(Debug, Serialize)]
pub struct PartyOffOutcome {
    pub broken: usize,
    pub left: Vec<String>,
    #[serde(skip)]
    notes: Vec<String>,
}

impl Report for PartyOffOutcome {
    fn text(&self) -> String {
        if self.broken == 0 {
            "No rooms were grouped.".to_string()
        } else {
            "Every room is on its own.".to_string()
        }
    }
    fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// What `party` did: joined every room into one group, or broke them all up.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PartyOutcome {
    Joined(GroupOutcome),
    Off(PartyOffOutcome),
}

impl Report for PartyOutcome {
    fn text(&self) -> String {
        match self {
            PartyOutcome::Joined(o) => o.text(),
            PartyOutcome::Off(o) => o.text(),
        }
    }
    fn notes(&self) -> &[String] {
        match self {
            PartyOutcome::Joined(o) => o.notes(),
            PartyOutcome::Off(o) => o.notes(),
        }
    }
}

/// Mask an identifier down to something still comparable but not publishable.
///
/// A serial and an address both matter in a bug report only as "are these two
/// lines the same speaker", so the tail is what gets kept - the last two
/// segments of it. One segment is not enough: a serial ends in a single check
/// character, so `…C` would collapse most of a household onto the same label
/// and lose the only thing the tail was kept for.
fn masked(value: &str) -> String {
    // An IPv6 address is full of ':' but its tail can embed the MAC (EUI-64),
    // so the two-segment rule would keep three octets of it; one group is
    // plenty to compare. Unreachable today - players publish IPv4 Locations -
    // but guarded anyway, so a future v6 household does not leak through the
    // one flag that promises masking.
    if value.parse::<std::net::Ipv6Addr>().is_ok() {
        return match value.rsplit_once(':') {
            Some((_, tail)) if !tail.is_empty() => format!("…{tail}"),
            _ => "…".to_owned(),
        };
    }
    let cuts: Vec<_> = value
        .match_indices(['-', '.', ':'])
        .map(|(i, _)| i)
        .collect();
    match cuts.len() {
        0 => "…".to_owned(),
        // Only one separator, so the whole tail is already the last two
        // segments and masking it further would leave nothing to compare.
        1 => format!("…{}", &value[cuts[0] + 1..]),
        n => format!("…{}", &value[cuts[n - 2] + 1..]),
    }
}

/// The mask for a `RINCON_…` uuid, which embeds the speaker's MAC verbatim -
/// the very identifier the serial mask withholds, so it cannot be printed raw
/// under `--redact`. It has no separators for [`masked`] to cut on; the kept
/// tail is the last MAC octet plus the fixed suffix, the same exposure the
/// masked serial gives.
fn masked_uuid(uuid: &str) -> String {
    match uuid.char_indices().rev().nth(6) {
        Some((i, _)) => format!("…{}", &uuid[i..]),
        None => "…".to_owned(),
    }
}

/// The household by player, grouped under the room each one belongs to.
fn print_system(
    rows: &[(&upnp::SystemPlayer, Result<upnp::DeviceInfo>)],
    json: bool,
    redact: bool,
) {
    // The one policy `--redact` enforces, written once. Every identifier goes
    // through here, so a new field cannot forget the flag - which is exactly
    // how the raw uuid once slipped into output the flag promised was safe.
    let show = |value: &str| {
        if redact {
            masked(value)
        } else {
            value.to_owned()
        }
    };
    let show_uuid = |uuid: &str| {
        if redact {
            masked_uuid(uuid)
        } else {
            uuid.to_owned()
        }
    };
    let show_ip = |ip: Option<IpAddr>| ip.map(|ip| show(&ip.to_string()));
    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(player, found)| {
                let mut entry = json!({
                    "room": player.room,
                    "uuid": show_uuid(&player.uuid),
                    "role": player.role(),
                    "channels": player.channels,
                    "bonded": player.bonded(),
                    "satellite": player.satellite,
                    "hidden": player.invisible,
                    "ip": show_ip(player.ip),
                    "connection": player.connection(),
                    // The raw number beside the word, because the word covers
                    // only the values seen on real hardware - see
                    // `SystemPlayer::connection`. Anything else reads
                    // "unknown" here and is still legible there.
                    "connection_type": player.connection_type,
                    "eth_link": player.eth_link,
                });
                match found {
                    Ok(info) => {
                        entry["model"] = json!(info.model_name);
                        entry["model_number"] = json!(info.model_number);
                        entry["serial"] = json!(show(&info.serial));
                        entry["sonos_os"] = json!(format!("S{}", info.sw_gen));
                        entry["display_version"] = json!(info.display_version);
                        entry["build"] = json!(info.build());
                        entry["software_version"] = json!(info.software_version);
                        entry["hardware_version"] = json!(info.hardware_version);
                        entry["series_id"] = json!(info.series_id);
                    }
                    // Reported rather than dropped: the topology knows this
                    // player exists, so silence about it would be a lie.
                    Err(e) => entry["error"] = json!(format!("{e:#}")),
                }
                entry
            })
            .collect();
        println!("{}", serde_json::to_string(&items).expect("serializable"));
        return;
    }
    if rows.is_empty() {
        println!("No players answered.");
        return;
    }
    let mut room = None;
    for (player, found) in rows {
        if room != Some(&player.room) {
            let count = rows.iter().filter(|(p, _)| p.room == player.room).count();
            let plural = if count == 1 { "player" } else { "players" };
            println!("{}  ({count} {plural})", player.room);
            room = Some(&player.room);
        }
        let label = match player.role() {
            Some(role) => format!("({role})"),
            None => String::new(),
        };
        match found {
            Ok(info) => {
                let addr = show_ip(player.ip).unwrap_or_else(|| "no address".to_owned());
                println!(
                    "  {:<22} {:<5} {:<9} {:<8} build {:<10} hw {:<16} {:<5} {:<15} {}",
                    info.model_name,
                    label,
                    player.connection(),
                    info.display_version,
                    info.build(),
                    info.hardware_version,
                    info.model_number,
                    addr,
                    show(&info.serial),
                );
            }
            Err(e) => println!("  {:<22} {label:<5} unreachable ({e:#})", "?"),
        }
    }
}

/// Every household a subnet sweep found, remembered. The `discover` and
/// `households` half of what `session::connect`'s rescan also does; the
/// discovering, the "found devices but none would talk" error and the
/// remembering all live in `session::discover_households`, so the three
/// callers cannot drift - which they had, once.
async fn discover_and_remember_households(
    scan: &discover::Scan,
) -> Result<Vec<session::Discovered>> {
    let mut state = State::load()?;
    let fingerprint = netid::network_fingerprint();
    session::discover_households(&scan.found, &mut state, fingerprint.as_deref()).await
}

/// `x2rock discover`: sweep the network, and print (and remember) every
/// Sonos household found - not just one.
///
/// A rescan is exactly the moment a second household should not go unnoticed:
/// this is the one command whose whole job is "tell me what's actually out
/// there," so unlike `connect`'s rescan it never needs `--household` to pick
/// a winner - there is no session to hand back, only a report. With one
/// household (every ordinary home) the output is the flat list this always
/// printed; a second changes only the heading, naming what `x2rock
/// households` and `--household` are for.
pub async fn discover_and_remember() -> Result<()> {
    let network = discover::local_network()?;
    eprintln!("Scanning {}/{} ...", network.ip, network.prefix_len());
    // Sweep it all: the point of stopping early was to avoid opening a session
    // per responder, not to stop looking. Stopping at the first hit made a
    // player that answers on 1400 but will not complete a WebSocket - mid
    // reboot, host firewall - the end of the whole command.
    let scan = discover::scan_local_subnet().await?;
    if let Some(prefix) = scan.narrowed_from {
        eprintln!(
            "Network is a /{prefix}, too large to sweep; scanned {} addresses in the local /24 only.",
            scan.scanned
        );
    }
    if scan.found.is_empty() {
        println!("No Sonos players found.");
        return Ok(());
    }

    let discovered = discover_and_remember_households(&scan).await?;

    let several = discovered.len() > 1;
    if several {
        println!(
            "{} Sonos households found on this network - `x2rock households` names them for \
             --household.",
            discovered.len()
        );
    }
    for (i, found) in discovered.iter().enumerate() {
        if several {
            println!("\nHousehold {}:", i + 1);
        }
        let mut players: Vec<_> = found.session.groups.players.iter().collect();
        players.sort_by(|a, b| a.name.cmp(&b.name));
        for player in players {
            match player.ip() {
                Some(ip) => println!("{ip}  {}", player.name),
                None => println!("(no address)  {}", player.name),
            }
        }
    }
    Ok(())
}

/// `x2rock households`: name every Sonos household reachable here, for
/// `--household` to choose between.
///
/// Always scans - `discover`'s honesty, not `status`'s - because the one job
/// this command has is telling two households apart *right now*; a cached
/// answer could be the reason someone is confused in the first place.
pub async fn run_households(json: bool, redact: bool) -> Result<()> {
    let scan = discover::scan_local_subnet().await?;
    // The same honesty `discover` has: on a /16 only the local /24 was swept,
    // and a household outside it is missing from this list, not absent.
    if let Some(prefix) = scan.narrowed_from {
        eprintln!(
            "Network is a /{prefix}, too large to sweep; scanned {} addresses in the local /24 only.",
            scan.scanned
        );
    }
    if scan.found.is_empty() {
        println!(
            "{}",
            if json {
                "[]"
            } else {
                "No Sonos players found."
            }
        );
        return Ok(());
    }

    let discovered = discover_and_remember_households(&scan).await?;

    // `masked`, not `masked_uuid`: a household id (`Sonos_…​.Zv1xanSF--vUn91aMpBs`,
    // a real one observed 2026-09-12) has real separators, unlike the bare hex
    // run of a RINCON uuid `masked_uuid` exists for - so the separator-aware
    // mask is the one built for this shape, keeping more of the tail than a
    // fixed 7 characters would.
    let show_id = |id: &str| {
        if redact { masked(id) } else { id.to_owned() }
    };
    let rows: Vec<(String, Vec<&str>)> = discovered
        .iter()
        .map(|found| {
            let mut rooms: Vec<_> = found
                .session
                .groups
                .players
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            rooms.sort_unstable();
            (show_id(&found.household_id), rooms)
        })
        .collect();

    if json {
        let rows: Vec<_> = rows
            .iter()
            .map(|(id, rooms)| json!({ "id": id, "rooms": rooms }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for (id, rooms) in &rows {
            println!("{id}  {}", rooms.join(", "));
        }
    }
    Ok(())
}

/// `x2rock update`: each speaker's installed and offered firmware. Read-only;
/// applying an update is the Sonos app's job.
pub async fn update(session: &Session, json: bool) -> Result<()> {
    // All at once: sequentially each unreachable player stacked its 8s
    // timeout onto a read-only command - the same shape `system` fixed.
    let rows: Vec<_> =
        futures_util::future::join_all(session.groups.players.iter().map(|player| async move {
            let found = match player.ip() {
                Some(ip) => Upnp::new(ip).software_update().await,
                None => Err(anyhow!("no address to reach it on")),
            };
            (player.name.clone(), found)
        }))
        .await;
    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(room, found)| match found {
                Ok(u) => json!({
                    "room": room,
                    "installed": u.installed,
                    "offered": u.offered,
                    "up_to_date": u.up_to_date(),
                    "download_bytes": u.download_bytes,
                    "swgen": u.swgen,
                    "latest_swgen": u.latest_swgen,
                }),
                // A speaker that would not answer is reported as one, not
                // dropped - "no update" and "no answer" are different news.
                Err(e) => json!({ "room": room, "error": format!("{e:#}") }),
            })
            .collect();
        println!("{}", serde_json::to_string(&items).expect("serializable"));
    } else {
        for (room, found) in &rows {
            match found {
                Ok(u) if u.up_to_date() => {
                    println!("{room:<24} {}  up to date", u.installed)
                }
                Ok(u) => println!(
                    "{room:<24} {} → {}  update offered ({:.1} MB)",
                    u.installed,
                    u.offered.as_deref().unwrap_or("?"),
                    u.download_bytes as f64 / 1_000_000.0,
                ),
                Err(e) => println!("{room:<24} unreachable ({e:#})"),
            }
        }
        // Said once, not per room: applying it is the app's job.
        println!("Applying an update is the Sonos app's job; x2rock only reads this.");
    }
    Ok(())
}

/// `x2rock system`: every player with its model, role, bonding and firmware -
/// the About My System readout. `redact` masks serials, ips and uuids.
pub async fn system(session: &Session, json: bool, redact: bool) -> Result<()> {
    let any = session
        .groups
        .players
        .iter()
        .find_map(|p| p.ip())
        .ok_or_else(|| anyhow!("no player has an address to ask for the topology"))?;
    let players = Upnp::new(any).system_players().await?;
    // All at once, not one after another: the fetches are independent, and
    // sequentially each unreachable player would stack its whole 8s timeout
    // onto a read-only command - three dark satellites made it half a
    // minute. Together they cost one timeout at worst.
    let mut rows: Vec<_> =
        futures_util::future::join_all(players.iter().map(|player| async move {
            let found = match player.ip {
                Some(ip) => Upnp::new(ip).device_info().await,
                None => Err(anyhow!("no address to reach it on")),
            };
            (player, found)
        }))
        .await;
    // By room, and within a room the primary before its satellites, which is
    // the order the apps print and the order the bonding is legible in.
    rows.sort_by(|(a, _), (b, _)| {
        a.room
            .cmp(&b.room)
            .then(a.satellite.cmp(&b.satellite))
            .then(a.invisible.cmp(&b.invisible))
            .then(a.role().unwrap_or("").cmp(b.role().unwrap_or("")))
    });
    print_system(&rows, json, redact);
    Ok(())
}

/// `x2rock battery`: what the household's portables say about their packs.
///
/// Swept across every player rather than asked of one, because which speakers
/// have a battery is not something worth making a person remember - and a
/// portable that has gone flat is *absent from the topology entirely*, which is
/// itself the answer to "why is it not charging" often enough to be worth
/// saying out loud rather than printing an empty table.
pub async fn battery(session: &Session, room: Option<&str>, json: bool) -> Result<()> {
    // Borrowed rather than collected by value: `Player` is deliberately not
    // `Clone`, and nothing here needs to own one.
    let players: Vec<_> = match room {
        Some(name) => vec![session.groups.player_named(name)?],
        None => session.groups.players.iter().collect(),
    };
    // Together, for the same reason `system` does it: one unreachable player
    // must not stack its timeout onto all the others.
    let rows: Vec<_> = futures_util::future::join_all(players.iter().map(|player| async move {
        let found = match player.ip() {
            Some(ip) => Upnp::new(ip).battery().await,
            None => Err(anyhow!("no address to reach it on")),
        };
        (player, found)
    }))
    .await;

    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|(player, found)| match found {
                Ok(Some(b)) => json!({
                    "room": player.name,
                    "battery": true,
                    "level": b.level,
                    "health": b.health,
                    "temperature": b.temperature,
                    "power_source": b.power_source,
                    "charging": b.charging(),
                }),
                Ok(None) => json!({"room": player.name, "battery": false}),
                Err(e) => json!({"room": player.name, "error": format!("{e:#}")}),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let mut said = false;
    for (player, found) in &rows {
        match found {
            Ok(Some(b)) => {
                said = true;
                let level = b.level.map_or("--".to_string(), |l| format!("{l}%"));
                let source = match (b.charging(), b.power_source.as_deref()) {
                    (Some(true), Some(source)) => format!("charging ({source})"),
                    (Some(false), _) => "on battery".to_string(),
                    _ => "power source unknown".to_string(),
                };
                let health = b.health.as_deref().unwrap_or("health unknown");
                let temp = b.temperature.as_deref().unwrap_or("temperature unknown");
                println!(
                    "{:<24} {level:>4}  {source}, health {health}, temperature {temp}",
                    player.name
                );
            }
            // Not printed: a mains speaker having no battery is the expected
            // answer from most of a household and would bury the portables.
            Ok(None) => {}
            Err(e) => {
                said = true;
                println!("{:<24} could not be asked ({e:#})", player.name);
            }
        }
    }
    if !said {
        println!(
            "No speaker here reports a battery. A portable that has run flat \
             drops off the network altogether, so it would not be listed at all."
        );
    }
    Ok(())
}

/// `x2rock group`: pull `rooms` into the group `room` coordinates.
pub async fn group(
    session: &Session,
    room: Option<&str>,
    rooms: &[String],
) -> Result<GroupOutcome> {
    let host = session.groups.resolve(room)?;
    let mut joining = Vec::new();
    let mut already = Vec::new();
    for name in rooms {
        let player = session.groups.player_named(name)?;
        if host.player_ids.contains(&player.id) {
            already.push(player.name.as_str());
        } else if !joining.iter().any(|(id, _)| id == &player.id) {
            joining.push((player.id.clone(), player.name.as_str()));
        }
    }
    let mut notes = Vec::new();
    if !already.is_empty() {
        notes.push(format!("Already in this group: {}", already.join(", ")));
    }
    if joining.is_empty() {
        let mut outcome = GroupOutcome::of(host, &session.groups);
        outcome.notes = notes;
        return Ok(outcome);
    }
    let host_id = host.id.clone();
    let ids: Vec<String> = joining.iter().map(|(id, _)| id.clone()).collect();
    let target = session::target_for(&session.groups, host);
    let coordinator = session::coordinator(session, &target).await?;
    let info = coordinator
        .modify_group_members(&host_id, &ids, &[])
        .await?;
    let mut outcome = GroupOutcome::of(&info.group, &session.groups);
    outcome.notes = notes;
    Ok(outcome)
}

/// `x2rock party`: every room into `room`'s group, or with `off` every room
/// back on its own.
pub async fn party(
    session: &Session,
    room: Option<&str>,
    mode: Option<&str>,
) -> Result<PartyOutcome> {
    match mode {
        None => {
            let host = session.groups.resolve(room)?;
            let host_id = host.id.clone();
            let joining: Vec<String> = session
                .groups
                .players
                .iter()
                .filter(|p| !host.player_ids.contains(&p.id))
                .map(|p| p.id.clone())
                .collect();
            if joining.is_empty() {
                return Ok(PartyOutcome::Joined(GroupOutcome::of(
                    host,
                    &session.groups,
                )));
            }
            let target = session::target_for(&session.groups, host);
            let coordinator = session::coordinator(session, &target).await?;
            let info = coordinator
                .modify_group_members(&host_id, &joining, &[])
                .await?;
            Ok(PartyOutcome::Joined(GroupOutcome::of(
                &info.group,
                &session.groups,
            )))
        }
        Some("off") => {
            // Each group keeps its coordinator and loses everyone else, so
            // every player ends up a group of its own. Groups are
            // independent, so the snapshot taken at connect stays valid as
            // this walks it - only the group being changed changes.
            let mut outcome = PartyOffOutcome {
                broken: 0,
                left: Vec::new(),
                notes: Vec::new(),
            };
            for group in &session.groups.groups {
                let leaving: Vec<&Player> = group
                    .player_ids
                    .iter()
                    .filter(|id| **id != group.coordinator_id)
                    .filter_map(|id| session.groups.player(id))
                    .collect();
                if leaving.is_empty() {
                    continue;
                }
                // This group's own coordinator, not the default group's -
                // some other group's coordinator refuses this one. A group
                // whose coordinator the topology does not list has nobody to
                // ask, so it is named and left as it is.
                if session.groups.player(&group.coordinator_id).is_none() {
                    outcome.notes.push(format!(
                        "{}: coordinator unknown, left as it is",
                        group.name
                    ));
                    continue;
                }
                let ids: Vec<String> = leaving.iter().map(|p| p.id.clone()).collect();
                let target = session::target_for(&session.groups, group);
                let coordinator = session::coordinator(session, &target).await?;
                coordinator
                    .modify_group_members(&group.id, &[], &ids)
                    .await?;
                outcome.broken += 1;
                outcome.left.extend(leaving.iter().map(|p| p.name.clone()));
            }
            Ok(PartyOutcome::Off(outcome))
        }
        Some(other) => bail!("party takes no argument, or off (got {other:?})"),
    }
}

/// `x2rock ungroup`: take `room` out of its group. Positional, not `--room`.
pub async fn ungroup(session: &Session, room: &str) -> Result<GroupOutcome> {
    let leaving = session.groups.player_named(room)?;
    let Some(group) = session.groups.group_of(&leaving.id) else {
        bail!("{} is not in any group", leaving.name);
    };
    if group.player_ids.len() < 2 {
        let mut outcome = GroupOutcome::of(group, &session.groups);
        // The line names the room, as it always has; a lone room's group is
        // named for it anyway.
        outcome.group = leaving.name.clone();
        outcome.already_alone = true;
        return Ok(outcome);
    }
    // Removing the coordinator is not leaving; the group is the
    // coordinator. Everyone else leaves it instead.
    ensure!(
        leaving.id != group.coordinator_id,
        "{} coordinates {}; ungroup the other rooms instead, or use `party off`",
        leaving.name,
        group.name
    );
    let group_id = group.id.clone();
    let leaving_id = leaving.id.clone();
    let leaving_name = leaving.name.clone();
    // The group being changed is the one the room is leaving, whatever
    // --room might otherwise have selected.
    let target = session::target_for(&session.groups, group);
    let coordinator = session::coordinator(session, &target).await?;
    let info = coordinator
        .modify_group_members(&group_id, &[], &[leaving_id])
        .await?;
    let mut outcome = GroupOutcome::of(&info.group, &session.groups);
    outcome.left = Some(leaving_name);
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(group: &str, members: &[&str]) -> GroupOutcome {
        GroupOutcome {
            group: group.into(),
            coordinator: members[0].into(),
            members: members.iter().map(|m| m.to_string()).collect(),
            left: None,
            already_alone: false,
            notes: Vec::new(),
        }
    }

    /// The lines `group`, `party` and `ungroup` have always printed, and the
    /// keys `--json` now documents for them.
    #[test]
    fn a_group_outcome_prints_the_group_and_names_who_left() {
        let joined = outcome("Kitchen + 1", &["Kitchen", "Dining Room"]);
        assert_eq!(
            joined.text(),
            "Kitchen + 1              [Kitchen + Dining Room]"
        );
        let json = serde_json::to_value(&joined).unwrap();
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["coordinator", "group", "left", "members"]);
        assert_eq!(json["left"], serde_json::Value::Null);

        let mut alone = outcome("Dining Room", &["Dining Room"]);
        assert_eq!(alone.text(), "Dining Room              on its own");
        alone.already_alone = true;
        assert_eq!(
            alone.text(),
            "Dining Room              was already on its own"
        );

        let mut left = outcome("Kitchen", &["Kitchen"]);
        left.left = Some("Dining Room".into());
        assert_eq!(
            left.text(),
            "Dining Room              left Kitchen\nKitchen                  on its own"
        );
        assert_eq!(serde_json::to_value(&left).unwrap()["left"], "Dining Room");

        let mut noted = outcome("Kitchen + 1", &["Kitchen", "Dining Room"]);
        noted
            .notes
            .push("Already in this group: Dining Room".into());
        assert_eq!(noted.notes().len(), 1);
    }

    #[test]
    fn party_off_says_whether_anything_was_grouped() {
        let none = PartyOffOutcome {
            broken: 0,
            left: Vec::new(),
            notes: Vec::new(),
        };
        assert_eq!(none.text(), "No rooms were grouped.");
        let some = PartyOutcome::Off(PartyOffOutcome {
            broken: 1,
            left: vec!["Dining Room".into()],
            notes: vec!["Attic: coordinator unknown, left as it is".into()],
        });
        assert_eq!(some.text(), "Every room is on its own.");
        assert_eq!(some.notes().len(), 1);
        let json = serde_json::to_value(&some).unwrap();
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["broken", "left"]);
    }

    /// `--redact` has to leave the output still readable *as a household*: two
    /// lines for the same speaker must match and two speakers must not collide.
    #[test]
    fn redaction_keeps_enough_tail_to_tell_two_speakers_apart() {
        // A serial ends in a one-character check digit, so keeping a single
        // segment would render most of a household as the same label.
        assert_eq!(masked("54-2A-1B-83-31-80:C"), "…80:C");
        assert_eq!(masked("48-A6-B8-A3-BA-52:3"), "…52:3");
        assert_ne!(masked("48-A6-B8-A3-BA-52:3"), masked("48-A6-B8-A3-B9-36:8"));
        assert_eq!(masked("192.168.86.24"), "…86.24");
        assert_ne!(masked("192.168.86.24"), masked("192.168.86.35"));
        // Nothing to cut on, so nothing is revealed.
        assert_eq!(masked("opaque"), "…");
        assert_eq!(masked(""), "…");
        // One separator only: the tail is already the last two segments.
        assert_eq!(masked("a-b"), "…b");
        // An IPv6 tail can embed the MAC (EUI-64), so it keeps one group where
        // everything else keeps two. Unreachable today - players publish IPv4 -
        // but the flag's promise must not depend on that staying true.
        assert_eq!(masked("fe80::4aa6:b8ff:fe18:d138"), "…d138");

        // The RINCON uuid is the MAC verbatim plus a suffix - the exact
        // identifier the serial mask withholds - so it has its own mask, and
        // what it keeps matches the serial's exposure: one MAC octet.
        assert_eq!(masked_uuid("RINCON_542A1B83318001400"), "…8001400");
        assert_ne!(
            masked_uuid("RINCON_48A6B8A3BA5201400"),
            masked_uuid("RINCON_48A6B8A3B93601400")
        );
        assert_eq!(masked_uuid("short"), "…");
    }

    #[test]
    fn masked_keeps_two_households_apart_by_their_real_shape() {
        // A real household id, observed 2026-09-12: separators throughout,
        // unlike a RINCON uuid's bare hex run - `masked`, the separator-aware
        // mask, is the one built for this shape, and it keeps more than
        // `masked_uuid`'s fixed 7 characters would.
        let real = "Sonos_BgzkDDCeWajFguqqdHEXzFKe3x.Zv1xanSF--vUn91aMpBs";
        assert_eq!(masked(real), "…-vUn91aMpBs");

        // Two households differing only in the segment `masked` keeps must
        // still read apart - the one property `households --redact` exists
        // to preserve. Synthetic (only one real household was ever observed
        // to test against), but exercises the same separator-driven rule.
        assert_ne!(
            masked("Sonos_aaaa.bbbb-cccc111"),
            masked("Sonos_aaaa.bbbb-cccc222")
        );
    }
}
