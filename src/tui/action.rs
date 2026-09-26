//! Changing something, by whichever of the two routes can express it.
//!
//! MPRIS carries transport, repeat and shuffle, so those go straight down the
//! bus and land instantly. It carries nothing for grouping, party, TV input,
//! mute, crossfade, a single speaker's volume beneath its group - or a
//! *relative* volume step, which is what a key is (see [`Speakers::nudge`]).
//! Those call the CLI's own command functions, in this process, over a session
//! this module holds open: the sockets to the players are opened on the first
//! write and kept, so a volume key costs one round trip rather than the
//! reconnect a subprocess paid on every press.
//!
//! **The session is re-read before every write and dropped on any failure.**
//! Rooms are resolved against the topology last read, and a regroup makes that
//! wrong; one `getGroups` on the held socket before each write is cheaper than
//! any scheme for noticing. A socket that has stopped answering - what a
//! resume from suspend leaves behind - fails the write, and the next write
//! reconnects from nothing: one visible error, then recovery.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::commands::{Report, household, playback, speaker, volume};
use crate::session::{self, Session};
use crate::state::State;

/// The players, reached the way the CLI reaches them - `--ip` and
/// `--household` as the TUI was started with them - and held.
#[derive(Clone)]
pub struct Speakers {
    ip: Option<IpAddr>,
    household: Option<String>,
    session: Arc<Mutex<Option<Session>>>,
}

impl Speakers {
    pub fn new(ip: Option<IpAddr>, household: Option<String>) -> Self {
        Self {
            ip,
            household,
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// The held session with its topology just re-read, opened first if there
    /// is none. A handle: the write runs on the clone, outside the lock, so a
    /// slow party does not hold up a volume key behind it.
    async fn session(&self) -> Result<Session> {
        let mut held = self.session.lock().await;
        if held.is_none() {
            let mut state = State::load()?;
            let opened =
                session::connect(self.ip, &mut state, self.household.as_deref(), None).await?;
            *held = Some(opened);
        }
        let session = held.as_mut().expect("opened just above");
        if let Err(e) = session.refresh_groups().await {
            // The socket is not answering; nothing here is worth keeping.
            if let Some(dead) = held.take() {
                dead.close().await;
            }
            return Err(e);
        }
        Ok(session.clone())
    }

    /// What a write leaves for the screen: its notes, or its error - and on an
    /// error, no session, so the next write starts from nothing.
    async fn settle<O: Report>(&self, outcome: Result<O>) -> Result<Vec<String>> {
        match outcome {
            Ok(outcome) => Ok(outcome.notes().to_vec()),
            Err(e) => {
                if let Some(session) = self.session.lock().await.take() {
                    session.close().await;
                }
                Err(e)
            }
        }
    }

    /// Join rooms to a coordinator. `group` takes the coordinator as `-r` and
    /// the others positionally - an asymmetry with `ungroup`, which takes its
    /// room positionally and no `-r` at all.
    pub async fn group(&self, coordinator: &str, others: &[String]) -> Result<Vec<String>> {
        let session = self.session().await?;
        let outcome = household::group(&session, Some(coordinator), others).await;
        self.settle(outcome).await
    }

    pub async fn ungroup(&self, room: &str) -> Result<Vec<String>> {
        let session = self.session().await?;
        let outcome = household::ungroup(&session, room).await;
        self.settle(outcome).await
    }

    /// Party captures every room in the house, which is why the TUI asks before
    /// sending it rather than putting it under a bare keystroke.
    pub async fn party(&self, room: &str) -> Result<Vec<String>> {
        let session = self.session().await?;
        let outcome = household::party(&session, Some(room), None).await;
        self.settle(outcome).await
    }

    pub async fn party_off(&self) -> Result<Vec<String>> {
        let session = self.session().await?;
        let outcome = household::party(&session, None, Some("off")).await;
        self.settle(outcome).await
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
    /// volume, which is why this is a command and not a property write.
    pub async fn nudge(&self, room: &str, by: i16, player: bool) -> Result<Vec<String>> {
        let session = self.session().await?;
        let outcome = async {
            let target = session::target(&session.groups, Some(room))?;
            volume::apply_vol(
                &session,
                &target,
                Some(room),
                Some(format!("{by:+}")),
                player,
                false,
            )
            .await
        }
        .await;
        self.settle(outcome).await
    }

    /// Mute or unmute a group. Group mute is what mute means - the command
    /// refuses `--player` here, since muting one speaker of a group is not a
    /// thing anyone asks for - so this offers no per-speaker form.
    pub async fn mute(&self, room: &str, on: bool) -> Result<Vec<String>> {
        let session = self.session().await?;
        let word = if on { "mute" } else { "unmute" };
        let outcome = async {
            let target = session::target(&session.groups, Some(room))?;
            volume::apply_vol(
                &session,
                &target,
                Some(room),
                Some(word.into()),
                false,
                false,
            )
            .await
        }
        .await;
        self.settle(outcome).await
    }

    /// Crossfade on or off for a group. A play mode like repeat and shuffle,
    /// but MPRIS has no property for it, so of the three it is the one that
    /// goes through a command.
    pub async fn crossfade(&self, room: &str, on: bool) -> Result<Vec<String>> {
        let session = self.session().await?;
        let word = if on { "on" } else { "off" };
        let outcome = async {
            let target = session::target(&session.groups, Some(room))?;
            playback::apply_crossfade(&session, &target, Some(word.into())).await
        }
        .await;
        self.settle(outcome).await
    }

    pub async fn tv(&self, room: &str) -> Result<Vec<String>> {
        let session = self.session().await?;
        let outcome = async {
            let target = session::target(&session.groups, Some(room))?;
            let player = session::coordinator(&session, &target).await?;
            speaker::tv(&session, &player, &target, Some(room)).await
        }
        .await;
        self.settle(outcome).await
    }
}
