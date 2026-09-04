//! The Control API commands x2rock uses, as methods on a connection.
//!
//! Kept apart from the transport so `local.rs` stays about moving frames and this
//! stays about what the frames mean. Group-targeted commands should be sent to the
//! group's coordinator; callers are responsible for connecting to the right player.

use anyhow::Result;
use serde_json::{Value, json};

use super::local::Connection;
use super::proto::{
    FavoritesList, GroupInfo, MetadataStatus, PlaybackStatus, PlaylistsList, Repeat, Volume,
};

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

/// The whole `setPlayModes` body for a repeat setting.
///
/// Both flags go every time, so the result never depends on what the other one
/// was. Split out from the call so the mapping can be read - and tested -
/// without a socket, and built complete rather than nested into a `json!` at
/// the call site: a `Value` placed inside another `json!` is re-serialised
/// through `to_value`, which would build this small object twice.
fn repeat_body(repeat: Repeat) -> Value {
    json!({ "playModes": {
        "repeat": repeat == Repeat::All,
        "repeatOne": repeat == Repeat::One,
    }})
}

/// The options for a `musicServiceAccounts:1 match`.
///
/// `linkCode` is sent only when there is one: the field is optional, and a null
/// would be a different request from an absent key.
fn match_options(
    service_id: &str,
    user_id_hash_code: &str,
    nickname: &str,
    link_code: Option<&str>,
) -> Value {
    let mut options = json!({
        "serviceId": service_id,
        "userIdHashCode": user_id_hash_code,
        "nickname": nickname,
    });
    if let Some(code) = link_code {
        options["linkCode"] = json!(code);
    }
    options
}

/// The household's account id out of a `match` reply, under either name it
/// arrives with. A reply without one is still a success - the household
/// accepted the account either way, and inventing an error over a missing field
/// would undo a completed link.
fn account_id(body: &Value) -> Option<String> {
    body.get("id")
        .or_else(|| body.get("accountId"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
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
            repeat_body(repeat),
        )
        .await?;
        Ok(())
    }

    /// Crossfade: overlap the end of one track with the start of the next.
    ///
    /// A third play mode beside repeat and shuffle, and settable the same way -
    /// only the mode named changes.
    pub async fn set_crossfade(&self, group_id: &str, crossfade: bool) -> Result<()> {
        self.call(
            on_group("playback:1", "setPlayModes", group_id),
            json!({ "playModes": { "crossfade": crossfade } }),
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

    /// Register a linked music service account on the household.
    ///
    /// `musicServiceAccounts:1 match` is the last step of a device link, and the
    /// one place a controller uses that namespace for anything but cache
    /// invalidation. An earlier reading of the spec called this a
    /// service-provider-only endpoint, because `userIdHashCode` is documented as
    /// something "your SMAPI server" computes - but `getDeviceAuthToken` hands
    /// that field to whoever completes the link, precisely so the controller can
    /// come here with it.
    ///
    /// Returns the household's own id for the account. Household-scoped, so any
    /// player will answer.
    pub async fn match_music_service_account(
        &self,
        household_id: &str,
        service_id: &str,
        user_id_hash_code: &str,
        nickname: &str,
        link_code: Option<&str>,
    ) -> Result<Option<String>> {
        let body = self
            .call(
                json!({
                    "namespace": "musicServiceAccounts:1",
                    "command": "match",
                    "householdId": household_id,
                }),
                match_options(service_id, user_id_hash_code, nickname, link_code),
            )
            .await?;
        Ok(account_id(&body))
    }

    /// Every saved queue in the household. Household-scoped, like favorites.
    pub async fn playlists(&self, household_id: &str) -> Result<PlaylistsList> {
        let body = self
            .call(
                json!({
                    "namespace": "playlists:1",
                    "command": "getPlaylists",
                    "householdId": household_id,
                }),
                json!({}),
            )
            .await?;
        Ok(serde_json::from_value(body)?)
    }

    /// Replace what a group is playing with a saved playlist, and start it.
    ///
    /// The playlist id must be the bare one `getPlaylists` reports, not the
    /// `SQ:0` form UPnP uses - see [`super::proto::Playlist`].
    ///
    /// **`action` is not optional in practice.** Left out, the player defaults
    /// to `APPEND`: the playlist is added to the end of the queue and playback
    /// jumps there, so calling this twice on a four-track queue leaves twelve
    /// tracks. `REPLACE` is what "play this playlist" means and what
    /// `loadFavorite` does without being asked. `APPEND` and `INSERT_NEXT` are
    /// the other two the player accepts; `queue add` already covers appending.
    pub async fn load_playlist(&self, group_id: &str, playlist_id: &str) -> Result<()> {
        self.call(
            on_group("playlists:1", "loadPlaylist", group_id),
            json!({
                "playlistId": playlist_id,
                "playOnCompletion": true,
                "action": "REPLACE",
            }),
        )
        .await?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    // Every method here is one `self.call`, so what is testable without a socket
    // is the shape of what would go over it: the envelope, and the two bodies
    // that are more than a field copied through.

    #[test]
    fn a_group_command_and_a_player_command_are_addressed_differently() {
        // The same namespace is reached by group or by player depending on the
        // command, and the only difference on the wire is which id key is used.
        // Sending one where the other belongs fails at the player, far from here.
        assert_eq!(
            on_group("groupVolume:1", "setVolume", "g:1"),
            json!({"namespace": "groupVolume:1", "command": "setVolume", "groupId": "g:1"})
        );
        assert_eq!(
            on_player("playerVolume:1", "setVolume", "RINCON_1"),
            json!({"namespace": "playerVolume:1", "command": "setVolume", "playerId": "RINCON_1"})
        );
    }

    #[test]
    fn every_repeat_setting_sends_both_flags() {
        // Off is not "send nothing": leaving a flag out keeps whatever the group
        // had, so turning repeat-one off would silently leave repeat-all on.
        // Asserted as the whole body, which is what goes over the socket.
        assert_eq!(
            repeat_body(Repeat::Off),
            json!({"playModes": {"repeat": false, "repeatOne": false}})
        );
        assert_eq!(
            repeat_body(Repeat::All),
            json!({"playModes": {"repeat": true, "repeatOne": false}})
        );
        assert_eq!(
            repeat_body(Repeat::One),
            json!({"playModes": {"repeat": false, "repeatOne": true}})
        );
    }

    #[test]
    fn a_link_code_is_absent_rather_than_null_when_there_is_none() {
        let without = match_options("284", "hash", "x2rock", None);
        // Absent, not null - the two are different requests, and a service that
        // reads a null linkCode as an empty one rejects the link.
        assert!(without.get("linkCode").is_none());
        assert_eq!(without["serviceId"], "284");
        assert_eq!(without["userIdHashCode"], "hash");
        assert_eq!(without["nickname"], "x2rock");

        let with = match_options("284", "hash", "x2rock", Some("ABCD"));
        assert_eq!(with["linkCode"], "ABCD");
    }

    #[test]
    fn the_account_id_is_read_under_either_name_and_its_absence_is_not_a_failure() {
        assert_eq!(account_id(&json!({"id": "sn_2"})).as_deref(), Some("sn_2"));
        assert_eq!(
            account_id(&json!({"accountId": "sn_3"})).as_deref(),
            Some("sn_3")
        );
        // `id` wins when both are there, being the documented one.
        assert_eq!(
            account_id(&json!({"id": "sn_2", "accountId": "sn_3"})).as_deref(),
            Some("sn_2")
        );
        // No id at all: the link still completed, so this is None rather than an
        // error that would tell the user to redo something already done.
        assert_eq!(account_id(&json!({})), None);
        // And a non-string id is not coerced into one.
        assert_eq!(account_id(&json!({"id": 7})), None);
    }
}
