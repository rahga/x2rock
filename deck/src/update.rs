//! Whether a newer release is out. A plain binary does not update itself, so
//! once a launch, unless Settings says not to, the latest release on GitHub is
//! asked for and named if it is newer than this build. Nothing else is sent:
//! no identifier, nothing about the household.
//!
//! The deck shares its repository with the x2rock CLI, whose releases are
//! `v*` tags; the deck's are `deck-v*`. GitHub's "latest release" is the CLI's,
//! so the list is read and the newest deck release taken from it.

use std::time::Duration;

use anyhow::{Context, Result};

const LIST: &str = "https://api.github.com/repos/rahga/x2rock/releases";
/// GitHub's most per page; the list is read page by page to its end, so a
/// deck release is found however many CLI releases come before it.
const PER_PAGE: usize = 100;
/// More pages than that is not a release list this code knows.
const PAGES: usize = 10;
pub const RELEASES: &str = "github.com/rahga/x2rock/releases";
/// What the deck's release tags start with.
const TAG: &str = "deck-v";

/// The newest release's version, where it is newer than this build.
pub async fn newer_release() -> Result<Option<String>> {
    let agent = format!("x2rock-deck/{}", env!("CARGO_PKG_VERSION"));
    let mut releases = Vec::new();
    for page in 1..=PAGES {
        let url = format!("{LIST}?per_page={PER_PAGE}&page={page}");
        let (status, body) = x2rock::sonos::http::get_with(
            &url,
            Duration::from_secs(10),
            &[
                ("User-Agent", &agent),
                ("Accept", "application/vnd.github+json"),
            ],
        )
        .await
        .context("asking GitHub for its releases")?;
        if status == 404 {
            return Ok(None);
        }
        anyhow::ensure!(status == 200, "GitHub answered HTTP {status}");
        let json: serde_json::Value = serde_json::from_str(&body)?;
        let Some(listed) = json.as_array() else {
            break;
        };
        let last_page = listed.len() < PER_PAGE;
        releases.extend(listed.iter().cloned());
        if last_page {
            break;
        }
    }
    let Some(version) = newest_deck(&releases) else {
        return Ok(None);
    };
    Ok(newer(&version, env!("CARGO_PKG_VERSION")).then_some(version))
}

fn parse_version(v: &str) -> Option<Vec<u64>> {
    v.trim_start_matches('v')
        .split(['-', '+'])
        .next()?
        .split('.')
        .map(|n| n.parse().ok())
        .collect()
}

/// The newest published deck release's version (`1.2.3` of `deck-v1.2.3`), from
/// GitHub's list of releases. The release with the highest parsed version is taken
/// so that maintenance releases for older lines (e.g. 0.1.1 published after 0.2.0)
/// do not mask newer lines. Drafts and pre-releases are not offered, as
/// `releases/latest` never offered them.
fn newest_deck(releases: &[serde_json::Value]) -> Option<String> {
    releases
        .iter()
        .filter_map(|r| {
            let flag = |name| r.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
            if flag("draft") || flag("prerelease") {
                return None;
            }
            let version = r.get("tag_name")?.as_str()?.strip_prefix(TAG)?;
            let parsed = parse_version(version)?;
            Some((parsed, version.to_owned()))
        })
        .max_by(|(p1, _), (p2, _)| p1.cmp(p2))
        .map(|(_, version)| version)
}

/// Whether `tag` (v1.2.3) is a later version than `current` (1.2.0).
pub fn newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers() {
        assert!(newer("v0.2.0", "0.1.9"));
        assert!(newer("v0.10.0", "0.9.0"));
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(newer("0.1.0", "0.0.9"));
        assert!(!newer("v0.1.0", "0.2.0"));
        assert!(!newer("nightly", "0.1.0"));
        assert!(newer("v1.0.0-rc1", "0.9.0"));
    }

    #[test]
    fn only_the_decks_own_releases_are_offered_never_the_clis() {
        let releases = serde_json::json!([
            { "tag_name": "v0.3.0", "draft": false, "prerelease": false },
            { "tag_name": "deck-v0.2.0", "draft": true, "prerelease": false },
            { "tag_name": "deck-v0.1.2", "draft": false, "prerelease": true },
            { "tag_name": "deck-v0.1.1", "draft": false, "prerelease": false },
            { "tag_name": "deck-v0.1.0", "draft": false, "prerelease": false },
        ]);
        assert_eq!(
            newest_deck(releases.as_array().unwrap()).as_deref(),
            Some("0.1.1"),
            "the CLI's v0.3.0, a draft and a pre-release passed over"
        );
        assert!(newer("0.1.1", "0.1.0"));
        assert_eq!(
            newest_deck(&[serde_json::json!({ "tag_name": "v0.2.0" })]),
            None,
            "only CLI releases: nothing to offer"
        );
    }

    #[test]
    fn older_line_created_later_does_not_mask_newer_line() {
        // GitHub lists releases in reverse chronological creation order.
        // If 0.1.1 is published as a maintenance release after 0.2.0,
        // it appears first in the list, but 0.2.0 should still be selected as newest.
        let releases = serde_json::json!([
            { "tag_name": "deck-v0.1.1", "draft": false, "prerelease": false },
            { "tag_name": "deck-v0.2.0", "draft": false, "prerelease": false },
        ]);
        assert_eq!(
            newest_deck(releases.as_array().unwrap()).as_deref(),
            Some("0.2.0")
        );
    }

    #[test]
    fn deck_release_past_first_page_of_cli_releases_is_found() {
        // However many CLI releases come before the deck's, it is found.
        let mut list = Vec::new();
        for i in 1..=35 {
            list.push(serde_json::json!({
                "tag_name": format!("v0.{i}.0"),
                "draft": false,
                "prerelease": false
            }));
        }
        list.push(serde_json::json!({
            "tag_name": "deck-v0.1.0",
            "draft": false,
            "prerelease": false
        }));
        assert_eq!(newest_deck(&list).as_deref(), Some("0.1.0"));
    }
}
