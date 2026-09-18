//! What the household has saved and what a room has queued: favorites, saved
//! playlists, x2rock's own bookmarks, and the queue itself - plus `play-item`
//! and `queue-item`, which take a search or browse hit by id. Everything here
//! resolves a name to something the player already knows about; finding new
//! music is `services.rs`, and a bare URL is `stream.rs`.

use std::net::IpAddr;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;

use super::stream::{StreamStart, stream_item};
use super::{find_named, is_refusal, mmss};
use crate::cli::{BookmarksAction, QueueAction};
use crate::session::{self, Session, Target};
use crate::sonos::local::Connection;
use crate::sonos::proto::Favorite;
use crate::sonos::upnp::{self, Upnp};
use crate::state::State;
use crate::{bookmarks, catalogue, credentials, sonos};

fn print_sources(sources: &[upnp::BrowseItem], json: bool) {
    if json {
        let items: Vec<_> = sources
            .iter()
            .map(|i| {
                json!({
                    "id": i.id,
                    "title": i.title,
                    // Saved playlists live under SQ:, favorites under FV:.
                    "kind": if i.id.starts_with("SQ:") { "playlist" } else { "favorite" },
                    "uri": i.uri,
                    "addable": i.can_enqueue(),
                    "art_url": i.art_url,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&items).expect("serializable"));
        return;
    }
    if sources.is_empty() {
        println!("Nothing to add.");
        return;
    }
    for item in sources {
        let kind = if item.id.starts_with("SQ:") {
            "playlist"
        } else {
            "favorite"
        };
        // A service's own content can only replace the queue, never join it.
        let how = if item.can_enqueue() { "add" } else { "play" };
        println!("{:<10} {:<9} {:<5} {}", item.id, kind, how, item.title);
    }
}

/// The playlist or favorite a query names, searched across both.
///
/// Same rules as [`find_favorite`]: an exact id wins, then a case-insensitive
/// name match, with several matches reported rather than guessed between and a
/// whole name beating a partial one.
pub fn find_content<'a>(
    items: &'a [upnp::BrowseItem],
    query: &str,
) -> Result<&'a upnp::BrowseItem> {
    find_named(
        items,
        query,
        |i| &i.id,
        |i| &i.title,
        "source",
        "x2rock queue sources",
    )
}

/// One position, or an inclusive `4-8` range, as a start and a count.
fn parse_range(text: &str) -> Result<(u32, u32)> {
    let (start, count) = match text.split_once('-') {
        None => (text.trim().parse::<u32>()?, 1),
        Some((first, last)) => {
            let first: u32 = first.trim().parse()?;
            let last: u32 = last.trim().parse()?;
            ensure!(last >= first, "{text}: the range ends before it starts");
            (first, last - first + 1)
        }
    };
    ensure!(start >= 1, "queue tracks are numbered from 1");
    Ok((start, count))
}

fn print_favorites(favorites: &[Favorite], json: bool) {
    if json {
        let items: Vec<_> = favorites
            .iter()
            .map(|f| {
                json!({
                    "id": f.id,
                    "name": f.name,
                    "description": f.description,
                    "service": f.service(),
                    "type": f.kind(),
                    "art_url": f.image_url,
                    // A heuristic, not a guarantee: false marks an empty shell -
                    // neither a service nor a content type, which is what a
                    // favorite for a shut-down service decays into. It cannot
                    // catch a live service that recycled an id (iHeartRadio does
                    // this at the holidays), only one with nothing left to play.
                    "playable": f.service().is_some() || f.kind().is_some(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&items).expect("serializable"));
        return;
    }
    if favorites.is_empty() {
        println!("No favorites.");
        return;
    }
    for favorite in favorites {
        let tags: Vec<_> = [
            favorite.kind().map(str::to_lowercase),
            favorite.service().map(str::to_string),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut line = format!("{:>4}  {}", favorite.id, favorite.name);
        if !tags.is_empty() {
            line.push_str(&format!("  [{}]", tags.join(", ")));
        }
        println!("{line}");
    }
}

fn find_favorite<'a>(favorites: &'a [Favorite], query: &str) -> Result<&'a Favorite> {
    find_named(
        favorites,
        query,
        |f| &f.id,
        |f| &f.name,
        "favorite",
        "x2rock favorites",
    )
}

/// `in_use` is the Sonos app's "Queue" versus "Queue (Not In Use)": whether the
/// group's source is its queue, which an empty queue can still be.
fn queue_json(queue: &upnp::Queue, current: u32, in_use: bool) -> serde_json::Value {
    let items: Vec<_> = queue
        .items
        .iter()
        .map(|i| {
            json!({
                "index": i.index,
                "title": i.title,
                "artist": i.artist,
                "album": i.album,
                "duration_ms": i.duration.map(|d| d.as_millis() as u64),
                "art_url": i.art_url,
                "current": i.index == current,
            })
        })
        .collect();
    json!({ "total": queue.total, "current": current, "in_use": in_use, "items": items })
}

fn print_queue(queue: &upnp::Queue, current: u32, in_use: bool, json: bool) {
    if json {
        println!("{}", queue_json(queue, current, in_use));
        return;
    }
    if queue.items.is_empty() {
        println!("Queue is empty.");
        return;
    }
    for item in &queue.items {
        let marker = if item.index == current { "▶" } else { " " };
        // A row the player has nothing to say about still occupies a position,
        // and a blank line reads as corruption rather than as an answer. The
        // Sonos app makes these: its "..." -> Play Now adds an entry carrying no
        // metadata at all, where tapping the track on the now-playing view adds
        // a normal one. Nothing here can fill it in - the player has no title to
        // give - so it is named rather than left empty.
        let title = if item.title.trim().is_empty() {
            "(no title from the player)"
        } else {
            &item.title
        };
        let mut line = format!("{marker} {:>3}  {}", item.index, title);
        if let Some(artist) = &item.artist {
            line.push_str(" — ");
            line.push_str(artist);
        }
        let length = mmss(item.duration);
        if !length.is_empty() {
            line.push_str(&format!("  {length}"));
        }
        println!("{line}");
    }
    if (queue.items.len() as u32) < queue.total {
        println!(
            "  … {} more (showing the first {})",
            queue.total - queue.items.len() as u32,
            queue.items.len()
        );
    }
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
pub async fn play_item(
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
    // A container of containers is not queue material and is not a stream
    // either, so neither path fits: falling through would try to stream an
    // artist, which fails silently minutes later at IDLE.
    if kind.is_some_and(bookmarks::container_of_containers) {
        bail!(
            "{title:?} is a {}, which holds albums and playlists rather than \
             tracks. Open it with `x2rock browse` and play what is inside.",
            kind.unwrap_or_default()
        );
    }
    if let (false, Some(cdudn)) = (streamish, service.cdudn()) {
        match enqueue_item(session, room, service, &cdudn, id, title, kind, true).await {
            Ok(()) => return Ok(()),
            // Only a refusal earns the fallback. An unreachable coordinator is
            // not the item's fault and the stream session cannot fix it.
            Err(e) if is_refusal(&e) => {
                eprintln!("x2rock: {title:?} would not go in the queue ({e:#}); streaming it")
            }
            Err(e) => return Err(e),
        }
    }
    stream_item(session, room, service, token, id, title, StreamStart::Fresh).await
}

/// Put a service item in the room's queue, and optionally jump to it.
///
/// Playing is deliberately the same sequence `bookmark` uses, down to making the
/// queue the current source first: after a station it is not, and `Seek` fails
/// with 701. With `play` false none of that happens - the track is added to the
/// end and whatever is playing keeps playing, which is the whole point of the
/// distinction.
#[allow(clippy::too_many_arguments)]
async fn enqueue_item(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    cdudn: &str,
    id: &str,
    title: &str,
    kind: Option<&str>,
    play: bool,
) -> Result<()> {
    let target = session::target(&session.groups, room)?;
    let upnp = Upnp::new(
        target
            .coordinator_ip
            .unwrap_or_else(|| session.connection.ip()),
    );
    // **A container is enqueued by a different scheme from a track.** A track is
    // fetched; a container is expanded by the player, which walks it and adds
    // each track it holds. Handing a container a track's URI is what got UPnP
    // error 804 - the player calling the URI malformed, rather than the 800 it
    // gives for something it cannot play.
    //
    // No `sn=` either way: nothing here has ever played, so there is no serial
    // to harvest, and the player does not need one. See `bookmarks::service_uri`.
    let container = kind.is_some_and(bookmarks::container_holds_tracks);
    let (uri, didl) = match container {
        true => (
            bookmarks::container_uri(id, &service.id, None),
            bookmarks::container_didl(id, title, kind.unwrap_or_default(), cdudn),
        ),
        false => (
            bookmarks::service_uri(id, &service.id, None),
            bookmarks::service_didl(id, title, cdudn),
        ),
    };
    let length = upnp.add_to_queue(&uri, &didl, false).await?;
    if !play {
        println!("{} — queued {title} at {length}", target.name);
        return Ok(());
    }
    if !upnp.playing_from_queue().await? {
        upnp.use_queue(&target.coordinator_id).await?;
    }
    upnp.seek_track(length).await?;
    let coordinator = session::coordinator(session, &target).await?;
    coordinator.playback(&target.group_id, "play").await?;
    println!("{} — {title} on {}", target.name, service.name);
    Ok(())
}

/// Put a bookmark in the room's queue and jump to it, using its own remembered
/// URI and DIDL (account serial included) rather than rebuilding them.
///
/// Split out of `Command::Bookmark` so a refusal can fall back to streaming the
/// same way a fresh search/browse hit does in [`play_item`] - a bookmarked
/// Sonos Radio "program" answers `AddURIToQueue` with the identical UPnP 800 a
/// live stream does, because it is not discrete queue material either, and
/// until this existed `bookmark` had no way to notice and just gave up.
async fn play_bookmark(
    session: &session::Session,
    room: Option<&str>,
    bookmark: &bookmarks::Bookmark,
    cdudn: &str,
) -> Result<()> {
    let target = session::target(&session.groups, room)?;
    let upnp = Upnp::new(
        target
            .coordinator_ip
            .unwrap_or_else(|| session.connection.ip()),
    );
    let length = upnp
        .add_to_queue(&bookmark.uri(), &bookmark.didl(cdudn), false)
        .await?;
    if !upnp.playing_from_queue().await? {
        upnp.use_queue(&target.coordinator_id).await?;
    }
    upnp.seek_track(length).await?;
    let coordinator = session::coordinator(session, &target).await?;
    coordinator.playback(&target.group_id, "play").await?;
    Ok(())
}

/// `x2rock play-item`: play a hit whose id is already known.
///
/// `search --play N` re-runs the search to find the Nth result, which costs a
/// second round trip and can land on a different item if the service reorders.
/// Anything holding results already - the bar widget - should come here instead.
pub async fn run_play_item(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    service: &str,
    kind: Option<&str>,
    id: &str,
    title: Option<&String>,
) -> Result<()> {
    let mut state = State::load()?;
    let session = session::connect(ip, &mut state, household, room).await?;
    let mut catalogue = catalogue::Catalogue::load();
    catalogue
        .refresh(&Upnp::new(session.connection.ip()), false)
        .await?;
    let linked = credentials::Credentials::load()?;
    let usable = catalogue.usable(&linked);
    let chosen = catalogue::Catalogue::find(&usable, service)?.clone();
    let token = linked.token_for(&chosen.id);
    play_item(
        &session,
        room,
        &chosen,
        token.as_ref(),
        kind,
        id,
        title.map(String::as_str).unwrap_or(id),
    )
    .await
}

/// `queue-item`: the same lookup `run_play_item` does, then enqueue without
/// playing.
pub async fn run_queue_item(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    service: &str,
    kind: Option<&str>,
    id: &str,
    title: Option<&String>,
) -> Result<()> {
    let mut state = State::load()?;
    let session = session::connect(ip, &mut state, household, room).await?;
    let mut catalogue = catalogue::Catalogue::load();
    catalogue
        .refresh(&Upnp::new(session.connection.ip()), false)
        .await?;
    let linked = credentials::Credentials::load()?;
    let usable = catalogue.usable(&linked);
    let chosen = catalogue::Catalogue::find(&usable, service)?.clone();
    let title = title.map(String::as_str).unwrap_or(id);

    // Refused rather than half-worked. `play-item` answers a stream by streaming
    // it, which is a different thing from queueing and cannot be what someone
    // pressing "add to queue" meant.
    if kind.is_some_and(|k| k.eq_ignore_ascii_case("stream")) {
        bail!(
            "{title:?} is a live stream, which has no queue form. \
             Play it with `x2rock play-item` instead."
        );
    }
    // A place rather than a thing. The player refuses it with a bare UPnP 804,
    // which says the URI was malformed and not that an artist has no tracks of
    // its own to add - so this says the second, where every other refusal here
    // explains itself.
    if kind.is_some_and(bookmarks::container_of_containers) {
        bail!(
            "{title:?} is a {}, which holds albums and playlists rather than \
             tracks. Open it with `x2rock browse` and queue what is inside.",
            kind.unwrap_or_default()
        );
    }
    // Without a service type there is no cdudn, and `SA_RINCONNone` is not an
    // account - the enqueue would be refused by the player with less to say.
    let Some(cdudn) = chosen.cdudn() else {
        bail!(
            "{} is not in the player's service-type list, so nothing can be \
             built to name the account that owns {title:?}.",
            chosen.name
        );
    };
    enqueue_item(&session, room, &chosen, &cdudn, id, title, kind, false).await
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
    if item.item_type.eq_ignore_ascii_case("stream") || service.cdudn().is_none() {
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

pub fn run_bookmarks(
    action: Option<&BookmarksAction>,
    query: Option<&str>,
    all: bool,
    json: bool,
) -> Result<()> {
    run_bookmarks_at(
        &bookmarks::path()?,
        &mut std::io::stdout(),
        action,
        query,
        all,
        json,
    )
}

fn run_bookmarks_at<W: std::io::Write>(
    path: &std::path::Path,
    out: &mut W,
    action: Option<&BookmarksAction>,
    query: Option<&str>,
    all: bool,
    json: bool,
) -> Result<()> {
    if let Some(act) = action {
        match act {
            BookmarksAction::Remove { query } => {
                let gone = bookmarks::Bookmarks::update_at(path, |list| list.forget(query))?;
                if json {
                    writeln!(out, "{}", json!({ "removed": gone.name }))?;
                } else {
                    writeln!(out, "Forgot {}.", gone.name)?;
                }
                return Ok(());
            }
            BookmarksAction::Pin { query } => {
                let (pinned, was_pinned) =
                    bookmarks::Bookmarks::update_at(path, |list| list.pin(query))?;
                if json {
                    writeln!(
                        out,
                        "{}",
                        json!({
                            "name": pinned.name,
                            "pinned": true,
                            "already_pinned": was_pinned,
                        })
                    )?;
                } else if was_pinned {
                    writeln!(out, "Already pinned {}.", pinned.name)?;
                } else {
                    writeln!(out, "Pinned {}.", pinned.name)?;
                }
                return Ok(());
            }
            BookmarksAction::Rename { query, new_name } => {
                let (old, new) =
                    bookmarks::Bookmarks::update_at(path, |list| list.rename(query, new_name))?;
                if json {
                    writeln!(out, "{}", json!({ "old_name": old, "new_name": new }))?;
                } else {
                    writeln!(out, "Renamed {old} to {new}.")?;
                }
                return Ok(());
            }
            BookmarksAction::Prune => {
                let (pruned, kept) =
                    bookmarks::Bookmarks::update_at(path, |list| Ok(list.prune()))?;
                if json {
                    writeln!(out, "{}", json!({ "pruned": pruned, "total": kept }))?;
                } else if pruned == 0 {
                    writeln!(out, "No unpinned history entries to prune ({kept} kept).")?;
                } else {
                    let entries = if pruned == 1 { "entry" } else { "entries" };
                    writeln!(
                        out,
                        "Pruned {pruned} history {entries}, {kept} bookmarks kept."
                    )?;
                }
                return Ok(());
            }
        }
    }
    let list = bookmarks::Bookmarks::load_from(path)?;
    let mut items = list.listed(all);
    if let Some(query) = query {
        let needle = query.to_lowercase();
        items.retain(|b| b.name.to_lowercase().contains(&needle));
    }
    if json {
        let rows: Vec<_> = items
            .iter()
            .map(|b| {
                // Same field names as `favorites --json` and `search --json`,
                // so the widget's picker can concatenate all three.
                json!({
                    "id": b.object_id,
                    "name": b.name,
                    "type": b.kind,
                    "description": b.artist,
                    "service": b.service_name,
                    "art_url": b.art_url,
                })
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string(&rows)?)?;
    } else if items.is_empty() {
        // Four states were wearing one message, and a query filtering
        // everything out got the worst of it: "Nothing kept. Play something
        // and run `x2rock keep`" told someone with a full file that their
        // file was empty. What is empty, and what to do about it, differ.
        match query {
            Some(q) => {
                // Whether `--all` would have found it is the useful half of
                // the answer, and it costs one pass over what is loaded.
                let deeper = if all {
                    0
                } else {
                    let needle = q.to_lowercase();
                    list.listed(true)
                        .iter()
                        .filter(|b| b.name.to_lowercase().contains(&needle))
                        .count()
                };
                if deeper > 0 {
                    writeln!(
                        out,
                        "Nothing kept matches {q:?}, but {deeper} of what played recently \
                         does. `x2rock bookmarks --all {q:?}`."
                    )?;
                } else if all {
                    writeln!(out, "Nothing kept or played recently matches {q:?}.")?;
                } else {
                    writeln!(out, "Nothing kept matches {q:?}.")?;
                }
            }
            None => {
                let hidden = list.items.len();
                if hidden > 0 && !all {
                    writeln!(
                        out,
                        "Nothing kept, but {hidden} played recently. `x2rock bookmarks --all`."
                    )?;
                } else {
                    writeln!(out, "Nothing kept. Play something and run `x2rock keep`.")?;
                }
            }
        }
    } else {
        for b in items {
            let by = b
                .artist
                .as_deref()
                .map(|a| format!(" — {a}"))
                .unwrap_or_default();
            let on = b
                .service_name
                .as_deref()
                .map(|s| format!("  [{s}]"))
                .unwrap_or_default();
            // A mark for the deliberate ones, so `--all` still tells them
            // apart from whatever happened to play.
            let mark = if b.pinned { "*" } else { " " };
            writeln!(out, "{mark} {}{by}{on}", b.name)?;
        }
    }
    Ok(())
}

/// `x2rock favorites`: the household's saved favorites, or those matching
/// `query`. Household-wide, so it takes no room.
pub async fn favorites(session: &Session, query: Option<&str>, json: bool) -> Result<()> {
    let household = session.connection.household_id().await?;
    let mut favorites = session.connection.favorites(&household).await?.items;
    if let Some(query) = query {
        let needle = query.to_lowercase();
        favorites.retain(|f| f.name.to_lowercase().contains(&needle));
    }
    print_favorites(&favorites, json);
    Ok(())
}

/// `x2rock keep`: remember what `group` is playing - the track, or with
/// `container` the album, playlist or station it came from.
pub async fn keep(
    session: &Session,
    player: &Connection,
    group: &str,
    name: Option<String>,
    container: bool,
) -> Result<()> {
    let meta = player.metadata(group).await?;
    // The track by default: "play this again" almost always means the
    // song, and the container is there for the times it means the album.
    let (id, fallback, artist, art, kind) = if container {
        let c = meta
            .container
            .ok_or_else(|| anyhow!("nothing is playing to keep"))?;
        (c.id, c.name, None, c.image_url, c.kind)
    } else {
        let t = meta
            .current_item
            .and_then(|i| i.track)
            .ok_or_else(|| anyhow!("nothing is playing to keep"))?;
        (
            t.id,
            t.name,
            t.artist.and_then(|a| a.name),
            t.image_url,
            Some("track".to_string()),
        )
    };
    let id = id.ok_or_else(|| {
        anyhow!("the player reported no id for what is playing, so it cannot be kept")
    })?;
    let title = name
        .or(fallback)
        .ok_or_else(|| anyhow!("what is playing has no name; give one: x2rock keep <name>"))?;

    let mut bookmark = bookmarks::Bookmark::from_id(&title, &id)?;
    bookmark.artist = artist;
    bookmark.art_url = art;
    bookmark.kind = kind;
    // The service's own name, for the listing. Best effort: a catalogue
    // that cannot be read costs a label, not the bookmark.
    let mut catalogue = catalogue::Catalogue::load();
    let _ = catalogue
        .refresh(&Upnp::new(session.connection.ip()), false)
        .await;
    bookmark.service_name = catalogue
        .services()
        .iter()
        .find(|s| s.id == bookmark.service_id)
        .map(|s| s.name.clone());

    let replaced = bookmarks::Bookmarks::update(|list| Ok(list.keep(bookmark)))?;
    println!("{} {title}", if replaced { "Updated" } else { "Kept" });
    Ok(())
}

/// `x2rock bookmark`: play something kept earlier on `target`, or with `next`
/// queue it after the current track.
pub async fn bookmark(
    session: &Session,
    player: &Connection,
    target: &Target,
    room: Option<&str>,
    query: &str,
    next: bool,
) -> Result<()> {
    let list = bookmarks::Bookmarks::load()?;
    let bookmark = list.find(query)?.clone();

    // The cdudn names the account the player resolves the content with,
    // and it is derived from the service type list rather than copied
    // from anything - see `Service::cdudn`.
    let mut catalogue = catalogue::Catalogue::load();
    catalogue
        .refresh(&Upnp::new(session.connection.ip()), false)
        .await?;
    // Two different failures, worth telling apart: a service the player
    // has never heard of, and one it lists but gives no type for.
    let service = catalogue
        .services()
        .iter()
        .find(|s| s.id == bookmark.service_id)
        .ok_or_else(|| {
            anyhow!(
                "service {} is not in this player's service list, so {:?} cannot be played",
                bookmark.service_id,
                bookmark.name
            )
        })?
        .clone();
    let cdudn = service.cdudn().ok_or_else(|| {
        anyhow!(
            "{} has no service type in this player's list, so {:?} cannot name its account",
            service.name,
            bookmark.name
        )
    })?;

    if next {
        // Queuing for later, not playing now - streaming would start it
        // immediately and break what `--next` promised, so there is no
        // fallback here: a refusal is just a refusal.
        let upnp = Upnp::new(target.coordinator_ip.unwrap_or(player.ip()));
        upnp.add_to_queue(&bookmark.uri(), &bookmark.didl(&cdudn), true)
            .await?;
        println!("{:<24} {}", target.name, bookmark.name);
    } else {
        match play_bookmark(session, room, &bookmark, &cdudn).await {
            Ok(()) => println!("{:<24} {}", target.name, bookmark.name),
            // The same rule `play_item` follows for a fresh search/browse
            // hit: a refusal means this is not queue material, most often
            // a Sonos Radio-style program with no discrete track to hold
            // a queue position, and the fix is the stream session, not a
            // retry.
            Err(e) if !is_refusal(&e) => return Err(e),
            Err(e) => {
                eprintln!(
                    "x2rock: {:?} would not go in the queue ({e:#}); streaming it",
                    bookmark.name
                );
                let token = credentials::Credentials::load()?.token_for(&service.id);
                stream_item(
                    session,
                    room,
                    &service,
                    token.as_ref(),
                    &bookmark.object_id,
                    &bookmark.name,
                    StreamStart::Fresh,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// `x2rock favorite`: play a household favorite on `group`, by name or id.
pub async fn favorite(
    session: &Session,
    player: &Connection,
    target: &Target,
    group: &str,
    query: &str,
) -> Result<()> {
    let household = session.connection.household_id().await?;
    let favorites = session.connection.favorites(&household).await?;
    let favorite = find_favorite(&favorites.items, query)?;
    // Household-scoped to find, group-scoped to play.
    player.load_favorite(group, &favorite.id).await?;
    println!("{:<24} {}", target.name, favorite.name);
    Ok(())
}

/// `x2rock playlist`: replace the queue with a saved playlist and play it.
pub async fn playlist(
    session: &Session,
    player: &Connection,
    target: &Target,
    group: &str,
    query: &str,
) -> Result<()> {
    let household = session.connection.household_id().await?;
    let saved = session.connection.playlists(&household).await?;
    let playlist = find_named(
        &saved.playlists,
        query,
        |p| &p.id,
        |p| &p.name,
        "playlist",
        "x2rock queue sources",
    )?;
    // Household-scoped to find, group-scoped to play, as with a
    // favorite. The id passed is the bare one this list reports: the
    // `SQ:0` form `queue sources` shows is refused here.
    player.load_playlist(group, &playlist.id).await?;
    println!("{:<24} {}", target.name, playlist.name);
    Ok(())
}

/// `x2rock queue`: list, edit, save or append to `target`'s queue. Every
/// change reports what the queue became rather than what was asked.
pub async fn queue(
    player: &Connection,
    target: &Target,
    action: Option<QueueAction>,
    json: bool,
) -> Result<()> {
    let upnp = Upnp::new(target.coordinator_ip.unwrap_or(player.ip()));
    let room = target.name.as_str();
    match action {
        None => {
            let queue = upnp.queue().await?;
            let in_use = upnp.playing_from_queue().await?;
            let current = if in_use {
                upnp.current_track().await?
            } else {
                0
            };
            print_queue(&queue, current, in_use, json);
        }
        // Changes report what the queue became rather than what was
        // asked for, and read the length cheaply rather than paging the
        // whole queue back just to count it.
        Some(QueueAction::Remove { range }) => {
            let (start, count) = parse_range(&range)?;
            if count == 1 {
                upnp.remove_track(start).await?;
            } else {
                upnp.remove_range(start, count).await?;
            }
            let left = upnp.queue_len().await?;
            if json {
                println!(
                    "{}",
                    json!({ "room": room, "removed": count, "total": left })
                );
            } else {
                let tracks = if count == 1 { "track" } else { "tracks" };
                println!("{room:<24} removed {count} {tracks}, {left} left");
            }
        }
        Some(QueueAction::Clear { yes }) => {
            ensure!(
                yes,
                "clearing the queue cannot be undone; pass --yes to confirm"
            );
            upnp.clear_queue().await?;
            if json {
                println!("{}", json!({ "room": room, "total": 0 }));
            } else {
                println!("{room:<24} queue cleared");
            }
        }
        Some(QueueAction::Move { from, to }) => {
            ensure!(from >= 1 && to >= 1, "queue tracks are numbered from 1");
            upnp.move_track(from, to).await?;
            if json {
                println!("{}", json!({ "room": room, "from": from, "to": to }));
            } else {
                println!("{room:<24} moved track {from} to {to}");
            }
        }
        Some(QueueAction::Sources { query }) => {
            let mut sources = upnp.browse_content("SQ:").await?;
            sources.extend(upnp.browse_content("FV:2").await?);
            // Shortcuts are not sources: they have no resource, so they
            // can neither be enqueued nor played, and offering one only
            // produces "has nothing to play" a step later.
            sources.retain(|item| !item.shortcut);
            if let Some(query) = &query {
                let needle = query.to_lowercase();
                sources.retain(|i| i.title.to_lowercase().contains(&needle));
            }
            print_sources(&sources, json);
        }
        Some(QueueAction::Add { query, next }) => {
            // Saved playlists and favorites both enqueue the same way,
            // so they are searched as one list.
            let mut sources = upnp.browse_content("SQ:").await?;
            sources.extend(upnp.browse_content("FV:2").await?);
            // Shortcuts are not sources: they have no resource, so they
            // can neither be enqueued nor played, and offering one only
            // produces "has nothing to play" a step later.
            sources.retain(|item| !item.shortcut);
            let item = find_content(&sources, &query)?;
            let uri = item
                .uri
                .as_deref()
                .with_context(|| format!("{:?} has nothing to play", item.title))?;

            ensure!(
                item.can_enqueue(),
                "{:?} is a station or a collection, and Sonos will only play one in \
                 place of the queue rather than adding it. \
                 Use `x2rock favorite {:?}` instead, which does that. \
                 Individual tracks can be added.",
                item.title,
                item.title
            );
            let before = upnp.queue_len().await?;
            let after = upnp.add_to_queue(uri, &item.metadata, next).await?;
            let added = after.saturating_sub(before);
            if json {
                println!(
                    "{}",
                    json!({ "room": room, "added": added, "source": item.title, "total": after })
                );
            } else {
                let tracks = if added == 1 { "track" } else { "tracks" };
                println!(
                    "{room:<24} added {added} {tracks} from {:?}, {after} in the queue",
                    item.title
                );
            }
        }
        Some(QueueAction::Save { name }) => {
            ensure!(!name.trim().is_empty(), "a playlist needs a name");
            let id = upnp.save_queue(&name).await?;
            if json {
                println!("{}", json!({ "room": room, "name": name, "id": id }));
            } else {
                println!("{room:<24} saved as {name:?} ({id})");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_one_track_or_many() {
        assert_eq!(parse_range("4").unwrap(), (4, 1));
        assert_eq!(parse_range("4-8").unwrap(), (4, 5));
        assert_eq!(parse_range(" 4 - 8 ").unwrap(), (4, 5));
        // A range of one is a range, not an error.
        assert_eq!(parse_range("6-6").unwrap(), (6, 1));
    }

    #[test]
    fn ranges_reject_what_cannot_be_a_position() {
        assert!(parse_range("8-4").is_err(), "ends before it starts");
        assert!(parse_range("0").is_err(), "tracks are numbered from 1");
        assert!(parse_range("").is_err());
        assert!(parse_range("nine").is_err());
    }

    #[test]
    fn run_bookmarks_executes_offline_without_network() {
        let dir = crate::testdir::TempDir::new("bookmarks");
        let path = dir.path().join("bookmarks.json");
        let sample = r#"{
  "schema": 1,
  "items": [
    {
      "name": "Synthetic Track",
      "object_id": "synthetic:123",
      "service_id": "284",
      "account": "1",
      "service_name": "YouTube Music",
      "artist": "Synthetic Artist",
      "art_url": "http://example.com/art.jpg",
      "kind": "track",
      "pinned": true,
      "last_played": 1789157987
    }
  ]
}"#;
        std::fs::write(&path, sample).unwrap();

        // Bookmarks manages local state and returns Ok(()) without reaching for network or players.
        let mut out = Vec::new();
        let res = run_bookmarks_at(&path, &mut out, None, None, false, true);
        assert!(res.is_ok());

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Synthetic Track"));
        assert!(!output.contains("Bodies"));
    }

    #[test]
    fn a_queue_not_in_use_lists_items_with_none_current() {
        let queue = upnp::Queue {
            update_id: "1".to_owned(),
            items: vec![upnp::QueueItem {
                index: 1,
                title: "No More Words".to_owned(),
                artist: Some("Berlin".to_owned()),
                album: None,
                duration: None,
                art_url: None,
            }],
            total: 1,
        };
        let q = queue_json(&queue, 0, false);
        let mut keys: Vec<&str> = q.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["current", "in_use", "items", "total"]);
        assert_eq!(q["in_use"], json!(false));
        assert_eq!(q["current"], json!(0));
        assert_eq!(q["items"][0]["current"], json!(false));
        assert_eq!(
            queue_json(&queue, 1, true)["items"][0]["current"],
            json!(true)
        );
    }
}
