//! Volume, the one control that is both per group and per speaker. `apply_vol`
//! is the single place a level is set - the group slider, `--player` on one
//! member, mute, the fixed-volume refusal and the report all live here once -
//! and `normalize` and `--each` are the two ways of evening a group out: to
//! its own average, or to one level on every member.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;

use super::{PerRoom, Report, fan_out_as, transition};
use crate::session::{self, Session};
use crate::sonos;
use crate::sonos::local::Connection;
use crate::sonos::proto::Player;
use crate::sonos::upnp::Upnp;

enum VolumeChange {
    Set(u8),
    Adjust(i8),
    Mute(bool),
    Normalize,
}

fn parse_volume(text: &str) -> Result<VolumeChange> {
    match text {
        "mute" => Ok(VolumeChange::Mute(true)),
        "unmute" => Ok(VolumeChange::Mute(false)),
        "normalize" => Ok(VolumeChange::Normalize),
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

/// A volume read or set on a group or one speaker.
///
/// `previous_volume` is present only when there was a previous - a set, even
/// to the same value - which is what tells a set from a read where nothing
/// moved, and makes a clamp (+5 at 100) or a fixed-volume refusal visible.
/// `audible` folds volume and mute into one answer. `balanced` is only on a
/// group read: after a set the members' levels are not yet readable, and one
/// speaker has no balance to report.
#[derive(Debug, Serialize)]
pub struct VolumeOutcome {
    pub room: String,
    pub volume: u8,
    pub previous_volume: Option<u8>,
    pub muted: bool,
    pub audible: bool,
    pub fixed: bool,
    pub ramp_seconds: Option<u64>,
    pub balanced: Option<bool>,
    /// A ramp was asked for; the text says "(ramping)" when the player did not
    /// say for how long. Not part of the documented shape.
    #[serde(skip)]
    ramped: bool,
    #[serde(skip)]
    notes: Vec<String>,
}

impl Report for VolumeOutcome {
    fn text(&self) -> String {
        let before = self.previous_volume.unwrap_or(self.volume);
        let from = transition(&before.to_string(), &self.volume.to_string());
        let muted = if self.muted { "  (muted)" } else { "" };
        let over = match self.ramp_seconds {
            Some(s) => format!("  (over ~{s}s)"),
            // Ramping, but the player did not say for how long.
            None if self.ramped => "  (ramping)".to_string(),
            None => String::new(),
        };
        let uneven = if self.balanced == Some(false) {
            "  (members differ; `vol normalize` evens them)"
        } else {
            ""
        };
        format!(
            "{:<24} {from}{}{muted}{over}{uneven}",
            self.room, self.volume
        )
    }
    fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// One member of a normalized group: where it was, where it is now.
#[derive(Debug, Serialize)]
pub struct MemberVolume {
    pub room: String,
    pub volume: u8,
    pub previous_volume: u8,
}

/// `vol normalize`: the group's level, and every member brought to it.
#[derive(Debug, Serialize)]
pub struct NormalizeOutcome {
    pub room: String,
    pub volume: u8,
    /// Always true once normalized; kept so the shape matches a group read's.
    pub balanced: bool,
    pub members: Vec<MemberVolume>,
}

impl Report for NormalizeOutcome {
    fn text(&self) -> String {
        let (label, level) = (&self.room, self.volume);
        if self.members.len() == 1 {
            format!("{label:<24} {level}  (not grouped; nothing to normalize)")
        } else if self.members.iter().all(|m| m.previous_volume == m.volume) {
            format!("{label:<24} {level}  (already even)")
        } else {
            let each: Vec<String> = self
                .members
                .iter()
                .map(|m| {
                    format!(
                        "{} {}{}",
                        m.room,
                        transition(&m.previous_volume.to_string(), &m.volume.to_string()),
                        m.volume
                    )
                })
                .collect();
            format!("{label:<24} {level}  normalized: {}", each.join(", "))
        }
    }
}

/// What `vol` did: the word `normalize` is one of the volume words, so the one
/// entry point answers with either shape.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum VolOutcome {
    Level(VolumeOutcome),
    Normalized(NormalizeOutcome),
}

impl Report for VolOutcome {
    fn text(&self) -> String {
        match self {
            VolOutcome::Level(o) => o.text(),
            VolOutcome::Normalized(o) => o.text(),
        }
    }
    fn notes(&self) -> &[String] {
        match self {
            VolOutcome::Level(o) => o.notes(),
            VolOutcome::Normalized(o) => o.notes(),
        }
    }
}

/// Set or read a room's volume. The one place volume is applied, so the
/// single-room arm and the multi-room fan-out share it - `--player` scoping,
/// the fixed-volume refusal, mute, and the report-what-was-asked rule all live
/// here once.
pub async fn apply_vol(
    session: &session::Session,
    target: &session::Target,
    room: Option<&str>,
    change: Option<String>,
    one_room: bool,
    ramp: bool,
) -> Result<VolOutcome> {
    let group = target.group_id.as_str();
    // A ramp is a RenderingControl action, which is per speaker and has no
    // group counterpart, so asking for one *is* asking for --player. Implied
    // rather than required, because a lone room - the common case - has no
    // meaningful difference between the two and should not have to say both.
    let one_room = one_room || ramp;
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
    // Resolved before anything is opened, so the address is available without a
    // mutable written from inside a match arm.
    let speaker_ip = this
        .as_ref()
        .map(|named| {
            named
                .ip()
                .with_context(|| format!("{} did not report an address to reach it on", named.name))
        })
        .transpose()?;
    // One connection, resolved lazily. A player-scoped command is refused by
    // anyone but that player ("Incorrect playerId") so it cannot ride the
    // coordinator's; a group-scoped one *is* the coordinator's, which is why
    // the group calls below use this same handle. Reaching the coordinator is a
    // full WSS handshake, and `--player`/`--ramp` never need it - opening it
    // unconditionally spent one per call on nothing.
    let speaker = match speaker_ip {
        Some(ip) => session.player(Some(ip)).await?,
        None => session::coordinator(session, target).await?,
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
        None => speaker.group_volume(group).await?,
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
    if matches!(change, Some(VolumeChange::Normalize)) {
        ensure!(
            this.is_none(),
            "normalize sets a whole group to its level; it takes no --player or --ramp"
        );
        return normalize_group(session, target, &label, before.volume)
            .await
            .map(VolOutcome::Normalized);
    }
    // One pass that both validates the ramp and produces the level it slides
    // to, so there is a single thing to branch on below rather than a flag, an
    // Option and two `ensure!`s that re-derive each other. Both the absolute
    // and relative forms funnel into one call; the clamp is the same one the
    // relative path does, because the player takes an absolute level.
    let ramp_to = match (ramp, &change) {
        (false, _) => None,
        (true, Some(VolumeChange::Set(level))) => Some(*level),
        (true, Some(VolumeChange::Adjust(delta))) => {
            Some((i16::from(before.volume) + i16::from(*delta)).clamp(0, 100) as u8)
        }
        (true, Some(VolumeChange::Mute(_))) => {
            bail!("--ramp does not apply to mute; there is no level to slide to")
        }
        (true, None) => bail!("--ramp needs a level to slide to, e.g. `vol 30 --ramp`"),
        (true, Some(VolumeChange::Normalize)) => unreachable!("normalize returned above"),
    };
    let mut ramp_secs = None;
    let mut notes = Vec::new();
    let (level, muted) = match change {
        _ if ramp_to.is_some() => {
            let level = ramp_to.expect("matched just above");
            // `ramp` implies `one_room`, which is what makes `speaker_ip` Some.
            let ip = speaker_ip.expect("a ramp is always addressed to one speaker");
            // Stays `None` when the player did not say, so `ramp_seconds` is
            // null rather than a zero that would read as "already there".
            ramp_secs = Upnp::new(ip)
                .ramp_to_volume(level)
                .await?
                .map(|d| d.as_secs());
            // A ramp leaves mute alone rather than clearing it the way a plain
            // set does, so a muted speaker would slide silently. Say so instead
            // of letting the level look like it took effect.
            if before.muted {
                notes.push(format!(
                    "note: {label} is muted, so the ramp will not be heard until it is unmuted"
                ));
            }
            (level, before.muted)
        }
        None => (before.volume, before.muted),
        // Both setVolume and setRelativeVolume unmute (verified).
        Some(VolumeChange::Set(level)) => {
            match &this {
                Some(id) => speaker.set_player_volume(id, level).await?,
                None => speaker.set_group_volume(group, level).await?,
            }
            (level, false)
        }
        Some(VolumeChange::Adjust(delta)) => {
            match &this {
                Some(id) => speaker.adjust_player_volume(id, delta).await?,
                None => speaker.adjust_group_volume(group, delta).await?,
            }
            let level = (i16::from(before.volume) + i16::from(delta)).clamp(0, 100);
            (level as u8, false)
        }
        Some(VolumeChange::Mute(muted)) => {
            // Muting one speaker of a group is not offered: the group mute is
            // what people mean, and a silently muted member is a puzzle later.
            ensure!(this.is_none(), "--player does not apply to mute");
            speaker.set_group_mute(group, muted).await?;
            (before.volume, muted)
        }
        Some(VolumeChange::Normalize) => unreachable!("normalize returned above"),
    };
    // Only on a group read: after a set the members' levels are not yet
    // readable, and one speaker has no balance to report.
    let grouped = session
        .groups
        .group_of(&target.coordinator_id)
        .is_some_and(|g| g.player_ids.len() > 1);
    let balanced = match (this.is_none() && !was_set, grouped) {
        (false, _) => None,
        (true, false) => Some(true),
        (true, true) => {
            let members = member_volumes(session, target).await?;
            Some(all_at(before.volume, members.iter().map(|(_, _, v)| v)))
        }
    };
    Ok(VolOutcome::Level(VolumeOutcome {
        room: label,
        volume: level,
        previous_volume: was_set.then_some(before.volume),
        muted,
        audible: !muted && level > 0,
        fixed: before.fixed,
        ramp_seconds: ramp_secs,
        balanced,
        ramped: ramp,
        notes,
    }))
}

/// Each speaker in the target's group with its own volume, read in parallel.
///
/// A player-scoped read is refused by any other player, so each member is
/// asked over its own connection, which the session's pool holds or opens.
async fn member_volumes<'a>(
    session: &'a session::Session,
    target: &session::Target,
) -> Result<Vec<(&'a Player, Connection, sonos::proto::Volume)>> {
    let group = session
        .groups
        .group_of(&target.coordinator_id)
        .with_context(|| format!("no group for {}", target.name))?;
    let reads = session
        .groups
        .members(group)
        .into_iter()
        .map(|p| async move {
            let ip = p
                .ip()
                .with_context(|| format!("{} did not report an address to reach it on", p.name))?;
            let connection = session.player(Some(ip)).await?;
            let volume = connection.player_volume(&p.id).await?;
            anyhow::Ok((p, connection, volume))
        });
    futures_util::future::join_all(reads)
        .await
        .into_iter()
        .collect()
}

/// Whether every member with a volume of its own sits at the group's level.
/// A fixed-volume member has no level to even out, so it does not count.
fn all_at<'a>(level: u8, mut volumes: impl Iterator<Item = &'a sonos::proto::Volume>) -> bool {
    volumes.all(|v| v.fixed || v.volume == level)
}

/// `vol normalize`: every speaker in the group set to the group's own level,
/// the Sonos app's "Normalize Group Volume". The group level is the rounded
/// average of its members, so writing it back leaves the group level where it
/// was. Members already there are left alone - a set also unmutes that
/// speaker, and it has nothing to change.
async fn normalize_group(
    session: &session::Session,
    target: &session::Target,
    label: &str,
    level: u8,
) -> Result<NormalizeOutcome> {
    let mut members = Vec::new();
    for (player, connection, volume) in member_volumes(session, target).await? {
        if !volume.fixed && volume.volume != level {
            connection.set_player_volume(&player.id, level).await?;
        }
        members.push(MemberVolume {
            room: player.name.clone(),
            volume: if volume.fixed { volume.volume } else { level },
            previous_volume: volume.volume,
        });
    }
    Ok(NormalizeOutcome {
        room: label.to_string(),
        volume: level,
        balanced: true,
        members,
    })
}

/// `vol --each`: every member of `room`'s group to one level, as `--player`
/// fanned over the members read from the current topology. One group only,
/// so `all` and a second room are refused; `rooms` is how many `--room` were
/// typed.
pub async fn each(
    session: &Session,
    room: Option<&str>,
    all: bool,
    rooms: usize,
    change: Option<String>,
    ramp: bool,
    json: bool,
) -> Result<()> {
    // One group only: --each already means "every member here", so
    // spreading it over --all's groups or several --room is a second axis
    // that would only muddy what it does.
    ensure!(!all, "--each acts on one group; drop --all");
    ensure!(rooms <= 1, "--each acts on one group; name a single --room");
    // Refused up front, not left to surface per member as a confusing
    // "--player does not apply to mute": muting each speaker is not what
    // --each is for, and group mute is what mute means.
    match change.as_deref().map(parse_volume).transpose()? {
        Some(VolumeChange::Mute(_)) => {
            bail!("--each does not apply to mute; mute is group-wide")
        }
        Some(VolumeChange::Normalize) => {
            bail!("--each does not apply to normalize, which already sets every member")
        }
        _ => {}
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
    // Carried through: `--each` fans over member names as `--player`, which
    // is exactly the shape a ramp needs, so `--ramp --each` slides every
    // member rather than being refused.
    let per_member = PerRoom::Vol {
        change: &change,
        one_room: true,
        ramp,
        json,
    };
    fan_out_as(session, &members, per_member).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `normalize` parses as a volume word alongside the levels and `mute`, and
    /// `all_at` - the read that decides whether a group reports "members
    /// differ" - leaves a fixed-volume member out of the question, since a Port
    /// feeding an amplifier has no level of its own to even out.
    #[test]
    fn normalize_is_a_volume_word_and_fixed_members_do_not_unbalance() {
        assert!(matches!(
            parse_volume("normalize"),
            Ok(VolumeChange::Normalize)
        ));
        let at = |volume, fixed| sonos::proto::Volume {
            volume,
            muted: false,
            fixed,
        };
        // The screenshots' group: 5, 5, 5 and 0 is a group at 4.
        let uneven = [at(5, false), at(5, false), at(5, false), at(0, false)];
        assert!(!all_at(4, uneven.iter()));
        let even = [at(4, false), at(4, false), at(100, true)];
        assert!(all_at(4, even.iter()));
    }

    fn level(volume: u8, previous: Option<u8>) -> VolumeOutcome {
        VolumeOutcome {
            room: "Kitchen".into(),
            volume,
            previous_volume: previous,
            muted: false,
            audible: volume > 0,
            fixed: false,
            ramp_seconds: None,
            balanced: None,
            ramped: false,
            notes: Vec::new(),
        }
    }

    /// The JSON is a documented shape (the skill's `vol --json`), and a read
    /// and a set print exactly the lines they always have.
    #[test]
    fn a_volume_outcome_keeps_the_documented_keys_and_lines() {
        let read = serde_json::to_value(VolOutcome::Level(level(9, None))).unwrap();
        let keys: Vec<_> = read.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "audible",
                "balanced",
                "fixed",
                "muted",
                "previous_volume",
                "ramp_seconds",
                "room",
                "volume"
            ]
        );
        assert_eq!(read["previous_volume"], serde_json::Value::Null);
        assert_eq!(level(9, None).text(), "Kitchen                  9");
        assert_eq!(level(14, Some(9)).text(), "Kitchen                  9 → 14");

        let mut ramp = level(12, Some(9));
        ramp.muted = true;
        ramp.ramp_seconds = Some(2);
        ramp.ramped = true;
        ramp.notes.push("note: Kitchen is muted".into());
        assert_eq!(
            ramp.text(),
            "Kitchen                  9 → 12  (muted)  (over ~2s)"
        );
        assert_eq!(ramp.notes().len(), 1);
        ramp.ramp_seconds = None;
        assert!(ramp.text().ends_with("(ramping)"));

        let mut uneven = level(9, None);
        uneven.balanced = Some(false);
        assert!(uneven.text().contains("`vol normalize` evens them"));
    }

    #[test]
    fn a_normalize_outcome_says_which_of_its_three_things_happened() {
        let member = |room: &str, previous, volume| MemberVolume {
            room: room.into(),
            volume,
            previous_volume: previous,
        };
        let mut out = NormalizeOutcome {
            room: "Kitchen + 1".into(),
            volume: 9,
            balanced: true,
            members: vec![member("Kitchen", 9, 9)],
        };
        assert_eq!(
            out.text(),
            "Kitchen + 1              9  (not grouped; nothing to normalize)"
        );
        out.members.push(member("Dining Room", 9, 9));
        assert_eq!(out.text(), "Kitchen + 1              9  (already even)");
        out.members[1].previous_volume = 4;
        assert_eq!(
            out.text(),
            "Kitchen + 1              9  normalized: Kitchen 9, Dining Room 4 → 9"
        );
        let json = serde_json::to_value(VolOutcome::Normalized(out)).unwrap();
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["balanced", "members", "room", "volume"]);
        assert_eq!(json["members"][1]["previous_volume"], 4);
    }
}
