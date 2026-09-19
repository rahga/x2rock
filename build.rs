//! Stamp the build with the commit it came from.
//!
//! `version` in Cargo.toml has read 0.1.0 since the rewrite and will go on
//! reading it: nothing here is released, so the crate version cannot answer
//! the question people actually ask of a binary in `~/.local/bin` - which
//! build is this? The commit can, and three places want to say it: `--version`,
//! the daemon's first log line, and the header of the unit `service install`
//! writes. So it is resolved once here and handed to the compiler as
//! `X2ROCK_VERSION`.
//!
//! Falls back to the bare crate version when git cannot answer - a build from
//! a published tarball, or on a machine with no git - because a version string
//! is not worth failing a build over.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Ask git for a path under the git directory (e.g. `HEAD`, `index`, `packed-refs`,
/// or a branch ref). Works across regular checkouts, worktrees, and submodules.
fn git_path(what: &str) -> Option<PathBuf> {
    if let Some(path) = Command::new("git")
        .args(["rev-parse", "--git-path", what])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| PathBuf::from(text.trim()))
        .filter(|p| p.exists())
    {
        return Some(path);
    }
    let direct = Path::new(".git").join(what);
    if direct.exists() {
        return Some(direct);
    }
    None
}

fn main() {
    let package = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let described = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());

    match described {
        Some(commit) => println!("cargo:rustc-env=X2ROCK_VERSION={package} ({commit})"),
        None => println!("cargo:rustc-env=X2ROCK_VERSION={package}"),
    }

    // In a worktree or submodule, `.git` is a file pointing to the actual git
    // directory. Watching the file itself catches relocation or re-pointing.
    if Path::new(".git").is_file() {
        println!("cargo:rerun-if-changed=.git");
    }

    // Without these the stamp is cached with the first build, and every later
    // one reports the commit it was first built at. `HEAD` and the ref it
    // points to catch commits and branch switches. `index` catches `git add`
    // and `git stash`, so the `-dirty` marker tracks *staged* changes; a bare
    // edit to a tracked file touches none of these and does not re-run this
    // script, so for unstaged work the marker is best-effort. Cargo has no way
    // to watch "any tracked file changed" short of listing every one of them.
    for name in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git_path(name) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    // If HEAD is a symbolic ref ("ref: refs/heads/..."), watch the ref file
    // too: HEAD does not change mtime when a commit lands on the current branch.
    if let Some(head_path) = git_path("HEAD")
        && let Ok(head_content) = std::fs::read_to_string(&head_path)
        && let Some(ref_name) = head_content.trim().strip_prefix("ref: ")
        && let Some(ref_path) = git_path(ref_name.trim())
    {
        println!("cargo:rerun-if-changed={}", ref_path.display());
    }
}
