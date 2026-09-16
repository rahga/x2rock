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

use std::path::Path;
use std::process::Command;

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

    // Without these the stamp is cached with the first build, and every later
    // one reports the commit it was first built at. `index` is what makes
    // `--dirty` honest: it moves when the working tree does.
    for path in [".git/HEAD", ".git/index"] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
