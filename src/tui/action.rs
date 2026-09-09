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

use anyhow::{Context, Result};

/// The CLI as the TUI was started: this binary, with whatever `--ip` it was
/// given. Carried rather than reconstructed, because the network that needs
/// `--ip` for the daemon needs it for every child too - and the child that
/// dropped it would fail with the very hint the user had already followed.
#[derive(Clone, Copy)]
pub struct Cli {
    ip: Option<IpAddr>,
}

impl Cli {
    pub fn new(ip: Option<IpAddr>) -> Self {
        Self { ip }
    }

    /// The binary to shell out to - this one, whatever it is called and wherever
    /// it was installed. Resolving it rather than trusting `PATH` matters
    /// because a TUI launched from a desktop entry does not inherit the `PATH`
    /// an interactive shell has, which is the same trap the README documents
    /// for `~/.local/bin`.
    fn binary() -> Result<OsString> {
        Ok(std::env::current_exe()
            .context("finding this binary to run its CLI")?
            .into_os_string())
    }

    /// Run one CLI command and wait for it.
    ///
    /// Waiting for the child is what makes its exit status and its last line
    /// available to report. It is the *caller's* task that waits, not the
    /// screen: `drive` runs each of these off the event loop and gives up on it
    /// after a while, and a child that is given up on is killed rather than
    /// left to finish on a household nobody is watching any more.
    async fn run(&self, args: &[&str]) -> Result<()> {
        let mut command = tokio::process::Command::new(Self::binary()?);
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

    pub async fn tv(&self, room: &str) -> Result<()> {
        self.run(&["-r", room, "tv"]).await
    }
}
