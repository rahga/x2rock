//! The command line as clap sees it: `Cli`, `Command` and every subcommand
//! enum, plus the one question `main` asks of a parsed command before running
//! it - whether it was asked for `--json`. Nothing here runs anything; the
//! dispatcher is `run` in `main.rs` and the handlers live under `commands/`.
//!
//! Every `///` on a variant or field is `--help` text, so this file is moved
//! and edited as prose as much as code.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "x2rock", version = crate::VERSION, about = "Local-first Sonos control")]
pub struct Cli {
    /// Room to control. Not needed when the household has a single group.
    /// Repeatable for the per-room commands (volume, transport, repeat,
    /// shuffle, crossfade): `-r Kitchen -r Bedroom vol 10` applies to each, topology
    /// resolved once. Other commands take a single `--room`.
    #[arg(long, short = 'r', global = true, env = "X2ROCK_ROOM")]
    pub room: Vec<String>,

    /// Apply a per-room command to every room, topology resolved once - "turn
    /// it down everywhere" as `--all vol -10`. Only the per-room commands
    /// (volume, transport, repeat, shuffle, crossfade); exclusive with a typed `--room`,
    /// while an exported X2ROCK_ROOM is simply set aside.
    // Not `conflicts_with = "room"`: clap fires that on the env var exactly as
    // on a typed `-r`, which made `--all vol -10` a usage error in every shell
    // that had taken `x2rock rooms` up on its `export`. `main` tells the two
    // apart and `run` refuses the typed one.
    #[arg(long, global = true)]
    pub all: bool,

    /// Address of a player, bypassing what is remembered for this network.
    #[arg(long, short = 'i', global = true, env = "X2ROCK_PLAYER")]
    pub ip: Option<IpAddr>,

    /// Which Sonos household to use, when more than one is reachable on this
    /// network - an office running two systems, a guest property on the same
    /// LAN - **and --room has not already said**: a room name picks its own
    /// household, so this is only needed when a command names no room (the
    /// daemon, `link`, `accounts`) or when the room's name exists in more than
    /// one household. Any room name belonging to it, or (for that collision) a
    /// household id from `x2rock households`. Ignored on a network with a
    /// single household, which is every ordinary home; ignored entirely
    /// alongside --ip, which already names one player unambiguously.
    #[arg(long, global = true, env = "X2ROCK_HOUSEHOLD")]
    pub household: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
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
    Play { track: Option<u32> },
    /// Pause playback.
    Pause,
    /// Play if paused, pause if playing.
    Toggle,
    /// Skip to the next track.
    Next,
    /// Skip to the previous track.
    Prev,
    /// Rate the currently playing track up or down, on services that offer it
    /// (Pandora-style radio, iHeartRadio's Custom Stations) - refused on
    /// anything else, including a Live broadcast, which has no per-track
    /// identity to rate at all (verified against a real household,
    /// 2026-09-12: a Live station's current track carries no id whatsoever,
    /// not merely an unratable one).
    Rate {
        #[arg(value_enum)]
        direction: RateDirection,
        /// Re-read the service catalogue even if its version has not moved.
        ///
        /// The way out of a cached "publishes no ratings". Whether a service
        /// offers ratings is learned from its presentation map and cached, and
        /// that cache is otherwise cleared only when the *player's*
        /// service-list version moves - which a service turning the feature on
        /// does not touch. Without this, one trimmed or ratings-less reply
        /// would be believed indefinitely.
        #[arg(long)]
        refresh: bool,
        /// `{service, rating, should_skip, skipped, message}` on success.
        /// `should_skip` is the service's live answer; `skipped` is whether
        /// the room actually advanced - the two can disagree if the skip
        /// itself failed, which does not undo an already-landed rating.
        #[arg(long)]
        json: bool,
    },
    /// Show or change volume: a level (0-100), a change (+5, -5), mute/unmute,
    /// or normalize - every speaker in a group set to the group's level, the
    /// Sonos app's "Normalize Group Volume".
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
        /// Slide to the new level over several seconds instead of jumping.
        ///
        /// **Slides one speaker at a time, and implies `--player`** - there is
        /// no such thing as ramping a group, because the group volume service
        /// publishes no ramp action. It still composes with the fan-outs that
        /// are themselves over speakers: several `--room`, and `--each` for
        /// every member of one group. Only `--all` is refused, because that
        /// fans over group coordinators and would slide one speaker per group
        /// while reporting it as the group.
        ///
        /// Roughly a second and a half per ten steps, and the room is left at
        /// the new level. Not for mute, which is not a level to slide to.
        #[arg(long)]
        ramp: bool,
        /// The resulting `{room, volume, muted, fixed}` as JSON - for reading it
        /// or for confirming a change. With `--ramp`, a `ramp_seconds` beside
        /// them: the player's own estimate, which runs a little long. A group
        /// read adds `balanced`, whether every member is at the group's level;
        /// `normalize` adds `members`, each `{room, volume, previous_volume}`.
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
    /// What the household's portables say about their batteries: charge,
    /// health, temperature and what they are drawing power from.
    ///
    /// One of the few things the cloud Control API has never exposed, so it is
    /// read where it lives - each player's own `/status/batterystatus` page.
    /// Mains-powered speakers are skipped rather than listed as having no
    /// battery, which would be most of a household.
    ///
    /// **A portable that has gone completely flat is not on the network**, and
    /// so cannot be listed here at all. That absence is a diagnosis in itself.
    Battery {
        /// Ask one room rather than sweeping the household.
        #[arg(long, short = 'r')]
        room: Option<String>,
        /// One object per player: `room`, `battery`, and where there is one,
        /// `level`, `health`, `temperature`, `power_source` and `charging`.
        /// Mains speakers are included here, as `battery: false`.
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
    /// listing them and `on`/`off`/`remove` take no --room; `add` names the
    /// speaker that will ring with --room. x2rock can create, arm, disarm and
    /// remove them.
    Alarms {
        #[command(subcommand)]
        action: Option<AlarmsAction>,
        /// The list as JSON: `{id, room, room_id, start, duration_ms,
        /// recurrence, enabled, volume, play_mode, program, include_grouped}`
        /// per alarm. `room_id` is the raw `RINCON_...` uuid, which is what
        /// names the room when `room` is null because the speaker is off the
        /// network - and which embeds that speaker's MAC, so strip it before
        /// pasting output anywhere public. With `add`, the created alarm as
        /// one such object - its `id` is what `x2rock alarm <id> off` wants
        /// later.
        // Global, so `alarms add --json` is this flag and not a usage error.
        #[arg(long, global = true)]
        json: bool,
    },
    /// Turn an alarm on or off, or remove it, by id from `x2rock alarms`.
    Alarm {
        id: u32,
        #[command(subcommand)]
        action: AlarmAction,
    },
    /// Silence the alarm that is going off, for a while.
    ///
    /// The answer to the one thing the `alarm` subcommands cannot do. Turning
    /// an alarm off stops it scheduling again and removing it deletes it, but
    /// **neither stops the one already playing** - only this and `pause` do,
    /// and this is the one that brings it back.
    ///
    /// Addressed per group, like transport, and it acts rather than reads:
    /// when an alarm is waking the house at 07:00 the useful thing to type is
    /// one word.
    Snooze {
        /// How long to silence it: `9`, `9m`, `1h`, or `HH:MM:SS`. Bare digits
        /// are minutes. Defaults to 9 minutes, which is what a clock radio has
        /// meant by snooze since the 1950s.
        duration: Option<String>,
        /// The resulting `{room, alarm_id, snoozed_ms}` as JSON.
        #[arg(long)]
        json: bool,
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
    /// Show or set a soundbar's TV-remote settings.
    ///
    /// Two things, both about the bar's relationship with the TV remote rather
    /// than with music: whether it flashes its light to acknowledge a remote
    /// command, and whether it passes the remote's infrared through to the TV
    /// sitting behind it - the setting that matters when the bar is parked in
    /// front of the TV's own IR receiver and swallows the signal.
    ///
    /// Soundbar-only, and per speaker. With no flags it reads, and also reports
    /// whether a TV remote has been taught to the bar at all. Teaching it one
    /// is an interactive press-the-button flow and stays in the Sonos app.
    Remote {
        /// The acknowledgement flash: on or off. Not the same light as `led`,
        /// which is the speaker's own status LED.
        #[arg(long)]
        feedback: Option<String>,
        /// Infrared pass-through to the TV: on or off.
        #[arg(long)]
        repeater: Option<String>,
        /// The resulting `{room, feedback, repeater, remote_configured}` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Rename a room.
    ///
    /// Changes what the room is called for **everyone** - every Sonos app in
    /// the house, every controller, and every script that addresses it by
    /// name, including this one. Reversible by renaming it back, and nothing
    /// about what is playing is disturbed.
    ///
    /// Per speaker, like `led`: the name belongs to the player. Renaming one
    /// half of a stereo pair or a room's satellite is not what anyone means, so
    /// `--room` must name a room rather than a bonded speaker.
    Rename {
        /// What to call it instead.
        name: String,
    },
    /// Show or set a speaker's status light: on or off.
    ///
    /// Per speaker rather than per group, like `eq`: the light is on the
    /// hardware, and a room backed by a stereo pair has two of them, of which
    /// `--room` addresses the one it names. With no argument it reads.
    ///
    /// The usual reason to want this is a speaker in a bedroom.
    Led {
        /// `on` or `off`.
        mode: Option<String>,
        /// The resulting `{room, led}` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show or set whether a speaker's touch controls are locked.
    ///
    /// `lock` makes the play/pause and volume controls on the speaker itself do
    /// nothing; `unlock` restores them. Playback over the network is unaffected
    /// either way - this is only about the buttons on the box, and is the Sonos
    /// app's "Button Control" switch. Per speaker, like `led`. With no argument
    /// it reads.
    Buttons {
        /// `lock` or `unlock`.
        mode: Option<String>,
        /// The resulting `{room, buttons_locked}` as JSON.
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
        /// The resulting `{room, bass, treble, loudness, trueplay,
        /// trueplay_available}` as JSON, plus `night_mode`,
        /// `dialog_enhancement` and `dialog_level` on a room with a TV input.
        /// `dialog_level` is the graduated level the speaker holds; `--dialog`
        /// itself only turns it on and off.
        #[arg(long)]
        json: bool,
    },
    /// List the queue, or change it.
    Queue {
        #[command(subcommand)]
        action: Option<QueueAction>,
        /// The queue, or `sources`, as JSON; a change reports what the queue
        /// became.
        // Global to the queue subcommands, so `queue sources --json` is this
        // same flag rather than a second one `queue --json sources` would miss.
        #[arg(long, global = true)]
        json: bool,
    },
    /// List saved favorites, or only those whose name matches a query.
    Favorites {
        query: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Play a favorite, by name or id. One way to start a room that has
    /// nothing queued, which `play` cannot do.
    Favorite { query: String },
    /// Search music services. A term with no `--service` asks every service
    /// that can answer, at once, and merges the results; `--service` asks one.
    /// Neither, and it lists what can be searched.
    ///
    /// Anonymous services need nothing. The rest answer only once `x2rock link`
    /// has a token for them, and a linked service is listed first in a merged
    /// search: it is the tier with real albums, real metadata and content the
    /// player will queue rather than stream.
    Search {
        term: Option<String>,
        /// Service to search, by name. Case-insensitive, and a prefix will do.
        /// Left out, every searchable service is asked.
        #[arg(long, short = 's')]
        service: Option<String>,
        /// Category within the service, by its own name (`stations`, `tracks`),
        /// or several separated by commas (`artists,tracks`) in the order that
        /// matters most. A service is asked once per category it actually has,
        /// and one that has none of them is skipped rather than searched in a
        /// category nobody asked for.
        ///
        /// Left out: the service's `all` where it declares one - which is how a
        /// service says it supports Sonos's Universal Search - else `tracks`,
        /// `artists` and `albums` where it has them, else whatever it lists
        /// first, which is what keeps a stations-only service answering.
        #[arg(long, short = 'c')]
        category: Option<String>,
        /// Ask every category a service publishes, including the ones it made
        /// up. A service may declare a shelf Sonos never standardised - Hype
        /// Machine searches blogs, Sveriges Radio searches radio shows - and
        /// those have service-specific names, so no fixed list can reach them.
        /// Overrides `--category`.
        #[arg(long)]
        all_categories: bool,
        /// How many rows one service contributes to a merged search, after its
        /// categories are interleaved. Defaults to 3, the number Sonos's own
        /// mobile app shows under a service heading; 0 keeps everything. No
        /// effect alongside `--service`.
        #[arg(long, value_name = "N")]
        per_service: Option<usize>,
        /// Only the services with a linked account, in a merged search. Faster,
        /// and the tier whose answers are worth the most. No effect alongside
        /// `--service`.
        #[arg(long)]
        only_linked: bool,
        /// Results per service **per category**. Defaults to 20 for one service
        /// and 5 for a merged search, where several categories of several
        /// services are asked and `--per-service` decides how many of each
        /// service's rows survive.
        #[arg(long)]
        count: Option<u32>,
        /// First result to return, 0-based. Page with `--index 20 --count 20`,
        /// `--index 40`, and so on; `--json` reports `total` so a caller knows
        /// when to stop. **`--play N` counts within the page returned**, so
        /// `--index 20 --play 1` plays the 21st result overall.
        #[arg(long, default_value_t = 0, value_name = "N")]
        index: u32,
        /// Play the Nth result, 1-based, in --room. A stream plays in a
        /// session alongside the queue; anything else is added to the queue and
        /// played, falling back to a stream if the player refuses it. A
        /// container is refused: open it with `browse` instead.
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
        /// First result to return, 0-based. Page with `--index 20 --count 20`,
        /// `--index 40`, and so on; `--json` reports `total` so a caller knows
        /// when to stop. **`--play N` counts within the page returned**, so
        /// `--index 20 --play 1` plays the 21st result overall.
        #[arg(long, default_value_t = 0, value_name = "N")]
        index: u32,
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
    /// A live stream opens a playback session, as one from a service does, so
    /// the room's queue is left exactly as it was. A URL that turns out to be a
    /// *file* - a podcast enclosure, a signed track from a service's CDN -
    /// cannot be played that way at all, so it goes to the transport instead
    /// and does replace what the room was playing. Which one a URL is gets
    /// asked of the server, not guessed from the URL.
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
        /// With no service named, print that list as JSON: `id`, `name` and
        /// whether it is already `linked`. For a caller offering to link one -
        /// the bar widget's service list is the only surface where a service
        /// with no token is visible at all, since every other listing filters
        /// to what can already be reached.
        #[arg(long)]
        json: bool,
        /// Print the URL instead of opening it. What to use over ssh.
        #[arg(long)]
        no_open: bool,
        /// What the household should call the account. Defaults to the hostname,
        /// so a household with several machines can tell them apart.
        #[arg(long)]
        nickname: Option<String>,
        /// Store the token without asking the household to match the account.
        /// Search and browse work either way. On-demand playback depends on the
        /// household holding its own account for the service (added in the
        /// Sonos app), not on this step.
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
        /// No browser at all: read the token the household already stores for
        /// this service - the one the Sonos app minted when it was added there -
        /// and keep it. This is how a service whose own link flow x2rock cannot
        /// complete (Qobuz, Apple Music, Amazon) still becomes searchable, and
        /// it needs no `match`: playback already rides the household's own
        /// registration. Name a service to take just that one, or omit it to
        /// import every service the household holds a usable token for. Requires
        /// a player, and that this machine can receive an event callback from it
        /// - a host firewall must allow the inbound connection the command names.
        #[arg(long, conflicts_with = "from_player")]
        from_household: bool,
        /// Which local TCP port the player calls back on for `--from-household`.
        /// A fixed default so a firewall rule is set once and reused; change it
        /// only if something else holds that port. `0` picks an ephemeral one,
        /// which suits a host with no firewall to open. Ignored without
        /// `--from-household`.
        #[arg(long, default_value_t = 3401)]
        callback_port: u16,
    },
    /// Forget a linked account's stored token.
    ///
    /// Local only: it does not revoke anything at the service, which is done
    /// from that service's own account page.
    Unlink {
        /// Which account to forget, by id, name or unique prefix. Forgotten from
        /// every household that holds it. Omit only with `--all`.
        service: Option<String>,
        /// Forget every stored token, in every household - a clean wipe of what
        /// this machine holds. The tokens stay valid at their services; this
        /// clears only the local copies. `link --from-household` re-imports them.
        #[arg(long, conflicts_with = "service")]
        all: bool,
    },
    /// List the accounts this machine holds a token for.
    Accounts {
        /// Also show the account serials this household's favorites and queues
        /// name. Needs a player; the rest of this command does not.
        // Named `content`, not `household`: the latter is the global
        // `--household` selector (which Sonos household to talk to at all),
        // and clap refuses two args sharing an id - this one is about what
        // *content* is inspected once connected, a narrower and unrelated
        // question.
        #[arg(long)]
        content: bool,
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
        // Global to the bookmarks subcommands, so `bookmarks prune --json` is this
        // same flag rather than a usage error.
        #[arg(long, global = true)]
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
    Playlist { query: String },
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
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
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
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
        volume: Option<u8>,
    },
    /// Group rooms into --room's group, so they play what it plays.
    Group {
        #[arg(required = true)]
        rooms: Vec<String>,
    },
    /// Take a room out of its group, leaving it playing on its own.
    Ungroup { room: String },
    /// Party mode: every room joins --room's group. `party off` breaks it up
    /// and leaves each room on its own.
    Party { mode: Option<String> },
    /// Send one command straight to a player and print what comes back.
    ///
    /// A probe, not a feature: both wires are far wider than this CLI covers,
    /// and settling what one actually answers should not need a rebuild.
    ///
    /// Two transports, as two subcommands, because they share no grammar -
    /// different arity, different scopes, different flags. `api` is the Control
    /// API over the WebSocket; `upnp` is SOAP on port 1400, the older and much
    /// wider surface.
    ///
    /// **A refusal is a result**: a player-side error prints and still exits 0,
    /// so a loop over candidate commands is not stopped by the first
    /// unsupported one. An unreachable speaker is a real failure and exits
    /// non-zero.
    #[command(after_long_help = RAW_EXAMPLES)]
    Raw {
        #[command(subcommand)]
        transport: RawTransport,
    },
    /// Scan the local network for players and remember them.
    Discover,
    /// List every Sonos household reachable on this network, each with its
    /// rooms - the only place a household id is printed.
    ///
    /// Only useful when `--household` is: an ordinary home has exactly one
    /// household, `-r` alone resolves everything, and this just confirms
    /// that. It matters once two Sonos systems share a network (an office,
    /// a guest property) and a room name is not enough to tell them apart -
    /// this is where the id `--household <id>` wants comes from.
    ///
    /// **Always scans fresh**, unlike `status`: a stale household list would
    /// defeat the one thing this exists to get right.
    Households {
        /// One object per household: `{id, rooms}`.
        #[arg(long)]
        json: bool,
        /// Mask each household id down to a comparable tail, the same policy
        /// `system --redact` applies to hardware identifiers - for pasting
        /// somewhere public.
        #[arg(long)]
        redact: bool,
    },
    /// Publish every room as an MPRIS2 media player, until stopped.
    Daemon {
        /// Verbose reconnect logging (also via X2ROCK_LOG_VERBOSE).
        #[arg(long)]
        verbose: bool,
        /// Log every incoming event payload (also via X2ROCK_LOG_EVENTS).
        #[arg(long)]
        log_events: bool,
    },
    /// Every room on one screen, in the terminal. Needs the daemon running.
    Tui,
    /// Install the x2rock agent skill so an AI assistant on this machine knows
    /// how to drive the CLI. Auto-detects installed assistant directories
    /// (Claude, Antigravity / Gemini) by default; the skill is embedded in the
    /// binary, so it always matches this version.
    Skill {
        /// Target agent assistant: `claude` (`~/.claude/skills`), `antigravity` / `gemini`
        /// (`~/.gemini/antigravity-cli/skills`), or `all`. Auto-detects installed
        /// assistants when omitted.
        #[arg(long, value_enum)]
        agent: Option<AgentTarget>,
        /// Where to write it, in place of the default assistant skills directory.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Print the skill to stdout instead of writing it - for inspection, or
        /// to seed another agent.
        #[arg(long)]
        print: bool,
        /// Remove the skill from assistant skills directories instead of installing it.
        #[arg(long, conflicts_with = "print")]
        remove: bool,
    },
    /// Install the desktop entry and icon for MPRIS media player identity.
    ///
    /// Writes `~/.local/share/applications/x2rock.desktop` and
    /// `~/.local/share/icons/hicolor/scalable/apps/x2rock.svg` so Linux desktop
    /// shells (GNOME, KDE, Waybar) show the speaker icon and application title.
    Desktop {
        #[command(subcommand)]
        action: Option<DesktopAction>,
        /// Overwrite hand-edited desktop entry or icon files.
        #[arg(long, global = true)]
        force: bool,
    },
    /// Manage the daemon's systemd user service (install, status, uninstall).
    ///
    /// Writes `~/.config/systemd/user/x2rock.service` from the shipped unit with
    /// `ExecStart` set to the path this command is running from - so it is right
    /// whether the binary came from `cargo install`, a clone, or a package, where
    /// the shipped file assumes one of them. Refuses to overwrite a unit whose
    /// settings differ from what it would write, so hand edits survive;
    /// comments and a unit copied from `systemd/` are replaced without asking.
    /// `--force` overwrites. Re-run after moving or reinstalling the binary.
    Service {
        #[command(subcommand)]
        action: Option<ServiceAction>,
        /// Output status information as JSON.
        #[arg(long, global = true)]
        json: bool,
    },
    /// Generate shell completion scripts for bash, zsh, fish, elvish, or powershell.
    ///
    /// Outputs the script to stdout. Room names, households, services,
    /// bookmarks, and linked accounts are dynamically completed from local
    /// state.
    Completions {
        /// Shell to generate completions for. Auto-detects from $SHELL when omitted.
        shell: Option<clap_complete::Shell>,
        /// Install the completion script to the default user directory for the shell.
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Remove installed completion script from the default user directory for the shell.
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
    /// Internal completion helper for shell scripts: the names in one list,
    /// one per line, optionally only those starting with `prefix`
    /// (case-insensitive). The filtering is here rather than in the shell
    /// because bash's `compgen -W` re-parses its word list with shell quoting
    /// rules and loses every entry after one with a parenthesis in it.
    #[command(name = "__complete", hide = true)]
    Complete {
        what: String,
        prefix: Option<String>,
    },
}

/// Worked examples for `raw --help`. Every one of these was run against a real
/// player, so a reader copying one is copying something that answered.
const RAW_EXAMPLES: &str = "\
Examples:
  # What a soundbar is receiving over HDMI (group-scoped).
  x2rock -r 'Living Room' raw api --scope group playbackMetadata:1 getMetadataStatus

  # One player's own volume, not its group's (player-scoped).
  x2rock -r 'Living Room' raw api --scope player playerVolume:1 getVolume

  # Household state needs no --room.
  x2rock raw api favorites:1 getFavorites

  # A subscribe replies empty and the state arrives after, so watch for it.
  x2rock raw api --watch 5 musicServiceAccounts:1 subscribe

  # Parameters are one JSON object, in the body.
  x2rock -r Kitchen raw api --scope group playback:1 seek '{\"positionMillis\": 30000}'

  # The other surface: UPnP on port 1400, addressed to one speaker.
  x2rock -r Kitchen raw upnp DeviceProperties GetZoneAttributes

  # UPnP arguments are Name=Value pairs, not JSON. Most take InstanceID=0.
  x2rock -r Kitchen raw upnp RenderingControl GetOutputFixed InstanceID=0

  # AVTransport is answered by the group's coordinator, so aim there.
  x2rock -r Kitchen raw upnp --scope group AVTransport GetCurrentTransportActions \\
      InstanceID=0
";

/// Which wire a raw command goes out on.
///
/// A subcommand rather than a flag because the two transports do not share a
/// grammar: different argument arity, a different set of meaningful scopes with
/// a different default, and flags (`--watch`, `--session`) that exist only on
/// one side. As one flag-bearing command all of that had to be re-checked at
/// runtime - an arity `ensure!`, a `bail!` for the two scopes UPnP cannot mean,
/// two transport-dependent defaults and a `conflicts_with_all` - and every new
/// flag or scope had to be classified by hand or it was silently ignored.
/// Declared per transport, clap enforces the lot and documents it in `--help`.
#[derive(Subcommand)]
pub enum RawTransport {
    /// The Control API, over the player's WebSocket. The newer surface.
    Api {
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
        /// Household is the default because the namespaces left to explore are
        /// mostly household-scoped. `group` and `player` resolve through
        /// --room and connect to the right player themselves, so --ip is never
        /// needed to reach one.
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
    /// UPnP/SOAP on port 1400. The older surface, and much the wider one.
    ///
    /// The Control API never got line-in, the physical speaker (LED,
    /// touch-button lock, room name, stereo pairing), soundbar IR, or the local
    /// music library, and UPnP has all of it. An unknown service name lists the
    /// sixteen there are.
    Upnp {
        /// Service name, e.g. `DeviceProperties`. Case-insensitive.
        service: String,
        /// Action within it, e.g. `GetZoneAttributes`.
        action: String,
        /// `Name=Value` pairs, one argument each. SOAP takes a flat list of
        /// named strings, not a nested object, so there is nothing for JSON to
        /// express here. Most actions need `InstanceID=0`.
        #[arg(value_name = "ARGS")]
        args: Vec<String>,
        /// Which speaker to send to. UPnP addresses a player, never a group, so
        /// only these two mean anything: `player` (the default) is --room's own
        /// speaker, which is what RenderingControl and DeviceProperties are
        /// per; `group` is its coordinator, which is the one that answers for
        /// AVTransport.
        #[arg(long, value_enum, default_value_t = UpnpScope::Player)]
        scope: UpnpScope,
    },
}

/// Which speaker a UPnP action is addressed to.
///
/// Its own enum rather than a subset of [`RawScope`] checked at runtime: the
/// household and unaddressed variants have no meaning over a transport that
/// posts to one speaker's IP, and an enum that cannot express them needs no
/// `bail!` arm to reject them.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UpnpScope {
    /// --room's own speaker.
    Player,
    /// --room's group coordinator.
    Group,
}

/// Which target key a raw command carries, which is per-namespace and is half
/// of what a probe is trying to find out.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RawScope {
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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RateDirection {
    Up,
    Down,
}

/// What `service` can do to the daemon's unit: install, check status, or uninstall.
#[derive(Subcommand)]
pub enum ServiceAction {
    /// Write the unit (and, with --headless, its drop-in), then `daemon-reload`.
    Install {
        /// Also install the drop-in for a machine with no graphical session,
        /// so the daemon starts from boot. Pair it with `loginctl
        /// enable-linger`, which this prints but does not run.
        #[arg(long)]
        headless: bool,
        /// Run `systemctl --user enable --now x2rock.service` afterwards.
        #[arg(long)]
        enable: bool,
        /// Overwrite a unit whose settings differ from what would be written.
        /// Without it, the differing lines are shown and nothing changes.
        #[arg(long)]
        force: bool,
        /// Print the unit to stdout instead of writing it.
        #[arg(long)]
        print: bool,
        /// Drop the `X2ROCK_HOUSEHOLD` line. Without `--household` or this, a
        /// household already set in the installed unit is kept.
        #[arg(long)]
        no_household: bool,
    },
    /// Check whether the systemd user service and daemon are active, enabled, or stale.
    Status,
    /// Stop, disable, and remove the systemd user service and drop-in.
    Uninstall {
        /// Also remove installed desktop entry and icon files.
        #[arg(long)]
        desktop: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AgentTarget {
    Claude,
    Antigravity,
    Gemini,
    All,
}

#[derive(Subcommand)]
pub enum DesktopAction {
    /// Install `~/.local/share/applications/x2rock.desktop` and `~/.local/share/icons/.../x2rock.svg`.
    Install,
    /// Remove `~/.local/share/applications/x2rock.desktop` and `~/.local/share/icons/.../x2rock.svg`.
    Uninstall,
    /// Check whether the desktop entry and icon files are installed.
    Status {
        /// Output desktop file status as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// What `bookmarks` can do: remove, pin from history, rename, or prune.
///
/// Subcommands rather than top-level commands, to sit beside `queue remove`:
/// they act on the bookmarks file printed by the base command.
#[derive(Subcommand)]
pub enum BookmarksAction {
    /// Forget one, by name. Matches the history too, not just what was kept.
    Remove { query: String },
    /// Pin an item already in bookmarks or daemon history by name, keeping it permanently.
    Pin { query: String },
    /// Rename a kept or history bookmark.
    Rename { query: String, new_name: String },
    /// Prune unpinned history entries recorded by the daemon, preserving kept bookmarks.
    Prune,
}

#[derive(Subcommand)]
pub enum AlarmsAction {
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
        #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u8).range(0..=100))]
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
pub enum AlarmAction {
    /// Arm it.
    On,
    /// Disarm it, leaving it in the list to be armed again.
    Off,
    /// Delete it. Sonos keeps no undo; `x2rock alarms add` makes a new one.
    Remove {
        /// Confirm: removal cannot be undone.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum QueueAction {
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
    Sources { query: Option<String> },
    /// Add a saved playlist or favorite to the queue, by name or id.
    Add {
        query: String,
        /// Put it next rather than at the end.
        #[arg(long)]
        next: bool,
    },
}

impl Command {
    /// Whether the command was asked for `--json`, so an error can match the
    /// output the caller expected. Every variant with the flag is here - a test
    /// walks clap's tree to hold it to that, because the list drifted once and
    /// `system --json` against a dead address printed a prose error an agent
    /// branching on `code` could not read.
    pub fn json(&self) -> bool {
        match self {
            Command::Rooms { json, .. }
            | Command::Now { json, .. }
            | Command::Status { json, .. }
            | Command::Vol { json, .. }
            | Command::Repeat { json, .. }
            | Command::Shuffle { json, .. }
            | Command::Update { json, .. }
            | Command::System { json, .. }
            | Command::Battery { json, .. }
            | Command::Alarms { json, .. }
            | Command::Sleep { json, .. }
            | Command::Snooze { json, .. }
            | Command::Crossfade { json, .. }
            | Command::Remote { json, .. }
            | Command::Led { json, .. }
            | Command::Buttons { json, .. }
            | Command::Eq { json, .. }
            | Command::Favorites { json, .. }
            | Command::Search { json, .. }
            | Command::Browse { json, .. }
            | Command::Stations { json, .. }
            | Command::PlayUrl { json, .. }
            | Command::Accounts { json, .. }
            | Command::Link { json, .. }
            | Command::Bookmarks { json, .. }
            | Command::Households { json, .. }
            | Command::Rate { json, .. }
            | Command::Service { json, .. }
            | Command::Queue { json, .. } => *json,
            Command::Desktop {
                action: Some(DesktopAction::Status { json }),
                ..
            } => *json,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

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
    fn bookmarks_subcommands_accept_json_flag() {
        let cli = Cli::try_parse_from(["x2rock", "bookmarks", "prune", "--json"]).unwrap();
        assert!(cli.command.json());
        assert!(matches!(
            cli.command,
            Command::Bookmarks {
                action: Some(BookmarksAction::Prune),
                json: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from(["x2rock", "bookmarks", "pin", "Bodies", "--json"]).unwrap();
        assert!(cli.command.json());
        assert!(matches!(
            cli.command,
            Command::Bookmarks {
                action: Some(BookmarksAction::Pin { ref query }),
                json: true,
                ..
            } if query == "Bodies"
        ));

        let cli =
            Cli::try_parse_from(["x2rock", "bookmarks", "rename", "A", "B", "--json"]).unwrap();
        assert!(cli.command.json());

        let cli = Cli::try_parse_from(["x2rock", "bookmarks", "remove", "A", "--json"]).unwrap();
        assert!(cli.command.json());
    }

    #[test]
    fn service_accepts_global_json_flag() {
        let cli = Cli::try_parse_from(["x2rock", "service", "--json"]).unwrap();
        assert!(cli.command.json());
        assert!(matches!(
            cli.command,
            Command::Service {
                action: None,
                json: true,
            }
        ));

        let cli = Cli::try_parse_from(["x2rock", "service", "status", "--json"]).unwrap();
        assert!(cli.command.json());
        assert!(matches!(
            cli.command,
            Command::Service {
                action: Some(ServiceAction::Status),
                json: true,
            }
        ));
    }

    #[test]
    fn completions_uninstall_flag() {
        let cli = Cli::try_parse_from(["x2rock", "completions", "--uninstall"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Completions {
                shell: None,
                install: false,
                uninstall: true,
            }
        ));

        let cli = Cli::try_parse_from(["x2rock", "completions", "fish", "--uninstall"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Completions {
                shell: Some(clap_complete::Shell::Fish),
                install: false,
                uninstall: true,
            }
        ));

        // --install and --uninstall conflict with each other
        assert!(
            Cli::try_parse_from(["x2rock", "completions", "--install", "--uninstall"]).is_err()
        );
    }

    #[test]
    fn desktop_force_flag() {
        let cli = Cli::try_parse_from(["x2rock", "desktop"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Desktop {
                action: None,
                force: false,
            }
        ));

        let cli = Cli::try_parse_from(["x2rock", "desktop", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Desktop {
                action: None,
                force: true,
            }
        ));

        let cli = Cli::try_parse_from(["x2rock", "desktop", "install"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Desktop {
                action: Some(DesktopAction::Install),
                force: false,
            }
        ));

        let cli = Cli::try_parse_from(["x2rock", "desktop", "install", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Desktop {
                action: Some(DesktopAction::Install),
                force: true,
            }
        ));

        let cli = Cli::try_parse_from(["x2rock", "desktop", "--force", "install"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Desktop {
                action: Some(DesktopAction::Install),
                force: true,
            }
        ));
    }

    /// `remote --feedback` is the soundbar's acknowledgement flash and `led` is
    /// the speaker's own status light. Two different lights, one word, so the
    /// commands are pinned apart here: neither flag belongs to the other.
    #[test]
    fn the_two_lights_are_different_commands() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "x2rock",
            "-r",
            "Guest TV",
            "remote",
            "--feedback",
            "off",
            "--repeater",
            "on",
        ])
        .expect("--feedback/--repeater are valid remote flags");
        assert!(matches!(
            cli.command,
            Command::Remote { feedback: Some(ref f), repeater: Some(ref r), .. }
                if f == "off" && r == "on"
        ));

        // The status light takes a bare word and knows nothing of --feedback.
        assert!(Cli::try_parse_from(["x2rock", "-r", "Kitchen", "led", "off"]).is_ok());
        assert!(
            Cli::try_parse_from(["x2rock", "-r", "Kitchen", "led", "--feedback", "off"]).is_err()
        );
        // And `remote` takes no bare word.
        assert!(Cli::try_parse_from(["x2rock", "-r", "Guest TV", "remote", "off"]).is_err());
    }

    #[test]
    fn daemon_flags_parse_from_cli() {
        let cli = Cli::try_parse_from(["x2rock", "daemon", "--verbose", "--log-events"]).unwrap();
        match cli.command {
            Command::Daemon {
                verbose,
                log_events,
            } => {
                assert!(verbose);
                assert!(log_events);
            }
            _ => panic!("expected Command::Daemon"),
        }
    }

    #[test]
    fn every_command_that_takes_json_reports_it() {
        use clap::CommandFactory;

        // Walk clap's own tree, so a `--json` cannot be added to a command
        // without `Command::json` learning of it: the hand-kept list is what
        // let `system --json` print its failures as prose.
        fn walk(cmd: &clap::Command, path: &mut Vec<String>) {
            if cmd.get_arguments().any(|a| a.get_id() == "json") {
                let mut argv = vec!["x2rock".to_string()];
                argv.extend(path.iter().cloned());
                // A required positional gets a placeholder that reads as any
                // type - "1" for a bare string or number, or its own first
                // possible value for a closed enum (`rate <up|down>`), which
                // "1" is not one of.
                for arg in cmd.get_arguments().filter(|a| a.is_required_set()) {
                    let placeholder = arg
                        .get_possible_values()
                        .first()
                        .map_or_else(|| "1".to_string(), |v| v.get_name().to_string());
                    argv.push(placeholder);
                }
                argv.push("--json".to_string());
                let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                assert!(
                    cli.command.json(),
                    "{argv:?} takes --json, but Command::json says no"
                );
            }
            for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
                path.push(sub.get_name().to_string());
                walk(sub, path);
                path.pop();
            }
        }
        let mut root = Cli::command();
        root.build();
        walk(&root, &mut Vec::new());
    }

    #[test]
    fn a_default_room_yields_to_all_but_a_typed_one_does_not() {
        use clap::parser::ValueSource;

        // What `main` does with the matches, without the environment: a room
        // from the env var is set aside for `--all`, a typed one is kept for
        // `run` to refuse. (`conflicts_with` could not tell them apart.)
        let matches = Cli::command()
            .try_get_matches_from(["x2rock", "--all", "-r", "Kitchen", "vol", "-10"])
            .expect("no longer a clap conflict");
        let cli = Cli::from_arg_matches(&matches).unwrap();
        assert!(cli.all);
        assert_eq!(matches.value_source("room"), Some(ValueSource::CommandLine));
        assert_eq!(cli.room, ["Kitchen"]);
    }

    /// A service that starts offering ratings does not move the player's
    /// service-list version, which is the only thing that otherwise clears the
    /// learned-ratings cache - so without this flag one ratings-less reply is
    /// believed forever and the only escapes are `search --refresh` on an
    /// unrelated service or deleting the cache by hand.
    #[test]
    fn rate_can_force_a_catalogue_re_read_like_search_and_browse() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("x2rock").chain(args.iter().copied()))
        };
        assert!(matches!(
            parse(&["rate", "up", "--refresh"]).unwrap().command,
            Command::Rate { refresh: true, .. }
        ));
        // Off by default: the point of the cache is not paying for it twice.
        assert!(matches!(
            parse(&["rate", "up"]).unwrap().command,
            Command::Rate { refresh: false, .. }
        ));
    }

    /// Paging is the difference between "this container has 20 things" and
    /// "here are the first 20 things there might be more of", and `--json` had
    /// no way to say which. `--index` is 0-based and pairs with `--count`.
    #[test]
    fn a_bare_term_is_the_merged_search_and_count_defaults_per_mode() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("x2rock").chain(args.iter().copied()))
                .unwrap()
                .command
        };
        // The shape the fan-out keys off: a term, and no service named.
        assert!(matches!(
            parse(&["search", "travis scott"]),
            Command::Search {
                service: None,
                count: None,
                only_linked: false,
                ..
            }
        ));
        // `count` is optional rather than defaulted, because 20 is right for one
        // service and wrong for thirty-five; the two defaults live in the
        // command, not in clap.
        assert!(matches!(
            parse(&["search", "-s", "Deezer", "travis scott", "--count", "7"]),
            Command::Search { count: Some(7), .. }
        ));
        assert!(matches!(
            parse(&["search", "--only-linked", "travis scott"]),
            Command::Search {
                only_linked: true,
                service: None,
                ..
            }
        ));
        // A comma list is one argument to clap; the splitting is the command's,
        // so that an unknown name can be dropped per service rather than
        // rejected outright for everyone.
        let Command::Search {
            category,
            per_service,
            ..
        } = parse(&[
            "search",
            "-c",
            "artists,tracks",
            "--per-service",
            "5",
            "sum 41",
        ])
        else {
            panic!("not a search")
        };
        assert_eq!(category.as_deref(), Some("artists,tracks"));
        assert_eq!(per_service, Some(5));
        // Absent rather than defaulted, so the command can tell 3-for-merged
        // from 20-for-one the way `count` already does.
        let Command::Search { per_service, .. } = parse(&["search", "sum 41"]) else {
            panic!("not a search")
        };
        assert_eq!(per_service, None);
        // Still the listing when there is no term at all.
        assert!(matches!(
            parse(&["search"]),
            Command::Search {
                term: None,
                service: None,
                ..
            }
        ));
    }

    #[test]
    fn browse_and_search_take_a_page_to_fetch() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("x2rock").chain(args.iter().copied()))
        };
        assert!(matches!(
            parse(&[
                "browse", "-s", "Relisten", "latest", "--count", "3", "--index", "3"
            ])
            .unwrap()
            .command,
            Command::Browse {
                count: 3,
                index: 3,
                ..
            }
        ));
        assert!(matches!(
            parse(&["search", "-s", "TuneIn", "jazz", "--index", "40"])
                .unwrap()
                .command,
            Command::Search { index: 40, .. }
        ));
        // Zero by default, so every existing caller keeps asking for page one.
        assert!(matches!(
            parse(&["browse", "-s", "Relisten"]).unwrap().command,
            Command::Browse { index: 0, .. }
        ));
        assert!(matches!(
            parse(&["search", "-s", "TuneIn", "jazz"]).unwrap().command,
            Command::Search { index: 0, .. }
        ));
    }

    /// Each transport declares its own grammar, so clap rejects the other's
    /// rather than anything being checked - or silently ignored - at runtime.
    /// `--watch`/`--session` are the event socket and have no counterpart over
    /// UPnP; the arity and the scope set differ in both directions.
    #[test]
    fn each_raw_transport_accepts_only_its_own_grammar() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("x2rock").chain(args.iter().copied()))
        };
        assert!(parse(&["raw", "api", "favorites:1", "getFavorites"]).is_ok());
        assert!(parse(&["raw", "api", "--watch", "5", "favorites:1", "subscribe"]).is_ok());
        assert!(
            parse(&[
                "raw",
                "upnp",
                "AVTransport",
                "GetTransportInfo",
                "InstanceID=0"
            ])
            .is_ok()
        );

        // The event-socket flags do not exist on the UPnP side.
        assert!(parse(&["raw", "upnp", "--watch", "5", "AVTransport", "Play"]).is_err());
        assert!(parse(&["raw", "upnp", "--session", "x", "AVTransport", "Play"]).is_err());

        // Arity: UPnP takes many Name=Value args, the Control API exactly one
        // JSON object. The second used to need a runtime `ensure!`.
        assert!(parse(&["raw", "upnp", "RenderingControl", "SetEQ", "a=1", "b=2"]).is_ok());
        assert!(parse(&["raw", "api", "playback:1", "seek", "{}", "extra"]).is_err());

        // Scope: `household` cannot be spelled over UPnP at all now, rather
        // than parsing and being rejected later.
        assert!(
            parse(&[
                "raw",
                "api",
                "--scope",
                "household",
                "groups:1",
                "getGroups"
            ])
            .is_ok()
        );
        assert!(
            parse(&[
                "raw",
                "upnp",
                "--scope",
                "household",
                "AlarmClock",
                "ListAlarms"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "raw",
                "upnp",
                "--scope",
                "player",
                "AlarmClock",
                "ListAlarms"
            ])
            .is_ok()
        );

        // And the old flag spelling is gone rather than quietly meaning the
        // Control API with a stray positional.
        assert!(parse(&["raw", "--upnp", "AVTransport", "GetTransportInfo"]).is_err());
    }
}
