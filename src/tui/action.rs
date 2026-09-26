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
//! **The topology is re-read before every write, and the session is dropped
//! when a socket proves dead.** Rooms are resolved against the topology last
//! read, and a regroup makes that wrong; one `getGroups` on the held socket
//! before each write is cheaper than any scheme for noticing. A socket that
//! has stopped answering - what a resume from suspend leaves behind - fails
//! the write with [`Unreachable`], and the next write reconnects from
//! nothing: one visible error, then recovery. A player *refusing* a write, or
//! a room that cannot be resolved, is not that, and costs no reconnect.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use tokio::sync::Mutex;

use crate::commands::{Report, household, playback, speaker, volume};
use crate::session::{self, Session};
use crate::sonos::local::Unreachable;
use crate::state::State;

/// The players, reached the way the CLI reaches them - `--ip` and
/// `--household` as the TUI was started with them - and held.
#[derive(Clone)]
pub struct Speakers {
    ip: Option<IpAddr>,
    household: Option<String>,
    held: Arc<Mutex<Held>>,
}

/// The one connect in flight, shared by every write that arrives while it
/// runs. The work is a spawned task that stores its own result, so a write
/// given up on mid-connect neither cancels the connect nor loses the session
/// it was about to produce.
type Connecting = Shared<BoxFuture<'static, Result<(u64, Session), Arc<anyhow::Error>>>>;

#[derive(Default)]
struct Held {
    session: Option<Session>,
    /// Bumped each time a session is stored, so a write that fails can tell
    /// whether the session it ran on is still the one held - and not close a
    /// newer one that another write opened meanwhile and is using.
    generation: u64,
    connecting: Option<Connecting>,
}

/// Whether a failure means the socket is no good, as opposed to the player
/// declining or the command refusing: the one case that earns a reconnect.
fn severs(e: &anyhow::Error) -> bool {
    Unreachable::of(e).is_some()
}

impl Speakers {
    pub fn new(ip: Option<IpAddr>, household: Option<String>) -> Self {
        Self {
            ip,
            household,
            held: Arc::new(Mutex::new(Held::default())),
        }
    }

    /// The held session with its topology just re-read - opened first if there
    /// is none - and the generation it belongs to. A handle: the write runs on
    /// the clone, outside the lock, so a slow party does not hold up a volume
    /// key behind it, and neither does the re-read.
    async fn session(&self) -> Result<(u64, Session)> {
        let (generation, mut session) = match self.current().await {
            Some(held) => held,
            None => self.connect().await?,
        };
        if let Err(e) = session.refresh_groups().await {
            if severs(&e) || !session.connection.is_alive() {
                self.release(generation).await;
            }
            return Err(e);
        }
        // The re-read is the session's, not just this write's.
        let mut held = self.held.lock().await;
        if held.generation == generation
            && let Some(kept) = held.session.as_mut()
        {
            kept.groups = session.groups.clone();
        }
        Ok((generation, session))
    }

    async fn current(&self) -> Option<(u64, Session)> {
        let held = self.held.lock().await;
        held.session
            .as_ref()
            .map(|session| (held.generation, session.clone()))
    }

    /// Open a session, or join the one already being opened.
    async fn connect(&self) -> Result<(u64, Session)> {
        let connecting = {
            let mut held = self.held.lock().await;
            if let Some(session) = &held.session {
                return Ok((held.generation, session.clone()));
            }
            held.connecting
                .get_or_insert_with(|| {
                    let task = tokio::spawn(Self::open_and_store(
                        self.held.clone(),
                        self.ip,
                        self.household.clone(),
                    ));
                    async move {
                        task.await
                            .map_err(|e| Arc::new(anyhow!("connecting to the players: {e}")))?
                    }
                    .boxed()
                    .shared()
                })
                .clone()
        };
        connecting.await.map_err(|e| anyhow!("{e:#}"))
    }

    /// The connect itself, off the caller's future. Stores what it opened
    /// unless a session arrived meanwhile, in which case that one wins and
    /// this one is closed rather than dropped - `Connection` has no `Drop`.
    async fn open_and_store(
        held: Arc<Mutex<Held>>,
        ip: Option<IpAddr>,
        household: Option<String>,
    ) -> Result<(u64, Session), Arc<anyhow::Error>> {
        let opened = async {
            let mut state = State::load()?;
            session::connect(ip, &mut state, household.as_deref(), None).await
        }
        .await;
        let mut held = held.lock().await;
        held.connecting = None;
        let session = opened.map_err(Arc::new)?;
        match &held.session {
            Some(existing) => {
                let existing = existing.clone();
                session.close().await;
                Ok((held.generation, existing))
            }
            None => {
                held.generation += 1;
                held.session = Some(session.clone());
                Ok((held.generation, session))
            }
        }
    }

    /// Forget the held session, if it is still the one of `generation`, and
    /// close its sockets. A newer one, opened by some other write since, is
    /// theirs and is left alone.
    async fn release(&self, generation: u64) {
        let taken = {
            let mut held = self.held.lock().await;
            (held.generation == generation)
                .then(|| held.session.take())
                .flatten()
        };
        if let Some(session) = taken {
            session.close().await;
        }
    }

    /// Run one write on the held session and settle what it leaves for the
    /// screen: its notes, or its error. The policy lives here once: only a
    /// severed socket costs the session, and only the session the write ran
    /// on.
    async fn write<O, F, Fut>(&self, f: F) -> Result<Vec<String>>
    where
        O: Report,
        F: FnOnce(Session) -> Fut,
        Fut: Future<Output = Result<O>>,
    {
        let (generation, session) = self.session().await?;
        let alive = session.connection.clone();
        match f(session).await {
            Ok(outcome) => Ok(outcome.notes().to_vec()),
            Err(e) => {
                if severs(&e) || !alive.is_alive() {
                    self.release(generation).await;
                }
                Err(e)
            }
        }
    }

    /// Join rooms to a coordinator. `group` takes the coordinator as `-r` and
    /// the others positionally - an asymmetry with `ungroup`, which takes its
    /// room positionally and no `-r` at all.
    pub async fn group(&self, coordinator: &str, others: &[String]) -> Result<Vec<String>> {
        self.write(|s| async move { household::group(&s, Some(coordinator), others).await })
            .await
    }

    pub async fn ungroup(&self, room: &str) -> Result<Vec<String>> {
        self.write(|s| async move { household::ungroup(&s, room).await })
            .await
    }

    /// Party captures every room in the house, which is why the TUI asks before
    /// sending it rather than putting it under a bare keystroke.
    pub async fn party(&self, room: &str) -> Result<Vec<String>> {
        self.write(|s| async move { household::party(&s, Some(room), None).await })
            .await
    }

    pub async fn party_off(&self) -> Result<Vec<String>> {
        self.write(|s| async move { household::party(&s, None, Some("off")).await })
            .await
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
        self.write(|s| async move {
            let target = session::target(&s.groups, Some(room))?;
            volume::apply_vol(
                &s,
                &target,
                Some(room),
                Some(format!("{by:+}")),
                player,
                false,
            )
            .await
        })
        .await
    }

    /// Mute or unmute a group. Group mute is what mute means - the command
    /// refuses `--player` here, since muting one speaker of a group is not a
    /// thing anyone asks for - so this offers no per-speaker form.
    pub async fn mute(&self, room: &str, on: bool) -> Result<Vec<String>> {
        let word = if on { "mute" } else { "unmute" };
        self.write(|s| async move {
            let target = session::target(&s.groups, Some(room))?;
            volume::apply_vol(&s, &target, Some(room), Some(word.into()), false, false).await
        })
        .await
    }

    /// Crossfade on or off for a group. A play mode like repeat and shuffle,
    /// but MPRIS has no property for it, so of the three it is the one that
    /// goes through a command.
    pub async fn crossfade(&self, room: &str, on: bool) -> Result<Vec<String>> {
        let word = if on { "on" } else { "off" };
        self.write(|s| async move {
            let target = session::target(&s.groups, Some(room))?;
            playback::apply_crossfade(&s, &target, Some(word.into())).await
        })
        .await
    }

    pub async fn tv(&self, room: &str) -> Result<Vec<String>> {
        self.write(|s| async move {
            let target = session::target(&s.groups, Some(room))?;
            let player = session::coordinator(&s, &target).await?;
            speaker::tv(&s, &player, &target, Some(room)).await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonos::local::ApiError;

    /// Only a socket that is no good earns the reconnect: a player refusing,
    /// a room that cannot be resolved, or a command's own refusal all leave
    /// the session as it is. `Unreachable` is read through whatever context a
    /// caller wrapped it in.
    #[test]
    fn only_a_severed_socket_costs_the_session() {
        let refused: anyhow::Error = ApiError {
            what: "groups:1 modifyGroupMembers".into(),
            code: Some("ERROR_INVALID_OBJECT_ID".into()),
            reason: None,
        }
        .into();
        assert!(!severs(&refused));
        assert!(!severs(&anyhow!("no room named \"Attic\"")));
        assert!(!severs(&anyhow!(
            "Kitchen has fixed volume; adjust it on the amplifier"
        )));
        // The only constructor is private to the connection, so a lost socket
        // is provoked the way one really is: a command on a closed one.
        let lost = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let ip: IpAddr = "127.0.0.1".parse().unwrap();
            // Port 9 is discard; nothing listens on it here, and the connect
            // fails at once with an `Unreachable` rather than a timeout.
            crate::sonos::local::Connection::open(ip)
                .await
                .err()
                .unwrap()
        });
        assert!(severs(&lost), "{lost:#}");
        assert!(severs(&lost.context("opening Kitchen")), "through context");
    }
}
