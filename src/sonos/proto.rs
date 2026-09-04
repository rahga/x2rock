//! Wire types for the Sonos Control API.
//!
//! The same JSON protocol is spoken by the cloud API and by players on the LAN.
//! Every exchange is a two-element array: `[header, body]`.
//!
//! These mirror the wire format rather than current use, so some fields are
//! deserialized before anything reads them.
#![allow(dead_code)]

use std::fmt;
use std::net::IpAddr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// First element of every response and event.
///
/// `Serialize` is here for `x2rock raw`, which prints the header back out: a
/// probe that hid the header would hide half of what it went to find.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    pub namespace: Option<String>,
    pub household_id: Option<String>,
    pub group_id: Option<String>,
    pub player_id: Option<String>,
    /// Echoed from the command, so replies can be matched to callers.
    pub cmd_id: Option<String>,
    pub response: Option<String>,
    pub success: Option<bool>,
    /// Present on events; also `globalError` on failures.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

/// Something the player told us unprompted, after a `subscribe`.
#[derive(Debug)]
pub struct Event {
    pub namespace: String,
    /// `playbackStatus`, `metadataStatus`, `groupVolume`, `groups`, ...
    pub kind: String,
    pub group_id: Option<String>,
    pub player_id: Option<String>,
    pub body: serde_json::Value,
}

impl Event {
    /// Synthetic: the connection died. Delivered last, so listeners can stop waiting.
    pub const LOST: &str = "connectionLost";

    pub fn new(header: Header, body: serde_json::Value) -> Self {
        Self {
            namespace: header.namespace.unwrap_or_default(),
            kind: header.kind.unwrap_or_default(),
            group_id: header.group_id,
            player_id: header.player_id,
            body,
        }
    }

    pub fn lost() -> Self {
        Self {
            namespace: "x2rock".into(),
            kind: Self::LOST.into(),
            group_id: None,
            player_id: None,
            body: serde_json::Value::Null,
        }
    }
}

/// Body of a failed command.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub error_code: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Groups {
    pub groups: Vec<Group>,
    pub players: Vec<Player>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub name: String,
    pub coordinator_id: String,
    /// Present in `getGroups` replies, absent from `groups` events.
    #[serde(default)]
    pub playback_state: String,
    pub player_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    pub id: String,
    pub name: String,
    /// Each player reports its own URL, so reaching one player reveals them all.
    pub websocket_url: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl Player {
    /// The player's address, taken from its own `wss://<ip>:1443/...` URL.
    pub fn ip(&self) -> Option<IpAddr> {
        let rest = self.websocket_url.strip_prefix("wss://")?;
        let host = rest.split([':', '/']).next()?;
        host.parse().ok()
    }
}

impl Groups {
    pub fn player(&self, id: &str) -> Option<&Player> {
        self.players.iter().find(|p| p.id == id)
    }

    pub fn members(&self, group: &Group) -> Vec<&Player> {
        group
            .player_ids
            .iter()
            .filter_map(|id| self.player(id))
            .collect()
    }

    /// The group to act on: the one containing a player with this room name, or
    /// with no name given, the only group there is.
    pub fn resolve(&self, room: Option<&str>) -> Result<&Group> {
        match room {
            Some(name) => {
                let wanted = name.to_lowercase();
                self.groups
                    .iter()
                    .find(|g| {
                        self.members(g)
                            .iter()
                            .any(|p| p.name.to_lowercase() == wanted)
                    })
                    .ok_or_else(|| self.unknown_room(name))
            }
            None => match self.groups.as_slice() {
                [only] => Ok(only),
                [] => bail!("this household has no groups"),
                _ => bail!(
                    "this household has several groups; choose one with --room. Rooms: {}",
                    self.room_names()
                ),
            },
        }
    }

    /// The player with this name. Rooms are players, but groups are named after
    /// whichever player coordinates them, so grouping has to resolve players
    /// rather than groups - "Kitchen" the speaker outlives "Kitchen" the group.
    pub fn player_named(&self, name: &str) -> Result<&Player> {
        let wanted = name.to_lowercase();
        self.players
            .iter()
            .find(|p| p.name.to_lowercase() == wanted)
            .ok_or_else(|| self.unknown_room(name))
    }

    /// The `unknown_room` error, shared by every name-resolution site: the room
    /// list and the typo-tolerant near-misses ride along in its `data`, so an
    /// agent fixes a mistyped name from this one reply. Both `resolve` and
    /// `player_named` raise it, so they cannot word or shape it differently.
    fn unknown_room(&self, name: &str) -> anyhow::Error {
        let rooms: Vec<&str> = self.players.iter().map(|p| p.name.as_str()).collect();
        let did_you_mean = near_matches(name, &rooms);
        crate::hint::Hint::new(
            format!("no room named {name:?}. Rooms: {}", rooms.join(", ")),
            "unknown_room",
            Some("x2rock rooms".into()),
        )
        .with_data(serde_json::json!({
            "did_you_mean": did_you_mean,
            "rooms": rooms,
        }))
        .into()
    }

    /// The group a player currently belongs to.
    pub fn group_of(&self, player_id: &str) -> Option<&Group> {
        self.groups
            .iter()
            .find(|g| g.player_ids.iter().any(|id| id == player_id))
    }

    pub fn room_names(&self) -> String {
        self.players
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Room names close enough to a mistyped one to be worth suggesting: a small
/// edit distance (so "bedoom" finds "Bedroom") or one name containing the other
/// (so a fragment finds its room). Ordered nearest-first, and capped, because a
/// long list of guesses is no better than none.
fn near_matches(query: &str, candidates: &[&str]) -> Vec<String> {
    let q = query.to_lowercase();
    let mut scored: Vec<(usize, &str)> = candidates
        .iter()
        .filter_map(|&name| {
            let lower = name.to_lowercase();
            let distance = levenshtein(&q, &lower);
            // A third of the longer word (at least one edit), or a containment
            // either way - loose enough for a typo, tight enough to stay useful.
            let tolerance = (q.len().max(lower.len()) / 3).max(1);
            (distance <= tolerance || lower.contains(&q) || q.contains(&lower))
                .then_some((distance, name))
        })
        .collect();
    scored.sort_by_key(|&(distance, _)| distance);
    scored
        .into_iter()
        .take(3)
        .map(|(_, n)| n.to_string())
        .collect()
}

/// Ordinary Levenshtein edit distance, two rolling rows.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// `playback:1 getPlaybackStatus`, and the body of `playbackStatus` events.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackStatus {
    /// **Absent from some event bodies**, though present in every
    /// `getPlaybackStatus` reply seen here. Observed missing four times in a day
    /// on firmware 95.0-77060 (2026-09-02/03), which used to fail the whole
    /// event's deserialization and drop it - position, play modes and the
    /// queue-version refresh going with it, leaving MPRIS stale until the next
    /// event.
    ///
    /// `None` therefore means *unchanged*, never stopped: a consumer keeps the
    /// state it already had. Defaulting it to an empty string instead would
    /// report a playing room as stopped, because that is the arm an unknown
    /// state falls into.
    pub playback_state: Option<String>,
    /// `None` means unchanged, for the same reason and in the same events: a
    /// partial body omits this alongside `playback_state`, and defaulting it to
    /// 0 would rewind the position every time one arrived.
    pub position_millis: Option<u64>,
    /// Bumps whenever the queue changes - the cue to re-read it over UPnP.
    pub queue_version: Option<String>,
    /// The current item's id, which is **the 1-based queue position** whenever
    /// the queue is what is driving, and an opaque hash otherwise (a radio
    /// stream, a service station).
    ///
    /// Verified by moving a track: removing the queue's second entry left "Zero
    /// to Hero" at position 2 instead of 3, and its `itemId` read 2 - so this
    /// follows the position rather than identifying an item, and renumbers when
    /// the queue is edited. See [`Self::queue_position`].
    pub item_id: Option<String>,
    /// `None` means unchanged, for the same reason as the fields above.
    ///
    /// This used to be `#[serde(default)]`, which is all-false - "nothing is
    /// allowed". A body that simply does not mention the actions was therefore
    /// read as a source that cannot play, pause, skip or seek, and a consumer
    /// tracking state across events published exactly that. A `playbackError`
    /// carries none of these fields, so every failed stream did it.
    pub available_playback_actions: Option<PlaybackActions>,
    /// `None` means unchanged, as above: defaulting to all-false reported a
    /// queue on repeat as a queue on nothing.
    pub play_modes: Option<PlayModes>,
}

/// What the current source allows. A radio stream, for instance, cannot skip,
/// and neither it nor a service's station can be shuffled or repeated.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackActions {
    #[serde(default)]
    pub can_play: bool,
    #[serde(default)]
    pub can_pause: bool,
    #[serde(default)]
    pub can_skip: bool,
    #[serde(default)]
    pub can_skip_back: bool,
    #[serde(default)]
    pub can_seek: bool,
    #[serde(default)]
    pub can_repeat: bool,
    #[serde(default)]
    pub can_repeat_one: bool,
    #[serde(default)]
    pub can_shuffle: bool,
}

impl PlaybackActions {
    /// Turning repeat off is always allowed; turning it on is not. One place for
    /// the rule, so the CLI and the MPRIS setter cannot drift apart.
    pub fn allows(&self, repeat: Repeat) -> bool {
        match repeat {
            Repeat::Off => true,
            Repeat::All => self.can_repeat,
            Repeat::One => self.can_repeat_one,
        }
    }
}

/// How the queue is traversed. Sonos keeps `repeat` and `repeatOne` as two flags;
/// [`PlayModes::repeat`] folds them into the one three-way setting that the
/// Sonos app, MPRIS `LoopStatus` and everyone else actually present.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayModes {
    #[serde(default)]
    pub repeat: bool,
    #[serde(default)]
    pub repeat_one: bool,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub crossfade: bool,
}

impl PlayModes {
    pub fn repeat(&self) -> Repeat {
        if self.repeat_one {
            Repeat::One
        } else if self.repeat {
            Repeat::All
        } else {
            Repeat::Off
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    #[default]
    Off,
    /// The whole queue.
    All,
    /// The current track.
    One,
}

impl Repeat {
    pub fn as_str(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
    }

    pub fn parse(text: &str) -> Option<Repeat> {
        match text {
            "off" => Some(Repeat::Off),
            "all" => Some(Repeat::All),
            "one" => Some(Repeat::One),
            _ => None,
        }
    }

    /// Fills in "what <room> is playing cannot be ___", so the CLI error and the
    /// D-Bus `NotSupported` say the same thing.
    pub fn denied_as(self) -> &'static str {
        match self {
            Repeat::Off => "unrepeated",
            Repeat::All => "repeated",
            Repeat::One => "repeated one track at a time",
        }
    }
}

impl PlaybackStatus {
    /// `PLAYBACK_STATE_PLAYING` -> `PLAYING`, and `None` when the player sent
    /// no state at all - see [`PlaybackStatus::playback_state`].
    pub fn state(&self) -> Option<&str> {
        self.playback_state
            .as_deref()
            .map(|state| state.strip_prefix("PLAYBACK_STATE_").unwrap_or(state))
    }

    /// The actions the source allows, reading an absent set as all-false.
    ///
    /// For a one-shot reader - the CLI, off a `getPlaybackStatus` reply, which
    /// has always carried them. Anything tracking state across events wants the
    /// `Option` itself, because there "absent" means keep what you had.
    pub fn actions(&self) -> PlaybackActions {
        self.available_playback_actions.unwrap_or_default()
    }

    /// The play modes, reading an absent set as all-false. Same caveat as
    /// [`Self::actions`].
    pub fn modes(&self) -> PlayModes {
        self.play_modes.unwrap_or_default()
    }

    /// Where in the queue this is, 1-based, or `None` when the queue is not the
    /// source.
    ///
    /// `itemId` carries the position for a queue and an opaque hash for a
    /// stream, so parsing is the discriminator: a hash never reads as a number.
    /// The *length* is not here - that needs the queue itself, over UPnP - so
    /// this is a position without a total on purpose.
    pub fn queue_position(&self) -> Option<u32> {
        self.item_id.as_deref()?.parse().ok()
    }
}

/// The other body `playback:1` delivers: a failure to play something.
///
/// Every field is optional because the players do not send them all - a second
/// error moments after the first arrived here with no `itemId` at all.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackError {
    pub error_code: Option<String>,
    pub reason: Option<String>,
    pub track_name: Option<String>,
    pub service_name: Option<String>,
}

impl fmt::Display for PlaybackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `reason` is the specific half (ERROR_CANT_REACH_SERVER) and
        // `errorCode` the general one (ERROR_PLAYBACK_FAILED), so prefer the
        // first and fall back rather than printing both.
        let why = self
            .reason
            .as_deref()
            .or(self.error_code.as_deref())
            .unwrap_or("no reason given");
        match self.track_name.as_deref() {
            Some(track) => write!(f, "{why} on {track:?}"),
            None => write!(f, "{why}"),
        }
    }
}

/// A `playback:1` body read as an error, or `None` if it is a status.
///
/// The namespace carries both shapes and only `_objectType` tells them apart.
/// It matters because a `PlaybackError` deserializes *cleanly* into a
/// `PlaybackStatus` - every field of one is optional and none of them appears
/// in the other - so an error folds in silently as "nothing changed" unless it
/// is caught here first.
pub fn playback_error(body: &serde_json::Value) -> Option<PlaybackError> {
    if body.get("_objectType")?.as_str()? != "playbackError" {
        return None;
    }
    serde_json::from_value(body.clone()).ok()
}

/// `playlists:1 getPlaylists`: the household's saved queues.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistsList {
    #[serde(default)]
    pub playlists: Vec<Playlist>,
}

/// One saved queue - what the Sonos app calls a Sonos playlist.
///
/// **The id here is bare** (`"0"`), where UPnP's `SaveQueue` answers `SQ:0` and
/// `queue sources` reports that form. They are not interchangeable:
/// `loadPlaylist` refuses `SQ:0` with `ERROR_INVALID_OBJECT_ID`, so a caller
/// holding the UPnP form has to resolve it through this list rather than
/// trimming the prefix and hoping.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub track_count: Option<u32>,
}

/// `playbackMetadata:1 getMetadataStatus`, and the body of `metadataStatus` events.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataStatus {
    /// The album, playlist or station being played from.
    pub container: Option<Container>,
    pub current_item: Option<QueueItem>,
    pub next_item: Option<QueueItem>,
}

/// How Sonos names a piece of a service's catalogue: what it is, which service
/// it belongs to, and which of the household's accounts on that service.
///
/// The three together are enough to enqueue the item again later, without any
/// credential of our own - the player resolves the account it already holds.
/// `accountId` reads like `sn_3`, and the same `3` appears as `sn=3` inside the
/// player's own `x-sonosapi-*` URIs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicObjectId {
    pub object_id: String,
    pub service_id: Option<String>,
    pub account_id: Option<String>,
}

impl MusicObjectId {
    /// The account serial from `sn_3`, as the `sn=` a playback URI wants.
    pub fn account_serial(&self) -> Option<&str> {
        self.account_id.as_deref()?.strip_prefix("sn_")
    }

    /// Whether this names real service content. A player reports `objectId: "-1"`
    /// for a container it has nothing to say about - a radio station's "album",
    /// for instance - and that is not something to store or replay.
    pub fn is_real(&self) -> bool {
        !self.object_id.is_empty() && self.object_id != "-1"
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    pub id: Option<MusicObjectId>,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub service: Option<Named>,
    pub image_url: Option<String>,
    /// Present only while a soundbar is on its TV input.
    pub ht_input_format: Option<HomeTheaterFormat>,
}

/// What a soundbar is actually receiving over HDMI, which is not the same as
/// what the source claims to send: a TV or streaming box that has quietly
/// dropped to stereo reports `Dolby Digital` with two channels and no LFE.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HomeTheaterFormat {
    #[serde(default)]
    pub num_ground_channels: u8,
    /// The player spells this one `numLFEChannels`, which is not the casing
    /// `rename_all` derives from the field name.
    #[serde(default, rename = "numLFEChannels")]
    pub num_lfe_channels: u8,
    #[serde(default)]
    pub num_height_channels: u8,
    /// The codec, e.g. `Dolby Digital`, `Dolby Digital Plus`, `PCM`.
    pub stream_description: Option<String>,
}

impl HomeTheaterFormat {
    /// The channel layout as people write it: `2.0`, `5.1`, `5.1.2`.
    pub fn channels(&self) -> String {
        let base = format!("{}.{}", self.num_ground_channels, self.num_lfe_channels);
        if self.num_height_channels > 0 {
            format!("{base}.{}", self.num_height_channels)
        } else {
            base
        }
    }

    /// Codec and layout together, which is the pair that tells you whether the
    /// source fell back - "Dolby Digital 2.0" rather than "Dolby Digital 5.1".
    pub fn summary(&self) -> String {
        // With the television off the player reports "No Signal" and no
        // channels at all, and "No Signal 0.0" reads worse than "No Signal".
        let silent = self.num_ground_channels == 0
            && self.num_lfe_channels == 0
            && self.num_height_channels == 0;
        match self.stream_description.as_deref() {
            Some(codec) if !codec.is_empty() && silent => codec.to_owned(),
            Some(codec) if !codec.is_empty() => format!("{codec} {}", self.channels()),
            _ if silent => String::new(),
            _ => self.channels(),
        }
    }

    /// Whether more than plain stereo is arriving.
    pub fn is_surround(&self) -> bool {
        self.num_ground_channels > 2 || self.num_lfe_channels > 0 || self.num_height_channels > 0
    }
}

#[derive(Debug, Deserialize)]
pub struct QueueItem {
    pub track: Option<Track>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: Option<MusicObjectId>,
    pub name: Option<String>,
    pub artist: Option<Named>,
    pub album: Option<Named>,
    /// Served by the player itself on port 1400 - usable as MPRIS `mpris:artUrl`.
    pub image_url: Option<String>,
    pub duration_millis: Option<u64>,
    /// The explicit-content flag, which every controller shows as a badge on
    /// the row. The player also sends `tags: ["TAG_EXPLICIT"]` beside it; this
    /// is the boolean form and the one worth reading.
    pub explicit: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct Named {
    pub name: Option<String>,
}

/// `groups:1 modifyGroupMembers`, which answers with the group as it ended up.
#[derive(Debug, Deserialize)]
pub struct GroupInfo {
    pub group: Group,
}

/// `favorites:1 getFavorites`.
#[derive(Debug, Deserialize)]
pub struct FavoritesList {
    pub items: Vec<Favorite>,
}

/// One saved favorite. Only `id` and `name` are dependable: of the 70 on the
/// household this was built against, 26 carry no service at all, and the
/// resource - and so the kind - is missing from some too.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Favorite {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub image_url: Option<String>,
    pub service: Option<Named>,
    pub resource: Option<Resource>,
}

impl Favorite {
    /// The service it plays from, when it names one.
    pub fn service(&self) -> Option<&str> {
        self.service.as_ref()?.name.as_deref()
    }

    /// `STREAM`, `PLAYLIST`, `ALBUM`, `TRACK`, `PROGRAM` - when given.
    pub fn kind(&self) -> Option<&str> {
        self.resource.as_ref()?.kind.as_deref()
    }
}

#[derive(Debug, Deserialize)]
pub struct Resource {
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

/// `groupVolume:1` / `playerVolume:1` `getVolume`, and their events.
#[derive(Debug, Deserialize)]
pub struct Volume {
    pub volume: u8,
    pub muted: bool,
    /// Line-level output with no volume control, e.g. a Port feeding an amp.
    #[serde(default)]
    pub fixed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_matches_catches_a_typo_and_ignores_the_unrelated() {
        let rooms = [
            "Bedroom",
            "Living Room",
            "Dining Room",
            "Guest TV",
            "Kitchen",
        ];
        // A one-letter typo finds its room.
        assert_eq!(near_matches("bedoom", &rooms), vec!["Bedroom"]);
        // A fragment finds its room by containment.
        assert_eq!(near_matches("kitch", &rooms), vec!["Kitchen"]);
        // Something unlike any room suggests nothing rather than a bad guess.
        assert!(near_matches("garage", &rooms).is_empty());
        // The list is capped, and ordered nearest-first.
        assert!(near_matches("room", &rooms).len() <= 3);
    }

    #[test]
    fn player_ip_comes_from_its_websocket_url() {
        let player = Player {
            id: "RINCON_1".into(),
            name: "Media Room".into(),
            websocket_url: "wss://192.168.77.94:1443/websocket/api".into(),
            capabilities: vec![],
        };
        assert_eq!(player.ip(), "192.168.77.94".parse().ok());
    }

    #[test]
    fn playback_state_prefix_is_stripped() {
        let status = PlaybackStatus {
            playback_state: Some("PLAYBACK_STATE_PLAYING".into()),
            position_millis: Some(0),
            queue_version: None,
            item_id: None,
            available_playback_actions: None,
            play_modes: None,
        };
        assert_eq!(status.state(), Some("PLAYING"));
    }

    /// A `playbackStatus` body without the state field. This used to fail the
    /// whole event with "missing field `playbackState`" and drop it; it must now
    /// parse, and say "unchanged" rather than a state of its own.
    #[test]
    fn a_partial_playback_body_parses_as_unchanged() {
        let status: PlaybackStatus = serde_json::from_str(
            r#"{"availablePlaybackActions":{"canSkip":true},"playModes":{"shuffle":true}}"#,
        )
        .expect("a body without playbackState still parses");
        assert_eq!(status.state(), None);
        assert_eq!(status.position_millis, None);
        // The rest of the body is still read, which is the point of not
        // dropping the event.
        assert!(status.actions().can_skip);
        assert!(status.modes().shuffle);
    }

    /// The two `playback:1` bodies captured on 2026-09-03/04 when a stream
    /// failed. They must not be read as statuses: every field of a
    /// `PlaybackStatus` is optional and none of them appears here, so one
    /// parses perfectly and says "nothing changed" while carrying the only
    /// notice that the music stopped.
    #[test]
    fn a_playback_error_is_told_apart_from_a_status() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"_objectType":"playbackError","errorCode":"ERROR_PLAYBACK_FAILED",
                "itemId":"VXiDuCccgtRrXdIc81isYseHrI4=","reason":"ERROR_CANT_REACH_SERVER",
                "serviceId":-1,"serviceName":"https:","trackName":"Apple Music Chill"}"#,
        )
        .unwrap();
        let error = playback_error(&body).expect("an error body reads as an error");
        // Reason before code: the specific half is the useful one.
        assert_eq!(
            error.to_string(),
            "ERROR_CANT_REACH_SERVER on \"Apple Music Chill\""
        );
        // It would otherwise have parsed, which is exactly the trap.
        let as_status: PlaybackStatus = serde_json::from_value(body).unwrap();
        assert_eq!(as_status.state(), None);
        assert_eq!(as_status.available_playback_actions, None);

        // The second one arrived seconds later with no itemId at all.
        let terse: serde_json::Value = serde_json::from_str(
            r#"{"_objectType":"playbackError","errorCode":"ERROR_PLAYBACK_FAILED",
                "reason":"ERROR_CANT_REACH_SERVER","serviceId":-1,"serviceName":"https:",
                "trackName":"Apple Music Chill"}"#,
        )
        .unwrap();
        assert!(playback_error(&terse).is_some());

        // And a real status is not mistaken for an error.
        let status: serde_json::Value = serde_json::from_str(
            r#"{"_objectType":"playbackStatus","playbackState":"PLAYBACK_STATE_PLAYING"}"#,
        )
        .unwrap();
        assert!(playback_error(&status).is_none());
        // Nor is a body that names no type - the partial statuses seen in the
        // wild carry no `_objectType` either.
        assert!(playback_error(&serde_json::json!({"positionMillis": 1})).is_none());
    }

    #[test]
    fn an_error_with_no_track_still_says_why() {
        let error: PlaybackError =
            serde_json::from_str(r#"{"errorCode":"ERROR_PLAYBACK_FAILED"}"#).unwrap();
        // Falls back to the code when there is no reason, and says something
        // rather than nothing when there is neither.
        assert_eq!(error.to_string(), "ERROR_PLAYBACK_FAILED");
        let bare: PlaybackError = serde_json::from_str("{}").unwrap();
        assert_eq!(bare.to_string(), "no reason given");
    }

    #[test]
    fn repeat_one_wins_over_repeat() {
        let modes = |repeat, repeat_one| PlayModes {
            repeat,
            repeat_one,
            shuffle: false,
            crossfade: false,
        };
        assert_eq!(modes(false, false).repeat(), Repeat::Off);
        assert_eq!(modes(true, false).repeat(), Repeat::All);
        assert_eq!(modes(false, true).repeat(), Repeat::One);
        assert_eq!(modes(true, true).repeat(), Repeat::One);
        assert_eq!(Repeat::parse("all"), Some(Repeat::All));
        assert_eq!(Repeat::parse("queue"), None);

        let radio = PlaybackActions::default();
        assert!(radio.allows(Repeat::Off));
        assert!(!radio.allows(Repeat::All));
        assert!(!radio.allows(Repeat::One));
    }

    #[test]
    fn lfe_channel_survives_the_wire_name() {
        // Verbatim from a Beam on its TV input: the player spells LFE in caps,
        // which is not what camelCase would produce from `num_lfe_channels`.
        let wire = r#"{"numGroundChannels":5,"numLFEChannels":1,
            "numHeightChannels":0,"streamDescription":"Dolby Digital Surround"}"#;
        let format: HomeTheaterFormat = serde_json::from_str(wire).unwrap();
        assert_eq!(format.channels(), "5.1");
        assert_eq!(format.summary(), "Dolby Digital Surround 5.1");
        assert!(format.is_surround());
    }
}
