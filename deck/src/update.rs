//! Whether a newer release is out. A plain binary does not update itself, so
//! once a launch, unless Settings says not to, the latest release on GitHub is
//! asked for and named if it is newer than this build. Nothing else is sent:
//! no identifier, nothing about the household.

use std::time::Duration;

use anyhow::{Context, Result};

const LATEST: &str = "https://api.github.com/repos/rahga/x2rockdeck/releases/latest";
pub const RELEASES: &str = "github.com/rahga/x2rockdeck/releases";

/// The newest release's version, where it is newer than this build.
pub async fn newer_release() -> Result<Option<String>> {
    let agent = format!("x2rock-deck/{}", env!("CARGO_PKG_VERSION"));
    let (status, body) = x2rock::sonos::http::get_with(LATEST, Duration::from_secs(10), &[("User-Agent", &agent), ("Accept", "application/vnd.github+json")])
        .await
        .context("asking GitHub for the latest release")?;
    if status == 404 {
        return Ok(None);
    }
    anyhow::ensure!(status == 200, "GitHub answered HTTP {status}");
    let json: serde_json::Value = serde_json::from_str(&body)?;
    let tag = json.get("tag_name").and_then(|t| t.as_str()).unwrap_or_default();
    Ok(newer(tag, env!("CARGO_PKG_VERSION")).then(|| tag.trim_start_matches('v').to_owned()))
}

/// Whether `tag` (v1.2.3) is a later version than `current` (1.2.0).
pub fn newer(tag: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.trim_start_matches('v').split(['-', '+']).next()?.split('.').map(|n| n.parse().ok()).collect()
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
        assert!(!newer("v0.1.0", "0.2.0"));
        assert!(!newer("nightly", "0.1.0"));
        assert!(newer("v1.0.0-rc1", "0.9.0"));
    }
}
