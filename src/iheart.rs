//! iHeartRadio's search, shaped the way the Sonos app shows it.
//!
//! iHeart's presentation map publishes six search categories - stations,
//! artists, tracks, albums, playlists, podcasts - and searching them as named
//! gives answers that do not mean what the names say. Measured against both
//! households, 2026-10-07 (and by x2rocktv the same day):
//!
//! - **`artists` answers artist *stations*.** "coldplay" gives
//!   `artist_radio.1648` and its kin, `itemType` `program`: a station seeded
//!   by the artist, not the artist.
//! - **`tracks` answers artist stations too.** Its first hit for "coldplay",
//!   "A COLD PLAY", is `artist_radio.32433934`; played, it starts The Kid
//!   LAROI's station rather than the song. The free tier plays no song on
//!   demand, so a row that promises one delivers a radio station.
//! - **`stations` answers only live radio** - "z100" finds Z100 and its
//!   namesakes - and nothing for an artist.
//! - `albums` answered nothing; `playlists` answers iHeart's own custom radio.
//!
//! The Sonos app shows iHeart's results as three sections - app 90, read off
//! a phone on 2026-10-12: **Stations**, the `artists` list ("miles davis":
//! Miles Davis, Miles Davis Quintet, Cannonball Adderley & Miles Davis);
//! **Live stations**, the `stations` list (KOST 103.5, La Mezcla Fuego,
//! ShaqFu Radio); and **Podcasts**. Three rows each.
//!
//! So x2rock offers the same three, under its own names: `stations` (sent as
//! `artists`), `live` (sent as `stations`) and `podcasts`, all asked when no
//! category is named. They are not one list, either order: iHeart matches
//! loosely, so "z100" finds "Z100wave", "Z1K0" and dozens more artist
//! stations before Z100 itself, and "miles davis" finds KOST 103.5 among live
//! radio. The app keeps both kinds visible by showing three of each; asked
//! together, the several-categories search does it by taking one row from each
//! in turn, each labelled with its category. Nothing is ever presented as an
//! artist or a song that is a station.

use std::sync::LazyLock;

use crate::sonos::smapi::Category;

/// iHeartRadio's service id in Sonos's catalogue.
pub const SERVICE_ID: &str = "6";

/// Whether a service is iHeartRadio.
pub fn serves(service_id: &str) -> bool {
    service_id == SERVICE_ID
}

/// What can be searched: what the Sonos app offers, under the app's
/// headings, each sent as the category iHeart actually files it under.
pub fn categories() -> &'static [Category] {
    static CATEGORIES: LazyLock<Vec<Category>> = LazyLock::new(|| {
        [
            ("stations", "artists"),
            ("live", "stations"),
            ("podcasts", "podcasts"),
        ]
        .into_iter()
        .map(|(id, mapped)| Category {
            id: id.into(),
            mapped_id: mapped.into(),
            default: true,
        })
        .collect()
    });
    &CATEGORIES
}
