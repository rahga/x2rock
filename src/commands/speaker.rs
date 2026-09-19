//! What is set on one speaker rather than on the group it plays with: tone
//! and TruePlay, the soundbar's TV input, remote and night settings, the
//! status light and button lock, the room's name, and the timers - sleep,
//! snooze, and the alarms, which belong to a speaker by uuid. `-r` names the
//! speaker here even when it is grouped, which is the opposite of transport.

use std::net::IpAddr;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;

use super::content::{find_content, queue_sources};
use super::{on_off, on_word, transition, upnp_ip};
use crate::cli::{AlarmAction, AlarmsAction};
use crate::session::{self, Session, Target};
use crate::sonos;
use crate::sonos::local::Connection;
use crate::sonos::proto::{Groups, Player};
use crate::sonos::upnp::{self, Upnp};
use crate::state::State;

/// One alarm as the JSON object `alarms --json` lists and `alarms add --json`
/// returns, so the id an agent reads off a creation is the id it lists by.
fn alarm_json(a: &upnp::Alarm, groups: &Groups) -> serde_json::Value {
    json!({
        "id": a.id,
        "room": groups.player(&a.room_uuid).map(|p| p.name.clone()),
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
}

/// The household's alarms, one line each.
///
/// `RoomUUID` is resolved against the topology for a name, and left as the id
/// when it does not resolve - an alarm survives its room being switched off, and
/// hiding it would be worse than showing a raw id.
fn print_alarms(alarms: &[upnp::Alarm], groups: &Groups, json: bool) {
    let room_of = |uuid: &str| groups.player(uuid).map(|p| p.name.clone());
    if json {
        let items: Vec<_> = alarms.iter().map(|a| alarm_json(a, groups)).collect();
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
            on_word(a.enabled),
            a.program(),
        );
    }
}

/// What one `eq` invocation asked to change; `None` means leave it alone.
///
/// A struct rather than four more parameters: they arrive together, are
/// consumed together, and travel from clap to the handler unchanged.
pub struct ToneRequest {
    pub bass: Option<i8>,
    pub treble: Option<i8>,
    pub loudness: Option<String>,
    pub trueplay: Option<String>,
    pub night: Option<String>,
    pub dialog: Option<String>,
}

/// The speaker `--room` names, and a UPnP handle on it.
///
/// Every per-player command - `eq`, `led`, `buttons`, `rename`, `remote`, and
/// `raw upnp --scope player` - needs exactly this pair, and each used to
/// resolve it itself: the same three lines and the same "did not report an
/// address" wording, seven times over. Returning the handle rather than just
/// the `Player` is what makes "which speaker does `--room` mean" one function
/// instead of a convention.
pub fn named_speaker<'a>(
    session: &'a session::Session,
    target: &session::Target,
    room: Option<&str>,
) -> Result<(&'a sonos::proto::Player, Upnp)> {
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
    Ok((speaker, Upnp::new(ip)))
}

/// A soundbar's TV-remote settings, read or set.
pub async fn apply_remote(
    session: &session::Session,
    target: &session::Target,
    room: Option<&str>,
    feedback: Option<String>,
    repeater: Option<String>,
    json: bool,
) -> Result<()> {
    let (speaker, upnp) = named_speaker(session, target, room)?;

    // Gated on the capability rather than on the fault, the way `eq` gates
    // night mode and dialog: a speaker with no TV input answers every one of
    // these with an opaque UPnP code, and relaying that helps nobody. Checked
    // before the reads too, since even reading is meaningless here.
    let is_soundbar = speaker.has_tv();
    ensure!(
        is_soundbar,
        "{} has no TV input, so it has no TV-remote settings - this is a soundbar command",
        speaker.name
    );

    let wanted_feedback = on_off("feedback", feedback.as_deref())?;
    let wanted_repeater = on_off("repeater", repeater.as_deref())?;

    if let Some(on) = wanted_feedback {
        upnp.set_led_feedback(on).await?;
    }
    if let Some(on) = wanted_repeater {
        upnp.set_ir_repeater(on).await?;
    }
    // Read back rather than echoing what was asked. It does cost a wave the
    // echo would not - `feedback` and `repeater` are known once set, so only
    // `configured` would have to be fetched - but the player is the authority
    // on what it now holds, and `remote_settings` fetches all three
    // concurrently, so the wave is one round trip rather than three.
    let now = upnp.remote_settings().await?;

    if json {
        println!(
            "{}",
            json!({
                "room": speaker.name,
                "feedback": now.feedback,
                "repeater": now.repeater,
                "remote_configured": now.configured,
            })
        );
    } else {
        println!(
            "{:<24} feedback {}  repeater {}  remote {}",
            speaker.name,
            on_word(now.feedback),
            now.repeater.to_lowercase(),
            if now.configured {
                "configured"
            } else {
                "not configured"
            }
        );
    }
    Ok(())
}

/// Rename a room, preserving everything else stored with the name.
///
/// The read-before-write is not caution, it is required: `SetZoneAttributes`
/// has no change-one-field form, so the icon and configuration must be carried
/// over or they are erased. Guessing an icon from the new name looks harmless
/// and is not - a household here had a Dining Room carrying the `living` icon,
/// which a guess would have silently "corrected".
pub async fn apply_rename(
    session: &session::Session,
    state: &mut State,
    target: &session::Target,
    room: Option<&str>,
    new_name: &str,
) -> Result<()> {
    // Every other per-speaker command falls back to the resolved group's
    // coordinator, which is right for a setting nobody else sees. A rename is
    // not that: it changes the name for every app in the house, and in a
    // single-group household the fallback would pick a room and rename it with
    // nothing typed to say which. Make the caller name it.
    ensure!(
        room.is_some(),
        "rename needs an explicit --room: it changes the name for everyone, \
         so which room is not something to infer"
    );
    let (speaker, upnp) = named_speaker(session, target, room)?;
    let id = speaker.id.clone();

    let wanted = new_name.trim();
    ensure!(!wanted.is_empty(), "a room needs a name");
    // The player accepts a duplicate and leaves the household with two rooms of
    // the same name, which `--room` then cannot tell apart. Refused here
    // because nothing downstream can recover from it.
    if let Some(clash) = session
        .groups
        .players
        .iter()
        // `to_lowercase`, not `eq_ignore_ascii_case`: `player_named` and
        // `resolve` fold with `to_lowercase`, and a guard that folds less than
        // the resolver lets through exactly the collision it exists to stop -
        // "KÜCHE" beside an existing "Küche" passes an ASCII check and then
        // resolves ambiguously.
        .find(|p| p.id != id && p.name.to_lowercase() == wanted.to_lowercase())
    {
        bail!(
            "{:?} is already the name of another speaker; \
             two rooms with one name cannot be told apart by --room",
            clash.name
        );
    }

    let before = upnp.zone_attributes().await?;
    if before.name == wanted {
        println!("{:<24} already named {wanted:?}", before.name);
        return Ok(());
    }
    let after = upnp::ZoneAttributes {
        name: wanted.to_string(),
        ..before.clone()
    };
    upnp.set_zone_attributes(&after).await?;

    // The remembered list is keyed by network and only rewritten on attach, so
    // without this the old name is offered by shell completions until the next
    // command runs.
    if state.rename_player(&id, wanted) {
        state.save()?;
    }
    // `transition` yields only the "old \u{2192} " prefix; the new value is the
    // caller's to append, the way every other command here does it.
    println!("{}{wanted}", transition(&before.name, wanted));
    Ok(())
}

/// `on`/`off` for the status light.
///
/// Parsed here rather than passed to the player, which takes any string and
/// reads everything but `Off` as on - `DesiredLEDState=Maybe` was accepted and
/// lit the light. A typo must not quietly mean "on".
fn parse_led(text: &str) -> Result<bool> {
    Ok(on_off("led", Some(text))?.expect("Some in, Some out"))
}

/// `lock`/`unlock` for the touch controls.
///
/// Deliberately **not** on/off. "buttons on" reads as both "the buttons work"
/// and "the lock is on", and the two are opposites; the wire makes it worse by
/// calling the locked state `On` where the Sonos app's switch calls the same
/// state off. Naming the action leaves nothing to guess, so on/off is refused
/// rather than picked a meaning for.
fn parse_button_lock(text: &str) -> Result<bool> {
    match text {
        "lock" => Ok(true),
        "unlock" => Ok(false),
        _ => bail!("buttons takes lock or unlock"),
    }
}

/// A speaker's status light, read or set.
pub async fn apply_led(
    session: &session::Session,
    target: &session::Target,
    room: Option<&str>,
    mode: Option<String>,
    json: bool,
) -> Result<()> {
    let (speaker, upnp) = named_speaker(session, target, room)?;
    let before = upnp.led().await?;
    let after = match mode.as_deref() {
        None => before,
        Some(text) => {
            let on = parse_led(text)?;
            upnp.set_led(on).await?;
            on
        }
    };
    if json {
        println!("{}", json!({ "room": speaker.name, "led": after }));
    } else {
        let from = transition(on_word(before), on_word(after));
        println!("{:<24} led {from}{}", speaker.name, on_word(after));
    }
    Ok(())
}

/// A speaker's touch-control lock, read or set.
pub async fn apply_buttons(
    session: &session::Session,
    target: &session::Target,
    room: Option<&str>,
    mode: Option<String>,
    json: bool,
) -> Result<()> {
    let (speaker, upnp) = named_speaker(session, target, room)?;
    let before = upnp.buttons_locked().await?;
    let after = match mode.as_deref() {
        None => before,
        Some(text) => {
            let locked = parse_button_lock(text)?;
            upnp.set_buttons_locked(locked).await?;
            locked
        }
    };
    if json {
        println!(
            "{}",
            json!({ "room": speaker.name, "buttons_locked": after })
        );
    } else {
        // Its own vocabulary, not on/off: see `parse_button_lock` for why the
        // two words point in opposite directions here.
        let locked = |locked: bool| if locked { "locked" } else { "unlocked" };
        let from = transition(locked(before), locked(after));
        println!("{:<24} buttons {from}{}", speaker.name, locked(after));
    }
    Ok(())
}

/// Bass, treble and loudness on one speaker.
///
/// Addressed to a player, not a group: two rooms playing together each keep
/// their own tone, and the Sonos app agrees - its panel is titled "EQ Settings
/// for <room>".
pub async fn apply_eq(
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
    let (speaker, upnp) = named_speaker(session, target, room)?;

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
    let wanted_loudness = on_off("loudness", loudness.as_deref())?;
    let wanted_trueplay = on_off("trueplay", trueplay.as_deref())?;
    let wanted_night = on_off("night", night.as_deref())?;
    let wanted_dialog = on_off("dialog", dialog.as_deref())?;

    // Night mode and dialog are soundbar-only over UPnP - a non-soundbar answers
    // SetEQ with UPnP 402. Refuse up front with the reason rather than relaying
    // that opaque code, and only when actually setting one: reading them on a
    // non-soundbar already just omits them.
    let is_soundbar = speaker.has_tv();
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
        let control = if upnp.ip() == session.connection.ip() {
            session.connection.clone()
        } else {
            Connection::open(upnp.ip()).await?
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
        // "unavailable" rather than "off" when there is nothing measured: the
        // two are different answers to "is this room corrected?".
        let calibration = if after.trueplay_available {
            format!(
                "{}{}",
                transition(on_word(before.trueplay), on_word(after.trueplay)),
                on_word(after.trueplay)
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
                    on_word(ht.enhance_dialog).to_string()
                };
                format!("  night {}  dialog {dialog}", on_word(ht.night_mode))
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
            transition(on_word(before.loudness), on_word(after.loudness)),
            on_word(after.loudness),
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
pub async fn apply_sleep(
    target: &session::Target,
    player_ip: IpAddr,
    duration: Option<String>,
    json: bool,
) -> Result<()> {
    // AVTransport answers for the group on its coordinator, the way the queue
    // and the TV input do.
    let upnp = Upnp::new(upnp_ip(target, player_ip));
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

/// What snooze means with no duration given: the clock-radio nine minutes.
const SNOOZE_DEFAULT: std::time::Duration = std::time::Duration::from_secs(9 * 60);

/// Silence a sounding alarm, and say which one it was.
///
/// Reads the running alarm *before* snoozing rather than only handling the
/// refusal afterwards, for two reasons: the reply can then name the alarm (so
/// `alarm <id> off` is one obvious step away for someone who wants it to stop
/// permanently, not just this morning), and "no alarm is running" is a clearer
/// thing to say than passing UPnP 701 through.
pub async fn apply_snooze(
    target: &session::Target,
    player_ip: IpAddr,
    duration: Option<String>,
    json: bool,
) -> Result<()> {
    // AVTransport answers for the group on its coordinator, like the sleep
    // timer above.
    let upnp = Upnp::new(upnp_ip(target, player_ip));
    let how_long = match duration.as_deref() {
        None => SNOOZE_DEFAULT,
        // `parse_sleep` is reused for the grammar, but its `off` arm has no
        // meaning here - there is no such thing as snoozing for no time - so it
        // is refused rather than silently treated as the default.
        Some(text) => parse_sleep(text)?.ok_or_else(|| {
            anyhow!("snooze takes a duration; to stop an alarm outright use pause")
        })?,
    };

    let running = upnp.running_alarm().await?.ok_or_else(|| {
        anyhow!(
            "no alarm is running in {}. Snooze silences an alarm that is \
             sounding; to stop this room use pause, and to stop an alarm \
             firing again use: x2rock alarm <id> off",
            target.name
        )
    })?;
    upnp.snooze_alarm(how_long).await?;

    if json {
        println!(
            "{}",
            json!({
                "room": target.name,
                "alarm_id": running.id,
                "snoozed_ms": how_long.as_millis(),
            })
        );
    } else {
        println!(
            "{:<24} alarm {} snoozed {}",
            target.name,
            running.id,
            hms_short(how_long)
        );
    }
    Ok(())
}

/// `x2rock tv`: switch the soundbar in `room`'s group to its TV input. The
/// soundbar is whichever member has one, not necessarily the coordinator.
pub async fn tv(
    session: &Session,
    player: &Connection,
    target: &Target,
    room: Option<&str>,
) -> Result<()> {
    // The soundbar is the player with the HDMI socket, which is not
    // necessarily the one coordinating the group it is in. The room
    // named is asked first; otherwise (or when the widget names the
    // group by its coordinator) it is whichever member has one.
    let is_soundbar = |p: &&Player| p.has_tv();
    // The target's own group, not the room resolved a second time: the caller
    // resolved `room` into `target` already, and the coordinator is always a
    // member of the group it coordinates.
    let group = session
        .groups
        .group_of(&target.coordinator_id)
        .ok_or_else(|| anyhow!("no group for {}", target.name))?;
    let members = session.groups.members(group);
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
    let coordinator_ip = upnp_ip(target, player.ip());
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
    Ok(())
}

/// `x2rock alarms`: list the household's alarms, or with `add` create one on
/// the speaker `room` names.
pub async fn alarms(
    session: &Session,
    room: Option<&str>,
    action: Option<&AlarmsAction>,
    json: bool,
) -> Result<()> {
    let upnp = Upnp::new(session.connection.ip());
    match action {
        None => {
            let alarms = upnp.alarms().await?;
            print_alarms(&alarms, &session.groups, json);
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
            // The program: a favorite or playlist resolved to the same
            // (uri, metadata) pair `queue add` uses, or the built-in chime.
            let (uri, metadata) = match program {
                None => ("x-rincon-buzzer:0".to_string(), String::new()),
                Some(query) => {
                    let sources = queue_sources(&upnp).await?;
                    let item = find_content(&sources, query)?;
                    let uri = item
                        .uri
                        .as_deref()
                        .with_context(|| format!("{:?} has nothing to play", item.title))?;
                    (uri.to_string(), item.metadata.clone())
                }
            };
            let mut alarm = upnp::Alarm {
                id: 0,
                start,
                duration: upnp::format_hms(plays),
                recurrence: recurrence.to_uppercase(),
                enabled: !off,
                room_uuid: speaker.id.clone(),
                program_uri: uri,
                program_metadata: metadata,
                play_mode: play_mode.to_uppercase(),
                volume: *volume,
                include_linked_zones: *grouped,
            };
            alarm.id = upnp.create_alarm(&alarm).await?;
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
                    eprintln!("note: alarm times are the household's; its clock reads {clock}.");
                }
            }
            if json {
                println!("{}", alarm_json(&alarm, &session.groups));
            } else {
                println!(
                    "alarm {} created  {:<16} {}  {}  for {}  vol {}  {}",
                    alarm.id,
                    speaker.name,
                    alarm.start,
                    alarm.recurrence,
                    alarm.duration,
                    alarm.volume,
                    on_word(alarm.enabled),
                );
            }
        }
    }
    Ok(())
}

/// `x2rock alarm <id> on|off|remove`: one alarm, by the id `alarms` lists.
pub async fn alarm(session: &Session, id: u32, action: &AlarmAction) -> Result<()> {
    let upnp = Upnp::new(session.connection.ip());
    let alarms = upnp.alarms().await?;
    let alarm = alarms
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| anyhow!("no alarm with id {id}. `x2rock alarms` lists them."))?;
    match action {
        AlarmAction::Remove { yes } => {
            ensure!(
                *yes,
                "removing alarm {id} cannot be undone (`x2rock alarms add` makes a new \
                 one) - pass --yes"
            );
            upnp.destroy_alarm(id).await?;
            println!("alarm {id} removed");
        }
        wanted => {
            let enabled = matches!(wanted, AlarmAction::On);
            if alarm.enabled != enabled {
                // The whole record goes back, not just this field:
                // UpdateAlarm refuses a partial one with UPnP 402.
                let mut updated = alarm.clone();
                updated.enabled = enabled;
                upnp.update_alarm(&updated).await?;
            }
            println!(
                "alarm {id} {}{}",
                transition(on_word(alarm.enabled), on_word(enabled)),
                on_word(enabled)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The player takes any string for these and reads everything but `Off` as
    /// on, so a typo would silently mean "on". x2rock has to be the thing that
    /// refuses, and `buttons` in particular must refuse on/off rather than
    /// pick a meaning for a word that points both ways.
    #[test]
    fn the_speaker_toggles_refuse_what_the_player_would_have_accepted() {
        assert!(parse_led("on").unwrap());
        assert!(!parse_led("off").unwrap());
        for bad in ["maybe", "On", "1", "true", "lock", ""] {
            assert!(parse_led(bad).is_err(), "led accepted {bad:?}");
        }

        assert!(parse_button_lock("lock").unwrap());
        assert!(!parse_button_lock("unlock").unwrap());
        // The whole reason the command is worded `lock`/`unlock`: "on" means
        // "the buttons work" to one reader and "the lock is on" to another,
        // and the wire and the Sonos app disagree about which is which.
        for ambiguous in ["on", "off", "locked", "unlocked", ""] {
            assert!(
                parse_button_lock(ambiguous).is_err(),
                "buttons accepted {ambiguous:?}"
            );
        }
    }
}
