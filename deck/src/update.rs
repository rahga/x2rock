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

const LIST: &str = "https://api.github.com/repos/rahga/x2rock/releases?per_page=30";
pub const RELEASES: &str = "github.com/rahga/x2rock/releases";
/// What the deck's release tags start with.
const TAG: &str = "deck-v";

/// The newest release's version, where it is newer than this build.
pub async fn newer_release() -> Result<Option<String>> {
    let agent = format!("x2rock-deck/{}", env!("CARGO_PKG_VERSION"));
    let (status, body) = x2rock::sonos::http::get_with(
        LIST,
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
    let Some(version) = newest_deck(&json) else {
        return Ok(None);
    };
    Ok(newer(&version, env!("CARGO_PKG_VERSION")).then_some(version))
}

/// The newest published deck release's version (`1.2.3` of `deck-v1.2.3`), from
/// GitHub's list of releases, newest first. Drafts and pre-releases are not
/// offered, as `releases/latest` never offered them.
fn newest_deck(releases: &serde_json::Value) -> Option<String> {
    releases.as_array()?.iter().find_map(|r| {
        let flag = |name| r.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
        if flag("draft") || flag("prerelease") {
            return None;
        }
        r.get("tag_name")?
            .as_str()?
            .strip_prefix(TAG)
            .map(str::to_owned)
    })
}

/// Whether `tag` (v1.2.3) is a later version than `current` (1.2.0).
pub fn newer(tag: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.trim_start_matches('v')
            .split(['-', '+'])
            .next()?
            .split('.')
            .map(|n| n.parse().ok())
            .collect()
    };
    match (parse(tag), parse(current)) {
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
            newest_deck(&releases).as_deref(),
            Some("0.1.1"),
            "the CLI's v0.3.0, a draft and a pre-release passed over"
        );
        assert!(newer("0.1.1", "0.1.0"));
        assert_eq!(
            newest_deck(&serde_json::json!([{ "tag_name": "v0.2.0" }])),
            None,
            "only CLI releases: nothing to offer"
        );
    }
}
