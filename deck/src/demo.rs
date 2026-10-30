//! A household that is not there: the TV app's screenshot of 12 October 2026,
//! as protocol replies. `--demo` runs on it, and the screenshots are taken of
//! it, so the look can be checked with no speakers on the network.

use std::time::Instant;

use serde_json::{Value, json};
use x2rock::sonos::proto::{Groups, MetadataStatus, PlaybackStatus, Volume};

use crate::art::Decoded;
use crate::model::{GroupLive, Household};

fn player(id: &str, name: &str, ip: &str, tv: bool) -> Value {
    json!({
        "id": id,
        "name": name,
        "websocketUrl": format!("wss://{ip}:1443/websocket/api"),
        "capabilities": if tv { json!(["PLAYBACK", "HT_PLAYBACK", "AIRPLAY"]) } else { json!(["PLAYBACK", "AIRPLAY"]) },
    })
}

fn group(id: &str, name: &str, coordinator: &str, players: &[&str], state: &str) -> Value {
    json!({ "id": id, "name": name, "coordinatorId": coordinator, "playbackState": state, "playerIds": players })
}

fn live(
    state: &str,
    position: u64,
    actions: Value,
    modes: Value,
    metadata: Value,
    volume: u8,
    muted: bool,
) -> GroupLive {
    let mut g = GroupLive::default();
    let status: PlaybackStatus = serde_json::from_value(json!({
        "playbackState": state,
        "positionMillis": position,
        "availablePlaybackActions": actions,
        "playModes": modes,
    }))
    .expect("a playback status");
    g.apply_playback(&status, Instant::now());
    g.metadata =
        Some(serde_json::from_value::<MetadataStatus>(metadata).expect("a metadata status"));
    g.volume = Some(
        serde_json::from_value::<Volume>(
            json!({ "volume": volume, "muted": muted, "fixed": false }),
        )
        .expect("a volume"),
    );
    g
}

fn queue_actions() -> Value {
    json!({ "canPlay": true, "canPause": true, "canSkip": true, "canSkipBack": true, "canSeek": true,
            "canRepeat": true, "canRepeatOne": true, "canShuffle": true, "canCrossfade": true })
}

fn station_actions() -> Value {
    json!({ "canPlay": true, "canPause": true, "canSkip": true })
}

pub const COVER_LOVE_SONGS: &str = "demo:love-songs";
pub const COVER_HOT_HOUSE: &str = "demo:hot-house";
pub const COVER_GAME: &str = "demo:game";
pub const COVER_KID_KOALA: &str = "demo:kid-koala";

pub fn household() -> Household {
    let groups: Groups = serde_json::from_value(json!({
        "groups": [
            group("RINCON_B1:1", "Bedroom", "RINCON_B1", &["RINCON_B1"], "PLAYBACK_STATE_PAUSED"),
            group("RINCON_D1:2", "Dining Room", "RINCON_D1", &["RINCON_D1", "RINCON_D2"], "PLAYBACK_STATE_PLAYING"),
            group("RINCON_G1:3", "Guest TV", "RINCON_G1", &["RINCON_G1"], "PLAYBACK_STATE_PLAYING"),
            group("RINCON_K1:4", "Kitchen", "RINCON_K1", &["RINCON_K1"], "PLAYBACK_STATE_PAUSED"),
            group("RINCON_L1:5", "Living Room", "RINCON_L1", &["RINCON_L1"], "PLAYBACK_STATE_PLAYING"),
        ],
        "players": [
            player("RINCON_B1", "Bedroom", "192.168.1.21", true),
            player("RINCON_D1", "Dining Room", "192.168.1.22", false),
            player("RINCON_D2", "Dining Room Sonos", "192.168.1.27", false),
            player("RINCON_G1", "Guest TV", "192.168.1.23", true),
            player("RINCON_K1", "Kitchen", "192.168.1.24", false),
            player("RINCON_L1", "Living Room", "192.168.1.25", true),
        ],
    }))
    .expect("a topology");

    let mut house = Household {
        groups: Some(groups),
        ..Default::default()
    };
    house.live.insert(
        "RINCON_B1:1".into(),
        live(
            "PLAYBACK_STATE_PAUSED",
            0,
            station_actions(),
            json!({}),
            json!({
                "container": { "name": "Love Songs Radio", "type": "station", "service": { "name": "iHeartRadio" }, "imageUrl": COVER_LOVE_SONGS },
                "currentItem": { "track": { "name": "Amazed", "artist": { "name": "LONESTAR" }, "imageUrl": COVER_LOVE_SONGS } },
            }),
            22,
            false,
        ),
    );
    house.live.insert(
        "RINCON_D1:2".into(),
        live(
            "PLAYBACK_STATE_PLAYING",
            92_000,
            queue_actions(),
            json!({ "shuffle": false, "repeat": false }),
            json!({
                "container": { "name": "Ryo Fukui in New York", "type": "album", "service": { "name": "Qobuz" } },
                "currentItem": { "track": { "name": "Hot House", "artist": { "name": "Ryo Fukui, 福居良" },
                    "album": { "name": "Ryo Fukui in New York" }, "imageUrl": COVER_HOT_HOUSE, "durationMillis": 369_000 } },
                "nextItem": { "track": { "name": "Early Summer", "artist": { "name": "Ryo Fukui" } } },
            }),
            25,
            false,
        ),
    );
    house.live.insert(
        "RINCON_G1:3".into(),
        live(
            "PLAYBACK_STATE_PLAYING",
            1_200_000,
            queue_actions(),
            json!({}),
            json!({
                "container": { "name": "Slow Walkthroughs / Longplays", "type": "playlist", "service": { "name": "YouTube Music" } },
                "currentItem": { "track": { "name": "BALDURS GATE 3 Campaign Soundtrack", "artist": { "name": "Slow Walkthroughs / Longplays" },
                    "imageUrl": COVER_GAME, "durationMillis": 5_400_000 } },
            }),
            18,
            false,
        ),
    );
    house.live.insert(
        "RINCON_K1:4".into(),
        live(
            "PLAYBACK_STATE_PAUSED",
            61_000,
            queue_actions(),
            json!({}),
            json!({
                "container": { "name": "Carpal Tunnel Syndrome", "type": "album" },
                "currentItem": { "track": { "name": "Scratchcratchratchatcha", "artist": { "name": "Kid Koala" },
                    "imageUrl": COVER_KID_KOALA, "durationMillis": 210_000 } },
            }),
            14,
            false,
        ),
    );
    house.live.insert(
        "RINCON_L1:5".into(),
        live(
            "PLAYBACK_STATE_PLAYING",
            0,
            json!({ "canPlay": true, "canPause": true }),
            json!({}),
            json!({
                "container": { "name": "TV Audio", "type": "linein.homeTheater",
                    "htInputFormat": { "numGroundChannels": 5, "numLFEChannels": 1, "streamDescription": "Dolby Digital Surround" } },
            }),
            30,
            false,
        ),
    );
    for (id, level) in [
        ("RINCON_D1", 25),
        ("RINCON_D2", 18),
        ("RINCON_B1", 22),
        ("RINCON_G1", 18),
        ("RINCON_K1", 14),
        ("RINCON_L1", 30),
    ] {
        house.player_volumes.insert(
            id.into(),
            serde_json::from_value(json!({ "volume": level, "muted": false })).unwrap(),
        );
    }
    house.home_theater.insert("RINCON_L1".into(), (true, false));
    house
}

/// Covers drawn here rather than fetched: two colours on a diagonal with a
/// disc, enough to give the pane its shade and Artwork its accent.
pub fn covers() -> Vec<(&'static str, Decoded)> {
    vec![
        (
            COVER_LOVE_SONGS,
            cover([250, 236, 236], [214, 40, 70], [190, 20, 60]),
        ),
        (
            COVER_HOT_HOUSE,
            cover([24, 20, 18], [196, 150, 60], [230, 190, 90]),
        ),
        (
            COVER_GAME,
            cover([40, 70, 140], [120, 60, 40], [230, 140, 60]),
        ),
        (
            COVER_KID_KOALA,
            cover([230, 226, 214], [40, 80, 140], [30, 40, 70]),
        ),
    ]
}

fn cover(top: [u8; 3], bottom: [u8; 3], disc: [u8; 3]) -> Decoded {
    let side = 256u32;
    let mut rgba = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let t = (x + y) as f32 / (2 * side) as f32;
            let (dx, dy) = (x as f32 - 170.0, y as f32 - 150.0);
            let c = if dx * dx + dy * dy < 55.0 * 55.0 {
                disc
            } else {
                [0, 1, 2].map(|i| (top[i] as f32 + (bottom[i] as f32 - top[i] as f32) * t) as u8)
            };
            rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    let pixels: Vec<[u8; 4]> = rgba
        .chunks(4)
        .step_by(37)
        .map(|p| [p[0], p[1], p[2], p[3]])
        .collect();
    Decoded {
        width: side,
        height: side,
        colors: crate::palette::art_colors(&pixels),
        rgba,
        _permit: None,
    }
}

/// A room's settings, as a read would find them: Living Room has a Sub and
/// surrounds, as the household the TV app was built against does.
pub fn room_details(group: &str, player: &str) -> crate::household::RoomDetails {
    use x2rock::sonos::upnp::Tone;
    let tone = Tone {
        bass: 2,
        treble: -1,
        loudness: true,
        trueplay: false,
        trueplay_available: false,
    };
    let satellites = (player == "RINCON_L1").then(|| crate::household::Satellites {
        player: player.to_owned(),
        sub_on: Some(true),
        sub_level: Some(3),
        surrounds_on: Some(true),
        tv_level: Some(2),
        music_level: Some(-4),
    });
    crate::household::RoomDetails {
        group: group.to_owned(),
        player: player.to_owned(),
        sleep_read: true,
        tone: Some(tone),
        status_light: Some(true),
        buttons_locked: Some(false),
        satellites,
        // A limit is said where one is set: the Kitchen's.
        volume_limit: (player == "RINCON_K1").then_some(70),
        remote: ["RINCON_B1", "RINCON_G1", "RINCON_L1"]
            .contains(&player)
            .then(|| {
                (
                    player.to_owned(),
                    x2rock::sonos::upnp::RemoteSettings {
                        feedback: true,
                        repeater: "Off".into(),
                        configured: true,
                    },
                )
            }),
        read_at: Some(std::time::Instant::now()),
        ..Default::default()
    }
}

/// Every speaker, as About and Send a report read them.
pub fn speakers() -> Vec<crate::household::SpeakerInfo> {
    let speaker = |id: &str, room: &str, model: &str| crate::household::SpeakerInfo {
        id: id.into(),
        room: room.into(),
        software: "97.1-80312".into(),
        model: model.into(),
        version: "18.8".into(),
        update_waiting: false,
        satellite: false,
    };
    vec![
        speaker("RINCON_B1", "Bedroom", "Sonos Beam"),
        speaker("RINCON_D1", "Dining Room", "SYMFONISK Bookshelf"),
        speaker("RINCON_D2", "Dining Room", "SYMFONISK Bookshelf"),
        speaker("RINCON_G1", "Guest TV", "Sonos Beam"),
        speaker("RINCON_K1", "Kitchen", "Sonos One SL"),
        speaker("RINCON_L1", "Living Room", "Sonos Beam"),
    ]
}

/// One alarm, as a read would find it.
pub fn alarms() -> Vec<x2rock::sonos::upnp::Alarm> {
    vec![x2rock::sonos::upnp::Alarm {
        id: 7,
        start: "06:45:00".into(),
        duration: "00:15:00".into(),
        recurrence: "WEEKDAYS".into(),
        enabled: true,
        room_uuid: "RINCON_K1".into(),
        program_uri: crate::household::CHIME.into(),
        program_metadata: String::new(),
        play_mode: "NORMAL".into(),
        volume: 12,
        include_linked_zones: false,
    }]
}

/// The favourites an alarm can play.
pub fn alarm_sounds() -> Vec<crate::household::AlarmProgram> {
    ["Smooth Jazz", "Morning Edition"]
        .into_iter()
        .map(|t| crate::household::AlarmProgram {
            title: t.into(),
            uri: format!("x-demo:{t}"),
            metadata: String::new(),
        })
        .collect()
}
