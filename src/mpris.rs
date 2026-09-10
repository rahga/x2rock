//! One MPRIS2 media player per Sonos group.
//!
//! Anything that already speaks MPRIS - Omarchy's `omarchy.media` bar widget,
//! Waybar's `mpris` module, `playerctl`, desktop media keys - gets Sonos control
//! for free. State comes from the player's own events; nothing here polls.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::Instant;

use mpris_server::zbus::{self, fdo};
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, Property, RootInterface,
    Time, TrackId,
};

use crate::sonos::local::Connection;
use crate::sonos::proto::{self, MetadataStatus, PlayModes, PlaybackActions, Repeat};
use crate::sonos::upnp::Upnp;

/// `org.mpris.MediaPlayer2.<suffix>`. "Media Room" becomes `x2rock-media-room`,
/// the same convention the Kotlin daemon used, so existing bar configs carry over.
fn bus_suffix(room: &str) -> String {
    let mut suffix = String::from("x2rock-");
    let mut pending_dash = false;
    for c in room.chars() {
        if c.is_ascii_alphanumeric() {
            suffix.push(c.to_ascii_lowercase());
            pending_dash = false;
        } else if !pending_dash {
            suffix.push('-');
            pending_dash = true;
        }
    }
    suffix.trim_end_matches('-').to_string()
}

/// A suffix none of `taken` is using. [`bus_suffix`] is many-to-one - "Kitchen"
/// and "kitchen", "Media Room" and "Media-Room", any two names with no ASCII in
/// them at all - and zbus asks for a bus name without queueing, so a second
/// player on a taken name is refused outright (`NameTaken`), which used to
/// cost the household its players. The first room keeps the plain name, so a
/// bar config written against it stays right; the next gets `-2`, then `-3`.
pub fn bus_suffix_among(room: &str, taken: &HashSet<String>) -> String {
    let base = bus_suffix(room);
    if !taken.contains(&base) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[derive(Default)]
struct RoomState {
    status: Option<PlaybackStatus>,
    metadata: Metadata,

    /// Position at the last event, and when that was, so `Position` can advance
    /// between events without polling the player.
    position_millis: u64,
    position_at: Option<Instant>,
    /// What the current source allows, and how the queue is being traversed,
    /// both straight from the last `playbackStatus` event.
    actions: PlaybackActions,
    play_modes: PlayModes,
    /// Every room in this group, coordinator first, as `(player id, name)`.
    /// Fixed for the life of the player: a group whose membership changes is
    /// republished from scratch.
    members: Vec<(String, String)>,
    /// Each member's own volume, in the same order. Grouped rooms share a group
    /// volume; this is the balance between them, which MPRIS has no room for.
    member_volumes: Vec<u8>,
    /// Whether the group, and each member, is muted - see [`MUTED`].
    muted: bool,
    member_muted: Vec<bool>,
    /// Whether the group, and each member, has a fixed volume - see
    /// [`FIXED_VOLUME`].
    fixed: bool,
    member_fixed: Vec<bool>,
    /// The group's and each member's volume regardless of mute - see
    /// [`VOLUME_LEVEL`].
    level: u8,
    member_levels: Vec<u8>,
    /// Nothing loaded - see [`NO_SOURCE`].
    no_source: bool,
    /// The queue's version, straight from `playback:1`.
    queue_version: String,
    /// Whether the room is on its TV input right now.
    on_tv_input: bool,
    /// What a soundbar is receiving over HDMI, e.g. "Dolby Digital 5.1". Empty
    /// off the TV input, but also on it when the player names no codec and no
    /// channels, so this is not the way to ask; [`RoomState::on_tv_input`] is.
    input_format: String,
    /// Whether this room has a TV input to switch to at all.
    has_tv_input: bool,
    /// Night sound and speech enhancement, the two settings the Sonos app puts
    /// in front of a room on its TV input. `None` until the owning player has
    /// said, and forever on anything that is not a soundbar - which is why they
    /// are `Option` and not `bool`: a client has to be able to tell "this room
    /// has no such setting" from "it has it and it is off", and only the first
    /// means do not draw the control.
    night_mode: Option<bool>,
    enhance_dialog: Option<bool>,
    /// What was last announced, so a change can be noticed rather than
    /// predicted - see [`RoomState::announce`].
    announced: Metadata,
}

/// MPRIS has `CanGoNext` but no `CanLoop` or `CanShuffle`, so whether the current
/// source allows those goes out as namespaced metadata keys - the one place the
/// spec lets a player add its own fields - for clients that want to grey out a
/// button rather than have the set fail.
pub(crate) const CAN_REPEAT: &str = "x2rock:canRepeat";
pub(crate) const CAN_REPEAT_ONE: &str = "x2rock:canRepeatOne";
pub(crate) const CAN_SHUFFLE: &str = "x2rock:canShuffle";
/// The rooms in this group. MPRIS describes one player, and has no way to say
/// that player is really several speakers - but a bar widget wants to show it,
/// and needs it to tell "everything is grouped" from "there is only one room".
pub(crate) const MEMBERS: &str = "x2rock:members";
/// The queue's version: a number that moves whenever the queue changes.
///
/// **Observed empty on every player here, always** (2026-09-01). It is taken
/// from `playbackStatus.queueVersion`, and this household's firmware
/// (95.0-77060) does not send that field: `getPlaybackStatus` answers with
/// `playbackState`, `positionMillis`, `itemId`, `playModes`,
/// `availablePlaybackActions`, `isDucking` and the two `previous*` fields, and
/// nothing else. Forcing real events - a pause and a play - did not produce it
/// either, so it is absent from the event body too and not merely from the
/// polled response.
///
/// **So it is filled from UPnP instead**, by [`RoomPlayer::refresh_queue_version`]
/// on each playback event: the `UpdateID` of a `Q:0` browse, which is the
/// version every queue mutation already reads before acting. The local API has
/// no queue namespace to subscribe to - `queue:1` and `playbackQueue:1` both
/// answer `ERROR_UNSUPPORTED_NAMESPACE` - so UPnP is the only source.
///
/// Now carries a real number (84, 85, ... on this household) and moves when the
/// queue does.
const QUEUE_VERSION: &str = "x2rock:queueVersion";
/// Each member's own volume, aligned with [`MEMBERS`].
///
/// Sent as decimal strings rather than numbers, because a D-Bus array of ints
/// does not survive the trip: Quickshell hands `as` to QML as an ordinary array
/// but `ai` arrives with no length and no indexing, so every slider read zero.
/// An array of strings is what [`MEMBERS`] already proves works.
pub(crate) const MEMBER_VOLUMES: &str = "x2rock:memberVolumes";
/// Whether the group is muted, and each member in [`MEMBERS`] order. The
/// volumes read zero while muted - that is what is heard, and what a generic
/// MPRIS client should show - so without these a muted room and a silent one
/// are the same picture, and a client cannot offer "unmute" where it would
/// otherwise offer a volume step. Strings for the member list, for the reason
/// [`MEMBER_VOLUMES`] gives.
pub(crate) const MUTED: &str = "x2rock:muted";
pub(crate) const MEMBER_MUTED: &str = "x2rock:memberMuted";
/// Crossfade, the one play mode MPRIS has no property for. Repeat and shuffle
/// go out as LoopStatus and Shuffle; this rides on Metadata like the hints.
pub(crate) const CROSSFADE: &str = "x2rock:crossfade";
/// Whether the source can crossfade at all, with [`CAN_REPEAT`] and the others.
pub(crate) const CAN_CROSSFADE: &str = "x2rock:canCrossfade";
/// Whether the group, and each member, has a fixed volume: line-level output
/// with no volume control of its own, a Port feeding an amplifier. The CLI
/// refuses a volume change on such a room and says to adjust the amplifier; a
/// client that knows can withdraw the control instead of offering one that
/// fails. Read from the same `fixed` field the CLI already reports. **Untested
/// against a real Port** - this household has none - so what is verified is
/// that the flag travels, not what a Port sends.
pub(crate) const FIXED_VOLUME: &str = "x2rock:fixedVolume";
pub(crate) const MEMBER_FIXED_VOLUME: &str = "x2rock:memberFixedVolume";
/// The slider position: the group's volume regardless of mute, and each
/// member's in [`MEMBERS`] order. MPRIS Volume and [`MEMBER_VOLUMES`] read zero
/// while muted, which is right for what is heard and wrong for a slider - the
/// Sonos app keeps the slider where it was and dims it, and moving it unmutes.
/// The player keeps the level under mute and reports it (checked: a muted room
/// at 27 answers `getVolume` with 27); this passes it on. Strings, for the
/// reason [`MEMBER_VOLUMES`] gives.
pub(crate) const VOLUME_LEVEL: &str = "x2rock:volumeLevel";
pub(crate) const MEMBER_VOLUME_LEVELS: &str = "x2rock:memberVolumeLevels";
/// Whether the room has nothing loaded at all: no current item and no
/// container - a queue that was cleared, a room that has never played. The
/// Sonos app calls this "No Content" and withdraws transport, the position
/// bar, repeat and shuffle, leaving volume and mute. The player withdraws play,
/// pause and skip itself (all false) but keeps canRepeat and canShuffle true,
/// so under this flag the mode hints above go out false as well: what the
/// current source allows is nothing, there being no source.
pub(crate) const NO_SOURCE: &str = "x2rock:noSource";
/// What a soundbar is receiving, and whether it has a TV input to receive on.
/// The format is the interesting one: a source that has quietly fallen back to
/// stereo is invisible anywhere else, and this is what makes it a glance.
pub(crate) const INPUT_FORMAT: &str = "x2rock:inputFormat";
pub(crate) const ON_TV_INPUT: &str = "x2rock:onTvInput";
pub(crate) const HAS_TV_INPUT: &str = "x2rock:hasTvInput";
/// Night sound and speech enhancement, sent only for a room that has them.
///
/// The Sonos app offers a room on its TV input no transport at all - volume,
/// grouping, Night Sound, Speech Enhancement and a sleep timer are the whole
/// set - so a client drawing that room needs these two the way it needs
/// [`CAN_REPEAT`] elsewhere. Writing them is not MPRIS's to do: it has no verb
/// for either, and `x2rock eq --night/--dialog` is the write path.
const NIGHT_MODE: &str = "x2rock:nightMode";
const ENHANCE_DIALOG: &str = "x2rock:enhanceDialog";
/// Whether what is playing is a live stream rather than something on demand.
///
/// MPRIS has no way to say it. `mpris:length` being absent is the closest
/// signal and it is not the same question - a track whose duration the service
/// simply did not send looks identical, and a client that dropped the icon on
/// that would be wrong about the source rather than about the metadata.
pub(crate) const LIVE_STREAM: &str = "x2rock:isLiveStream";
/// The station behind a live stream, when it is not already the title.
///
/// Sonos Radio names the *track* in `currentItem` and the station only in the
/// container, so a client that shows the title alone says "Intervallo (from
/// "Veruschka") (II)" and never says where it came from. TuneIn has no track at
/// all and the title already *is* the station, which is why this is sent only
/// when it would add something rather than repeat the line above it.
pub(crate) const STATION_NAME: &str = "x2rock:stationName";
/// A live stream's own "now playing" text, verbatim and unparsed.
///
/// For a stream started by `play-url` this is the *only* track information
/// there is: `xesam:title` is the station (often just its host) and artist,
/// album and length are all absent, so a client with no access to this shows
/// the URL and nothing about the music.
///
/// **Never split it.** `Artist - Title` is an Icecast convention rather than a
/// format - a station is equally free to put a show name or a slogan here - and
/// splitting on the hyphen invents an artist wherever it happens to land.
///
/// Blank counts as absent, matching `now --json`: a station between titles
/// sends a few spaces, and an empty headline reads worse than saying nothing.
pub(crate) const STREAM_INFO: &str = "x2rock:streamInfo";

impl RoomState {
    /// Fold a `playbackStatus` body into the room, returning what MPRIS has to
    /// be told about.
    ///
    /// **Every field of the body is optional and every `None` means
    /// *unchanged*.** A body that omits a field is not a body saying the field
    /// is empty: the room keeps the state, position, actions and modes it had.
    /// See `proto::PlaybackStatus::playback_state`.
    ///
    /// On `RoomState` rather than on [`RoomPlayer`] because none of it needs
    /// the connection - which is also what lets it be tested.
    fn apply_playback(&mut self, status: &proto::PlaybackStatus) -> Vec<Property> {
        let mpris_status = status.state().map(|state| match state {
            "PLAYING" | "BUFFERING" => PlaybackStatus::Playing,
            "PAUSED" => PlaybackStatus::Paused,
            _ => PlaybackStatus::Stopped,
        });
        if let Some(mpris_status) = mpris_status {
            self.status = Some(mpris_status);
        }
        if let Some(position_millis) = status.position_millis {
            self.position_millis = position_millis;
            self.position_at = Some(Instant::now());
        }
        // The queue version, the availability hints and crossfade all ride on
        // Metadata, and each used to be compared here to decide whether to
        // re-announce it. `announce` compares the whole thing instead.
        if let Some(version) = status.queue_version.as_deref() {
            self.queue_version = version.to_owned();
        }
        if let Some(actions) = status.available_playback_actions {
            self.actions = actions;
        }
        if let Some(modes) = status.play_modes {
            self.play_modes = modes;
        }
        // Read back off the state rather than the body, so a field the body
        // left out re-announces what is still true instead of announcing that
        // nothing is allowed.
        let actions = self.actions;
        let mut properties = vec![
            Property::CanGoNext(actions.can_skip),
            Property::CanGoPrevious(actions.can_skip_back),
            Property::CanPlay(actions.can_play),
            Property::CanPause(actions.can_pause),
            Property::CanSeek(actions.can_seek),
            Property::LoopStatus(self.loop_status()),
            Property::Shuffle(self.play_modes.shuffle),
        ];
        if let Some(mpris_status) = mpris_status {
            properties.push(Property::PlaybackStatus(mpris_status));
        }
        properties.extend(self.announce());
        properties
    }

    fn loop_status(&self) -> LoopStatus {
        match self.play_modes.repeat() {
            Repeat::Off => LoopStatus::None,
            Repeat::All => LoopStatus::Playlist,
            Repeat::One => LoopStatus::Track,
        }
    }

    /// Fold a `groupVolume` event in.
    fn apply_volume(&mut self, volume: &proto::Volume) -> Vec<Property> {
        // Metadata carries the level, the mute and the fixed flag, so any of
        // the three moving has to be announced - the level included.
        //
        // **A client caches what a signal names and re-reads nothing else.**
        // The level, the mute and the fixed flag all ride on Metadata, and an
        // earlier version here announced it only when the level moved *under
        // mute* - on the reasoning that a Volume change prompts a re-read
        // anyway. It does not: setting a room to 25 sent `Volume` 0.25 and left
        // `x2rock:volumeLevel` reading the level before it, so a bar widget's
        // slider sat at the old position, seen on a live household. Deciding
        // that here is what went wrong; `announce` decides it now.
        self.level = volume.volume;
        self.muted = volume.muted;
        self.fixed = volume.fixed;
        let mut properties = vec![Property::Volume(self.heard())];
        properties.extend(self.announce());
        properties
    }

    /// MPRIS `Volume`: 0.0-1.0, and 0.0 while muted, since that is what is
    /// heard. Derived rather than stored - the level and the mute are what the
    /// player reports, and a third field holding what those two already say is
    /// a field that can disagree with them.
    fn heard(&self) -> f64 {
        if self.muted {
            0.0
        } else {
            f64::from(self.level) / 100.0
        }
    }

    /// The metadata to announce, if it has moved since the last announcement.
    ///
    /// **No caller has to know which keys ride on Metadata.** [`Self::with_hints`]
    /// writes twenty of them off this struct, and each `apply_` site used to
    /// carry its own predicate naming the ones it believed it could move. Miss
    /// one and nothing complains: the daemon is right, the terminal UI is right
    /// because it re-reads everything, and only a client that caches what a
    /// signal names is wrong - which is how a volume level that had moved sat
    /// stale on a live household. A comparison cannot miss one.
    ///
    /// The cost is a map compare against the last announcement, on events that
    /// arrive a few times a second at their fastest.
    fn announce(&mut self) -> Option<Property> {
        let next = self.with_hints();
        if next == self.announced {
            return None;
        }
        self.announced = next.clone();
        Some(Property::Metadata(next))
    }

    /// The track metadata plus the availability hints.
    fn with_hints(&self) -> Metadata {
        let mut metadata = self.metadata.clone();
        // With no source there is nothing for a mode to apply to, whatever the
        // player says about the modes - see NO_SOURCE.
        let has_source = !self.no_source;
        metadata.set(CAN_REPEAT, Some(self.actions.can_repeat && has_source));
        metadata.set(
            CAN_REPEAT_ONE,
            Some(self.actions.can_repeat_one && has_source),
        );
        metadata.set(CAN_SHUFFLE, Some(self.actions.can_shuffle && has_source));
        metadata.set(
            CAN_CROSSFADE,
            Some(self.actions.can_crossfade && has_source),
        );
        metadata.set(NO_SOURCE, Some(self.no_source));
        let names: Vec<_> = self.members.iter().map(|(_, name)| name.clone()).collect();
        metadata.set(MEMBERS, Some(names));
        metadata.set(
            MEMBER_VOLUMES,
            Some(
                self.member_volumes
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>(),
            ),
        );
        metadata.set(MUTED, Some(self.muted));
        metadata.set(
            MEMBER_MUTED,
            Some(
                self.member_muted
                    .iter()
                    .map(bool::to_string)
                    .collect::<Vec<_>>(),
            ),
        );
        metadata.set(CROSSFADE, Some(self.play_modes.crossfade));
        metadata.set(FIXED_VOLUME, Some(self.fixed));
        metadata.set(
            MEMBER_FIXED_VOLUME,
            Some(
                self.member_fixed
                    .iter()
                    .map(bool::to_string)
                    .collect::<Vec<_>>(),
            ),
        );
        metadata.set(VOLUME_LEVEL, Some(self.level.to_string()));
        metadata.set(
            MEMBER_VOLUME_LEVELS,
            Some(
                self.member_levels
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>(),
            ),
        );
        metadata.set(QUEUE_VERSION, Some(self.queue_version.clone()));
        metadata.set(INPUT_FORMAT, Some(self.input_format.clone()));
        metadata.set(ON_TV_INPUT, Some(self.on_tv_input));
        metadata.set(HAS_TV_INPUT, Some(self.has_tv_input));
        // Left out entirely when unknown, rather than sent as false: see the
        // `Option` on the fields.
        metadata.set(NIGHT_MODE, self.night_mode);
        metadata.set(ENHANCE_DIALOG, self.enhance_dialog);
        metadata
    }
}

/// The MPRIS face of one group. Commands go straight to the player; state is
/// whatever its events last said.
pub struct RoomPlayer {
    connection: Connection,
    pub group_id: String,
    pub room: String,
    state: Mutex<RoomState>,
}

fn failed(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

/// As [`failed`], for the property setters, which return `zbus::Result`.
fn set_failed(e: anyhow::Error) -> zbus::Error {
    failed(e).into()
}

impl RoomPlayer {
    pub fn new(
        connection: Connection,
        group_id: String,
        room: String,
        members: Vec<(String, String)>,
        has_tv_input: bool,
    ) -> Self {
        let member_volumes = vec![0; members.len()];
        let member_muted = vec![false; members.len()];
        let member_fixed = vec![false; members.len()];
        let member_levels = vec![0; members.len()];
        Self {
            connection,
            group_id,
            room,
            state: Mutex::new(RoomState {
                members,
                member_volumes,
                member_muted,
                member_fixed,
                member_levels,
                has_tv_input,
                ..RoomState::default()
            }),
        }
    }

    /// The players in this group, for deciding whether a republish is due.
    pub fn member_ids(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .members
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Fold one member's `playerVolume` in; returns the properties to announce,
    /// or nothing when the level has not actually moved.
    pub fn apply_member_volume(&self, player_id: &str, volume: &proto::Volume) -> Vec<Property> {
        let mut state = self.state.lock().unwrap();
        let Some(at) = state.members.iter().position(|(id, _)| id == player_id) else {
            return Vec::new();
        };
        // Muted reads as nothing heard, matching how group volume is reported;
        // the flag beside it is what says which of the two it is.
        let level = if volume.muted { 0 } else { volume.volume };
        if state.member_volumes.get(at) == Some(&level)
            && state.member_muted.get(at) == Some(&volume.muted)
            && state.member_fixed.get(at) == Some(&volume.fixed)
            && state.member_levels.get(at) == Some(&volume.volume)
        {
            return Vec::new();
        }
        state.member_volumes[at] = level;
        if let Some(slot) = state.member_muted.get_mut(at) {
            *slot = volume.muted;
        }
        if let Some(slot) = state.member_fixed.get_mut(at) {
            *slot = volume.fixed;
        }
        if let Some(slot) = state.member_levels.get_mut(at) {
            *slot = volume.volume;
        }
        state.announce().into_iter().collect()
    }

    /// Fold a `homeTheater:1` options body in, for the member that owns the TV
    /// socket - which need not be the coordinator, so this is matched by player
    /// the way [`Self::apply_member_volume`] is, and a body for someone else's
    /// player is not this room's news.
    pub fn apply_home_theater(
        &self,
        player_id: &str,
        update: &proto::HomeTheaterUpdate,
    ) -> Vec<Property> {
        let mut state = self.state.lock().unwrap();
        if !state.members.iter().any(|(id, _)| id == player_id) {
            return Vec::new();
        }
        // A field the body did not mention keeps what the room had: see
        // `proto::HomeTheaterUpdate` for the confirmation event that made this
        // the difference between "dialog on" and silently the reverse.
        let night = update.night_mode.or(state.night_mode);
        let dialog = update.enhance_dialog.or(state.enhance_dialog);
        if (night, dialog) == (state.night_mode, state.enhance_dialog) {
            return Vec::new();
        }
        state.night_mode = night;
        state.enhance_dialog = dialog;
        state.announce().into_iter().collect()
    }

    /// Fold a `playbackStatus` event in; returns the MPRIS properties to announce.
    pub fn apply_playback(&self, status: &proto::PlaybackStatus) -> Vec<Property> {
        self.state.lock().unwrap().apply_playback(status)
    }

    /// Read the queue's real version over UPnP, and say so if it moved.
    ///
    /// The players do not send one. `playbackStatus.queueVersion` is the field
    /// this was designed around and firmware 95.0-77060 omits it entirely, in
    /// the polled response and in events alike - see [`QUEUE_VERSION`]. UPnP
    /// does have it, as the `UpdateID` on a `Q:0` browse, which is what every
    /// queue mutation already reads before acting.
    ///
    /// **Called on a playback event, not on a timer.** That is the whole of the
    /// no-polling promise this keeps: the read rides an event the daemon was
    /// already handling, so a room doing nothing costs nothing. The price is one
    /// small SOAP browse per playback event, which is a state change or a track
    /// boundary rather than anything frequent.
    ///
    /// **What it therefore catches, and does not.** Anything that moves playback
    /// is seen - the Sonos app's Play Now, a track advancing, a queue cleared
    /// under a playing room. A silent append to a room that keeps playing what
    /// it was emits no event and is still missed until something else happens.
    /// Closing that needs UPnP GENA eventing, which needs the players to reach
    /// an HTTP callback here, which Omarchy's default-deny firewall does not
    /// allow - the same wall the cloud-queue note runs into.
    ///
    /// Never fails a caller: a browse that does not answer means the version is
    /// simply not updated this time round.
    pub async fn refresh_queue_version(&self) -> Option<Property> {
        let version = Upnp::new(self.connection.ip()).update_id().await.ok()?;
        let mut state = self.state.lock().unwrap();
        if state.queue_version == version {
            return None;
        }
        state.queue_version = version;
        state.announce()
    }

    /// Fold a `metadataStatus` event in.
    pub fn apply_metadata(&self, meta: &MetadataStatus) -> Vec<Property> {
        let mut state = self.state.lock().unwrap();
        state.metadata = to_metadata(&self.group_id, meta);
        // The format is present only on the TV input, but its summary can be
        // empty there too (no codec named, no channels yet), so being on TV is
        // its presence, not its wording.
        let format = meta
            .container
            .as_ref()
            .and_then(|c| c.ht_input_format.as_ref());
        state.on_tv_input = format.is_some();
        state.input_format = format.map(|f| f.summary()).unwrap_or_default();
        // No item and no container is nothing loaded. A URL stream has no item
        // but names its host in the container; the TV input has no item but its
        // format in the container; only an emptied room has neither.
        let container_names_something = meta
            .container
            .as_ref()
            .is_some_and(|c| c.id.is_some() || c.name.is_some());
        state.no_source =
            meta.current_item.is_none() && !container_names_something && format.is_none();
        state.announce().into_iter().collect()
    }

    /// Fold a `groupVolume` event in.
    pub fn apply_volume(&self, volume: &proto::Volume) -> Vec<Property> {
        self.state.lock().unwrap().apply_volume(volume)
    }

    async fn playback(&self, command: &str) -> fdo::Result<()> {
        self.connection
            .playback(&self.group_id, command)
            .await
            .map_err(failed)
    }

    /// Refuse, rather than silently send, a mode the current source cannot do.
    /// The same sentence the CLI prints, so both faces of x2rock agree.
    fn cannot(&self, what: &str) -> zbus::Error {
        fdo::Error::NotSupported(format!("what {} is playing cannot be {what}", self.room)).into()
    }
}

fn to_metadata(group_id: &str, meta: &MetadataStatus) -> Metadata {
    let track = meta.current_item.as_ref().and_then(|i| i.track.as_ref());
    let container = meta.container.as_ref();
    let title = track
        .and_then(|t| t.name.as_deref())
        .or_else(|| container.and_then(|c| c.name.as_deref()));
    let artist = track
        .and_then(|t| t.artist.as_ref())
        .and_then(|a| a.name.as_deref());
    let album = track
        .and_then(|t| t.album.as_ref())
        .and_then(|a| a.name.as_deref());
    let art = track
        .and_then(|t| t.image_url.as_deref())
        .or_else(|| container.and_then(|c| c.image_url.as_deref()));

    let mut builder = Metadata::builder().trackid(track_id(group_id, title, artist));
    if let Some(title) = title {
        builder = builder.title(title);
    }
    if let Some(artist) = artist {
        builder = builder.artist([artist]);
    }
    if let Some(album) = album {
        builder = builder.album(album);
    }
    if let Some(art) = art {
        builder = builder.art_url(art);
    }
    if let Some(ms) = track.and_then(|t| t.duration_millis) {
        builder = builder.length(Time::from_millis(ms as i64));
    }
    let mut metadata = builder.build();
    metadata.set(LIVE_STREAM, Some(is_live_stream(meta)));
    metadata.set(
        STATION_NAME,
        Some(station_name(meta, title).unwrap_or_default().to_owned()),
    );
    metadata.set(
        STREAM_INFO,
        Some(
            meta.stream_info
                .as_deref()
                .map(str::trim)
                .filter(|info| !info.is_empty())
                .unwrap_or_default()
                .to_owned(),
        ),
    );
    metadata
}

/// Whether a `metadataStatus` describes a live stream - internet radio, and
/// anything else the player resolves continuously rather than as an item.
///
/// **`container.type` is the whole of it, and it is verified.** Three captures
/// off the Media Room, 2026-09-01:
///
/// | | `container.type` | `currentItem` | `objectId` |
/// |---|---|---|---|
/// | YouTube Music track | `track` | present, with `durationMillis` | real |
/// | TuneIn "Jazz Club" | `station` | absent | `-1` |
/// | Sonos Radio "Sound System" | `station` | **present, with name and artist** | **`97034`** |
///
/// The third is the control, and it settled two things at once. It was started
/// from the Sonos app, so nothing x2rock sent was in the loop - which rules out
/// the player merely echoing back the `"type": "station"` that `loadStreamUrl`
/// puts in its own `stationMetadata`. The container type is the player's own
/// vocabulary.
///
/// It also killed two signals an earlier version of this comment leaned on. A
/// missing `currentItem` and `objectId "-1"` are **TuneIn's** shape, not a live
/// stream's: Sonos Radio streams a named track by a named artist and still has
/// no duration and no end. Only the container type and the absent duration
/// survive all three, and the duration is not the question anyway (see
/// [`LIVE_STREAM`]).
fn is_live_stream(meta: &MetadataStatus) -> bool {
    meta.container
        .as_ref()
        .and_then(|c| c.kind.as_deref())
        .is_some_and(|kind| kind == "station")
}

/// The station name, when a client showing the title would not already have it.
///
/// Only for a live stream, and only when the container names something the
/// title does not already say - see [`STATION_NAME`].
fn station_name<'a>(meta: &'a MetadataStatus, title: Option<&str>) -> Option<&'a str> {
    if !is_live_stream(meta) {
        return None;
    }
    let name = meta.container.as_ref()?.name.as_deref()?.trim();
    (!name.is_empty() && Some(name) != title).then_some(name)
}

/// MPRIS wants an object path per track. Sonos ids are not valid path segments,
/// so derive one from what identifies the track to a listener.
fn track_id(group_id: &str, title: Option<&str>, artist: Option<&str>) -> TrackId {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (title, artist).hash(&mut hasher);
    let group: String = group_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = format!("/com/rahga/x2rock/{group}/track/{:x}", hasher.finish());
    TrackId::try_from(path).expect("path built only from [A-Za-z0-9_/]")
}

impl RootInterface for RoomPlayer {
    async fn raise(&self) -> fdo::Result<()> {
        Ok(())
    }
    async fn quit(&self) -> fdo::Result<()> {
        Ok(())
    }
    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(false)
    }
    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }
    async fn set_fullscreen(&self, _fullscreen: bool) -> zbus::Result<()> {
        Ok(())
    }
    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }
    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }
    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }
    async fn identity(&self) -> fdo::Result<String> {
        Ok(self.room.clone())
    }
    /// The basename of the desktop file an MPRIS client reads for this player's
    /// app name and icon. It must match a `x2rock.desktop` installed on the
    /// machine (shipped in `desktop/`); without it GNOME and KDE fail the
    /// lookup and render every room player generically. `identity` gives the
    /// per-room label; this gives the shared app identity.
    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok("x2rock".into())
    }
    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }
    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }
}

impl PlayerInterface for RoomPlayer {
    async fn next(&self) -> fdo::Result<()> {
        self.playback("skipToNextTrack").await
    }
    async fn previous(&self) -> fdo::Result<()> {
        self.playback("skipToPreviousTrack").await
    }
    async fn pause(&self) -> fdo::Result<()> {
        self.playback("pause").await
    }
    async fn play_pause(&self) -> fdo::Result<()> {
        self.playback("togglePlayPause").await
    }
    async fn stop(&self) -> fdo::Result<()> {
        // Sonos has no stop distinct from pause for most sources.
        self.playback("pause").await
    }
    async fn play(&self) -> fdo::Result<()> {
        self.playback("play").await
    }
    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        self.connection
            .seek_by(&self.group_id, offset.as_millis())
            .await
            .map_err(failed)
    }
    async fn set_position(&self, _track_id: TrackId, position: Time) -> fdo::Result<()> {
        self.connection
            .seek_to(&self.group_id, position.as_millis().max(0) as u64)
            .await
            .map_err(failed)
    }
    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("x2rock cannot open URIs".into()))
    }
    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .status
            .unwrap_or(PlaybackStatus::Stopped))
    }
    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(self.state.lock().unwrap().loop_status())
    }
    async fn set_loop_status(&self, loop_status: LoopStatus) -> zbus::Result<()> {
        let repeat = match loop_status {
            LoopStatus::None => Repeat::Off,
            LoopStatus::Playlist => Repeat::All,
            LoopStatus::Track => Repeat::One,
        };
        let actions = self.state.lock().unwrap().actions;
        if !actions.allows(repeat) {
            return Err(self.cannot(repeat.denied_as()));
        }
        self.connection
            .set_repeat(&self.group_id, repeat)
            .await
            .map_err(set_failed)
    }
    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }
    async fn set_rate(&self, _rate: PlaybackRate) -> zbus::Result<()> {
        Ok(())
    }
    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.state.lock().unwrap().play_modes.shuffle)
    }
    async fn set_shuffle(&self, shuffle: bool) -> zbus::Result<()> {
        // Turning it off is always allowed, as with repeat.
        if shuffle && !self.state.lock().unwrap().actions.can_shuffle {
            return Err(self.cannot("shuffled"));
        }
        self.connection
            .set_shuffle(&self.group_id, shuffle)
            .await
            .map_err(set_failed)
    }
    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(self.state.lock().unwrap().with_hints())
    }
    async fn volume(&self) -> fdo::Result<f64> {
        Ok(self.state.lock().unwrap().heard())
    }
    async fn set_volume(&self, volume: f64) -> zbus::Result<()> {
        let level = (volume.clamp(0.0, 1.0) * 100.0).round() as u8;
        self.connection
            .set_group_volume(&self.group_id, level)
            .await
            .map_err(set_failed)
    }
    async fn position(&self) -> fdo::Result<Time> {
        let state = self.state.lock().unwrap();
        let mut millis = state.position_millis;
        if state.status == Some(PlaybackStatus::Playing)
            && let Some(at) = state.position_at
        {
            millis += at.elapsed().as_millis() as u64;
        }
        Ok(Time::from_millis(millis as i64))
    }
    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }
    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }
    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.state.lock().unwrap().actions.can_skip)
    }
    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.state.lock().unwrap().actions.can_skip_back)
    }
    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(self.state.lock().unwrap().actions.can_play)
    }
    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(self.state.lock().unwrap().actions.can_pause)
    }
    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.state.lock().unwrap().actions.can_seek)
    }
    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug the raw-event capture of 2026-09-03/04 turned up. A body that
    /// carries no `availablePlaybackActions` and no `playModes` used to be read
    /// as a source that allows *nothing* - because both fields defaulted to
    /// all-false and were assigned unconditionally. Every failed stream sent
    /// one, so a `playbackError` blanked the room's capabilities and reported
    /// its queue as neither repeating nor shuffling.
    #[test]
    fn a_body_that_omits_the_actions_leaves_the_capabilities_standing() {
        let mut state = RoomState::default();
        let full: proto::PlaybackStatus = serde_json::from_str(
            r#"{"playbackState":"PLAYBACK_STATE_PLAYING","positionMillis":1000,
                "availablePlaybackActions":{"canPlay":true,"canPause":true,"canSkip":true,
                    "canSkipBack":true,"canSeek":true,"canRepeat":true,"canShuffle":true},
                "playModes":{"repeat":true,"shuffle":true}}"#,
        )
        .unwrap();
        state.apply_playback(&full);
        assert!(state.actions.can_skip);
        assert!(matches!(state.loop_status(), LoopStatus::Playlist));

        // Now a body with neither field - a partial status, or the error body
        // that parses as one.
        let partial: proto::PlaybackStatus =
            serde_json::from_str(r#"{"positionMillis":2000}"#).unwrap();
        let properties = state.apply_playback(&partial);

        // Kept, not reset: the source still allows what it allowed a moment ago.
        assert!(state.actions.can_skip);
        assert!(state.actions.can_pause);
        assert!(state.play_modes.shuffle);
        assert!(matches!(state.loop_status(), LoopStatus::Playlist));
        // The position did move, since that field was there.
        assert_eq!(state.position_millis, 2000);

        // And MPRIS is told what is still true rather than all-false, which is
        // the half a caller actually sees.
        for property in &properties {
            match property {
                Property::CanGoNext(v)
                | Property::CanGoPrevious(v)
                | Property::CanPlay(v)
                | Property::CanPause(v)
                | Property::CanSeek(v)
                | Property::Shuffle(v) => {
                    assert!(v, "{property:?} was published as false");
                }
                _ => {}
            }
        }
        assert!(
            properties
                .iter()
                .any(|p| matches!(p, Property::LoopStatus(LoopStatus::Playlist)))
        );
        // The hints did not move and the queue did not either, so Metadata is
        // not re-announced.
        assert!(
            !properties
                .iter()
                .any(|p| matches!(p, Property::Metadata(_)))
        );
    }

    #[test]
    fn a_state_and_a_position_still_mean_unchanged_when_absent() {
        let mut state = RoomState::default();
        let playing: proto::PlaybackStatus = serde_json::from_str(
            r#"{"playbackState":"PLAYBACK_STATE_PLAYING","positionMillis":5000}"#,
        )
        .unwrap();
        state.apply_playback(&playing);

        // A body with neither must not announce the room stopped at zero.
        let quiet: proto::PlaybackStatus = serde_json::from_str("{}").unwrap();
        let properties = state.apply_playback(&quiet);
        assert_eq!(state.status, Some(PlaybackStatus::Playing));
        assert_eq!(state.position_millis, 5000);
        assert!(
            !properties
                .iter()
                .any(|p| matches!(p, Property::PlaybackStatus(_))),
            "an unchanged state is not re-announced"
        );
    }

    /// The bug that made `announce` exist: a level that moves has to be
    /// announced, because a client caches what a signal names. Deciding that
    /// per-field is what got it wrong, so this asks the question the way a
    /// client does - did the metadata I was handed change?
    #[test]
    fn a_volume_that_moves_is_announced_and_one_that_does_not_is_not() {
        let mut state = RoomState::default();
        let at = |volume: u8, muted: bool| proto::Volume {
            volume,
            muted,
            fixed: false,
        };
        // Published as a decimal string, for the reason MEMBER_VOLUMES gives.
        let level_of = |properties: &[Property]| {
            properties.iter().find_map(|p| match p {
                Property::Metadata(metadata) => metadata
                    .get_value(VOLUME_LEVEL)
                    .and_then(|value| <&str>::try_from(value).ok())
                    .map(str::to_owned),
                _ => None,
            })
        };

        let first = state.apply_volume(&at(25, false));
        assert_eq!(level_of(&first).as_deref(), Some("25"));
        assert_eq!(state.heard(), 0.25);

        // The same event again says nothing new.
        assert_eq!(level_of(&state.apply_volume(&at(25, false))), None);

        // Muting moves the heard volume to zero and keeps the level, and both
        // facts go out.
        let muted = state.apply_volume(&at(25, true));
        assert_eq!(state.heard(), 0.0);
        assert_eq!(level_of(&muted).as_deref(), Some("25"));

        // A level that moves *under* mute moves no heard volume at all, which
        // is the case a Volume-only signal cannot carry.
        let quietly = state.apply_volume(&at(40, true));
        assert_eq!(state.heard(), 0.0);
        assert_eq!(level_of(&quietly).as_deref(), Some("40"));
    }

    #[test]
    fn bus_suffix_matches_the_kotlin_convention() {
        assert_eq!(bus_suffix("Media Room"), "x2rock-media-room");
        assert_eq!(bus_suffix("Kitchen"), "x2rock-kitchen");
        assert_eq!(bus_suffix("Björn's Den!!"), "x2rock-bj-rn-s-den");
        assert_eq!(bus_suffix("  "), "x2rock");
    }

    #[test]
    fn colliding_rooms_get_numbered_and_the_first_keeps_the_plain_name() {
        let mut taken = HashSet::new();
        for (room, expected) in [
            ("Kitchen", "x2rock-kitchen"),
            ("kitchen", "x2rock-kitchen-2"),
            ("Kitchen!", "x2rock-kitchen-3"),
            ("Media Room", "x2rock-media-room"),
            ("Media-Room", "x2rock-media-room-2"),
            // Two names with nothing ASCII in them both reduce to the bare prefix.
            ("書斎", "x2rock"),
            ("寝室", "x2rock-2"),
        ] {
            let suffix = bus_suffix_among(room, &taken);
            assert_eq!(suffix, expected, "{room:?}");
            taken.insert(suffix);
        }
    }

    /// Both shapes are trimmed from real `getMetadataStatus` responses off the
    /// Media Room, 2026-09-01 - the capture the detector was written against.
    fn status(json: &str) -> MetadataStatus {
        serde_json::from_str(json).expect("fixture parses as metadataStatus")
    }

    /// Read the published flag back out the way a client would.
    fn live_flag(md: &Metadata) -> Option<bool> {
        md.get::<bool>(LIVE_STREAM)?.ok().copied()
    }

    #[test]
    fn a_station_is_a_live_stream_and_a_track_is_not() {
        // TuneIn "Jazz Club": no currentItem at all, and the player's own
        // objectId "-1" for a container it has nothing to say about.
        let station = status(
            r#"{"container":{"id":{"objectId":"-1"},"name":"Jazz Club",
                 "type":"station","service":{"id":"254","name":"TuneIn"}}}"#,
        );
        assert!(is_live_stream(&station));

        // YouTube Music "Bodies": a real object id, and a duration to go with it.
        let track = status(
            r#"{"container":{"id":{"objectId":"ALkSOiGTPQu20Hqb","accountId":"sn_2",
                 "serviceId":"284"},"name":"Bodies","type":"track"},
                "currentItem":{"track":{"name":"Bodies","durationMillis":179000}}}"#,
        );
        assert!(!is_live_stream(&track));
    }

    #[test]
    fn sonos_radio_is_a_stream_even_though_it_names_a_track() {
        // The control, started from the Sonos app on 2026-09-01 - nothing
        // x2rock sent was in the loop, which is what makes container.type the
        // player's own word rather than an echo of our stationMetadata. It also
        // has a real objectId and a currentItem, both of which an earlier
        // version of this detector wrongly treated as signs of on-demand.
        let radio = status(
            r#"{"container":{"id":{"objectId":"97034","accountId":"sn_1","serviceId":"303"},
                 "name":"Sound System","type":"station",
                 "service":{"id":"303","name":"Sonos Radio"}},
                "currentItem":{"track":{"name":"Intervallo (from \"Veruschka\") (II)",
                 "artist":{"name":"Ennio Morricone"}}}}"#,
        );
        assert!(is_live_stream(&radio));
        // The title is the track, so the station would otherwise go unsaid.
        assert_eq!(
            station_name(&radio, Some("Intervallo (from \"Veruschka\") (II)")),
            Some("Sound System")
        );
    }

    #[test]
    fn a_station_that_is_already_the_title_is_not_repeated() {
        // TuneIn has no track, so to_metadata falls back to the container name
        // and the title already is the station. Saying it twice is noise.
        let tunein = status(r#"{"container":{"name":"Jazz Club","type":"station"}}"#);
        assert_eq!(station_name(&tunein, Some("Jazz Club")), None);
        // And an on-demand album never names a station, whatever it is called.
        let track = status(r#"{"container":{"name":"Bodies","type":"track"}}"#);
        assert_eq!(station_name(&track, Some("Bodies")), None);
        assert_eq!(station_name(&track, Some("Something else")), None);
    }

    #[test]
    fn nothing_playing_is_not_a_live_stream() {
        // An empty status and a container that names no type both mean "no
        // reason to think so", which must not read as a station.
        assert!(!is_live_stream(&status("{}")));
        assert!(!is_live_stream(&status(
            r#"{"container":{"name":"Something"}}"#
        )));
    }

    #[test]
    fn the_live_stream_flag_reaches_the_published_metadata() {
        // The point of the detector is the key a client reads, so assert on
        // that rather than on the function behind it.
        let station = status(r#"{"container":{"name":"Jazz Club","type":"station"}}"#);
        let md = to_metadata("RINCON_48A6:836412709", &station);
        assert_eq!(live_flag(&md), Some(true));

        let track = status(r#"{"container":{"name":"Bodies","type":"track"}}"#);
        let md = to_metadata("RINCON_48A6:836412709", &track);
        assert_eq!(live_flag(&md), Some(false));
    }

    #[test]
    fn track_ids_are_valid_paths_and_stable() {
        let a = track_id(
            "RINCON_48A6:836412709",
            Some("Espresso"),
            Some("Sabrina Carpenter"),
        );
        let b = track_id(
            "RINCON_48A6:836412709",
            Some("Espresso"),
            Some("Sabrina Carpenter"),
        );
        let c = track_id("RINCON_48A6:836412709", Some("Other"), None);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(
            a.as_str()
                .starts_with("/com/rahga/x2rock/RINCON_48A6_836412709/track/")
        );
    }
}
