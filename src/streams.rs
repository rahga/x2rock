//! The last direct stream x2rock started in a room, so `play` can resume it.
//!
//! A service with no queue support here (Amazon Music on a Prime account among
//! them) is played from a signed URL `getMediaURI` hands back, and that URL
//! expires. When it does the room goes idle and `play` cannot resume it: the
//! player holds only the dead URL, not the item that produced it, and the
//! item's own id is often `-1` in the metadata. So each such stream is
//! remembered here by the room it played in, and `play` on a room whose stream
//! has died re-resolves a fresh URL from the item and plays that.
//!
//! A convenience cache, not user data: last-writer-wins with no lock, and a
//! missing or unreadable file is simply "nothing remembered", the way the
//! service catalogue treats its own. Keyed by the **coordinator player's id**,
//! which grouping and ungrouping leave alone - a group's display name
//! ("Dining Room + 1") changes with its members, and a note keyed by that name
//! went missing the moment a room was grouped into or out of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::store;

const SCHEMA: u32 = 2;

/// The item behind a direct stream: enough to ask the service for a fresh URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    pub service_id: String,
    pub item_id: String,
    /// What the room shows while it plays. The resume guard matches on it: the
    /// player keeps the dead stream's title, so a title that no longer matches
    /// means the room moved on and the note is stale.
    pub title: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Streams {
    #[serde(default)]
    schema: u32,
    /// Coordinator player id -> the last direct stream started on its group.
    #[serde(default)]
    players: BTreeMap<String, Stream>,
}

fn path() -> Result<PathBuf> {
    store::path("streams.json")
}

impl Streams {
    /// Load, treating missing or corrupt as empty - it is only a cache, wholly
    /// rebuilt the next time a stream is started.
    pub fn load() -> Self {
        path().ok().map(|p| Self::load_at(&p)).unwrap_or_default()
    }

    fn load_at(path: &Path) -> Self {
        store::read_optional(path)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .filter(|s| s.schema == SCHEMA)
            .unwrap_or_default()
    }

    pub fn get(&self, coordinator_id: &str) -> Option<&Stream> {
        self.players.get(coordinator_id)
    }

    /// Remember the stream now playing on `coordinator_id`'s group, replacing
    /// any before it.
    pub fn remember(coordinator_id: &str, stream: Stream) -> Result<()> {
        Self::remember_at(&path()?, coordinator_id, stream)
    }

    fn remember_at(path: &Path, coordinator_id: &str, stream: Stream) -> Result<()> {
        let mut all = Self::load_at(path);
        all.schema = SCHEMA;
        all.players.insert(coordinator_id.to_string(), stream);
        store::write_atomically(path, &serde_json::to_string_pretty(&all)?, store::PLAIN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("x2rock-streams-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("streams.json")
    }

    fn stream(id: &str, title: &str) -> Stream {
        Stream {
            service_id: "201".into(),
            item_id: id.into(),
            title: title.into(),
        }
    }

    #[test]
    fn a_remembered_stream_reads_back_by_coordinator() {
        let path = scratch("roundtrip");
        Streams::remember_at(&path, "RINCON_1", stream("i1", "Bodies")).unwrap();
        let back = Streams::load_at(&path);
        assert_eq!(back.get("RINCON_1"), Some(&stream("i1", "Bodies")));
        assert_eq!(back.get("RINCON_2"), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn remembering_a_player_again_replaces_its_stream() {
        let path = scratch("replace");
        Streams::remember_at(&path, "RINCON_1", stream("i1", "Old")).unwrap();
        Streams::remember_at(&path, "RINCON_1", stream("i2", "New")).unwrap();
        // Another player is untouched by the replacement.
        Streams::remember_at(&path, "RINCON_2", stream("k1", "Kitchen thing")).unwrap();
        let back = Streams::load_at(&path);
        assert_eq!(back.get("RINCON_1"), Some(&stream("i2", "New")));
        assert_eq!(back.get("RINCON_2"), Some(&stream("k1", "Kitchen thing")));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_or_stale_schema_file_is_empty_not_an_error() {
        let missing = scratch("missing").with_file_name("nothing.json");
        assert!(Streams::load_at(&missing).players.is_empty());

        // Schema 1 keyed by room display name, which grouping renamed from
        // under it; discarded rather than read as a set of unknown players.
        let stale = scratch("stale");
        std::fs::write(
            &stale,
            r#"{"schema":1,"rooms":{"Media Room":{"service_id":"201","item_id":"i","title":"t"}}}"#,
        )
        .unwrap();
        assert!(
            Streams::load_at(&stale).players.is_empty(),
            "a schema this build does not understand is discarded, like the catalogue"
        );
        std::fs::remove_file(&stale).ok();
    }
}
