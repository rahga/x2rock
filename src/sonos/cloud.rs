//! Browse and search through **Sonos's cloud content API**, for the pages the
//! direct SMAPI path cannot serve.
//!
//! The modern Sonos app reaches every service's front page not from the
//! service's own SMAPI endpoint but from `api.ws.sonos.com/content/api`, which
//! brokers the request server-side. That is how it shows Amazon's Home, Apple's
//! shelves and YouTube Music's library and search when a third party talking
//! SMAPI directly is refused (Amazon 401, Apple 500, YouTube Music's 403 at
//! Google's project gate).
//!
//! **Opt-in, bring-your-own-registration, no shipped credentials.** This rides
//! the same account OAuth token as [`crate::sonos::login`] - the person's *own*
//! Sonos developer integration, read from `sonos-integration.json`, under their
//! own developer agreement. [`available`] is false, and every call here is
//! skipped, unless they have set that up and signed in. x2rock ships no Sonos
//! key or secret, and never will (see docs/reversing-the-sonos-app.md).
//!
//! **The id arithmetic that makes it fit.** The content API names a service by
//! `serviceId = smapiId * 256 + schemaRev` - which is exactly the
//! `service_type` x2rock already derives from `AvailableServiceTypeList` - and
//! an account by its bare `sn_N` serial. So no id is harvested: a service and
//! the account's serial, both already in hand, address the cloud directly.
//!
//! A container's browse URL is carried whole in the row's `href`, so a cloud
//! row to descend is handed back with its id set to `cloud:<url>`; [`is_cloud`]
//! spots it and [`browse`] follows it. A leaf keeps its real object id, which
//! the player plays through the household registration the same as any other -
//! the player fetches the content with the household's token, so playback works
//! even where x2rock's own SMAPI call would be refused.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::sonos::smapi::Item;
use crate::sonos::{http, login};

const BASE: &str = "https://api.ws.sonos.com/content/api/v3";
const TIMEOUT: Duration = Duration::from_secs(15);

/// `cloud:` marks an id whose rest is a ready-made content-API browse URL.
const PREFIX: &str = "cloud:";

/// Whether an id is a cloud browse URL this module should follow.
pub fn is_cloud(id: &str) -> bool {
    id.starts_with(PREFIX)
}

/// Whether cloud browse/search is set up and usable: the person has their own
/// Sonos integration and has signed in. Nothing here runs otherwise.
pub async fn available() -> bool {
    login::bearer().await.is_some()
}

async fn get(url: &str) -> Result<String> {
    let Some(token) = login::bearer().await else {
        bail!(
            "not signed in to a Sonos account - cloud browse needs `x2rock login` \
             with your own Sonos integration"
        )
    };
    let auth = format!("Bearer {token}");
    let (status, body) = http::get_with(
        url,
        TIMEOUT,
        &[("Authorization", &auth), ("Accept", "application/json")],
    )
    .await?;
    if status != 200 {
        bail!("Sonos content API: HTTP {status}");
    }
    Ok(body)
}

/// The service's front page (its `root` container), as the shelves the app
/// shows - each a container to descend with [`browse`].
pub async fn home(household: &str, service_type: u32, serial: u32) -> Result<Vec<Item>> {
    let url = format!(
        "{BASE}/households/{household}/services/{service_type}/accounts/{serial}/containers/root/browse"
    );
    let page: Value = serde_json::from_str(&get(&url).await?)?;
    Ok(home_rows(&page))
}

/// A home page is `sections.items[]`. A **titled** section is one shelf, a
/// container descended by its own `href` (Amazon's "Songs for You", Apple's
/// "Daily Top 100"). An **untitled** section is a bare wrapper whose inner
/// `items` are themselves the entries (YouTube Music's Home / Library / My
/// Supermix), so those are emitted directly.
fn home_rows(page: &Value) -> Vec<Item> {
    let Some(sections) = page
        .get("sections")
        .and_then(|s| s.get("items"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for section in sections {
        let titled = section
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty());
        if titled {
            if let Some(item) = shelf_item(section) {
                rows.push(item);
            }
        } else if let Some(items) = section.get("items").and_then(Value::as_array) {
            rows.extend(items.iter().filter_map(browse_row));
        }
    }
    rows
}

/// Follow a `cloud:<url>` id one level down.
pub async fn browse(cloud_id: &str) -> Result<Vec<Item>> {
    let url = cloud_id.strip_prefix(PREFIX).unwrap_or(cloud_id);
    let page: Value = serde_json::from_str(&get(url).await?)?;
    Ok(rows_of(&page))
}

/// Browse a bare object id through the cloud - the fallback for descending a
/// cloud search result (an album, an artist) whose own SMAPI browse the direct
/// path refuses. `containers/{id}/browse` serves any object kind: measured
/// against a YouTube Music album, where the typed `albums/{id}` path returned
/// nothing (2026-10-13).
pub async fn browse_object(
    household: &str,
    service_type: u32,
    serial: u32,
    object_id: &str,
) -> Result<Vec<Item>> {
    let url = format!(
        "{BASE}/households/{household}/services/{service_type}/accounts/{serial}/containers/{}/browse",
        http::urlencode(object_id)
    );
    let page: Value = serde_json::from_str(&get(&url).await?)?;
    Ok(rows_of(&page))
}

/// Search one service through the cloud. The content API searches every
/// configured service at once; this keeps the block whose `serviceId` is this
/// service's `service_type`, which is the one the caller asked for.
pub async fn search(household: &str, service_type: u32, query: &str) -> Result<(Vec<Item>, u32)> {
    let url = format!(
        "{BASE}/households/{household}/search?query={}",
        http::urlencode(query)
    );
    let page: Value = serde_json::from_str(&get(&url).await?)?;
    let want = service_type.to_string();
    let Some(block) = page
        .get("services")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b.get("serviceId").and_then(Value::as_str) == Some(&want))
        })
    else {
        return Ok((Vec::new(), 0));
    };
    let total = block
        .get("info")
        .and_then(|i| i.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let items = block
        .get("resources")
        .and_then(Value::as_array)
        .map(|rs| rs.iter().filter_map(search_item).collect())
        .unwrap_or_default();
    Ok((items, total))
}

/// Rows out of a browsed container: either a `resources[]` list, or a
/// `sections.items[]` home-style page (whose shelves descend further), or a
/// section's own inline `items[]`.
fn rows_of(page: &Value) -> Vec<Item> {
    if let Some(rs) = page.get("resources").and_then(Value::as_array) {
        return rs.iter().filter_map(browse_row).collect();
    }
    // `TEMPLATE_LIST_PAGE`: a single `section` holding the rows.
    if let Some(items) = page
        .get("section")
        .and_then(|s| s.get("items"))
        .and_then(Value::as_array)
    {
        return items.iter().filter_map(browse_row).collect();
    }
    // A page that itself paints shelves (`sections.items[]`): keep them as
    // containers to descend further.
    if let Some(items) = page
        .get("sections")
        .and_then(|s| s.get("items"))
        .and_then(Value::as_array)
    {
        return items.iter().filter_map(shelf_item).collect();
    }
    if let Some(items) = page.get("items").and_then(Value::as_array) {
        return items.iter().filter_map(browse_row).collect();
    }
    Vec::new()
}

/// A home-page shelf → a container row whose id is its browse URL.
fn shelf_item(node: &Value) -> Option<Item> {
    let title = node.get("title").and_then(Value::as_str)?.to_string();
    let href = node.get("href").and_then(Value::as_str)?;
    Some(Item {
        id: format!("{PREFIX}{href}"),
        title,
        item_type: "container".into(),
        summary: None,
        art_url: first_image(node),
        container: true,
    })
}

/// A row inside a browsed container (home item or `resources` entry carrying an
/// `href`/srn id and a type).
fn browse_row(node: &Value) -> Option<Item> {
    let title = node
        .get("title")
        .or_else(|| node.get("name"))
        .and_then(Value::as_str)?
        .to_string();
    let srn = node.get("id").and_then(Value::as_str);
    let kind = srn.and_then(srn_kind);
    let container = kind.map(is_container_kind).unwrap_or(false)
        || node.get("type").and_then(Value::as_str) == Some("CONTAINER");
    let href = node.get("href").and_then(Value::as_str);
    // A container descends by its href; a leaf keeps its object id so the
    // player can play it through the household registration.
    let id = if container {
        match href {
            Some(h) => format!("{PREFIX}{h}"),
            None => srn.and_then(srn_object_id)?,
        }
    } else {
        srn.and_then(srn_object_id)?
    };
    Some(Item {
        id,
        title,
        item_type: kind.unwrap_or("container").to_string(),
        summary: node
            .get("subtitle")
            .and_then(Value::as_str)
            .map(str::to_string),
        art_url: first_image(node),
        container,
    })
}

/// A search result (`{id:{objectId,..}, name, type, playable}`).
fn search_item(node: &Value) -> Option<Item> {
    let title = node.get("name").and_then(Value::as_str)?.to_string();
    let object_id = node
        .get("id")
        .and_then(|i| i.get("objectId"))
        .and_then(Value::as_str)?
        .to_string();
    let typ = node
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let playable = node
        .get("playable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // A container (artist, album, playlist) is browsed further; the content API
    // does not hand a search result its own href, so it is addressed by object
    // id the same as a leaf, and the browse path rebuilds the URL when needed.
    let container =
        matches!(typ.as_str(), "artist" | "album" | "playlist" | "container") && !playable;
    Some(Item {
        id: object_id,
        title,
        item_type: if typ.is_empty() { "track".into() } else { typ },
        summary: node
            .get("bio")
            .and_then(|b| b.get("content"))
            .and_then(Value::as_str)
            .map(str::to_string),
        art_url: first_image(node),
        container,
    })
}

/// The first cover URL a node offers, across the shapes the API uses.
fn first_image(node: &Value) -> Option<String> {
    let images = node.get("images")?;
    // search: `images: [{url}]`; browse/home: `images: {tile1x1: url, ...}`.
    if let Some(arr) = images.as_array() {
        return arr
            .first()
            .and_then(|i| i.get("url"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if let Some(obj) = images.as_object() {
        return obj.values().find_map(Value::as_str).map(str::to_string);
    }
    None
}

/// The kind segment of an `srn:content:audio:<kind>:<objectId>#sid:acct`.
fn srn_kind(srn: &str) -> Option<&str> {
    let head = srn.split('#').next().unwrap_or(srn);
    head.strip_prefix("srn:content:audio:")?.split(':').next()
}

/// The object id out of an srn: the part after the kind and before the `#`,
/// percent-decoded (the API encodes its `:` separators as `%3A`).
fn srn_object_id(srn: &str) -> Option<String> {
    let head = srn.split('#').next().unwrap_or(srn);
    let rest = head.strip_prefix("srn:content:audio:")?;
    let encoded = rest.split_once(':').map(|(_, o)| o)?;
    Some(percent_decode(encoded))
}

fn is_container_kind(kind: &str) -> bool {
    matches!(
        kind,
        "container" | "playlist" | "album" | "artist" | "program"
    )
}

/// Minimal percent-decode for the srn object ids (`%3A` and friends).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_srn_splits_into_kind_and_decoded_object_id() {
        let srn = "srn:content:audio:container:library%3Aplaylists%3Afollowed#51463:30";
        assert_eq!(srn_kind(srn), Some("container"));
        assert_eq!(
            srn_object_id(srn).as_deref(),
            Some("library:playlists:followed")
        );
        assert!(is_container_kind("playlist"));
        assert!(!is_container_kind("track"));
    }

    #[test]
    fn a_home_shelf_becomes_a_cloud_container_row() {
        let node = serde_json::json!({
            "title": "Followed Playlists",
            "href": "https://api.ws.sonos.com/x/browse",
            "images": {"tile1x1": "https://img/x.jpg"}
        });
        let item = shelf_item(&node).unwrap();
        assert!(item.container);
        assert!(is_cloud(&item.id));
        assert_eq!(item.id, "cloud:https://api.ws.sonos.com/x/browse");
        assert_eq!(item.art_url.as_deref(), Some("https://img/x.jpg"));
    }

    #[test]
    fn a_search_leaf_keeps_its_object_id_for_playback() {
        let node = serde_json::json!({
            "id": {"objectId": "catalog:track:B00XYZ", "serviceId": "51463"},
            "name": "Some Song", "type": "TRACK", "playable": true,
            "images": [{"url": "https://img/a.jpg"}]
        });
        let item = search_item(&node).unwrap();
        assert!(!item.container);
        assert_eq!(item.id, "catalog:track:B00XYZ");
        assert_eq!(item.item_type, "track");
    }

    #[test]
    fn a_search_artist_is_a_container() {
        let node = serde_json::json!({
            "id": {"objectId": "static/artist/1"}, "name": "Kid Koala",
            "type": "ARTIST", "playable": false
        });
        assert!(search_item(&node).unwrap().container);
    }
}
