//! Wire types for the Sonos Control API.
//!
//! The same JSON protocol is spoken by the cloud API and by players on the LAN.
//! Every exchange is a two-element array: `[header, body]`.
//!
//! These mirror the wire format rather than current use, so some fields are
//! deserialized before anything reads them.
#![allow(dead_code)]

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

    fn room_names(&self) -> String {
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
    scored.into_iter().take(3).map(|(_, n)| n.to_string()).collect()
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
    pub playback_state: String,
    #[serde(default)]
    pub position_millis: u64,
    /// Bumps whenever the queue changes - the cue to re-read it over UPnP.
    pub queue_version: Option<String>,
    #[serde(default)]
    pub available_playback_actions: PlaybackActions,
    #[serde(default)]
    pub play_modes: PlayModes,
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
    /// `PLAYBACK_STATE_PLAYING` -> `PLAYING`.
    pub fn state(&self) -> &str {
        self.playback_state
            .strip_prefix("PLAYBACK_STATE_")
            .unwrap_or(&self.playback_state)
    }
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
        let rooms = ["Bedroom", "Living Room", "Dining Room", "Guest TV", "Kitchen"];
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
            playback_state: "PLAYBACK_STATE_PLAYING".into(),
            position_millis: 0,
            queue_version: None,
            available_playback_actions: PlaybackActions::default(),
            play_modes: PlayModes::default(),
        };
        assert_eq!(status.state(), "PLAYING");
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
