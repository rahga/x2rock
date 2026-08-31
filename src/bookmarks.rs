//! Things worth playing again, remembered by id.
//!
//! Sonos favorites are the household's, saved from the Sonos app, and this
//! household has none. These are x2rock's own, saved from whatever is playing,
//! and they exist because **discovery and repetition are different problems**.
//! Searching a service like YouTube Music needs a credential x2rock cannot get;
//! *replaying* something it has already seen needs none at all, because the id
//! is enough and the player resolves the account it already holds.
//!
//! So: start it in the Sonos app once, `x2rock keep` it, and it is on the bar
//! from then on.
//!
//! Stored under `$XDG_STATE_HOME/x2rock/` with the player list and the service
//! catalogue. Regenerable in principle - everything here can be re-kept from the
//! app - but losing it would be a real annoyance, so it is written atomically.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::sonos::proto::MusicObjectId;

/// How many unpinned entries the history keeps. Pinned ones never count.
const RECENT_CAP: usize = 50;

/// The shape of the file. Unlike the service catalogue, a mismatch here is
/// **migrated rather than discarded**: a cache can be refetched in a second and
/// this is the only copy of something a person chose to save. `pinned` defaults
/// to true precisely because everything written before it existed got there by
/// someone running `keep`.
const SCHEMA: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Bookmarks {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub items: Vec<Bookmark>,
}

fn pinned_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    /// What to call it. The track or container name unless one was given.
    pub name: String,
    pub object_id: String,
    pub service_id: String,
    /// The account serial, as it appears in a playback URI's `sn=`.
    pub account: String,
    /// Purely for display: which service, what artist, what cover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_url: Option<String>,
    /// `album`, `station`, `track`, ... as the player described it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Kept on purpose, rather than merely seen. Pinned entries are never
    /// evicted and are what `x2rock bookmarks` shows by default.
    #[serde(default = "pinned_by_default")]
    pub pinned: bool,
    /// Unix seconds, for ordering the history. Absent on anything saved before
    /// the daemon started noticing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<u64>,
}

impl Bookmark {
    /// Build one from an id the player reported, or explain what was missing.
    ///
    /// A container with `objectId: "-1"` is the player saying it has nothing to
    /// name - a radio stream's notional "album" - and keeping that would store a
    /// bookmark that could never be played.
    pub fn from_id(name: &str, id: &MusicObjectId) -> Result<Self> {
        if !id.is_real() {
            bail!("what is playing has no id to remember - a live stream, most likely");
        }
        let service_id = id
            .service_id
            .clone()
            .ok_or_else(|| anyhow!("{name:?} names no service, so it could not be played back"))?;
        let account = id
            .account_serial()
            .ok_or_else(|| {
                anyhow!("{name:?} names no account, so the player could not resolve it")
            })?
            .to_string();
        Ok(Self {
            name: name.to_string(),
            object_id: id.object_id.clone(),
            service_id,
            account,
            service_name: None,
            artist: None,
            art_url: None,
            kind: None,
            pinned: false,
            last_played: None,
        })
    }

    /// The URI a player is handed to play this.
    ///
    /// `flags=65544` is what the player itself puts in the URIs it builds for
    /// service tracks; it is carried across verbatim rather than reasoned about,
    /// because nothing here knows what the bits mean.
    pub fn uri(&self) -> String {
        format!(
            "x-sonosapi-hls-static:{}?sid={}&flags=65544&sn={}",
            self.object_id, self.service_id, self.account
        )
    }

    /// The DIDL-Lite that must travel with the URI.
    ///
    /// Synthesized rather than copied: a service item in the queue carries no
    /// `r:resMD` to reuse. The `cdudn` is the load-bearing part - it names the
    /// account the player resolves the content with, and without it the player
    /// accepts the item and then has nothing to show, for the whole queue rather
    /// than just the new row.
    pub fn didl(&self, cdudn: &str) -> String {
        let esc = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let artist = self
            .artist
            .as_deref()
            .map(|a| format!("<dc:creator>{}</dc:creator>", esc(a)))
            .unwrap_or_default();
        format!(
            concat!(
                r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
                r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" "#,
                r#"xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/" "#,
                r#"xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/">"#,
                r#"<item id="00032020{object}" parentID="-1" restricted="true">"#,
                "<dc:title>{title}</dc:title>{artist}",
                "<upnp:class>object.item.audioItem.musicTrack</upnp:class>",
                r#"<desc id="cdudn" nameSpace="urn:schemas-rinconnetworks-com:metadata-1-0/">"#,
                "{cdudn}</desc></item></DIDL-Lite>"
            ),
            object = esc(&self.object_id),
            title = esc(&self.name),
            artist = artist,
            cdudn = esc(cdudn),
        )
    }
}

fn path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "x2rock")
        .ok_or_else(|| anyhow!("no home directory"))?;
    let dir = dirs
        .state_dir()
        .ok_or_else(|| anyhow!("no XDG state directory on this platform"))?;
    Ok(dir.join("bookmarks.json"))
}

impl Bookmarks {
    /// Load, treating a missing file as empty.
    ///
    /// A *corrupt* file is an error here, unlike the service catalogue: that can
    /// be refetched in a second, and this is the only copy of something a person
    /// deliberately saved. Better to say so than to silently start over.
    pub fn load() -> Result<Self> {
        let path = path()?;
        match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut copy = Self {
            schema: SCHEMA,
            items: self.items.clone(),
        };
        copy.items.sort_by(|a, b| a.object_id.cmp(&b.object_id));
        fs::write(&tmp, serde_json::to_string_pretty(&copy)?)?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }

    /// Pin something, adding it if it is not already known.
    ///
    /// Keeping what the daemon already noticed is the common case: it promotes
    /// that entry rather than adding a second one, and takes the new name if a
    /// better one was given. Returns whether it was already there.
    pub fn keep(&mut self, mut bookmark: Bookmark) -> bool {
        bookmark.pinned = true;
        match self
            .items
            .iter()
            .position(|b| b.object_id == bookmark.object_id)
        {
            Some(i) => {
                // Fill in from what is already stored rather than overwriting
                // with blanks. Keeping the container of a track already kept
                // arrives with no artist, and losing the one already recorded
                // would be a silent downgrade of something the person saved.
                let old = &self.items[i];
                bookmark.last_played = bookmark.last_played.or(old.last_played);
                bookmark.artist = bookmark.artist.or_else(|| old.artist.clone());
                bookmark.art_url = bookmark.art_url.or_else(|| old.art_url.clone());
                bookmark.service_name = bookmark.service_name.or_else(|| old.service_name.clone());
                bookmark.kind = bookmark.kind.or_else(|| old.kind.clone());
                self.items[i] = bookmark;
                true
            }
            None => {
                self.items.push(bookmark);
                false
            }
        }
    }

    /// Record that something played, without pinning it.
    ///
    /// What the daemon calls. Returns whether anything changed, so the common
    /// case - the same track still playing - costs no write at all.
    pub fn note(&mut self, mut bookmark: Bookmark, now: u64) -> bool {
        bookmark.last_played = Some(now);
        if let Some(i) = self
            .items
            .iter()
            .position(|b| b.object_id == bookmark.object_id)
        {
            if self.items[i].last_played == Some(now) {
                return false;
            }
            // A pinned entry keeps its name and its pin; only the timestamp
            // moves. Someone named this deliberately and the daemon must not
            // rename it back.
            self.items[i].last_played = Some(now);
            return true;
        }
        bookmark.pinned = false;
        self.items.push(bookmark);

        // Evict the oldest unpinned beyond the cap. Anything with no timestamp
        // predates the history and is treated as oldest.
        let mut unpinned: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, b)| !b.pinned)
            .map(|(i, _)| i)
            .collect();
        if unpinned.len() > RECENT_CAP {
            unpinned.sort_by_key(|&i| self.items[i].last_played.unwrap_or(0));
            let doomed: std::collections::HashSet<usize> = unpinned[..unpinned.len() - RECENT_CAP]
                .iter()
                .copied()
                .collect();
            let mut i = 0;
            self.items.retain(|_| {
                let keep = !doomed.contains(&i);
                i += 1;
                keep
            });
        }
        true
    }

    /// Newest first, pinned entries always present.
    pub fn listed(&self, include_recent: bool) -> Vec<&Bookmark> {
        let mut out: Vec<&Bookmark> = self
            .items
            .iter()
            .filter(|b| include_recent || b.pinned)
            .collect();
        out.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.last_played.cmp(&a.last_played))
        });
        out
    }

    /// Find one by name: exact match, then unique substring.
    ///
    /// Substring rather than prefix, unlike services: these are titles, and
    /// remembering the middle of one is as likely as remembering its start.
    pub fn find(&self, query: &str) -> Result<&Bookmark> {
        let needle = query.to_lowercase();
        if let Some(exact) = self.items.iter().find(|b| b.name.to_lowercase() == needle) {
            return Ok(exact);
        }
        let matches: Vec<_> = self
            .items
            .iter()
            .filter(|b| b.name.to_lowercase().contains(&needle))
            .collect();
        match matches.as_slice() {
            [only] => Ok(only),
            [] => bail!("nothing kept matches {query:?}. `x2rock bookmarks` lists them."),
            several => {
                let names: Vec<_> = several.iter().map(|b| b.name.as_str()).collect();
                bail!("{query:?} matches {}: {}", several.len(), names.join(", "))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(name: &str, object: &str) -> Bookmark {
        Bookmark::from_id(name, &id(object, Some("284"), Some("sn_3"))).unwrap()
    }

    fn id(object: &str, service: Option<&str>, account: Option<&str>) -> MusicObjectId {
        MusicObjectId {
            object_id: object.into(),
            service_id: service.map(str::to_string),
            account_id: account.map(str::to_string),
        }
    }

    #[test]
    fn a_container_with_no_id_of_its_own_is_refused() {
        // What a player reports while a radio stream plays. Keeping it would
        // store something that could never be played back.
        let err = Bookmark::from_id("Coffeehouse", &id("-1", Some("254"), Some("sn_1")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no id to remember"), "{err}");
    }

    #[test]
    fn an_id_without_a_service_or_account_is_refused_with_the_reason() {
        let err = Bookmark::from_id("X", &id("ALk123", None, Some("sn_3")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("names no service"), "{err}");
        let err = Bookmark::from_id("X", &id("ALk123", Some("284"), None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("names no account"), "{err}");
    }

    #[test]
    fn the_playback_uri_matches_what_the_player_builds_for_itself() {
        // Verbatim from this household's own queue, for a YouTube Music track.
        let b =
            Bookmark::from_id("Bodies", &id("ALkSOiGTPQu2", Some("284"), Some("sn_3"))).unwrap();
        assert_eq!(
            b.uri(),
            "x-sonosapi-hls-static:ALkSOiGTPQu2?sid=284&flags=65544&sn=3"
        );
    }

    #[test]
    fn the_didl_carries_the_cdudn_and_escapes_the_title() {
        let mut b =
            Bookmark::from_id("Rock & <Roll>", &id("ALk1", Some("284"), Some("sn_3"))).unwrap();
        b.artist = Some("A & B".into());
        let didl = b.didl("SA_RINCON72711_X_#Svc72711-0-Token");
        assert!(didl.contains("<dc:title>Rock &amp; &lt;Roll&gt;</dc:title>"));
        assert!(didl.contains("<dc:creator>A &amp; B</dc:creator>"));
        assert!(didl.contains(r#"<desc id="cdudn""#));
        assert!(didl.contains("SA_RINCON72711_X_#Svc72711-0-Token"));
    }

    #[test]
    fn keeping_again_fills_in_rather_than_blanking_what_is_known() {
        // Real case: keep the track, then keep the container of the same
        // object. The container carries no artist, and the artist already
        // recorded must survive it.
        let mut list = Bookmarks::default();
        let mut track = bm("Bodies", "a");
        track.artist = Some("Offset, JID".into());
        track.art_url = Some("http://art".into());
        track.kind = Some("track".into());
        list.keep(track);

        let bare = bm("Bodies", "a");
        assert!(bare.artist.is_none(), "the container has none to give");
        list.keep(bare);

        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].artist.as_deref(), Some("Offset, JID"));
        assert_eq!(list.items[0].art_url.as_deref(), Some("http://art"));
        assert_eq!(list.items[0].kind.as_deref(), Some("track"));
    }

    #[test]
    fn a_new_value_still_wins_over_the_stored_one() {
        let mut list = Bookmarks::default();
        let mut first = bm("Bodies", "a");
        first.artist = Some("wrong".into());
        list.keep(first);
        let mut second = bm("Bodies", "a");
        second.artist = Some("right".into());
        list.keep(second);
        assert_eq!(list.items[0].artist.as_deref(), Some("right"));
    }

    #[test]
    fn the_daemon_noticing_something_does_not_pin_it() {
        let mut list = Bookmarks::default();
        assert!(list.note(bm("Bodies", "a"), 100));
        assert!(!list.items[0].pinned, "seen is not the same as saved");
        assert_eq!(list.items[0].last_played, Some(100));
        // The same track still playing costs no write.
        assert!(!list.note(bm("Bodies", "a"), 100));
        assert!(list.note(bm("Bodies", "a"), 200), "a later play does");
        assert_eq!(list.items.len(), 1);
    }

    #[test]
    fn the_daemon_never_renames_or_unpins_what_someone_kept() {
        let mut list = Bookmarks::default();
        let mut named = bm("Friday mix", "a");
        named.artist = Some("mine".into());
        list.keep(named);
        // The player calls the same object something else; the deliberate name
        // must survive it.
        list.note(bm("ALk-whatever", "a"), 500);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].name, "Friday mix");
        assert!(list.items[0].pinned);
        assert_eq!(list.items[0].last_played, Some(500), "but the time moves");
    }

    #[test]
    fn history_evicts_the_oldest_unpinned_and_never_a_pinned_one() {
        let mut list = Bookmarks::default();
        list.keep(bm("pinned", "keepme"));
        for i in 0..(RECENT_CAP + 5) {
            list.note(bm(&format!("t{i}"), &format!("o{i}")), 1000 + i as u64);
        }
        let unpinned = list.items.iter().filter(|b| !b.pinned).count();
        assert_eq!(unpinned, RECENT_CAP, "capped");
        assert!(
            list.items.iter().any(|b| b.object_id == "keepme"),
            "pin survives"
        );
        // The five oldest went, the newest stayed.
        assert!(!list.items.iter().any(|b| b.object_id == "o0"));
        assert!(list.items.iter().any(|b| b.object_id == "o54"));
    }

    #[test]
    fn listing_puts_pinned_first_then_the_most_recent() {
        let mut list = Bookmarks::default();
        list.note(bm("old", "a"), 100);
        list.note(bm("new", "b"), 300);
        list.keep(bm("saved", "c"));
        assert_eq!(
            list.listed(true)
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>(),
            ["saved", "new", "old"]
        );
        assert_eq!(
            list.listed(false)
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>(),
            ["saved"],
            "unpinned history is hidden unless asked for"
        );
    }

    #[test]
    fn a_file_written_before_pinning_existed_stays_pinned() {
        // User data is migrated, not discarded like the service cache: anything
        // already in this file got there because someone ran `keep`.
        let old = r#"{"items":[{"name":"Bodies","object_id":"a","service_id":"284",
                     "account":"3"}]}"#;
        let list: Bookmarks = serde_json::from_str(old).unwrap();
        assert!(list.items[0].pinned, "absent pinned means kept on purpose");
        assert_eq!(list.schema, 0);
    }

    #[test]
    fn keeping_the_same_object_twice_replaces_rather_than_duplicates() {
        let mut list = Bookmarks::default();
        let first = Bookmark::from_id("Bodies", &id("ALk1", Some("284"), Some("sn_3"))).unwrap();
        let renamed =
            Bookmark::from_id("Bodies (single)", &id("ALk1", Some("284"), Some("sn_3"))).unwrap();
        assert!(!list.keep(first));
        assert!(list.keep(renamed), "same object id");
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].name, "Bodies (single)");
    }

    #[test]
    fn names_resolve_by_substring_but_never_ambiguously() {
        let mut list = Bookmarks::default();
        for (n, o) in [
            ("Bodies", "a"),
            ("Body Language", "b"),
            ("Bodies Live", "c"),
        ] {
            list.keep(Bookmark::from_id(n, &id(o, Some("284"), Some("sn_3"))).unwrap());
        }
        // Exact wins even though it is a substring of another.
        assert_eq!(list.find("bodies").unwrap().object_id, "a");
        assert_eq!(list.find("language").unwrap().object_id, "b");
        assert!(
            list.find("bod")
                .unwrap_err()
                .to_string()
                .contains("matches 3")
        );
        assert!(list.find("nothing").is_err());
    }
}
