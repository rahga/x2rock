//! One module per family of commands. `run` in `main.rs` parses, resolves the
//! room and hands off here; nothing in this tree parses arguments and nothing
//! in `main.rs` talks to a speaker. Each file is named for what the person is
//! doing - installing, playing, adjusting a speaker - not for a Sonos API.

pub mod admin;
pub mod content;
pub mod household;
pub mod playback;
pub mod raw;
pub mod services;
pub mod speaker;
pub mod status;
pub mod stream;
pub mod volume;

use std::net::IpAddr;

use anyhow::{Context, Result, bail};

use crate::cli::Command;
use crate::hint;
use crate::session::{self, Session, Target};
use crate::sonos::upnp::{self, Upnp};
use crate::state::State;
use crate::{catalogue, credentials, sonos};
use playback::{apply_crossfade, apply_repeat, apply_shuffle, apply_transport, play_or_resume};
use volume::apply_vol;

/// "a" or "an" for a word about to follow it.
///
/// Only ever used on SMAPI item types - `artist`, `album`, `genre`, `playlist` -
/// which are plain ASCII words where the spelling rule holds. It would be wrong
/// about "an hour" and "a European", and there is no reason for either to reach
/// it.
pub fn article(word: &str) -> &'static str {
    match word.chars().next().map(|c| c.to_ascii_lowercase()) {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    }
}

/// An exact id wins, then a case-insensitive substring of the name; among
/// several of those, a whole-name match settles it, and anything else is
/// ambiguous and says so - naming `hint` as the command that lists them.
pub fn find_named<'a, T>(
    items: &'a [T],
    query: &str,
    id: impl Fn(&T) -> &str,
    name: impl Fn(&T) -> &str,
    what: &str,
    hint: &str,
) -> Result<&'a T> {
    if let Some(exact) = items.iter().find(|i| id(i) == query) {
        return Ok(exact);
    }
    let needle = query.to_lowercase();
    let matches: Vec<_> = items
        .iter()
        .filter(|i| name(i).to_lowercase().contains(&needle))
        .collect();

    match matches.as_slice() {
        [] => bail!("no {what} matches {query:?}. `{hint}` lists them."),
        [only] => Ok(only),
        several => {
            // An exact name wins over the substrings around it - but only when
            // it is unique. Two favorites *named the same* (the household ages
            // into these) cannot be told apart by name, so name the ids rather
            // than silently pick the first.
            let exact: Vec<_> = several
                .iter()
                .filter(|i| name(i).to_lowercase() == needle)
                .collect();
            match exact.as_slice() {
                [whole] => return Ok(whole),
                [_, ..] => {
                    let shown: Vec<_> = exact
                        .iter()
                        .map(|i| format!("{} (id {})", name(i), id(i)))
                        .collect();
                    bail!(
                        "{} {what}s are named {query:?}: {}. Give an id to pick one.",
                        exact.len(),
                        shown.join(", ")
                    );
                }
                [] => {}
            }
            let shown: Vec<_> = several.iter().take(8).map(|i| name(i)).collect();
            bail!(
                "{} {what}s match {query:?}: {}{}",
                several.len(),
                shown.join(", "),
                if several.len() > shown.len() {
                    ", ..."
                } else {
                    ""
                }
            )
        }
    }
}

/// "old → " when a command changed something, nothing when it only reported.
pub fn transition(before: &str, after: &str) -> String {
    if before == after {
        String::new()
    } else {
        format!("{before} → ")
    }
}

pub fn mmss(duration: Option<std::time::Duration>) -> String {
    match duration {
        Some(d) => format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60),
        None => String::new(),
    }
}

/// Whether an error is the *player* declining, as opposed to not being reached.
///
/// The distinction the `Fault` type exists to draw, asked in three places: the
/// two enqueue fallbacks below and `raw upnp`. A refusal means "this is not
/// queue material", which is a reason to try the stream session instead; a
/// timeout or a dead socket means nothing of the kind, and falling back on one
/// spends a second round trip to fail the same way while printing a sentence
/// that blames the content.
///
/// A UPnP fault is one way the player says it; `not_queue_material` is the
/// other, raised by `enqueue_and_play` when the row was accepted and then would
/// not play. Both mean the same thing to a caller holding a stream fallback.
pub fn is_refusal(e: &anyhow::Error) -> bool {
    upnp::Fault::of(e).is_some() || hint::of(e).0 == "not_queue_material"
}

/// `on`/`off`, for every flag and argument that takes those two words.
///
/// Hoisted out of `apply_eq`'s closure once `remote`, `led`, `shuffle` and
/// `crossfade` all wanted the same three lines and the same message.
pub fn on_off(what: &str, text: Option<&str>) -> Result<Option<bool>> {
    match text {
        None => Ok(None),
        Some(word @ ("on" | "off")) => Ok(Some(word == "on")),
        Some(_) => bail!("{what} takes on or off"),
    }
}

/// The word for a boolean, for the read-back lines.
pub fn on_word(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The `n`th of `items`, counted from 1 the way every listing here numbers
/// its rows - `search --play 3` plays the row printed as `3.` - so `0` is
/// nobody rather than the first.
pub fn nth<T>(items: &[T], n: usize) -> Option<&T> {
    items.get(n.checked_sub(1)?)
}

/// Where a group's UPnP calls go: its coordinator, which owns the queue and
/// the transport, or `fallback` - the connection already open - when the
/// topology gave no address for it. One rule, because a call that went to a
/// member instead would edit the wrong queue while looking like it worked.
pub fn upnp_ip(target: &Target, fallback: IpAddr) -> IpAddr {
    target.coordinator_ip.unwrap_or(fallback)
}

/// The service catalogue, brought up to date against the player the session
/// reached. Every by-id lookup wants this before it trusts an id: a cleared or
/// schema-bumped cache would otherwise make a bookmark or a remembered stream
/// look unknown until some `search` happened to rebuild it. Not saved here -
/// the callers that care whether anything changed refresh for themselves.
pub async fn refreshed_catalogue(session: &Session) -> Result<catalogue::Catalogue> {
    let mut catalogue = catalogue::Catalogue::load();
    catalogue
        .refresh(&Upnp::new(session.connection.ip()), false)
        .await?;
    Ok(catalogue)
}

/// The opening `play-item` and `queue-item` share: connect, refresh the
/// catalogue, and resolve `service` among what this machine can use, with the
/// token held for it. The two commands take the same arguments and differ
/// only in what they do with the item once it is named.
pub async fn connect_for_service(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    service: &str,
) -> Result<(Session, sonos::smapi::Service, Option<sonos::smapi::Token>)> {
    let mut state = State::load()?;
    let session = session::connect(ip, &mut state, household, room).await?;
    let catalogue = refreshed_catalogue(&session).await?;
    let linked = credentials::Credentials::load()?;
    let usable = catalogue.usable(&linked);
    let chosen = catalogue::Catalogue::find(&usable, service)?.clone();
    let token = linked.token_for(&chosen.id);
    Ok((session, chosen, token))
}

/// Fan a per-room command across several `--room`, topology resolved once. Only
/// the per-room-state commands accept it; anything else is refused with a clear
/// message rather than silently acting on the first room. A failure on one room
/// stops the run - a half-applied "set them all to 10" is worse than a clear
/// stop naming the room that failed.
pub async fn fan_out(
    session: &session::Session,
    rooms: &[String],
    command: &Command,
) -> Result<()> {
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
pub fn too_many_rooms() -> anyhow::Error {
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
pub fn fans_out(command: &Command) -> bool {
    per_room(command).is_some()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_article_matches_the_word_after_it() {
        // "is a artist" is what prompted this.
        assert_eq!(super::article("artist"), "an");
        assert_eq!(super::article("albumList"), "an");
        assert_eq!(super::article("Artist"), "an", "case is not the question");
        assert_eq!(super::article("genre"), "a");
        assert_eq!(super::article("collection"), "a");
        assert_eq!(super::article("playlist"), "a");
        assert_eq!(super::article(""), "a", "nothing to look at");
    }

    use super::*;
    use crate::cli::Cli;
    use anyhow::anyhow;
    use clap::Parser;

    #[test]
    fn find_named_disambiguates_two_of_the_same_name() {
        fn id(i: &(String, String)) -> &str {
            i.0.as_str()
        }
        fn name(i: &(String, String)) -> &str {
            i.1.as_str()
        }
        let items = [
            ("fv1".to_string(), "That Christmas Channel".to_string()),
            ("fv2".to_string(), "That Christmas Channel".to_string()),
            ("fv7".to_string(), "Jazz24".to_string()),
        ];
        // A unique name resolves; an exact id always resolves.
        assert_eq!(
            find_named(&items, "jazz24", id, name, "f", "h").unwrap().0,
            "fv7"
        );
        assert_eq!(
            find_named(&items, "fv2", id, name, "f", "h").unwrap().0,
            "fv2"
        );
        // Two favorites sharing a name are not silently reduced to the first -
        // the error names both ids so a caller can pick one.
        let err = find_named(&items, "That Christmas Channel", id, name, "favorite", "h")
            .unwrap_err()
            .to_string();
        assert!(err.contains("fv1") && err.contains("fv2"), "{err}");
        assert!(err.contains("Give an id"), "{err}");
    }

    /// The enqueue fallbacks turn on this one question, so it has to survive a
    /// `.context()` layer: a refusal wrapped in explanation is still a refusal,
    /// and reading it as a transport failure would silently retire the stream
    /// fallback that `play_item` and `bookmark` depend on.
    #[test]
    fn only_a_player_refusal_counts_as_a_refusal() {
        let fault = anyhow!(upnp::Fault {
            action: "AddURIToQueue".into(),
            kind: upnp::FaultKind::Action("800".into()),
            detail: String::new(),
        });
        assert!(is_refusal(&fault));
        // A per-action refusal is not the transport being off - checked before
        // `.context()` below consumes `fault`.
        assert!(upnp::Fault::of(&fault).is_some_and(upnp::Fault::is_per_action));
        assert!(
            is_refusal(&fault.context("enqueuing the track")),
            "a refusal must stay recognisable under added context"
        );

        // The cases that must NOT take the fallback: the speaker was never
        // reached, so the stream session cannot help and would fail the same
        // way a round trip later.
        // HTTP 403 - UPnP switched off in the Sonos app - is the player
        // declining too, and the one case where the fallback matters most:
        // `stream_item` is pure Control API, so it still works on a household
        // where every UPnP call is refused.
        let forbidden = anyhow!(upnp::Fault {
            action: "AddURIToQueue".into(),
            kind: upnp::FaultKind::UpnpDisabled,
            detail: "UPnP is turned off".into(),
        });
        assert!(is_refusal(&forbidden));
        // ...but it is the transport being off, not one action refused, which
        // is the distinction `raw upnp` draws and the fallbacks do not.
        assert!(upnp::Fault::of(&forbidden).is_some_and(|f| !f.is_per_action()));

        assert!(!is_refusal(&anyhow!("connection refused")));
        assert!(!is_refusal(
            &anyhow!("timed out after 8s").context("reaching Kitchen")
        ));
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
}
