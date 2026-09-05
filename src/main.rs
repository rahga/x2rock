mod bookmarks;
mod catalogue;
mod credentials;
mod daemon;
mod discover;
mod hint;
mod mpris;
mod netid;
mod restart;
mod session;
mod sonos;
mod state;
mod stations;

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;

use sonos::local::Connection;
use sonos::proto::{
    Favorite, Group, Groups, MetadataStatus, PlaybackStatus, Player, Repeat, Volume,
};
use sonos::upnp::{self, Upnp};
use state::State;

#[derive(Parser)]
#[command(name = "x2rock", version, about = "Local-first Sonos control")]
struct Cli {
    /// Room to control. Not needed when the household has a single group.
    /// Repeatable for the per-room commands (volume, transport, repeat,
    /// shuffle): `-r Kitchen -r Bedroom vol 10` applies to each, topology
    /// resolved once. Other commands take a single `--room`.
    #[arg(long, short = 'r', global = true, env = "X2ROCK_ROOM")]
    room: Vec<String>,

    /// Apply a per-room command to every room, topology resolved once - "turn
    /// it down everywhere" as `--all vol -10`. Only the per-room commands
    /// (volume, transport, repeat, shuffle); exclusive with `--room`.
    #[arg(long, global = true, conflicts_with = "room")]
    all: bool,

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
    /// The whole household at a glance: every room with its playback,
    /// now-playing, volume, grouping and TV capability - one call, for scripts
    /// and agents that want the full picture without a call per room.
    Status {
        #[arg(long)]
        json: bool,
        /// Wrap the rooms in a household envelope: which household and network,
        /// how many rooms answered, and warnings for any that did not. With
        /// `--json` it becomes an object `{household, network, total, reachable,
        /// warnings, rooms}`; on its own it adds a summary line. Plain `--json`
        /// stays a bare array.
        #[arg(long)]
        full: bool,
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
        /// Every speaker in the group, each set to this level directly, rather
        /// than through the group slider. The slider preserves the members'
        /// balance - the same scaling the Sonos app shows - so `--each 30`
        /// is the way to *erase* that balance, setting everyone to 30 in one
        /// call, without the set-to-zero-then-raise the slider otherwise needs.
        /// Reads the members from the current grouping, so it always covers
        /// exactly who is grouped now. Exclusive with `--player` (one speaker
        /// vs every speaker), and not for mute - group mute is what mute means.
        #[arg(long, conflicts_with = "player")]
        each: bool,
        /// The resulting `{room, volume, muted, fixed}` as JSON - for reading it
        /// or for confirming a change.
        #[arg(long)]
        json: bool,
    },
    /// Show or set repeat: off, all (the queue) or one (the current track).
    Repeat {
        mode: Option<String>,
        /// The resulting `{room, repeat}` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show or set shuffle: on or off.
    Shuffle {
        mode: Option<String>,
        /// The resulting `{room, shuffle}` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check what firmware every speaker has, and whether one is offered.
    ///
    /// **Read-only.** x2rock will not apply an update: that reboots speakers,
    /// and the Sonos app gates it behind a dialog warning against unplugging
    /// anything, which a command-line flag would not replace. Use the app.
    Update {
        /// `{room, installed, offered, up_to_date, download_bytes}` per speaker.
        #[arg(long)]
        json: bool,
    },
    /// Every speaker in the household: model, firmware, hardware and bonding.
    ///
    /// The Sonos apps' "About My System", and the one command that speaks in
    /// *players* rather than rooms. Everything else here hides bonding on
    /// purpose - a room is a room whether one speaker or four back it - which
    /// leaves no way to ask what a household is actually made of. A Sub, a
    /// surround and the hidden half of a stereo pair appear nowhere else,
    /// firmware included, so a satellite left behind by an update is invisible
    /// to `x2rock update` and visible here.
    ///
    /// **Read-only**, and local: the household's own topology plus each
    /// player's self-description, no account and nothing sent anywhere.
    System {
        /// One object per player, with `room`, `model`, `role`, `channels`,
        /// `serial`, `display_version`, `build`, `hardware_version` and `ip`.
        #[arg(long)]
        json: bool,
        /// Mask serial numbers, addresses and uuids - everything that
        /// identifies hardware, since a RINCON uuid embeds the MAC verbatim -
        /// for pasting somewhere public. The household id is never printed
        /// either way.
        #[arg(long)]
        redact: bool,
    },
    /// List the household's alarms.
    ///
    /// Alarms are household-wide - one list, each entry naming its room - so
    /// this takes no --room. Created in the Sonos app; x2rock can turn them on
    /// and off and remove them.
    Alarms {
        #[command(subcommand)]
        action: Option<AlarmsAction>,
        /// The list as JSON: `{id, room, start, duration_ms, recurrence,
        /// enabled, volume, play_mode, program, include_grouped}` per alarm.
        #[arg(long)]
        json: bool,
    },
    /// Turn an alarm on or off, or remove it, by id from `x2rock alarms`.
    Alarm {
        id: u32,
        #[command(subcommand)]
        action: AlarmAction,
    },
    /// Show or set the sleep timer: the room stops playing when it runs out.
    ///
    /// With no argument it reads what is left. Per group, like transport.
    Sleep {
        /// How long: `45` or `45m` for minutes, `2h`, `1h30m`, `90s`, or the
        /// wire's own `HH:MM:SS`. `off` cancels a running timer.
        duration: Option<String>,
        /// The resulting `{room, sleep_ms}` as JSON, null when none is set.
        #[arg(long)]
        json: bool,
    },
    /// Show or set crossfade: on or off.
    ///
    /// The third play mode beside repeat and shuffle - it overlaps the end of
    /// one track with the start of the next. Per group, like the other two.
    Crossfade {
        mode: Option<String>,
        /// The resulting `{room, crossfade}` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show or set one speaker's tone controls: bass, treble and loudness.
    ///
    /// Per speaker rather than per group - rooms playing together share a group
    /// volume but keep their own tone - so --room names the speaker itself.
    /// With no flags it reads what the speaker holds. Reachable only over UPnP:
    /// the Control API has no EQ namespace, so this is the one door to it.
    Eq {
        /// Bass, -10 to 10. 0 is flat.
        #[arg(long, allow_negative_numbers = true)]
        bass: Option<i8>,
        /// Treble, -10 to 10. 0 is flat.
        #[arg(long, allow_negative_numbers = true)]
        treble: Option<i8>,
        /// Loudness: on or off. A bass lift that works at low listening levels,
        /// and on from the factory - a speaker nobody has touched is not flat.
        #[arg(long)]
        loudness: Option<String>,
        /// TruePlay: on or off. The room correction the iPhone app measures,
        /// applied underneath bass and treble rather than alongside them. Off
        /// is the honest setting for a speaker that has moved rooms since it
        /// was measured, since the stored curve describes the old one.
        #[arg(long)]
        trueplay: Option<String>,
        /// Night mode: on or off. Soundbars only - a room with a TV input. Evens
        /// out loud and quiet so late viewing does not wake the house.
        #[arg(long)]
        night: Option<String>,
        /// Dialog / speech enhancement: on or off. Soundbars only. Lifts voices
        /// out of the mix.
        #[arg(long)]
        dialog: Option<String>,
        /// The resulting `{room, bass, treble, loudness}` as JSON.
        #[arg(long)]
        json: bool,
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
    /// Search the internet radio directory: stations from outside Sonos's
    /// catalogue entirely.
    ///
    /// `search` reaches the services the player knows about - about a hundred,
    /// and Sonos's ceiling; this reaches
    /// past them, into a community directory of ordinary HTTP streams. No
    /// account, no key, no registration. `--play N` plays the Nth result the
    /// same way `play-url` does, alongside the queue rather than in it.
    ///
    /// With no arguments it lists the most-voted stations, which is the
    /// directory's nearest thing to a front page.
    Stations {
        /// Match on the station's name. Free text, and the directory matches
        /// on a substring.
        query: Option<String>,
        /// Match on a tag instead - `jazz`, `news`, `ambient`. Community
        /// assigned and free-form, so it is a guess that often pays off.
        #[arg(long)]
        tag: Option<String>,
        /// Two-letter country code, e.g. `GB`, `DE`, `US`.
        #[arg(long)]
        country: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Play the Nth result, 1-based, in --room.
        #[arg(long)]
        play: Option<usize>,
        /// With `--play`, return as soon as the player takes the URL instead
        /// of waiting to see whether it plays.
        #[arg(long)]
        no_wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Play an internet radio stream by its own URL, with no music service in
    /// the loop at all.
    ///
    /// The other end of `search`/`browse`: those reach the hundred or so
    /// services the household's player knows about, and this reaches anything
    /// else that serves audio over HTTP - an Icecast or SHOUTcast station, a podcast
    /// enclosure, a stream a service does not carry. No account, no
    /// registration, no sid.
    ///
    /// It opens a playback session, as a live stream from a service does, so
    /// the room's queue is left exactly as it was.
    PlayUrl {
        /// The stream's URL. `http` or `https`; the *player* fetches it, so it
        /// must be reachable from the speaker rather than from this machine.
        url: String,
        /// What the room should display. Defaults to the URL's host, which is
        /// the most recognisable part of a stream URL.
        #[arg(long)]
        title: Option<String>,
        /// Return as soon as the player takes the URL, without waiting to see
        /// whether it plays. Faster, and dishonest by design - for scripts that
        /// will check for themselves.
        #[arg(long)]
        no_wait: bool,
        /// Unlike the other play commands this one takes `--json`, because its
        /// refusals are about the *argument* and the *stream*:
        /// `bad_stream_url`, `stream_did_not_play` and `stream_unverified` are
        /// codes a caller can act on, and a code that only ever prints as
        /// prose is a contract with nobody on the other end. The third is not
        /// a verdict on the stream - the room's state could not be read, so
        /// look at the room (the message says whether it answered at all)
        /// rather than swapping the URL.
        #[arg(long)]
        json: bool,
    },
    /// Put one search result in the queue without playing it.
    ///
    /// `play-item`'s sibling, and the same enqueue underneath - it simply stops
    /// before making the queue current, seeking to the new track and pressing
    /// play. What the bar widget's `+` uses.
    ///
    /// A live stream is refused rather than half-worked: it has no queue form,
    /// which is why `play-item` streams one instead of queueing it.
    QueueItem {
        #[arg(long, short = 's')]
        service: String,
        id: String,
        /// What the queue should show. Defaults to the id.
        #[arg(long)]
        title: Option<String>,
        /// The item's own kind, as `search`/`browse --json` report it in `type`.
        /// `stream` is refused; anything else is queued.
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
        /// Plex only: no browser at all - store the token the household's own
        /// Plex integration exposes, read from the players' Plex art URLs.
        /// Needs Plex playing (or paused) in some room. That token can browse
        /// the service's root where a fresh account token sometimes cannot
        /// (a server without Remote Access), and it dies whenever Plex is
        /// relinked to Sonos - the browser flow's token is the durable one.
        #[arg(long)]
        from_player: bool,
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
        /// Also show the account serials this household's favorites and queues
        /// name. Needs a player; the rest of this command does not.
        #[arg(long)]
        household: bool,
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
    /// Play a saved playlist - a "Sonos playlist" - by name or id.
    ///
    /// Replaces what the room is playing and starts it, the way `favorite`
    /// does. `queue add` appends one to the queue instead, `queue save` creates
    /// one from what is queued now, and `queue sources` lists them.
    Playlist {
        query: String,
    },
    /// Switch a soundbar to its TV input.
    Tv,
    /// Play a short chime on a room, over whatever it is doing.
    ///
    /// The player's built-in notification sound, ducked over the current
    /// playback and gone in a second - the queue and the room's own volume are
    /// left exactly as they were. `notify` is the same mechanism with a sound
    /// of your own. It addresses the room's *own* player, so a chime lands on
    /// the room named rather than its whole group.
    Chime {
        /// How loud the chime plays, 0-100. Independent of the room's volume
        /// and not remembered after. Defaults to the player's own setting.
        #[arg(long)]
        volume: Option<u8>,
    },
    /// Play a sound from a URL on a room, over whatever it is doing.
    ///
    /// An announcement, a doorbell, any short clip. Like `chime` it ducks
    /// rather than replaces: the queue and the room's volume survive it. The
    /// *player* fetches the URL, so it must be reachable from the speaker (a
    /// public `http`/`https` address), not merely from this machine - the same
    /// rule `play-url` follows.
    Notify {
        /// The clip's URL, `http` or `https`. The player fetches it.
        url: String,
        /// How loud it plays, 0-100. Independent of the room's volume and not
        /// remembered after. Defaults to the player's own setting.
        #[arg(long)]
        volume: Option<u8>,
    },
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
    /// Install the x2rock agent skill so an AI assistant on this machine knows
    /// how to drive the CLI. Writes to `~/.claude/skills/x2rock/` by default
    /// (or `$CLAUDE_CONFIG_DIR/skills/`); the skill is embedded in the binary,
    /// so it always matches this version.
    Skill {
        /// Where to write it, in place of the default Claude skills directory.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Print the skill to stdout instead of writing it - for inspection, or
        /// to seed an agent that is not Claude.
        #[arg(long)]
        print: bool,
    },
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
enum AlarmsAction {
    /// Create an alarm. It is armed unless --off is given.
    Add {
        /// When, as `HH:MM` or `HH:MM:SS`, local to the household.
        time: String,
        /// How long it plays for: `15`/`15m` minutes, `1h`, `HH:MM:SS`.
        #[arg(long, default_value = "15m")]
        duration: String,
        /// `once` (the default), `daily`, `weekdays`, `weekends`, or `on_` and
        /// the days as digits with Sunday 0 - `on_135` for Mon/Wed/Fri. The
        /// player takes more than its own description admits, so this is passed
        /// through rather than checked against a list.
        #[arg(long, default_value = "once")]
        recurrence: String,
        /// 0-100. Loud enough to wake someone is the point, so this does not
        /// inherit the room's current level.
        #[arg(long, default_value_t = 25)]
        volume: u8,
        /// What it plays: a favorite or saved playlist, by name or id. Left
        /// out, it is the speaker's built-in chime.
        #[arg(long)]
        program: Option<String>,
        /// `normal` (the default), `repeat_all`, `shuffle`, `shuffle_norepeat`.
        #[arg(long, default_value = "normal")]
        play_mode: String,
        /// Also sound in rooms grouped with this one.
        #[arg(long)]
        grouped: bool,
        /// Create it disarmed, to be turned on later.
        #[arg(long)]
        off: bool,
    },
}

#[derive(Subcommand)]
enum AlarmAction {
    /// Arm it.
    On,
    /// Disarm it, leaving it in the list to be armed again.
    Off,
    /// Delete it. Sonos keeps no undo, and the app is the only way to make a
    /// new one.
    Remove {
        #[arg(long)]
        yes: bool,
    },
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
        let has_tv = session
            .groups
            .members(group)
            .iter()
            .any(|p| p.capabilities.iter().any(|c| c == "HT_PLAYBACK"));
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
                    "  {:<22} {:<5} {:<8} build {:<10} hw {:<16} {:<5} {:<15} {}",
                    info.model_name,
                    label,
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

/// The favorite a query names: its id exactly, else a case-insensitive match on
/// the name. Several matches are reported rather than guessed between, except
/// where one of them is the whole name - "Bedtime" should not be ambiguous just
/// because "Bedtime P5 Mix" also exists.
/// The household's alarms, one line each.
///
/// `RoomUUID` is resolved against the topology for a name, and left as the id
/// when it does not resolve - an alarm survives its room being switched off, and
/// hiding it would be worse than showing a raw id.
fn print_alarms(alarms: &[upnp::Alarm], groups: &Groups, json: bool) {
    let room_of = |uuid: &str| groups.player(uuid).map(|p| p.name.clone());
    if json {
        let items: Vec<_> = alarms
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "room": room_of(&a.room_uuid),
                    "room_id": a.room_uuid,
                    "start": a.start,
                    "duration_ms": a.duration_ms(),
                    "recurrence": a.recurrence,
                    "enabled": a.enabled,
                    "volume": a.volume,
                    "play_mode": a.play_mode,
                    "program": a.program_uri,
                    "include_grouped": a.include_linked_zones,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&items).expect("serializable"));
        return;
    }
    if alarms.is_empty() {
        println!("No alarms.");
        return;
    }
    for a in alarms {
        println!(
            "{:<4} {:<16} {}  {:<9} for {}  vol {:<4} {:<4} {}",
            a.id,
            room_of(&a.room_uuid).unwrap_or_else(|| a.room_uuid.clone()),
            a.start,
            a.recurrence,
            a.duration,
            a.volume,
            if a.enabled { "on" } else { "off" },
            a.program(),
        );
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
        match enqueue_item(session, room, service, &cdudn, id, title, true).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("x2rock: {title:?} would not go in the queue ({e:#}); streaming it")
            }
        }
    }
    stream_item(session, room, service, token, id, title).await
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
    play: bool,
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
    // **Deliberately does not wait**, unlike `play-url` and `stations`. This is
    // the path the bar widget takes through `play-item`, where up to ten
    // seconds before the button responds would be a worse bug than the one
    // being fixed - and the URL here was resolved by the service that owns the
    // content, so the silent failure is rare rather than routine. Revisit if
    // that turns out to be wrong; the capability is one argument away.
    let (room_name, started) =
        stream_url(session, room, &uri, title, Some(service), Duration::ZERO).await?;
    report_started(&room_name, title, Some(&service.name), &started)
}

/// How long to wait for a loaded stream to actually reach `PLAYING`.
///
/// Measured rather than picked: a stream sits in `TRANSITIONING`/`BUFFERING`
/// for several seconds first - about four for SomaFM over https, longer for
/// others - so anything under five would report a working station as broken.
/// Ten is comfortably past that. Only a *failure* spends the whole budget; the
/// happy path returns the moment it sees `PLAYING`.
const STREAM_START: Duration = Duration::from_secs(10);

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
async fn run_stations(
    ip: Option<IpAddr>,
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
        let session = session::connect(ip, &mut state).await?;
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
fn require_http_url(url: &str) -> Result<()> {
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
async fn play_audio_clip(
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
async fn run_play_url(
    ip: Option<IpAddr>,
    room: Option<&str>,
    url: &str,
    title: Option<&str>,
    no_wait: bool,
    json: bool,
) -> Result<()> {
    let name = stream_display_name(url, title)?;
    let mut state = State::load()?;
    let session = session::connect(ip, &mut state).await?;
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
    let usable = catalogue.usable(&linked);
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

/// `queue-item`: the same lookup `run_play_item` does, then enqueue without
/// playing.
async fn run_queue_item(
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
    // Without a service type there is no cdudn, and `SA_RINCONNone` is not an
    // account - the enqueue would be refused by the player with less to say.
    let Some(cdudn) = chosen.cdudn() else {
        bail!(
            "{} is not in the player's service-type list, so nothing can be \
             built to name the account that owns {title:?}.",
            chosen.name
        );
    };
    enqueue_item(&session, room, &chosen, &cdudn, id, title, false).await
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
fn queueable(item: &sonos::smapi::Item, service: &sonos::smapi::Service) -> bool {
    !item.container && !item.item_type.eq_ignore_ascii_case("stream") && service.cdudn().is_some()
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

/// Put a link page in front of the person, or print it when asked to.
///
/// A failure to open is not a failure to link: the URL is right there, and
/// printing it is the one path that matters over ssh.
fn announce_link_page(name: &str, url: &str, no_open: bool) {
    if no_open {
        println!("Open this and log in to {name}:\n\n  {url}\n");
        return;
    }
    match open_in_browser(url) {
        Ok(()) => println!("Opened {name} in your browser."),
        Err(e) => println!("Could not open a browser ({e:#}). Open this yourself:\n\n  {url}\n"),
    }
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
    from_player: bool,
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
        println!(
            "App-link services are not listed, but naming one still asks it \
             for a browser page - some hand one over, and a refusal costs \
             nothing."
        );
        return Ok(());
    };

    let chosen = catalogue.find_any(query)?.clone();
    ensure!(
        chosen.auth != sonos::smapi::Auth::Anonymous,
        "{} needs no account at all - search it as it is.",
        chosen.name
    );

    let household = session.connection.household_id().await?;

    // Plex first: its SMAPI link half is dead in both flavours - `getAppLink`
    // answers `Server.ServiceUnknownError` and `getDeviceLinkCode` answers
    // `Client.AuthTokenExpired`, whatever the credentials header carries - and
    // the token its content half wants is a plain Plex account token, which
    // Plex's own published PIN flow mints for any client. See sonos/plex.rs.
    // From out here it is the same flow as every other link: open a page,
    // wait, store.
    let (auth, link_code) = if from_player {
        ensure!(
            chosen.id == sonos::plex::SERVICE_ID,
            "--from-player is a Plex-only path: only Plex puts a usable token \
             in the players' art URLs"
        );
        // The same move as `keep`: read what the player itself built. Every
        // Plex art URL a player hands out carries the household integration's
        // token, so any room currently on Plex is a source. Metadata is
        // group-scoped, so each group is asked through its own coordinator -
        // the session's socket only answers for the group it coordinates.
        let mut token = None;
        for group in &session.groups.groups {
            let target = session::Target {
                group_id: group.id.clone(),
                name: group.name.clone(),
                coordinator_id: group.coordinator_id.clone(),
                coordinator_ip: session
                    .groups
                    .player(&group.coordinator_id)
                    .and_then(|p| p.ip()),
            };
            let Ok(connection) = session::coordinator(&session, &target).await else {
                continue;
            };
            let Ok(status) = connection.metadata(&group.id).await else {
                continue;
            };
            let urls = [
                status
                    .container
                    .as_ref()
                    .and_then(|c| c.image_url.as_deref()),
                status
                    .current_item
                    .as_ref()
                    .and_then(|i| i.track.as_ref())
                    .and_then(|t| t.image_url.as_deref()),
            ];
            token = urls.into_iter().flatten().find_map(sonos::plex::token_in);
            if token.is_some() {
                break;
            }
        }
        let Some(token) = token else {
            bail!(
                "no room is showing Plex art right now, so there is no token \
                 to read. Play something from Plex in any room, then run this \
                 again - or use `x2rock link plex` for the browser flow."
            );
        };
        let auth = sonos::smapi::DeviceAuth {
            auth_token: token,
            private_key: String::new(),
            user_id_hash_code: None,
        };
        (auth, None)
    } else if chosen.id == sonos::plex::SERVICE_ID {
        let (pin, url) = sonos::plex::pin().await?;
        announce_link_page(&chosen.name, &url, no_open);
        let deadline = tokio::time::Instant::now() + sonos::smapi::LINK_DEADLINE;
        eprint!("Waiting for you to finish");
        let token = loop {
            match sonos::plex::poll(&pin).await {
                Ok(Some(token)) => {
                    eprintln!();
                    break token;
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
        let auth = sonos::smapi::DeviceAuth {
            auth_token: token,
            private_key: String::new(),
            user_id_hash_code: None,
        };
        (auth, None)
    } else {
        // Two flavours of the same browser flow. App link is named for handing
        // off to the service's own app, but that is the controller's choice -
        // Sonos's desktop controller links without one - so the reply nests the
        // identical regUrl/linkCode pair, and a service may fill it in with a
        // real page. Asking is the only way to know, and a refusal arrives
        // immediately with the service's own words in it. The catch-all arm
        // also takes whatever parse_services could not classify, which is why
        // the advice it prints does not name a mechanism.
        let code = match chosen.auth {
            sonos::smapi::Auth::DeviceLink => {
                sonos::smapi::device_link_code(&chosen, &household).await?
            }
            _ => sonos::smapi::app_link_code(&chosen, &household).await?,
        };
        announce_link_page(&chosen.name, &code.reg_url, no_open);
        if code.show_link_code {
            println!("Enter this code when asked:\n\n  {}\n", code.link_code);
        }

        let deadline = tokio::time::Instant::now() + sonos::smapi::LINK_DEADLINE;
        eprint!("Waiting for you to finish");
        let auth = loop {
            match sonos::smapi::device_auth_token(
                &chosen,
                &household,
                &code.link_code,
                code.link_device_id.as_deref(),
            )
            .await
            {
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
        (auth, Some(code.link_code))
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
        // Quiet when there is no link code either (Plex): registering on the
        // household was never part of that flow, and the household's own
        // registration - made from the Sonos app - is what playback rides on.
        if link_code.is_some() {
            println!(
                "{} sent no userIdHashCode, so the household cannot be told about \
                 the account. Search works; playback through the household may not.",
                chosen.name
            );
        }
        return Ok(());
    };
    match session
        .connection
        .match_music_service_account(
            &household,
            &chosen.id,
            &hash,
            &nickname,
            link_code.as_deref(),
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
    let live =
        || -> Result<&session::Session> { reached.as_ref().map_err(hint::no_player_to_play) };

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
    // Everything reachable, which is wider than what `search` offers. Browsing
    // needs an endpoint and, for a linked service, a token; searching needs a
    // published search category on top of that. This comment used to say the
    // two sets were the same - Radio Paloma is the counterexample, browse-only,
    // and filtering here would have removed the one route that works for it.
    let usable = catalogue.usable(&linked);

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
                    "queueable": queueable(i, &chosen),
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
    let live =
        || -> Result<&session::Session> { reached.as_ref().map_err(hint::no_player_to_play) };

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
            // "More" means it: what could be linked and is not yet, so the
            // count stays honest once some of the linkable set is searchable.
            let linkable = catalogue
                .linkable()
                .iter()
                .filter(|s| linked.get(&s.id).is_none())
                .count();
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
    // worth the extra lookup. Only for a service that needs one: an anonymous
    // service is always searchable, so if `find` still refused it the refusal
    // is about the query - ambiguity - and saying anything about accounts
    // would answer a question nobody asked.
    let chosen = catalogue::Catalogue::find(&usable, query)
        .map_err(|e| {
            match catalogue
                .services()
                .iter()
                .find(|s| s.name.to_lowercase() == query.to_lowercase())
            {
                Some(s) if s.auth != sonos::smapi::Auth::Anonymous => s.needs_link_hint().into(),
                // A real service that `searchable` has since dropped for
                // publishing no categories. Without this arm `find` calls it
                // unmatched, which reads as a typo rather than as the fact it
                // is - and buys a `x2rock search` retry that will not help.
                Some(s) if catalogue.publishes_no_categories(&s.id) => {
                    s.no_search_categories_hint().into()
                }
                _ => e,
            }
        })?
        .clone();

    // Asked before the call, because a freshly learned *empty* list is the
    // whole reason to write here and `is_empty()` afterwards cannot tell it
    // from a cache hit. Persisting the negative is what stops the next
    // `x2rock search` listing this service as searchable again.
    let learned = !catalogue.categories_cached(&chosen.id);
    let categories = catalogue.categories_for(&chosen).await?;
    dirty |= learned;
    if dirty {
        catalogue.save()?;
    }
    let chosen = &chosen;
    // The cold-cache path to the same refusal the `find` arm above gives: on a
    // first encounter nothing had been asked, so the service was still listed
    // and `find` had no reason to object. Same hint, so the two cannot drift.
    if categories.is_empty() {
        return Err(chosen.no_search_categories_hint().into());
    }
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
                    "queueable": queueable(i, chosen),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if items.is_empty() {
        // Name the category that was searched: a service with no `all` was
        // searched in one category only (Plex defaults to artists), and
        // "nothing" without that context reads as "the service has it not"
        // when the truth may be "you searched the wrong shelf".
        let others: Vec<_> = categories
            .iter()
            .map(|c| c.id.as_str())
            .filter(|id| *id != picked.id)
            .collect();
        match others.is_empty() {
            true => println!("Nothing on {} for {term:?}.", chosen.name),
            false => println!(
                "Nothing on {} for {term:?} in {}. Also searchable: {}.",
                chosen.name,
                picked.id,
                others.join(", ")
            ),
        }
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
async fn main() {
    let cli = Cli::parse();
    // Decided before the command runs, so a failure knows how to report itself.
    let json = wants_json(&cli.command);
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

/// Set or read a room's volume, printing the outcome (JSON of it under `json`).
/// The one place volume is applied, so the single-room arm and the multi-room
/// fan-out share it - `--player` scoping, the fixed-volume refusal, mute, and
/// the report-what-was-asked rule all live here once.
async fn apply_vol(
    session: &session::Session,
    target: &session::Target,
    room: Option<&str>,
    change: Option<String>,
    one_room: bool,
    json: bool,
) -> Result<()> {
    let group = target.group_id.as_str();
    let player = session::coordinator(session, target).await?;
    // --player names the speaker, so it resolves the room asked for rather than
    // the group's name: once rooms are grouped the group is called after its
    // coordinator ("Dining Room + 1"), which is no player's name at all.
    let this = one_room
        .then(|| match room {
            Some(name) => session.groups.player_named(name),
            // No room named, so the group resolved by default; its coordinator
            // is the speaker meant. By id: the group's name ("Kitchen + 1") is
            // not a player's once grouped.
            None => session
                .groups
                .player(&target.coordinator_id)
                .ok_or_else(|| anyhow!("no player for {}", target.name)),
        })
        .transpose()?;
    // A player-scoped command is refused by anyone but that player ("Incorrect
    // playerId"), so it cannot ride the coordinator's connection.
    let speaker = match this.as_ref() {
        Some(named) => {
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
    // Name the speaker, not the group: "Dining Room + 1  22" is a confusing way
    // to report what Kitchen was set to.
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
    // Whether this was a set, not a read - so `previous_volume` is present only
    // when there was a previous, distinguishing a set (even to the same value)
    // from a read where nothing moved.
    let was_set = change.is_some();
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
            // Muting one speaker of a group is not offered: the group mute is
            // what people mean, and a silently muted member is a puzzle later.
            ensure!(this.is_none(), "--player does not apply to mute");
            player.set_group_mute(group, muted).await?;
            (before.volume, muted)
        }
    };
    if json {
        // previous_volume makes a set distinguishable from a read, and a clamp
        // (+5 at 100) or a fixed-volume refusal visible: the value did not move.
        // audible folds volume+muted into the one outcome.
        println!(
            "{}",
            json!({
                "room": label,
                "volume": level,
                "previous_volume": was_set.then_some(before.volume),
                "muted": muted,
                "audible": !muted && level > 0,
                "fixed": before.fixed,
            })
        );
    } else {
        let from = transition(&before.volume.to_string(), &level.to_string());
        let muted = if muted { "  (muted)" } else { "" };
        println!("{label:<24} {from}{level}{muted}");
    }
    Ok(())
}

/// Set or read repeat, printing the outcome. Shared by the single arm and fan-out.
async fn apply_repeat(
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
async fn apply_shuffle(
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
        let word = |on: bool| if on { "on" } else { "off" };
        let from = transition(word(before), word(after));
        println!("{:<24} shuffle {from}{}", target.name, word(after));
    }
    Ok(())
}

/// Apply one transport verb to a group, through its coordinator.
/// What one `eq` invocation asked to change; `None` means leave it alone.
///
/// A struct rather than four more parameters: they arrive together, are
/// consumed together, and travel from clap to the handler unchanged.
struct ToneRequest {
    bass: Option<i8>,
    treble: Option<i8>,
    loudness: Option<String>,
    trueplay: Option<String>,
    night: Option<String>,
    dialog: Option<String>,
}

/// Bass, treble and loudness on one speaker.
///
/// Addressed to a player, not a group: two rooms playing together each keep
/// their own tone, and the Sonos app agrees - its panel is titled "EQ Settings
/// for <room>".
async fn apply_eq(
    session: &session::Session,
    target: &session::Target,
    room: Option<&str>,
    want: ToneRequest,
    json: bool,
) -> Result<()> {
    let ToneRequest {
        bass,
        treble,
        loudness,
        trueplay,
        night,
        dialog,
    } = want;
    let speaker = match room {
        Some(name) => session.groups.player_named(name)?,
        // No room named, so the default group resolved; its coordinator is the
        // speaker meant. By id, because once grouped the group's name
        // ("Kitchen + 1") is no player's name at all.
        None => session
            .groups
            .player(&target.coordinator_id)
            .ok_or_else(|| anyhow!("no player for {}", target.name))?,
    };
    let ip = speaker
        .ip()
        .with_context(|| format!("{} did not report an address to reach it on", speaker.name))?;
    let upnp = Upnp::new(ip);

    // Both levels are checked before either is sent, so a bad treble cannot
    // leave a good bass already applied - the same partial-application care the
    // fan-out takes.
    for (what, level) in [("bass", bass), ("treble", treble)] {
        if let Some(level) = level {
            ensure!(
                upnp::TONE_RANGE.contains(&level),
                "{what} {level} is outside the {}..{} a player accepts",
                upnp::TONE_RANGE.start(),
                upnp::TONE_RANGE.end()
            );
        }
    }
    let on_off = |what: &str, text: Option<&str>| match text {
        None => Ok(None),
        Some(word @ ("on" | "off")) => Ok(Some(word == "on")),
        Some(_) => bail!("{what} takes on or off"),
    };
    let wanted_loudness = on_off("loudness", loudness.as_deref())?;
    let wanted_trueplay = on_off("trueplay", trueplay.as_deref())?;
    let wanted_night = on_off("night", night.as_deref())?;
    let wanted_dialog = on_off("dialog", dialog.as_deref())?;

    // Night mode and dialog are soundbar-only over UPnP - a non-soundbar answers
    // SetEQ with UPnP 402. Refuse up front with the reason rather than relaying
    // that opaque code, and only when actually setting one: reading them on a
    // non-soundbar already just omits them.
    let is_soundbar = speaker.capabilities.iter().any(|c| c == "HT_PLAYBACK");
    if (wanted_night.is_some() || wanted_dialog.is_some()) && !is_soundbar {
        bail!(
            "{} has no TV input, so night mode and dialog do not apply - they are soundbar settings",
            speaker.name
        );
    }

    let before = upnp.tone().await?;
    if let Some(level) = bass {
        upnp.set_bass(level).await?;
    }
    if let Some(level) = treble {
        upnp.set_treble(level).await?;
    }
    if let Some(on) = wanted_loudness {
        upnp.set_loudness(on).await?;
    }
    if let Some(on) = wanted_trueplay {
        // Refused rather than silently ignored: enabling a correction that was
        // never measured would report `trueplay on` and change nothing.
        ensure!(
            !on || before.trueplay_available,
            "{} has no room calibration to enable - measure one in the Sonos app first",
            speaker.name
        );
        upnp.set_trueplay(on).await?;
    }
    // Night and dialog over UPnP SetEQ - the Control API reads them but refuses
    // to write them. Gated to a soundbar above.
    if let Some(on) = wanted_night {
        upnp.set_eq("NightMode", on).await?;
    }
    if let Some(on) = wanted_dialog {
        upnp.set_eq("DialogLevel", on).await?;
    }
    // Read back rather than echo what was asked for: the setters answer with an
    // empty body, so what the speaker now holds is the only truthful report.
    let changed = bass.is_some()
        || treble.is_some()
        || wanted_loudness.is_some()
        || wanted_trueplay.is_some();
    let after = if changed { upnp.tone().await? } else { before };

    // Night mode and dialog enhancement, read over the Control API - the one
    // path that carries them, and it reflects a UPnP SetEQ write immediately
    // (verified 2026-09-05), so the read-back after a write is truthful.
    // Soundbars only: the block is returned on every player but inert on
    // anything without a TV input, and reporting `night off` on a One SL would
    // imply a control it does not have. Best-effort: a tone read must not fail
    // because this secondary read did.
    let home_theater = if is_soundbar {
        let control = if ip == session.connection.ip() {
            session.connection.clone()
        } else {
            Connection::open(ip).await?
        };
        control
            .player_settings(&speaker.id)
            .await
            .ok()
            .and_then(|s| s.home_theater)
    } else {
        None
    };

    if json {
        let mut out = json!({
            "room": speaker.name,
            "bass": after.bass,
            "treble": after.treble,
            "loudness": after.loudness,
            "trueplay": after.trueplay,
            // Whether there is a calibration at all. `trueplay` alone
            // cannot be read as "this room is corrected".
            "trueplay_available": after.trueplay_available,
        });
        // Present only for a soundbar, so their absence is "not a soundbar"
        // rather than "off" - the same reason the prose omits them.
        if let Some(ht) = &home_theater {
            out["night_mode"] = json!(ht.night_mode);
            out["dialog_enhancement"] = json!(ht.enhance_dialog);
            out["dialog_level"] = json!(ht.enhance_dialog_level);
        }
        println!("{out}");
    } else {
        let word = |on: bool| if on { "on" } else { "off" };
        // "unavailable" rather than "off" when there is nothing measured: the
        // two are different answers to "is this room corrected?".
        let calibration = if after.trueplay_available {
            format!(
                "{}{}",
                transition(word(before.trueplay), word(after.trueplay)),
                word(after.trueplay)
            )
        } else {
            "unavailable".to_string()
        };
        // Night mode and dialog only for a soundbar, appended so the tone line
        // reads the same everywhere else. Dialog shows its level when enhanced,
        // since the setting is a level and "on" alone loses it.
        let ht = match &home_theater {
            Some(ht) => {
                let dialog = if ht.enhance_dialog && ht.enhance_dialog_level > 0 {
                    format!("on ({})", ht.enhance_dialog_level)
                } else {
                    word(ht.enhance_dialog).to_string()
                };
                format!("  night {}  dialog {dialog}", word(ht.night_mode))
            }
            None => String::new(),
        };
        println!(
            "{:<24} bass {}{}  treble {}{}  loudness {}{}  trueplay {calibration}{ht}",
            speaker.name,
            transition(&before.bass.to_string(), &after.bass.to_string()),
            after.bass,
            transition(&before.treble.to_string(), &after.treble.to_string()),
            after.treble,
            transition(word(before.loudness), word(after.loudness)),
            word(after.loudness),
        );
    }
    Ok(())
}

/// `HH:MM` or `HH:MM:SS` as the alarm service wants it: `HH:MM:SS`.
///
/// Padded rather than reformatted loosely, because the player takes the string
/// as given - `7:00` is refused where `07:00:00` is not.
fn parse_time_of_day(text: &str) -> Result<String> {
    let bad = || anyhow!("{text:?} is not a time of day - try 07:00 or 07:00:00");
    let parts: Option<Vec<u32>> = text.trim().split(':').map(|p| p.parse().ok()).collect();
    let (h, m, sec) = match parts.as_deref() {
        Some([h, m]) => (*h, *m, 0),
        Some([h, m, s]) => (*h, *m, *s),
        _ => return Err(bad()),
    };
    ensure!(h < 24 && m < 60 && sec < 60, "{text:?} is not a real time");
    Ok(format!("{h:02}:{m:02}:{sec:02}"))
}

/// A sleep-timer duration as someone would type it; `None` means cancel.
///
/// The player accepts `HH:MM:SS` and nothing else, so this is where `30m`
/// becomes something it will take. Bare digits are **minutes**, because that is
/// what "sleep 30" means to everyone who types it.
fn parse_sleep(text: &str) -> Result<Option<std::time::Duration>> {
    let raw = text.trim().to_lowercase();
    if matches!(raw.as_str(), "off" | "cancel" | "none" | "0") {
        return Ok(None);
    }
    let bad = || anyhow!("{text:?} is not a duration - try 30m, 1h30m, 90s or 00:30:00");
    let secs = if raw.contains(':') {
        // The wire's own form, taken as-is so a value read back can be handed
        // straight back without conversion.
        let parts: Option<Vec<u64>> = raw.split(':').map(|p| p.parse().ok()).collect();
        match parts.as_deref() {
            Some([h, m, s]) => h * 3600 + m * 60 + s,
            Some([m, s]) => m * 60 + s,
            _ => return Err(bad()),
        }
    } else if raw.chars().all(|c| c.is_ascii_digit()) {
        raw.parse::<u64>().map_err(|_| bad())? * 60
    } else {
        let mut total = 0u64;
        let mut digits = String::new();
        for c in raw.chars() {
            if c.is_ascii_digit() {
                digits.push(c);
                continue;
            }
            let n: u64 = digits.parse().map_err(|_| bad())?;
            total += n * match c {
                'h' => 3600,
                'm' => 60,
                's' => 1,
                _ => return Err(bad()),
            };
            digits.clear();
        }
        // A trailing number with no unit sits ambiguously next to the units
        // before it, so it is refused rather than guessed at.
        if !digits.is_empty() {
            return Err(bad());
        }
        total
    };
    ensure!(secs > 0, "a timer of no time is `x2rock sleep off`");
    // HH:MM:SS carries two digits of hours, and the player has no use for more.
    ensure!(
        secs < 24 * 3600,
        "{text:?} is longer than a day, which the wire cannot carry"
    );
    Ok(Some(std::time::Duration::from_secs(secs)))
}

/// `H:MM:SS` past an hour, `M:SS` under it.
fn hms_short(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// The group's sleep timer, read or set.
async fn apply_sleep(
    target: &session::Target,
    player_ip: IpAddr,
    duration: Option<String>,
    json: bool,
) -> Result<()> {
    // AVTransport answers for the group on its coordinator, the way the queue
    // and the TV input do.
    let upnp = Upnp::new(target.coordinator_ip.unwrap_or(player_ip));
    let wanted = duration.as_deref().map(parse_sleep).transpose()?;
    if let Some(after) = wanted {
        upnp.set_sleep_timer(after).await?;
    }
    // Read back rather than echo what was asked: the player starts counting
    // from the moment it accepted, so its own number is already the honest one.
    let left = upnp.sleep_timer().await?;

    if json {
        println!(
            "{}",
            json!({
                "room": target.name,
                "sleep_ms": left.map(|d| d.as_millis()),
            })
        );
    } else {
        match left {
            Some(d) => println!("{:<24} sleep {}", target.name, hms_short(d)),
            None => println!("{:<24} no sleep timer", target.name),
        }
    }
    Ok(())
}

/// Crossfade, which is a play mode like shuffle and set the same way.
async fn apply_crossfade(
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
        let word = |on: bool| if on { "on" } else { "off" };
        let from = transition(word(before), word(after));
        println!("{:<24} crossfade {from}{}", target.name, word(after));
    }
    Ok(())
}

async fn apply_transport(
    session: &session::Session,
    target: &session::Target,
    verb: &str,
) -> Result<()> {
    let coordinator = session::coordinator(session, target).await?;
    coordinator.playback(&target.group_id, verb).await
}

/// Fan a per-room command across several `--room`, topology resolved once. Only
/// the per-room-state commands accept it; anything else is refused with a clear
/// message rather than silently acting on the first room. A failure on one room
/// stops the run - a half-applied "set them all to 10" is worse than a clear
/// stop naming the room that failed.
async fn fan_out(session: &session::Session, rooms: &[String], command: &Command) -> Result<()> {
    for name in rooms {
        let target = session::target(&session.groups, Some(name))?;
        let outcome = match command {
            Command::Vol {
                change,
                player: one_room,
                json,
                ..
            } => {
                apply_vol(
                    session,
                    &target,
                    Some(name),
                    change.clone(),
                    *one_room,
                    *json,
                )
                .await
            }
            Command::Repeat { mode, json } => {
                apply_repeat(session, &target, mode.clone(), *json).await
            }
            Command::Shuffle { mode, json } => {
                apply_shuffle(session, &target, mode.clone(), *json).await
            }
            Command::Play { track: None } => apply_transport(session, &target, "play").await,
            Command::Pause => apply_transport(session, &target, "pause").await,
            Command::Toggle => apply_transport(session, &target, "togglePlayPause").await,
            Command::Next => apply_transport(session, &target, "skipToNextTrack").await,
            Command::Prev => apply_transport(session, &target, "skipToPreviousTrack").await,
            _ => Err(too_many_rooms()),
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

/// Whether a command applies per room, so several `--room` fan it out. The
/// read/whole-household and single-target commands do not.
fn fans_out(command: &Command) -> bool {
    matches!(
        command,
        Command::Vol { .. }
            | Command::Repeat { .. }
            | Command::Shuffle { .. }
            | Command::Crossfade { .. }
            | Command::Play { track: None }
            | Command::Pause
            | Command::Toggle
            | Command::Next
            | Command::Prev
    )
}

/// The agent skill, embedded so it ships with the binary and cannot drift from
/// the CLI it documents. Written to disk, or printed, by `x2rock skill`.
const SKILL: &str = include_str!("../skills/x2rock/SKILL.md");

/// Where `x2rock skill` writes, absent `--dir`: `$CLAUDE_CONFIG_DIR/skills` when
/// that is set (Claude Code honours it), else `~/.claude/skills`.
fn default_skills_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("skills"));
    }
    let home = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("no home directory to find ~/.claude in; pass --dir"))?
        .home_dir()
        .to_path_buf();
    Ok(home.join(".claude").join("skills"))
}

/// `x2rock skill`: drop the embedded skill into a Claude skills directory (or
/// print it). Needs no network - it is a local file write.
fn install_skill(dir: Option<&std::path::Path>, print: bool) -> Result<()> {
    if print {
        print!("{SKILL}");
        return Ok(());
    }
    let base = match dir {
        Some(d) => d.to_path_buf(),
        None => default_skills_dir()?,
    };
    let target = base.join("x2rock");
    std::fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
    let path = target.join("SKILL.md");
    std::fs::write(&path, SKILL).with_context(|| format!("writing {}", path.display()))?;
    println!("Wrote the x2rock skill to {}.", path.display());
    println!("A Claude assistant on this machine will pick it up for Sonos tasks.");
    Ok(())
}

/// Whether the invoked command was asked for `--json`, so an error can match the
/// output the caller expected. Only the data commands carry the flag.
fn wants_json(command: &Command) -> bool {
    matches!(
        command,
        Command::Rooms { json, .. }
            | Command::Now { json, .. }
            | Command::Status { json, .. }
            | Command::Queue { json, .. }
            | Command::Favorites { json, .. }
            | Command::PlayUrl { json, .. }
            | Command::Stations { json, .. }
            | Command::Search { json, .. }
            | Command::Browse { json, .. }
            | Command::Accounts { json, .. }
            | Command::Bookmarks { json, .. }
            | Command::Vol { json, .. }
            | Command::Repeat { json, .. }
            | Command::Shuffle { json, .. }
            | Command::Crossfade { json, .. }
        if *json
    )
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
    if cli.all && !matches!(cli.command, Command::Bookmarks { .. }) {
        ensure!(
            fans_out(&cli.command),
            "--all applies only to the per-room commands (volume, transport, repeat, shuffle)"
        );
    }
    match cli.command {
        Command::Discover => return discover_and_remember(&mut State::load()?).await,
        Command::Skill { ref dir, print } => return install_skill(dir.as_deref(), print),
        Command::PlayItem {
            ref service,
            ref id,
            ref title,
            ref kind,
        } => {
            return run_play_item(cli.ip, room, service, kind.as_deref(), id, title.as_ref()).await;
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
            return run_stations(
                cli.ip,
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
            return run_play_url(cli.ip, room, url, title.as_deref(), no_wait, json).await;
        }
        Command::QueueItem {
            ref service,
            ref id,
            ref title,
            ref kind,
        } => {
            return run_queue_item(cli.ip, room, service, kind.as_deref(), id, title.as_ref())
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
                room,
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
            from_player,
        } => {
            return run_link(
                cli.ip,
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
        Command::Accounts { household, json } => {
            let linked = credentials::Credentials::load()?;
            // Only `--household` reaches the network, so the default keeps the
            // promise made above: this command reads a file on this machine.
            let serials = if household {
                let mut state = State::load()?;
                let session = session::connect(cli.ip, &mut state).await?;
                // Favorites are household-wide, so any player answers for the
                // half that matters, and demanding --room to read them would be
                // a question with no bearing on the answer. A room is honoured
                // when given - it picks whose queue is read - and otherwise the
                // first reachable player serves.
                let ip = match room {
                    Some(_) => {
                        let target = session::target(&session.groups, room)?;
                        target
                            .coordinator_ip
                            .ok_or_else(|| anyhow!("no address for {}", target.name))?
                    }
                    None => session
                        .groups
                        .players
                        .iter()
                        .find_map(Player::ip)
                        .ok_or_else(|| anyhow!("no player with a known address"))?,
                };
                let upnp = Upnp::new(ip);
                let mut found = std::collections::BTreeSet::new();
                // Favorites are household-wide; the queue is this coordinator's.
                // Neither is the account list - see `serials_in`.
                for object in ["FV:2", "Q:0"] {
                    found.extend(upnp::serials_in(&upnp.browse_content(object).await?));
                }
                Some(found)
            } else {
                None
            };
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
                let out = match &serials {
                    Some(found) => json!({
                        "linked": rows,
                        // Named exactly what it is. A consumer that reads this
                        // as the household's accounts will be wrong in both
                        // directions - see `upnp::serials_in`.
                        "serials_named_by_content": found
                            .iter()
                            .map(|(sid, sn)| json!({ "service_id": sid, "account": sn }))
                            .collect::<Vec<_>>(),
                    }),
                    None => json!(rows),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                if linked.services.is_empty() {
                    println!("No accounts linked. Run `x2rock link` to see what can be.");
                } else {
                    for (id, a) in &linked.services {
                        // `account_id` is set only when *this machine's* `match`
                        // succeeded, which has never happened. Saying "not
                        // registered on the household" read as a fact about the
                        // household, which this file cannot know: the household
                        // may hold several accounts for the service already.
                        let registered = match &a.account_id {
                            Some(account) => format!("registered from here as {account}"),
                            None => "no registration from this machine".to_string(),
                        };
                        println!(
                            "{:<20} {:<10} {:<12} {registered}",
                            a.service_name,
                            id,
                            ago(a.linked)
                        );
                    }
                }
                if let Some(found) = &serials {
                    let catalogue = catalogue::Catalogue::load();
                    println!();
                    if found.is_empty() {
                        println!("No serials named by this household's favorites or queue.");
                    } else {
                        println!("Serials named by this household's favorites and queue:");
                        // By serial, which is the order they were created in.
                        // Sorting by service id puts "6" after "333".
                        let mut rows: Vec<_> = found.iter().collect();
                        rows.sort_by_key(|(_, sn)| sn.parse::<u64>().unwrap_or(u64::MAX));
                        for (sid, sn) in rows {
                            let name = catalogue
                                .services()
                                .iter()
                                .find(|s| &s.id == sid)
                                .map(|s| s.name.clone())
                                .unwrap_or_else(|| "not in the catalogue".to_string());
                            println!("  sn_{sn:<4} {name:<24} sid {sid}");
                        }
                    }
                    println!();
                    println!(
                        "Not the household's account list. A serial stays here after its account"
                    );
                    println!(
                        "is removed, and an account that has only played a station never appears."
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
                room,
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

    // Like `rooms`, this is a whole-household view and must not be forced to a
    // single group; it queries every coordinator itself, so it runs here rather
    // than after the single-room resolution below.
    if let Command::Status { json, full } = cli.command {
        return print_status(&session, json, full).await;
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
        let upnp = Upnp::new(session.connection.ip());
        match action {
            None => {
                let alarms = upnp.alarms().await?;
                print_alarms(&alarms, &session.groups, *json);
            }
            Some(AlarmsAction::Add {
                time,
                duration,
                recurrence,
                volume,
                program,
                play_mode,
                grouped,
                off,
            }) => {
                // The alarm belongs to a speaker, so the room resolves to a
                // player rather than to the group it happens to play with.
                let speaker = match room {
                    Some(name) => session.groups.player_named(name)?,
                    // An alarm belongs to exactly one speaker, so there is no
                    // defensible default past a one-speaker household: guessing
                    // would put it in a room nobody asked to be woken in.
                    None => match session.groups.players.as_slice() {
                        [only] => only,
                        _ => bail!(
                            "which room? an alarm belongs to one speaker - pass --room. \
                             Rooms: {}",
                            session.groups.room_names()
                        ),
                    },
                };
                let start = parse_time_of_day(time)?;
                let plays = parse_sleep(duration)?
                    .ok_or_else(|| anyhow!("an alarm that plays for no time is not an alarm"))?;
                ensure!(*volume <= 100, "volume is 0-100, not {volume}");
                // The program: a favorite or playlist resolved to the same
                // (uri, metadata) pair `queue add` uses, or the built-in chime.
                let (uri, metadata) = match program {
                    None => ("x-rincon-buzzer:0".to_string(), String::new()),
                    Some(query) => {
                        let mut sources = upnp.browse_content("SQ:").await?;
                        sources.extend(upnp.browse_content("FV:2").await?);
                        sources.retain(|item| !item.shortcut);
                        let item = find_content(&sources, query)?;
                        let uri = item
                            .uri
                            .as_deref()
                            .with_context(|| format!("{:?} has nothing to play", item.title))?;
                        (uri.to_string(), item.metadata.clone())
                    }
                };
                let secs = plays.as_secs();
                let alarm = upnp::Alarm {
                    id: 0,
                    start,
                    duration: format!(
                        "{:02}:{:02}:{:02}",
                        secs / 3600,
                        (secs / 60) % 60,
                        secs % 60
                    ),
                    recurrence: recurrence.to_uppercase(),
                    enabled: !off,
                    room_uuid: speaker.id.clone(),
                    program_uri: uri,
                    program_metadata: metadata,
                    play_mode: play_mode.to_uppercase(),
                    volume: *volume,
                    include_linked_zones: *grouped,
                };
                let id = upnp.create_alarm(&alarm).await?;
                // The time is local *to the household*, which is not
                // necessarily local to whoever typed it. Said on stderr so it
                // stays out of anything reading the result, and always - a
                // clock that agrees is worth confirming too.
                if let Ok((clock, zone)) = upnp.household_time().await {
                    if zone < 0 {
                        eprintln!(
                            "note: this household has no timezone set, so {} is UTC. \
                             Its clock reads {clock}.",
                            alarm.start
                        );
                    } else {
                        eprintln!(
                            "note: alarm times are the household's; its clock reads {clock}."
                        );
                    }
                }
                println!(
                    "alarm {id} created  {:<16} {}  {}  for {}  vol {}  {}",
                    speaker.name,
                    alarm.start,
                    alarm.recurrence,
                    alarm.duration,
                    alarm.volume,
                    if alarm.enabled { "on" } else { "off" },
                );
            }
        }
        return Ok(());
    }

    if let Command::Alarm { id, action } = &cli.command {
        let upnp = Upnp::new(session.connection.ip());
        let alarms = upnp.alarms().await?;
        let alarm = alarms
            .iter()
            .find(|a| a.id == *id)
            .ok_or_else(|| anyhow!("no alarm with id {id}. `x2rock alarms` lists them."))?;
        match action {
            AlarmAction::Remove { yes } => {
                ensure!(
                    *yes,
                    "removing alarm {id} cannot be undone, and only the Sonos app can make a \
                     new one - pass --yes"
                );
                upnp.destroy_alarm(*id).await?;
                println!("alarm {id} removed");
            }
            wanted => {
                let enabled = matches!(wanted, AlarmAction::On);
                let word = |on: bool| if on { "on" } else { "off" };
                if alarm.enabled != enabled {
                    // The whole record goes back, not just this field:
                    // UpdateAlarm refuses a partial one with UPnP 402.
                    let mut updated = alarm.clone();
                    updated.enabled = enabled;
                    upnp.update_alarm(&updated).await?;
                }
                println!(
                    "alarm {id} {}{}",
                    transition(word(alarm.enabled), word(enabled)),
                    word(enabled)
                );
            }
        }
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

    // Kept items are x2rock's own and live on this machine, so the *data* needs
    // no household. The command still does, which the older version of this
    // comment claimed it did not: `session::connect` above is unconditional, so
    // `bookmarks` on a network with no remembered players fails with "no players
    // remembered" before ever reaching here. Found by trying to list an empty
    // store in a scratch `XDG_STATE_HOME`, which had no `networks.json` either.
    //
    // Left as it is rather than hoisted above the connect. Doing that would buy
    // offline listing at the price of a second place deciding which commands are
    // local-only, and this is a single-user install where the case does not come
    // up. The comment is corrected instead of the behaviour, so the next reader
    // is not misled about what actually runs first.
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
        json,
        ..
    } = &cli.command
    {
        // One group only: --each already means "every member here", so
        // spreading it over --all's groups or several --room is a second axis
        // that would only muddy what it does.
        ensure!(!cli.all, "--each acts on one group; drop --all");
        ensure!(
            cli.room.len() <= 1,
            "--each acts on one group; name a single --room"
        );
        // Refused up front, not left to surface per member as a confusing
        // "--player does not apply to mute": muting each speaker is not what
        // --each is for, and group mute is what mute means.
        if matches!(
            change.as_deref().map(parse_volume).transpose()?,
            Some(VolumeChange::Mute(_))
        ) {
            bail!("--each does not apply to mute; mute is group-wide");
        }
        let target = session::target(&session.groups, room)?;
        let members: Vec<String> = session
            .groups
            .group_of(&target.coordinator_id)
            .map(|g| session.groups.members(g))
            .unwrap_or_default()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        // Each member addressed as its own speaker: `--player`, not the group.
        let per_member = Command::Vol {
            change: change.clone(),
            player: true,
            each: false,
            json: *json,
        };
        return fan_out(&session, &members, &per_member).await;
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
        Command::Playlist { query } => {
            let household = session.connection.household_id().await?;
            let saved = session.connection.playlists(&household).await?;
            let playlist = find_named(
                &saved.playlists,
                &query,
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
        }
        Command::Tv => {
            // The soundbar is the player with the HDMI socket, which is not
            // necessarily the one coordinating the group it is in. The room
            // named is asked first; otherwise (or when the widget names the
            // group by its coordinator) it is whichever member has one.
            let is_soundbar = |p: &&Player| p.capabilities.iter().any(|c| c == "HT_PLAYBACK");
            let members = session.groups.members(session.groups.resolve(room)?);
            let named = match room {
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
        Command::Chime { volume } => {
            play_audio_clip(&session, &target, room, None, volume).await?;
        }
        Command::Notify { url, volume } => {
            require_http_url(&url)?;
            play_audio_clip(&session, &target, room, Some(&url), volume).await?;
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
        Command::Repeat { mode, json } => apply_repeat(&session, &target, mode, json).await?,
        Command::Shuffle { mode, json } => apply_shuffle(&session, &target, mode, json).await?,
        Command::Crossfade { mode, json } => apply_crossfade(&session, &target, mode, json).await?,
        Command::Sleep { duration, json } => {
            apply_sleep(&target, player.ip(), duration, json).await?
        }
        Command::Pause => player.playback(group, "pause").await?,
        Command::Toggle => player.playback(group, "togglePlayPause").await?,
        Command::Next => player.playback(group, "skipToNextTrack").await?,
        Command::Prev => player.playback(group, "skipToPreviousTrack").await?,
        Command::Vol {
            change,
            player: one_room,
            json,
            ..
        } => apply_vol(&session, &target, room, change, one_room, json).await?,
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
        | Command::Skill { .. }
        | Command::Daemon => unreachable!("handled above"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_time_of_day_is_padded_to_what_the_player_takes() {
        // The player takes the string as given: `7:00` is refused where
        // `07:00:00` is not, so padding is the whole job.
        assert_eq!(parse_time_of_day("7:00").unwrap(), "07:00:00");
        assert_eq!(parse_time_of_day("07:00").unwrap(), "07:00:00");
        assert_eq!(parse_time_of_day("06:30:15").unwrap(), "06:30:15");
        assert_eq!(parse_time_of_day(" 23:59 ").unwrap(), "23:59:00");

        assert!(parse_time_of_day("25:00").is_err(), "no 25th hour");
        assert!(parse_time_of_day("07:60").is_err(), "no 60th minute");
        // An hour alone is ambiguous and a 12-hour clock is not parsed, so
        // both are refused rather than guessed into a wrong time of day.
        assert!(parse_time_of_day("7").is_err());
        assert!(parse_time_of_day("7pm").is_err());
    }

    #[test]
    fn a_sleep_duration_is_read_the_way_people_type_it() {
        let secs = |text: &str| parse_sleep(text).unwrap().map(|d| d.as_secs());
        // Bare digits are minutes: "sleep 30" means half an hour to everyone
        // who types it, and seconds to nobody.
        assert_eq!(secs("30"), Some(1800));
        assert_eq!(secs("45m"), Some(2700));
        assert_eq!(secs("2h"), Some(7200));
        assert_eq!(secs("1h30m"), Some(5400));
        assert_eq!(secs("90s"), Some(90));
        // The wire's own form goes through untouched, so a value read back can
        // be handed straight back.
        assert_eq!(secs("00:30:00"), Some(1800));
        assert_eq!(secs("1:00:00"), Some(3600));
        assert_eq!(secs("5:00"), Some(300));
        // Cancelling has several spellings because all of them get typed.
        for off in ["off", "cancel", "none", "0", " OFF "] {
            assert_eq!(secs(off), None, "{off:?} should cancel");
        }
    }

    #[test]
    fn a_sleep_duration_refuses_what_it_cannot_mean() {
        // A trailing number after units is ambiguous - is "1h30" thirty
        // minutes or thirty seconds? - so it is refused rather than guessed.
        assert!(parse_sleep("1h30").is_err());
        assert!(parse_sleep("later").is_err());
        assert!(parse_sleep("30x").is_err());
        assert!(parse_sleep("").is_err());
        // The cap is what HH:MM:SS can carry, so 23:59:59 is the last valid
        // value and a whole day is already too long.
        assert!(parse_sleep("1439").is_ok(), "23h59m fits");
        assert!(parse_sleep("1440").is_err(), "24h exactly does not");
        assert!(parse_sleep("25h").is_err());
    }

    #[test]
    fn a_sleep_remaining_reads_as_a_clock() {
        use std::time::Duration;
        assert_eq!(hms_short(Duration::from_secs(1800)), "30:00");
        assert_eq!(hms_short(Duration::from_secs(59)), "0:59");
        // Past an hour it grows a field rather than counting to 90 minutes.
        assert_eq!(hms_short(Duration::from_secs(5400)), "1:30:00");
        assert_eq!(hms_short(Duration::from_secs(3600)), "1:00:00");
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
        assert!(parse_range("").is_err());
        assert!(parse_range("nine").is_err());
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
    fn find_named_disambiguates_two_of_the_same_name() {
        let items = [
            ("fv1".to_string(), "That Christmas Channel".to_string()),
            ("fv2".to_string(), "That Christmas Channel".to_string()),
            ("fv7".to_string(), "Jazz24".to_string()),
        ];
        fn id(i: &(String, String)) -> &str {
            i.0.as_str()
        }
        fn name(i: &(String, String)) -> &str {
            i.1.as_str()
        }
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

    #[test]
    fn each_parses_and_will_not_pair_with_player() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["x2rock", "-r", "Living Room", "vol", "30", "--each"])
            .expect("--each is a valid flag");
        assert!(matches!(
            cli.command,
            Command::Vol {
                each: true,
                player: false,
                ..
            }
        ));
        // One speaker vs every speaker: clap refuses the pair rather than
        // letting the two per-speaker modes both claim the command.
        assert!(
            Cli::try_parse_from([
                "x2rock",
                "-r",
                "Living Room",
                "vol",
                "30",
                "--each",
                "--player"
            ])
            .is_err()
        );
    }

    #[test]
    fn eq_takes_the_soundbar_night_and_dialog_flags() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "x2rock", "-r", "Guest TV", "eq", "--night", "on", "--dialog", "off",
        ])
        .expect("--night/--dialog are valid eq flags");
        assert!(matches!(
            cli.command,
            Command::Eq { night: Some(ref n), dialog: Some(ref d), .. } if n == "on" && d == "off"
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

    #[test]
    fn the_embedded_skill_carries_its_frontmatter_and_contracts() {
        // include_str! guarantees the file exists at build time; this guards its
        // shape - the frontmatter a skill needs, and the two contracts the skill
        // exists to teach, so an edit cannot quietly drop them.
        assert!(
            SKILL.starts_with("---\nname: x2rock\n"),
            "needs skill frontmatter"
        );
        assert!(
            SKILL.contains("description:"),
            "needs a description to be discovered"
        );
        assert!(
            SKILL.contains("x2rock status --json"),
            "should teach the status snapshot"
        );
        assert!(
            SKILL.contains("unregistered_network"),
            "should teach the error codes"
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
    /// names the six fields only the latter has. Both directions are pinned:
    /// nothing group- or volume-shaped leaks into `now`, and a status entry adds
    /// nothing beyond those six.
    #[test]
    fn a_status_entry_is_a_now_entry_plus_exactly_six_room_facts() {
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
