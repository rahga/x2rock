//! Transport on a group - play, pause, skip, repeat, shuffle, crossfade - and
//! the part of `play` that is not a button press: confirming the room actually
//! started, telling a refusal from a lost socket, and re-resolving a direct
//! stream whose signed URL has expired. `fan_out` in `mod.rs` calls the
//! `apply_*` functions once per group; `run` calls them for a single room.

use anyhow::{Result, anyhow, bail, ensure};
use serde_json::json;

use super::stream::{STREAM_START, StreamStart, stream_item};
use super::{on_word, transition};
use crate::session::{self, Target};
use crate::sonos::local::Connection;
use crate::sonos::proto::{MetadataStatus, Repeat};
use crate::sonos::upnp::Upnp;
use crate::{catalogue, credentials, hint, sonos, streams};

/// Set or read repeat, printing the outcome. Shared by the single arm and fan-out.
pub async fn apply_repeat(
    session: &session::Session,
    target: &session::Target,
    mode: Option<String>,
    json: bool,
) -> Result<()> {
    let group = target.group_id.as_str();
    let player = session::coordinator(session, target).await?;
    let status = player.playback_status(group).await?;
    let before = status.modes().repeat();
    let after = match mode.as_deref() {
        None => before,
        Some(text) => {
            let Some(repeat) = Repeat::parse(text) else {
                bail!("repeat takes off, all or one");
            };
            ensure!(
                status.actions().allows(repeat),
                "what {} is playing cannot be {}",
                target.name,
                repeat.denied_as()
            );
            player.set_repeat(group, repeat).await?;
            repeat
        }
    };
    if json {
        println!(
            "{}",
            json!({ "room": target.name, "repeat": after.as_str() })
        );
    } else {
        let from = transition(before.as_str(), after.as_str());
        println!("{:<24} repeat {from}{}", target.name, after.as_str());
    }
    Ok(())
}

/// Set or read shuffle, printing the outcome. Shared by the single arm and fan-out.
pub async fn apply_shuffle(
    session: &session::Session,
    target: &session::Target,
    mode: Option<String>,
    json: bool,
) -> Result<()> {
    let group = target.group_id.as_str();
    let player = session::coordinator(session, target).await?;
    let status = player.playback_status(group).await?;
    let before = status.modes().shuffle;
    let after = match mode.as_deref() {
        None => before,
        Some(text @ ("on" | "off")) => {
            let shuffle = text == "on";
            ensure!(
                !shuffle || status.actions().can_shuffle,
                "what {} is playing cannot be shuffled",
                target.name
            );
            player.set_shuffle(group, shuffle).await?;
            shuffle
        }
        Some(_) => bail!("shuffle takes on or off"),
    };
    if json {
        println!("{}", json!({ "room": target.name, "shuffle": after }));
    } else {
        let from = transition(on_word(before), on_word(after));
        println!("{:<24} shuffle {from}{}", target.name, on_word(after));
    }
    Ok(())
}

/// Crossfade, which is a play mode like shuffle and set the same way.
pub async fn apply_crossfade(
    session: &session::Session,
    target: &session::Target,
    mode: Option<String>,
    json: bool,
) -> Result<()> {
    let group = target.group_id.as_str();
    let player = session::coordinator(session, target).await?;
    let before = player.playback_status(group).await?.modes().crossfade;
    let after = match mode.as_deref() {
        None => before,
        Some(text @ ("on" | "off")) => {
            let crossfade = text == "on";
            player.set_crossfade(group, crossfade).await?;
            crossfade
        }
        Some(_) => bail!("crossfade takes on or off"),
    };
    if json {
        println!("{}", json!({ "room": target.name, "crossfade": after }));
    } else {
        let from = transition(on_word(before), on_word(after));
        println!("{:<24} crossfade {from}{}", target.name, on_word(after));
    }
    Ok(())
}

pub async fn apply_transport(
    session: &session::Session,
    target: &session::Target,
    verb: &str,
) -> Result<()> {
    let coordinator = session::coordinator(session, target).await?;
    transport(&coordinator, &target.group_id, verb).await
}

/// One transport verb, to a group whose coordinator the caller already holds.
///
/// The bottom of both paths: [`apply_transport`] resolves a coordinator and
/// calls this, and `run` calls it directly for the four bare transport
/// commands, which reach it with the coordinator already open. Routing those
/// through `apply_transport` instead would resolve the same coordinator a
/// second time, and on a grouped room that is a second socket.
pub async fn transport(player: &Connection, group: &str, verb: &str) -> Result<()> {
    player.playback(group, verb).await
}

/// Ask a room to play, then confirm it actually did.
///
/// `play` on a room whose source has gone stale - an expired stream URL, a
/// track a service will no longer serve - returns success while the player
/// drops straight back to idle and raises a `playbackError` a beat later. So
/// this does not trust the command's own reply: it watches `playback:1` until
/// the room reaches PLAYING, an error the player raises, or a short deadline
/// with the room still idle. The reasoning is `stream_url`'s - a loaded stream
/// that never plays says nothing on its own - reached here through resume
/// rather than a fresh load.
async fn play_confirmed(player: &Connection, upnp: &Upnp, group: &str, room: &str) -> Result<()> {
    // Subscribed, and the receiver attached, before the play is sent: the error
    // can overtake the command's own reply, and a receiver opened afterwards
    // would miss exactly the event this went to see - the rule `raw api --watch`
    // and the stream loader both follow.
    player.subscribe_group("playback:1", group).await?;
    let mut events = player.events();
    // A dead source fails in one of two shapes: the player takes the play and
    // raises a `playbackError` a beat later (an expired Amazon URL did), or it
    // refuses the play outright with the same code (a URL that never loaded
    // does). Both are "the room cannot play what it holds", and both must read
    // as `playback_failed`, since that is what the resume waits for.
    if let Err(e) = player.playback(group, "play").await {
        return Err(match refused_play(&e) {
            Some(error) => playback_failed(room, &error, upnp).await,
            None => e,
        });
    }

    // A play on an already-playing room raises no transition event to wait for,
    // and an instant resume has often already landed by now: one status read
    // settles both without spending the failure budget on a room that is fine.
    if player.playback_status(group).await?.state() == Some("PLAYING") {
        return Ok(());
    }

    let deadline = tokio::time::Instant::now() + STREAM_START;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // A lost socket or a closed channel is not a verdict on the play; fall
        // through to the one status read below rather than decide from here.
        let Ok(Ok(event)) = tokio::time::timeout(remaining, events.recv()).await else {
            break;
        };
        if event.namespace != "playback:1" {
            continue;
        }
        // An error deserializes cleanly into a status ("nothing changed"), so it
        // is tested for first, before the body is read as one.
        if let Some(error) = sonos::proto::playback_error(&event.body) {
            return Err(playback_failed(room, &error, upnp).await);
        }
        if let Ok(status) =
            serde_json::from_value::<sonos::proto::PlaybackStatus>(event.body.clone())
            && status.state() == Some("PLAYING")
        {
            return Ok(());
        }
    }

    // No error and no PLAYING within the window. A room still buffering is on
    // its way and must not be called broken; only one left idle is the silent
    // failure this exists to catch.
    match player.playback_status(group).await?.state() {
        Some("IDLE") | Some("STOPPED") | None => Err(stayed_idle(room)),
        _ => Ok(()),
    }
}

/// A `play` the player refused because it cannot play what it holds, read off
/// the typed refusal; `None` for any other failure (a lost socket, a stale
/// group id), which is not about the source and must not be treated as one.
fn refused_play(e: &anyhow::Error) -> Option<sonos::proto::PlaybackError> {
    let api = e.downcast_ref::<sonos::local::ApiError>()?;
    (api.code.as_deref() == Some("ERROR_PLAYBACK_FAILED")).then(|| sonos::proto::PlaybackError {
        error_code: api.code.clone(),
        reason: api.reason.clone(),
        track_name: None,
        service_name: None,
    })
}

/// The player raised a `playbackError` on a play: an expired stream URL, or a
/// track a service pulled. One code with [`stayed_idle`], since the remedy is
/// the same - the source is gone, so load a fresh one - and the sentence names
/// which of the two it was.
///
/// Which source failed is asked of the coordinator only now, on the failure
/// path; a read that fails gives the stream wording rather than hiding the
/// play's own error.
async fn playback_failed(
    room: &str,
    error: &sonos::proto::PlaybackError,
    upnp: &Upnp,
) -> anyhow::Error {
    let from_queue = upnp.playing_from_queue().await.unwrap_or(false);
    playback_failed_message(room, error, from_queue)
}

fn playback_failed_message(
    room: &str,
    error: &sonos::proto::PlaybackError,
    from_queue: bool,
) -> anyhow::Error {
    let message = if from_queue {
        format!(
            "{room}: {error}. Nothing is playing now. That track is in the room's queue \
             and its service would not serve it - a queued track can expire or be pulled. \
             `x2rock queue` lists what else is there: `play N` plays another track, and \
             `queue remove N` drops this one."
        )
    } else {
        format!(
            "{room}: {error}. Nothing is playing now. If the room was on a direct stream \
             (some services have no queue here, so x2rock streams them - Amazon Music on \
             a Prime account among them), its URL has most likely expired; start it again \
             with `favorite`, `bookmark`, or a fresh search to fetch a new one."
        )
    };
    hint::Hint::new(message, "playback_failed", None).into()
}

/// A play the player accepted without ever leaving idle, and without raising an
/// error to say why - most often a room with nothing loaded.
fn stayed_idle(room: &str) -> anyhow::Error {
    hint::Hint::new(
        format!(
            "{room}: asked to play, but still idle {}s later, with no error from the \
             player. The room most likely has nothing loaded - `x2rock now` shows what it \
             holds, and `favorite`, `bookmark` or a search starts something.",
            STREAM_START.as_secs()
        ),
        "playback_failed",
        None,
    )
    .into()
}

/// Ask a room to play; if that fails on a dead source x2rock started as a direct
/// stream, resume it from the remembered item. The one play path for a single
/// room, `--all` and several `-r` alike, so what SKILL.md says of `play` holds
/// wherever `play` is typed.
pub async fn play_or_resume(
    session: &session::Session,
    player: &Connection,
    target: &session::Target,
) -> Result<()> {
    let upnp = Upnp::new(target.coordinator_ip.unwrap_or(player.ip()));
    let failed = match play_confirmed(player, &upnp, &target.group_id, &target.name).await {
        Ok(()) => return Ok(()),
        Err(e) if hint::of(&e).0 == "playback_failed" => e,
        Err(e) => return Err(e),
    };
    match try_resume_stream(session, player, target).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(failed),
        // A resume that failed for a *named* reason - the fresh URL did not play
        // either, an account needs relinking - is the sharper report, and its
        // code is the one to act on. One that failed for no named reason must
        // not bury the original: the code and remedy a caller branches on stay
        // the play's, and the resume's failure joins the sentence.
        Err(resume) => match hint::of(&resume).0 {
            "unknown" => Err(hint::Hint::new(
                format!("{failed:#} A resume was tried and failed too: {resume:#}"),
                "playback_failed",
                None,
            )
            .into()),
            _ => Err(resume),
        },
    }
}

/// `play` failed on a room whose direct stream x2rock started: re-resolve a
/// fresh URL from the remembered item and play that. `Ok(true)` when a stream
/// was resumed, `Ok(false)` when there was nothing to resume - no remembered
/// stream, the room has moved on, or the service is no longer known - which
/// leaves the caller's original error to stand.
async fn try_resume_stream(
    session: &session::Session,
    player: &Connection,
    target: &session::Target,
) -> Result<bool> {
    let Some(stream) = streams::Streams::load()
        .get(&target.coordinator_id)
        .cloned()
    else {
        return Ok(false);
    };
    // Refreshed, as every other by-id lookup does: a cleared or schema-bumped
    // cache would otherwise make this give up until some `search` happened to
    // rebuild it, and the failure would look like the URL's.
    let mut catalogue = catalogue::Catalogue::load();
    catalogue
        .refresh(&Upnp::new(session.connection.ip()), false)
        .await?;
    let Some(service) = catalogue.by_id(&stream.service_id).cloned() else {
        return Ok(false);
    };
    let meta = player.metadata(&target.group_id).await?;
    if !holds_stream(&meta, &stream, &service.name) {
        return Ok(false);
    }

    // The same fallback that started it - a refreshed URL plays exactly as the
    // first did. Addressed by the coordinator's own room name, since a group's
    // display name is a composite no player answers to.
    let coord_room = session
        .groups
        .player(&target.coordinator_id)
        .map(|p| p.name.as_str())
        .ok_or_else(|| anyhow!("no player for {}", target.name))?;
    let token = credentials::Credentials::load()?.token_for(&service.id);
    eprintln!(
        "x2rock: {:?} was a direct stream whose URL expired; fetching a fresh one.",
        stream.title
    );
    stream_item(
        session,
        Some(coord_room),
        &service,
        token.as_ref(),
        &stream.item_id,
        &stream.title,
        StreamStart::Resume,
    )
    .await?;
    Ok(true)
}

/// Whether the room still holds the remembered stream, read off its metadata.
///
/// The container is what echoes the `stationMetadata` a direct stream was
/// loaded with - its name is the title x2rock gave, its service the one named -
/// while `currentItem.track` is the stream's *own* now-playing, the song a
/// station is on, which changes under the same stream (an iHeartRadio station
/// remembered as "The BIG 98" shows "Springsteen" there). So the container is
/// what is compared, and the track only when there is no container to read.
/// The player keeps an expired stream's metadata, so a match means the dead
/// stream is what just failed; a mismatch means the room moved on and the note
/// is stale, and resuming would start something the room is not on.
fn holds_stream(meta: &MetadataStatus, stream: &streams::Stream, service_name: &str) -> bool {
    let title = Some(stream.title.as_str());
    match meta.container.as_ref() {
        Some(c) => {
            c.name.as_deref() == title
                && c.service
                    .as_ref()
                    .and_then(|s| s.name.as_deref())
                    .is_none_or(|name| name == service_name)
        }
        None => {
            meta.current_item
                .as_ref()
                .and_then(|i| i.track.as_ref())
                .and_then(|t| t.name.as_deref())
                == title
        }
    }
}

/// `x2rock play N`: play queue track `n` on `target`, switching the group
/// back to its queue first if a station or line-in had replaced it.
pub async fn play_track(player: &Connection, target: &Target, n: u32) -> Result<()> {
    ensure!(n >= 1, "queue tracks are numbered from 1");
    // The queue lives on the coordinator and only UPnP can address it by
    // position. Make sure the queue is the source first: after a radio
    // station or line-in it is not, and Seek would fail with error 701.
    let upnp = Upnp::new(target.coordinator_ip.unwrap_or(player.ip()));
    if !upnp.playing_from_queue().await? {
        upnp.use_queue(&target.coordinator_id).await?;
    }
    upnp.seek_track(n).await?;
    play_confirmed(player, &upnp, &target.group_id, &target.name).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_play_is_a_playback_failure_and_a_lost_socket_is_not() {
        let refused: anyhow::Error = sonos::local::ApiError {
            what: "playback:1 play".into(),
            code: Some("ERROR_PLAYBACK_FAILED".into()),
            reason: None,
        }
        .into();
        // The forced live test: a URL that never loaded, refused at the command.
        let error = refused_play(&refused).expect("a playback refusal");
        assert_eq!(error.error_code.as_deref(), Some("ERROR_PLAYBACK_FAILED"));
        for from_queue in [false, true] {
            assert_eq!(
                hint::of(&playback_failed_message("Media Room", &error, from_queue)).0,
                "playback_failed"
            );
        }
        let queued = playback_failed_message("Media Room", &error, true).to_string();
        assert!(queued.contains("queue remove") && !queued.contains("direct stream"));
        // Context wrapped around it still downcasts - anyhow reaches through.
        let wrapped = refused.context("on room \"Media Room\"");
        assert!(refused_play(&wrapped).is_some());
        // Other refusals, and plain failures, are not about the source.
        let other: anyhow::Error = sonos::local::ApiError {
            what: "playback:1 play".into(),
            code: Some("ERROR_INVALID_OBJECT_ID".into()),
            reason: Some("Incorrect groupId".into()),
        }
        .into();
        assert!(refused_play(&other).is_none());
        assert!(refused_play(&anyhow!("connection to player was lost")).is_none());
        // The sentence reads exactly as the flattened one always did.
        assert_eq!(
            other.to_string(),
            "playback:1 play failed: ERROR_INVALID_OBJECT_ID (Incorrect groupId)"
        );
    }

    #[test]
    fn a_room_holds_the_remembered_stream_by_its_container_not_its_song() {
        let stream = streams::Stream {
            service_id: "6".into(),
            item_id: "live_stations.2157".into(),
            title: "The BIG 98".into(),
        };
        let meta =
            |json: serde_json::Value| -> MetadataStatus { serde_json::from_value(json).unwrap() };
        // The capture in architecture.md: the station in the container, the
        // song it is on in currentItem. It is the station that was remembered.
        let station = meta(json!({
            "container": {"name": "The BIG 98", "service": {"name": "iHeartRadio"}},
            "currentItem": {"track": {"name": "Springsteen", "artist": {"name": "Eric Church"}}}
        }));
        assert!(holds_stream(&station, &stream, "iHeartRadio"));
        // A different station on the same service: the room moved on.
        let other = meta(json!({
            "container": {"name": "Some Other Station", "service": {"name": "iHeartRadio"}}
        }));
        assert!(!holds_stream(&other, &stream, "iHeartRadio"));
        // Same title, different service: not the same stream.
        let elsewhere = meta(json!({
            "container": {"name": "The BIG 98", "service": {"name": "TuneIn (New)"}}
        }));
        assert!(!holds_stream(&elsewhere, &stream, "iHeartRadio"));
        // No service named on the container is not a mismatch.
        let unnamed = meta(json!({"container": {"name": "The BIG 98"}}));
        assert!(holds_stream(&unnamed, &stream, "iHeartRadio"));
        // With no container at all, the track name is the only thing to read.
        let bare = meta(json!({"currentItem": {"track": {"name": "The BIG 98"}}}));
        assert!(holds_stream(&bare, &stream, "iHeartRadio"));
        assert!(!holds_stream(&meta(json!({})), &stream, "iHeartRadio"));
    }
}
