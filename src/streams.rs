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
//! service catalogue treats its own. Keyed by the room (or group) display name
//! the play resolved to, which both the write and the resume read the same way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::store;

const SCHEMA: u32 = 1;

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
    /// Room (or group) display name -> the last direct stream started there.
    #[serde(default)]
    rooms: BTreeMap<String, Stream>,
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

    pub fn get(&self, room: &str) -> Option<&Stream> {
        self.rooms.get(room)
    }

    /// Remember the stream now playing in `room`, replacing any before it.
    pub fn remember(room: &str, stream: Stream) -> Result<()> {
        Self::remember_at(&path()?, room, stream)
    }

    fn remember_at(path: &Path, room: &str, stream: Stream) -> Result<()> {
        let mut all = Self::load_at(path);
        all.schema = SCHEMA;
        all.rooms.insert(room.to_string(), stream);
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
    fn a_remembered_stream_reads_back_by_room() {
        let path = scratch("roundtrip");
        Streams::remember_at(&path, "Media Room", stream("i1", "Bodies")).unwrap();
        let back = Streams::load_at(&path);
        assert_eq!(back.get("Media Room"), Some(&stream("i1", "Bodies")));
        assert_eq!(back.get("Kitchen"), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn remembering_a_room_again_replaces_its_stream() {
        let path = scratch("replace");
        Streams::remember_at(&path, "Media Room", stream("i1", "Old")).unwrap();
        Streams::remember_at(&path, "Media Room", stream("i2", "New")).unwrap();
        // Another room is untouched by the replacement.
        Streams::remember_at(&path, "Kitchen", stream("k1", "Kitchen thing")).unwrap();
        let back = Streams::load_at(&path);
        assert_eq!(back.get("Media Room"), Some(&stream("i2", "New")));
        assert_eq!(back.get("Kitchen"), Some(&stream("k1", "Kitchen thing")));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_or_stale_schema_file_is_empty_not_an_error() {
        let missing = scratch("missing").with_file_name("nothing.json");
        assert!(Streams::load_at(&missing).rooms.is_empty());

        let stale = scratch("stale");
        std::fs::write(&stale, r#"{"schema":999,"rooms":{"Media Room":{"service_id":"201","item_id":"i","title":"t"}}}"#).unwrap();
        assert!(
            Streams::load_at(&stale).rooms.is_empty(),
            "a schema this build does not understand is discarded, like the catalogue"
        );
        std::fs::remove_file(&stale).ok();
    }
}
