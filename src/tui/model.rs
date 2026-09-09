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
    CAN_CROSSFADE, CAN_REPEAT, CAN_REPEAT_ONE, CAN_SHUFFLE, CROSSFADE, FIXED_VOLUME, HAS_TV_INPUT,
    INPUT_FORMAT, LIVE_STREAM, MEMBER_FIXED_VOLUME, MEMBER_MUTED, MEMBER_VOLUME_LEVELS,
    MEMBER_VOLUMES, MEMBERS, MUTED, ON_TV_INPUT, STATION_NAME, STREAM_INFO, VOLUME_LEVEL,
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
    /// The slider position, 0.0-1.0: where the bar is drawn and what a step
    /// starts from. Not the heard volume - a muted room keeps its level here
    /// and says so through [`RoomSnapshot::muted`], the way the Sonos app keeps
    /// the slider where it was and dims it. Settled by
    /// [`RoomSnapshot::settle_volume`] once both sources have been read.
    pub volume: f64,
    /// The level the daemon publishes regardless of mute, where it publishes
    /// one. An older daemon does not, and then the heard volume is all there
    /// is - which reads as zero while muted, the best that can be done.
    pub level: Option<f64>,
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
    /// Whether the group, and each member, is muted. The volumes above read
    /// zero while muted, so these are the only way to tell muted from quiet.
    pub muted: bool,
    pub member_muted: Vec<bool>,
    /// Whether the group, and each member, has a fixed volume: line-level
    /// output with no volume control of its own. The bar and its keys mean
    /// nothing on such a room, and are withdrawn - see
    /// [`RoomSnapshot::volume_available`].
    pub fixed_volume: bool,
    pub member_fixed: Vec<bool>,
    /// Crossfade, the play mode MPRIS has no property for.
    pub crossfade: bool,
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
    pub can_crossfade: bool,
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
        // The slider positions, where the daemon sends them: the same list with
        // mute not zeroing anything. Trusted only when it lines up with the
        // members, since a half-published list would pair levels with the
        // wrong rooms.
        let levels: Vec<u8> = strings(get(MEMBER_VOLUME_LEVELS))
            .iter()
            .map(|v| v.parse().unwrap_or(0))
            .collect();
        if !levels.is_empty() && levels.len() == self.member_volumes.len() {
            self.member_volumes = levels;
        }
        self.level = first_string(get(VOLUME_LEVEL))
            .parse::<u8>()
            .ok()
            .map(|level| f64::from(level) / 100.0);
        self.muted = flag(get(MUTED));
        self.member_muted = strings(get(MEMBER_MUTED))
            .iter()
            .map(|v| v == "true")
            .collect();
        self.fixed_volume = flag(get(FIXED_VOLUME));
        self.member_fixed = strings(get(MEMBER_FIXED_VOLUME))
            .iter()
            .map(|v| v == "true")
            .collect();
        self.crossfade = flag(get(CROSSFADE));
        self.on_tv = flag(get(ON_TV_INPUT));
        self.has_tv = flag(get(HAS_TV_INPUT));
        self.input_format = first_string(get(INPUT_FORMAT));
        self.is_live_stream = flag(get(LIVE_STREAM));
        self.station_name = first_string(get(STATION_NAME));
        self.stream_info = first_string(get(STREAM_INFO));
        self.can_repeat = flag(get(CAN_REPEAT));
        self.can_repeat_one = flag(get(CAN_REPEAT_ONE));
        self.can_shuffle = flag(get(CAN_SHUFFLE));
        self.can_crossfade = flag(get(CAN_CROSSFADE));
    }

    /// Decide where the bar sits, given what MPRIS Volume said. The published
    /// level wins where there is one; the heard volume is the fallback for a
    /// daemon that predates it.
    pub fn settle_volume(&mut self, heard: f64) {
        self.volume = self.level.unwrap_or(heard);
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
    ///
    /// But only while the headline is on the line above. With no headline the
    /// title is itself the top line ([`Self::now_line`]), and naming it again
    /// here said the host twice - which is what an idle `play-url` room did on
    /// a real household.
    pub fn station_label(&self) -> &str {
        if !self.station_name.is_empty() {
            return &self.station_name;
        }
        if self.is_live_stream && !self.stream_info.is_empty() {
            &self.title
        } else {
            ""
        }
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

    /// Whether the group's volume can be changed from here at all. A fixed
    /// volume is set on the amplifier the room feeds, and the CLI refuses a
    /// step or a mute on it in those words; offering the keys anyway would be
    /// offering an error.
    pub fn volume_available(&self) -> bool {
        !self.fixed_volume
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

    /// An idle `play-url` room has a host and no headline: the host is the top
    /// line, and the line below has nothing to add rather than the host again.
    #[test]
    fn a_stream_with_no_headline_names_its_host_once() {
        let room = stream("ice1.somafm.com", "", "");
        assert_eq!(room.now_line(), "ice1.somafm.com");
        assert_eq!(room.station_label(), "");
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

    /// The member flags arrive as the strings the daemon has to send them as,
    /// and anything that is not the word `true` is not muted.
    #[test]
    fn mute_flags_are_read_from_the_daemon_s_keys() {
        let mut room = RoomSnapshot::default();
        let mut metadata = HashMap::new();
        metadata.insert(
            MUTED.to_owned(),
            OwnedValue::try_from(zbus::zvariant::Value::from(true)).unwrap(),
        );
        metadata.insert(
            MEMBER_MUTED.to_owned(),
            OwnedValue::try_from(zbus::zvariant::Value::from(vec!["true", "false"])).unwrap(),
        );
        room.apply_metadata(&metadata);
        assert!(room.muted);
        assert_eq!(room.member_muted, vec![true, false]);
    }

    /// Fixed volume withdraws the volume controls; nothing else does, and a
    /// muted room in particular still has them, since unmute is one of them.
    #[test]
    fn only_a_fixed_volume_withdraws_the_volume_keys() {
        let mut room = RoomSnapshot::default();
        assert!(room.volume_available());
        room.muted = true;
        assert!(room.volume_available());
        room.fixed_volume = true;
        assert!(!room.volume_available());
    }

    /// The bar shows the level, not what is heard: a muted room at 40 is drawn
    /// at 40 with the word beside it. Without a published level - an older
    /// daemon - the heard volume is all there is.
    #[test]
    fn the_bar_sits_at_the_published_level_and_falls_back_to_what_is_heard() {
        let mut room = RoomSnapshot {
            level: Some(0.4),
            ..RoomSnapshot::default()
        };
        room.settle_volume(0.0);
        assert_eq!(room.volume, 0.4);

        let mut older = RoomSnapshot::default();
        older.settle_volume(0.25);
        assert_eq!(older.volume, 0.25);
    }

    /// Member levels replace the heard member volumes only when the two lists
    /// line up; a list of the wrong length would pair levels with the wrong
    /// rooms and is left alone.
    #[test]
    fn member_levels_replace_heard_volumes_only_when_they_line_up() {
        let value = |v: Vec<&str>| OwnedValue::try_from(zbus::zvariant::Value::from(v)).unwrap();
        let mut room = RoomSnapshot::default();
        let mut metadata = HashMap::new();
        metadata.insert(MEMBERS.to_owned(), value(vec!["Kitchen", "Office"]));
        metadata.insert(MEMBER_VOLUMES.to_owned(), value(vec!["0", "30"]));
        metadata.insert(MEMBER_VOLUME_LEVELS.to_owned(), value(vec!["40", "30"]));
        room.apply_metadata(&metadata);
        assert_eq!(room.member_volumes, vec![40, 30]);

        metadata.insert(MEMBER_VOLUME_LEVELS.to_owned(), value(vec!["40"]));
        room.apply_metadata(&metadata);
        assert_eq!(room.member_volumes, vec![0, 30]);
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
