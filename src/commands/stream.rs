//! Sound that is not in any catalogue: a stream by URL, an internet radio
//! station from the directory, and the short clips `chime` and `notify` play
//! over whatever is on. Also the stream *session* other commands fall back to
//! when the player refuses to queue something - `stream_item`, `StreamStart`
//! and the `Started` verdict that `play-url` and `stations --play` report.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::session;
use crate::sonos::local::Connection;
use crate::state::State;
use crate::{credentials, hint, save_refreshed_token, sonos, stations, streams};

/// Whether a stream is starting fresh or replacing one whose URL expired.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StreamStart {
    /// Started by a person or the bar widget. Says it is a direct stream, and
    /// does not wait for PLAYING: this is the path the widget takes through
    /// `play-item`, where ten seconds before the button responds would be a
    /// worse bug than the rare silent failure - and the URL was resolved by the
    /// service that owns the content, so that failure is rare.
    Fresh,
    /// `play` re-resolving a remembered stream. Waits for PLAYING, since a fresh
    /// URL that is also dead must come back as a failure rather than a cheerful
    /// "(starting)" - the whole point of confirming the play was to catch that
    /// - and says nothing about direct streams, having just resumed one.
    Resume,
}

/// Play a service item as a stream, alongside the queue rather than in it.
pub async fn stream_item(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    token: Option<&sonos::smapi::Token>,
    id: &str,
    title: &str,
    how: StreamStart,
) -> Result<()> {
    let mut refreshed = None;
    let uri = sonos::smapi::media_uri(service, token, id, &mut refreshed).await?;
    if let Some(new_token) = refreshed
        && let Ok(mut creds) = credentials::Credentials::load()
    {
        save_refreshed_token(&mut creds, &service.id, new_token);
    }
    // A direct stream, not a queued track: the player fetches a URL the service
    // signed, so it neither pauses-and-resumes nor survives that URL ageing out
    // - and when it ages out the room simply goes idle, which reads as a
    // mystery unless it was said here. Said on stderr, so a caller reading the
    // result is unaffected; this is the fallback path (a service with no queue
    // support here, Amazon Music on a Prime account among them), not the queued
    // one, so it is not on every play.
    if how == StreamStart::Fresh {
        eprintln!(
            "x2rock: {title:?} is playing as a direct stream from {}; a direct stream cannot \
             be paused and resumed, and its URL may stop working after a while.",
            service.name
        );
    }
    let wait = match how {
        StreamStart::Fresh => Duration::ZERO,
        StreamStart::Resume => STREAM_START,
    };
    let target = session::target(&session.groups, room)?;
    let (_, started) = stream_url(session, room, &uri, title, Some(service), wait).await?;
    // Remembered against the group's coordinator, so `play` can re-resolve a
    // fresh URL once this one expires - the player will hold only the dead URL
    // by then, not the item that made it. Non-fatal: a stream that plays but is
    // not remembered simply cannot be auto-resumed later.
    if let Err(e) = streams::Streams::remember(
        &target.coordinator_id,
        streams::Stream {
            service_id: service.id.clone(),
            item_id: id.to_string(),
            title: title.to_string(),
        },
    ) {
        eprintln!("x2rock: could not remember this stream for resume ({e:#})");
    }
    report_started(&target.name, title, Some(&service.name), &started)
}

/// How long to wait for a loaded stream to actually reach `PLAYING`.
///
/// Measured rather than picked: a stream sits in `TRANSITIONING`/`BUFFERING`
/// for several seconds first - about four for SomaFM over https, longer for
/// others - so anything under five would report a working station as broken.
/// Ten is comfortably past that. Only a *failure* spends the whole budget; the
/// happy path returns the moment it sees `PLAYING`.
pub const STREAM_START: Duration = Duration::from_secs(10);

/// How often to ask. Cheap - it is one Control API call to a player on the LAN.
const STREAM_POLL: Duration = Duration::from_millis(500);

/// What became of a stream after the player accepted it.
///
/// **Three outcomes, because there are three.** `loadStreamUrl` returning
/// success means only that the URL was taken, so collapsing this to
/// worked/failed would have to guess which of the other two a buffering room
/// is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Started {
    /// Reached `PLAYING`. Real sound.
    Playing,
    /// Still buffering when the wait ran out. **Not a failure** - a slow
    /// stream on its way to playing looks exactly like this, and calling it
    /// broken would be the same lie in the other direction.
    Starting,
    /// Idle or stopped at the deadline. This is the silent failure: the player
    /// took a URL it cannot play and said nothing about it.
    Silent,
    /// **The state could not be established**, so nothing is known about the
    /// stream either way. Two ways to get here, and neither is a verdict:
    /// every poll failed for the whole wait, or they answered without naming a
    /// state - which `PlaybackStatus::playback_state` documents as meaning
    /// *unchanged*, and therefore nothing.
    ///
    /// Distinct from `Silent` on purpose, and the distinction is the remedy: a
    /// silent stream means try another one, while this means stop and look at
    /// the room. Folding the two together also contradicted the loop's own
    /// rule - a failed poll is evidence about the poll, not about the stream.
    ///
    /// `answered` says which of the two ways it was: `false` means no poll got
    /// an answer at all, so the room is not talking to us; `true` means the
    /// room answered every poll and simply never named a state, so nothing is
    /// wrong with the connection and "the room is not answering" would be a
    /// lie. The two need different sentences, which is why the flag is carried
    /// rather than folded into the message here.
    ///
    /// `why` carries what the last failed poll said, where one failed at all.
    /// The polls do not only fail because a room went away: an API error body
    /// or a stale `groupId` fails deterministically for the whole wait and
    /// looks identical from here, so the message reports the cause it has
    /// instead of asserting one it does not.
    Unverified { answered: bool, why: Option<String> },
}

/// One wording for the three outcomes, shared by every caller that starts a
/// stream so they cannot describe the same result differently.
///
/// `Silent` is an error rather than a warning: the caller asked for sound and
/// there is none, and an agent driving this needs a non-zero exit and a code to
/// branch on rather than a cheerful line it has to go and disprove. No `fix` -
/// nothing here can mint a stream that plays.
fn report_started(room: &str, title: &str, on: Option<&str>, started: &Started) -> Result<()> {
    let on = on.map(|svc| format!(" on {svc}")).unwrap_or_default();
    match started {
        Started::Playing => println!("{room} — {title}{on}"),
        Started::Starting => println!("{room} — {title}{on} (starting)"),
        Started::Silent => {
            return Err(hint::Hint::new(
                format!(
                    "{room} took {title:?} and is still idle {}s later. The player accepts a \
                     stream URL it cannot play without complaining, so this stream most likely \
                     does not work - nothing is wrong with the room. Try another.",
                    STREAM_START.as_secs()
                ),
                "stream_did_not_play",
                None,
            )
            .into());
        }
        // **Its own code, not `no_player`.** These commands already emit
        // `no_player` before a stream is loaded, from `session::connect` - so
        // reusing it here would leave a caller branching on `code` unable to
        // tell "no speakers answered, nothing was loaded" from "the stream was
        // loaded and then the room went quiet on us", which are different
        // situations with different remedies.
        // One code, two sentences: a room that never answered and a room that
        // answered without naming a state are the same unknown to a caller
        // branching on `code`, but telling the second one to "find out why the
        // room is not answering" sends a person to debug a connection that is
        // demonstrably fine.
        Started::Unverified { answered, why } => {
            let message = if *answered {
                let because = why
                    .as_deref()
                    .map(|e| format!(" One poll along the way did fail, saying: {e}."))
                    .unwrap_or_default();
                format!(
                    "{room} took {title:?} and answered every poll for {}s without ever naming \
                     a playback state, so whether it is playing is unknown.{because} This is \
                     not a verdict on the stream and the room is reachable: check again with \
                     `x2rock now` before swapping the stream for another.",
                    STREAM_START.as_secs()
                )
            } else {
                let because = why
                    .as_deref()
                    .map(|e| format!(" The last attempt said: {e}."))
                    .unwrap_or_default();
                format!(
                    "{room} took {title:?}, but its state could not be read for {}s, so whether \
                     it is playing is unknown.{because} This is not a verdict on the stream: do \
                     not swap it for a different one, find out why the room is not answering.",
                    STREAM_START.as_secs()
                )
            };
            return Err(hint::Hint::new(message, "stream_unverified", None).into());
        }
    }
    Ok(())
}

/// The `--json` form of [`report_started`], so the two commands that start a
/// stream cannot describe success differently.
///
/// It exists because they did: `stations --play --json` printed a prose line on
/// success while rendering failures as JSON, which is the worst of both - a
/// caller told to branch on `code` could parse the failure and not the success.
/// A failure still routes through `report_started`, so the error shape stays
/// the standard `{error, code, fix}` rather than a second invented one.
fn report_started_json(room: &str, title: &str, url: &str, started: &Started) -> Result<()> {
    if matches!(started, Started::Silent | Started::Unverified { .. }) {
        return report_started(room, title, None, started);
    }
    // No `stream_info`: the player has often not read the station's metadata
    // yet at this instant, so reporting it would report null and mean nothing.
    // `x2rock now --json` is where to read it.
    println!(
        "{}",
        serde_json::json!({
            "room": room,
            "title": title,
            "url": url,
            "started": if *started == Started::Playing { "playing" } else { "starting" },
        })
    );
    Ok(())
}

/// Open a playback session in the room and load one stream URL into it.
///
/// The half of [`stream_item`] that has nothing to do with services, shared so
/// that `play-url` and a service's live stream cannot drift apart: they are the
/// same two calls to the same namespace, and the only difference is whether a
/// service gets named in the metadata. Returns the room's name, so the caller
/// can word its own confirmation.
///
/// **A session rather than the transport, on purpose.** `SetAVTransportURI`
/// with `x-rincon-mp3radio://<url>` also plays an arbitrary stream (verified
/// 2026-09-04, and see "A stream URL needs no service" in
/// docs/architecture.md), but it *replaces* what the room was doing and loses
/// the queue's position. A session plays alongside the queue and leaves it
/// exactly as it was, which is what a radio station should do.
async fn stream_url(
    session: &session::Session,
    room: Option<&str>,
    url: &str,
    title: &str,
    service: Option<&sonos::smapi::Service>,
    wait: Duration,
) -> Result<(String, Started)> {
    let target = session::target(&session.groups, room)?;
    let coordinator = session::coordinator(session, &target).await?;

    let opened = coordinator
        .call(
            json!({
                "namespace": "playbackSession:1",
                "command": "createSession",
                "groupId": target.group_id,
            }),
            json!({ "appId": "com.rahga.x2rock", "appContext": "cli" }),
        )
        .await?;
    let session_id = opened["sessionId"]
        .as_str()
        .ok_or_else(|| anyhow!("player opened a session but did not name it"))?;

    // stationMetadata is optional, but it is where the name the room displays
    // comes from; without it the stream plays with nothing to show. `service`
    // is omitted entirely for a bare URL - there is no service to name, and
    // naming a false one would put a wrong sid in the room's now-playing.
    let mut metadata = json!({ "name": title, "type": "station" });
    if let Some(service) = service {
        metadata["service"] = json!({ "name": service.name, "id": service.id });
    }
    coordinator
        .call(
            json!({
                "namespace": "playbackSession:1",
                "command": "loadStreamUrl",
                "sessionId": session_id,
            }),
            json!({
                "streamUrl": url,
                "playOnCompletion": true,
                "stationMetadata": metadata,
            }),
        )
        .await?;

    // **The load succeeding is not the stream playing.** `loadStreamUrl`
    // accepts a URL it cannot play and then leaves the room idle without ever
    // erroring, so a confirmation printed here would be a guess. The same
    // reasoning as `use_tv_input`, which waits and then goes and looks: a
    // lost - or in this case meaningless - answer is checked rather than
    // believed.
    if wait.is_zero() {
        return Ok((target.name.clone(), Started::Starting));
    }

    let deadline = tokio::time::Instant::now() + wait;
    let mut last = None;
    // Whether *any* poll came back at all, which is a different question from
    // what it said - and the one that separates a dead stream from a room that
    // went away.
    let mut answered = false;
    // Kept rather than discarded: it is the only evidence about *why* nothing
    // could be read, and the failure message would otherwise have to guess.
    let mut last_err = None;
    loop {
        // A failed poll is not evidence about the stream - it is evidence
        // about the poll. Keep asking until the deadline and let the last
        // reading that did arrive decide.
        match coordinator.playback_status(&target.group_id).await {
            Ok(status) => {
                answered = true;
                match status.state() {
                    Some("PLAYING") => return Ok((target.name.clone(), Started::Playing)),
                    Some(state) => last = Some(state.to_string()),
                    None => {}
                }
            }
            Err(e) => last_err = Some(format!("{e:#}")),
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(STREAM_POLL).await;
    }

    // Decided on the *last* reading rather than the first: a room is briefly
    // IDLE between taking the URL and starting to buffer, so an early look
    // would condemn every stream.
    let started = match (answered, last.as_deref()) {
        (true, Some("IDLE") | Some("STOPPED")) => Started::Silent,
        (true, Some(_)) => Started::Starting,
        // Answered, but never named a state for the whole wait. That field is
        // documented as meaning *unchanged* rather than stopped, so it is
        // evidence of nothing - which makes this unknown rather than either a
        // success or a dead stream. Reporting it as `Starting` would have been
        // a false success on a stream that may well be dead.
        //
        // `last_err` is carried here too, not just in the arm below: polls can
        // be mixed, some erroring while the ones that answer never name a
        // state, and that error is then the only evidence there is about why.
        // `answered` rides along so the message can say which of the two this
        // was - a room that answered without a state must not be described as
        // "not answering".
        (true, None) => Started::Unverified {
            answered: true,
            why: last_err,
        },
        (false, _) => Started::Unverified {
            answered: false,
            why: last_err,
        },
    };
    Ok((target.name.clone(), started))
}

/// `x2rock stations`: search the radio directory, and optionally play a hit.
///
/// **No player is needed to search**, only to `--play`, which is the same
/// bargain `search` strikes: the directory is on the internet and has nothing
/// to do with the household, so a listing works with every speaker off. The
/// connection is therefore made lazily, after the directory has answered.
#[allow(clippy::too_many_arguments)]
pub async fn run_stations(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    query: Option<&str>,
    tag: Option<&str>,
    country: Option<&str>,
    limit: u32,
    play: Option<usize>,
    no_wait: bool,
    json: bool,
) -> Result<()> {
    let found = stations::search(query, tag, country, limit).await?;
    if found.is_empty() {
        let what = query.or(tag).unwrap_or("that");
        bail!("nothing in the radio directory for {what:?}");
    }

    if let Some(n) = play {
        let station = found
            .get(n.checked_sub(1).unwrap_or(usize::MAX))
            .ok_or_else(|| {
                anyhow!(
                    "there is no result {n}: the directory returned {}",
                    found.len()
                )
            })?;
        let mut state = State::load()?;
        let session = session::connect(ip, &mut state, household, room).await?;
        let wait = if no_wait {
            Duration::ZERO
        } else {
            STREAM_START
        };
        // A directory row is a stranger's URL and the directory's own liveness
        // check is stale, so this is the one place the silent failure is
        // routine rather than exotic. That is why waiting is the default here.
        let (room_name, started) = stream_url(
            &session,
            room,
            &station.url_resolved,
            &station.name,
            None,
            wait,
        )
        .await?;
        if json {
            return report_started_json(&room_name, &station.name, &station.url_resolved, &started);
        }
        return report_started(&room_name, &station.name, None, &started);
    }

    if json {
        let rows: Vec<_> = found
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    // The key is `url` because it is the one to play - the
                    // directory's own `url` field is a playlist as often as not
                    // and is not carried out of `stations::Station`.
                    "url": s.url_resolved,
                    "codec": (!s.codec.is_empty()).then_some(&s.codec),
                    "bitrate": (s.bitrate > 0).then_some(s.bitrate),
                    "country": (!s.countrycode.is_empty()).then_some(&s.countrycode),
                    "tags": s.tags.split(',').filter(|t| !t.is_empty()).collect::<Vec<_>>(),
                    "votes": s.votes,
                    "hls": s.hls == 1,
                    "homepage": (!s.homepage.is_empty()).then_some(&s.homepage),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    let width = found.iter().map(|s| s.format().len()).max().unwrap_or(0);
    for (i, s) in found.iter().enumerate() {
        // HLS is marked rather than hidden. Sonos plays some of it and this
        // has not been surveyed, so the flag is passed on as the directory
        // reports it instead of being turned into a promise either way.
        let hls = if s.hls == 1 { "  [hls]" } else { "" };
        let where_ = if s.countrycode.is_empty() {
            String::new()
        } else {
            format!("  {}", s.countrycode)
        };
        println!(
            "{:>3}  {:<width$}{where_}  {}{hls}",
            i + 1,
            s.format(),
            s.name
        );
    }
    println!("\nPlay one with: x2rock stations --play <n>");
    Ok(())
}

/// Check a URL is one a speaker could fetch, and decide what the room shows.
///
/// Split out from [`run_play_url`] because it is the whole of what can be
/// judged without a speaker, and therefore the whole of what a test can pin.
fn stream_display_name(url: &str, title: Option<&str>) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| bad_stream_url(url))
        .map(|(s, r)| (s.to_lowercase(), r))?;
    if !matches!(scheme.as_str(), "http" | "https") || rest.is_empty() {
        return Err(bad_stream_url(url));
    }
    if let Some(title) = title {
        return Ok(title.to_owned());
    }
    // The host, not the last path segment: a stream URL's path is usually a
    // bitrate-and-format slug ("groovesalad-128-mp3") while the host names the
    // station. The player picks that slug when given nothing at all, which is
    // what makes this default worth having.
    Ok(rest
        .split('/')
        .next()
        .filter(|host| !host.is_empty())
        .unwrap_or(url)
        .to_owned())
}

fn bad_stream_url(url: &str) -> anyhow::Error {
    hint::Hint::new(
        format!(
            "{url:?} is not an http(s) URL. A stream URL is fetched by the speaker itself over \
             HTTP, so nothing else can be one - and note it must be reachable from the speaker \
             rather than from this machine."
        ),
        "bad_stream_url",
        None,
    )
    .into()
}

/// Whether a URL is one the speaker can fetch: `http` or `https` with a host.
/// The same rule `play-url` enforces, since an audio clip is fetched the same
/// way, by the player rather than by this machine.
pub fn require_http_url(url: &str) -> Result<()> {
    match url.split_once("://") {
        Some((scheme, rest))
            if matches!(scheme.to_lowercase().as_str(), "http" | "https") && !rest.is_empty() =>
        {
            Ok(())
        }
        _ => Err(bad_stream_url(url)),
    }
}

/// The reverse-DNS id every audio clip is tagged with. `loadAudioClip` requires
/// one - an absent `appId` is `ERROR_INVALID_PARAMETER` - and the player groups
/// a caller's clips under it.
const APP_ID: &str = "com.github.rahga.x2rock";

/// Play a clip on the room's *own* player - the shared body of `chime` (the
/// built-in sound, `stream_url` None) and `notify` (a URL). Player-scoped, so it
/// resolves the named room to its own speaker the way `vol --player` does rather
/// than to the group's coordinator: a chime should land on the room asked for,
/// not the whole group it happens to be playing with.
pub async fn play_audio_clip(
    session: &session::Session,
    target: &session::Target,
    room: Option<&str>,
    stream_url: Option<&str>,
    volume: Option<u8>,
) -> Result<()> {
    let this = match room {
        Some(name) => session.groups.player_named(name)?,
        None => session
            .groups
            .player(&target.coordinator_id)
            .ok_or_else(|| anyhow!("no player for {}", target.name))?,
    };
    let ip = this
        .ip()
        .ok_or_else(|| anyhow!("no address for {}", this.name))?;
    // Player-scoped, so it must ride the player's own socket, not a
    // coordinator's - the same rule the per-player volume path follows.
    let speaker = if ip == session.connection.ip() {
        session.connection.clone()
    } else {
        Connection::open(ip).await?
    };
    let name = if stream_url.is_some() {
        "x2rock notify"
    } else {
        "x2rock chime"
    };
    speaker
        .load_audio_clip(&this.id, APP_ID, name, stream_url, volume)
        .await?;
    // The clip is accepted, not measured - the player returns before it sounds,
    // and unlike a stream there is no state to poll, so this reports what was
    // sent rather than claiming it was heard.
    let what = if stream_url.is_some() {
        "clip"
    } else {
        "chime"
    };
    println!("{:<24} {what}", this.name);
    Ok(())
}

/// `x2rock play-url`: play a stream URL with no service behind it.
///
/// **The player fetches the URL, not this machine**, so the only validation
/// worth doing here is the scheme: anything else is the speaker's verdict to
/// give, and it gives it late. `loadStreamUrl` accepts a URL it cannot play
/// and then fails *silently*, minutes later, at `IDLE` - the same trap
/// `play_item` documents - so a wrong URL is reported by the room going quiet
/// rather than by an error. Nothing here can improve on that; saying so is the
/// next best thing.
pub async fn run_play_url(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    url: &str,
    title: Option<&str>,
    no_wait: bool,
    json: bool,
) -> Result<()> {
    let name = stream_display_name(url, title)?;
    let mut state = State::load()?;
    let session = session::connect(ip, &mut state, household, room).await?;
    let wait = if no_wait {
        Duration::ZERO
    } else {
        STREAM_START
    };
    let (room_name, started) = stream_url(&session, room, url, &name, None, wait).await?;
    if json {
        return report_started_json(&room_name, &name, url, &started);
    }
    report_started(&room_name, &name, None, &started)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stream_outcome_is_told_apart_from_the_others() {
        // Playing and Starting are both successes - a stream still buffering
        // when the wait ran out has not failed, and must not be reported as
        // though it had.
        assert!(report_started("Media Room", "SomaFM", None, &Started::Playing).is_ok());
        assert!(report_started("Media Room", "Jazz", Some("TuneIn"), &Started::Starting).is_ok());

        // Silent is the one that used to print a cheerful line. It is an error,
        // it carries a code to branch on, and it names the stream.
        let err = report_started("Media Room", ".977 Country", None, &Started::Silent).unwrap_err();
        assert_eq!(hint::of(&err).0, "stream_did_not_play");
        assert!(
            hint::of(&err).1.is_none(),
            "no fix: nothing here can mint a stream that plays"
        );
        let text = format!("{err:#}");
        assert!(text.contains(".977 Country"), "{text}");
        assert!(
            text.contains("nothing is wrong with the room"),
            "the room is not the fault and the message should say so: {text}"
        );

        // And "nobody answered" is a *different* failure from "the stream is
        // dead", because the remedy differs: trying more streams at a room
        // that has gone away is pointless. It must not carry the
        // stream_did_not_play code, and must not blame the stream.
        // "Could not be read" is a *third* thing, not a dead stream: its own
        // code, because these commands already emit `no_player` before a
        // stream is loaded and a caller must be able to tell those apart.
        let gone = report_started(
            "Media Room",
            "SomaFM",
            None,
            &Started::Unverified {
                answered: false,
                why: None,
            },
        )
        .unwrap_err();
        assert_eq!(hint::of(&gone).0, "stream_unverified");
        assert!(hint::of(&gone).1.is_none());
        let text = format!("{gone:#}").to_lowercase();
        assert!(text.contains("could not be read"), "{text}");
        // Case-insensitively, because the previous version of this assertion
        // checked for "Try another" while the message said "trying another" -
        // it passed on capitalisation alone and enforced nothing.
        assert!(
            !text.contains("another one") && !text.contains("try another"),
            "must not send a caller after a different stream: {text}"
        );

        // And it reports the cause it has rather than asserting one, because a
        // stale groupId fails the polls exactly like a room going away.
        let why = report_started(
            "Media Room",
            "SomaFM",
            None,
            &Started::Unverified {
                answered: false,
                why: Some("connection refused".into()),
            },
        )
        .unwrap_err();
        assert!(format!("{why:#}").contains("connection refused"), "{why:#}");

        // The *answered* flavour is the same code but must not tell anyone the
        // room is not answering - it answered every poll. It carries its mixed
        // evidence too, and points at re-checking rather than at connectivity.
        let mixed = report_started(
            "Media Room",
            "SomaFM",
            None,
            &Started::Unverified {
                answered: true,
                why: Some("stale groupId".into()),
            },
        )
        .unwrap_err();
        assert_eq!(hint::of(&mixed).0, "stream_unverified");
        let text = format!("{mixed:#}");
        assert!(text.contains("stale groupId"), "{text}");
        assert!(text.contains("answered every poll"), "{text}");
        assert!(
            !text.to_lowercase().contains("not answering"),
            "a room that answered must not be described as not answering: {text}"
        );
    }

    #[test]
    fn both_stream_commands_report_success_the_same_way() {
        // The bug this closes: `stations --play --json` printed prose on
        // success while rendering failure as JSON, so a caller could parse the
        // failure and not the success. One helper now serves both.
        assert!(
            report_started_json("Media Room", "SomaFM", "http://x/s", &Started::Playing).is_ok()
        );
        assert!(
            report_started_json("Media Room", "SomaFM", "http://x/s", &Started::Starting).is_ok()
        );
        // Failures keep the standard {error, code, fix} shape rather than a
        // second invented one, for both failing outcomes.
        for (outcome, code) in [
            (Started::Silent, "stream_did_not_play"),
            (
                Started::Unverified {
                    answered: false,
                    why: None,
                },
                "stream_unverified",
            ),
        ] {
            let err =
                report_started_json("Media Room", "SomaFM", "http://x/s", &outcome).unwrap_err();
            assert_eq!(hint::of(&err).0, code);
        }
    }

    #[test]
    fn a_stream_url_is_checked_and_named() {
        // A title wins outright.
        assert_eq!(
            stream_display_name(
                "http://ice1.somafm.com/groovesalad-128-mp3",
                Some("Groove Salad")
            )
            .unwrap(),
            "Groove Salad"
        );
        // Without one, the host - not the path slug the player would pick.
        assert_eq!(
            stream_display_name("http://ice1.somafm.com/groovesalad-128-mp3", None).unwrap(),
            "ice1.somafm.com"
        );
        assert_eq!(
            stream_display_name("https://example.test:8000/stream?x=1", None).unwrap(),
            "example.test:8000"
        );
        assert_eq!(
            stream_display_name("HTTP://Example.Test/s", None).unwrap(),
            "Example.Test",
            "the scheme is matched case-insensitively without lowercasing the host"
        );
    }

    #[test]
    fn only_a_fetchable_scheme_is_a_stream_url() {
        // The player fetches this over HTTP; nothing else can be a stream URL.
        for bad in [
            "ice1.somafm.com/stream",
            "file:///tmp/x.mp3",
            "x-rincon-mp3radio://ice1.somafm.com/s",
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
            "http://",
        ] {
            let err = stream_display_name(bad, None).unwrap_err();
            assert_eq!(hint::of(&err).0, "bad_stream_url", "{bad} was accepted");
            // No fix: nothing here can mint a working URL for the caller.
            assert!(hint::of(&err).1.is_none(), "{bad} handed out a fix");
        }
    }

    #[test]
    fn notify_accepts_only_a_url_the_player_can_fetch() {
        // `notify` fetches the clip from the player, so it holds the stream-URL
        // rule: http/https with a host, and the same `bad_stream_url` code.
        assert!(require_http_url("http://x/s.mp3").is_ok());
        assert!(require_http_url("https://EXAMPLE.test/clip.wav").is_ok());
        for bad in ["file:///tmp/x.mp3", "x.mp3", "http://", "spotify:track:1"] {
            let err = require_http_url(bad).unwrap_err();
            assert_eq!(hint::of(&err).0, "bad_stream_url", "{bad} was accepted");
        }
    }
}
