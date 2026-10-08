//! Sonos Radio's front page, which its SMAPI does not serve.
//!
//! `getMetadata root` on Sonos Radio (sid 303) answers 200 with nothing in it,
//! so `browse -s "Sonos Radio"` said "root is empty" while the Sonos app shows
//! a page of shelves - Trending Now, Discover Sonos Radio, Sonos Presents, a
//! seasonal one or two, the genres. The app does not get them from SMAPI. The
//! service's manifest names a second endpoint, `{"type": "browse", "uri":
//! "https://sali.sonos.superhi.fi/browse/v1"}`, and a plain GET there, with no
//! credential, answers the page as JSON: `views`, one per shelf (28 on
//! 2026-10-08), each with an `id.objectId` like
//! `/stations/en-US/US/c2Q6VVM6dHJlbmRpbmctbm93` and its stations inline.
//!
//! Only the top level is taken from here. A shelf's `objectId` is an ordinary
//! SMAPI container id - `getMetadata` on it lists the shelf's stations as
//! `program`s (`sonos:2997`, Hit List), and the same holds below "Browse
//! Radio" (genres, News & Talk, Sports, Locations) - so everything past the
//! front page is walked the way every other service is.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::sonos::http;
use crate::sonos::smapi::{Item, Service, TIMEOUT, manifest};

/// Sonos Radio's service id in Sonos's catalogue.
pub const SERVICE_ID: &str = "303";

/// Whether a service is Sonos Radio.
pub fn serves(service_id: &str) -> bool {
    service_id == SERVICE_ID
}

/// The front page's shelves, as containers to open.
pub async fn shelves(service: &Service) -> Result<Vec<Item>> {
    let uri = manifest(service)
        .await?
        .and_then(|m| browse_endpoint(&m))
        .with_context(|| format!("{}'s manifest names no browse endpoint", service.name))?;
    let (status, body) = http::get(&uri, TIMEOUT).await?;
    if status != 200 {
        bail!("{} front page: HTTP {status}", service.name);
    }
    parse_shelves(&body)
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

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    views: Vec<View>,
}

#[derive(Deserialize)]
struct View {
    id: ObjectId,
    content: Content,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectId {
    object_id: String,
}

#[derive(Deserialize)]
struct Content {
    container: Named,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

fn parse_shelves(body: &str) -> Result<Vec<Item>> {
    let page: Page = serde_json::from_str(body).context("parsing Sonos Radio's front page")?;
    Ok(page
        .views
        .into_iter()
        .map(|v| Item {
            id: v.id.object_id,
            title: v.content.container.name,
            item_type: "container".into(),
            summary: None,
            art_url: None,
            container: true,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real page (2026-10-08), cut to two of its 28 shelves.
    const PAGE: &str = include_str!("sonos/testdata/sonos-radio-browse.json");

    #[test]
    fn each_shelf_is_a_container_under_its_own_object_id() {
        let shelves = parse_shelves(PAGE).unwrap();
        assert_eq!(shelves.len(), 2);
        assert_eq!(shelves[0].title, "Trending Now");
        assert_eq!(shelves[0].id, "/stations/en-US/US/c2Q6VVM6dHJlbmRpbmctbm93");
        assert!(shelves.iter().all(|s| s.container));
        assert_eq!(shelves[1].title, "Browse Radio");
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
