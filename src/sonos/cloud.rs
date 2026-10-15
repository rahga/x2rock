//! Browse and search through **Sonos's cloud content API**, for the pages the
//! direct SMAPI path cannot serve.
//!
//! **Inactive under current license requirements** ([`ACTIVE`] is false). The
//! Sonos developer program does not license this service (see
//! docs/sonos-terms-boundaries.md), so every call here fails before touching
//! the network and the callers keep their direct answer. The code is kept, not
//! deleted, so the path can be re-enabled if the licensing changes.
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
//! own developer agreement. Every call here fails before touching the network
//! unless they have set that up and signed in, and the callers treat a failure
//! as "no fallback". x2rock ships no Sonos key or secret, and never will (see
//! docs/sonos-terms-boundaries.md).
//!
//! **The id arithmetic that makes it fit.** The content API names a service by
//! `serviceId = smapiId * 256 + schemaRev` - which is exactly the
//! `service_type` x2rock already derives from `AvailableServiceTypeList` - and
//! an account by its bare `sn_N` serial. No id is harvested: a service and the
//! account's serial, both already in hand, address the cloud directly.
//!
//! A container's browse URL is carried whole in the row's `href`, so a cloud
//! row to descend is handed back with its id set to `cloud:<url>`; [`is_cloud`]
//! spots it and [`browse`] follows it. A leaf keeps its real object id, which
//! the player plays through the household registration the same as any other -
//! the player fetches the content with the household's token, so playback works
//! even where x2rock's own SMAPI call would be refused.
//!
//! The API hands each listing back whole; every function here returns one
//! `index`/`count` page of it with the full total, the shape the SMAPI calls
//! return, so a caller swaps one for the other.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::sonos::smapi::{self, Item};
use crate::sonos::{http, login};

/// Whether the cloud content path may run at all. False while the Sonos terms
/// do not license the content service; flipping it is the whole switch.
pub const ACTIVE: bool = false;

const BASE: &str = "https://api.ws.sonos.com/content/api/v3";
const TIMEOUT: Duration = Duration::from_secs(15);

/// `cloud:` marks an id whose rest is a ready-made content-API browse URL.
const PREFIX: &str = "cloud:";

/// Whether an id is a cloud browse URL this module should follow.
pub fn is_cloud(id: &str) -> bool {
    id.starts_with(PREFIX)
}

async fn get_json(url: &str) -> Result<Value> {
    if !ACTIVE {
        bail!("the Sonos cloud content path is inactive under current license requirements")
    }
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
    serde_json::from_str(&body).context("Sonos content API: unreadable reply")
}

/// The browse URL of one container of one account.
fn container_url(household: &str, service_type: u32, serial: u32, id: &str) -> String {
    format!(
        "{BASE}/households/{household}/services/{service_type}/accounts/{serial}/containers/{}/browse",
        http::urlencode(id)
    )
}

/// The service's front page (its `root` container), as the shelves the app
/// shows - each a container to descend with [`browse`].
pub async fn home(
    household: &str,
    service_type: u32,
    serial: u32,
    index: u32,
    count: u32,
) -> Result<(Vec<Item>, u32)> {
    let page = get_json(&container_url(household, service_type, serial, "root")).await?;
    Ok(smapi::page(home_rows(&page), index, count))
}

/// A home page is `sections.items[]`. A **titled** section is one shelf, a
/// container descended by its own `href` (Amazon's "Songs for You", Apple's
/// "Daily Top 100"). An **untitled** section is a bare wrapper whose inner
/// `items` are themselves the entries (YouTube Music's Home / Library / My
/// Supermix), so those are emitted directly.
fn home_rows(page: &Value) -> Vec<Item> {
    let Some(sections) = page.pointer("/sections/items").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for section in sections {
        let titled = section
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty());
        if titled {
            rows.extend(shelf_item(section));
        } else if let Some(items) = section.get("items").and_then(Value::as_array) {
            rows.extend(items.iter().filter_map(browse_row));
        }
    }
    rows
}

/// Follow a `cloud:<url>` id one level down. Only to the content API: the id
/// can be typed by hand, and the request carries the account's bearer token.
pub async fn browse(cloud_id: &str, index: u32, count: u32) -> Result<(Vec<Item>, u32)> {
    let url = cloud_id.strip_prefix(PREFIX).unwrap_or(cloud_id);
    if !url.starts_with("https://api.ws.sonos.com/") {
        bail!("not a Sonos content API address: {url}");
    }
    Ok(smapi::page(rows_of(&get_json(url).await?), index, count))
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
    index: u32,
    count: u32,
) -> Result<(Vec<Item>, u32)> {
    let page = get_json(&container_url(household, service_type, serial, object_id)).await?;
    Ok(smapi::page(rows_of(&page), index, count))
}

/// Search one service through the cloud. The content API searches every
/// configured service at once; this keeps the block whose `serviceId` is this
/// service's `service_type`, which is the one the caller asked for. The total
/// is the API's own count when that runs past the rows it sent.
pub async fn search(
    household: &str,
    service_type: u32,
    query: &str,
    index: u32,
    count: u32,
) -> Result<(Vec<Item>, u32)> {
    let url = format!(
        "{BASE}/households/{household}/search?query={}",
        http::urlencode(query)
    );
    let page = get_json(&url).await?;
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
    let claimed = block
        .pointer("/info/count")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let items = block
        .get("resources")
        .and_then(Value::as_array)
        .map(|rs| rs.iter().filter_map(search_item).collect())
        .unwrap_or_default();
    let (items, sent) = smapi::page(items, index, count);
    Ok((items, claimed.max(sent)))
}

/// Rows out of a browsed container: a `resources[]` list, a single
/// `section.items[]` (`TEMPLATE_LIST_PAGE`), a `sections.items[]` page that
/// itself paints shelves (kept as containers to descend further), or a bare
/// `items[]`.
fn rows_of(page: &Value) -> Vec<Item> {
    let rows = |at: &str| page.pointer(at).and_then(Value::as_array);
    if let Some(items) = rows("/resources").or_else(|| rows("/section/items")) {
        return items.iter().filter_map(browse_row).collect();
    }
    if let Some(items) = rows("/sections/items") {
        return items.iter().filter_map(shelf_item).collect();
    }
    rows("/items")
        .map(|items| items.iter().filter_map(browse_row).collect())
        .unwrap_or_default()
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
    let container = kind.is_some_and(is_container_kind)
        || node.get("type").and_then(Value::as_str) == Some("CONTAINER");
    // A container descends by its href; a leaf keeps its object id so the
    // player can play it through the household registration.
    let href = node
        .get("href")
        .and_then(Value::as_str)
        .filter(|_| container);
    let id = match href {
        Some(h) => format!("{PREFIX}{h}"),
        None => srn.and_then(srn_object_id)?,
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
        .pointer("/id/objectId")
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
            .pointer("/bio/content")
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

/// `<kind>:<objectId>` out of an `srn:content:audio:<kind>:<objectId>#sid:acct`.
fn srn_body(srn: &str) -> Option<&str> {
    srn.split('#').next()?.strip_prefix("srn:content:audio:")
}

/// The kind segment of an srn.
fn srn_kind(srn: &str) -> Option<&str> {
    srn_body(srn)?.split(':').next()
}

/// The object id out of an srn: the part after the kind and before the `#`,
/// percent-decoded (the API encodes its `:` separators as `%3A`).
fn srn_object_id(srn: &str) -> Option<String> {
    let (_, encoded) = srn_body(srn)?.split_once(':')?;
    Some(http::urldecode(encoded))
}

fn is_container_kind(kind: &str) -> bool {
    matches!(
        kind,
        "container" | "playlist" | "album" | "artist" | "program"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_inactive_path_refuses_before_reaching_the_network() {
        // Pins the switch: while `ACTIVE` is false, no cloud call is made,
        // whatever the caller holds. Flipping it means changing this test too.
        let err = search("Sonos_household", 72711, "anything", 0, 5)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("inactive"), "{err:#}");
        let err = browse("cloud:https://api.ws.sonos.com/x", 0, 5)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("inactive"), "{err:#}");
    }

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
