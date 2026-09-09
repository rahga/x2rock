//! The daemon, read over D-Bus.
//!
//! The TUI has one state source and requires the daemon, unlike the CLI, which
//! promises to need nothing running. That is a deliberate split: a terminal UI
//! wants push, and the daemon is the only thing that has it. What it publishes
//! is exactly the set the bar widget renders, so this is a second consumer of a
//! contract that already exists rather than a new one.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, MatchRule, MessageStream, fdo};

use super::model::RoomSnapshot;

/// Every player the daemon publishes carries this prefix, one per group.
const PREFIX: &str = "org.mpris.MediaPlayer2.x2rock-";
const PATH: &str = "/org/mpris/MediaPlayer2";

/// A burst of events is one change: regrouping republishes every player, and a
/// track change arrives as several property signals at once. Reading once after
/// the burst settles beats reading once per signal, and at this scale the wait
/// is the only cost. The signals that queued up during the wait are thrown away
/// before the read - see [`Source::watch`] - or the burst would be read once per
/// signal after all, just later.
const SETTLE: Duration = Duration::from_millis(150);
/// How long to wait for the daemon to have a player on the bus before deciding
/// it is not there. It owns no name of its own, so "no players" is also what a
/// running daemon looks like for the second or two of every republish.
const PATIENCE: Duration = Duration::from_secs(3);
const PATIENCE_STEP: Duration = Duration::from_millis(250);

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait MediaPlayer2 {
    #[zbus(property)]
    fn identity(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub trait Player {
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;
    fn play_pause(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn volume(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn set_volume(&self, level: f64) -> zbus::Result<()>;
    #[zbus(property)]
    fn loop_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_loop_status(&self, status: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn shuffle(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_shuffle(&self, on: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn can_go_next(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_go_previous(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_pause(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_play(&self) -> zbus::Result<bool>;
}

/// Cloning shares the connection rather than opening a second one - a zbus
/// `Connection` is a handle - which is what lets the watcher own one while the
/// app keeps another to write through.
#[derive(Clone)]
pub struct Source {
    connection: Connection,
}

impl Source {
    /// Attach to the session bus and check the daemon is actually there.
    ///
    /// The check is the point. Without it the first symptom of a stopped daemon
    /// is an empty screen, which reads as "no speakers" - a different and much
    /// more alarming problem than "the service is not running".
    ///
    /// It waits a little first, because the daemon has no bus name of its own:
    /// the only sign of it is its players, and there are none of those while it
    /// is between dropping one set and publishing the next, or while it is
    /// still working out which network it is on. Exiting during that window
    /// would tell someone to start a service `systemctl` shows running.
    pub async fn connect() -> Result<Self> {
        let connection = Connection::session()
            .await
            .context("connecting to the session bus")?;
        let source = Self { connection };
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while source.players().await?.is_empty() {
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "x2rock's daemon has published no rooms, so there is nothing to control.\n\
                     If it is not running, start it with: systemctl --user start x2rock\n\
                     If it is, it has not found the household yet: journalctl --user -u x2rock"
                );
            }
            tokio::time::sleep(PATIENCE_STEP).await;
        }
        Ok(source)
    }

    /// The bus names the daemon currently owns, one per group.
    async fn players(&self) -> Result<Vec<String>> {
        let dbus = fdo::DBusProxy::new(&self.connection).await?;
        let mut names: Vec<String> = dbus
            .list_names()
            .await?
            .into_iter()
            .map(|n| n.to_string())
            .filter(|n| n.starts_with(PREFIX))
            .collect();
        // Stable order, so a repaint never reshuffles the rows under someone
        // about to press a key.
        names.sort();
        Ok(names)
    }

    /// Read every room. Cheap enough to do wholesale: this is a local socket, a
    /// household is a handful of groups, and diffing would buy nothing but a
    /// chance to be subtly wrong about which row moved.
    pub async fn snapshot(&self) -> Result<Vec<RoomSnapshot>> {
        let mut rooms = Vec::new();
        for name in self.players().await? {
            match self.read_one(&name).await {
                Ok(room) => rooms.push(room),
                // A player that vanished between listing and reading is not an
                // error, it is a regroup landing mid-read. The next event brings
                // the corrected list along.
                Err(_) => continue,
            }
        }
        Ok(rooms)
    }

    async fn read_one(&self, bus_name: &str) -> Result<RoomSnapshot> {
        let app = MediaPlayer2Proxy::builder(&self.connection)
            .destination(bus_name.to_owned())?
            .build()
            .await?;
        let player = PlayerProxy::builder(&self.connection)
            .destination(bus_name.to_owned())?
            .build()
            .await?;

        let mut room = RoomSnapshot {
            bus_name: bus_name.to_owned(),
            room: app.identity().await?,
            ..RoomSnapshot::default()
        };
        room.apply_metadata(&player.metadata().await?);
        room.set_playback_state(&player.playback_status().await?);
        // After the metadata, which may carry the level mute is holding; the
        // Volume property alone reads zero on a muted room.
        room.settle_volume(player.volume().await.unwrap_or(0.0));
        room.loop_status = player.loop_status().await.unwrap_or_default();
        room.shuffle = player.shuffle().await.unwrap_or(false);
        room.can_go_next = player.can_go_next().await.unwrap_or(false);
        room.can_go_previous = player.can_go_previous().await.unwrap_or(false);
        room.can_pause = player.can_pause().await.unwrap_or(false);
        room.can_play = player.can_play().await.unwrap_or(false);
        Ok(room)
    }

    pub async fn player(&self, bus_name: &str) -> Result<PlayerProxy<'_>> {
        Ok(PlayerProxy::builder(&self.connection)
            .destination(bus_name.to_owned())?
            .build()
            .await?)
    }

    /// Watch for anything that changes a room, and send the whole list when it
    /// settles.
    ///
    /// Two sources, because two different things move: property signals from any
    /// player carry track, volume and mode changes, and `NameOwnerChanged`
    /// carries players arriving and leaving - which is how a regroup and a
    /// daemon restart both correct themselves without a refresh key.
    ///
    /// One match rule for every player rather than a proxy each: players come
    /// and go with the topology, and per-player bookkeeping would be one more
    /// thing to get wrong on exactly the events that already move the ground.
    pub async fn watch(self, tx: mpsc::UnboundedSender<Vec<RoomSnapshot>>) -> Result<()> {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path(PATH)?
            .build();
        let mut properties = MessageStream::for_match_rule(rule, &self.connection, None).await?;

        let dbus = fdo::DBusProxy::new(&self.connection).await?;
        let mut owners = dbus.receive_name_owner_changed().await?;

        loop {
            // Wait for something to happen, then let the burst finish before
            // reading: see SETTLE.
            // Neither stream should ever end while the bus is up. If one does,
            // say which: the TUI exited once during a household's first
            // republish after a network change (2026-09-09) and took no reason
            // with it, and "lost the session bus" was all it could have said.
            tokio::select! {
                message = properties.next() => {
                    if message.is_none() {
                        bail!("the PropertiesChanged signal stream ended");
                    }
                }
                owner = owners.next() => {
                    let Some(owner) = owner else {
                        bail!("the NameOwnerChanged signal stream ended");
                    };
                    let matched = owner
                        .args()
                        .map(|a| a.name().starts_with(PREFIX))
                        .unwrap_or(false);
                    if !matched {
                        continue;
                    }
                }
            }
            tokio::time::sleep(SETTLE).await;
            // Whatever else arrived while settling was part of the same burst,
            // and the read below covers it. Left in the queue, each one would
            // wake the loop again for a sleep and a full read of its own - a
            // regroup of five groups is a dozen signals, so a dozen reads - and
            // a queue nobody drains eventually stalls the connection's reader
            // for every other stream on it, this one's own snapshot included.
            while properties.next().now_or_never().flatten().is_some() {}
            while owners.next().now_or_never().flatten().is_some() {}
            let rooms = self.snapshot().await.unwrap_or_default();
            if tx.send(rooms).is_err() {
                // The screen has gone; nothing left to tell.
                return Ok(());
            }
        }
    }
}
