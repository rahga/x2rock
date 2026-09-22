//! The snapshot: `status`, `now` and `rooms`, and the one JSON shape behind
//! them. A `status` entry is a `now` entry plus exactly seven room facts, and
//! every key either emits is named in the skill - tests here hold both. The
//! fixtures the tests build a household from live in this file too, since
//! nothing else reads them.

use anyhow::Result;
use serde_json::json;

use super::{mmss, upnp_ip};
use crate::session::{self, Target};
use crate::sonos::local::Connection;
use crate::sonos::proto::{Groups, MetadataStatus, PlaybackStatus, Repeat, Volume};
use crate::sonos::upnp::Upnp;
use crate::{catalogue, hint, netid};

fn now_line(status: &PlaybackStatus, meta: &MetadataStatus) -> String {
    let track = meta.current_item.as_ref().and_then(|i| i.track.as_ref());
    let title = meta.title();
    let artist = track
        .and_then(|t| t.artist.as_ref())
        .and_then(|a| a.name.as_deref());
    let album = track.and_then(|t| t.collection());

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
    let repeating;
    if repeat != Repeat::Off {
        repeating = format!("repeat {}", repeat.as_str());
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
        "album": track.and_then(|t| t.collection()),
        // The show, when the episode belongs to one. Also folded into `album`
        // above so every existing reader shows something; here as well so a
        // reader that cares can tell an episode from a track.
        "podcast": track.and_then(|t| t.podcast.as_ref()).and_then(|p| p.name.as_deref()),
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
pub async fn print_status(session: &session::Session, json: bool, full: bool) -> Result<()> {
    let mut values = Vec::new();
    let mut lines = Vec::new();
    // Rooms whose coordinator did not answer - the "expected but unreachable"
    // an envelope warns about, and the count `reachable` is derived from.
    let mut unreachable: Vec<String> = Vec::new();
    // The cached catalogue names a service the player's metadata leaves blank
    // (YouTube Music now-playing carries the sid, not the name). Best-effort and
    // read-only - a cold or absent cache just leaves `service` null as before.
    let services = json.then(catalogue::Catalogue::load);
    // Everything that can be known without asking a speaker, per group - then
    // every coordinator asked at once. Sequentially each unreachable one
    // stacked its whole connect timeout onto a read-only command, N dark
    // coordinators making N×5s; together they cost one timeout at worst. The
    // same shape `system` uses for its device reads.
    let planned: Vec<_> = session
        .groups
        .groups
        .iter()
        .map(|group| {
            let target = session::target_for(&session.groups, group);
            let members: Vec<String> = session
                .groups
                .members(group)
                .iter()
                .map(|p| p.name.clone())
                .collect();
            // A soundbar's HDMI belongs to the player, so the group has a TV
            // input if any member does - the same rule `x2rock tv` uses.
            let has_tv = session.groups.members(group).iter().any(|p| p.has_tv());
            let coordinator = session
                .groups
                .player(&group.coordinator_id)
                .map(|p| p.name.as_str());
            (group, target, members, has_tv, coordinator)
        })
        .collect();
    let fetched_all = futures_util::future::join_all(
        planned
            .iter()
            .map(|(_, target, ..)| fetch_room(session, target)),
    )
    .await;
    for ((group, _, members, has_tv, coordinator), fetched) in planned.iter().zip(fetched_all) {
        let facts = RoomFacts {
            name: &group.name,
            members,
            coordinator: *coordinator,
            has_tv: *has_tv,
        };
        // A failure is this room's alone. Both branches push, so one
        // unreachable coordinator is tagged, never propagated - the snapshot
        // always describes the whole household.
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

pub fn print_rooms(groups: &Groups, json: bool) {
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

/// `x2rock now`: what `target` is playing, as one line or as the documented
/// JSON object.
pub async fn now(player: &Connection, target: &Target, json: bool) -> Result<()> {
    let status = player.playback_status(&target.group_id).await?;
    let meta = player.metadata(&target.group_id).await?;
    if json {
        let services = catalogue::Catalogue::load();
        let mut out = now_json(&target.name, &status, &meta, Some(&services));
        fill_missing_duration(player, target, &mut out).await;
        println!("{out}");
    } else {
        println!("{}", now_line(&status, &meta));
    }
    Ok(())
}

/// Ask the player for a duration the Control API did not carry.
///
/// **One room, and only when there is a gap to fill.** A URI set straight on
/// the transport - what `stream_url` does for a finite file - plays with no
/// duration in the Control API's metadata, while `GetPositionInfo` has it (see
/// [`Upnp::track_duration`]). So a single extra UPnP round trip buys back a
/// field that is otherwise silently null.
///
/// **`status` deliberately does not do this.** It answers for every room in one
/// call, and a conditional per-room UPnP request would turn the household
/// snapshot into N+1 of them - for a field that is null on the live streams
/// making up most of the rooms that would trigger it. `now` is the single-room
/// command, and can afford to ask.
///
/// Failure is silent: this is an improvement on a null, and a room that will
/// not answer UPnP should not turn a working `now` into an error.
async fn fill_missing_duration(player: &Connection, target: &Target, out: &mut serde_json::Value) {
    if !wants_duration_lookup(out) {
        return;
    }
    let upnp = Upnp::new(upnp_ip(target, player.ip()));
    if let Ok(Some(duration)) = upnp.track_duration().await {
        out["duration_ms"] = json!(duration.as_millis() as u64);
    }
}

/// Whether the extra round trip is worth making. Pure, so the two guards that
/// keep it from running on every `now` are pinned rather than assumed: a
/// duration the Control API already gave, and a room with nothing loaded, must
/// both cost nothing.
fn wants_duration_lookup(out: &serde_json::Value) -> bool {
    out["duration_ms"].is_null()
        && matches!(out["state"].as_str(), Some("PLAYING") | Some("PAUSED"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::admin::SKILL;
    use crate::sonos::proto::{Group, Player};
    use anyhow::anyhow;

    #[test]
    fn a_duration_is_only_looked_up_when_one_is_missing_and_something_is_playing() {
        let asks = |state: &str, duration: serde_json::Value| {
            wants_duration_lookup(&json!({ "state": state, "duration_ms": duration }))
        };
        // The gap this exists for: a file set straight on the transport.
        assert!(asks("PLAYING", json!(null)));
        assert!(asks("PAUSED", json!(null)));
        // Ordinary queue playback already has one - asking again would be a
        // round trip per `now` for nothing.
        assert!(!asks("PLAYING", json!(210000)));
        // And a room with nothing loaded has nothing to report either way.
        assert!(!asks("IDLE", json!(null)));
        assert!(!asks("BUFFERING", json!(null)));
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
                "podcast",
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
