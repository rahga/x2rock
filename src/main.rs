mod bookmarks;
mod catalogue;
mod credentials;
mod daemon;
mod discover;
mod mpris;
mod netid;
mod restart;
mod session;
mod sonos;
mod state;

use std::net::IpAddr;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;

use sonos::local::Connection;
use sonos::proto::{Favorite, Group, Groups, MetadataStatus, PlaybackStatus, Player, Repeat};
use sonos::upnp::{self, Upnp};
use state::State;

#[derive(Parser)]
#[command(name = "x2rock", version, about = "Local-first Sonos control")]
struct Cli {
    /// Room to control. Not needed when the household has a single group.
    #[arg(long, short = 'r', global = true, env = "X2ROCK_ROOM")]
    room: Option<String>,

    /// Address of a player, bypassing what is remembered for this network.
    #[arg(long, short = 'i', global = true, env = "X2ROCK_PLAYER")]
    ip: Option<IpAddr>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List rooms and their playback state.
    Rooms {
        #[arg(long)]
        json: bool,
    },
    /// Show what is playing.
    Now {
        #[arg(long)]
        json: bool,
    },
    /// Resume playback, or play track N from the queue.
    Play {
        track: Option<u32>,
    },
    Pause,
    /// Play if paused, pause if playing.
    Toggle,
    Next,
    Prev,
    /// Show or change volume: a level (0-100), a change (+5, -5), or mute/unmute.
    Vol {
        #[arg(allow_negative_numbers = true)]
        change: Option<String>,
        /// This room's own speaker rather than the group it plays with. Only
        /// differs while it is grouped, where the group volume moves every room
        /// together and this is the balance between them.
        #[arg(long)]
        player: bool,
    },
    /// Show or set repeat: off, all (the queue) or one (the current track).
    Repeat {
        mode: Option<String>,
    },
    /// Show or set shuffle: on or off.
    Shuffle {
        mode: Option<String>,
    },
    /// List the queue, or change it.
    Queue {
        #[command(subcommand)]
        action: Option<QueueAction>,
        #[arg(long)]
        json: bool,
    },
    /// List saved favorites, or only those whose name matches a query.
    Favorites {
        query: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Play a favorite, by name or id. The one way to start a room that has
    /// nothing queued, which `play` cannot do.
    Favorite {
        query: String,
    },
    /// Search a music service. Only services with anonymous access, which is
    /// most of the radio ones; the rest need a linked account x2rock cannot
    /// supply. `--service` with no term lists what can be searched.
    Search {
        term: Option<String>,
        /// Service to search, by name. Case-insensitive, and a prefix will do.
        #[arg(long, short = 's')]
        service: Option<String>,
        /// Category within the service, by its own name (`stations`, `tracks`).
        /// Defaults to `all` where the service offers it, else the first.
        #[arg(long, short = 'c')]
        category: Option<String>,
        #[arg(long, default_value_t = 20)]
        count: u32,
        /// Play the Nth result, 1-based, in --room. Opens a playback session
        /// rather than enqueuing: a service's content cannot be added to the
        /// Sonos queue, and Sonos does not intend it to be.
        #[arg(long, value_name = "N")]
        play: Option<usize>,
        /// Re-read the service catalogue even if its version has not moved.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Walk a music service's own containers: a personal library, a "For You",
    /// a genre tree - the parts of a service no search term can name.
    ///
    /// The other half of `search`. A search takes a word; this takes a place.
    Browse {
        /// Service to browse, by name. Omit to list the ones that can be.
        #[arg(long, short = 's')]
        service: Option<String>,
        /// The container to open. Defaults to `root`, where every service starts.
        container: Option<String>,
        #[arg(long, default_value_t = 50)]
        count: u32,
        /// Play the Nth row, 1-based, in --room. Refused for a container, which
        /// is something to open rather than something to play.
        #[arg(long, value_name = "N")]
        play: Option<usize>,
        /// Re-read the service catalogue even if its version has not moved.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Play one search result by its id, without searching again. What the bar
    /// widget uses once it already has results in hand.
    PlayItem {
        #[arg(long, short = 's')]
        service: String,
        id: String,
        /// What the room should display. Defaults to the id, which is what a
        /// service shows when given nothing better.
        #[arg(long)]
        title: Option<String>,
        /// The item's own kind, as `search`/`browse --json` report it in `type`.
        /// `stream` plays as a stream; anything else goes in the queue, which is
        /// the only way on-demand service content plays. Omitted, the queue is
        /// tried first and a refusal falls back to streaming.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Link a music service account, so its catalogue can be searched.
    ///
    /// Opens the service's own login page in whatever browser is already
    /// configured, waits for it to be finished, and stores the token it mints.
    /// No Sonos account, no partner registration, no embedded browser.
    Link {
        /// Which service, by name. Omit to list the ones that can be linked.
        service: Option<String>,
        /// Print the URL instead of opening it. What to use over ssh.
        #[arg(long)]
        no_open: bool,
        /// What the household should call the account. Defaults to the hostname,
        /// so a household with several machines can tell them apart.
        #[arg(long)]
        nickname: Option<String>,
        /// Store the token without registering the account on the household.
        /// Search works either way; only playback needs the household to know.
        #[arg(long)]
        no_match: bool,
    },
    /// Forget a linked account's stored token.
    ///
    /// Local only: it does not revoke anything at the service, which is done
    /// from that service's own account page.
    Unlink {
        service: String,
    },
    /// List the accounts this machine holds a token for.
    Accounts {
        #[arg(long)]
        json: bool,
    },
    /// Remember what is playing, so it can be started again later.
    ///
    /// The answer to a service x2rock cannot search: play it once from the
    /// Sonos app, keep it, and it is on the bar from then on.
    Keep {
        /// What to call it. Defaults to what the player calls it.
        name: Option<String>,
        /// Keep the album, playlist or station rather than the single track.
        #[arg(long)]
        container: bool,
    },
    /// List what has been kept. The daemon also notes what plays, and `--all`
    /// includes that history.
    Bookmarks {
        #[command(subcommand)]
        action: Option<BookmarksAction>,
        query: Option<String>,
        /// Include what the daemon noticed, not just what was kept on purpose.
        #[arg(long, short = 'a')]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Play something kept earlier, by name.
    Bookmark {
        query: String,
        /// Queue it after the current track instead of replacing what plays.
        #[arg(long)]
        next: bool,
    },
    /// Switch a soundbar to its TV input.
    Tv,
    /// Group rooms into --room's group, so they play what it plays.
    Group {
        #[arg(required = true)]
        rooms: Vec<String>,
    },
    /// Take a room out of its group, leaving it playing on its own.
    Ungroup {
        room: String,
    },
    /// Party mode: every room joins --room's group. `party off` breaks it up
    /// and leaves each room on its own.
    Party {
        mode: Option<String>,
    },
    /// Send one Control API command and print what comes back. A probe, not a
    /// feature: the API is wider than this CLI covers, and settling what a
    /// namespace actually answers should not need a rebuild. A refusal is a
    /// result here, so a player-side error prints and still exits 0.
    ///
    /// Every command is addressed to something, and which key it wants is a
    /// property of the namespace: see --scope, which is the flag most probes
    /// get wrong on the first try.
    #[command(after_long_help = RAW_EXAMPLES)]
    Raw {
        /// Namespace, e.g. `musicService:1`.
        namespace: String,
        /// Command within it, e.g. `getSessions`.
        command: String,
        /// The command's parameters, as one JSON object. Defaults to `{}`.
        ///
        /// These go in the message body. The target key does not - it belongs
        /// in the header, so passing `{"groupId": "..."}` here does nothing
        /// and the player still answers "Missing groupId". Use --scope.
        #[arg(value_name = "PARAMS")]
        options: Option<String>,
        /// What the command is addressed to. Per-namespace, and the player
        /// will not infer it: `ERROR_MISSING_PARAMETERS - Missing groupId`
        /// (or playerId, or householdId) means this flag is wrong, not the
        /// command. Verified against real players:
        ///
        /// group - playback:1, playbackMetadata:1, groupVolume:1
        ///
        /// player - playerVolume:1, homeTheater:1, audioClip:1
        ///
        /// household - groups:1, favorites:1, playlists:1,
        /// musicServiceAccounts:1
        ///
        /// Household is the default because the namespaces left to explore
        /// are mostly household-scoped. `group` and `player` resolve through
        /// --room and connect to the right player themselves, so --ip is
        /// never needed to reach one.
        #[arg(long, value_enum, default_value_t = RawScope::Household)]
        scope: RawScope,
        /// After the command, keep the socket open this many seconds and print
        /// every event that arrives. How `subscribe` is read: the reply to a
        /// subscribe is empty, and the state it asked for turns up afterwards
        /// as an event.
        #[arg(long, value_name = "SECONDS")]
        watch: Option<u64>,
        /// Address the command to a playback session. `playbackSession:1`
        /// commands after `createSession` are keyed by the session it returned,
        /// which is not a target `--scope` can derive from the household.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Scan the local network for players and remember them.
    Discover,
    /// Publish every room as an MPRIS2 media player, until stopped.
    Daemon,
}

/// Worked examples for `raw --help`. Every one of these was run against a real
/// player, so a reader copying one is copying something that answered.
const RAW_EXAMPLES: &str = "\
Examples:
  # What a soundbar is receiving over HDMI (group-scoped).
  x2rock -r 'Living Room' raw --scope group playbackMetadata:1 getMetadataStatus

  # One player's own volume, not its group's (player-scoped).
  x2rock -r 'Living Room' raw --scope player playerVolume:1 getVolume

  # Household state needs no --room.
  x2rock raw favorites:1 getFavorites

  # A subscribe replies empty and the state arrives after, so watch for it.
  x2rock raw --watch 5 musicServiceAccounts:1 subscribe

  # Parameters are one JSON object, in the body.
  x2rock -r Kitchen raw --scope group playback:1 seek '{\"positionMillis\": 30000}'
";

/// Which target key a raw command carries, which is per-namespace and is half
/// of what a probe is trying to find out.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RawScope {
    /// `householdId`. Household-wide state: groups, favorites, playlists,
    /// music service accounts.
    Household,
    /// `groupId` for --room's group, sent to that group's coordinator, which
    /// is the only player that answers for it.
    Group,
    /// `playerId` for --room's own player, sent to that player. A player
    /// answers player-scoped commands only for itself.
    Player,
    /// No target key at all. Some commands take none, and an unaddressed
    /// command is also the cheapest way to see a namespace reject the shape
    /// rather than the address.
    None,
}

/// The one thing `bookmarks` does besides list.
///
/// A subcommand rather than a top-level `forget`, to sit beside `queue remove`:
/// both take something out of a list the same command prints. The cost is that
/// a bookmark actually named "remove" can no longer be queried by name, which
/// `queue` has always accepted for the same reason.
#[derive(Subcommand)]
enum BookmarksAction {
    /// Forget one, by name. Matches the history too, not just what was kept.
    Remove { query: String },
}

#[derive(Subcommand)]
enum QueueAction {
    /// Remove one track, or an inclusive range like 4-8.
    Remove { range: String },
    /// Remove every track. Sonos keeps no undo for this.
    Clear {
        /// Confirm: clearing a queue cannot be undone.
        #[arg(long)]
        yes: bool,
    },
    /// Move a track to another position.
    Move { from: u32, to: u32 },
    /// Save the queue as a Sonos playlist.
    Save { name: String },
    /// List what `queue add` can draw on: saved playlists and favorites.
    Sources {
        query: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Add a saved playlist or favorite to the queue, by name or id.
    Add {
        query: String,
        /// Put it next rather than at the end.
        #[arg(long)]
        next: bool,
    },
}

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
fn find_content<'a>(items: &'a [upnp::BrowseItem], query: &str) -> Result<&'a upnp::BrowseItem> {
    find_named(
        items,
        query,
        |i| &i.id,
        |i| &i.title,
        "source",
        "x2rock queue sources",
    )
}

/// An exact id wins, then a case-insensitive substring of the name; among
/// several of those, a whole-name match settles it, and anything else is
/// ambiguous and says so - naming `hint` as the command that lists them.
fn find_named<'a, T>(
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
            if let Some(whole) = several.iter().find(|i| name(i).to_lowercase() == needle) {
                return Ok(whole);
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

enum VolumeChange {
    Set(u8),
    Adjust(i8),
    Mute(bool),
}

fn parse_volume(text: &str) -> Result<VolumeChange> {
    match text {
        "mute" => Ok(VolumeChange::Mute(true)),
        "unmute" => Ok(VolumeChange::Mute(false)),
        _ if text.starts_with(['+', '-']) => {
            let delta: i16 = text.parse()?;
            ensure!(
                (-100..=100).contains(&delta),
                "volume change must be within ±100"
            );
            Ok(VolumeChange::Adjust(delta as i8))
        }
        _ => {
            let level: u8 = text.parse()?;
            ensure!(level <= 100, "volume must be 0-100");
            Ok(VolumeChange::Set(level))
        }
    }
}

fn now_line(status: &PlaybackStatus, meta: &MetadataStatus) -> String {
    let track = meta.current_item.as_ref().and_then(|i| i.track.as_ref());
    let title = track
        .and_then(|t| t.name.as_deref())
        .or_else(|| meta.container.as_ref().and_then(|c| c.name.as_deref()));
    let artist = track
        .and_then(|t| t.artist.as_ref())
        .and_then(|a| a.name.as_deref());
    let album = track
        .and_then(|t| t.album.as_ref())
        .and_then(|a| a.name.as_deref());

    let mut line = status.state().to_string();
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
    // On a soundbar this is the whole point of looking: a source that has
    // quietly dropped to stereo says so here and nowhere else.
    if let Some(format) = meta
        .container
        .as_ref()
        .and_then(|c| c.ht_input_format.as_ref())
    {
        line.push_str(&format!("  [{}]", format.summary()));
    }
    let repeat = status.play_modes.repeat();
    let mut flags = Vec::new();
    if status.play_modes.shuffle {
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

fn now_json(room: &str, status: &PlaybackStatus, meta: &MetadataStatus) -> serde_json::Value {
    let track = meta.current_item.as_ref().and_then(|i| i.track.as_ref());
    let container = meta.container.as_ref();
    json!({
        "room": room,
        "state": status.state(),
        "title": track.and_then(|t| t.name.as_deref()).or(container.and_then(|c| c.name.as_deref())),
        "artist": track.and_then(|t| t.artist.as_ref()).and_then(|a| a.name.as_deref()),
        "album": track.and_then(|t| t.album.as_ref()).and_then(|a| a.name.as_deref()),
        "service": container.and_then(|c| c.service.as_ref()).and_then(|s| s.name.as_deref()),
        "position_ms": status.position_millis,
        "duration_ms": track.and_then(|t| t.duration_millis),
        "repeat": status.play_modes.repeat().as_str(),
        "shuffle": status.play_modes.shuffle,
        "input_format": meta.container.as_ref().and_then(|c| c.ht_input_format.as_ref()).map(|f| f.summary()),
        "surround": meta.container.as_ref().and_then(|c| c.ht_input_format.as_ref()).map(|f| f.is_surround()),
        "art_url": track.and_then(|t| t.image_url.as_deref()).or(container.and_then(|c| c.image_url.as_deref())),
    })
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

/// The favorite a query names: its id exactly, else a case-insensitive match on
/// the name. Several matches are reported rather than guessed between, except
/// where one of them is the whole name - "Bedtime" should not be ambiguous just
/// because "Bedtime P5 Mix" also exists.
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

/// "old → " when a command changed something, nothing when it only reported.
fn transition(before: &str, after: &str) -> String {
    if before == after {
        String::new()
    } else {
        format!("{before} → ")
    }
}

fn mmss(duration: Option<std::time::Duration>) -> String {
    match duration {
        Some(d) => format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60),
        None => String::new(),
    }
}

fn print_queue(queue: &upnp::Queue, current: u32, json: bool) {
    if json {
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
        println!(
            "{}",
            json!({ "total": queue.total, "current": current, "items": items })
        );
        return;
    }
    if queue.items.is_empty() {
        println!("Queue is empty.");
        return;
    }
    for item in &queue.items {
        let marker = if item.index == current { "▶" } else { " " };
        let mut line = format!("{marker} {:>3}  {}", item.index, item.title);
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

async fn discover_and_remember(state: &mut State) -> Result<()> {
    let network = discover::local_network()?;
    eprintln!("Scanning {}/{} ...", network.ip, network.prefix_len());
    // Sweep it all: the point of stopping early was to avoid opening a session
    // per responder, not to stop looking. Stopping at the first hit made a
    // player that answers on 1400 but will not complete a WebSocket - mid
    // reboot, host firewall - the end of the whole command.
    let scan = discover::scan_local_subnet(false).await?;
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

    let fingerprint = netid::network_fingerprint();
    if fingerprint.is_none() {
        eprintln!("Could not identify this network; results will not be remembered.");
    }
    // Reaching any one player is enough: getGroups reports every other player's
    // address. So try them in turn and stop at the first that actually talks,
    // rather than printing the same household once per responder.
    let mut session = None;
    for ip in &scan.found {
        match session::attach(IpAddr::V4(*ip), state, fingerprint.as_deref()).await {
            Ok(reached) => {
                session = Some(reached);
                break;
            }
            Err(e) => eprintln!("{ip}: {e:#}"),
        }
    }
    let Some(session) = session else {
        bail!(
            "found {} player(s) but none would talk; see the errors above",
            scan.found.len()
        );
    };

    let mut players: Vec<_> = session.groups.players.iter().collect();
    players.sort_by(|a, b| a.name.cmp(&b.name));
    for player in players {
        match player.ip() {
            Some(ip) => println!("{ip}  {}", player.name),
            None => println!("(no address)  {}", player.name),
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
async fn play_item(
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
    if let (false, Some(cdudn)) = (streamish, service.cdudn()) {
        match enqueue_item(session, room, service, &cdudn, id, title).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("x2rock: {title:?} would not go in the queue ({e:#}); streaming it")
            }
        }
    }
    stream_item(session, room, service, token, id, title).await
}

/// Put a service item in the room's queue and jump to it.
///
/// Deliberately the same sequence `bookmark` uses, down to making the queue the
/// current source first: after a station it is not, and `Seek` fails with 701.
async fn enqueue_item(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    cdudn: &str,
    id: &str,
    title: &str,
) -> Result<()> {
    let target = session::target(&session.groups, room)?;
    let upnp = Upnp::new(
        target
            .coordinator_ip
            .unwrap_or_else(|| session.connection.ip()),
    );
    // No `sn=`: nothing here has ever played, so there is no serial to harvest,
    // and the player does not need one. See `bookmarks::service_uri`.
    let uri = bookmarks::service_uri(id, &service.id, None);
    let length = upnp
        .add_to_queue(&uri, &bookmarks::service_didl(id, title, cdudn), false)
        .await?;
    if !upnp.playing_from_queue().await? {
        upnp.use_queue(&target.coordinator_id).await?;
    }
    upnp.seek_track(length).await?;
    let coordinator = session::coordinator(session, &target).await?;
    coordinator.playback(&target.group_id, "play").await?;
    println!("{} — {title} on {}", target.name, service.name);
    Ok(())
}

/// Play a service item as a stream, alongside the queue rather than in it.
async fn stream_item(
    session: &session::Session,
    room: Option<&str>,
    service: &sonos::smapi::Service,
    token: Option<&sonos::smapi::Token>,
    id: &str,
    title: &str,
) -> Result<()> {
    let uri = sonos::smapi::media_uri(service, token, id).await?;
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

    coordinator
        .call(
            json!({
                "namespace": "playbackSession:1",
                "command": "loadStreamUrl",
                "sessionId": session_id,
            }),
            // stationMetadata is optional, but it is where the name the room
            // displays comes from; without it the stream plays with nothing
            // to show.
            json!({
                "streamUrl": uri,
                "playOnCompletion": true,
                "stationMetadata": {
                    "name": title,
                    "type": "station",
                    "service": { "name": service.name, "id": service.id },
                },
            }),
        )
        .await?;
    println!("{} — {title} on {}", target.name, service.name);
    Ok(())
}

/// `x2rock play-item`: play a hit whose id is already known.
///
/// `search --play N` re-runs the search to find the Nth result, which costs a
/// second round trip and can land on a different item if the service reorders.
/// Anything holding results already - the bar widget - should come here instead.
async fn run_play_item(
    ip: Option<IpAddr>,
    room: Option<&str>,
    service: &str,
    kind: Option<&str>,
    id: &str,
    title: Option<&String>,
) -> Result<()> {
    let mut state = State::load()?;
    let session = session::connect(ip, &mut state).await?;
    let mut catalogue = catalogue::Catalogue::load();
    catalogue
        .refresh(&Upnp::new(session.connection.ip()), false)
        .await?;
    let linked = credentials::Credentials::load()?;
    let usable = catalogue.searchable(&linked);
    let chosen = catalogue::Catalogue::find(&usable, service)?.clone();
    let token = linked.get(&chosen.id).map(|a| a.token());
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

/// A rough age, for a list where the exact second has never mattered.
fn ago(then: u64) -> String {
    let now = credentials::now();
    let seconds = now.saturating_sub(then);
    match seconds {
        0..=90 => "just now".to_string(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// What the household should call this machine's account, when nothing was given.
///
/// The hostname, because a household can have several machines linked to the
/// same service account and the Sonos app shows this string.
fn default_nickname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| format!("x2rock on {h}"))
        .unwrap_or_else(|| "x2rock".to_string())
}

/// Hand a URL to whatever browser the person already uses.
///
/// Spawned and not waited on: `xdg-open` stays attached to some handlers for as
/// long as the browser lives, and the polling loop below is what should be
/// running, not a wait.
fn open_in_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("running xdg-open")?;
    Ok(())
}

/// `x2rock link`: the device-link flow, end to end.
///
/// A player is required, unlike search: the link is minted *for a household*,
/// and its id is in both SMAPI calls. The browser step is the person's own
/// browser, and the whole interaction for a service like Bandcamp is: open a
/// link, log in, done.
async fn run_link(
    ip: Option<IpAddr>,
    service: Option<&String>,
    no_open: bool,
    nickname: Option<&String>,
    no_match: bool,
) -> Result<()> {
    let mut linked = credentials::Credentials::load()?;
    let mut state = State::load()?;
    let session = session::connect(ip, &mut state).await?;
    let mut catalogue = catalogue::Catalogue::load();
    if catalogue
        .refresh(&Upnp::new(session.connection.ip()), false)
        .await?
    {
        catalogue.save()?;
    }

    let Some(query) = service else {
        let linkable = catalogue.linkable();
        println!("{} services can be linked:", linkable.len());
        for s in &linkable {
            let mark = match linked.get(&s.id) {
                Some(a) => format!("  (linked {})", ago(a.linked)),
                None => String::new(),
            };
            println!("  {}{mark}", s.name);
        }
        println!("\nLink one with: x2rock link <service>");
        return Ok(());
    };

    let chosen = catalogue.find_any(query)?.clone();
    ensure!(
        chosen.auth == sonos::smapi::Auth::DeviceLink,
        "{} does not use device linking. {}",
        chosen.name,
        match chosen.auth {
            sonos::smapi::Auth::Anonymous =>
                "It needs no account at all - search it as it is.".to_string(),
            _ => format!(
                "It hands off to its own app, which a Linux desktop cannot do, \
                 so x2rock cannot link it. Run `x2rock link` for the {} that work.",
                catalogue.linkable().len()
            ),
        }
    );

    let household = session.connection.household_id().await?;
    let code = sonos::smapi::device_link_code(&chosen, &household).await?;

    if no_open {
        println!(
            "Open this and log in to {}:\n\n  {}\n",
            chosen.name, code.reg_url
        );
    } else {
        // A failure to open is not a failure to link: the URL is right there,
        // and this is the one path that matters over ssh.
        match open_in_browser(&code.reg_url) {
            Ok(()) => println!("Opened {} in your browser.", chosen.name),
            Err(e) => println!(
                "Could not open a browser ({e:#}). Open this yourself:\n\n  {}\n",
                code.reg_url
            ),
        }
    }
    if code.show_link_code {
        println!("Enter this code when asked:\n\n  {}\n", code.link_code);
    }

    let deadline = tokio::time::Instant::now() + sonos::smapi::LINK_DEADLINE;
    eprint!("Waiting for you to finish");
    let auth = loop {
        match sonos::smapi::device_auth_token(&chosen, &household, &code.link_code).await {
            Ok(Some(auth)) => {
                eprintln!();
                break auth;
            }
            Ok(None) => {
                use std::io::Write;
                eprint!(".");
                let _ = std::io::stderr().flush();
            }
            Err(e) => {
                eprintln!();
                return Err(e);
            }
        }
        if tokio::time::Instant::now() + sonos::smapi::LINK_POLL >= deadline {
            eprintln!();
            bail!(
                "{} never confirmed the link. Run `x2rock link {}` again to start over.",
                chosen.name,
                chosen.name
            );
        }
        tokio::time::sleep(sonos::smapi::LINK_POLL).await;
    };

    let nickname = nickname.cloned().unwrap_or_else(default_nickname);
    let hash = auth.user_id_hash_code.clone();
    let (id, account) = credentials::from_device_auth(
        &chosen.id,
        &chosen.name,
        Some(&household),
        Some(&nickname),
        auth,
    );
    // Stored before anything else is attempted. A link code is single-use, so
    // losing the token to a later failure would mean walking back through the
    // browser to fix something that already worked.
    linked.remember(&id, account);
    linked.save()?;
    println!(
        "Linked {}. Search it with: x2rock search -s {}",
        chosen.name, chosen.name
    );

    if no_match {
        return Ok(());
    }
    let Some(hash) = hash else {
        println!(
            "{} sent no userIdHashCode, so the household cannot be told about \
             the account. Search works; playback through the household may not.",
            chosen.name
        );
        return Ok(());
    };
    match session
        .connection
        .match_music_service_account(
            &household,
            &chosen.id,
            &hash,
            &nickname,
            Some(&code.link_code),
        )
        .await
    {
        Ok(account_id) => {
            if let Some(entry) = linked.services.get_mut(&id) {
                entry.account_id = account_id.clone();
            }
            linked.save()?;
            match account_id {
                Some(id) => println!("Registered on the household as account {id}."),
                None => println!("Registered on the household."),
            }
        }
        // The token is already on disk and already useful, so this is a warning
        // and not an error: failing the command here would suggest the whole
        // flow needs repeating, and it does not.
        //
        // Worded mildly on purpose. Every `match` this project has attempted
        // has been refused, and nothing has yet needed it: a service's own
        // stream URL carries the account identity - iHeartRadio's has the
        // `userIdHashCode` in it as `profileId` - so the household does not have
        // to know about the account for x2rock to play from it. An alarming
        // message here would send someone chasing a step that may simply not be
        // available to a controller.
        Err(e) => println!(
            "The household would not register the account ({e:#}), which so far \
             has not mattered: the token is stored and search works. See \
             docs/architecture.md, \"match, and why nothing needs it yet\".",
        ),
    }
    Ok(())
}

/// `x2rock browse`: a music service's own containers, walked one level at a time.
///
/// The half of a linked service that `search` cannot reach. "Play something from
/// my playlists" is not a search - it names a place, not a word - and every
/// service puts those places behind `getMetadata` starting at `root`.
///
/// A player is wanted but not required, on the same terms as `search`: listing
/// what can be browsed comes from the on-disk catalogue, and only `--play` and a
/// first run with nothing cached genuinely need one.
#[allow(clippy::too_many_arguments)]
async fn run_browse(
    ip: Option<IpAddr>,
    room: Option<&str>,
    service: Option<&String>,
    container: Option<&str>,
    count: u32,
    play: Option<usize>,
    refresh: bool,
    json: bool,
) -> Result<()> {
    let mut state = State::load()?;
    let reached = session::connect(ip, &mut state).await;
    let live = || -> Result<&session::Session> {
        reached
            .as_ref()
            .map_err(|e| anyhow!("no player to play it on: {e:#}"))
    };

    let mut catalogue = catalogue::Catalogue::load();
    match &reached {
        Ok(session) => {
            if catalogue
                .refresh(&Upnp::new(session.connection.ip()), refresh)
                .await?
            {
                catalogue.save()?;
            }
        }
        Err(e) if catalogue.services().is_empty() => return Err(anyhow!("{e:#}")),
        Err(e) => eprintln!("x2rock: no player reached, using the cached catalogue ({e:#})"),
    }

    let linked = credentials::Credentials::load()?;
    // The same set `search` offers. Browsing needs exactly what searching needs -
    // an endpoint and, for a linked service, a token - so a service that can be
    // searched can be walked, and one that cannot, cannot.
    let usable = catalogue.searchable(&linked);

    let Some(query) = service else {
        let mut names: Vec<_> = usable.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable_by_key(|n| n.to_lowercase());
        if json {
            println!("{}", serde_json::to_string_pretty(&names)?);
        } else {
            println!("{} services can be browsed:", usable.len());
            for name in names {
                println!("  {name}");
            }
            println!("\nOpen one with: x2rock browse -s <service>");
        }
        return Ok(());
    };

    let chosen = catalogue::Catalogue::find(&usable, query)?.clone();
    let token = linked.get(&chosen.id).map(|a| a.token());
    // `root` is where every service starts, and no service documents it - it is
    // simply what the players ask for.
    let at = container.unwrap_or("root");
    let (items, total) = sonos::smapi::metadata(&chosen, token.as_ref(), at, 0, count).await?;

    if let Some(nth) = play {
        let item = items
            .get(nth.checked_sub(1).unwrap_or(usize::MAX))
            .ok_or_else(|| anyhow!("no row {nth}; {at} has {}", items.len()))?;
        // A container is a place, and refusing here is kinder than letting
        // getMediaURI refuse it with a grammar error about ids.
        ensure!(
            !item.container,
            "{:?} is a container. Open it with: x2rock browse -s {} {}",
            item.title,
            chosen.name,
            item.id
        );
        return play_item(
            live()?,
            room,
            &chosen,
            token.as_ref(),
            Some(item.item_type.as_str()),
            &item.id,
            &item.title,
        )
        .await;
    }

    if json {
        let rows: Vec<_> = items
            .iter()
            .map(|i| {
                // The field names `favorites`, `search` and `bookmarks` already
                // use, plus the one thing only browsing has: whether a row is a
                // place or a thing.
                json!({
                    "id": i.id,
                    "name": i.title,
                    "type": i.item_type,
                    "description": i.summary,
                    "service": chosen.name,
                    "art_url": i.art_url,
                    "container": i.container,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("{} is empty on {}.", at, chosen.name);
        return Ok(());
    }
    for item in &items {
        let summary = item
            .summary
            .as_deref()
            .map(|s| format!("  {s}"))
            .unwrap_or_default();
        // A trailing slash for somewhere to go, the way a directory listing
        // marks one. Cheaper to read than a column.
        let name = if item.container {
            format!("{}/", item.title)
        } else {
            item.title.clone()
        };
        println!("{:<40} {:<10} {name}{summary}", item.id, item.item_type);
    }
    if total > items.len() as u32 {
        println!("\n{} of {total} in {at}.", items.len());
    }
    Ok(())
}

/// `x2rock search`: the CLI talking to a music service. One of three commands
/// that leave the LAN - `browse` and `link` are the others - and like them it is
/// CLI-only and unreachable from the daemon. See "Rule: talking to a service
/// never enters the daemon".
///
/// A player is wanted but not required. Listing what can be searched, and a
/// service's categories, both come from the on-disk catalogue and must keep
/// working when the household is unreachable - a cache that fails whenever the
/// thing it caches is unavailable is not doing its job. Only `--play`, and a
/// first run with nothing cached, genuinely need a player.
#[allow(clippy::too_many_arguments)]
async fn run_search(
    ip: Option<IpAddr>,
    room: Option<&str>,
    term: Option<&String>,
    service: Option<&String>,
    category: Option<&String>,
    count: u32,
    play: Option<usize>,
    refresh: bool,
    json: bool,
) -> Result<()> {
    let mut state = State::load()?;
    let reached = session::connect(ip, &mut state).await;
    let live = || -> Result<&session::Session> {
        reached
            .as_ref()
            .map_err(|e| anyhow!("no player to play it on: {e:#}"))
    };

    let mut catalogue = catalogue::Catalogue::load();
    let mut dirty = false;
    match &reached {
        Ok(session) => {
            dirty = catalogue
                .refresh(&Upnp::new(session.connection.ip()), refresh)
                .await?;
        }
        Err(e) if catalogue.services().is_empty() => {
            // Nothing cached and nothing to ask: this is the one case with no
            // useful answer, so give the connection's own error rather than a
            // second-hand one about an empty catalogue.
            return Err(anyhow!("{e:#}"));
        }
        Err(e) => eprintln!("x2rock: no player reached, using the cached catalogue ({e:#})"),
    }

    let linked = credentials::Credentials::load()?;
    let usable = catalogue.searchable(&linked);

    let Some(query) = service else {
        let mut names: Vec<_> = usable.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable_by_key(|n| n.to_lowercase());
        if json {
            println!("{}", serde_json::to_string_pretty(&names)?);
        } else {
            let linkable = catalogue.linkable().len();
            println!(
                "{} of {} services can be searched:",
                usable.len(),
                catalogue.services().len()
            );
            for name in names {
                let mark = if linked.services.values().any(|a| a.service_name == name) {
                    "  (linked)"
                } else {
                    ""
                };
                println!("  {name}{mark}");
            }
            println!("\nSearch one with: x2rock search -s <service> <term>");
            if linkable > 0 {
                println!("{linkable} more can be linked: x2rock link");
            }
        }
        if dirty {
            catalogue.save()?;
        }
        return Ok(());
    };

    // Naming a real service that simply needs an account is a different
    // mistake from naming one that does not exist, and the difference is
    // worth the extra lookup.
    let chosen = catalogue::Catalogue::find(&usable, query)
        .map_err(|e| {
            match catalogue
                .services()
                .iter()
                .find(|s| s.name.to_lowercase() == query.to_lowercase())
            {
                Some(s) if s.auth == sonos::smapi::Auth::DeviceLink => anyhow!(
                    "{} needs a linked account. Run `x2rock link {}` once, \
                     then search it like any other.",
                    s.name,
                    s.name
                ),
                Some(s) => anyhow!(
                    "{} authenticates by handing off to its own app, which a \
                     Linux desktop cannot do. Run `x2rock search` for the ones \
                     that can be searched.",
                    s.name
                ),
                None => e,
            }
        })?
        .clone();

    let categories = catalogue.categories_for(&chosen).await?;
    dirty |= !categories.is_empty();
    if dirty {
        catalogue.save()?;
    }
    let chosen = &chosen;
    ensure!(
        !categories.is_empty(),
        "{} publishes no search categories, so it cannot be searched",
        chosen.name
    );
    let picked = match category {
        Some(want) => {
            let want = want.to_lowercase();
            categories
                .iter()
                .find(|c| c.id.to_lowercase() == want)
                .ok_or_else(|| {
                    let known: Vec<_> = categories.iter().map(|c| c.id.as_str()).collect();
                    anyhow!(
                        "{} has no category {want:?}. It has: {}",
                        chosen.name,
                        known.join(", ")
                    )
                })?
        }
        None => categories
            .iter()
            .find(|c| c.id.eq_ignore_ascii_case("all"))
            .unwrap_or(&categories[0]),
    };

    let Some(term) = term else {
        let known: Vec<_> = categories.iter().map(|c| c.id.as_str()).collect();
        println!("{} can search: {}", chosen.name, known.join(", "));
        println!("Default is {}. Give a term to search.", picked.id);
        return Ok(());
    };

    let token = linked.get(&chosen.id).map(|a| a.token());
    let (items, total) =
        sonos::smapi::search(chosen, token.as_ref(), &picked.mapped_id, term, 0, count).await?;

    if let Some(nth) = play {
        let item = items
            .get(nth.checked_sub(1).unwrap_or(usize::MAX))
            .ok_or_else(|| anyhow!("no result {nth}; the search returned {}", items.len()))?;
        // A search can return places rather than things: every Mixcloud hit is a
        // `tag:` collection, not a track. Refusing here beats letting
        // getMediaURI refuse it with a grammar error about ids.
        ensure!(
            !item.container,
            "{:?} is a container, not a track. Open it with: x2rock browse -s {} {}",
            item.title,
            chosen.name,
            item.id
        );
        return play_item(
            live()?,
            room,
            chosen,
            token.as_ref(),
            Some(item.item_type.as_str()),
            &item.id,
            &item.title,
        )
        .await;
    }
    if json {
        let rows: Vec<_> = items
            .iter()
            .map(|i| {
                // Deliberately the same field names `favorites --json` uses.
                // The bar widget merges the two lists into one picker, and
                // matching shapes keep that a concatenation rather than a
                // translation layer.
                json!({
                    "id": i.id,
                    "name": i.title,
                    "type": i.item_type,
                    "description": i.summary,
                    "service": chosen.name,
                    "art_url": i.art_url,
                    // A hit is not always a thing to play. Mixcloud searches
                    // tags and answers with collections, so a caller that
                    // assumed otherwise would hand a container to `play-item`.
                    "container": i.container,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("Nothing on {} for {term:?}.", chosen.name);
        return Ok(());
    }
    for item in &items {
        let summary = item
            .summary
            .as_deref()
            .map(|s| format!("  {s}"))
            .unwrap_or_default();
        // The same trailing slash `browse` uses, and for the same reason: some
        // services answer a search entirely in collections.
        let name = if item.container {
            format!("{}/", item.title)
        } else {
            item.title.clone()
        };
        println!("{:<14} {:<10} {name}{summary}", item.id, item.item_type);
    }
    if total > items.len() as u32 {
        println!("\n{} of {total} on {}.", items.len(), chosen.name);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Discover => return discover_and_remember(&mut State::load()?).await,
        Command::PlayItem {
            ref service,
            ref id,
            ref title,
            ref kind,
        } => {
            return run_play_item(
                cli.ip,
                cli.room.as_deref(),
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
            play,
            refresh,
            json,
        } => {
            return run_browse(
                cli.ip,
                cli.room.as_deref(),
                service.as_ref(),
                container.as_deref(),
                count,
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
        } => {
            return run_link(
                cli.ip,
                service.as_ref(),
                no_open,
                nickname.as_ref(),
                no_match,
            )
            .await;
        }
        // Both of these are about a file on this machine, so neither needs a
        // player and both work with the household unreachable.
        Command::Unlink { ref service } => {
            let mut linked = credentials::Credentials::load()?;
            let id = linked
                .services
                .iter()
                .find(|(_, a)| a.service_name.eq_ignore_ascii_case(service))
                .map(|(id, _)| id.clone())
                .ok_or_else(|| {
                    anyhow!("no account linked for {service:?}. Run `x2rock accounts` to see them.")
                })?;
            let dropped = linked.forget(&id);
            linked.save()?;
            if let Some(account) = dropped {
                println!(
                    "Forgot the {} token. It is still valid at the service - \
                     revoke it there if that matters.",
                    account.service_name
                );
            }
            return Ok(());
        }
        Command::Accounts { json } => {
            let linked = credentials::Credentials::load()?;
            if json {
                let rows: Vec<_> = linked
                    .services
                    .iter()
                    .map(|(id, a)| {
                        // Never the token or the key: this is printed to a
                        // terminal, into a widget's stdout, and into whatever
                        // logs those end up in.
                        json!({
                            "service_id": id,
                            "service": a.service_name,
                            "nickname": a.nickname,
                            "household": a.household,
                            "account_id": a.account_id,
                            "linked": a.linked,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if linked.services.is_empty() {
                println!("No accounts linked. Run `x2rock link` to see what can be.");
            } else {
                for (id, a) in &linked.services {
                    let registered = match &a.account_id {
                        Some(account) => format!("household account {account}"),
                        None => "not registered on the household".to_string(),
                    };
                    println!(
                        "{:<20} {:<10} {:<12} {registered}",
                        a.service_name,
                        id,
                        ago(a.linked)
                    );
                }
            }
            return Ok(());
        }
        Command::Search {
            ref term,
            ref service,
            ref category,
            count,
            play,
            refresh,
            json,
        } => {
            return run_search(
                cli.ip,
                cli.room.as_deref(),
                term.as_ref(),
                service.as_ref(),
                category.as_ref(),
                count,
                play,
                refresh,
                json,
            )
            .await;
        }
        Command::Daemon => {
            tokio::select! {
                result = daemon::run(cli.ip) => return result,
                signal = stop_signal() => {
                    eprintln!("x2rock: stopping on {signal}");
                    return Ok(());
                }
            }
        }
        _ => {}
    }

    let mut state = State::load()?;
    let session = session::connect(cli.ip, &mut state).await?;

    if let Command::Rooms { json } = cli.command {
        print_rooms(&session.groups, json);
        return Ok(());
    }

    // Favorites belong to the household, not a group, so listing them needs no
    // room and works when several groups would otherwise force a choice.
    if let Command::Favorites { query, json } = &cli.command {
        let household = session.connection.household_id().await?;
        let mut favorites = session.connection.favorites(&household).await?.items;
        if let Some(query) = query {
            let needle = query.to_lowercase();
            favorites.retain(|f| f.name.to_lowercase().contains(&needle));
        }
        print_favorites(&favorites, *json);
        return Ok(());
    }

    if let Command::Raw {
        namespace,
        command,
        options,
        scope,
        watch,
        session: session_id,
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
                let target = session::target(&session.groups, cli.room.as_deref())?;
                envelope["groupId"] = json!(target.group_id);
                connection = session::coordinator(&session, &target).await?;
            }
            RawScope::Player => {
                // A player answers player-scoped commands only for itself, so
                // naming one over a socket to another gets ERROR_INVALID_OBJECT_ID
                // - "Incorrect playerId" - for an id that is perfectly correct.
                let player = match cli.room.as_deref() {
                    Some(room) => session.groups.player_named(room)?,
                    None => {
                        let id = session.groups.resolve(None)?.coordinator_id.clone();
                        session
                            .groups
                            .player(&id)
                            .ok_or_else(|| anyhow!("group coordinator {id} is not a known player"))?
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

    // Kept items are x2rock's own and live on this machine, so listing them
    // needs no household at all.
    if let Command::Bookmarks {
        action,
        query,
        all,
        json,
    } = &cli.command
    {
        // Removing needs no household either, and has to happen before the
        // listing below reads the file it is about to change.
        if let Some(BookmarksAction::Remove { query }) = action {
            let mut list = bookmarks::Bookmarks::load()?;
            let gone = list.forget(query)?;
            list.save()?;
            println!("Forgot {}.", gone.name);
            return Ok(());
        }
        let list = bookmarks::Bookmarks::load()?;
        let mut items = list.listed(*all);
        if let Some(query) = query {
            let needle = query.to_lowercase();
            items.retain(|b| b.name.to_lowercase().contains(&needle));
        }
        if *json {
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
            println!("{}", serde_json::to_string(&rows)?);
        } else if items.is_empty() {
            // Four states were wearing one message, and a query filtering
            // everything out got the worst of it: "Nothing kept. Play something
            // and run `x2rock keep`" told someone with a full file that their
            // file was empty. What is empty, and what to do about it, differ.
            match query {
                Some(q) => {
                    // Whether `--all` would have found it is the useful half of
                    // the answer, and it costs one pass over what is loaded.
                    let deeper = if *all {
                        0
                    } else {
                        let needle = q.to_lowercase();
                        list.listed(true)
                            .iter()
                            .filter(|b| b.name.to_lowercase().contains(&needle))
                            .count()
                    };
                    if deeper > 0 {
                        println!(
                            "Nothing kept matches {q:?}, but {deeper} of what played recently \
                             does. `x2rock bookmarks --all {q:?}`."
                        );
                    } else if *all {
                        println!("Nothing kept or played recently matches {q:?}.");
                    } else {
                        println!("Nothing kept matches {q:?}.");
                    }
                }
                None => {
                    let hidden = list.items.len();
                    if hidden > 0 && !*all {
                        println!(
                            "Nothing kept, but {hidden} played recently. `x2rock bookmarks --all`."
                        );
                    } else {
                        println!("Nothing kept. Play something and run `x2rock keep`.");
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
                println!("{mark} {}{by}{on}", b.name);
            }
        }
        return Ok(());
    }

    // Grouping resolves rooms itself: `ungroup` names its room positionally and
    // must work without --room, which the shared target resolution below would
    // refuse while the household has several groups.
    if let Command::Group { rooms } = &cli.command {
        let host = session.groups.resolve(cli.room.as_deref())?;
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
        let target = session::target(&session.groups, cli.room.as_deref())?;
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
                let host = session.groups.resolve(cli.room.as_deref())?;
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
                let target = session::target(&session.groups, cli.room.as_deref())?;
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

    let target = session::target(&session.groups, cli.room.as_deref())?;
    let player = session::coordinator(&session, &target).await?;
    let group = target.group_id.as_str();

    match cli.command {
        Command::Now { json } => {
            let status = player.playback_status(group).await?;
            let meta = player.metadata(group).await?;
            if json {
                println!("{}", now_json(&target.name, &status, &meta));
            } else {
                println!("{}", now_line(&status, &meta));
            }
        }
        Command::Play { track: None } => player.playback(group, "play").await?,
        Command::Play { track: Some(n) } => {
            ensure!(n >= 1, "queue tracks are numbered from 1");
            // The queue lives on the coordinator and only UPnP can address it by
            // position. Make sure the queue is the source first: after a radio
            // station or line-in it is not, and Seek would fail with error 701.
            let upnp = Upnp::new(target.coordinator_ip.unwrap_or(player.ip()));
            if !upnp.playing_from_queue().await? {
                upnp.use_queue(&target.coordinator_id).await?;
            }
            upnp.seek_track(n).await?;
            player.playback(group, "play").await?;
        }
        Command::Keep { name, container } => {
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
            let title = name.or(fallback).ok_or_else(|| {
                anyhow!("what is playing has no name; give one: x2rock keep <name>")
            })?;

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

            let mut list = bookmarks::Bookmarks::load()?;
            let replaced = list.keep(bookmark);
            list.save()?;
            println!("{} {title}", if replaced { "Updated" } else { "Kept" });
        }
        Command::Bookmark { query, next } => {
            let list = bookmarks::Bookmarks::load()?;
            let bookmark = list.find(&query)?;

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
                })?;
            let cdudn = service.cdudn().ok_or_else(|| {
                anyhow!(
                    "{} has no service type in this player's list, so {:?} cannot name its account",
                    service.name,
                    bookmark.name
                )
            })?;

            let upnp = Upnp::new(target.coordinator_ip.unwrap_or(player.ip()));
            let length = upnp
                .add_to_queue(&bookmark.uri(), &bookmark.didl(&cdudn), next)
                .await?;
            if !next {
                // Appended, so it is the last track. Make the queue the source
                // first: after a station it is not, and Seek fails with 701.
                if !upnp.playing_from_queue().await? {
                    upnp.use_queue(&target.coordinator_id).await?;
                }
                upnp.seek_track(length).await?;
                player.playback(group, "play").await?;
            }
            println!("{:<24} {}", target.name, bookmark.name);
        }
        Command::Favorite { query } => {
            let household = session.connection.household_id().await?;
            let favorites = session.connection.favorites(&household).await?;
            let favorite = find_favorite(&favorites.items, &query)?;
            // Household-scoped to find, group-scoped to play.
            player.load_favorite(group, &favorite.id).await?;
            println!("{:<24} {}", target.name, favorite.name);
        }
        Command::Tv => {
            // The soundbar is the player with the HDMI socket, which is not
            // necessarily the one coordinating the group it is in. The room
            // named is asked first; otherwise (or when the widget names the
            // group by its coordinator) it is whichever member has one.
            let is_soundbar = |p: &&Player| p.capabilities.iter().any(|c| c == "HT_PLAYBACK");
            let members = session
                .groups
                .members(session.groups.resolve(cli.room.as_deref())?);
            let named = match cli.room.as_deref() {
                Some(name) => Some(session.groups.player_named(name)?),
                None => session.groups.player(&target.coordinator_id),
            };
            let room = match named.filter(is_soundbar) {
                Some(bar) => bar,
                None => members
                    .iter()
                    .copied()
                    .find(is_soundbar)
                    .ok_or_else(|| anyhow!("no room in {} has a TV input", target.name))?,
            };
            let coordinator_ip = target.coordinator_ip.unwrap_or(player.ip());
            let upnp = Upnp::new(coordinator_ip);
            // The soundbar's own address, so the switch can be confirmed there
            // when handing the group over costs the coordinator its reply.
            let bar = room
                .ip()
                .ok_or_else(|| anyhow!("no address for {}", room.name))?;
            // Taking a group over stalls every player in it for about fourteen
            // seconds. Said on stderr, so it stays out of anything reading the
            // result, and only when there is a group to take.
            if bar != coordinator_ip {
                eprintln!("{:<24} taking its group to the TV input...", room.name);
            }
            upnp.use_tv_input(&room.id, bar).await?;
            println!("{:<24} TV input", room.name);
        }
        Command::Queue { action, json } => {
            let upnp = Upnp::new(target.coordinator_ip.unwrap_or(player.ip()));
            let room = target.name.as_str();
            match action {
                None => {
                    let queue = upnp.queue().await?;
                    let current = if upnp.playing_from_queue().await? {
                        upnp.current_track().await?
                    } else {
                        0
                    };
                    print_queue(&queue, current, json);
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
                    let tracks = if count == 1 { "track" } else { "tracks" };
                    println!("{room:<24} removed {count} {tracks}, {left} left");
                }
                Some(QueueAction::Clear { yes }) => {
                    ensure!(
                        yes,
                        "clearing the queue cannot be undone; pass --yes to confirm"
                    );
                    upnp.clear_queue().await?;
                    println!("{room:<24} queue cleared");
                }
                Some(QueueAction::Move { from, to }) => {
                    ensure!(from >= 1 && to >= 1, "queue tracks are numbered from 1");
                    upnp.move_track(from, to).await?;
                    println!("{room:<24} moved track {from} to {to}");
                }
                Some(QueueAction::Sources { query, json }) => {
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
                    let tracks = if added == 1 { "track" } else { "tracks" };
                    println!(
                        "{room:<24} added {added} {tracks} from {:?}, {after} in the queue",
                        item.title
                    );
                }
                Some(QueueAction::Save { name }) => {
                    ensure!(!name.trim().is_empty(), "a playlist needs a name");
                    let id = upnp.save_queue(&name).await?;
                    println!("{room:<24} saved as {name:?} ({id})");
                }
            }
        }
        Command::Repeat { mode } => {
            let status = player.playback_status(group).await?;
            let before = status.play_modes.repeat();
            let after = match mode.as_deref() {
                None => before,
                Some(text) => {
                    let Some(repeat) = Repeat::parse(text) else {
                        bail!("repeat takes off, all or one");
                    };
                    ensure!(
                        status.available_playback_actions.allows(repeat),
                        "what {} is playing cannot be {}",
                        target.name,
                        repeat.denied_as()
                    );
                    player.set_repeat(group, repeat).await?;
                    repeat
                }
            };
            let from = transition(before.as_str(), after.as_str());
            println!("{:<24} repeat {from}{}", target.name, after.as_str());
        }
        Command::Shuffle { mode } => {
            let status = player.playback_status(group).await?;
            let before = status.play_modes.shuffle;
            let after = match mode.as_deref() {
                None => before,
                Some(text @ ("on" | "off")) => {
                    let shuffle = text == "on";
                    // Turning it off is always allowed, as with repeat.
                    ensure!(
                        !shuffle || status.available_playback_actions.can_shuffle,
                        "what {} is playing cannot be shuffled",
                        target.name
                    );
                    player.set_shuffle(group, shuffle).await?;
                    shuffle
                }
                Some(_) => bail!("shuffle takes on or off"),
            };
            let word = |on: bool| if on { "on" } else { "off" };
            let from = transition(word(before), word(after));
            println!("{:<24} shuffle {from}{}", target.name, word(after));
        }
        Command::Pause => player.playback(group, "pause").await?,
        Command::Toggle => player.playback(group, "togglePlayPause").await?,
        Command::Next => player.playback(group, "skipToNextTrack").await?,
        Command::Prev => player.playback(group, "skipToPreviousTrack").await?,
        Command::Vol {
            change,
            player: one_room,
        } => {
            // --player names the speaker, so it resolves the room asked for
            // rather than the group's name: once rooms are grouped the group is
            // called after its coordinator ("Dining Room + 1"), which is no
            // player's name at all.
            let this = one_room
                .then(|| match cli.room.as_deref() {
                    Some(name) => session.groups.player_named(name),
                    // No room named, so the group resolved by default; its
                    // coordinator is the speaker meant. By id: the group's
                    // name ("Kitchen + 1") is not a player's once grouped.
                    None => session
                        .groups
                        .player(&target.coordinator_id)
                        .ok_or_else(|| anyhow!("no player for {}", target.name)),
                })
                .transpose()?;
            // A player-scoped command is refused by anyone but that player
            // ("Incorrect playerId"), so it cannot ride the coordinator's
            // connection the way group commands do.
            let speaker = match this.as_ref() {
                Some(named) => {
                    // No falling back to whatever socket is handy: the command
                    // would be refused as "Incorrect playerId", which reads as a
                    // bug rather than as a player we could not address.
                    let ip = named.ip().with_context(|| {
                        format!("{} did not report an address to reach it on", named.name)
                    })?;
                    if ip == session.connection.ip() {
                        session.connection.clone()
                    } else {
                        Connection::open(ip).await?
                    }
                }
                None => player.clone(),
            };
            // Name the speaker, not the group: "Dining Room + 1  22" is a
            // confusing way to report what Kitchen was set to.
            let label = this.map_or(target.name.clone(), |p| p.name.clone());
            let this = this.map(|p| p.id.clone());
            // The player acks a volume command before the change is visible, so a read
            // straight after a write can return the old value. Report the outcome from
            // what was asked instead; the daemon gets the truth from events.
            let before = match &this {
                Some(id) => speaker.player_volume(id).await?,
                None => player.group_volume(group).await?,
            };
            let change = change.as_deref().map(parse_volume).transpose()?;
            if change.is_some() && before.fixed {
                bail!(
                    "{} has fixed volume; adjust it on the amplifier",
                    target.name
                );
            }
            let (level, muted) = match change {
                None => (before.volume, before.muted),
                // Both setVolume and setRelativeVolume unmute (verified).
                Some(VolumeChange::Set(level)) => {
                    match &this {
                        Some(id) => speaker.set_player_volume(id, level).await?,
                        None => player.set_group_volume(group, level).await?,
                    }
                    (level, false)
                }
                Some(VolumeChange::Adjust(delta)) => {
                    match &this {
                        Some(id) => speaker.adjust_player_volume(id, delta).await?,
                        None => player.adjust_group_volume(group, delta).await?,
                    }
                    let level = (i16::from(before.volume) + i16::from(delta)).clamp(0, 100);
                    (level as u8, false)
                }
                Some(VolumeChange::Mute(muted)) => {
                    // Muting one speaker of a group is not offered: the group
                    // mute is what people mean, and a silently muted member is
                    // a puzzle to find later.
                    ensure!(this.is_none(), "--player does not apply to mute");
                    player.set_group_mute(group, muted).await?;
                    (before.volume, muted)
                }
            };
            let from = transition(&before.volume.to_string(), &level.to_string());
            let muted = if muted { "  (muted)" } else { "" };
            println!("{label:<24} {from}{level}{muted}");
        }
        Command::Rooms { .. }
        | Command::Favorites { .. }
        | Command::Group { .. }
        | Command::Ungroup { .. }
        | Command::Party { .. }
        | Command::Raw { .. }
        | Command::Bookmarks { .. }
        | Command::Search { .. }
        | Command::PlayItem { .. }
        | Command::Browse { .. }
        | Command::Link { .. }
        | Command::Unlink { .. }
        | Command::Accounts { .. }
        | Command::Discover
        | Command::Daemon => unreachable!("handled above"),
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
}
