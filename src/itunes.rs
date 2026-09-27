//! Apple Music's catalogue, searched through Apple's public iTunes Search API.
//!
//! Apple Music cannot be searched the way every other service is. Its SMAPI
//! endpoint refuses the only credential a household holds for it - the speaker's
//! record carries an empty private key, and `search` and `browse` both answer
//! `InvalidTokenException` - and its browser link flow refuses to start. See
//! "Apple Music: playback yes, search no" in docs/architecture.md.
//!
//! **The catalogue is public, though, and its ids are the player's.** The iTunes
//! Search API needs no key and no account, and its `trackId` and `collectionId`
//! are the numbers behind the `song:` and `album:` ids the player enqueues:
//! `song:1299241646`, taken straight from a search and never played in the
//! household, played at once (verified 2026-09-26, home). So this module stands
//! in for SMAPI's `search` and nothing else. Playback is unchanged: the id goes
//! into the queue and the household's own Apple Music registration plays it.
//!
//! What it cannot reach: the person's library, and Apple's editorial `pl.…`
//! playlists. Neither is in the public API, and the Apple Music API that has
//! them wants a paid developer token.
//!
//! Like `stations`, this runs from the CLI on demand and never from the daemon,
//! and caches nothing: an answer is a query result, not a fact about the
//! household.

use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::sonos::http;
use crate::sonos::smapi::{Category, Item, TIMEOUT};

/// Apple Music's service id in Sonos's catalogue.
pub const SERVICE_ID: &str = "204";

/// Whether a service is searched here rather than over SMAPI.
pub fn serves(service_id: &str) -> bool {
    service_id == SERVICE_ID
}

/// The most rows the API returns for one request.
const MAX_LIMIT: u32 = 200;

/// What can be searched, in the same form a presentation map gives.
///
/// The mapped ids are the ones Apple Music's own map sends - `song` and `album` -
/// so a category list cached from the map before this module existed names the
/// same thing. Artists are left out: an `artist:` id is a container of albums,
/// and opening one takes the SMAPI `getMetadata` that Apple refuses.
pub fn categories() -> &'static [Category] {
    static CATEGORIES: LazyLock<Vec<Category>> = LazyLock::new(|| {
        [("tracks", "song"), ("albums", "album")]
            .into_iter()
            .map(|(id, mapped)| Category {
                id: id.into(),
                mapped_id: mapped.into(),
            })
            .collect()
    });
    &CATEGORIES
}

/// Search the catalogue in one category, SMAPI-shaped: the page of items from
/// `index`, and a total.
///
/// **The API ignores `offset`** (checked 2026-09-26: offsets 0, 1, 2 and 5 all
/// return the same first rows), so a page is sliced from one larger answer: ask
/// for `index + count + 1` rows and keep `count` of them. The extra row is what
/// makes `total` honest. The API reports no total of its own, so `total` is how
/// many rows came back, and it exceeds `index + count` exactly when there is
/// more to page to - the one thing the JSON envelope promises a caller.
pub async fn search(
    category: &str,
    term: &str,
    index: u32,
    count: u32,
) -> Result<(Vec<Item>, u32)> {
    let entity = match category {
        "song" => "song",
        "album" => "album",
        other => bail!(
            "Apple Music is searched through Apple's public catalogue, which has tracks \
             and albums only - not {other:?}"
        ),
    };
    let limit = index.saturating_add(count).saturating_add(1).min(MAX_LIMIT);
    let url = format!(
        "https://itunes.apple.com/search?media=music&entity={entity}&limit={limit}&country={}&term={}",
        storefront(),
        http::urlencode(term)
    );
    let (status, body) = http::get(&url, TIMEOUT)
        .await
        .context("reaching Apple's catalogue")?;
    if status != 200 {
        bail!("Apple's catalogue answered HTTP {status}");
    }
    let items = parse(&body)?;
    let total = items.len() as u32;
    let page = items
        .into_iter()
        .skip(index as usize)
        .take(count as usize)
        .collect();
    Ok((page, total))
}

/// The storefront to search: the region of the locale, else the US.
///
/// An id can exist in one country's catalogue and be unavailable in another's,
/// so the right storefront is the household's. Nothing here knows that, and the
/// locale of the machine asking is the nearest thing to hand. `C` and `POSIX`
/// name no region, which is what the fallback is for.
fn storefront() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|v| !v.is_empty())
        .and_then(|v| region(&v))
        .unwrap_or_else(|| "US".into())
}

/// `en_GB.UTF-8` -> `GB`.
fn region(locale: &str) -> Option<String> {
    let region = locale.split(['.', '@']).next()?.split_once('_')?.1;
    (region.len() == 2 && region.chars().all(|c| c.is_ascii_alphabetic()))
        .then(|| region.to_ascii_uppercase())
}

/// One row of an answer. Every field is defaulted: this is someone else's schema,
/// and a missing name is a reason to skip a row rather than fail a search.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Row {
    wrapper_type: String,
    kind: String,
    track_id: Option<u64>,
    track_name: String,
    collection_id: Option<u64>,
    collection_name: String,
    artist_name: String,
    artwork_url100: Option<String>,
    /// Present on tracks. `false` is a track that is sold and not streamed, which
    /// the household's subscription would refuse.
    is_streamable: Option<bool>,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    results: Vec<Row>,
}

/// An answer's rows as items, in the player's id grammar.
fn parse(body: &str) -> Result<Vec<Item>> {
    let answer: Answer = serde_json::from_str(body).context("parsing Apple's catalogue")?;
    Ok(answer.results.into_iter().filter_map(item).collect())
}

fn item(row: Row) -> Option<Item> {
    let summary = Some(row.artist_name).filter(|a| !a.is_empty());
    match row.wrapper_type.as_str() {
        "track" if row.kind == "song" && row.is_streamable != Some(false) => Some(Item {
            id: format!("song:{}", row.track_id?),
            title: row.track_name,
            item_type: "track".into(),
            summary,
            art_url: row.artwork_url100,
            container: false,
        }),
        "collection" => Some(Item {
            id: format!("album:{}", row.collection_id?),
            title: row.collection_name,
            item_type: "album".into(),
            summary,
            art_url: row.artwork_url100,
            container: true,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_and_an_album_take_the_ids_the_player_plays() {
        let body = r#"{"resultCount":2,"results":[
            {"wrapperType":"track","kind":"song","trackId":1299241646,
             "trackName":"August 10","collectionId":1299241642,
             "collectionName":"Con Todo El Mundo","artistName":"Khruangbin",
             "artworkUrl100":"https://example/100x100bb.jpg","isStreamable":true},
            {"wrapperType":"collection","collectionType":"Album",
             "collectionId":1299241642,"collectionName":"Con Todo El Mundo",
             "artistName":"Khruangbin","trackCount":11}
        ]}"#;
        let items = parse(body).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "song:1299241646");
        assert_eq!(items[0].item_type, "track");
        assert_eq!(items[0].title, "August 10");
        assert_eq!(items[0].summary.as_deref(), Some("Khruangbin"));
        assert!(!items[0].container);
        assert_eq!(items[1].id, "album:1299241642");
        assert_eq!(items[1].item_type, "album");
        assert!(items[1].container);
        assert_eq!(items[1].art_url, None);
    }

    #[test]
    fn what_cannot_be_streamed_or_played_is_dropped() {
        let body = r#"{"results":[
            {"wrapperType":"track","kind":"song","trackId":1,"trackName":"Sold only",
             "isStreamable":false},
            {"wrapperType":"track","kind":"music-video","trackId":2,"trackName":"A video"},
            {"wrapperType":"track","kind":"song","trackName":"No id"},
            {"wrapperType":"artist","artistName":"Somebody"},
            {"wrapperType":"track","kind":"song","trackId":3,"trackName":"Kept"}
        ]}"#;
        let ids: Vec<_> = parse(body).unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, ["song:3"]);
    }

    #[test]
    fn a_locale_gives_its_region_and_nothing_else_does() {
        assert_eq!(region("en_GB.UTF-8").as_deref(), Some("GB"));
        assert_eq!(region("de_de@euro").as_deref(), Some("DE"));
        assert_eq!(region("en_US").as_deref(), Some("US"));
        assert_eq!(region("C"), None);
        assert_eq!(region("POSIX"), None);
        assert_eq!(region("C.UTF-8"), None);
    }

    #[test]
    fn the_categories_send_what_apples_own_map_sends() {
        let mapped: Vec<_> = categories()
            .iter()
            .map(|c| (c.id.as_str(), c.mapped_id.as_str()))
            .collect();
        assert_eq!(mapped, [("tracks", "song"), ("albums", "album")]);
    }
}
