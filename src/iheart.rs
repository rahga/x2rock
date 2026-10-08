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
//! The Sonos app offers iHeart as **Stations** and **Podcasts** only, and its
//! Stations for "coldplay" are exactly the `artists` list - so it must merge
//! the live stations with the artist stations. This does the same: two
//! categories, with Stations searching `stations` then `artists` as one list
//! ([`STATIONS`]), so nothing is ever presented as an artist or a song that
//! is a station.

use std::sync::LazyLock;

use crate::sonos::smapi::Category;

/// iHeartRadio's service id in Sonos's catalogue.
pub const SERVICE_ID: &str = "6";

/// Whether a service is iHeartRadio.
pub fn serves(service_id: &str) -> bool {
    service_id == SERVICE_ID
}

/// The id - and, for iHeart, the mapped id - of the merged Stations category.
pub const STATIONS_ID: &str = "stations";

/// What Stations searches, in the order its rows appear: live radio first,
/// then the artist stations iHeart files under `artists`.
pub const STATIONS: [&str; 2] = ["stations", "artists"];

/// What can be searched: what the Sonos app offers, and no more.
pub fn categories() -> &'static [Category] {
    static CATEGORIES: LazyLock<Vec<Category>> = LazyLock::new(|| {
        [STATIONS_ID, "podcasts"]
            .into_iter()
            .map(|id| Category {
                id: id.into(),
                mapped_id: id.into(),
            })
            .collect()
    });
    &CATEGORIES
}
