//! Changing something, by whichever of the two routes can express it.
//!
//! MPRIS carries transport and the play modes, so those go straight down the
//! bus and land instantly. It carries nothing for grouping, party, TV input, a
//! single speaker's volume beneath its group - or a *relative* volume step,
//! which is what a key is (see [`Cli::nudge_volume`]). Those run the CLI as a
//! subprocess, which is what the bar widget does and for the same reason.
//!
//! **Not in-process, though this is the same binary.** Command dispatch in
//! `main.rs` is a run of inline `if let Command::X` blocks that print as they
//! go: 159 `println!` sites against 26 extracted functions. Calling that logic
//! directly means first separating doing from printing across most of a
//! six-thousand-line file. Worth doing one day; letting it gate this would put
//! the expensive half in front of the visible one.

use std::ffi::OsString;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The CLI as the TUI was started: this binary, with whatever `--ip` it was
/// given. Carried rather than reconstructed, because the network that needs
/// `--ip` for the daemon needs it for every child too - and the child that
/// dropped it would fail with the very hint the user had already followed.
#[derive(Clone)]
pub struct Cli {
    ip: Option<IpAddr>,
    /// Resolved once, at startup - see [`Cli::resolve`].
    binary: OsString,
}

impl Cli {
    pub fn new(ip: Option<IpAddr>) -> Self {
        Self {
            ip,
            binary: Self::resolve(),
        }
    }

    /// Which binary to run, decided once at startup.
    ///
    /// **An upgrade under a running TUI used to break every action it has.**
    /// `current_exe` reads `/proc/self/exe`, and the kernel appends
    /// " (deleted)" to that link once the file behind it is replaced - which is
    /// what installing a new build does, `cargo install` and a package upgrade
    /// alike, since each writes a new inode at the same path. Asking for it
    /// afresh at every keypress therefore returned a path that cannot be run,
    /// and grouping, party, TV input and every volume key failed with "No such
    /// file or directory" naming a file that is plainly there.
    ///
    /// Resolving at startup makes that window small, stripping the marker
    /// closes it, and a path that still leads nowhere falls back to the plain
    /// name for `PATH` to answer - a worse answer, since a TUI launched from a
    /// desktop entry has not the `PATH` a shell has, but a better one than a
    /// certain failure.
    fn resolve() -> OsString {
        let Ok(path) = std::env::current_exe() else {
            return OsString::from("x2rock");
        };
        if path.exists() {
            return path.into_os_string();
        }
        match undeleted(&path).filter(|path| path.exists()) {
            Some(path) => path.into_os_string(),
            None => OsString::from("x2rock"),
        }
    }

    /// Run one CLI command and wait for it.
    ///
    /// Waiting for the child is what makes its exit status and its last line
    /// available to report. It is the *caller's* task that waits, not the
    /// screen: `drive` runs each of these off the event loop and gives up on it
    /// after a while, and a child that is given up on is killed rather than
    /// left to finish on a household nobody is watching any more.
    async fn run(&self, args: &[&str]) -> Result<()> {
        let mut command = tokio::process::Command::new(&self.binary);
        command.kill_on_drop(true);
        if let Some(ip) = self.ip {
            command.arg("--ip").arg(ip.to_string());
        }
        let output = command
            .args(args)
            .output()
            .await
            .context("running x2rock")?;
        if output.status.success() {
            return Ok(());
        }
        // The CLI's own sentence is better than anything reconstructable from
        // an exit code: it is the one that names the room, the code and the
        // fix. It is the *last* line, though - progress notes go to stderr
        // first ("taking its group to the TV input...") and the reason follows
        // them - and it arrives prefixed the way `main` prints it, which the
        // footer says by colour instead.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .map(str::trim)
            .rev()
            .find(|line| !line.is_empty())
            .map(|line| line.strip_prefix("Error: ").unwrap_or(line))
            .unwrap_or_default();
        anyhow::bail!(
            "{}",
            if message.is_empty() {
                format!("x2rock {} failed", args.join(" "))
            } else {
                message.to_owned()
            }
        )
    }

    /// Join rooms to a coordinator. `group` takes the coordinator as `-r` and
    /// the others positionally - an asymmetry with `ungroup`, which takes its
    /// room positionally and no `-r` at all.
    pub async fn group(&self, coordinator: &str, others: &[String]) -> Result<()> {
        let mut args = vec!["-r", coordinator, "group"];
        args.extend(others.iter().map(String::as_str));
        self.run(&args).await
    }

    pub async fn ungroup(&self, room: &str) -> Result<()> {
        self.run(&["ungroup", room]).await
    }

    /// Party captures every room in the house, which is why the TUI asks before
    /// sending it rather than putting it under a bare keystroke.
    pub async fn party(&self, room: &str) -> Result<()> {
        self.run(&["-r", room, "party"]).await
    }

    pub async fn party_off(&self) -> Result<()> {
        self.run(&["party", "off"]).await
    }

    /// Move a volume by so many points: the group's, or with `player` this one
    /// speaker's own beneath it.
    ///
    /// **Relative, never read-modify-write.** A key is a stateless control, and
    /// the rule for those (docs/architecture.md, "setRelativeVolume for
    /// stateless controls") is not a style preference: the daemon publishes a
    /// muted room's volume as zero, because that is what is heard, so an
    /// absolute set computed from the screen would unmute a room at five
    /// percent and throw away the forty it was holding. MPRIS has no relative
    /// volume, which is why this is a subprocess and not a property write.
    pub async fn nudge_volume(&self, room: &str, by: i16, player: bool) -> Result<()> {
        let by = format!("{by:+}");
        let mut args = vec!["-r", room, "vol", &by];
        if player {
            args.push("--player");
        }
        self.run(&args).await
    }

    /// Mute or unmute a group. Group mute is what mute means - the CLI refuses
    /// `--player` here, since muting one speaker of a group is not a thing
    /// anyone asks for - so this offers no per-speaker form.
    pub async fn mute(&self, room: &str, on: bool) -> Result<()> {
        self.run(&["-r", room, "vol", if on { "mute" } else { "unmute" }])
            .await
    }

    /// Crossfade on or off for a group. A play mode like repeat and shuffle,
    /// but MPRIS has no property for it, so of the three it is the one that
    /// goes through the CLI.
    pub async fn crossfade(&self, room: &str, on: bool) -> Result<()> {
        self.run(&["-r", room, "crossfade", if on { "on" } else { "off" }])
            .await
    }

    pub async fn tv(&self, room: &str) -> Result<()> {
        self.run(&["-r", room, "tv"]).await
    }
}

/// The path a `/proc/self/exe` link names once the file behind it has been
/// replaced, with the kernel's marker taken back off. `None` when there is no
/// marker to take off, which is every ordinary case.
fn undeleted(path: &Path) -> Option<PathBuf> {
    Some(PathBuf::from(path.to_str()?.strip_suffix(" (deleted)")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing here that can be tested without a filesystem: an upgrade
    /// leaves the running binary's own path with a marker on the end, and the
    /// file it means is the one without it.
    #[test]
    fn a_replaced_binary_is_found_under_the_kernels_marker() {
        assert_eq!(
            undeleted(Path::new("/home/x/.local/bin/x2rock (deleted)")),
            Some(PathBuf::from("/home/x/.local/bin/x2rock"))
        );
        assert_eq!(undeleted(Path::new("/home/x/.local/bin/x2rock")), None);
        // Not a suffix match on the word alone: a file may be called that.
        assert_eq!(undeleted(Path::new("/home/x/bin/x2rock(deleted)")), None);
    }
}
