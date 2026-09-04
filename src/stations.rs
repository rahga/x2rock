//! The internet radio directory, which is deliberately not Sonos's.
//!
//! `search` and `browse` reach the music services the household's player knows
//! about. That list is Sonos's, and it is a ceiling. This module reaches past
//! it: [Radio Browser](https://www.radio-browser.info) is a community catalogue
//! of internet radio stations with no API key, no account and no registration,
//! and every row in it is an ordinary HTTP stream URL - which is exactly what
//! `play-url` proved a speaker will take.
//!
//! **This is not a hole in the axiom.** Calling a cloud service is fine; living
//! off a Sonos cloud login is not - x2rock already calls music services over
//! SMAPI for every search, and this directory asks for no account at all, from
//! Sonos or from anyone. See "The axiom, stated precisely" in
//! docs/architecture.md.
//!
//! The rule that does apply is "talking to a service never enters the daemon":
//! this runs from the CLI, on demand, and the daemon never calls it. Nothing
//! here is cached to disk either, because a directory answer is a query result
//! rather than a fact about the household.
//!
//! Why Radio Browser and not another: it is the only substantial directory that
//! needs no key, and its rows already carry the one field that matters, a
//! *resolved* stream URL.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::sonos::http;

/// The round-robin entry point across the mirrors.
///
/// Radio Browser's own guidance is to resolve `_api._tcp.radio-browser.info`
/// over DNS SRV and pick a mirror at random, which spreads load and survives
/// one going down. Not done here: it needs a DNS resolver this tree does not
/// have, and `all.api` already round-robins. Recorded so the shortcut is a
/// decision rather than an oversight.
const DIRECTORY: &str = "https://all.api.radio-browser.info";

/// Longer than a LAN call and shorter than a person's patience. The directory
/// is in another country and this is invoked from a terminal, not a widget.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Radio Browser asks third-party clients to identify themselves, and it costs
/// nothing to do. `http::get` sends no `User-Agent` of its own.
const AGENT: &str = concat!("x2rock/", env!("CARGO_PKG_VERSION"));

/// One station, as the directory reports it.
///
/// Every field is defaulted: this is someone else's schema, the rows are
/// community-edited, and a missing `country` is not a reason to fail a search.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Station {
    pub name: String,
    /// **The playable URL, and the reason to prefer it over `url`.** A station's
    /// registered `url` is often a `.pls` or `.m3u` *playlist* - SomaFM's is -
    /// which a Sonos player will not take. `url_resolved` is the directory's
    /// dereferenced stream URL, and it is what `play-url` is given.
    pub url_resolved: String,
    /// What the station registered, kept only to notice when the two differ.
    pub url: String,
    pub codec: String,
    pub bitrate: u32,
    pub country: String,
    pub countrycode: String,
    /// Comma-separated, free-form, community-assigned.
    pub tags: String,
    /// The directory's own popularity signal, and the default sort.
    pub votes: i64,
    /// `1` when the stream is HLS rather than a plain Icecast-style stream.
    pub hls: u8,
    pub homepage: String,
}

impl Station {
    /// `MP3 128k`, or just the codec when the bitrate is unknown.
    pub fn format(&self) -> String {
        match (self.codec.as_str(), self.bitrate) {
            ("", 0) => "—".into(),
            (codec, 0) => codec.into(),
            ("", rate) => format!("{rate}k"),
            (codec, rate) => format!("{codec} {rate}k"),
        }
    }
}

/// Search the directory. Every argument narrows; all of them absent asks for
/// the most-voted stations, which is the closest thing it has to a front page.
pub async fn search(
    name: Option<&str>,
    tag: Option<&str>,
    country: Option<&str>,
    limit: u32,
) -> Result<Vec<Station>> {
    // `hidebroken` is the directory's own liveness filter - it last-checked
    // every row and knows which answered. Asking for stations that are known
    // not to play would be a strange default.
    let mut query = format!("limit={limit}&hidebroken=true&order=votes&reverse=true");
    if let Some(name) = name {
        query.push_str(&format!("&name={}", http::urlencode(name)));
    }
    if let Some(tag) = tag {
        query.push_str(&format!("&tag={}", http::urlencode(tag)));
    }
    if let Some(country) = country {
        query.push_str(&format!("&countrycode={}", http::urlencode(country)));
    }

    let url = format!("{DIRECTORY}/json/stations/search?{query}");
    let (status, body) = http::get_with(&url, TIMEOUT, &[("User-Agent", AGENT)])
        .await
        .context("reaching the radio directory")?;
    if status != 200 {
        bail!("the radio directory answered HTTP {status}");
    }
    let mut stations: Vec<Station> =
        serde_json::from_str(&body).context("parsing the radio directory's reply")?;

    // A row with no resolved URL is not playable and there is nothing to show
    // for it. Rare, and the directory does not filter them itself.
    stations.retain(|s| !s.url_resolved.is_empty());
    Ok(stations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_format_reads_as_a_format_however_little_is_known() {
        let station = |codec: &str, bitrate| Station {
            codec: codec.into(),
            bitrate,
            ..Station::default()
        };
        assert_eq!(station("MP3", 128).format(), "MP3 128k");
        assert_eq!(station("AAC+", 0).format(), "AAC+");
        assert_eq!(station("", 96).format(), "96k");
        assert_eq!(station("", 0).format(), "—");
    }

    #[test]
    fn someone_elses_schema_is_read_defensively() {
        // A row carrying only a name and a URL must still parse: the fields are
        // community-edited and half of them are routinely absent.
        let one: Vec<Station> =
            serde_json::from_str(r#"[{"name":"X","url_resolved":"http://x/s"}]"#).unwrap();
        assert_eq!(one[0].name, "X");
        assert_eq!(one[0].bitrate, 0);
        assert_eq!(one[0].format(), "—");

        // And an unknown field arriving later must not break the parse.
        let two: Vec<Station> = serde_json::from_str(
            r#"[{"name":"Y","url_resolved":"http://y/s","something_new":42}]"#,
        )
        .unwrap();
        assert_eq!(two[0].name, "Y");
    }
}
