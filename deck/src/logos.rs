//! Each service's logo as Sonos lists it (support.sonos.com/en-us/services, by
//! the TV app's tools/service-logos.py), found by the name a player gives the
//! service (ServiceLogo.kt).

use std::collections::HashMap;
use std::sync::OnceLock;

static TABLE: &str = include_str!("../ui/assets/service-logos.json");

/// Shorter listed names than this are too likely to begin someone else's.
const MIN_PREFIX: usize = 5;

fn letters(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn by_letters() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        let raw: HashMap<String, String> = serde_json::from_str(TABLE).unwrap_or_default();
        raw.into_iter()
            .map(|(name, url)| (letters(&name), url))
            .collect()
    })
}

/// The logo for the service a player calls `name`, sized for a 56dp slot at
/// 2x, or none. Names are compared as letters and digits only ("Tidal" against
/// "TIDAL"); failing an exact match, the longest listed name the player's
/// begins with wins ("TuneIn (New)" finds "TuneIn").
pub fn logo_url(name: &str) -> Option<String> {
    let wanted = letters(name);
    let map = by_letters();
    let url = map.get(&wanted).or_else(|| {
        map.iter()
            .filter(|(listed, _)| listed.len() >= MIN_PREFIX && wanted.starts_with(listed.as_str()))
            .max_by_key(|(listed, _)| listed.len())
            .map(|(_, url)| url)
    })?;
    // `fm=png` because a few are SVG, which the art cache does not keep.
    Some(format!("{url}?w=112&fm=png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_on_letters_and_digits_then_on_the_longest_prefix() {
        assert!(logo_url("Apple Music").is_some());
        assert_eq!(logo_url("apple music"), logo_url("Apple Music"));
        assert!(logo_url("Amazon Music").unwrap().ends_with("?w=112&fm=png"));
        assert_eq!(logo_url("Calm Radio Plus"), logo_url("Calm Radio"));
        assert_eq!(logo_url("Nothing Like Any Service"), None);
    }
}
