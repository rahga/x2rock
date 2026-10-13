//! A service's front page, as the Sonos app draws it: from the service's own
//! `browse` endpoint, not from SMAPI's `root`.
//!
//! A manifest may name an endpoint of type `browse`. Where it does, the app's
//! first page comes from there - a JSON `views` list, one per shelf, each with
//! its items inline - and SMAPI `root` is something else, often much less.
//! Read 2026-10-11 against the home household:
//!
//! - **Sonos Radio** (`sali.sonos.superhi.fi/browse/v1`): SMAPI root answers
//!   200 and empty; the page is 28 shelves. Needs no credential. The first one
//!   built, on 2026-10-08.
//! - **Spotify** (`spotify-v5.ws.sonos.com/v1/browse`): SMAPI root is five
//!   generic shelves; the page is 22, nearly all personal - "Recommended for
//!   today", "Your top mixes", "More like <artist>".
//! - **Deezer** (`platform.deezer.com/api/sonos/browse`): six against nine -
//!   Recently played, the Flow moods, Made for you.
//! - **SiriusXM**: the same nine either way.
//! - **Amazon Music, TuneIn (New), Apple Music** name one and refuse it
//!   (401, a Cloudflare 403, a 500) with the token as a bearer. Their root
//!   stays SMAPI's.
//!
//! **A shelf's `objectId` is an ordinary SMAPI container id**, so only the top
//! level comes from here and everything under it is walked with `getMetadata`
//! as for any service. The one exception measured: Deezer's Recently played
//! (`user-recentlyplayed`) is refused by `getMetadata`, and the endpoint takes
//! no parameter that would serve one shelf alone - so a refused id that names
//! a shelf is answered from the items the page carries inline.
//!
//! The credential is the SMAPI token, sent as `Authorization: Bearer`. It must
//! be fresh: Spotify's lapses within hours, and this endpoint answers a stale
//! one with a bare 401 rather than SMAPI's refresh fault. So `root` is read
//! from SMAPI first, which refreshes it, and the page second.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::sonos::http;
use crate::sonos::smapi::{Item, Service, TIMEOUT, manifest};

/// The service's `browse` endpoint, if its manifest names one over https.
///
/// https only: the token goes with the request, and a manifest is a document
/// fetched from elsewhere.
///
/// Remembered per service for the life of the process: a browse of `root`
/// asks, and the refused-shelf fallback asks again moments later, and each
/// was a fetch of the manifest from Sonos's CDN. Only an answer is kept; a
/// failed fetch is asked again next time.
pub async fn endpoint(service: &Service) -> Result<Option<String>> {
    static SEEN: LazyLock<Mutex<HashMap<String, Option<String>>>> = LazyLock::new(Default::default);
    if let Some(known) = SEEN
        .lock()
        .ok()
        .and_then(|seen| seen.get(&service.id).cloned())
    {
        return Ok(known);
    }
    let uri = manifest(service)
        .await?
        .and_then(|m| browse_endpoint(&m))
        .filter(|uri| uri.starts_with("https://"));
    if let Ok(mut seen) = SEEN.lock() {
        seen.insert(service.id.clone(), uri.clone());
    }
    Ok(uri)
}

/// The front page at `uri`, read with `token` as a bearer where there is one.
///
/// **A refused bearer is retried without one.** Sonos Radio's page needs no
/// credential and does not accept one: sent the household's Sonos Radio token
/// it refused, where the same GET bare answers the page (2026-10-11).
pub async fn page(service: &Service, uri: &str, token: Option<&str>) -> Result<Page> {
    let bearer = token.map(|t| format!("Bearer {t}"));
    let headers: Vec<(&str, &str)> = bearer
        .as_deref()
        .map(|b| vec![("Authorization", b)])
        .unwrap_or_default();
    let (mut status, mut body) = http::get_with(uri, TIMEOUT, &headers).await?;
    if bearer.is_some() && matches!(status, 401 | 403) {
        (status, body) = http::get(uri, TIMEOUT).await?;
    }
    if status != 200 {
        bail!("{} front page: HTTP {status}", service.name);
    }
    serde_json::from_str(&body).with_context(|| format!("parsing {}'s front page", service.name))
}

/// The manifest's `endpoints[]` entry of type `browse`.
fn browse_endpoint(manifest: &serde_json::Value) -> Option<String> {
    manifest
        .get("endpoints")?
        .as_array()?
        .iter()
        .find(|e| e.get("type").and_then(|t| t.as_str()) == Some("browse"))?
        .get("uri")?
        .as_str()
        .map(str::to_owned)
}

#[derive(Debug, Deserialize)]
pub struct Page {
    #[serde(default)]
    views: Vec<View>,
}

#[derive(Debug, Deserialize)]
struct View {
    id: ObjectId,
    content: Content,
    #[serde(default)]
    items: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    id: ObjectId,
    content: Content,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectId {
    object_id: String,
}

/// One of `container` or `track`, whichever the entry is.
#[derive(Debug, Deserialize)]
struct Content {
    container: Option<Described>,
    track: Option<Described>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Described {
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    summary: Option<String>,
    artist: Option<Named>,
    image_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Named {
    name: String,
}

impl Page {
    /// The shelves, as containers to open.
    pub fn shelves(&self) -> Vec<Item> {
        self.views
            .iter()
            .filter_map(|v| {
                let c = v.content.container.as_ref()?;
                Some(Item {
                    id: v.id.object_id.clone(),
                    title: c.name.clone(),
                    item_type: "container".into(),
                    summary: None,
                    art_url: c.image_url.clone(),
                    container: true,
                })
            })
            .collect()
    }

    /// What the page carries inline for the shelf `id`, if it has one.
    pub fn shelf(&self, id: &str) -> Option<Vec<Item>> {
        let view = self.views.iter().find(|v| v.id.object_id == id)?;
        Some(view.items.iter().filter_map(entry_item).collect())
    }
}

/// An inline entry as SMAPI would have described it.
///
/// The page's types are not quite SMAPI's: Sonos Radio's and SiriusXM's
/// stations are `trackList.program`, a SiriusXM live channel is `station` -
/// what SMAPI search calls a `stream` - and a track arrives as `track`
/// content rather than a container. Mapped onto the SMAPI words so that the
/// rest of x2rock decides what to do with a row exactly as it would for a
/// `getMetadata` one.
fn entry_item(e: &Entry) -> Option<Item> {
    let (d, leaf) = match (&e.content.container, &e.content.track) {
        (Some(c), _) => (c, false),
        (None, Some(t)) => (t, true),
        (None, None) => return None,
    };
    let (item_type, container) = match d.kind.as_str() {
        "station" => ("stream".to_string(), false),
        k if k.ends_with("program") => ("program".to_string(), true),
        "" if leaf => ("track".to_string(), false),
        k => (k.to_string(), !leaf),
    };
    Some(Item {
        id: e.id.object_id.clone(),
        title: d.name.clone(),
        item_type,
        summary: d
            .artist
            .as_ref()
            .map(|a| a.name.clone())
            .or_else(|| d.summary.clone()),
        art_url: d.image_url.clone(),
        container,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real page (2026-10-08), cut to two of its 28 shelves.
    const SONOS_RADIO: &str = include_str!("sonos/testdata/sonos-radio-browse.json");
    /// Deezer's page (2026-10-11), cut to two shelves and their first items,
    /// ids and names replaced.
    const DEEZER: &str = include_str!("sonos/testdata/deezer-browse.json");

    #[test]
    fn each_shelf_is_a_container_under_its_own_object_id() {
        let page: Page = serde_json::from_str(SONOS_RADIO).unwrap();
        let shelves = page.shelves();
        assert_eq!(shelves.len(), 2);
        assert_eq!(shelves[0].title, "Trending Now");
        assert_eq!(shelves[0].id, "/stations/en-US/US/c2Q6VVM6dHJlbmRpbmctbm93");
        assert!(shelves.iter().all(|s| s.container));
        assert_eq!(shelves[1].title, "Browse Radio");
    }

    #[test]
    fn a_shelf_is_answered_from_what_the_page_carries() {
        let page: Page = serde_json::from_str(DEEZER).unwrap();
        let shelf = page.shelf("user-recentlyplayed").unwrap();
        let got: Vec<_> = shelf
            .iter()
            .map(|i| (i.id.as_str(), i.item_type.as_str(), i.container))
            .collect();
        assert_eq!(
            got,
            [
                ("playlist-1", "playlist", true),
                ("album-2", "album", true),
                ("flow-mood", "program", true),
            ]
        );
        assert_eq!(shelf[1].summary.as_deref(), Some("Some Artist"));
        assert!(page.shelf("not-a-shelf").is_none());
    }

    #[test]
    fn a_station_is_a_stream_and_a_track_is_a_leaf() {
        let page: Page = serde_json::from_str(
            r#"{"views":[{"id":{"objectId":"v"},"content":{"container":{"name":"V","type":"container"}},
               "items":[
                 {"id":{"objectId":"channel-linear:1"},"content":{"container":{"name":"CH 1","type":"station"}}},
                 {"id":{"objectId":"t1"},"content":{"track":{"name":"Song","type":"track"}}}
               ]}]}"#,
        )
        .unwrap();
        let shelf = page.shelf("v").unwrap();
        assert_eq!(
            (shelf[0].item_type.as_str(), shelf[0].container),
            ("stream", false)
        );
        assert_eq!(
            (shelf[1].item_type.as_str(), shelf[1].container),
            ("track", false)
        );
    }

    #[test]
    fn the_browse_endpoint_is_read_from_the_manifest() {
        let manifest = serde_json::json!({
            "presentationMap": {"uri": "https://cf.ws.sonos.com/p/p/x"},
            "endpoints": [{"type": "browse", "uri": "https://sali.sonos.superhi.fi/browse/v1", "version": "0"}]
        });
        assert_eq!(
            browse_endpoint(&manifest).as_deref(),
            Some("https://sali.sonos.superhi.fi/browse/v1")
        );
        assert_eq!(browse_endpoint(&serde_json::json!({})), None);
    }
}
