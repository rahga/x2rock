//! Playing a service's item in a room, by whichever route takes it: the queue,
//! the room's own source for a radio program, a stream session, `loadContent`.
//! Moved out of the CLI's `content`, `stream`, `playback` and `services`
//! commands so a front end other than the CLI plays things exactly as it does;
//! the routes, and the measurements behind each, are in docs/architecture.md
//! under "Playback routes".

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::json;

use crate::session::{self, Session, Target};
use crate::sonos::local::{ApiError, Connection, Unreachable};
use crate::sonos::proto::ContentId;
use crate::sonos::upnp::{self, Upnp};
use crate::{bookmarks, catalogue, credentials, hint, sonos, streams};


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
    upnp::Fault::of(e).is_some() || hint::of(e).0 == hint::Code::NotQueueMaterial
}

/// Where a group's UPnP calls go: its coordinator, which owns the queue and
/// the transport, or `fallback` - the connection already open - when the
/// topology gave no address for it. One rule, because a call that went to a
/// member instead would edit the wrong queue while looking like it worked.
pub fn upnp_ip(target: &Target, fallback: IpAddr) -> IpAddr {
    target.coordinator_ip.unwrap_or(fallback)
}


/// Play one item from a service in a room, by the id a search or browse returned.
///
/// **Two mechanisms, and which one is right depends on the item.** Both are
/// needed; neither covers the other:
///
/// - **Enqueue with a cdudn**, as `bookmark` does. The player resolves the media
///   itself against the credential it holds, which is the only thing that works
///   for on-demand content whose stream x2rock cannot resolve - a Mixcloud show
///   hands back an HLS playlist whose AES-128 key URI carries a 63-byte path
///   where 16 bytes of key belong, so no compliant client can play it and the
///   room stalls at `IDLE`. The player can.
/// - **`loadStreamUrl` in a session**, which plays alongside the queue and
///   leaves it untouched. The only thing that works for a *live stream*:
///   `AddURIToQueue` refuses an iHeartRadio `live_stations.` id outright with
///   UPnP 800, and Sonos does not intend stations to sit in a queue.
///
/// So: enqueue when there is a cdudn to name an account with and the item is not
/// a stream, and **fall back to the session on any refusal**, because a refusal
/// is the player saying this is not queue material. The reverse fallback is not
/// possible - `loadStreamUrl` fails *silently*, minutes later, at `IDLE`.
#[allow(clippy::too_many_arguments)]
pub async fn play_item(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    token: Option<&sonos::smapi::Token>,
    kind: Option<&str>,
    id: &str,
    title: &str,
    from_start: bool,
) -> Result<()> {
    // A container of containers is not queue material and is not a stream
    // either, so neither path fits: falling through would try to stream an
    // artist, which fails silently minutes later at IDLE.
    if kind.is_some_and(bookmarks::container_of_containers) {
        bail!(
            "{title:?} is {} {}, which holds albums and playlists rather than \
             tracks. Open it with `x2rock browse` and play what is inside.",
            article(kind.unwrap_or_default()),
            kind.unwrap_or_default()
        );
    }
    if !from_start && kind.is_some_and(is_resumable) {
        return play_resumed(session, room, service, token, id, title).await;
    }
    play_one(session, room, service, token, kind, id, title).await
}

/// Whether a kind is something the service keeps the listener's place in, so
/// playing it means picking up there. An audiobook, so far - Audible's.
pub fn is_resumable(kind: &str) -> bool {
    kind.eq_ignore_ascii_case("audiobook")
}

/// Play a book from where its listener left off: the chapter the service names,
/// then a seek to the offset into it. Without this a book was an album to the
/// queue - every chapter, from the first - and the place was lost without a
/// word, and then overwritten, since the speaker reports position back to the
/// service as it plays. See "Playback routes" in docs/architecture.md.
///
/// With no place kept (a book never started), the first chapter from its start.
pub async fn play_resumed(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    token: Option<&sonos::smapi::Token>,
    id: &str,
    title: &str,
) -> Result<()> {
    let mut refreshed = None;
    let (resume, first) = sonos::smapi::resume_point(service, token, id, &mut refreshed)
        .await
        .with_context(|| format!("reading where {title:?} was left off"))?;
    let token = after_refresh(&service.id, token, refreshed);
    let (chapter, offset) = match resume {
        Some(at) => (at.id, at.offset_millis),
        None => (
            first
                .ok_or_else(|| anyhow!("{title:?} has no chapters to play"))?
                .id,
            0,
        ),
    };
    // A chapter is an ordinary track: the queue takes it, as it takes any.
    play_one(
        session,
        room,
        service,
        token.as_ref(),
        Some("track"),
        &chapter,
        title,
    )
    .await?;
    if offset == 0 {
        return Ok(());
    }
    let target = session::target(&session.groups, room)?;
    let player = session::coordinator(session, &target).await?;
    // Sought once it is playing, which is when the measured seek landed; one
    // sent while the chapter is still buffering has nothing to move yet.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    while player.playback_status(&target.group_id).await?.state() != Some("PLAYING")
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    player
        .seek_to(&target.group_id, offset)
        .await
        .with_context(|| {
            format!(
                "{title:?} is playing, from the start of the chapter it was left in, but \
                 could not be moved to where it was left off"
            )
        })?;
    println!(
        "{} — resumed {} into the chapter",
        target.name,
        mmss(Some(std::time::Duration::from_millis(offset)))
    );
    Ok(())
}

/// One playable leaf - a track, a stream, a radio program - by whichever route
/// takes it: the queue, the room's own source, a stream, `loadContent`.
pub async fn play_one(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    token: Option<&sonos::smapi::Token>,
    kind: Option<&str>,
    id: &str,
    title: &str,
) -> Result<()> {
    // A stream is never queue material, and a service with no type in the
    // player's list has no cdudn to build - `SA_RINCONNone` is not an account.
    let streamish = kind.is_some_and(|k| k.eq_ignore_ascii_case("stream"));
    // Whether the queue was refused because the household has UPnP off, rather
    // than because of the item: if nothing below plays it either, that is the
    // reason worth ending on, and the one the person can do something about.
    // A `program` is a radio show or channel - an iHeartRadio artist station,
    // a Radio Paradise channel - and never queue material: `AddURIToQueue`
    // refuses every one with 800. The Sonos app plays one by making it the
    // room's source, so that is its first route, not a fallback reached after
    // a refusal that was certain (and printed as if something had gone wrong).
    // Measured on iHeart's `artist_radio.1648`, 2026-10-07.
    let program = kind.is_some_and(sonos::smapi::is_program);
    let mut upnp_off = false;
    if !streamish && let Some(naming) = Naming::of(service, token) {
        let first = match program {
            true => play_radio(session, room, service, &naming, id, title).await,
            false => {
                enqueue_item(
                    session,
                    room,
                    service,
                    &naming,
                    id,
                    title,
                    kind,
                    Enqueue::Play,
                )
                .await
            }
        };
        match first {
            Ok(()) => return Ok(()),
            // Only a refusal earns the fallback. An unreachable coordinator is
            // not the item's fault and the stream session cannot fix it.
            Err(e) if is_refusal(&e) => {
                upnp_off = upnp::Fault::of(&e).is_some_and(|f| !f.is_per_action());
                let what = match program {
                    true => "would not play as radio",
                    false => refusal_was(&e),
                };
                eprintln!("x2rock: {title:?} {what} ({e:#}); streaming it");
            }
            Err(e) => return Err(e),
        }
    }
    let played = play_unqueued(session, room, service, token, kind, id, title).await;
    match played {
        // A hint, not a plain context, so the code an agent branches on is
        // this one rather than whichever fallback failed last.
        Err(e) if upnp_off => Err(e.context(hint::Hint::new(
            "UPnP is off for this household, so it could not go in the queue - turn it \
             back on in the Sonos app under Account > Privacy and Security > Connection \
             Security > UPnP",
            hint::Code::UpnpDisabled,
            None,
        ))),
        other => other,
    }
}

/// A service's radio program as the room's own source - see
/// `bookmarks::radio_uri` - and played, confirmed the way a resume is.
pub async fn play_radio(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    naming: &Naming,
    id: &str,
    title: &str,
) -> Result<()> {
    let target = session::target(&session.groups, room)?;
    let player = session::coordinator(session, &target).await?;
    let upnp = Upnp::new(upnp_ip(&target, player.ip()));
    upnp.set_radio(
        &bookmarks::radio_uri(id, &service.id, naming.serial.as_deref()),
        &bookmarks::radio_didl(id, title, &naming.cdudn),
    )
    .await?;
    play_confirmed(&player, &upnp, &target.group_id, &target.name).await?;
    println!("{} — {title} on {}", target.name, service.name);
    Ok(())
}

/// `play_item`'s routes that need no queue: a direct stream, then `loadContent`.
pub async fn play_unqueued(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    token: Option<&sonos::smapi::Token>,
    kind: Option<&str>,
    id: &str,
    title: &str,
) -> Result<()> {
    let target = session::target(&session.groups, room)?;
    let streamed = stream_item(
        session,
        &target,
        service,
        token,
        id,
        title,
        StreamStart::Fresh,
    )
    .await;
    // **The last resort, after both of the above said no.** `loadContent` hands
    // the player the service, account and id and lets it resolve the rest, and
    // it plays what neither path can - an iHeartRadio `artist_radio` loaded
    // here before the radio route existed (verified 2026-09-26), refused by
    // `AddURIToQueue` (800) and by the service's own `getMediaURI`; it now plays
    // as the room's source first, so it reaches this only if that fails too.
    // Not the first choice: it replaces the queue
    // rather than adding to it, refuses every service the household holds no
    // account for, and takes a pause and a wait to start - see `replay`.
    match streamed {
        // Never for Mixcloud: `loadContent` on one of its items hangs the
        // coordinator until it restarts (x2rocktv P-21, 2026-10-11).
        Err(e) if service.name.eq_ignore_ascii_case("Mixcloud") => Err(e),
        Err(e) if sonos::smapi::Refused::of(&e).is_some() => {
            let (Some(kind), Some(serial)) = (kind, token.and_then(|t| t.serial)) else {
                return Err(e);
            };
            eprintln!("x2rock: {title:?} would not stream either ({e:#}); loading it");
            let player = session::coordinator(session, &target).await?;
            let id = ContentId {
                service_id: Some(service.id.clone()),
                account_id: Some(format!("sn_{serial}")),
                object_id: id.to_string(),
            };
            load_and_start(&player, &target, &id, kind, title).await?;
            println!("{} — {title} on {}", target.name, service.name);
            Ok(())
        }
        other => other,
    }
}

/// Put a service item in the room's queue, and optionally jump to it.
///
/// Playing is deliberately the same sequence `bookmark` uses, down to making the
/// queue the current source first: after a station it is not, and `Seek` fails
/// with 701. With `play` false none of that happens - the track is added to the
/// end and whatever is playing keeps playing, which is the whole point of the
/// distinction.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_item(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    naming: &Naming,
    id: &str,
    title: &str,
    kind: Option<&str>,
    how: Enqueue,
) -> Result<()> {
    let target = session::target(&session.groups, room)?;
    let (cdudn, sn) = (naming.cdudn.as_str(), naming.serial.as_deref());
    let upnp = Upnp::new(upnp_ip(&target, session.connection.ip()));
    // **A container is enqueued by a different scheme from a track.** A track is
    // fetched; a container is expanded by the player, which walks it and adds
    // each track it holds. Handing a container a track's URI is what got UPnP
    // error 804 - the player calling the URI malformed, rather than the 800 it
    // gives for something it cannot play.
    //
    // The `sn=` and the cdudn name the same account either way - see `Naming`.
    let container = kind.is_some_and(bookmarks::container_holds_tracks);
    let (uri, didl) = match container {
        true => (
            bookmarks::container_uri(id, &service.id, sn),
            bookmarks::container_didl(id, title, kind.unwrap_or_default(), cdudn),
        ),
        false => (
            bookmarks::service_uri(id, &service.id, sn),
            bookmarks::service_didl(id, title, cdudn),
        ),
    };
    if how != Enqueue::Play {
        let next = how == Enqueue::Next;
        let queued = upnp.add_to_queue(&uri, &didl, next).await?;
        // Said only where it went in ahead of something: with the queue not the
        // room's source, "next" appends (see `add_to_queue`), and calling an
        // append "next" would promise an order nothing is playing.
        let ahead = queued.first + queued.added <= queued.length;
        let place = if next && ahead { "next, " } else { "" };
        match queued.added {
            1 => println!(
                "{} — queued {title} {place}at {}",
                target.name, queued.first
            ),
            n => println!(
                "{} — queued {title} {place}at {}, {n} tracks",
                target.name, queued.first
            ),
        }
        return Ok(());
    }
    enqueue_and_play(session, &target, &upnp, &uri, &didl).await?;
    println!("{} — {title} on {}", target.name, service.name);
    Ok(())
}

/// What [`enqueue_item`] does with what it adds: the Sonos app's Play Now, Add
/// to End of Queue and Play Next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueue {
    Play,
    End,
    Next,
}

/// One row of a `search` or `browse` listing, 1-based, and what to do with it.
#[derive(Debug, Clone, Copy)]
pub struct Pick {
    pub row: usize,
    pub how: Enqueue,
}

/// Add one item to the group's queue and play it from there: the shared body
/// of [`enqueue_item`] and [`play_bookmark`], which differ only in where the
/// URI and DIDL come from. The queue is made the current source first - after
/// a station or line-in it is not, and `Seek` would fail with 701 - and the
/// play goes to the coordinator, the one player that answers for the group.
pub async fn enqueue_and_play(
    session: &session::Session,
    target: &Target,
    upnp: &Upnp,
    uri: &str,
    didl: &str,
) -> Result<()> {
    let queued = upnp.add_to_queue(uri, didl, false).await?;
    match start_queued(session, target, upnp, queued.first).await {
        Ok(()) => Ok(()),
        // **The queue took it and then would not play it.** `AddURIToQueue` is
        // not the only way the player says "this is not queue material" - a
        // live radio station is *accepted* by it (iHeartRadio's
        // `live_stations.*`, verified 2026-09-19: three of them sat in a real
        // queue, each showing the station's name) and only refused later, at
        // the play. The refusal that earns a stream fallback is therefore the
        // whole sequence's, not `AddURIToQueue`'s alone.
        //
        // The row is taken back out before returning. Leaving it would be the
        // failure mode already on record from the TIDAL account removal - dead
        // rows accumulating in a room's queue for a person to clear by hand -
        // and here they would accumulate one per attempt.
        // **Only the player answering "no" counts.** `start_queued` makes four
        // network calls, and a timeout or a dropped socket in any of them is
        // not the content's fault - the distinction `is_refusal` exists to
        // draw. Treating every failure here as a refusal meant a blip deleted
        // the row that had just been added successfully and then blamed the
        // item for it, which is exactly the "spends a second round trip to
        // fail the same way while printing a sentence that blames the content"
        // that function warns about. A UPnP fault or an `ApiError` means the
        // player replied; anything else means it did not.
        Err(e) if upnp::Fault::of(&e).is_some() || ApiError::of(&e).is_some() => {
            // Everything that went in comes back out: a container expands to
            // many rows, and taking only the last one left the rest behind.
            if let Err(cleanup) = upnp.remove_range(queued.first, queued.added, None).await {
                eprintln!(
                    "x2rock: could not take the unplayable rows back out of {}'s queue \
                     ({cleanup:#}); they start at position {}",
                    target.name, queued.first
                );
            }
            // An *internal* code, and deliberately absent from the skill's error
            // table. Every caller of this function reaches it through a match
            // arm on `is_refusal`, which names this code and turns it into the
            // stream fallback with the wording `refusal_was` picks - so it never
            // reaches `main` and an agent can never observe it. It exists to
            // tell "went in the queue and then would not play" from "would not
            // go in the queue" on stderr. If a caller is ever added that
            // propagates this with `?`, it becomes observable and belongs in
            // that table.
            Err(hint::Hint::new(format!("{e:#}"), hint::Code::NotQueueMaterial, None).into())
        }
        // Unreachable mid-sequence. The row is left alone: it may well be
        // fine, and a player that cannot be asked to play cannot be asked to
        // edit its queue either. The error travels as itself, so `is_refusal`
        // says no and no stream fallback is attempted.
        Err(e) => Err(e.context(format!(
            "{} was queued at {} but the player could not be told to play it",
            target.name, queued.first
        ))),
    }
}

/// How to word a refusal, which is two different events wearing one word. The
/// distinction matters to whoever reads the line: "would not go in the queue"
/// describes a row that was never created, and printing it for a station the
/// queue happily accepted and then choked on would send a reader looking for a
/// queue bug that is not there.
pub fn refusal_was(e: &anyhow::Error) -> &'static str {
    match hint::of(e).0 {
        hint::Code::NotQueueMaterial => "went in the queue and then would not play",
        _ => "would not go in the queue",
    }
}

/// Point the group at the queue and start the row that was just added. Split
/// out of [`enqueue_and_play`] so the failure of *any* of its three steps is
/// one thing the caller can act on.
pub async fn start_queued(
    session: &session::Session,
    target: &Target,
    upnp: &Upnp,
    length: u32,
) -> Result<()> {
    if !upnp.playing_from_queue().await? {
        upnp.use_queue(&target.coordinator_id).await?;
    }
    upnp.seek_track(length).await?;
    let coordinator = session::coordinator(session, target).await?;
    coordinator.playback(&target.group_id, "play").await
}

/// How an enqueue names the account that owns the item: the cdudn in its DIDL
/// and the `sn=` in its URI, both taken from one stored account.
///
/// **The two have to agree, and neither alone picks the account.** Measured
/// 2026-09-25 against a household holding two iHeartRadio accounts (`sn_24`,
/// `sn_25`): the account the player reports playing from follows `sn=`, while
/// the queued row's own `sn` is rewritten from the cdudn's selector, so a
/// mismatched pair leaves the two saying different things. A `-0-` cdudn, a
/// made-up selector, no `sn=` and one the household does not hold all land on
/// the household's default account. A matched pair is what the Sonos app sends,
/// and it came back consistent everywhere. See docs/architecture.md, "Which
/// account an enqueue plays from".
#[derive(Debug, PartialEq)]
pub struct Naming {
    pub cdudn: String,
    pub serial: Option<String>,
}

impl Naming {
    /// Whatever the token's account can say for itself - the token being the
    /// one resolution of "which account" the caller already made, so the
    /// service is asked and the player is told about the same one. With no
    /// selector (a primary, whose own is `-0-`, or a browser link) or no serial
    /// (a link `match` never answered) it sends what it has; no token at all is
    /// the `-0-`-and-no-`sn=` form, which the player resolves to its default
    /// account - the only one, for a service with one. A serial with no
    /// selector is still sent: `sn=` is what picks the account, and only the
    /// queued row is left disagreeing, until a `link --from-household` fills
    /// the selector in. `None` when the service has no type in the player's
    /// list, so no cdudn can be built at all.
    pub fn of(service: &sonos::smapi::Service, token: Option<&sonos::smapi::Token>) -> Option<Self> {
        Some(Self {
            cdudn: service.cdudn_for(token.and_then(|t| t.selector.as_deref()))?,
            serial: token.and_then(|t| t.serial).map(|n| n.to_string()),
        })
    }
}

/// Whether a row can be put in a queue, which is not the same as playable.
///
/// Two ways it cannot. A **live stream** has no queue form at all - `play-item`
/// answers one by streaming it alongside the queue. And a service missing from
/// the player's `AvailableServiceTypeList` has no `cdudn` to build, so nothing
/// can name the account that owns the item; on this household that is exactly
/// one service of 108, the anonymous TuneIn, which is also the widget's default.
///
/// A container is excluded too. A service may mark one playable and still refuse
/// its id with a grammar error - see `browse` - so it is somewhere to go rather
/// than something to add.
pub fn queueable(item: &sonos::smapi::Item, service: &sonos::smapi::Service) -> bool {
    if item.item_type.eq_ignore_ascii_case("stream")
        || sonos::smapi::is_program(&item.item_type)
        || service.cdudn().is_none()
    {
        return false;
    }
    // A container qualifies when what it holds is tracks - an album or a
    // playlist - because the player will expand it into the queue. One holding
    // other containers, an artist say, has nothing to enqueue and is refused.
    match item.container {
        true => bookmarks::container_holds_tracks(&item.item_type),
        false => true,
    }
}

/// Replace what `target` plays with one piece of content and see it start: the
/// shared tail of `replay` and of `play_item`'s last resort.
pub async fn load_and_start(
    player: &Connection,
    target: &Target,
    id: &ContentId,
    kind: &str,
    name: &str,
) -> Result<()> {
    // **Stopped first, so PLAYING can only mean the new content.** Loaded over a
    // playing room, the old item goes on reading PLAYING for a moment, and a
    // check that saw that declared success while the new load then settled at
    // IDLE, its play lost (2026-09-26). Paused, nothing reads PLAYING until the
    // load has been started. Best effort: a room already stopped, or stuck
    // mid-load from an earlier one, refuses the pause, and that is fine.
    let _ = player.playback(&target.group_id, "pause").await;
    // **PLAYING counts only on what was asked for.** Pausing first was not
    // enough: the press below can land before the load takes over and resume
    // what was paused, which reads PLAYING. Measured on Media Room
    // (2026-10-08): `replay "Happy Radio"` over Sonos Radio's Hit List saw
    // Hit List play again, reported success, and the Saavn load then settled
    // at IDLE. So the room must be playing something other than it was - or,
    // replaying what is already on, the thing named. And a load the player
    // never answered is not refused but judged the same way: a busy
    // coordinator can answer late or not at all and still do it (x2rocktv,
    // e4b2848). A refusal still ends it here.
    let before = player
        .metadata(&target.group_id)
        .await
        .ok()
        .map(|m| playing_identity(&m));
    let lost = match player.load_content(&target.group_id, id, kind).await {
        Ok(()) => false,
        Err(e) if Unreachable::of(&e).is_some() => true,
        Err(e) => return Err(e),
    };
    // Loaded, not started - and a room that was playing sits at BUFFERING
    // until told to play, so a play is what finishes the job either way. It is
    // pressed until the room is seen playing: once is not enough, because one
    // that lands mid-load can be accepted and still come to nothing.
    let deadline = tokio::time::Instant::now() + LOAD_SETTLE;
    loop {
        match player.playback(&target.group_id, "play").await {
            Ok(()) => {}
            Err(e)
                if ApiError::of(&e).and_then(|a| a.code.as_deref())
                    == Some("ERROR_PLAYBACK_NO_CONTENT") => {}
            Err(e) => return Err(e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let status = player.playback_status(&target.group_id).await?;
        let playing = match status.state() {
            Some("PLAYING") => true,
            // Already asked to play and on its way; give it the time.
            Some("BUFFERING") => {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let again = player.playback_status(&target.group_id).await?;
                again.state() == Some("PLAYING")
            }
            _ => false,
        };
        if playing && arrived(player, target, before.as_ref(), name).await? {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "{}",
            match lost {
                true => format!(
                    "{name:?} was sent to {} with no answer, and the room did not move on \
                     to it. Try again.",
                    target.name
                ),
                false => format!(
                    "{name:?} loaded in {} but did not start playing. The service may \
                     have withdrawn it.",
                    target.name
                ),
            }
        );
    }
    Ok(())
}

/// What a room is playing, as far as telling one thing from another goes: the
/// track's id and the container's name, the pair x2rocktv compares.
pub fn playing_identity(meta: &sonos::proto::MetadataStatus) -> (Option<String>, Option<String>) {
    (
        meta.track()
            .and_then(|t| t.id.as_ref())
            .map(|id| id.object_id.clone()),
        meta.container.as_ref().and_then(|c| c.name.clone()),
    )
}

/// Whether what the room plays now is what `load_and_start` asked for:
/// something other than `before`, or a container named `name` - the history
/// item's own name, which is how a replay of what was already on is told from
/// the paused content coming back. (The ids do not line up to compare: history
/// says `sonos:2997` where the container says `2997`.) With nothing known
/// before, anything counts.
pub async fn arrived(
    player: &Connection,
    target: &Target,
    before: Option<&(Option<String>, Option<String>)>,
    name: &str,
) -> Result<bool> {
    let Some(before) = before else {
        return Ok(true);
    };
    let now = playing_identity(&player.metadata(&target.group_id).await?);
    Ok(&now != before || now.1.as_deref() == Some(name))
}


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
    target: &session::Target,
    service: &sonos::smapi::Service,
    token: Option<&sonos::smapi::Token>,
    id: &str,
    title: &str,
    how: StreamStart,
) -> Result<()> {
    let mut refreshed = None;
    let uri = sonos::smapi::media_uri(service, token, id, &mut refreshed).await?;
    // Persisted for next time; this call has already spent the refresh.
    after_refresh(&service.id, token, refreshed);
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
    let started = stream_url(session, target, &uri, title, Some(service), wait).await?;
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

/// What became of a stream after the player accepted it.
///
/// **Three outcomes, because there are three.** `loadStreamUrl` returning
/// success means only that the URL was taken, so collapsing this to
/// worked/failed would have to guess which of the other two a buffering room
/// is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
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

/// Whether a URL is a finite file rather than a broadcast, asked of the server
/// rather than guessed from the URL.
///
/// A signed CDN URL says nothing useful in its path - Deezer's ends `.flac` and
/// TIDAL's ends `/0.flac?token=…`, but neither shape is promised by anything -
/// so the question goes to the server as a one-byte range request.
///
/// **"It answered 206" is not the test**, which is what the first version of
/// this got wrong and hardware caught within the minute: Icecast honours a
/// range request on a *live* stream, answering `206` with `Content-Range: bytes
/// 0-0/1073741823` - a 1 GiB sentinel standing in for "endless". NPR's stream
/// took the file path on that basis and played, but as a transport swap that
/// wiped the room's queue rather than the session it should have been.
///
/// What actually separates them is whether the thing behaves like a stored
/// object. Deezer and TIDAL both answer with an `ETag` *and* a `Last-Modified`
/// (TIDAL's reads 2017) over a real total; Icecast sends neither, and labels
/// itself besides with `icy-` headers. So all three must agree - no `icy-`, a
/// validator, and a length that is not the sentinel - before a URL is treated
/// as a file.
///
/// **False on anything unclear, including every error.** The stream path is
/// what has always run here and what every internet radio station needs, so a
/// probe that cannot reach the URL, times out, or comes back ambiguous leaves
/// the behaviour exactly as it was. Guessing "file" wrongly replaces a room's
/// queue for nothing; guessing "stream" wrongly costs the silent stall that is
/// already documented and already the status quo.
///
/// A file comes back with its `audio/*` `Content-Type`, where the server named
/// one, for [`Upnp::play_url_as_track`] to declare.
pub async fn finite_file(url: &str) -> Option<File> {
    match sonos::http::probe(url, PROBE).await {
        Ok((status, head)) if shaped_like_file(status, &head) => Some(File {
            mime: audio_type(&head),
        }),
        _ => None,
    }
}

/// What [`finite_file`] learned about a URL it judged to be a file.
pub struct File {
    mime: Option<String>,
}

/// The verdict itself, given what the server said. Pure, because the rule is
/// the part that was wrong the first time and the part worth testing: the
/// header blocks in its tests are real ones, copied from Deezer, TIDAL, an
/// Icecast relay and iHeartRadio.
pub fn shaped_like_file(status: u16, head: &str) -> bool {
    let head = head.to_ascii_lowercase();
    let header = |name: &str| {
        head.lines().find_map(|l| {
            let value = l.strip_prefix(name)?.strip_prefix(':')?.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
    };
    // A server that announces itself as a broadcast is believed on the spot.
    if head.lines().any(|l| l.starts_with("icy-")) {
        return false;
    }
    // An HLS playlist is finite by every other measure - it has a length, a
    // validator and a type - and is exactly the continuous thing the stream
    // path wants. Amazon Music's fallback is one of these and works today; it
    // must keep working.
    if let Some(kind) = header("content-type")
        && (kind.contains("mpegurl") || kind.contains("dash+xml") || kind.contains("x-scpls"))
    {
        return false;
    }
    // A stored object is cacheable, so it carries something to revalidate
    // against. A broadcast has nothing to validate and sends neither.
    if header("etag").is_none() && header("last-modified").is_none() {
        return false;
    }
    let total = match status {
        // `bytes 0-0/52299553`, or `/*` where the server will not say.
        206 => header("content-range").and_then(|v| v.rsplit('/').next()?.parse::<u64>().ok()),
        // A server that ignored the range answers the whole thing instead - and
        // says how big it is in the head, which is all `probe` reads. The body
        // it intended to send is never collected.
        200 => header("content-length").and_then(|v| v.parse::<u64>().ok()),
        _ => None,
    };
    // 2^30 - 1 exactly: Icecast's stand-in for a length it does not have. No
    // real track is that large, and treating the sentinel as a size is what
    // sent a live stream down the wrong path.
    total.is_some_and(|n| n > 0 && n != 1_073_741_823)
}

/// How long a URL gets to say what it is before the stream path is used anyway.
/// Short on purpose: this runs before every fallback play, and the answer is
/// only ever an optimisation over failing.
pub const PROBE: Duration = Duration::from_secs(4);

/// Open a playback session in the room and load one stream URL into it.
///
/// The half of [`stream_item`] that has nothing to do with services, shared so
/// that `play-url` and a service's live stream cannot drift apart: they are the
/// same two calls to the same namespace, and the only difference is whether a
/// service gets named in the metadata. The caller resolves the target, since it
/// needs the room's name to word its own confirmation anyway.
///
/// **A session rather than the transport, on purpose.** `SetAVTransportURI`
/// with `x-rincon-mp3radio://<url>` also plays an arbitrary stream (verified
/// 2026-09-04, and see "A stream URL needs no service" in
/// docs/architecture.md), but it *replaces* what the room was doing and loses
/// the queue's position. A session plays alongside the queue and leaves it
/// exactly as it was, which is what a radio station should do.
///
/// **Except when the URL is not a stream at all**, which is what a music
/// service's `getMediaURI` usually hands back: Deezer and TIDAL both answer
/// with a plain, signed, seekable FLAC file. A session cannot play one - see
/// [`Upnp::play_url_as_track`] for the measurement - so a file goes to the
/// transport instead, and the queue's position is lost. That is the worse of
/// two behaviours and better than the only alternative, which was silence.
pub async fn stream_url(
    session: &session::Session,
    target: &Target,
    url: &str,
    title: &str,
    service: Option<&sonos::smapi::Service>,
    wait: Duration,
) -> Result<Started> {
    // Together: the probe goes out to the internet and can take its whole
    // budget, which the coordinator's connection need not wait behind.
    let (coordinator, file) = tokio::join!(session::coordinator(session, target), finite_file(url));
    let coordinator = coordinator?;

    if let Some(file) = file {
        let upnp = Upnp::new(upnp_ip(target, session.connection.ip()));
        upnp.play_url_as_track(url, title, file.mime.as_deref())
            .await?;
        coordinator.playback(&target.group_id, "play").await?;
    } else {
        let opened = coordinator
            .call(
                json!({
                    "namespace": "playbackSession:1",
                    "command": "createSession",
                    "groupId": target.group_id,
                }),
                json!({ "appId": APP_ID, "appContext": "cli" }),
            )
            .await?;
        let session_id = opened["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("player opened a session but did not name it"))?;

        // stationMetadata is optional, but it is where the name the room
        // displays comes from; without it the stream plays with nothing to
        // show. `service` is omitted entirely for a bare URL - there is no
        // service to name, and naming a false one would put a wrong sid in the
        // room's now-playing.
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
    }

    // **The load succeeding is not the stream playing.** `loadStreamUrl`
    // accepts a URL it cannot play and then leaves the room idle without ever
    // erroring, so a confirmation printed here would be a guess. The same
    // reasoning as `use_tv_input`, which waits and then goes and looks: a
    // lost - or in this case meaningless - answer is checked rather than
    // believed.
    if wait.is_zero() {
        return Ok(Started::Starting);
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
                    Some("PLAYING") => return Ok(Started::Playing),
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
    Ok(started)
}

/// The reverse-DNS id x2rock names itself with to a player: on every audio
/// clip, where `loadAudioClip` requires one (an absent `appId` is
/// `ERROR_INVALID_PARAMETER`) and groups a caller's clips under it, and on every
/// playback session, which the player then reports back as `clientId`. One
/// constant, so the two can never name the program differently - they did,
/// until 2026-10-05. Under rahga.com, the project's own domain.
pub const APP_ID: &str = "com.rahga.x2rock";


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
pub async fn play_confirmed(
    player: &Connection,
    upnp: &Upnp,
    group: &str,
    room: &str,
) -> Result<()> {
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


/// Persist a token a service handed back inside a `tokenRefreshRequired`
/// fault, so the next command does not pay for the same refresh again.
///
/// Takes an already-loaded store rather than loading its own - every caller
/// either already has one in scope from resolving the account in the first
/// place, or is about to load one fresh for this alone, and hiding a second
/// load inside this function was paying for the same file twice in the one
/// command this feature exists to make cheaper.
///
/// A blank `private_key` is not trusted the way [`sonos::smapi::parse_device_auth`]
/// trusts one on a fresh link (there, an honest empty key is the fair test of
/// whether the service meant it). Here it would silently overwrite a key that
/// has been working, for a service that may simply not have sent one this
/// time - so the existing key is kept instead when the refresh's is empty.
///
/// Best-effort and silent otherwise: the refreshed token already did its job
/// for this call, in memory, whether or not it reaches disk, and a link that
/// predates this feature (or was somehow removed mid-command) is nothing to
/// report - there is no account left to attach the refresh to.
pub fn save_refreshed_token(
    creds: &mut credentials::Credentials,
    household: &str,
    service_id: &str,
    account: Option<&str>,
    refreshed: sonos::smapi::RefreshedToken,
) {
    // The account the token came from, not whichever one this service would
    // resolve to now: with two accounts for one service, writing the refresh to
    // the preferred one would overwrite a token that was never stale and leave
    // the stale one in place.
    let Some(existing) = creds.account(household, service_id, account).cloned() else {
        return;
    };
    let _ = creds.update(|now| {
        now.apply_refresh(household, service_id, account, &existing, refreshed);
        Ok(())
    });
}

/// A play path's refresh: persisted against the household and account the
/// token names, and the token to use from here on handed back - the refreshed
/// one, or the original when nothing was refreshed. For the paths that hold no
/// `Credentials` of their own (`play-item`'s stream and resume), which load one
/// only when there is something to write.
pub fn after_refresh(
    service_id: &str,
    token: Option<&sonos::smapi::Token>,
    refreshed: Option<sonos::smapi::RefreshedToken>,
) -> Option<sonos::smapi::Token> {
    let Some(new_token) = refreshed else {
        return token.cloned();
    };
    let fresh = merged(token.cloned().unwrap_or_default(), &new_token);
    if let Some(household) = fresh.household.as_deref()
        && let Ok(mut creds) = credentials::Credentials::load()
    {
        save_refreshed_token(
            &mut creds,
            household,
            service_id,
            fresh.account.as_deref(),
            new_token,
        );
    }
    Some(fresh)
}

/// `old` with a refresh applied: the new token, and the new private key
/// unless the refresh sent none (see [`save_refreshed_token`] on why a blank
/// one is not trusted). Household, account, serial and selector are the
/// token's own and do not change.
pub fn merged(old: sonos::smapi::Token, new: &sonos::smapi::RefreshedToken) -> sonos::smapi::Token {
    sonos::smapi::Token {
        token: new.auth_token.clone(),
        key: if new.private_key.is_empty() {
            old.key
        } else {
            new.private_key.clone()
        },
        ..old
    }
}

/// The token to use for whatever comes right after a call that may have
/// refreshed it - the refreshed one, persisted along the way, or the original
/// unchanged. Without this, a `browse`/`search --play` that had to refresh
/// mid-call handed the very next call (`play_item`) the token that call had
/// just proven stale.
///
/// Writes into the store the caller already holds - every caller loaded one
/// to look the token up in the first place - so the refresh is saved through
/// the same `Credentials` the command is working from, rather than a second
/// copy loaded here that could disagree with it.
pub fn use_refreshed_token(
    creds: &mut credentials::Credentials,
    service_id: &str,
    token: Option<sonos::smapi::Token>,
    refreshed: Option<sonos::smapi::RefreshedToken>,
) -> Option<sonos::smapi::Token> {
    let Some(new_token) = refreshed else {
        return token;
    };
    let fresh = merged(token.unwrap_or_default(), &new_token);
    // The refresh can only be persisted against the household the token names;
    // a token without one (there should be none from the store) is used for
    // this call and simply not written back.
    if let Some(hh) = fresh.household.as_deref() {
        save_refreshed_token(creds, hh, service_id, fresh.account.as_deref(), new_token);
    }
    Some(fresh)
}


/// A `play` the player refused because it cannot play what it holds, read off
/// the typed refusal; `None` for any other failure (a lost socket, a stale
/// group id), which is not about the source and must not be treated as one.
pub fn refused_play(e: &anyhow::Error) -> Option<sonos::proto::PlaybackError> {
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
pub async fn playback_failed(
    room: &str,
    error: &sonos::proto::PlaybackError,
    upnp: &Upnp,
) -> anyhow::Error {
    let from_queue = upnp.playing_from_queue().await.unwrap_or(false);
    playback_failed_message(room, error, from_queue)
}


/// How often to ask. Cheap - it is one Control API call to a player on the LAN.
pub const STREAM_POLL: Duration = Duration::from_millis(500);

/// One wording for the three outcomes, shared by every caller that starts a
/// stream so they cannot describe the same result differently.
///
/// `Silent` is an error rather than a warning: the caller asked for sound and
/// there is none, and an agent driving this needs a non-zero exit and a code to
/// branch on rather than a cheerful line it has to go and disprove. No `fix` -
/// nothing here can mint a stream that plays.
pub fn report_started(room: &str, title: &str, on: Option<&str>, started: &Started) -> Result<()> {
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
                hint::Code::StreamDidNotPlay,
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
            return Err(hint::Hint::new(message, hint::Code::StreamUnverified, None).into());
        }
    }
    Ok(())
}

/// The `Content-Type`, when it is an audio type - the only kind worth telling
/// the player, which otherwise guesses from the URL's extension. Parameters
/// (`; charset=…`) are dropped.
pub fn audio_type(head: &str) -> Option<String> {
    head.lines().find_map(|l| {
        let (name, value) = l.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case("content-type") {
            return None;
        }
        let kind = value.split(';').next()?.trim().to_ascii_lowercase();
        kind.starts_with("audio/").then_some(kind)
    })
}


/// How long `replay` keeps pressing play after `loadContent`. The load is
/// asynchronous, and a `play` that overtakes it goes one of two ways: refused
/// as `ERROR_PLAYBACK_NO_CONTENT`, or - worse - accepted and then lost, the
/// room settling at IDLE once the load lands (a YouTube Music album, twice,
/// 2026-09-26). A play sent two seconds after the load started every item
/// tried, so the play is repeated until the room is seen playing.
pub const LOAD_SETTLE: std::time::Duration = std::time::Duration::from_secs(12);


/// A play the player accepted without ever leaving idle, and without raising an
/// error to say why - most often a room with nothing loaded.
pub fn stayed_idle(room: &str) -> anyhow::Error {
    hint::Hint::new(
        format!(
            "{room}: asked to play, but still idle {}s later, with no error from the \
             player. The room most likely has nothing loaded - `x2rock now` shows what it \
             holds, and `favorite`, `bookmark` or a search starts something.",
            STREAM_START.as_secs()
        ),
        hint::Code::PlaybackFailed,
        None,
    )
    .into()
}


pub fn playback_failed_message(
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
    hint::Hint::new(message, hint::Code::PlaybackFailed, None).into()
}


/// The token and cdudn for `bookmark`'s own account on `service`.
///
/// The bookmark's URI already carries the serial it was kept from, so the
/// cdudn names that same account - not the preferred one - and so does the
/// token a stream fallback uses (see `Naming`). A serial the store does not
/// hold names no selector, the `-0-` form, and streams with the preferred
/// account. An unreadable store costs the naming, not the play: most
/// bookmarks need no stored account at all.
pub async fn account_naming(
    session: &Session,
    service: &sonos::smapi::Service,
    bookmark: &bookmarks::Bookmark,
) -> Result<(Option<sonos::smapi::Token>, String)> {
    let household = session.connection.household_id().await?;
    let linked = credentials::Credentials::load().unwrap_or_else(|e| {
        eprintln!("x2rock: stored accounts unreadable ({e:#}); playing without them");
        credentials::Credentials::default()
    });
    let own = bookmark
        .account
        .parse()
        .ok()
        .and_then(|sn| linked.token_by_serial(&household, &service.id, sn));
    let selector = own.as_ref().and_then(|t| t.selector.as_deref());
    let token = own
        .clone()
        .or_else(|| linked.token_for(&household, &service.id));
    let cdudn = service.cdudn_for(selector).ok_or_else(|| {
        anyhow!(
            "{} has no service type in this player's list, so {:?} cannot name its account",
            service.name,
            bookmark.name
        )
    })?;
    Ok((token, cdudn))
}

/// What is playing in `group`, as a bookmark would keep it: the track, or with
/// `container` the album or playlist it comes from. What `favorite add` saves
/// and what `favorite remove` looks for.
pub async fn playing_item(
    player: &Connection,
    group: &str,
    container: bool,
) -> Result<bookmarks::Bookmark> {
    let meta = player.metadata(group).await?;
    let (id, name, artist, art, kind) = if container {
        let c = meta
            .container
            .ok_or_else(|| anyhow!("nothing is playing to favorite"))?;
        (c.id, c.name, None, c.image_url, c.kind)
    } else {
        let t = meta
            .current_item
            .and_then(|i| i.track)
            .ok_or_else(|| anyhow!("nothing is playing to favorite"))?;
        (
            t.id,
            t.name,
            t.artist.and_then(|a| a.name),
            t.image_url,
            Some("track".to_string()),
        )
    };
    let id = id.ok_or_else(|| anyhow!("the player reported no id for what is playing"))?;
    let name = name.ok_or_else(|| anyhow!("what is playing has no name"))?;
    let mut item = bookmarks::Bookmark::from_id(&name, &id)?;
    item.artist = artist;
    item.art_url = art;
    item.kind = kind;
    Ok(item)
}

/// The URI and DIDL a queue would be given for `item`, which is also what
/// makes a favorite the household types and names like the Sonos app's own.
pub fn favorite_source(item: &bookmarks::Bookmark, cdudn: &str) -> Result<(String, String)> {
    match item.kind.as_deref() {
        Some("track") => Ok((item.uri(), item.didl(cdudn))),
        Some(kind) if bookmarks::container_holds_tracks(kind) => Ok((
            bookmarks::container_uri(&item.object_id, &item.service_id, Some(&item.account)),
            bookmarks::container_didl(&item.object_id, &item.name, kind, cdudn),
        )),
        other => bail!(
            "{:?} is {}, which a favorite saved from here cannot hold; only a song, \
             an album or a playlist",
            item.name,
            other.map_or("of no known kind".to_string(), |k| format!("a {k}")),
        ),
    }
}

/// The favorites that are `item`, by the URI a save would give them or by the
/// service id inside them - the second is what finds a shortcut the Sonos app
/// saved, which has no URI at all.
pub fn favorites_of<'a>(
    favorites: &'a [upnp::BrowseItem],
    item: &bookmarks::Bookmark,
    uri: Option<&str>,
) -> Vec<&'a upnp::BrowseItem> {
    favorites
        .iter()
        .filter(|f| {
            (uri.is_some() && f.uri.as_deref() == uri)
                || f.service_object().is_some_and(|(service, object)| {
                    service == item.service_id && object == item.object_id
                })
        })
        .collect()
}


/// The service catalogue, brought up to date against the player the session
/// reached. Every by-id lookup wants this before it trusts an id: a cleared or
/// schema-bumped cache would otherwise make a bookmark or a remembered stream
/// look unknown until some `search` happened to rebuild it. Saved when the
/// refresh changed it, so the next run need not parse the descriptors again.
pub async fn refreshed_catalogue(session: &Session) -> Result<catalogue::Catalogue> {
    let mut catalogue = catalogue::Catalogue::load();
    catalogue
        .refresh_and_save(&Upnp::new(session.connection.ip()), false)
        .await?;
    Ok(catalogue)
}

/// What saving the playing track (or its album or playlist) as a Sonos
/// favourite did.
#[derive(Debug, Clone)]
pub struct SavedFavorite {
    pub name: String,
    pub service: String,
    pub kind: Option<String>,
    /// The favourite's id, without its `FV:2/`, where one is known.
    pub id: Option<String>,
    /// False when it was a favourite already: a duplicate is never made.
    pub saved: bool,
}

/// Save what `target` is playing - the track, or with `container` its album or
/// playlist - to the household's Sonos favourites, unless it is one already.
/// Looked for first, rather than left to the player's 803: a shortcut the Sonos
/// app saved has no URI for the player to recognise.
pub async fn save_playing_favorite(
    session: &session::Session,
    player: &Connection,
    target: &Target,
    container: bool,
) -> Result<SavedFavorite> {
    let mut item = playing_item(player, &target.group_id, container).await?;
    let catalogue = refreshed_catalogue(session).await?;
    let service = catalogue
        .by_id(&item.service_id)
        .ok_or_else(|| anyhow!("service {} is not in this player's service list", item.service_id))?
        .clone();
    item.service_name = Some(service.name.clone());
    let (_, cdudn) = account_naming(session, &service, &item).await?;
    let (uri, didl) = favorite_source(&item, &cdudn)?;
    let upnp = Upnp::new(upnp_ip(target, player.ip()));
    let existing = upnp.browse_content("FV:2").await?;
    let already = favorites_of(&existing, &item, Some(&uri));
    let saved = match already.first() {
        Some(_) => None,
        None => {
            upnp.create_favorite(&upnp::NewFavorite {
                title: &item.name,
                uri: &uri,
                metadata: &didl,
                art_url: item.art_url.as_deref(),
                service_name: Some(&service.name),
            })
            .await?
        }
    };
    let id = saved.as_deref().or(already.first().map(|f| f.id.as_str())).map(|i| i.trim_start_matches("FV:2/").to_owned());
    Ok(SavedFavorite { name: item.name.clone(), service: service.name.clone(), kind: item.kind.clone(), id, saved: saved.is_some() })
}


/// What `rate` did: `{service, rating, action, should_skip, skipped,
/// message}` as JSON, and one line of text.
#[derive(Debug, Serialize)]
pub struct RateOutcome {
    pub service: String,
    pub rating: &'static str,
    pub action: String,
    pub should_skip: Option<bool>,
    pub skipped: bool,
    pub message: Option<String>,
    /// What the rating did in the service's terms - "Added to favorites".
    #[serde(skip)]
    pub done: &'static str,
    #[serde(skip)]
    pub notes: Vec<String>,
}

/// [`run_rate`]'s work, returned rather than printed, for the TUI's rating keys.
pub async fn rate(
    player: &Connection,
    group: &str,
    room_name: &str,
    want: sonos::smapi::RatingKind,
    refresh: bool,
) -> Result<RateOutcome> {
    // Independent of each other - one is the Control API over the socket that
    // is already open, the other a UPnP `ListAvailableServices` to the same
    // player - so they overlap rather than queue. `refresh` is a no-op when
    // the player's service-list version has not moved.
    let mut catalogue = catalogue::Catalogue::load();
    let upnp = Upnp::new(player.ip());
    let (meta, dirty) =
        tokio::try_join!(player.metadata(group), catalogue.refresh(&upnp, refresh))?;
    let track_id = meta
        .track()
        .and_then(|t| t.id.as_ref())
        .filter(|id| id.is_real())
        .ok_or_else(|| {
            anyhow!(
                "nothing rateable is playing in {room_name} (a Live broadcast's track carries \
                 no id to rate)"
            )
        })?;
    let service_id = track_id
        .service_id
        .as_deref()
        .ok_or_else(|| anyhow!("the current track names no service, so it cannot be rated"))?;

    let service = catalogue
        .by_id(service_id)
        .ok_or_else(|| anyhow!("service {service_id} is not in this player's service list"))?
        .clone();

    // Asked before the call for the same reason `search` asks before
    // `categories_for`: a freshly learned *empty* list is exactly what is
    // worth writing, and `is_empty()` afterwards cannot tell that from a hit.
    let learned = !catalogue.ratings_cached(&service.id);
    let ratings = catalogue.ratings_for(&service).await?;
    if dirty || learned {
        catalogue.save()?;
    }
    ensure!(
        !ratings.is_empty(),
        "{} publishes no ratings, so nothing here can be rated.{}",
        service.name,
        // Only worth suggesting when the answer came from cache: a reply just
        // fetched says what the service currently says, and re-fetching it is
        // a round trip that cannot change the outcome.
        if learned {
            ""
        } else {
            " (Cached; `x2rock rate --refresh` re-reads it if the service has since added them.)"
        }
    );

    let mut linked = credentials::Credentials::load()?;
    let household = player.household_id().await?;
    let token = linked.token_for(&household, &service.id);

    let mut refreshed = None;
    let properties = sonos::smapi::extended_metadata(
        &service,
        token.as_ref(),
        &track_id.object_id,
        &mut refreshed,
    )
    .await?;
    let token = use_refreshed_token(&mut linked, &service.id, token, refreshed);

    // Two different refusals, told apart: no rating state at all (a track the
    // service does not rate), and a state that does not offer this one (a
    // Deezer track already in the favourites has no second heart to give).
    // One message for both sent a person after the wrong cause.
    let offered: Vec<&sonos::smapi::Rating> = properties
        .iter()
        .filter_map(|(propname, value)| {
            sonos::smapi::RatingsMatch::offered(&ratings, propname, value)
        })
        .flatten()
        .collect();
    ensure!(
        !offered.is_empty(),
        "{} did not report a rating state for the current track, so it cannot be rated \
         right now.",
        service.name
    );
    let chosen = offered
        .iter()
        .copied()
        .find(|r| r.kind() == Some(want))
        .ok_or_else(|| {
            let mut can: Vec<&str> = offered
                .iter()
                .filter_map(|r| r.kind())
                .map(rate_word)
                .collect();
            can.dedup();
            anyhow!(
                "{} does not offer `rate {}` for this track right now; it offers {}.",
                service.name,
                rate_word(want),
                match can.is_empty() {
                    true => "nothing x2rock can name".to_string(),
                    false => can
                        .iter()
                        .map(|w| format!("`rate {w}`"))
                        .collect::<Vec<_>>()
                        .join(" and "),
                }
            )
        })?;

    let mut refreshed = None;
    let result = sonos::smapi::rate_item(
        &service,
        token.as_ref(),
        &track_id.object_id,
        &chosen.id,
        &mut refreshed,
    )
    .await?;
    // Nothing follows that needs the token; the call is for its save.
    let _ = use_refreshed_token(&mut linked, &service.id, token, refreshed);

    // The rating already landed - a failure here must not read as the rating
    // itself having failed, which `?` would do (and which could send a caller
    // that retries on error back to rate the same track twice). Warn and move
    // on; the room simply keeps playing what it was.
    let mut notes = Vec::new();
    let skipped = result.should_skip == Some(true)
        && match player.playback(group, "skipToNextTrack").await {
            Ok(()) => true,
            Err(e) => {
                notes.push(format!(
                    "x2rock: rated successfully, but the requested skip failed: {e:#}"
                ));
                false
            }
        };

    Ok(RateOutcome {
        service: service.name.clone(),
        rating: rate_word(want),
        action: chosen.string_id.clone(),
        should_skip: result.should_skip,
        skipped,
        message: result.message_string_id,
        done: rate_done(chosen),
        notes,
    })
}

/// A [`RatingKind`](sonos::smapi::RatingKind) as `rate` spells it.
pub fn rate_word(kind: sonos::smapi::RatingKind) -> &'static str {
    use sonos::smapi::RatingKind;
    match kind {
        RatingKind::Up => "up",
        RatingKind::Down => "down",
        RatingKind::Unfavorite => "unfavorite",
    }
}

/// What a landed rating did, in the service's terms: a heart service saves a
/// favourite where a thumbs one rates up.
pub fn rate_done(rating: &sonos::smapi::Rating) -> &'static str {
    let id = rating.string_id.to_uppercase();
    match rating.kind() {
        _ if id.starts_with("SAVE") => "Added to favorites",
        _ if id.starts_with("SKIP") => "Added to disliked tracks",
        Some(sonos::smapi::RatingKind::Unfavorite) => "Removed from favorites",
        Some(sonos::smapi::RatingKind::Down) => "Rated down",
        _ => "Rated up",
    }
}

/// Which ratings the track a group plays can be given now, by its service.
#[derive(Debug, Clone)]
pub struct RatingOffers {
    pub service: String,
    pub kinds: Vec<sonos::smapi::RatingKind>,
    /// A heart service (Deezer's favourites) rather than thumbs.
    pub heart: bool,
}

/// The ratings on offer for what `group` plays: `None` for a track with
/// nothing to rate (no id, a service publishing no ratings, no rating state).
pub async fn rating_offers(player: &Connection, group: &str) -> Result<Option<RatingOffers>> {
    let mut catalogue = catalogue::Catalogue::load();
    let upnp = Upnp::new(player.ip());
    let (meta, dirty) = tokio::try_join!(player.metadata(group), catalogue.refresh(&upnp, false))?;
    let Some(track_id) = meta.track().and_then(|t| t.id.as_ref()).filter(|id| id.is_real()) else {
        return Ok(None);
    };
    let Some(service) = track_id.service_id.as_deref().and_then(|id| catalogue.by_id(id)).cloned() else {
        return Ok(None);
    };
    let learned = !catalogue.ratings_cached(&service.id);
    let ratings = catalogue.ratings_for(&service).await?;
    if dirty || learned {
        catalogue.save()?;
    }
    if ratings.is_empty() {
        return Ok(None);
    }
    let mut linked = credentials::Credentials::load()?;
    let household = player.household_id().await?;
    let token = linked.token_for(&household, &service.id);
    let mut refreshed = None;
    let properties = sonos::smapi::extended_metadata(&service, token.as_ref(), &track_id.object_id, &mut refreshed).await?;
    let _ = use_refreshed_token(&mut linked, &service.id, token, refreshed);
    let offered: Vec<&sonos::smapi::Rating> = properties
        .iter()
        .filter_map(|(propname, value)| sonos::smapi::RatingsMatch::offered(&ratings, propname, value))
        .flatten()
        .collect();
    if offered.is_empty() {
        return Ok(None);
    }
    let heart = offered.iter().any(|r| r.string_id.to_uppercase().starts_with("SAVE") || r.kind() == Some(sonos::smapi::RatingKind::Unfavorite));
    let mut kinds: Vec<sonos::smapi::RatingKind> = offered.iter().filter_map(|r| r.kind()).collect();
    kinds.dedup();
    Ok(Some(RatingOffers { service: service.name.clone(), kinds, heart }))
}
