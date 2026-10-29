//! What the household has saved and what a room has queued: favorites, saved
//! playlists, x2rock's own bookmarks, and the queue itself - plus `play-item`
//! and `queue-item`, which take a search or browse hit by id. Everything here
//! resolves a name to something the player already knows about; finding new
//! music is `services.rs`, and a bare URL is `stream.rs`.

use std::net::IpAddr;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;

use super::stream::{StreamStart, stream_item};
use super::{connect_for_service, find_named, is_refusal, mmss, refreshed_catalogue, upnp_ip};
use crate::cli::{BookmarksAction, QueueAction};
use crate::session::{self, Session, Target};
use crate::sonos::local::Connection;
use crate::sonos::proto::Favorite;
#[cfg(test)]
use crate::sonos::proto::HistoryItem;
use crate::sonos::upnp::{self, Upnp};
use crate::{bookmarks, catalogue, credentials, sonos};
#[allow(unused_imports)]
pub use x2rock::play::*;

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
    match text.split_once('-') {
        None => {
            let track: u32 = text.trim().parse()?;
            ensure!(track >= 1, "queue tracks are numbered from 1");
            Ok((track, 1))
        }
        Some((first, last)) => {
            let first: u32 = first.trim().parse()?;
            let last: u32 = last.trim().parse()?;
            ensure!(first >= 1, "queue tracks are numbered from 1");
            ensure!(last >= first, "{text}: the range ends before it starts");
            Ok((first, last - first + 1))
        }
    }
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
                    "category": f.category(),
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
    // Sectioned the way the Sonos app shows them, in its order; the order
    // within a section is the household's own.
    let mut first = true;
    for (category, heading) in FAVORITE_SECTIONS {
        let section: Vec<_> = favorites
            .iter()
            .filter(|f| f.category() == category)
            .collect();
        if section.is_empty() {
            continue;
        }
        if !first {
            println!();
        }
        first = false;
        println!("{heading}");
        for favorite in section {
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
}

/// The Sonos app's favorites sections, in its order, keyed by
/// [`Favorite::category`].
const FAVORITE_SECTIONS: [(&str, &str); 5] = [
    ("playlist", "Playlists"),
    ("song", "Songs"),
    ("album", "Albums"),
    ("station", "Stations"),
    ("other", "Other"),
];

pub(crate) fn find_favorite<'a>(favorites: &'a [Favorite], query: &str) -> Result<&'a Favorite> {
    find_named(
        favorites,
        query,
        |f| &f.id,
        |f| &f.name,
        "favorite",
        "x2rock favorites",
    )
}

/// A refusal of a quoted version, said as what it means: the list the edit was
/// worked out on is no longer the queue. Anything else passes through.
fn stale(e: anyhow::Error, at: Option<&str>) -> anyhow::Error {
    match (at, upnp::Fault::of(&e).and_then(upnp::Fault::upnp_code)) {
        (Some(at), Some("1028")) => anyhow!(
            "the queue has changed since version {at} was read, so nothing was edited; \
             read it again (`x2rock queue --json` reports `version`) and redo the edit"
        ),
        _ => e,
    }
}

/// `in_use` is the Sonos app's "Queue" versus "Queue (Not In Use)": whether the
/// group's source is its queue, which an empty queue can still be. `version` is
/// the `UpdateID` the list was read at, for `queue remove`/`move --at`.
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
    json!({
        "total": queue.total,
        "current": current,
        "in_use": in_use,
        "version": queue.update_id,
        "items": items,
    })
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
    player: &Connection,
    target: &Target,
    bookmark: &bookmarks::Bookmark,
    cdudn: &str,
) -> Result<()> {
    let upnp = Upnp::new(upnp_ip(target, player.ip()));
    enqueue_and_play(
        session,
        target,
        &upnp,
        &bookmark.uri(),
        &bookmark.didl(cdudn),
    )
    .await
}

/// `x2rock play-item`: play a hit whose id is already known.
///
/// `search --play N` re-runs the search to find the Nth result, which costs a
/// second round trip and can land on a different item if the service reorders.
/// Anything holding results already - the bar widget - should come here instead.
#[allow(clippy::too_many_arguments)]
pub async fn run_play_item(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    service: &str,
    kind: Option<&str>,
    id: &str,
    title: Option<&String>,
    from_start: bool,
) -> Result<()> {
    let (session, chosen, token) = connect_for_service(ip, household, room, service).await?;
    play_item(
        &session,
        room,
        &chosen,
        token.as_ref(),
        kind,
        id,
        title.map(String::as_str).unwrap_or(id),
        from_start,
    )
    .await
}

/// `queue-item`: the same lookup `run_play_item` does, then enqueue without
/// playing - at the end, or with `next` after what is playing now.
#[allow(clippy::too_many_arguments)]
pub async fn run_queue_item(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    service: &str,
    kind: Option<&str>,
    id: &str,
    title: Option<&String>,
    next: bool,
) -> Result<()> {
    // Nothing here talks to the service. The token is wanted only for which
    // account it names, which the enqueue has to name to the player too.
    let (session, chosen, token) = connect_for_service(ip, household, room, service).await?;
    let title = title.map(String::as_str).unwrap_or(id);
    let how = if next { Enqueue::Next } else { Enqueue::End };
    queue_item(
        &session,
        room,
        &chosen,
        token.as_ref(),
        kind,
        id,
        title,
        how,
    )
    .await
}

/// Add one hit to the queue without playing it: `queue-item`, and `search` or
/// `browse` with `--queue`/`--next`. `how` is [`Enqueue::End`] or
/// [`Enqueue::Next`].
#[allow(clippy::too_many_arguments)]
pub async fn queue_item(
    session: &session::Session,
    room: Option<&str>,
    chosen: &sonos::smapi::Service,
    token: Option<&sonos::smapi::Token>,
    kind: Option<&str>,
    id: &str,
    title: &str,
    how: Enqueue,
) -> Result<()> {
    // Refused rather than half-worked. `play-item` answers a stream by streaming
    // it, which is a different thing from queueing and cannot be what someone
    // pressing "add to queue" meant.
    if kind.is_some_and(|k| k.eq_ignore_ascii_case("stream")) {
        bail!(
            "{title:?} is a live stream, which has no queue form. \
             Play it with `x2rock play-item` instead."
        );
    }
    // A station is played as the room's source, never queued: the player
    // refuses one with 800 or 804, neither of which says that.
    if kind.is_some_and(sonos::smapi::is_program) {
        bail!(
            "{title:?} is a station, which plays as the room's source and has no \
             queue form. Play it with `x2rock play-item` instead."
        );
    }
    // A place rather than a thing. The player refuses it with a bare UPnP 804,
    // which says the URI was malformed and not that an artist has no tracks of
    // its own to add - so this says the second, where every other refusal here
    // explains itself.
    if kind.is_some_and(bookmarks::container_of_containers) {
        bail!(
            "{title:?} is {} {}, which holds albums and playlists rather than \
             tracks. Open it with `x2rock browse` and queue what is inside.",
            super::article(kind.unwrap_or_default()),
            kind.unwrap_or_default()
        );
    }
    // Without a service type there is no cdudn, and `SA_RINCONNone` is not an
    // account - the enqueue would be refused by the player with less to say.
    let Some(naming) = Naming::of(chosen, token) else {
        bail!(
            "{} is not in the player's service-type list, so nothing can be \
             built to name the account that owns {title:?}.",
            chosen.name
        );
    };
    enqueue_item(session, room, chosen, &naming, id, title, kind, how).await
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

/// `x2rock recent`: what the household played lately, newest first.
pub async fn recent(session: &Session, query: Option<&str>, json: bool) -> Result<()> {
    let household = session.connection.household_id().await?;
    let history = session.connection.history(&household).await?;
    let linked = credentials::Credentials::load()?;
    let catalogue = history_catalogue(session).await;
    let needle = query.map(str::to_lowercase);
    let rows: Vec<_> = history
        .resources
        .iter()
        .filter(|i| {
            needle
                .as_deref()
                .is_none_or(|n| i.name.to_lowercase().contains(n))
        })
        .map(|i| replayable(i, &household, &linked, &catalogue))
        .collect();
    if json {
        // The favorites shape, so the widget can list the two alike; `id` is
        // the object id, which is what `replay` takes back.
        let items: Vec<_> = rows
            .iter()
            .map(|r| {
                json!({
                    "id": r.item.id.object_id,
                    "name": r.item.name,
                    "type": r.item.kind,
                    "service": r.service,
                    "service_id": r.item.id.service_id,
                    // The favorites field for an artist; history carries none,
                    // and the service already has its own key.
                    "description": null,
                    "art_url": r.item.art_url(),
                    "playable": r.playable,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&items)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("Nothing played lately.");
        return Ok(());
    }
    for r in &rows {
        let service = r.service.as_deref().unwrap_or("unknown service");
        let mark = if r.playable { "" } else { "  (not held here)" };
        println!("{:<44} {:<9} {service}{mark}", r.item.name, r.item.kind);
    }
    Ok(())
}

/// `x2rock replay`: play something from the household's recently played, by
/// name or by the id `recent --json` gives.
///
/// **By `play_item`'s routes, not by `loadContent`.** A history row names its
/// service, account and object id, which is exactly what `loadContent` takes,
/// so that is what this sent - and `loadContent` on a Mixcloud item hangs the
/// coordinator until it restarts, on either of the household's accounts
/// (x2rocktv P-21, 2026-10-11), where the Sonos app plays the same mix by
/// queueing it. `play_item` queues what the queue takes, starts a station as
/// the room's source, streams a stream and resumes a book, and keeps
/// `loadContent` as its last resort. Only a Sonos playlist, which no service
/// serves, still loads directly.
pub async fn replay(
    session: &Session,
    player: &Connection,
    target: &Target,
    room: Option<&str>,
    query: &str,
) -> Result<()> {
    let household = session.connection.household_id().await?;
    let history = session.connection.history(&household).await?;
    let item = find_named(
        &history.resources,
        query,
        |i| &i.id.object_id,
        |i| &i.name,
        "recently played item",
        "x2rock recent",
    )?;
    replay_item(session, player, target, room, item).await
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
        .refresh_and_save(&Upnp::new(session.connection.ip()), false)
        .await;
    bookmark.service_name = catalogue.name_of(&bookmark.service_id).map(str::to_string);

    let replaced = bookmarks::Bookmarks::update(|list| Ok(list.keep(bookmark)))?;
    println!("{} {title}", if replaced { "Updated" } else { "Kept" });
    Ok(())
}

/// A kept bookmark by name, with what playing it needs: its service as the
/// player lists it, the token a stream fallback would use, and the cdudn that
/// names its account. Shared by `bookmark` and scenes.
async fn resolve_bookmark(
    session: &Session,
    query: &str,
) -> Result<(
    bookmarks::Bookmark,
    sonos::smapi::Service,
    Option<sonos::smapi::Token>,
    String,
)> {
    let list = bookmarks::Bookmarks::load()?;
    let bookmark = list.find(query)?.clone();

    // The cdudn is derived from the service type list rather than copied from
    // anything - see `Service::cdudn` - and names the account alongside the
    // URI's `sn=`, which is what actually picks it (see `Naming`).
    let catalogue = refreshed_catalogue(session).await?;
    // Two different failures, worth telling apart: a service the player
    // has never heard of, and one it lists but gives no type for.
    let service = catalogue
        .by_id(&bookmark.service_id)
        .ok_or_else(|| {
            anyhow!(
                "service {} is not in this player's service list, so {:?} cannot be played",
                bookmark.service_id,
                bookmark.name
            )
        })?
        .clone();
    let (token, cdudn) = account_naming(session, &service, &bookmark).await?;
    Ok((bookmark, service, token, cdudn))
}

/// Start a kept bookmark on `target` by the queue route alone, printing
/// nothing, and answer with its name. For scenes: `bookmark` would fall back
/// to a direct stream on a refusal, which prints its own lines and so cannot
/// sit under a `--json` report - here a refusal is the answer.
pub async fn start_bookmark(
    session: &Session,
    player: &Connection,
    target: &Target,
    query: &str,
) -> Result<String> {
    let (bookmark, _, _, cdudn) = resolve_bookmark(session, query).await?;
    play_bookmark(session, player, target, &bookmark, &cdudn)
        .await
        .map_err(|e| {
            if is_refusal(&e) {
                anyhow!(
                    "it would not go in the queue ({e:#}); a scene can play it only as a \
                     Sonos favorite - save it as one in the Sonos app"
                )
            } else {
                e
            }
        })?;
    Ok(bookmark.name)
}

/// `x2rock bookmark`: play something kept earlier on `target`, or with `next`
/// queue it after the current track.
pub async fn bookmark(
    session: &Session,
    player: &Connection,
    target: &Target,
    query: &str,
    next: bool,
) -> Result<()> {
    let (bookmark, service, token, cdudn) = resolve_bookmark(session, query).await?;

    if next {
        // Queuing for later, not playing now - streaming would start it
        // immediately and break what `--next` promised, so there is no
        // fallback here: a refusal is just a refusal.
        let upnp = Upnp::new(upnp_ip(target, player.ip()));
        upnp.add_to_queue(&bookmark.uri(), &bookmark.didl(&cdudn), true)
            .await?;
        println!("{:<24} {}", target.name, bookmark.name);
    } else {
        match play_bookmark(session, player, target, &bookmark, &cdudn).await {
            Ok(()) => println!("{:<24} {}", target.name, bookmark.name),
            // The same rule `play_item` follows for a fresh search/browse
            // hit: a refusal means this is not queue material, most often
            // a Sonos Radio-style program with no discrete track to hold
            // a queue position, and the fix is the stream session, not a
            // retry.
            Err(e) if !is_refusal(&e) => return Err(e),
            Err(e) => {
                eprintln!(
                    "x2rock: {:?} {} ({e:#}); streaming it",
                    bookmark.name,
                    refusal_was(&e)
                );
                stream_item(
                    session,
                    target,
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

/// `x2rock favorite`: play a household favorite on `target`, by name or id.
pub async fn favorite(
    session: &Session,
    player: &Connection,
    target: &Target,
    query: &str,
) -> Result<()> {
    let household = session.connection.household_id().await?;
    let favorites = session.connection.favorites(&household).await?;
    let favorite = find_favorite(&favorites.items, query)?;
    // Household-scoped to find, group-scoped to play.
    player.load_favorite(&target.group_id, &favorite.id).await?;
    println!("{:<24} {}", target.name, favorite.name);
    Ok(())
}

/// `x2rock favorite add`: save what `target` is playing as a Sonos favorite.
pub async fn favorite_add(
    session: &Session,
    player: &Connection,
    target: &Target,
    container: bool,
    json: bool,
) -> Result<()> {
    let outcome = save_playing_favorite(session, player, target, container).await?;
    if json {
        println!(
            "{}",
            json!({
                "name": outcome.name,
                "service": outcome.service,
                "kind": outcome.kind,
                "id": outcome.id,
                "saved": outcome.saved,
            })
        );
    } else if outcome.saved {
        println!("Saved {} to Sonos favorites", outcome.name);
    } else {
        println!("{} is already a Sonos favorite", outcome.name);
    }
    Ok(())
}

/// `x2rock favorite remove`: take a Sonos favorite out, by name or id, or with
/// no name whatever `playing` says is on. Every favorite that is the playing
/// item goes, since a household can hold the same song under two names.
pub async fn favorite_remove(
    upnp: &Upnp,
    query: Option<&str>,
    playing: Option<(&Connection, &str, bool)>,
    json: bool,
) -> Result<()> {
    let favorites = upnp.browse_content("FV:2").await?;
    let (named, gone) = match (query, playing) {
        (Some(query), _) => {
            let found = find_named(
                &favorites,
                query,
                |f| f.id.trim_start_matches("FV:2/"),
                |f| &f.title,
                "favorite",
                "x2rock favorites",
            )?;
            (found.title.clone(), vec![found])
        }
        (None, Some((player, group, container))) => {
            let item = playing_item(player, group, container).await?;
            let uri = match item.kind.as_deref() {
                Some("track") => Some(item.uri()),
                _ => None,
            };
            let found = favorites_of(&favorites, &item, uri.as_deref());
            ensure!(!found.is_empty(), "{} is not a Sonos favorite", item.name);
            (item.name, found)
        }
        (None, None) => bail!("name a favorite to remove, or play it and give no name"),
    };
    let mut removed = Vec::new();
    for favorite in gone {
        upnp.destroy_favorite(&favorite.id).await?;
        removed.push(favorite.title.clone());
    }
    if json {
        println!("{}", json!({ "name": named, "removed": removed }));
    } else {
        for title in &removed {
            println!("Removed {title} from Sonos favorites");
        }
    }
    Ok(())
}

/// `x2rock playlist`: replace the queue with a saved playlist and play it.
pub async fn playlist(
    session: &Session,
    player: &Connection,
    target: &Target,
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
    player.load_playlist(&target.group_id, &playlist.id).await?;
    println!("{:<24} {}", target.name, playlist.name);
    Ok(())
}

/// Everything that can go in a queue by name: the saved playlists (`SQ:`) and
/// the favorites (`FV:2`), searched as one list because both enqueue the same
/// way. Shortcuts are dropped - they have no resource, so they cannot be
/// enqueued from here, and offering one only produces "has nothing to play" a
/// step later. That also drops album, artist and song favorites the Sonos app
/// saved as shortcuts, which would need the service path. The two browses are
/// independent, so they run together.
pub async fn queue_sources(upnp: &Upnp) -> Result<Vec<upnp::BrowseItem>> {
    let (mut sources, favorites) =
        tokio::try_join!(upnp.browse_content("SQ:"), upnp.browse_content("FV:2"))?;
    sources.extend(favorites);
    sources.retain(|item| !item.shortcut);
    Ok(sources)
}

/// `x2rock queue`: list, edit, save or append to `target`'s queue. Every
/// change reports what the queue became rather than what was asked.
pub async fn queue(
    player: &Connection,
    target: &Target,
    action: Option<QueueAction>,
    json: bool,
) -> Result<()> {
    let upnp = Upnp::new(upnp_ip(target, player.ip()));
    let room = target.name.as_str();
    match action {
        None => {
            let (queue, in_use) = tokio::try_join!(upnp.queue(), upnp.playing_from_queue())?;
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
        Some(QueueAction::Remove { range, at }) => {
            let (start, count) = parse_range(&range)?;
            let at = at.as_deref();
            let removed = match count {
                1 => upnp.remove_track(start, at).await,
                _ => upnp.remove_range(start, count, at).await,
            };
            removed.map_err(|e| stale(e, at))?;
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
        Some(QueueAction::Move { from, to, at }) => {
            ensure!(from >= 1 && to >= 1, "queue tracks are numbered from 1");
            let at = at.as_deref();
            upnp.move_track(from, to, at)
                .await
                .map_err(|e| stale(e, at))?;
            if json {
                println!("{}", json!({ "room": room, "from": from, "to": to }));
            } else {
                println!("{room:<24} moved track {from} to {to}");
            }
        }
        Some(QueueAction::Sources { query }) => {
            let mut sources = queue_sources(&upnp).await?;
            if let Some(query) = &query {
                let needle = query.to_lowercase();
                sources.retain(|i| i.title.to_lowercase().contains(&needle));
            }
            print_sources(&sources, json);
        }
        Some(QueueAction::Add { query, next }) => {
            let sources = queue_sources(&upnp).await?;
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
            let after = upnp.add_to_queue(uri, &item.metadata, next).await?.length;
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

    /// A dead account is swapped for the live preferred one; an account the
    /// household still holds is kept even when it is not the preferred one; a
    /// service with no account here is not offered; a Sonos playlist needs none.
    #[test]
    fn an_item_plays_from_its_own_account_while_it_lives_and_the_preferred_one_after() {
        const HH: &str = "Sonos_test";
        let mut linked = credentials::Credentials::default();
        let account = |serial: u32| -> credentials::Account {
            serde_json::from_value(json!({
                "service_name": "iHeartRadio", "auth_token": format!("t{serial}"),
                "private_key": "k", "linked": 0, "serial": serial, "household": HH,
                "account_id": format!("sn_{serial}"),
            }))
            .unwrap()
        };
        linked.remember(HH, "6", account(24));
        let preferred = linked.remember(HH, "6", account(25));
        linked.prefer(HH, "6", &preferred).unwrap();
        let catalogue = catalogue::Catalogue::default();

        let item = |service: Option<&str>, account: Option<&str>| -> HistoryItem {
            serde_json::from_value(json!({
                "name": "x", "type": "program", "images": [],
                "id": {"serviceId": service, "accountId": account, "objectId": "o"},
            }))
            .unwrap()
        };
        let account_of = |i: &HistoryItem| {
            let r = replayable(i, HH, &linked, &catalogue);
            (r.playable, r.id.account_id)
        };

        assert_eq!(
            account_of(&item(Some("6"), Some("sn_24"))),
            (true, Some("sn_24".into())),
            "still held, so kept: a custom station is its listener's"
        );
        assert_eq!(
            account_of(&item(Some("6"), Some("sn_15"))),
            (true, Some("sn_25".into())),
            "gone, so the preferred account plays it"
        );
        assert!(
            !account_of(&item(Some("12"), Some("sn_22"))).0,
            "not held here"
        );
        assert!(!account_of(&item(None, None)).0, "no service at all");
        assert_eq!(
            account_of(&item(Some(SONOS_PLAYLISTS), None)),
            (true, None),
            "a Sonos playlist names no account and needs none"
        );
    }

    #[test]
    fn an_enqueue_names_one_account_by_selector_and_serial_together() {
        let service = sonos::smapi::Service {
            id: "6".into(),
            name: "iHeartRadio".into(),
            uri: String::new(),
            auth: sonos::smapi::Auth::DeviceLink,
            manifest_uri: None,
            service_type: Some(1543),
        };
        let mut account: credentials::Account = serde_json::from_value(json!({
            "service_name": "iHeartRadio", "auth_token": "t", "private_key": "k", "linked": 0,
        }))
        .unwrap();
        // The token is what every play path holds, so that is what is named.
        let named = |a: &credentials::Account| Naming::of(&service, Some(&a.token("k")));
        // No account, or one with nothing to say for itself: the `-0-` form.
        let bare = Naming {
            cdudn: "SA_RINCON1543_X_#Svc1543-0-Token".into(),
            serial: None,
        };
        assert_eq!(Naming::of(&service, None).as_ref(), Some(&bare));
        assert_eq!(named(&account), Some(bare));
        // A browser link `match` answered: the serial lives in `account_id`.
        account.account_id = Some("sn_26".into());
        assert_eq!(named(&account).unwrap().serial.as_deref(), Some("26"));
        // Imported before selectors were stored: the serial still goes, since
        // `sn=` is what picks the account; the cdudn has no selector to offer.
        account.serial = Some(25);
        assert_eq!(
            named(&account),
            Some(Naming {
                cdudn: "SA_RINCON1543_X_#Svc1543-0-Token".into(),
                serial: Some("25".into()),
            })
        );
        // Imported with its selector: both halves name the same account.
        account.account_key = Some("885ebbcc".into());
        assert_eq!(
            named(&account),
            Some(Naming {
                cdudn: "SA_RINCON1543_X_#Svc1543-885ebbcc-Token".into(),
                serial: Some("25".into()),
            })
        );
        // No type in the player's list: nothing can be built.
        let untyped = sonos::smapi::Service {
            service_type: None,
            ..service
        };
        assert_eq!(Naming::of(&untyped, Some(&account.token("k"))), None);
    }

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
        assert!(parse_range("0-8").is_err(), "tracks are numbered from 1");
        assert!(parse_range("0-4294967295").is_err());
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
        assert_eq!(keys, ["current", "in_use", "items", "total", "version"]);
        assert_eq!(q["version"], json!("1"));
        assert_eq!(q["in_use"], json!(false));
        assert_eq!(q["current"], json!(0));
        assert_eq!(q["items"][0]["current"], json!(false));
        assert_eq!(
            queue_json(&queue, 1, true)["items"][0]["current"],
            json!(true)
        );
    }
}
