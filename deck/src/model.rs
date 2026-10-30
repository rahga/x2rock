//! The household as the screen sees it, and the wording rules the TV app uses
//! for it. Pure: nothing here touches the network or the UI, so every rule is
//! tested against the TV app's own expected strings.

use std::collections::HashMap;
use std::time::Instant;

use x2rock::sonos::proto::{
    Groups, MetadataStatus, PlayModes, PlaybackActions, PlaybackStatus, Volume,
};

/// One group's live state, folded from its events. A field an event leaves
/// out is unchanged, never reset: partial bodies are routine (see the
/// sibling's `PlaybackStatus` notes).
#[derive(Clone, Debug, Default)]
pub struct GroupLive {
    pub state: Option<String>,
    pub position_ms: u64,
    pub position_at: Option<Instant>,
    pub actions: PlaybackActions,
    pub modes: PlayModes,
    pub metadata: Option<MetadataStatus>,
    pub volume: Option<Volume>,
    /// When the room's sleep timer stops it, as `sleepTimer:1` pushes it.
    pub sleep_ends_at: Option<Instant>,
    /// Moves whenever the queue changes, as `playback:1` pushes it.
    pub queue_version: Option<String>,
}

/// `PT0H15M0S` as a duration: the sleep timer's `remainingTimeDuration`.
pub fn iso_duration(text: &str) -> Option<std::time::Duration> {
    let rest = text.strip_prefix("PT")?;
    let (mut total, mut number) = (0f64, String::new());
    for c in rest.chars() {
        match c {
            '0'..='9' | '.' => number.push(c),
            'H' | 'M' | 'S' => {
                let n: f64 = number.parse().ok()?;
                total += n * match c { 'H' => 3600.0, 'M' => 60.0, _ => 1.0 };
                number.clear();
            }
            _ => return None,
        }
    }
    number.is_empty().then(|| std::time::Duration::from_secs_f64(total))
}

impl GroupLive {
    /// A `sleepTimerStatus`: armed with what is left, or not.
    pub fn apply_sleep(&mut self, body: &serde_json::Value, now: Instant) {
        let active = body.get("active").and_then(|a| a.as_bool()).unwrap_or(false);
        let left = body.get("remainingTimeDuration").and_then(|d| d.as_str()).and_then(iso_duration);
        self.sleep_ends_at = match (active, left) {
            (true, Some(left)) => Some(now + left),
            _ => None,
        };
    }

    /// What is left on the sleep timer, if it is running.
    pub fn sleep_left(&self, now: Instant) -> Option<std::time::Duration> {
        self.sleep_ends_at.and_then(|end| end.checked_duration_since(now)).filter(|d| !d.is_zero())
    }

    pub fn apply_playback(&mut self, status: &PlaybackStatus, now: Instant) {
        if let Some(state) = &status.playback_state {
            self.state = Some(state.clone());
        }
        if let Some(position) = status.position_millis {
            self.position_ms = position;
            self.position_at = Some(now);
        }
        if let Some(actions) = status.available_playback_actions {
            self.actions = actions;
        }
        if let Some(modes) = status.play_modes {
            self.modes = modes;
        }
        if let Some(version) = &status.queue_version {
            self.queue_version = Some(version.clone());
        }
    }

    pub fn playing(&self) -> bool {
        matches!(self.state.as_deref(), Some("PLAYBACK_STATE_PLAYING" | "PLAYBACK_STATE_BUFFERING" | "PLAYING" | "BUFFERING"))
    }

    pub fn duration_ms(&self) -> u64 {
        self.metadata
            .as_ref()
            .and_then(|m| m.track())
            .and_then(|t| t.duration_millis)
            .unwrap_or(0)
    }

    /// The position now: the last report plus the time since it while playing,
    /// held at the track's end. A bar drawn from the report alone sat behind by
    /// however long the player had been quiet.
    pub fn live_position(&self, now: Instant) -> u64 {
        let mut position = self.position_ms;
        if self.playing()
            && let Some(at) = self.position_at {
                position += now.saturating_duration_since(at).as_millis() as u64;
            }
        match self.duration_ms() {
            0 => position,
            duration => position.min(duration),
        }
    }
}

/// The whole household as last heard.
#[derive(Clone, Debug, Default)]
pub struct Household {
    pub groups: Option<Groups>,
    pub live: HashMap<String, GroupLive>,
    pub player_volumes: HashMap<String, Volume>,
    /// Night sound and speech enhancement, by the soundbar's player id.
    pub home_theater: HashMap<String, (bool, bool)>,
    /// Bonded speakers (a Sub, surrounds, a pair's other half) as `zones:1`
    /// pushes them, by player id: which room each belongs to, and whether it
    /// has dropped off.
    pub bonds: std::collections::BTreeMap<String, x2rock::sonos::proto::Bond>,
}

impl Household {
    /// How many of a room's bonded speakers have dropped off: a Sub, a
    /// surround. The topology may no longer list them; `zones:1` still does.
    pub fn offline_in(&self, room: &str) -> usize {
        self.bonds.values().filter(|b| b.zone == room && b.disconnected == Some(true)).count()
    }

    /// Whether `player`'s room has a speaker on any of these channels: `SW`
    /// for a Sub, `LR`/`RR` for surrounds, as `zones:1` names them.
    fn room_has(&self, player: &str, channels: &[&str]) -> bool {
        let Some(room) = self.bonds.get(player).map(|b| &b.zone) else { return false };
        self.bonds.values().any(|b| &b.zone == room && b.channels.iter().any(|c| channels.contains(&c.as_str())))
    }

    pub fn has_sub(&self, player: &str) -> bool {
        self.room_has(player, &["SW"])
    }

    pub fn has_surrounds(&self, player: &str) -> bool {
        self.room_has(player, &["LR", "RR"])
    }
}

/// What a room is doing, in one of four words: the one rule the list, the pane
/// and the panel all use (RoomActivity.kt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Playing,
    Paused,
    Stopped,
    Empty,
}

impl Activity {
    pub fn label(self) -> &'static str {
        match self {
            Activity::Playing => "Playing",
            Activity::Paused => "Paused",
            Activity::Stopped => "Stopped",
            Activity::Empty => "Nothing playing",
        }
    }
}

/// The player says `PLAYBACK_STATE_PLAYING`; older captures say `PLAYING`.
pub fn activity(state: Option<&str>, has_source: bool) -> Activity {
    let state = state.map(|s| s.trim_start_matches("PLAYBACK_STATE_"));
    match state {
        Some("PLAYING" | "BUFFERING") => Activity::Playing,
        Some("PAUSED") => Activity::Paused,
        _ if has_source => Activity::Stopped,
        _ => Activity::Empty,
    }
}

/// What the room list needs to word a row (HomeViewModel.RoomInfo).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoomInfo {
    pub track_name: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub art_url: Option<String>,
    pub on_tv_input: bool,
    pub input_format: String,
    /// The container: an album, a station, or "TV Audio".
    pub source: Option<String>,
    pub stream_info: Option<String>,
    pub is_radio: bool,
    pub alarm_ringing: bool,
    pub muted: bool,
    pub service: Option<String>,
}

impl RoomInfo {
    pub fn from_live(live: &GroupLive) -> Self {
        let meta = live.metadata.as_ref();
        let container = meta.and_then(|m| m.container.as_ref());
        let track = meta.and_then(|m| m.track());
        let format = meta.and_then(|m| m.input_format());
        let source = container.and_then(|c| c.name.clone()).filter(|s| !s.trim().is_empty());
        RoomInfo {
            track_name: track.and_then(|t| t.name.clone()).filter(|s| !s.trim().is_empty()),
            artist: track.and_then(|t| t.artist_name()).map(str::to_owned),
            album: track
                .and_then(|t| t.album.as_ref())
                .and_then(|a| a.name.clone())
                .filter(|s| !s.trim().is_empty()),
            art_url: meta.and_then(|m| m.art_url()).map(str::to_owned),
            on_tv_input: format.is_some(),
            input_format: format.map(|f| f.summary()).unwrap_or_default(),
            alarm_ringing: source.as_deref().is_some_and(|s| s.starts_with("x-rincon-buzzer")),
            source,
            stream_info: meta.and_then(|m| m.stream_info()).map(str::to_owned),
            is_radio: container.and_then(|c| c.kind.as_deref()) == Some("station"),
            muted: live.volume.as_ref().is_some_and(|v| v.muted),
            service: container
                .and_then(|c| c.service.as_ref())
                .and_then(|s| s.name.clone()),
        }
    }

    /// Whether anything is loaded, by what the player has told us.
    pub fn has_source(&self) -> bool {
        self.on_tv_input || self.is_radio || self.track_name.is_some() || self.source.is_some() || self.stream_info.is_some()
    }
}

/// The lines under a room's name in the room list (HomeViewModel.roomRowLines).
pub fn room_row_lines(info: &RoomInfo, state: Option<&str>) -> Vec<String> {
    if info.alarm_ringing {
        return ["Alarm".to_owned()].into_iter().chain(info.track_name.clone()).collect();
    }
    if info.on_tv_input {
        let first = if info.input_format.is_empty() {
            activity(state, true).label().to_owned()
        } else {
            info.input_format.clone()
        };
        return [first].into_iter().chain(info.source.clone()).collect();
    }
    if let Some(name) = &info.track_name {
        let second = info
            .artist
            .clone()
            .or_else(|| info.stream_info.clone().filter(|s| s != name));
        return [name.clone()].into_iter().chain(second).collect();
    }
    if let Some(stream) = &info.stream_info {
        return [stream.clone()].into_iter().chain(info.source.clone()).collect();
    }
    if info.is_radio
        && let Some(source) = &info.source {
            return vec![source.clone()];
        }
    vec![activity(state, info.has_source()).label().to_owned()]
}

/// The line under a track's title in the pane: artist • album, and a station
/// after them where it says something new (PlayerViewModel.trackLine).
pub fn track_line(artist: Option<&str>, album: Option<&str>, source: Option<&str>, is_radio: bool, track: Option<&str>) -> String {
    let station = source.filter(|s| is_radio && !s.trim().is_empty() && Some(*s) != album && Some(*s) != track);
    [artist, album, station].into_iter().flatten().collect::<Vec<_>>().join(" \u{2022} ")
}

/// "Up next · I'll Be Missing You — Diddy", or nothing where nothing is named.
pub fn track_caption(label: &str, title: Option<&str>, artist: Option<&str>) -> Option<String> {
    let name = title.filter(|t| !t.trim().is_empty())?;
    Some(match artist.filter(|a| !a.trim().is_empty()) {
        Some(by) => format!("{label} \u{00B7} {name} \u{2014} {by}"),
        None => format!("{label} \u{00B7} {name}"),
    })
}

/// A stream's address as typed: http and https only, since the speaker fetches
/// it; with no scheme it is taken as http (RadioViewModel.streamUrl).
pub fn stream_url(input: &str) -> Option<String> {
    let typed = input.trim();
    if typed.is_empty() {
        return None;
    }
    let url = if typed.contains("://") { typed.to_owned() } else { format!("http://{typed}") };
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let ok = matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") && !host.is_empty() && !host.starts_with(':');
    ok.then_some(url)
}

/// A stream is named by its host, port and all: the path is a bitrate slug.
pub fn stream_name(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() { url.to_owned() } else { host.to_owned() }
}

/// A group's name as a listener reads it: "Kitchen + 2".
pub fn group_label(name: &str, players: usize) -> String {
    if players > 1 && !name.contains(" + ") {
        format!("{name} + {}", players - 1)
    } else {
        name.to_owned()
    }
}

/// 0:07, 3:45, 59:59, 1:00:00; a negative position clamps to 0:00.
pub fn time_text(millis: i64) -> String {
    let total = millis.max(0) / 1000;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// How far one volume press moves the level: 2, 3, then 5 within a burst; a
/// pause of 1.5s or a turn the other way starts again at 2. At most 2 below 10
/// and 1 below 4, judged by where the level is going (VolumeBurst.kt).
#[derive(Debug, Default)]
pub struct VolumeBurst {
    presses: usize,
    last_at: Option<u64>,
    last_direction: i32,
}

impl VolumeBurst {
    const STEPS: [i32; 3] = [2, 3, 5];
    const GAP_MS: u64 = 1_500;

    /// The signed step for one press in `direction` (+1 or -1) from `level`,
    /// at `now_ms` on any monotonic clock.
    pub fn step(&mut self, level: i32, direction: i32, now_ms: u64) -> i32 {
        let fresh = match self.last_at {
            None => true,
            Some(at) => direction != self.last_direction || now_ms.saturating_sub(at) > Self::GAP_MS,
        };
        if fresh {
            self.presses = 0;
        }
        self.last_at = Some(now_ms);
        self.last_direction = direction;
        let climb = Self::STEPS[self.presses.min(Self::STEPS.len() - 1)];
        self.presses += 1;
        let toward = if direction < 0 { level - 1 } else { level };
        let cap = if toward < 4 {
            1
        } else if toward < 10 {
            2
        } else {
            i32::MAX
        };
        direction * climb.min(cap)
    }
}

/// The seek bar's presses climb 10s, 20s, then 30s within a burst.
#[derive(Debug, Default)]
pub struct SeekBurst {
    presses: usize,
    last_at: Option<u64>,
    /// The running total shown under the bar, and where the burst began.
    pub total_ms: i64,
    pub from_ms: i64,
}

impl SeekBurst {
    const STEPS_MS: [i64; 3] = [10_000, 20_000, 30_000];
    /// How long the total stays up after the last press.
    pub const SHOWN_MS: u64 = 1_500;

    /// The signed step for one press, and the total so far clamped to the
    /// track: `position` is where the bar is now, `duration` the track's length.
    pub fn step(&mut self, direction: i64, position: i64, duration: i64, now_ms: u64) -> i64 {
        let fresh = self.last_at.is_none_or(|at| now_ms.saturating_sub(at) > Self::SHOWN_MS);
        if fresh {
            self.presses = 0;
            self.total_ms = 0;
            self.from_ms = position;
        }
        self.last_at = Some(now_ms);
        let delta = direction * Self::STEPS_MS[self.presses.min(Self::STEPS_MS.len() - 1)];
        self.presses += 1;
        self.total_ms = (self.total_ms + delta).clamp(-self.from_ms, (duration - self.from_ms).max(0));
        delta
    }

    /// Whether the total is still showing at `now_ms`.
    pub fn shown(&self, now_ms: u64) -> bool {
        self.last_at.is_some_and(|at| now_ms.saturating_sub(at) <= Self::SHOWN_MS)
    }

    /// "+0:30" or "−1:00".
    pub fn total_text(&self) -> String {
        let sign = if self.total_ms < 0 { "\u{2212}" } else { "+" };
        format!("{sign}{}", time_text(self.total_ms.abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(info: Option<&str>) -> RoomInfo {
        RoomInfo {
            is_radio: true,
            source: Some("ice1.somafm.com".into()),
            stream_info: info.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn a_streams_own_text_goes_on_top_its_host_beneath() {
        assert_eq!(
            room_row_lines(&stream(Some("Blancmange - Don't Tell Me")), Some("PLAYBACK_STATE_PLAYING")),
            ["Blancmange - Don't Tell Me", "ice1.somafm.com"]
        );
    }

    #[test]
    fn a_stream_with_no_text_is_named_by_its_host() {
        assert_eq!(room_row_lines(&stream(None), Some("PLAYBACK_STATE_PLAYING")), ["ice1.somafm.com"]);
    }

    #[test]
    fn a_tv_input_gives_its_format_then_its_source() {
        let tv = RoomInfo {
            on_tv_input: true,
            input_format: "Dolby Digital 5.1".into(),
            source: Some("TV Audio".into()),
            ..Default::default()
        };
        assert_eq!(room_row_lines(&tv, Some("PLAYBACK_STATE_PLAYING")), ["Dolby Digital 5.1", "TV Audio"]);
    }

    #[test]
    fn a_ringing_alarm_says_so() {
        let chime = RoomInfo {
            source: Some("x-rincon-buzzer:0".into()),
            alarm_ringing: true,
            ..Default::default()
        };
        assert_eq!(room_row_lines(&chime, Some("PLAYBACK_STATE_PLAYING")), ["Alarm"]);
    }

    #[test]
    fn a_room_with_nothing_loaded_says_its_state() {
        assert_eq!(room_row_lines(&RoomInfo::default(), Some("PLAYBACK_STATE_IDLE")), ["Nothing playing"]);
    }

    #[test]
    fn a_tv_input_with_no_format_says_the_rooms_word() {
        let tv = RoomInfo {
            on_tv_input: true,
            source: Some("TV Audio".into()),
            ..Default::default()
        };
        assert_eq!(room_row_lines(&tv, Some("PLAYBACK_STATE_IDLE")), ["Stopped", "TV Audio"]);
        assert_eq!(room_row_lines(&tv, Some("PLAYBACK_STATE_PLAYING")), ["Playing", "TV Audio"]);
    }

    #[test]
    fn a_track_gives_title_then_artist_else_what_the_station_says() {
        let track = RoomInfo {
            track_name: Some("Hot House".into()),
            artist: Some("Ryo Fukui".into()),
            ..Default::default()
        };
        assert_eq!(room_row_lines(&track, None), ["Hot House", "Ryo Fukui"]);
        let no_artist = RoomInfo {
            track_name: Some("Hot House".into()),
            stream_info: Some("KCSM Jazz".into()),
            ..Default::default()
        };
        assert_eq!(room_row_lines(&no_artist, None), ["Hot House", "KCSM Jazz"]);
    }

    #[test]
    fn a_stations_track_names_the_station_after_its_artist() {
        assert_eq!(
            track_line(Some("The Weeknd"), None, Some("Hit List"), true, Some("Save Your Tears (Remix)")),
            "The Weeknd \u{2022} Hit List"
        );
    }

    #[test]
    fn a_track_off_the_queue_is_its_artist_and_album_alone() {
        assert_eq!(
            track_line(Some("Travis Scott"), Some("UTOPIA"), Some("Favorite tracks"), false, Some("FE!N")),
            "Travis Scott \u{2022} UTOPIA"
        );
    }

    #[test]
    fn a_station_named_as_its_album_is_not_said_twice() {
        assert_eq!(track_line(Some("A"), Some("Hit List"), Some("Hit List"), true, Some("x")), "A \u{2022} Hit List");
    }

    #[test]
    fn up_next_names_the_title_and_its_artist() {
        assert_eq!(
            track_caption("Up next", Some("I\u{2019}ll Be Missing You"), Some("Diddy")).unwrap(),
            "Up next \u{00B7} I\u{2019}ll Be Missing You \u{2014} Diddy"
        );
        assert_eq!(track_caption("Up next", Some("Intro"), Some(" ")).unwrap(), "Up next \u{00B7} Intro");
        assert_eq!(track_caption("Up next", None, Some("Diddy")), None);
        assert_eq!(track_caption("Up next", Some(""), None), None);
    }

    #[test]
    fn time_pads_seconds_rolls_into_hours_and_clamps() {
        assert_eq!(time_text(0), "0:00");
        assert_eq!(time_text(7_000), "0:07");
        assert_eq!(time_text(225_000), "3:45");
        assert_eq!(time_text(3_599_000), "59:59");
        assert_eq!(time_text(3_600_000), "1:00:00");
        assert_eq!(time_text(4_800_000), "1:20:00");
        assert_eq!(time_text(-5_000), "0:00");
    }

    #[test]
    fn a_typed_stream_address_is_http_or_https_and_named_by_its_host() {
        assert_eq!(stream_url("ice1.somafm.com/groovesalad-128-mp3").as_deref(), Some("http://ice1.somafm.com/groovesalad-128-mp3"));
        assert_eq!(stream_url("https://a.b:8000/x").as_deref(), Some("https://a.b:8000/x"));
        assert_eq!(stream_url("ftp://a.b/x"), None);
        assert_eq!(stream_url("  "), None);
        assert_eq!(stream_url("http:///x"), None);
        assert_eq!(stream_name("http://ice1.somafm.com/groovesalad-128-mp3"), "ice1.somafm.com");
        assert_eq!(stream_name("https://a.b:8000/x"), "a.b:8000");
    }

    #[test]
    fn group_label_counts_the_other_rooms() {
        assert_eq!(group_label("Kitchen", 1), "Kitchen");
        assert_eq!(group_label("Kitchen", 3), "Kitchen + 2");
        assert_eq!(group_label("Kitchen + 2", 3), "Kitchen + 2");
    }

    fn presses(burst: &mut VolumeBurst, now: &mut u64, from: i32, direction: i32, count: usize, gap: u64) -> Vec<i32> {
        let mut level = from;
        (0..count)
            .map(|_| {
                *now += gap;
                level += burst.step(level, direction, *now);
                level
            })
            .collect()
    }

    #[test]
    fn a_volume_burst_climbs_2_3_then_5() {
        let (mut b, mut now) = (VolumeBurst::default(), 0);
        assert_eq!(presses(&mut b, &mut now, 20, 1, 5, 200), [22, 25, 30, 35, 40]);
    }

    #[test]
    fn a_pause_or_a_turn_starts_the_next_burst_at_2() {
        let (mut b, mut now) = (VolumeBurst::default(), 0);
        presses(&mut b, &mut now, 20, 1, 3, 200);
        assert_eq!(presses(&mut b, &mut now, 30, 1, 1, 2_000), [32]);
        let (mut b, mut now) = (VolumeBurst::default(), 0);
        presses(&mut b, &mut now, 20, 1, 3, 200);
        assert_eq!(presses(&mut b, &mut now, 30, -1, 1, 200), [28]);
    }

    #[test]
    fn quiet_levels_step_finely_both_ways_and_loud_ones_are_not_capped() {
        let (mut b, mut now) = (VolumeBurst::default(), 0);
        assert_eq!(presses(&mut b, &mut now, 0, 1, 8, 200), [1, 2, 3, 4, 6, 8, 10, 15]);
        let (mut b, mut now) = (VolumeBurst::default(), 0);
        assert_eq!(presses(&mut b, &mut now, 30, -1, 10, 200), [28, 25, 20, 15, 10, 8, 6, 4, 3, 2]);
        let (mut b, mut now) = (VolumeBurst::default(), 0);
        assert_eq!(presses(&mut b, &mut now, 90, 1, 3, 200), [92, 95, 100]);
    }

    #[test]
    fn a_seek_burst_climbs_10_20_30_and_clamps_its_total_to_the_track() {
        let mut s = SeekBurst::default();
        assert_eq!(s.step(1, 60_000, 300_000, 0), 10_000);
        assert_eq!(s.step(1, 60_000, 300_000, 100), 20_000);
        assert_eq!(s.step(1, 60_000, 300_000, 200), 30_000);
        assert_eq!(s.step(1, 60_000, 300_000, 300), 30_000);
        assert_eq!(s.total_ms, 90_000);
        assert_eq!(s.total_text(), "+1:30");
        let mut back = SeekBurst::default();
        back.step(-1, 5_000, 300_000, 0);
        assert_eq!(back.total_ms, -5_000);
        assert_eq!(back.total_text(), "\u{2212}0:05");
        // A pause past the total's showing starts again at 10s.
        back.step(-1, 120_000, 300_000, 5_000);
        assert_eq!(back.total_ms, -10_000);
    }

    #[test]
    fn the_sleep_timer_reads_its_iso_duration_and_clears_on_inactive() {
        assert_eq!(iso_duration("PT0H15M0S"), Some(std::time::Duration::from_secs(900)));
        assert_eq!(iso_duration("PT1H30M"), Some(std::time::Duration::from_secs(5400)));
        assert_eq!(iso_duration("15 minutes"), None);
        let t0 = Instant::now();
        let mut live = GroupLive::default();
        live.apply_sleep(&serde_json::json!({ "active": true, "remainingTimeDuration": "PT0H15M0S" }), t0);
        assert_eq!(live.sleep_left(t0), Some(std::time::Duration::from_secs(900)));
        live.apply_sleep(&serde_json::json!({ "active": false }), t0);
        assert_eq!(live.sleep_left(t0), None);
    }

    #[test]
    fn live_position_runs_while_playing_and_stops_at_the_end() {
        let t0 = Instant::now();
        let mut live = GroupLive {
            state: Some("PLAYBACK_STATE_PLAYING".into()),
            position_ms: 1_000,
            position_at: Some(t0),
            ..Default::default()
        };
        assert_eq!(live.live_position(t0 + std::time::Duration::from_secs(2)), 3_000);
        live.state = Some("PLAYBACK_STATE_PAUSED".into());
        assert_eq!(live.live_position(t0 + std::time::Duration::from_secs(2)), 1_000);
    }
}

#[cfg(test)]
mod bond_tests {
    use super::*;

    #[test]
    fn a_room_counts_its_bonded_speakers_that_dropped_off() {
        let zones: x2rock::sonos::proto::Zones = serde_json::from_value(serde_json::json!({
            "zones": [{ "name": "Living Room", "members": [
                { "id": "RINCON_BAR", "channelMap": ["LF", "RF"], "state": { "disconnected": false } },
                { "id": "RINCON_SUB", "channelMap": ["SW"], "state": { "disconnected": true } },
            ]}]
        }))
        .unwrap();
        let mut house = Household::default();
        x2rock::sonos::proto::Bond::merge(&mut house.bonds, zones);
        assert_eq!(house.offline_in("Living Room"), 1);
        assert_eq!(house.offline_in("Kitchen"), 0);
    }

    #[test]
    fn a_sub_and_surrounds_are_found_by_their_channels_in_the_soundbars_room() {
        let zones: x2rock::sonos::proto::Zones = serde_json::from_value(serde_json::json!({
            "zones": [
                { "name": "Living Room", "members": [
                    { "id": "RINCON_BAR", "channelMap": ["LF", "RF"] },
                    { "id": "RINCON_SUB", "channelMap": ["SW"] },
                    { "id": "RINCON_LR", "channelMap": ["LR"] },
                    { "id": "RINCON_RR", "channelMap": ["RR"] },
                ]},
                { "name": "Bedroom", "members": [
                    { "id": "RINCON_BEAM", "channelMap": ["LF", "RF"] },
                    { "id": "RINCON_SL1", "channelMap": ["LR"] },
                ]},
                { "name": "Kitchen", "members": [{ "id": "RINCON_ONE", "channelMap": ["LF", "RF"] }] },
            ]
        }))
        .unwrap();
        let mut house = Household::default();
        x2rock::sonos::proto::Bond::merge(&mut house.bonds, zones);
        assert!(house.has_sub("RINCON_BAR") && house.has_surrounds("RINCON_BAR"));
        assert!(!house.has_sub("RINCON_BEAM") && house.has_surrounds("RINCON_BEAM"));
        assert!(!house.has_sub("RINCON_ONE") && !house.has_surrounds("RINCON_ONE"));
        assert!(!house.has_sub("RINCON_UNKNOWN"));
    }
}
