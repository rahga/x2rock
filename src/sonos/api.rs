//! The Control API commands x2rock uses, as methods on a connection.
//!
//! Kept apart from the transport so `local.rs` stays about moving frames and this
//! stays about what the frames mean. Group-targeted commands should be sent to the
//! group's coordinator; callers are responsible for connecting to the right player.

use anyhow::Result;
use serde_json::{Value, json};

use super::local::Connection;
use super::proto::{FavoritesList, GroupInfo, MetadataStatus, PlaybackStatus, Repeat, Volume};

fn on_player(namespace: &str, command: &str, player_id: &str) -> Value {
    json!({
        "namespace": namespace,
        "command": command,
        "playerId": player_id,
    })
}

fn on_group(namespace: &str, command: &str, group_id: &str) -> Value {
    json!({
        "namespace": namespace,
        "command": command,
        "groupId": group_id,
    })
}

impl Connection {
    /// Start receiving a namespace's events for a group. They arrive on
    /// [`Connection::events`], beginning with a snapshot of the current state.
    pub async fn subscribe_group(&self, namespace: &str, group_id: &str) -> Result<()> {
        self.call(on_group(namespace, "subscribe", group_id), json!({}))
            .await?;
        Ok(())
    }

    /// As [`Self::subscribe_group`], for the player-scoped namespaces - the one
    /// that matters here being `playerVolume:1`, which is how a single speaker
    /// in a group is heard from and adjusted.
    pub async fn subscribe_player(&self, namespace: &str, player_id: &str) -> Result<()> {
        self.call(on_player(namespace, "subscribe", player_id), json!({}))
            .await?;
        Ok(())
    }

    /// As [`Self::subscribe_group`], for household-wide namespaces such as `groups:1`.
    pub async fn subscribe_household(&self, namespace: &str, household_id: &str) -> Result<()> {
        self.call(
            json!({
                "namespace": namespace,
                "command": "subscribe",
                "householdId": household_id,
            }),
            json!({}),
        )
        .await?;
        Ok(())
    }

    /// A `playback:1` command with no parameters: `play`, `pause`, `togglePlayPause`,
    /// `skipToNextTrack`, `skipToPreviousTrack`.
    pub async fn playback(&self, group_id: &str, command: &str) -> Result<()> {
        self.call(on_group("playback:1", command, group_id), json!({}))
            .await?;
        Ok(())
    }

    pub async fn playback_status(&self, group_id: &str) -> Result<PlaybackStatus> {
        let body = self
            .call(
                on_group("playback:1", "getPlaybackStatus", group_id),
                json!({}),
            )
            .await?;
        Ok(serde_json::from_value(body)?)
    }

    pub async fn seek_to(&self, group_id: &str, position_millis: u64) -> Result<()> {
        self.call(
            on_group("playback:1", "seek", group_id),
            json!({ "positionMillis": position_millis }),
        )
        .await?;
        Ok(())
    }

    pub async fn seek_by(&self, group_id: &str, delta_millis: i64) -> Result<()> {
        self.call(
            on_group("playback:1", "seekRelative", group_id),
            json!({ "deltaMillis": delta_millis }),
        )
        .await?;
        Ok(())
    }

    /// Repeat the queue, the current track, or neither. Both Sonos flags are sent
    /// so the result never depends on what the other one was.
    pub async fn set_repeat(&self, group_id: &str, repeat: Repeat) -> Result<()> {
        self.call(
            on_group("playback:1", "setPlayModes", group_id),
            json!({ "playModes": {
                "repeat": repeat == Repeat::All,
                "repeatOne": repeat == Repeat::One,
            }}),
        )
        .await?;
        Ok(())
    }

    /// Only the mode given changes; Sonos keeps the others as they were.
    pub async fn set_shuffle(&self, group_id: &str, shuffle: bool) -> Result<()> {
        self.call(
            on_group("playback:1", "setPlayModes", group_id),
            json!({ "playModes": { "shuffle": shuffle } }),
        )
        .await?;
        Ok(())
    }

    /// Add players to a group, or take them out of it.
    ///
    /// Preferred over `createGroup` for both directions: it keeps the group -
    /// and so whatever it is playing - and moves only the players named. A
    /// removed player becomes a group of its own. Empty on both sides is a
    /// no-op that still reports the group, which is how the caller can check.
    pub async fn modify_group_members(
        &self,
        group_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<GroupInfo> {
        let body = self
            .call(
                on_group("groups:1", "modifyGroupMembers", group_id),
                json!({ "playerIdsToAdd": add, "playerIdsToRemove": remove }),
            )
            .await?;
        Ok(serde_json::from_value(body)?)
    }

    /// Every favorite saved in the household. Household-scoped, so it can be
    /// asked of any player, not just a coordinator.
    pub async fn favorites(&self, household_id: &str) -> Result<FavoritesList> {
        let body = self
            .call(
                json!({
                    "namespace": "favorites:1",
                    "command": "getFavorites",
                    "householdId": household_id,
                }),
                json!({}),
            )
            .await?;
        Ok(serde_json::from_value(body)?)
    }

    /// Replace what a group is playing with a favorite, and start it.
    ///
    /// This is the one way x2rock can begin playback from nothing: `play` only
    /// resumes, and a room with an empty queue has nothing to resume.
    pub async fn load_favorite(&self, group_id: &str, favorite_id: &str) -> Result<()> {
        self.call(
            on_group("favorites:1", "loadFavorite", group_id),
            json!({ "favoriteId": favorite_id, "playOnCompletion": true }),
        )
        .await?;
        Ok(())
    }

    pub async fn metadata(&self, group_id: &str) -> Result<MetadataStatus> {
        let body = self
            .call(
                on_group("playbackMetadata:1", "getMetadataStatus", group_id),
                json!({}),
            )
            .await?;
        Ok(serde_json::from_value(body)?)
    }

    pub async fn group_volume(&self, group_id: &str) -> Result<Volume> {
        let body = self
            .call(on_group("groupVolume:1", "getVolume", group_id), json!({}))
            .await?;
        Ok(serde_json::from_value(body)?)
    }

    /// Absolute level. For controls with a known position, such as a slider.
    pub async fn set_group_volume(&self, group_id: &str, volume: u8) -> Result<()> {
        self.call(
            on_group("groupVolume:1", "setVolume", group_id),
            json!({ "volume": volume }),
        )
        .await?;
        Ok(())
    }

    /// Relative change. For stateless controls - buttons, scroll wheels - where
    /// read-modify-write would race against the player's own events.
    pub async fn adjust_group_volume(&self, group_id: &str, delta: i8) -> Result<()> {
        self.call(
            on_group("groupVolume:1", "setRelativeVolume", group_id),
            json!({ "volumeDelta": delta }),
        )
        .await?;
        Ok(())
    }

    /// One speaker's own volume, rather than the group's.
    ///
    /// Grouped rooms share a group volume that moves them together; this is the
    /// balance between them, and the only way to make one room quieter than the
    /// rest without ungrouping it.
    pub async fn player_volume(&self, player_id: &str) -> Result<Volume> {
        let body = self
            .call(
                on_player("playerVolume:1", "getVolume", player_id),
                json!({}),
            )
            .await?;
        Ok(serde_json::from_value(body)?)
    }

    pub async fn set_player_volume(&self, player_id: &str, volume: u8) -> Result<()> {
        self.call(
            on_player("playerVolume:1", "setVolume", player_id),
            json!({ "volume": volume }),
        )
        .await?;
        Ok(())
    }

    /// Relative, for the same reason [`Self::adjust_group_volume`] is.
    pub async fn adjust_player_volume(&self, player_id: &str, delta: i8) -> Result<()> {
        self.call(
            on_player("playerVolume:1", "setRelativeVolume", player_id),
            json!({ "volumeDelta": delta }),
        )
        .await?;
        Ok(())
    }

    /// Mute without touching the level, so the relative volumes of the group's
    /// players survive - which setting volume to zero would destroy.
    pub async fn set_group_mute(&self, group_id: &str, muted: bool) -> Result<()> {
        self.call(
            on_group("groupVolume:1", "setMute", group_id),
            json!({ "muted": muted }),
        )
        .await?;
        Ok(())
    }
}
