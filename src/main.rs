mod bookmarks;
mod catalogue;
mod cli;
mod commands;
mod completions;
mod credentials;
mod daemon;
mod discover;
mod hint;
mod mpris;
mod netid;
mod restart;
mod service;
mod session;
mod sonos;
mod state;
mod stations;
mod store;
mod streams;
mod tui;

use std::net::IpAddr;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{CommandFactory, FromArgMatches};
use serde_json::json;

use cli::{Cli, Command, RawScope, RawTransport, UpnpScope};
use commands::mmss;
use commands::playback::{
    apply_crossfade, apply_repeat, apply_shuffle, apply_transport, play_or_resume,
};
use commands::speaker::{
    ToneRequest, apply_buttons, apply_eq, apply_led, apply_remote, apply_rename, apply_sleep,
    apply_snooze, named_speaker,
};
use commands::volume::apply_vol;
use commands::{admin, content, playback, services, speaker, stream, volume};

use sonos::local::Connection;
use sonos::proto::{Group, Groups, MetadataStatus, PlaybackStatus, Player, Repeat, Volume};
use sonos::upnp::{self, Upnp};
use state::State;

/// `raw upnp`: one SOAP action against one speaker.
///
/// Separate from the Control API path rather than folded into it because
/// almost nothing is shared: a different transport, a different address (a
/// player, never a group id), a different argument shape, and a different
/// answer. What they do share is the contract that **a refusal is a result** -
/// a UPnP fault prints and exits 0, so a probe that discovers an action is
/// unsupported has succeeded at what it was for.
async fn raw_upnp(
    session: &session::Session,
    room: Option<&str>,
    service: &str,
    action: &str,
    args: &[String],
    scope: UpnpScope,
) -> Result<()> {
    ensure!(
        !action.is_empty()
            && action
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && action.chars().all(|c| c.is_ascii_alphanumeric()),
        "{action:?} is not a usable action name - UPnP action names are letters \
         and digits, starting with a letter"
    );
    let Some(entry) = upnp::service_entry(service) else {
        let names: Vec<&str> = upnp::SERVICES.iter().map(|s| s.name).collect();
        bail!(
            "no UPnP service named {service:?}. There are {}: {}",
            names.len(),
            names.join(", ")
        );
    };

    // SOAP arguments are a flat list of named strings. Split on the first `=`
    // only: values carry URIs, and a URI carries `=`.
    let mut parsed = Vec::with_capacity(args.len());
    for arg in args {
        let Some((name, value)) = arg.split_once('=') else {
            bail!(
                "UPnP arguments are Name=Value pairs; {arg:?} has no `=`. \
                 Most actions need InstanceID=0."
            );
        };
        // The name becomes an XML tag verbatim - only the value is escaped -
        // so anything that is not a valid tag produces an opaque parse failure
        // from the player instead of a message pointing at the typo.
        ensure!(
            name.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')),
            "{name:?} is not a usable argument name - UPnP argument names are \
             letters, digits, _ - and . , starting with a letter. Most actions \
             need InstanceID=0."
        );
        parsed.push((name.to_owned(), value.to_owned()));
    }

    // UPnP addresses a speaker, and `UpnpScope` has only the two that mean
    // something: `group` aims at the coordinator, the only player that answers
    // for the group's transport, and `player` at the room's own speaker, which
    // is what RenderingControl and DeviceProperties are per. The Control API's
    // household and unaddressed scopes are absent from the type rather than
    // rejected at runtime.
    let target = session::target(&session.groups, room)?;
    let ip = match scope {
        UpnpScope::Group => target
            .coordinator_ip
            .ok_or_else(|| anyhow!("no address for {}'s coordinator", target.name))?,
        // The same resolution every per-player command uses, so `raw upnp
        // --scope player` and `led` cannot drift on which speaker `--room`
        // means.
        UpnpScope::Player => named_speaker(session, &target, room)?.1.ip(),
    };

    match Upnp::new(ip).raw_action(entry, action, &parsed).await {
        Ok(out) if out.is_empty() => println!("{} {action}: ok, no output", entry.name),
        // An array of name/value pairs rather than an object, because a probe
        // is reading a shape it does not know yet: `serde_json::Map` is a
        // BTreeMap here (no `preserve_order` feature), so an object would
        // re-sort the player's own argument order alphabetically and keep only
        // the last of any repeated name. Both are exactly what `raw_action`
        // returns a Vec to avoid losing.
        Ok(out) => {
            let pairs: Vec<serde_json::Value> = out
                .into_iter()
                .map(|(name, value)| json!({ "name": name, "value": value }))
                .collect();
            println!("{}", serde_json::to_string_pretty(&pairs)?);
        }
        // A *refusal* is the finding: the player was reached and said no, which
        // is a result worth printing and worth exiting 0 for, so a shell loop
        // over candidate actions is not stopped by the first unsupported one.
        // Anything else - an unreachable speaker, a timeout, an unparseable
        // envelope, or UPnP switched off for the whole household - is a real
        // failure and must propagate, or a script's `|| handle_failure` never
        // fires. The last one matters most for a loop over candidate actions,
        // which would otherwise conclude every service is unsupported.
        Err(e) if upnp::Fault::of(&e).is_some_and(upnp::Fault::is_per_action) => {
            eprintln!("{} {action}: {e:#}", entry.name)
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

fn now_line(status: &PlaybackStatus, meta: &MetadataStatus) -> String {
    let track = meta.current_item.as_ref().and_then(|i| i.track.as_ref());
    let title = meta.title();
    let artist = track
        .and_then(|t| t.artist.as_ref())
        .and_then(|a| a.name.as_deref());
    let album = track
        .and_then(|t| t.album.as_ref())
        .and_then(|a| a.name.as_deref());

    // A state-less event is a daemon concern; the polled reply this reads has
    // always carried one. Named rather than blank so an odd line is legible.
    let mut line = status.state().unwrap_or("UNKNOWN").to_string();
    if let Some(title) = title {
        line.push_str("  ");
        line.push_str(title);
    }
    if let Some(artist) = artist {
        line.push_str(" — ");
        line.push_str(artist);
    }
    if let Some(album) = album.filter(|a| Some(*a) != title) {
        line.push_str(&format!(" ({album})"));
    }
    // What the station says is on right now, which for a stream loaded by URL
    // is the only track information there is. Shown only when it says something
    // the title does not, so a service stream that already names its track is
    // not made to say it twice - the same rule the daemon's `stationName` uses
    // for the opposite half of this problem.
    if let Some(info) = meta
        .stream_info
        .as_deref()
        .map(str::trim)
        .filter(|i| !i.is_empty() && Some(*i) != title)
    {
        line.push_str(&format!(" · {info}"));
    }
    // Where it is coming from. Only present for service content, so TV input and
    // a bare queue track leave it off rather than printing "on ".
    if let Some(service) = meta
        .container
        .as_ref()
        .and_then(|c| c.service.as_ref())
        .and_then(|s| s.name.as_deref())
    {
        line.push_str(&format!(" · on {service}"));
    }
    // Elapsed / total. Guarded on a real duration, which a live stream does not
    // have - so a station is not made to show a running clock against nothing.
    if let Some(duration) = track
        .and_then(|t| t.duration_millis)
        .filter(|ms| *ms > 0)
        .map(std::time::Duration::from_millis)
    {
        let position = status.position_millis.map(std::time::Duration::from_millis);
        line.push_str(&format!("  {} / {}", mmss(position), mmss(Some(duration))));
    }
    // On a soundbar this is the whole point of looking: a source that has
    // quietly dropped to stereo says so here and nowhere else.
    if let Some(format) = meta
        .container
        .as_ref()
        .and_then(|c| c.ht_input_format.as_ref())
    {
        line.push_str(&format!("  [{}]", format.summary()));
    }
    let repeat = status.modes().repeat();
    let mut flags = Vec::new();
    if status.modes().shuffle {
        flags.push("shuffle");
    }
    let repeating = format!("repeat {}", repeat.as_str());
    if repeat != Repeat::Off {
        flags.push(&repeating);
    }
    if !flags.is_empty() {
        line.push_str(&format!("  [{}]", flags.join(", ")));
    }
    line
}

/// The service id embedded in a player art URL - `…sid=284…`, or the
/// percent-encoded `…sid%3d284…` the getaa wrapper produces. The reliable sid
/// when the metadata object's own id disagrees (it does, for HLS/stream content).
fn service_id_from_art(url: &str) -> Option<&str> {
    for marker in ["sid=", "sid%3d", "sid%3D"] {
        if let Some(pos) = url.find(marker) {
            let rest = &url[pos + marker.len()..];
            let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            if digits > 0 {
                return Some(&rest[..digits]);
            }
        }
    }
    None
}

fn now_json(
    room: &str,
    status: &PlaybackStatus,
    meta: &MetadataStatus,
    services: Option<&catalogue::Catalogue>,
) -> serde_json::Value {
    let track = meta.current_item.as_ref().and_then(|i| i.track.as_ref());
    let next = meta.next_item.as_ref().and_then(|i| i.track.as_ref());
    let container = meta.container.as_ref();
    let art = track
        .and_then(|t| t.image_url.as_deref())
        .or(container.and_then(|c| c.image_url.as_deref()));
    // The art URL names the *playback* sid, and it is the reliable one: the
    // player's own metadata carries a wrong or internal id for HLS/stream
    // content (65435 for a YouTube Music stream) while the art URL says 284.
    // Prefer it; fall back to the metadata object's id when there is no art URL.
    let service_id = art.and_then(service_id_from_art).or_else(|| {
        container
            .and_then(|c| c.id.as_ref())
            .and_then(|id| id.service_id.as_deref())
    });
    json!({
        "room": room,
        "state": status.state(),
        "title": track.and_then(|t| t.name.as_deref()).or(container.and_then(|c| c.name.as_deref())),
        "artist": track.and_then(|t| t.artist.as_ref()).and_then(|a| a.name.as_deref()),
        "album": track.and_then(|t| t.album.as_ref()).and_then(|a| a.name.as_deref()),
        // The player leaves `service` null for some sources (a soundbar playlist,
        // YouTube Music now-playing) while still carrying the sid. Fall back to
        // the catalogue's name for that sid, so `status` names the service
        // `favorites` does. `service_id` is emitted regardless, never lossy.
        "service": container.and_then(|c| c.service.as_ref()).and_then(|s| s.name.as_deref())
            .or_else(|| service_id.and_then(|sid| services.and_then(|c| c.name_of(sid)))),
        "service_id": service_id,
        "position_ms": status.position_millis,
        "duration_ms": track.and_then(|t| t.duration_millis),
        "repeat": status.modes().repeat().as_str(),
        "shuffle": status.modes().shuffle,
        // A third play mode the CLI can now set as well as read.
        "crossfade": status.modes().crossfade,
        // Where in the queue this is, 1-based. Null when the queue is not what
        // is driving - a radio stream has no position. The *length* is not here
        // because it needs the queue itself over UPnP; `queue --json` has both.
        "queue_position": status.queue_position(),
        // The explicit badge every controller shows on the row.
        "explicit": track.and_then(|t| t.explicit),
        // What follows, which the players supply beside the current item and
        // nothing here read until now. Null at the end of a queue, and on a
        // stream, which has no next.
        "next_title": next.and_then(|t| t.name.as_deref()),
        "next_artist": next.and_then(|t| t.artist.as_ref()).and_then(|a| a.name.as_deref()),
        // Answers "is this the TV input?" as a field rather than by matching the
        // "TV Audio" title. The audio format only exists on a soundbar's TV
        // stream, so its presence is the signal.
        "on_tv": container.and_then(|c| c.ht_input_format.as_ref()).is_some(),
        "input_format": meta.container.as_ref().and_then(|c| c.ht_input_format.as_ref()).map(|f| f.summary()),
        "surround": meta.container.as_ref().and_then(|c| c.ht_input_format.as_ref()).map(|f| f.is_surround()),
        "art_url": art,
        // The station's own "now playing" text, verbatim and unparsed. Null for
        // anything that is not a live stream, and the only track information a
        // stream loaded by `play-url` has - see `MetadataStatus::stream_info`.
        "stream_info": meta.stream_info.as_deref().map(str::trim).filter(|i| !i.is_empty()),
    })
}

/// Every group's coordinator answers for its own group and no other, so the
/// snapshot opens a connection per coordinator (reusing the session's own where
/// it coincides). One unreachable coordinator is that room's problem alone - it
/// gets an `error` field and the rest of the household still reports.
async fn print_status(session: &session::Session, json: bool, full: bool) -> Result<()> {
    let mut values = Vec::new();
    let mut lines = Vec::new();
    // Rooms whose coordinator did not answer - the "expected but unreachable"
    // an envelope warns about, and the count `reachable` is derived from.
    let mut unreachable: Vec<String> = Vec::new();
    // The cached catalogue names a service the player's metadata leaves blank
    // (YouTube Music now-playing carries the sid, not the name). Best-effort and
    // read-only - a cold or absent cache just leaves `service` null as before.
    let services = json.then(catalogue::Catalogue::load);
    for group in &session.groups.groups {
        let target = session::Target {
            group_id: group.id.clone(),
            name: group.name.clone(),
            coordinator_id: group.coordinator_id.clone(),
            coordinator_ip: session
                .groups
                .player(&group.coordinator_id)
                .and_then(Player::ip),
        };
        let members: Vec<String> = session
            .groups
            .members(group)
            .iter()
            .map(|p| p.name.clone())
            .collect();
        // A soundbar's HDMI belongs to the player, so the group has a TV input
        // if any member does - the same rule `x2rock tv` uses to find it.
        let has_tv = session.groups.members(group).iter().any(|p| p.has_tv());
        let coordinator = session
            .groups
            .player(&group.coordinator_id)
            .map(|p| p.name.as_str());

        let facts = RoomFacts {
            name: &group.name,
            members: &members,
            coordinator,
            has_tv,
        };
        // Fetched once; a failure is this room's alone. Both branches push, so
        // one unreachable coordinator is tagged, never propagated - the snapshot
        // always describes the whole household.
        let fetched = fetch_room(session, &target).await;
        if fetched.is_err() {
            unreachable.push(group.name.clone());
        }
        if json {
            values.push(room_value(&facts, fetched, services.as_ref()));
        } else {
            lines.push(room_line(&facts, fetched));
        }
    }

    // The envelope's household context, gathered only when asked for it: a bare
    // `status` should not pay a household round trip. Both are best-effort - a
    // null beats a failed snapshot.
    let (household, network, total) = if full {
        (
            session.connection.household_id().await.ok(),
            netid::network_fingerprint(),
            session.groups.groups.len(),
        )
    } else {
        (None, None, 0)
    };
    if json {
        if full {
            println!(
                "{}",
                serde_json::to_string(&status_envelope(
                    household.as_deref(),
                    network.as_deref(),
                    total,
                    &unreachable,
                    values,
                ))?
            );
        } else {
            // Bare array by default: the shape jq and existing callers expect.
            println!("{}", serde_json::to_string(&values)?);
        }
    } else {
        if full {
            println!(
                "household {}  network {}  {} rooms{}",
                household.as_deref().unwrap_or("?"),
                network.as_deref().unwrap_or("?"),
                total,
                if unreachable.is_empty() {
                    String::new()
                } else {
                    format!("  ({} unreachable)", unreachable.len())
                },
            );
        }
        for line in lines {
            println!("{line}");
        }
    }
    Ok(())
}

/// What the group listing knows about a room independent of reaching it: enough
/// that an errored room still tells an agent its identity, grouping and TV.
struct RoomFacts<'a> {
    name: &'a str,
    members: &'a [String],
    coordinator: Option<&'a str>,
    has_tv: bool,
}

type Fetched = Result<(PlaybackStatus, MetadataStatus, Option<Volume>)>;

/// One room's JSON, whether or not its coordinator answered. On success it is
/// the `now --json` object plus the group facts; on failure an `error` entry
/// that still carries the facts, so a dead room is legible rather than absent.
fn room_value(
    facts: &RoomFacts,
    fetched: Fetched,
    services: Option<&catalogue::Catalogue>,
) -> serde_json::Value {
    match fetched {
        Ok((status, meta, volume)) => {
            let mut obj = now_json(facts.name, &status, &meta, services);
            if let serde_json::Value::Object(map) = &mut obj {
                map.insert("volume".into(), json!(volume.as_ref().map(|v| v.volume)));
                map.insert("muted".into(), json!(volume.as_ref().map(|v| v.muted)));
                // Volume 0 and muted are different fields with the same outcome:
                // silence. Derive the outcome so "will this make a sound?" is one
                // read, and starting a room at volume 0 is a warning, not a
                // silent no-op.
                map.insert(
                    "audible".into(),
                    json!(volume.as_ref().map(|v| !v.muted && v.volume > 0)),
                );
                // A fixed-volume room - a Port or Amp feeding something with its
                // own control - takes every volume command and changes nothing.
                // `vol --json` has always reported it; without it here, the one
                // call that is meant to be the whole household leaves an agent
                // to discover the refusal by making it. Distinct from `audible`,
                // which stays true: a fixed room is not silent, it is just not
                // yours to turn down.
                map.insert("fixed".into(), json!(volume.as_ref().map(|v| v.fixed)));
                map.insert("members".into(), json!(facts.members));
                map.insert("coordinator".into(), json!(facts.coordinator));
                map.insert("has_tv".into(), json!(facts.has_tv));
            }
            obj
        }
        Err(e) => json!({
            "room": facts.name,
            "error": format!("{e:#}"),
            "members": facts.members,
            "coordinator": facts.coordinator,
            "has_tv": facts.has_tv,
        }),
    }
}

/// The `--full` envelope: the household context wrapped around the room array.
///
/// Split out of [`print_status`], which gathers that context over the network
/// and then prints in one breath - so the shape agents are promised,
/// `{household, network, total, reachable, warnings, rooms}`, had nowhere a
/// test could see it.
fn status_envelope(
    household: Option<&str>,
    network: Option<&str>,
    total: usize,
    unreachable: &[String],
    rooms: Vec<serde_json::Value>,
) -> serde_json::Value {
    json!({
        "household": household,
        "network": network,
        "total": total,
        // `saturating_sub`, not `-`. The two counts are measured in different
        // places - one per group as the snapshot fails, one off the topology
        // afterwards - and nothing but an implicit invariant keeps `unreachable`
        // the smaller of the two. A `usize` underflow here would not fail
        // loudly: with overflow checks off it would report something near
        // 1.8e19 reachable rooms to whoever is reading the JSON.
        "reachable": total.saturating_sub(unreachable.len()),
        "warnings": unreachable
            .iter()
            .map(|room| format!("{room} unreachable"))
            .collect::<Vec<_>>(),
        "rooms": rooms,
    })
}

/// The text form of the same, one line per room.
fn room_line(facts: &RoomFacts, fetched: Fetched) -> String {
    match fetched {
        Ok((status, meta, volume)) => {
            let vol = match &volume {
                Some(v) if v.muted => "  vol muted".to_string(),
                Some(v) => format!("  vol {}", v.volume),
                None => String::new(),
            };
            let grouped = if facts.members.len() > 1 {
                format!("  [{}]", facts.members.join(", "))
            } else {
                String::new()
            };
            format!(
                "{:<16} {}{vol}{grouped}",
                facts.name,
                now_line(&status, &meta)
            )
        }
        Err(e) => format!("{:<16} unreachable ({e:#})", facts.name),
    }
}

/// The three group-scoped reads a snapshot wants, off the group's coordinator.
/// Volume is best-effort: a room that will not report it is still worth showing.
async fn fetch_room(
    session: &session::Session,
    target: &session::Target,
) -> Result<(PlaybackStatus, MetadataStatus, Option<Volume>)> {
    let conn = session::coordinator(session, target).await?;
    let status = conn.playback_status(&target.group_id).await?;
    let meta = conn.metadata(&target.group_id).await?;
    let volume = conn.group_volume(&target.group_id).await.ok();
    Ok((status, meta, volume))
}

/// The line `x2rock rooms` adds for a person who is about to need `--room` and
/// has no default set.
///
/// **Shown only where it would actually help**, which is the whole design: a
/// household with one group needs no `--room` at all - `Groups::resolve` picks
/// the only one - so mentioning the variable there is noise, and someone who
/// has already set it is being told what they know. This is the command a
/// person runs *before* hitting "this household has several groups; choose one
/// with --room", so it is the right place to answer that question early.
///
/// The example is a **player** name and not a group name. A group is named
/// after whichever player coordinates it and reads as `Dining Room + 1` when
/// several are joined, which is a label and not a room - handing that back as
/// something to export would produce `no room named "Dining Room + 1"`. Room
/// names have spaces, so it goes through `shell_arg` for the same reason a
/// `fix` does.
fn room_default_hint(groups: &Groups, current: Option<&str>) -> Option<String> {
    if groups.groups.len() < 2 {
        return None;
    }
    // An empty or blank value is treated as unset: clap would pass it through
    // as a room name and it would resolve to nothing, so the person still
    // needs this line.
    if current.is_some_and(|v| !v.trim().is_empty()) {
        return None;
    }
    let example = groups.players.first()?.name.as_str();
    Some(format!(
        "Several rooms here, so most commands want `-r <room>`. For a default in this shell: \
         export X2ROCK_ROOM={}",
        hint::shell_arg(example)
    ))
}

fn print_rooms(groups: &Groups, json: bool) {
    if json {
        let rooms: Vec<_> = groups
            .groups
            .iter()
            .map(|g| {
                json!({
                    "room": g.name,
                    "state": g.playback_state.strip_prefix("PLAYBACK_STATE_").unwrap_or(&g.playback_state),
                    "members": groups.members(g).iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
                    "coordinator": groups.player(&g.coordinator_id).map(|p| p.name.as_str()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&rooms).expect("serializable"));
        return;
    }
    for group in &groups.groups {
        let state = group
            .playback_state
            .strip_prefix("PLAYBACK_STATE_")
            .unwrap_or(&group.playback_state);
        let members = groups.members(group);
        if members.len() > 1 {
            let names: Vec<_> = members.iter().map(|p| p.name.as_str()).collect();
            println!("{:<24} {:<10} [{}]", group.name, state, names.join(" + "));
        } else {
            println!("{:<24} {}", group.name, state);
        }
    }
    // Deliberately after the rooms and only in the human output: a hint is not
    // data, and `--json` returned above.
    if let Some(line) = room_default_hint(groups, std::env::var("X2ROCK_ROOM").ok().as_deref()) {
        println!("\n{line}");
    }
}

/// A group named by the rooms in it, so the result of a change is visible
/// rather than merely reported as having happened.
fn group_line(group: &Group, groups: &Groups) -> String {
    let names: Vec<_> = group
        .player_ids
        .iter()
        .filter_map(|id| groups.player(id))
        .map(|p| p.name.as_str())
        .collect();
    if names.len() > 1 {
        format!("{:<24} [{}]", group.name, names.join(" + "))
    } else {
        format!("{:<24} on its own", group.name)
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
async fn discover_and_remember() -> Result<()> {
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
async fn run_households(json: bool, redact: bool) -> Result<()> {
    let scan = discover::scan_local_subnet().await?;
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

/// Wait for whichever asks the daemon to stop, and name it for the log.
///
/// Ctrl-C is not the usual one: as a systemd user service, `systemctl stop` and
/// the restart on upgrade both send SIGTERM. Left unhandled that is a default
/// kill - no unwinding, no line in the journal saying why the daemon went away.
async fn stop_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        // Nothing to be done about it, and Ctrl-C still works.
        let _ = tokio::signal::ctrl_c().await;
        return "interrupt";
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "interrupt",
        _ = terminate.recv() => "SIGTERM",
    }
}

#[tokio::main]
async fn main() {
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    // `X2ROCK_ROOM` is the default room, and `--all` means every room: the
    // default has nothing to add to that, so it is set aside rather than
    // fought over. A `-r` someone typed alongside `--all` is a contradiction,
    // and `run` refuses it.
    if cli.all && matches.value_source("room") == Some(clap::parser::ValueSource::EnvVariable) {
        cli.room.clear();
    }
    // An exported-but-empty `X2ROCK_ROOM=` is no default, not a room named "".
    cli.room.retain(|room| !room.is_empty());
    // Decided before the command runs, so a failure knows how to report itself.
    let json = cli.command.json();
    if let Err(e) = run(cli).await {
        if json {
            // Structured for an agent: the message it always printed, plus a
            // stable code, the fix command when the error carried one, and any
            // detail the hint attached (unknown_room's did_you_mean, say).
            let obj = hint::error_json(&e);
            eprintln!(
                "{}",
                serde_json::to_string(&obj).unwrap_or_else(|_| format!("{{\"error\":{e:?}}}"))
            );
        } else {
            eprintln!("Error: {e:#}");
        }
        std::process::exit(1);
    }
}

/// Fan a per-room command across several `--room`, topology resolved once. Only
/// the per-room-state commands accept it; anything else is refused with a clear
/// message rather than silently acting on the first room. A failure on one room
/// stops the run - a half-applied "set them all to 10" is worse than a clear
/// stop naming the room that failed.
async fn fan_out(session: &session::Session, rooms: &[String], command: &Command) -> Result<()> {
    let Some(action) = per_room(command) else {
        return Err(too_many_rooms());
    };
    for name in rooms {
        let target = session::target(&session.groups, Some(name))?;
        let outcome = match action {
            PerRoom::Vol {
                change,
                one_room,
                ramp,
                json,
            } => {
                apply_vol(
                    session,
                    &target,
                    Some(name),
                    change.clone(),
                    one_room,
                    ramp,
                    json,
                )
                .await
            }
            PerRoom::Repeat { mode, json } => {
                apply_repeat(session, &target, mode.clone(), json).await
            }
            PerRoom::Shuffle { mode, json } => {
                apply_shuffle(session, &target, mode.clone(), json).await
            }
            PerRoom::Crossfade { mode, json } => {
                apply_crossfade(session, &target, mode.clone(), json).await
            }
            // `play` alone confirms and resumes; the other verbs have nothing
            // to confirm against, and `pause` on an idle room is a no-op that
            // must not spend the failure budget.
            PerRoom::Transport("play") => match session::coordinator(session, &target).await {
                Ok(player) => play_or_resume(session, &player, &target).await,
                Err(e) => Err(e),
            },
            PerRoom::Transport(verb) => apply_transport(session, &target, verb).await,
        };
        // Name the room the batch stopped on: a fan-out that halts silently on
        // the third of five rooms is a debugging puzzle. The rooms before it
        // already applied; the ones after did not.
        outcome.with_context(|| format!("on room {name:?}"))?;
    }
    Ok(())
}

/// Several `--room` on a command that takes one. Its own code, not the generic
/// `error` bucket, so an agent drops the extra `--room` from the code rather
/// than parsing the sentence. No `fix` command: the remedy is to re-run with a
/// single `--room`, which is not a canned line.
fn too_many_rooms() -> anyhow::Error {
    hint::Hint::new(
        "several --room were given, but this command takes a single room",
        "too_many_rooms",
        None,
    )
    .into()
}

/// What a per-room command does to one room. Borrowed from the `Command`, so
/// [`fan_out`] can apply it to each room in turn without re-matching.
#[derive(Clone, Copy)]
enum PerRoom<'a> {
    Vol {
        change: &'a Option<String>,
        one_room: bool,
        ramp: bool,
        json: bool,
    },
    Repeat {
        mode: &'a Option<String>,
        json: bool,
    },
    Shuffle {
        mode: &'a Option<String>,
        json: bool,
    },
    Crossfade {
        mode: &'a Option<String>,
        json: bool,
    },
    /// A `playback:1` verb.
    Transport(&'static str),
}

/// The per-room reading of a command, or `None` for the read, whole-household
/// and single-target commands, which several `--room` do not fan out. The one
/// list: [`fans_out`] asks whether a command is on it and [`fan_out`] applies
/// what it finds, so a command cannot be admitted by one and missed by the
/// other - which is how `--all crossfade on` came to fail with "several --room
/// were given" on a command line that gave none.
fn per_room(command: &Command) -> Option<PerRoom<'_>> {
    Some(match command {
        Command::Vol {
            change,
            player,
            ramp,
            json,
            ..
        } => PerRoom::Vol {
            change,
            one_room: *player,
            ramp: *ramp,
            json: *json,
        },
        Command::Repeat { mode, json } => PerRoom::Repeat { mode, json: *json },
        Command::Shuffle { mode, json } => PerRoom::Shuffle { mode, json: *json },
        Command::Crossfade { mode, json } => PerRoom::Crossfade { mode, json: *json },
        Command::Play { track: None } => PerRoom::Transport("play"),
        Command::Pause => PerRoom::Transport("pause"),
        Command::Toggle => PerRoom::Transport("togglePlayPause"),
        Command::Next => PerRoom::Transport("skipToNextTrack"),
        Command::Prev => PerRoom::Transport("skipToPreviousTrack"),
        _ => return None,
    })
}

/// Whether a command applies per room, so several `--room` fan it out.
fn fans_out(command: &Command) -> bool {
    per_room(command).is_some()
}

async fn run(cli: Cli) -> Result<()> {
    // The single room most commands act on: the first `--room`, bound from the
    // field (not a `&self` method) so it stays disjoint from `match cli.command`
    // moving the command out. Multi-room commands read `cli.room` (the whole
    // list) instead.
    let room = cli.room.first().map(String::as_str);
    // Refuse a misapplied --all before dispatch: most commands return from the
    // match below without ever reaching the fan-out, and a silently ignored
    // flag reads as whole-house semantics honored. `bookmarks` is exempt: its
    // own `-a/--all` ("include daemon history") shares clap's arg id with this
    // flag, so setting either sets both.
    // `--all` fans over group *coordinators*, so a ramp there would slide one
    // speaker per group and report it as the group - the one combination that
    // cannot be made honest. Several `--room` and `--each` both fan over
    // speakers, which is exactly what a ramp wants, so they are carried through
    // rather than refused.
    if let Command::Vol { ramp: true, .. } = &cli.command {
        ensure!(
            !cli.all,
            "--ramp slides one speaker at a time and --all fans over groups; \
             name the rooms with --room, or use --each for one group's members"
        );
    }
    if cli.all && !matches!(cli.command, Command::Bookmarks { .. }) {
        ensure!(
            cli.room.is_empty(),
            "--all already means every room; drop the -r (an exported X2ROCK_ROOM is set aside on its own)"
        );
        ensure!(
            fans_out(&cli.command),
            "--all applies only to the per-room commands (volume, transport, repeat, shuffle)"
        );
    }
    match cli.command {
        Command::Discover => return discover_and_remember().await,
        Command::Households { json, redact } => return run_households(json, redact).await,
        Command::Skill {
            agent,
            ref dir,
            print,
            remove,
        } => {
            return admin::handle_skill(agent, dir.as_deref(), print, remove);
        }
        Command::Desktop { action } => return admin::desktop(action),
        Command::Service { action, json } => {
            return admin::service(action, json, cli.household.as_deref());
        }
        Command::Completions {
            shell,
            install,
            uninstall,
        } => return admin::completions(shell, install, uninstall),
        Command::Complete {
            ref what,
            ref prefix,
        } => {
            return completions::complete(what, prefix.as_deref(), &mut std::io::stdout());
        }
        Command::PlayItem {
            ref service,
            ref id,
            ref title,
            ref kind,
        } => {
            return content::run_play_item(
                cli.ip,
                cli.household.as_deref(),
                room,
                service,
                kind.as_deref(),
                id,
                title.as_ref(),
            )
            .await;
        }
        Command::Stations {
            ref query,
            ref tag,
            ref country,
            limit,
            play,
            no_wait,
            json,
        } => {
            return stream::run_stations(
                cli.ip,
                cli.household.as_deref(),
                room,
                query.as_deref(),
                tag.as_deref(),
                country.as_deref(),
                limit,
                play,
                no_wait,
                json,
            )
            .await;
        }
        Command::PlayUrl {
            ref url,
            ref title,
            no_wait,
            json,
        } => {
            return stream::run_play_url(
                cli.ip,
                cli.household.as_deref(),
                room,
                url,
                title.as_deref(),
                no_wait,
                json,
            )
            .await;
        }
        Command::QueueItem {
            ref service,
            ref id,
            ref title,
            ref kind,
        } => {
            return content::run_queue_item(
                cli.ip,
                cli.household.as_deref(),
                room,
                service,
                kind.as_deref(),
                id,
                title.as_ref(),
            )
            .await;
        }
        Command::Browse {
            ref service,
            ref container,
            count,
            index,
            play,
            refresh,
            json,
        } => {
            return services::run_browse(
                cli.ip,
                cli.household.as_deref(),
                room,
                service.as_ref(),
                container.as_deref(),
                count,
                index,
                play,
                refresh,
                json,
            )
            .await;
        }
        Command::Link {
            ref service,
            no_open,
            ref nickname,
            no_match,
            from_player,
        } => {
            return services::run_link(
                cli.ip,
                cli.household.as_deref(),
                service.as_ref(),
                no_open,
                nickname.as_ref(),
                no_match,
                from_player,
            )
            .await;
        }
        // Both of these are about a file on this machine, so neither needs a
        // player and both work with the household unreachable.
        Command::Unlink { ref service } => return services::unlink(service),
        Command::Bookmarks {
            ref action,
            ref query,
            all,
            json,
        } => {
            return content::run_bookmarks(action.as_ref(), query.as_deref(), all, json);
        }
        Command::Accounts { content, json } => {
            return services::accounts(cli.ip, cli.household.as_deref(), room, content, json).await;
        }
        Command::Search {
            ref term,
            ref service,
            ref category,
            count,
            index,
            play,
            refresh,
            json,
        } => {
            return services::run_search(
                cli.ip,
                cli.household.as_deref(),
                room,
                term.as_ref(),
                service.as_ref(),
                category.as_ref(),
                count,
                index,
                play,
                refresh,
                json,
            )
            .await;
        }
        // Before the household session below: the TUI reads the daemon, which has
        // its own connection, and opening a second one here would be a
        // connection nothing in the TUI ever uses.
        Command::Tui => return tui::run(cli.ip).await,
        Command::Daemon {
            verbose,
            log_events,
        } => {
            daemon::init_logging(verbose, log_events);
            tokio::select! {
                result = daemon::run(cli.ip, cli.household.as_deref()) => return result,
                signal = stop_signal() => {
                    eprintln!("x2rock: stopping on {signal}");
                    return Ok(());
                }
            }
        }
        _ => {}
    }

    let mut state = State::load()?;
    let session = session::connect(cli.ip, &mut state, cli.household.as_deref(), room).await?;

    if let Command::Rooms { json } = cli.command {
        print_rooms(&session.groups, json);
        return Ok(());
    }

    // Like `rooms`, this is a whole-household view and must not be forced to a
    // single group; it queries every coordinator itself, so it runs here rather
    // than after the single-room resolution below.
    if let Command::Status { json, full } = cli.command {
        return print_status(&session, json, full).await;
    }

    // Favorites belong to the household, not a group, so listing them needs no
    // room and works when several groups would otherwise force a choice.
    if let Command::Favorites { query, json } = &cli.command {
        return content::favorites(&session, query.as_deref(), *json).await;
    }

    // Every speaker has its own firmware, so this asks each rather than the
    // group's coordinator - and needs no target at all.
    if let Command::Update { json } = &cli.command {
        let mut rows = Vec::new();
        for player in &session.groups.players {
            let found = match player.ip() {
                Some(ip) => Upnp::new(ip).software_update().await,
                None => Err(anyhow!("no address to reach it on")),
            };
            rows.push((player.name.clone(), found));
        }
        if *json {
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
        return Ok(());
    }

    // Players, not rooms - so this reads the topology rather than `getGroups`,
    // which has no word for a Sub. One player answers for the whole household,
    // and each one is then asked to describe itself.
    if let Command::System { json, redact } = &cli.command {
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
        print_system(&rows, *json, *redact);
        return Ok(());
    }

    // Household-wide, and addressed by id rather than by room, so these run
    // before a target is resolved - `alarms` in a two-group house must not
    // demand a --room it has no use for.
    if let Command::Alarms { action, json } = &cli.command {
        return speaker::alarms(&session, room, action.as_ref(), *json).await;
    }

    if let Command::Alarm { id, action } = &cli.command {
        return speaker::alarm(&session, *id, action).await;
    }

    if let Command::Raw {
        transport:
            RawTransport::Upnp {
                service,
                action,
                args,
                scope,
            },
    } = &cli.command
    {
        return raw_upnp(&session, room, service, action, args, *scope).await;
    }

    if let Command::Raw {
        transport:
            RawTransport::Api {
                namespace,
                command,
                options,
                scope,
                watch,
                session: session_id,
            },
    } = &cli.command
    {
        let options: serde_json::Value = match options.as_deref() {
            None => json!({}),
            Some(text) => serde_json::from_str(text)
                .with_context(|| format!("options must be a JSON object: {text}"))?,
        };
        ensure!(
            options.is_object(),
            "options must be a JSON object, not {}",
            match &options {
                serde_json::Value::Array(_) => "an array",
                serde_json::Value::Null => "null",
                _ => "a scalar",
            }
        );

        let mut envelope = json!({ "namespace": namespace, "command": command });
        // Group commands are answered by the coordinator, so a probe that does
        // not go there measures the wrong player's refusal.
        let mut connection = session.connection.clone();
        // A session id is an explicit address, so it wins over --scope rather
        // than combining with it: the two would name different targets.
        if let Some(id) = session_id {
            envelope["sessionId"] = json!(id);
        }
        match scope {
            _ if session_id.is_some() => {}
            RawScope::Household => {
                envelope["householdId"] = json!(session.connection.household_id().await?);
            }
            RawScope::Group => {
                let target = session::target(&session.groups, room)?;
                envelope["groupId"] = json!(target.group_id);
                connection = session::coordinator(&session, &target).await?;
            }
            RawScope::Player => {
                // A player answers player-scoped commands only for itself, so
                // naming one over a socket to another gets ERROR_INVALID_OBJECT_ID
                // - "Incorrect playerId" - for an id that is perfectly correct.
                let player = match room {
                    Some(room) => session.groups.player_named(room)?,
                    None => {
                        let id = session.groups.resolve(None)?.coordinator_id.clone();
                        session.groups.player(&id).ok_or_else(|| {
                            anyhow!("group coordinator {id} is not a known player")
                        })?
                    }
                };
                envelope["playerId"] = json!(player.id);
                if let Some(ip) = player.ip()
                    && ip != connection.ip()
                {
                    connection = Connection::open(ip).await?;
                }
            }
            RawScope::None => {}
        }

        // Attached before the command is sent: a subscribe can be answered by an
        // event that overtakes the reply, and a receiver created afterwards
        // would miss exactly the thing the probe went to see.
        let mut events = connection.events();

        let (header, body) = connection.command(envelope, options).await?;
        if header.success != Some(true) {
            let err: sonos::proto::ErrorBody =
                serde_json::from_value(body.clone()).unwrap_or_default();
            eprintln!(
                "{namespace} {command}: {}{}",
                err.error_code.as_deref().unwrap_or("refused"),
                err.reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            );
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "header": serde_json::to_value(&header)?,
                "body": body,
            }))?
        );

        if let Some(seconds) = watch {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(*seconds);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, events.recv()).await {
                    Err(_) => break,
                    Ok(Err(_)) => break,
                    Ok(Ok(event)) => {
                        if event.kind == sonos::proto::Event::LOST {
                            eprintln!("connection lost");
                            break;
                        }
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "event": event.kind,
                                "namespace": event.namespace,
                                "groupId": event.group_id,
                                "playerId": event.player_id,
                                "body": event.body,
                            }))?
                        );
                    }
                }
            }
        }
        return Ok(());
    }

    // Grouping resolves rooms itself: `ungroup` names its room positionally and
    // must work without --room, which the shared target resolution below would
    // refuse while the household has several groups.
    if let Command::Group { rooms } = &cli.command {
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
        if !already.is_empty() {
            eprintln!("Already in this group: {}", already.join(", "));
        }
        if joining.is_empty() {
            println!("{}", group_line(host, &session.groups));
            return Ok(());
        }
        let host_id = host.id.clone();
        let ids: Vec<String> = joining.iter().map(|(id, _)| id.clone()).collect();
        let target = session::target(&session.groups, room)?;
        let coordinator = session::coordinator(&session, &target).await?;
        let info = coordinator
            .modify_group_members(&host_id, &ids, &[])
            .await?;
        println!("{}", group_line(&info.group, &session.groups));
        return Ok(());
    }

    if let Command::Party { mode } = &cli.command {
        match mode.as_deref() {
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
                    println!("{}", group_line(host, &session.groups));
                    return Ok(());
                }
                let target = session::target(&session.groups, room)?;
                let coordinator = session::coordinator(&session, &target).await?;
                let info = coordinator
                    .modify_group_members(&host_id, &joining, &[])
                    .await?;
                println!("{}", group_line(&info.group, &session.groups));
            }
            Some("off") => {
                // Each group keeps its coordinator and loses everyone else, so
                // every player ends up a group of its own. Groups are
                // independent, so the snapshot taken at connect stays valid as
                // this walks it - only the group being changed changes.
                let mut broken = 0;
                for group in &session.groups.groups {
                    let leaving: Vec<String> = group
                        .player_ids
                        .iter()
                        .filter(|id| **id != group.coordinator_id)
                        .cloned()
                        .collect();
                    if leaving.is_empty() {
                        continue;
                    }
                    // Resolving with no name would pick the default group -
                    // some other group's coordinator, which refuses this one.
                    let Some(host) = session.groups.player(&group.coordinator_id) else {
                        eprintln!("{}: coordinator unknown, left as it is", group.name);
                        continue;
                    };
                    let target = session::target(&session.groups, Some(&host.name))?;
                    let coordinator = session::coordinator(&session, &target).await?;
                    coordinator
                        .modify_group_members(&group.id, &[], &leaving)
                        .await?;
                    broken += 1;
                }
                if broken == 0 {
                    println!("No rooms were grouped.");
                } else {
                    println!("Every room is on its own.");
                }
            }
            Some(other) => bail!("party takes no argument, or off (got {other:?})"),
        }
        return Ok(());
    }

    if let Command::Ungroup { room } = &cli.command {
        let leaving = session.groups.player_named(room)?;
        let Some(group) = session.groups.group_of(&leaving.id) else {
            bail!("{} is not in any group", leaving.name);
        };
        if group.player_ids.len() < 2 {
            println!("{:<24} was already on its own", leaving.name);
            return Ok(());
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
        let target = session::target(&session.groups, Some(room))?;
        let coordinator = session::coordinator(&session, &target).await?;
        let info = coordinator
            .modify_group_members(&group_id, &[], &[leaving_id])
            .await?;
        println!("{:<24} left {}", leaving_name, info.group.name);
        println!("{}", group_line(&info.group, &session.groups));
        return Ok(());
    }

    // `vol --each` sets every speaker in one group individually - the flatten
    // the group slider cannot do, because the slider preserves the members'
    // balance to match the Sonos app. It desugars to fanning `--player` over
    // the group's own members, read from the current topology, so all the
    // per-player machinery (per-speaker connection, clamp, fixed-volume
    // refusal, json) is reused rather than duplicated.
    if let Command::Vol {
        each: true,
        change,
        ramp,
        json,
        ..
    } = &cli.command
    {
        return volume::each(
            &session,
            room,
            cli.all,
            cli.room.len(),
            change.clone(),
            *ramp,
            *json,
        )
        .await;
    }

    // --all fans a per-room command across every group, resolved by each
    // group's coordinator name (a real room name; the composite group name is
    // not addressable). Already vetted against fans_out at the top of run().
    if cli.all {
        let every: Vec<String> = session
            .groups
            .groups
            .iter()
            .filter_map(|g| session.groups.player(&g.coordinator_id))
            .map(|p| p.name.clone())
            .collect();
        return fan_out(&session, &every, &cli.command).await;
    }

    // Several --room fan a per-room command across each, topology already in
    // hand from the one connect above. A single --room (or none) falls through
    // to the ordinary path; more than one on a command that does not fan out is
    // an error, not a silent act on the first.
    if cli.room.len() > 1 {
        if !fans_out(&cli.command) {
            return Err(too_many_rooms());
        }
        return fan_out(&session, &cli.room, &cli.command).await;
    }

    let target = session::target(&session.groups, room)?;
    let player = session::coordinator(&session, &target).await?;
    let group = target.group_id.as_str();

    match cli.command {
        Command::Now { json } => {
            let status = player.playback_status(group).await?;
            let meta = player.metadata(group).await?;
            if json {
                let services = catalogue::Catalogue::load();
                println!(
                    "{}",
                    now_json(&target.name, &status, &meta, Some(&services))
                );
            } else {
                println!("{}", now_line(&status, &meta));
            }
        }
        Command::Rate {
            direction,
            refresh,
            json,
        } => services::run_rate(&player, group, &target.name, direction, refresh, json).await?,
        Command::Play { track: None } => play_or_resume(&session, &player, &target).await?,
        Command::Play { track: Some(n) } => playback::play_track(&player, &target, n).await?,
        Command::Keep { name, container } => {
            content::keep(&session, &player, group, name, container).await?
        }
        Command::Bookmark { query, next } => {
            content::bookmark(&session, &player, &target, room, &query, next).await?
        }
        Command::Favorite { query } => {
            content::favorite(&session, &player, &target, group, &query).await?
        }
        Command::Playlist { query } => {
            content::playlist(&session, &player, &target, group, &query).await?
        }
        Command::Tv => speaker::tv(&session, &player, &target, room).await?,
        Command::Chime { volume } => {
            stream::play_audio_clip(&session, &target, room, None, volume).await?;
        }
        Command::Notify { url, volume } => {
            stream::require_http_url(&url)?;
            stream::play_audio_clip(&session, &target, room, Some(&url), volume).await?;
        }
        Command::Eq {
            bass,
            treble,
            loudness,
            json,
            trueplay,
            night,
            dialog,
        } => {
            let want = ToneRequest {
                bass,
                treble,
                loudness,
                trueplay,
                night,
                dialog,
            };
            apply_eq(&session, &target, room, want, json).await?
        }
        Command::Queue { action, json } => content::queue(&player, &target, action, json).await?,
        Command::Repeat { mode, json } => apply_repeat(&session, &target, mode, json).await?,
        Command::Shuffle { mode, json } => apply_shuffle(&session, &target, mode, json).await?,
        Command::Crossfade { mode, json } => apply_crossfade(&session, &target, mode, json).await?,
        Command::Remote {
            feedback,
            repeater,
            json,
        } => apply_remote(&session, &target, room, feedback, repeater, json).await?,
        Command::Rename { name } => {
            apply_rename(&session, &mut state, &target, room, &name).await?
        }
        Command::Led { mode, json } => apply_led(&session, &target, room, mode, json).await?,
        Command::Buttons { mode, json } => {
            apply_buttons(&session, &target, room, mode, json).await?
        }
        Command::Sleep { duration, json } => {
            apply_sleep(&target, player.ip(), duration, json).await?
        }
        Command::Snooze { duration, json } => {
            apply_snooze(&target, player.ip(), duration, json).await?
        }
        Command::Pause => player.playback(group, "pause").await?,
        Command::Toggle => player.playback(group, "togglePlayPause").await?,
        Command::Next => player.playback(group, "skipToNextTrack").await?,
        Command::Prev => player.playback(group, "skipToPreviousTrack").await?,
        Command::Vol {
            change,
            player: one_room,
            ramp,
            json,
            ..
        } => volume::apply_vol(&session, &target, room, change, one_room, ramp, json).await?,
        Command::Rooms { .. }
        | Command::Status { .. }
        | Command::Favorites { .. }
        | Command::Alarms { .. }
        | Command::Alarm { .. }
        | Command::Update { .. }
        | Command::System { .. }
        | Command::Group { .. }
        | Command::Ungroup { .. }
        | Command::Party { .. }
        | Command::Raw { .. }
        | Command::Bookmarks { .. }
        | Command::Search { .. }
        | Command::PlayItem { .. }
        | Command::PlayUrl { .. }
        | Command::Stations { .. }
        | Command::QueueItem { .. }
        | Command::Browse { .. }
        | Command::Link { .. }
        | Command::Unlink { .. }
        | Command::Accounts { .. }
        | Command::Discover
        | Command::Households { .. }
        | Command::Skill { .. }
        | Command::Desktop { .. }
        | Command::Service { .. }
        | Command::Completions { .. }
        | Command::Complete { .. }
        | Command::Tui
        | Command::Daemon { .. } => unreachable!("handled above"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::admin::SKILL;
    use clap::Parser;

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

    #[test]
    fn the_service_id_comes_off_the_art_url_encoded_or_not() {
        // The real Guest TV art URL: a YouTube Music HLS stream, sid=284
        // percent-encoded, while the metadata object carried the wrong 65435.
        let art = "http://192.168.86.31:1400/getaa?s=1&u=x-sonosapi-hls-static%3aALk\
                   SOiG%3fsid%3d284%26flags%3d8%26sn%3d2";
        assert_eq!(service_id_from_art(art), Some("284"));
        // Plain, unencoded form.
        assert_eq!(
            service_id_from_art("http://x/getaa?u=y?sid=212&flags=1"),
            Some("212")
        );
        // No sid (a TV or line-in art URL) yields nothing rather than a guess.
        assert_eq!(
            service_id_from_art("http://x/getaa?s=1&u=x-sonos-htastream"),
            None
        );
    }

    #[test]
    fn only_the_per_room_commands_fan_out() {
        // These act on one room's state, so several --room fan them out.
        assert!(fans_out(&Command::Pause));
        assert!(fans_out(&Command::Toggle));
        assert!(fans_out(&Command::Next));
        assert!(fans_out(&Command::Play { track: None }));
        assert!(fans_out(&Command::Vol {
            change: None,
            player: false,
            each: false,
            ramp: false,
            json: false
        }));
        assert!(fans_out(&Command::Repeat {
            mode: None,
            json: false
        }));
        assert!(fans_out(&Command::Shuffle {
            mode: None,
            json: false
        }));
        assert!(fans_out(&Command::Crossfade {
            mode: None,
            json: false
        }));
        // Playing a specific queue position is per-queue, not a broadcast.
        assert!(!fans_out(&Command::Play { track: Some(3) }));
        // Reads and whole-household commands are not fanned out.
        assert!(!fans_out(&Command::Now { json: false }));
        assert!(!fans_out(&Command::Status {
            json: false,
            full: false
        }));
        assert!(!fans_out(&Command::Rooms { json: false }));
    }

    /// A `playbackStatus` and a `metadataStatus` as the Media Room actually
    /// sent them (captured 2026-09-03), trimmed of fields nothing here reads.
    fn playing_body() -> (PlaybackStatus, MetadataStatus) {
        let status = serde_json::from_str(
            r#"{"_objectType":"playbackStatus","playbackState":"PLAYBACK_STATE_PLAYING",
                "positionMillis":33349,"queueVersion":"1","itemId":"2",
                "availablePlaybackActions":{"canPause":true,"canSeek":true,"canSkip":false},
                "playModes":{"repeat":false,"repeatOne":false,"shuffle":false}}"#,
        )
        .unwrap();
        let meta = serde_json::from_str(
            r#"{"_objectType":"metadataStatus",
                "container":{"_objectType":"container","name":"Bodies","type":"track",
                    "id":{"accountId":"sn_2","objectId":"ALkSOiGTPQu2","serviceId":"284"},
                    "service":{"id":"284","name":"YouTube Music"}},
                "currentItem":{"track":{"_objectType":"track","name":"Bodies",
                    "artist":{"name":"Offset, JID"},"album":{"name":"Bodies"},
                    "durationMillis":179000,"explicit":true,"tags":["TAG_EXPLICIT"],
                    "imageUrl":"http://192.168.77.94:1400/getaa?s=1&u=x-sonosapi-hls-static%3aALk%3fsid%3d284%26flags%3d65544%26sn%3d2"}},
                "nextItem":{"track":{"_objectType":"track","name":"Enemies",
                    "artist":{"name":"Offset"},"album":{"name":"KIARI:OFFSET"},"explicit":true}}}"#,
        )
        .unwrap();
        (status, meta)
    }

    /// Two groups, three players, one of them a joined pair - so the group list
    /// carries a composite label that is not a room name.
    fn two_group_household() -> Groups {
        Groups {
            groups: vec![
                Group {
                    id: "g:media".into(),
                    name: "Media Room".into(),
                    coordinator_id: "RINCON_1".into(),
                    playback_state: String::new(),
                    player_ids: vec!["RINCON_1".into()],
                },
                Group {
                    id: "g:dining".into(),
                    name: "Dining Room + 1".into(),
                    coordinator_id: "RINCON_2".into(),
                    playback_state: String::new(),
                    player_ids: vec!["RINCON_2".into(), "RINCON_3".into()],
                },
            ],
            players: vec![
                Player {
                    id: "RINCON_1".into(),
                    name: "Media Room".into(),
                    websocket_url: String::new(),
                    capabilities: vec![],
                },
                Player {
                    id: "RINCON_2".into(),
                    name: "Dining Room".into(),
                    websocket_url: String::new(),
                    capabilities: vec![],
                },
            ],
        }
    }

    #[test]
    fn the_room_default_hint_appears_only_where_it_helps() {
        let several = two_group_household();

        // The case it exists for: several rooms, nothing set.
        let hint = room_default_hint(&several, None).expect("several rooms, no default");
        assert!(hint.contains("export X2ROCK_ROOM='Media Room'"), "{hint}");

        // Quoted, and a *player* name - never the composite group label, which
        // is not a room and would not resolve.
        assert!(!hint.contains("Dining Room + 1"), "{hint}");

        // Already set: saying it again is telling someone what they know.
        assert!(room_default_hint(&several, Some("Kitchen")).is_none());
        // Blank counts as unset - clap would pass it on and it would resolve
        // to nothing.
        assert!(room_default_hint(&several, Some("")).is_some());
        assert!(room_default_hint(&several, Some("   ")).is_some());

        // One group needs no --room at all, so the line would be noise.
        let mut single = two_group_household();
        single.groups.truncate(1);
        assert!(room_default_hint(&single, None).is_none());
    }

    #[test]
    fn a_station_says_what_is_on_without_repeating_itself() {
        let (status, mut meta) = playing_body();

        // A live stream's only track information, appended to the line.
        meta.stream_info = Some("Eguana - Kineta Lounge".into());
        let line = now_line(&status, &meta);
        assert!(line.contains("· Eguana - Kineta Lounge"), "{line}");
        assert_eq!(
            now_json("Media Room", &status, &meta, None)["stream_info"],
            "Eguana - Kineta Lounge"
        );

        // Saying the same thing as the title is noise, not information.
        meta.stream_info = Some("Bodies".into());
        assert!(
            !now_line(&status, &meta).contains("· Bodies"),
            "the title is already Bodies"
        );

        // Whitespace-only is a station sending nothing, not a track called " ".
        meta.stream_info = Some("   ".into());
        assert!(now_json("Media Room", &status, &meta, None)["stream_info"].is_null());
    }

    fn volume(volume: u8, muted: bool) -> Volume {
        Volume {
            volume,
            muted,
            fixed: false,
        }
    }

    /// The keys `now --json` emits. The skill teaches agents to read these by
    /// name, so a rename or a drop breaks every consumer silently - and the
    /// binary is the side that has to be held to it, because prose cannot
    /// enforce itself.
    #[test]
    fn now_json_emits_exactly_the_documented_keys() {
        let (status, meta) = playing_body();
        let now = now_json("Media Room", &status, &meta, None);
        let mut keys: Vec<&str> = now
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "album",
                "art_url",
                "artist",
                "crossfade",
                "duration_ms",
                "explicit",
                "input_format",
                "next_artist",
                "next_title",
                "on_tv",
                "position_ms",
                "queue_position",
                "repeat",
                "room",
                "service",
                "service_id",
                "shuffle",
                "state",
                "stream_info",
                "surround",
                "title",
            ]
        );
        // Spot-check that the keys carry what they claim, so this cannot pass
        // on a body that parsed into nothing.
        assert_eq!(now["state"], "PLAYING");
        assert_eq!(now["title"], "Bodies");
        assert_eq!(now["artist"], "Offset, JID");
        assert_eq!(now["duration_ms"], json!(179000));
        assert_eq!(now["position_ms"], json!(33349));
        assert_eq!(now["repeat"], "off");
        assert_eq!(now["shuffle"], json!(false));
        assert_eq!(now["on_tv"], json!(false));
        // The four added by the parity pass, each from data the snapshot was
        // already fetching and throwing away.
        assert_eq!(now["queue_position"], json!(2));
        assert_eq!(now["explicit"], json!(true));
        assert_eq!(now["next_title"], "Enemies");
        assert_eq!(now["next_artist"], "Offset");
        assert_eq!(now["crossfade"], json!(false));
    }

    /// A stream has no position in a queue and nothing after it, and must say
    /// so with null rather than with a plausible number.
    #[test]
    fn a_stream_has_no_queue_position_and_no_next() {
        // `itemId` is an opaque hash off the queue, which is what makes parsing
        // it the discriminator rather than a guess about the source.
        let status: PlaybackStatus = serde_json::from_str(
            r#"{"playbackState":"PLAYBACK_STATE_PLAYING","itemId":"5zyo+/67QgriUYJZ8nB8ZwWcmqg="}"#,
        )
        .unwrap();
        let meta: MetadataStatus =
            serde_json::from_str(r#"{"container":{"name":"BTPM NPR","type":"station"}}"#).unwrap();
        let now = now_json("Media Room", &status, &meta, None);
        assert_eq!(now["queue_position"], serde_json::Value::Null);
        assert_eq!(now["next_title"], serde_json::Value::Null);
        assert_eq!(now["explicit"], serde_json::Value::Null);
    }

    /// The skill documents `now --json` as a **subset** of a `status` entry and
    /// names the seven fields only the latter has. Both directions are pinned:
    /// nothing group- or volume-shaped leaks into `now`, and a status entry adds
    /// nothing beyond those seven.
    #[test]
    fn a_status_entry_is_a_now_entry_plus_exactly_seven_room_facts() {
        let (status, meta) = playing_body();
        let now = now_json("Media Room", &status, &meta, None);
        let members = vec!["Media Room".to_string()];
        let facts = RoomFacts {
            name: "Media Room",
            members: &members,
            coordinator: Some("Media Room"),
            has_tv: false,
        };
        let entry = room_value(&facts, Ok((status, meta, Some(volume(2, false)))), None);

        let keys = |v: &serde_json::Value| -> std::collections::BTreeSet<String> {
            v.as_object().unwrap().keys().cloned().collect()
        };
        let now_keys = keys(&now);
        let entry_keys = keys(&entry);
        assert!(
            now_keys.is_subset(&entry_keys),
            "a status entry must still contain every now field"
        );
        let extra: Vec<&str> = entry_keys
            .difference(&now_keys)
            .map(String::as_str)
            .collect();
        assert_eq!(
            extra,
            [
                "audible",
                "coordinator",
                "fixed",
                "has_tv",
                "members",
                "muted",
                "volume"
            ]
        );
    }

    /// `audible` is the one read for "will this make a sound?", because muted
    /// and volume 0 are different fields with the same outcome. A room at
    /// volume 1 is audible - barely - which is true rather than "loud enough".
    #[test]
    fn audible_is_derived_from_both_mute_and_a_zero_level() {
        let members = vec!["Media Room".to_string()];
        for (level, muted, expected) in [
            (2, false, true),
            (1, false, true),
            (0, false, false),
            (2, true, false),
            (0, true, false),
        ] {
            let (status, meta) = playing_body();
            let facts = RoomFacts {
                name: "Media Room",
                members: &members,
                coordinator: Some("Media Room"),
                has_tv: false,
            };
            let entry = room_value(&facts, Ok((status, meta, Some(volume(level, muted)))), None);
            assert_eq!(
                entry["audible"],
                json!(expected),
                "volume {level}, muted {muted}"
            );
        }

        // A room that would not report its volume at all: null rather than
        // absent or guessed, so a consumer sees "unknown" instead of "silent".
        let (status, meta) = playing_body();
        let facts = RoomFacts {
            name: "Media Room",
            members: &members,
            coordinator: Some("Media Room"),
            has_tv: false,
        };
        let entry = room_value(&facts, Ok((status, meta, None)), None);
        assert_eq!(entry["audible"], serde_json::Value::Null);
    }

    /// A ramp slides one speaker, so it composes with the fan-outs that are
    /// *over speakers* and not with the one that is over groups. Only `--all`
    /// is refused, and that refusal lives in `run()` rather than in clap
    /// because `--all` is a global flag - parsing must succeed for the message
    /// to be able to name the reason.
    #[test]
    fn ramp_composes_with_the_per_speaker_fan_outs_but_not_with_all() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("x2rock").chain(args.iter().copied()))
        };
        for ok in [
            vec!["-r", "Kitchen", "vol", "30", "--ramp"],
            // Several rooms: the fan-out iterates the names as typed, each
            // resolved to its own speaker.
            vec!["-r", "a", "-r", "b", "vol", "30", "--ramp"],
            // --each rebuilds the command as --player over the group's members,
            // which is the shape a ramp already needs.
            vec!["vol", "30", "--ramp", "--each"],
        ] {
            assert!(parse(&ok).is_ok(), "{ok:?} should parse");
        }
        // Parses, then refused in run() - pinned by the fan-out test below.
        assert!(parse(&["--all", "vol", "30", "--ramp"]).is_ok());
    }

    /// The flag has to survive `per_room`'s rebuild, which is where it was
    /// previously dropped: a ramp that silently became a jump would look like
    /// the command simply ignoring `--ramp`.
    #[test]
    fn the_fan_out_carries_ramp_rather_than_dropping_it() {
        let command = Command::Vol {
            change: Some("30".into()),
            player: false,
            each: false,
            ramp: true,
            json: false,
        };
        match per_room(&command).expect("vol fans out") {
            PerRoom::Vol { ramp, .. } => assert!(ramp, "per_room dropped --ramp"),
            _ => panic!("expected PerRoom::Vol"),
        }
    }

    /// `fixed` is a different question from `audible` and has to survive
    /// beside it: a Port feeding an amp is perfectly audible and still refuses
    /// every volume command. An agent reading only `audible` would try.
    #[test]
    fn a_fixed_volume_room_says_so_in_status_without_claiming_silence() {
        let members = vec!["Study".to_string()];
        let facts = || RoomFacts {
            name: "Study",
            members: &members,
            coordinator: Some("Study"),
            has_tv: false,
        };

        let (status, meta) = playing_body();
        let fixed = Volume {
            volume: 40,
            muted: false,
            fixed: true,
        };
        let entry = room_value(&facts(), Ok((status, meta, Some(fixed))), None);
        assert_eq!(entry["fixed"], json!(true));
        assert_eq!(entry["audible"], json!(true), "fixed is not silent");

        let (status, meta) = playing_body();
        let entry = room_value(&facts(), Ok((status, meta, Some(volume(40, false)))), None);
        assert_eq!(entry["fixed"], json!(false));

        // Unknown rather than assumed-false when the room never answered, the
        // same way `volume` and `audible` are.
        let (status, meta) = playing_body();
        let entry = room_value(&facts(), Ok((status, meta, None)), None);
        assert_eq!(entry["fixed"], serde_json::Value::Null);
        assert_eq!(entry["volume"], serde_json::Value::Null);
        assert_eq!(entry["muted"], serde_json::Value::Null);
    }

    /// The other half of `the_embedded_skill_carries_its_frontmatter_and_contracts`.
    /// That one checks the skill still *says* the right things; this one checks
    /// the binary still emits what the skill says, so the two cannot drift in
    /// either direction. An added field that nobody documented fails here.
    #[test]
    fn every_field_a_status_entry_emits_is_documented_in_the_skill() {
        let (status, meta) = playing_body();
        let members = vec!["Media Room".to_string()];
        let facts = RoomFacts {
            name: "Media Room",
            members: &members,
            coordinator: Some("Media Room"),
            has_tv: false,
        };
        let entry = room_value(&facts, Ok((status, meta, Some(volume(2, false)))), None);

        for key in entry.as_object().unwrap().keys() {
            // Quoted, because that is how the skill's worked example writes
            // them - a bare substring would match half the prose.
            assert!(
                SKILL.contains(&format!("\"{key}\"")),
                "`{key}` is emitted but the skill never names it; \
                 an agent told to read fields cannot read this one"
            );
        }
    }

    /// The envelope's shape, which the skill promises as
    /// `{household, network, total, reachable, warnings, rooms}` - the one
    /// documented JSON shape nothing held until now.
    #[test]
    fn the_full_envelope_has_the_documented_shape() {
        let rooms = vec![json!({"room": "Media Room"})];
        let envelope =
            status_envelope(Some("Sonos_abc123"), Some("gw:192.168.77.1"), 3, &[], rooms);
        let mut keys: Vec<&str> = envelope
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "household",
                "network",
                "reachable",
                "rooms",
                "total",
                "warnings"
            ]
        );
        assert_eq!(envelope["household"], "Sonos_abc123");
        assert_eq!(envelope["total"], json!(3));
        assert_eq!(envelope["reachable"], json!(3));
        assert_eq!(envelope["warnings"], json!([]));
        // The rooms ride inside, rather than the envelope replacing them.
        assert_eq!(envelope["rooms"][0]["room"], "Media Room");
    }

    #[test]
    fn a_room_that_did_not_answer_is_counted_out_and_warned_about() {
        let unreachable = vec!["Kitchen".to_string(), "Study".to_string()];
        let envelope = status_envelope(None, None, 3, &unreachable, vec![]);
        assert_eq!(envelope["total"], json!(3));
        // `reachable` is what answered, not what exists.
        assert_eq!(envelope["reachable"], json!(1));
        assert_eq!(
            envelope["warnings"],
            json!(["Kitchen unreachable", "Study unreachable"])
        );
        // The household context is best-effort: null beats failing a snapshot
        // that otherwise succeeded.
        assert_eq!(envelope["household"], serde_json::Value::Null);
        assert_eq!(envelope["network"], serde_json::Value::Null);
    }

    /// `reachable` subtracts two counts that are measured in different places.
    /// It cannot go negative today; if it ever can, it must clamp rather than
    /// wrap, because a `usize` underflow would report ~1.8e19 reachable rooms
    /// into JSON an agent believes.
    #[test]
    fn reachable_clamps_instead_of_wrapping() {
        let unreachable = vec!["Kitchen".to_string(), "Study".to_string()];
        let envelope = status_envelope(None, None, 1, &unreachable, vec![]);
        assert_eq!(envelope["reachable"], json!(0));
    }

    /// As with a status entry, both directions. The skill writes the envelope's
    /// keys as a brace list rather than as JSON, so that list is parsed back out
    /// and compared - a key added to either side without the other fails.
    #[test]
    fn the_envelope_and_the_skill_name_the_same_fields() {
        let documented = SKILL
            .split_once("wraps the array in `{")
            .expect("the skill documents the --full envelope")
            .1
            .split_once("}`")
            .expect("the brace list is closed")
            .0;
        let mut documented: Vec<&str> = documented.split(',').map(str::trim).collect();
        documented.sort_unstable();

        let envelope = status_envelope(None, None, 0, &[], vec![]);
        let mut emitted: Vec<&str> = envelope
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        emitted.sort_unstable();

        assert_eq!(
            emitted, documented,
            "the envelope and the skill's brace list have drifted"
        );
    }

    #[test]
    fn an_unreachable_room_is_tagged_not_dropped() {
        // Proven live by unplugging a speaker: a coordinator that will not answer
        // must not sink the snapshot. Its entry carries the error and still the
        // room's identity, grouping and TV, so an agent is not blind about it -
        // and no playback state, so an error is never misread as "stopped".
        let members = vec!["Kitchen".to_string()];
        let facts = RoomFacts {
            name: "Kitchen",
            members: &members,
            coordinator: Some("Kitchen"),
            has_tv: false,
        };
        let v = room_value(
            &facts,
            Err(anyhow!(
                "timed out connecting to player at 192.168.86.26:1443"
            )),
            None,
        );
        assert_eq!(v["room"], "Kitchen");
        assert!(v["error"].as_str().unwrap().contains("timed out"), "{v}");
        assert_eq!(v["has_tv"], json!(false));
        assert_eq!(v["members"], json!(["Kitchen"]));
        assert_eq!(v["coordinator"], json!("Kitchen"));
        assert!(
            v.get("state").is_none(),
            "an errored room has no playback state"
        );
    }
}
