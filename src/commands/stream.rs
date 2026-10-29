//! Sound that is not in any catalogue: a stream by URL, an internet radio
//! station from the directory, and the short clips `chime` and `notify` play
//! over whatever is on. Also the stream *session* other commands fall back to
//! when the player refuses to queue something - `stream_item`, `StreamStart`
//! and the `Started` verdict that `play-url` and `stations --play` report.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use super::nth;
use super::speaker::named_speaker;
use crate::session::{self};
use crate::state::State;
use crate::{hint, stations};
#[allow(unused_imports)]
pub use x2rock::play::*;

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
        let station = nth(&found, n).ok_or_else(|| {
            anyhow!(
                "there is no result {n}: the directory returned {}",
                found.len()
            )
        })?;
        // A directory row is a stranger's URL and the directory's own liveness
        // check is stale, so this is the one place the silent failure is
        // routine rather than exotic. That is why waiting is the default here.
        let reach = Reach {
            ip,
            household,
            room,
        };
        return play_stream(reach, &station.url_resolved, &station.name, no_wait, json).await;
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
    require_http_url(url)?;
    if let Some(title) = title {
        return Ok(title.to_owned());
    }
    // The host, not the last path segment: a stream URL's path is usually a
    // bitrate-and-format slug ("groovesalad-128-mp3") while the host names the
    // station. The player picks that slug when given nothing at all, which is
    // what makes this default worth having. The scheme is present - the check
    // above passed - so the split cannot miss.
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
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
        hint::Code::BadStreamUrl,
        None,
    )
    .into()
}

/// Whether a URL is one the speaker can fetch: `http` or `https` with a host.
/// The same rule `play-url` enforces, since an audio clip is fetched the same
/// way, by the player rather than by this machine.
pub fn require_http_url(url: &str) -> Result<()> {
    match url.split_once("://") {
        Some((scheme, rest)) if matches!(scheme.to_lowercase().as_str(), "http" | "https") => {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            let host_part = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            let host = host_part
                .strip_prefix('[')
                .and_then(|h| h.split_once(']'))
                .map_or_else(
                    || host_part.split(':').next().unwrap_or(""),
                    |(ipv6, _)| ipv6,
                );
            if !host.is_empty() {
                return Ok(());
            }
            Err(bad_stream_url(url))
        }
        _ => Err(bad_stream_url(url)),
    }
}

/// Play a clip on the room's *own* player - the shared body of `chime` (the
/// built-in sound, `stream_url` None) and `notify` (a URL). Player-scoped, so it
/// resolves the named room to its own speaker the way `vol --player` and every
/// other per-speaker command do (`named_speaker`) rather than to the group's
/// coordinator: a chime should land on the room asked for, not the whole group
/// it happens to be playing with.
pub async fn play_audio_clip(
    session: &session::Session,
    target: &session::Target,
    room: Option<&str>,
    stream_url: Option<&str>,
    volume: Option<u8>,
) -> Result<()> {
    // Only the address is wanted off the UPnP handle: the clip goes over the
    // Control API, which is the one wire that carries `loadAudioClip`.
    let (this, upnp) = named_speaker(session, target, room)?;
    let ip = upnp.ip();
    // Player-scoped, so it must ride the player's own socket, not a
    // coordinator's - the same rule the per-player volume path follows.
    let speaker = session.player(Some(ip)).await?;
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
    play_stream(
        Reach {
            ip,
            household,
            room,
        },
        url,
        &name,
        no_wait,
        json,
    )
    .await
}

/// Where a command was pointed: `--ip`, `--household` and `--room` as given.
struct Reach<'a> {
    ip: Option<IpAddr>,
    household: Option<&'a str>,
    room: Option<&'a str>,
}

/// Connect, play one stream URL in the room, and report it - the tail `play-url`
/// and `stations --play` share, so the two cannot report a start differently.
async fn play_stream(
    reach: Reach<'_>,
    url: &str,
    name: &str,
    no_wait: bool,
    json: bool,
) -> Result<()> {
    let mut state = State::load()?;
    let session = session::connect(reach.ip, &mut state, reach.household, reach.room).await?;
    let target = session::target(&session.groups, reach.room)?;
    let wait = if no_wait {
        Duration::ZERO
    } else {
        STREAM_START
    };
    let started = stream_url(&session, &target, url, name, None, wait).await?;
    if json {
        return report_started_json(&target.name, name, url, &started);
    }
    report_started(&target.name, name, None, &started)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real header blocks, as the servers sent them on 2026-09-19.
    const DEEZER_FILE: &str = "HTTP/1.1 206 Partial Content\r\n\
         Last-Modified: Mon, 23 Mar 2026 15:18:41 GMT\r\n\
         ETag: \"e21e038d9cef36827bfb335642880041\"\r\n\
         Content-Type: audio/flac\r\n\
         Accept-Ranges: bytes\r\n\
         Content-Range: bytes 0-0/55933354\r\n\
         Content-Length: 1";
    const TIDAL_FILE: &str = "HTTP/1.1 206 Partial Content\r\n\
         Content-Type: audio/flac\r\n\
         ETag: \"3bb27f3e6d8f7fd987bcc0d3cdc7c452\"\r\n\
         Last-Modified: Wed, 08 Feb 2017 14:08:54 GMT\r\n\
         Accept-Ranges: bytes\r\n\
         Content-Range: bytes 0-1048575/52299553";
    /// The one that fooled the first version of this: a *live* stream that
    /// honours a range request, over a 1 GiB sentinel length.
    const ICECAST_LIVE: &str = "HTTP/1.1 206 Partial Content\r\n\
         Server: Icecast\r\n\
         Content-Type: audio/mpeg\r\n\
         Accept-Ranges: bytes\r\n\
         Content-Range: bytes 0-0/1073741823\r\n\
         icy-br: 96\r\n\
         icy-name: NPR 24 Hour Program Stream";

    #[test]
    fn a_stored_file_is_told_from_a_live_stream_by_what_the_server_says() {
        assert!(shaped_like_file(206, DEEZER_FILE));
        assert!(shaped_like_file(206, TIDAL_FILE));

        // Icecast fails on every one of the three tests independently, so no
        // single header carries the verdict on its own.
        assert!(!shaped_like_file(206, ICECAST_LIVE));
        assert!(!shaped_like_file(
            206,
            "HTTP/1.1 206\r\nContent-Range: bytes 0-0/1073741823\r\nETag: \"x\""
        ));
        assert!(!shaped_like_file(
            206,
            "HTTP/1.1 206\r\nContent-Range: bytes 0-0/9999\r\nServer: Icecast\r\nicy-br: 96"
        ));
        assert!(!shaped_like_file(
            206,
            "HTTP/1.1 206\r\nContent-Range: bytes 0-0/9999"
        ));
    }

    #[test]
    fn a_playlist_stays_on_the_stream_path_however_file_shaped_it_looks() {
        // Amazon Music's fallback is a presigned HLS playlist: cacheable,
        // finite, and exactly what a playback session wants. It plays today
        // and must not be moved.
        assert!(!shaped_like_file(
            200,
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/vnd.apple.mpegurl\r\n\
             ETag: \"abc\"\r\n\
             Content-Length: 8213"
        ));
    }

    #[test]
    fn anything_unclear_stays_on_the_stream_path() {
        // A redirect, which is what iHeartRadio answers with, says nothing
        // about what is at the other end.
        assert!(!shaped_like_file(
            302,
            "HTTP/1.0 302 Found\r\nContent-Length: 0"
        ));
        assert!(!shaped_like_file(200, "HTTP/1.1 200 OK\r\nETag: \"x\""));
        assert!(!shaped_like_file(
            200,
            "HTTP/1.1 200 OK\r\nETag: \"x\"\r\nContent-Length: 0"
        ));
        assert!(!shaped_like_file(500, DEEZER_FILE));
    }

    #[test]
    fn a_file_declares_its_audio_type_and_nothing_else() {
        assert_eq!(
            audio_type("HTTP/2 206\r\ncontent-type: audio/mpeg\r\n").as_deref(),
            Some("audio/mpeg")
        );
        assert_eq!(
            audio_type("HTTP/1.1 200 OK\r\nContent-Type: Audio/FLAC; charset=x\r\n").as_deref(),
            Some("audio/flac")
        );
        assert_eq!(
            audio_type("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n"),
            None
        );
        assert_eq!(audio_type("HTTP/1.1 200 OK\r\n"), None);
    }

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
        assert_eq!(hint::of(&err).0, hint::Code::StreamDidNotPlay);
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
        assert_eq!(hint::of(&gone).0, hint::Code::StreamUnverified);
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
        assert_eq!(hint::of(&mixed).0, hint::Code::StreamUnverified);
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
            (Started::Silent, hint::Code::StreamDidNotPlay),
            (
                Started::Unverified {
                    answered: false,
                    why: None,
                },
                hint::Code::StreamUnverified,
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
            "http:///path",
            "http://?query",
        ] {
            let err = stream_display_name(bad, None).unwrap_err();
            assert_eq!(
                hint::of(&err).0,
                hint::Code::BadStreamUrl,
                "{bad} was accepted"
            );
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
        for bad in [
            "file:///tmp/x.mp3",
            "x.mp3",
            "http://",
            "http:///path",
            "http://:8080/clip.mp3",
            "spotify:track:1",
        ] {
            let err = require_http_url(bad).unwrap_err();
            assert_eq!(
                hint::of(&err).0,
                hint::Code::BadStreamUrl,
                "{bad} was accepted"
            );
        }
    }
}
