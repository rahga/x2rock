//! One room as the TUI sees it, and the display rules that turn it into lines.
//!
//! **This is deliberately not the CLI's view of a room.** `x2rock status --json`
//! assembles 27 fields at print time out of [`RoomFacts`], a `PlaybackStatus`, a
//! `MetadataStatus` and an `Option<Volume>` (see `main.rs`), which is the shape
//! JSON wants. What arrives here is whatever the daemon publishes over MPRIS -
//! the same set the bar widget renders, no more - so one type serving both would
//! be a type with two thirds of its fields empty half the time.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

// The daemon's own names for its keys, so the two ends of the contract cannot
// drift apart by a typo in one of them.
use crate::mpris::{
    CAN_REPEAT, CAN_REPEAT_ONE, CAN_SHUFFLE, HAS_TV_INPUT, INPUT_FORMAT, LIVE_STREAM,
    MEMBER_VOLUMES, MEMBERS, ON_TV_INPUT, STATION_NAME, STREAM_INFO,
};

/// What the room is doing, as `PlaybackStatus` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl State {
    fn parse(s: &str) -> Self {
        match s {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Stopped => "idle",
        }
    }
}

/// One MPRIS player - which is one *group*, not one speaker. The daemon
/// publishes a player per group and republishes them all when the topology
/// moves, so a grouped pair is one row here and gains a second member rather
/// than a second row.
#[derive(Debug, Clone, Default)]
pub struct RoomSnapshot {
    /// The bus name, kept because every write and every proxy needs it.
    pub bus_name: String,
    /// `Identity` - the room or group label the daemon chose.
    pub room: String,
    pub state: State,
    pub title: String,
    pub artist: String,
    pub volume: f64,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_pause: bool,
    pub can_play: bool,
    /// `LoopStatus`, verbatim: "None", "Track" or "Playlist".
    pub loop_status: String,
    pub shuffle: bool,
    /// The rooms in this group, and each one's own volume beneath the group
    /// mix. Same order, and the pairing is what the balance view is for.
    ///
    /// **In the order Sonos lists them, which is not coordinator-first.** The
    /// coordinator is whichever of these is [`RoomSnapshot::room`]: the daemon
    /// names a group's player after its coordinator, so the label and the
    /// list together say which one it is, and position says nothing.
    pub members: Vec<String>,
    pub member_volumes: Vec<u8>,
    pub on_tv: bool,
    pub has_tv: bool,
    pub input_format: String,
    pub is_live_stream: bool,
    pub station_name: String,
    /// A live stream's own "now playing" text, verbatim and **never split** -
    /// see [`RoomSnapshot::track_suffix`].
    pub stream_info: String,
    pub can_repeat: bool,
    pub can_repeat_one: bool,
    pub can_shuffle: bool,
}

/// `xesam:artist` is an array per the MPRIS spec, and the daemon writes a
/// one-element one. Everything else it sends is a plain string.
fn first_string(value: Option<&OwnedValue>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Ok(s) = <&str>::try_from(value) {
        return s.to_owned();
    }
    if let Ok(list) = <Vec<String>>::try_from(value.clone()) {
        return list.into_iter().next().unwrap_or_default();
    }
    String::new()
}

fn flag(value: Option<&OwnedValue>) -> bool {
    value.and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

fn strings(value: Option<&OwnedValue>) -> Vec<String> {
    value
        .and_then(|v| <Vec<String>>::try_from(v.clone()).ok())
        .unwrap_or_default()
}

impl RoomSnapshot {
    /// Fold an MPRIS `Metadata` map in. Absent keys leave their fields empty,
    /// which is the right reading here: the daemon omits a key it has nothing
    /// to say about - `nightMode` on a speaker that is not a soundbar, a
    /// station name that would only repeat the title.
    pub fn apply_metadata(&mut self, metadata: &HashMap<String, OwnedValue>) {
        let get = |key: &str| metadata.get(key);
        self.title = first_string(get("xesam:title"));
        self.artist = first_string(get("xesam:artist"));
        self.members = strings(get(MEMBERS));
        self.member_volumes = strings(get(MEMBER_VOLUMES))
            .iter()
            .map(|v| v.parse().unwrap_or(0))
            .collect();
        self.on_tv = flag(get(ON_TV_INPUT));
        self.has_tv = flag(get(HAS_TV_INPUT));
        self.input_format = first_string(get(INPUT_FORMAT));
        self.is_live_stream = flag(get(LIVE_STREAM));
        self.station_name = first_string(get(STATION_NAME));
        self.stream_info = first_string(get(STREAM_INFO));
        self.can_repeat = flag(get(CAN_REPEAT));
        self.can_repeat_one = flag(get(CAN_REPEAT_ONE));
        self.can_shuffle = flag(get(CAN_SHUFFLE));
    }

    pub fn set_playback_state(&mut self, status: &str) {
        self.state = State::parse(status);
    }

    /// Whether transport applies to this room at all.
    ///
    /// False on the TV input, where the television is the source and its own
    /// remote owns play, pause and skip - the player says as much, reporting
    /// `canPause`, `canSkip`, `canSkipBack`, `canSeek` and `canStop` all false,
    /// and the Sonos app offers such a room no transport either. `canPause`
    /// alone is not the test: a live stream reports that too and *can* be
    /// stopped, which is the conflation that once put an inert stop button on
    /// the bar widget.
    pub fn transport_available(&self) -> bool {
        !self.on_tv
    }

    /// The station's name, wherever the daemon put it.
    ///
    /// Usually its own field. For a `play-url` stream the field is empty and the
    /// *title* is the station - the stream host - because the daemon drops a
    /// name that would only repeat the line above. So the fallback is not a
    /// guess: an empty station on something known to be a live stream means the
    /// title is carrying it.
    pub fn station_label(&self) -> &str {
        if !self.station_name.is_empty() {
            return &self.station_name;
        }
        if self.is_live_stream { &self.title } else { "" }
    }

    /// What follows the title: the artist where there is one, else the stream's
    /// own headline where it says something the title does not.
    ///
    /// **Never split the headline.** `Artist - Title` is an Icecast convention
    /// rather than a format - a station is equally free to put a show name or a
    /// slogan there - and splitting on the hyphen invents an artist wherever it
    /// happens to land.
    ///
    /// Filtered against the title rather than shown whenever present. The two
    /// have not been seen together on any household here, but that is an
    /// observation about a couple of services and not a rule, and a station
    /// putting the same text in both would otherwise say it twice.
    pub fn track_suffix(&self) -> &str {
        if !self.artist.is_empty() {
            return &self.artist;
        }
        if self.stream_info.is_empty() {
            return "";
        }
        if self
            .title
            .to_lowercase()
            .contains(&self.stream_info.to_lowercase())
        {
            return "";
        }
        &self.stream_info
    }

    /// The room's now-playing line, with the station left to the line beneath.
    ///
    /// For a `play-url` stream this inverts what the fields are called: `title`
    /// is the stream host and the only word about the music is the headline, so
    /// the headline is promoted here and the host demoted to
    /// [`Self::station_label`]. A stream carrying a real track keeps the
    /// ordinary shape - its title stays on top and its station is a field of its
    /// own.
    pub fn now_line(&self) -> String {
        if self.station_name.is_empty() && self.is_live_stream && !self.stream_info.is_empty() {
            return self.stream_info.clone();
        }
        if self.title.is_empty() {
            return self.station_name.clone();
        }
        match self.track_suffix() {
            "" => self.title.clone(),
            suffix => format!("{} — {suffix}", self.title),
        }
    }

    /// Whether this row is a group rather than a lone room.
    pub fn is_group(&self) -> bool {
        self.members.len() > 1
    }

    /// Whether this member is the one the group answers to - by name, because
    /// that is the only place the daemon says so.
    pub fn is_coordinator(&self, member: &str) -> bool {
        member == self.room
    }

    /// The members that are not the coordinator: the rooms that have joined,
    /// and the ones that can leave.
    pub fn others(&self) -> impl Iterator<Item = &String> {
        self.members
            .iter()
            .filter(|member| !self.is_coordinator(member))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(title: &str, info: &str, station: &str) -> RoomSnapshot {
        RoomSnapshot {
            title: title.into(),
            stream_info: info.into(),
            station_name: station.into(),
            is_live_stream: true,
            ..RoomSnapshot::default()
        }
    }

    /// The `play-url` case that prompted the rule: the title is the stream host
    /// and the headline is the only thing that names the music.
    #[test]
    fn a_bare_stream_leads_with_its_headline_and_demotes_the_host() {
        let room = stream("ice1.somafm.com", "Gary Numan - I'm An Agent", "");
        assert_eq!(room.now_line(), "Gary Numan - I'm An Agent");
        assert_eq!(room.station_label(), "ice1.somafm.com");
    }

    /// A stream that names its own station keeps the ordinary shape, so the
    /// promotion above cannot reorder a service that was already right.
    #[test]
    fn a_stream_with_a_station_field_keeps_the_title_on_top() {
        let mut room = stream("Intervallo", "", "Sonos Radio");
        room.artist = "Piero Umiliani".into();
        assert_eq!(room.now_line(), "Intervallo — Piero Umiliani");
        assert_eq!(room.station_label(), "Sonos Radio");
    }

    /// The artist wins where there is one; the headline is only a fallback.
    #[test]
    fn the_artist_outranks_a_headline() {
        let mut room = stream("Bodies", "Offset, JID - Bodies", "");
        room.artist = "Offset, JID".into();
        assert_eq!(room.track_suffix(), "Offset, JID");
    }

    /// A headline the title already contains says nothing new, and a line that
    /// repeats itself is worse than a shorter one.
    #[test]
    fn a_headline_the_title_already_carries_is_dropped() {
        let room = stream(
            "Gary Numan - I'm An Agent",
            "gary numan - i'm an agent",
            "x",
        );
        assert_eq!(room.track_suffix(), "");
    }

    /// The headline is passed through whole. Splitting `Artist - Title` would
    /// invent an artist on a station that puts a show name there instead.
    #[test]
    fn the_headline_is_never_split_on_its_hyphen() {
        let room = stream("WFMU", "Wake and Bake with Clay Pigeon", "");
        assert_eq!(room.now_line(), "Wake and Bake with Clay Pigeon");
        assert!(!room.now_line().contains('—'));
    }

    /// A room on its TV input has no transport to offer, and says so through
    /// the one flag rather than through `can_pause`, which a stream shares.
    #[test]
    fn tv_input_withdraws_transport_but_a_stream_does_not() {
        let mut tv = RoomSnapshot {
            on_tv: true,
            has_tv: true,
            ..RoomSnapshot::default()
        };
        assert!(!tv.transport_available());
        tv.on_tv = false;
        assert!(tv.transport_available());
        assert!(stream("x", "", "").transport_available());
    }

    /// Sonos lists a group's players in its own order and the coordinator can
    /// be anywhere in it. Reading position instead of the name would call the
    /// wrong room the coordinator whenever it is not first.
    #[test]
    fn the_coordinator_is_found_by_name_not_position() {
        let room = RoomSnapshot {
            room: "Kitchen".into(),
            members: vec!["Office".into(), "Kitchen".into(), "Den".into()],
            ..RoomSnapshot::default()
        };
        assert!(room.is_coordinator("Kitchen"));
        assert!(!room.is_coordinator("Office"));
        assert_eq!(room.others().collect::<Vec<_>>(), vec!["Office", "Den"]);
    }

    /// Absent keys mean "nothing to say", not "false" - the daemon omits what
    /// does not apply, and a blank line reads better than a wrong one.
    #[test]
    fn an_empty_metadata_map_leaves_a_blank_room_rather_than_a_wrong_one() {
        let mut room = RoomSnapshot::default();
        room.apply_metadata(&HashMap::new());
        assert_eq!(room.now_line(), "");
        assert_eq!(room.station_label(), "");
        assert!(room.members.is_empty());
    }
}
