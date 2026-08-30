//! Reasons to drop every connection and start over, from the system bus.
//!
//! Both are things the sockets themselves will not report. A socket does not
//! survive a suspend, but on resume it is a zombie whose writes succeed into a
//! dead TCP session. A socket does not survive the network moving either, and
//! the address behind it stops being true well before anything says so.
//!
//! Watching neither is a valid state: the keepalive's silence limit still finds
//! a dead socket, just a minute and a half later. Each source is therefore
//! attempted separately, and a missing one costs only its own speed.

use std::fmt;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::StreamExt;
use tokio::sync::{broadcast, mpsc};

/// `NM_STATE_CONNECTED_LOCAL`. Site and global connectivity are higher, and any
/// of the three is enough to try a player on the LAN.
const NM_STATE_CONNECTED_LOCAL: u32 = 50;

/// NetworkManager reports one switch as a burst - disconnect, connect, address,
/// primary connection - and reconnecting per step would flap every MPRIS bus
/// name on the way through. Wait for it to go quiet instead, which also means
/// the reconnect lands on a network that has finished arriving.
const SETTLE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Resumed,
    NetworkChanged,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reason::Resumed => "resumed from suspend",
            Reason::NetworkChanged => "the network changed",
        })
    }
}

/// A reason, and when the thing behind it last moved.
///
/// The time is what makes a stale request recognisable. Settling delays a
/// network change by a couple of seconds, and a resume sends the daemon into a
/// retry loop against a network that has not arrived yet, so the reconnect those
/// two provoke can easily land *before* the change is reported. Carrying the
/// moment of the change - not the moment it was reported - lets a connection
/// made since then be recognised as the answer to it.
#[derive(Clone, Copy, Debug)]
pub struct Restart {
    pub reason: Reason,
    pub at: Instant,
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login1 {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool);
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    #[zbus(signal)]
    fn state_changed(&self, state: u32);

    // Named around the `StateChanged` signal above: as `state` this would
    // generate the same `receive_state_changed` the signal already has.
    #[zbus(property, name = "State")]
    fn current_state(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn primary_connection(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

/// Hands out receivers that fire when something has invalidated the daemon's
/// connections.
pub struct Restarts {
    restarts: broadcast::Sender<Restart>,
    /// Network triggers before settling, each stamped with when it was raised;
    /// the debouncer is what reaches `restarts`.
    network: mpsc::UnboundedSender<Instant>,
}

impl Restarts {
    /// Watching nothing yet. Every source is optional, so this alone is a
    /// working - if slow - configuration.
    pub fn new() -> Self {
        let (restarts, _) = broadcast::channel(1);
        let (network, mut raw) = mpsc::unbounded_channel();

        let sender = restarts.clone();
        tokio::spawn(async move {
            while let Some(mut at) = raw.recv().await {
                // Swallow the rest of the burst, keeping when it last moved -
                // that, not the moment it went quiet, is when the network
                // changed, and a connection made after it answers it.
                while let Ok(Some(seen)) = tokio::time::timeout(SETTLE, raw.recv()).await {
                    at = seen;
                }
                let _ = sender.send(Restart {
                    reason: Reason::NetworkChanged,
                    at,
                });
            }
        });

        Self { restarts, network }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Restart> {
        self.restarts.subscribe()
    }

    /// Wake from suspend, over logind.
    ///
    /// `PrepareForSleep` is emitted twice per cycle: `true` on the way down,
    /// `false` on the way back. Only the wake is worth acting on - the socket is
    /// dead from the moment the machine froze, so there is nothing to save by
    /// closing it early, and a laptop that never comes back needs nothing.
    pub async fn watch_suspend(&self) -> Result<()> {
        let connection = zbus::Connection::system().await?;
        let proxy = Login1Proxy::new(&connection).await?;
        let mut signals = proxy.receive_prepare_for_sleep().await?;

        let restarts = self.restarts.clone();
        tokio::spawn(async move {
            // Kept alongside the stream it feeds: dropping the connection would
            // end the subscription.
            let _connection = connection;
            while let Some(signal) = signals.next().await {
                let Ok(args) = signal.args() else { continue };
                if !args.start {
                    // Failing to send only means nothing is serving a connection
                    // right now, which is already what a restart would ask for.
                    let _ = restarts.send(Restart {
                        reason: Reason::Resumed,
                        at: Instant::now(),
                    });
                }
            }
        });
        Ok(())
    }

    /// The network moving underneath, over NetworkManager.
    pub async fn watch_network(&self) -> Result<()> {
        let connection = zbus::Connection::system().await?;
        let proxy = NetworkManagerProxy::new(&connection).await?;
        let mut states = proxy.receive_state_changed().await?;
        let mut primary = proxy.receive_primary_connection_changed().await;
        // A property stream opens by announcing the value it already has, and
        // NetworkManager will re-announce one that did not move, so the route
        // out is compared rather than counted. Starting from what it is now
        // means the opening announcement is not mistaken for a change.
        let mut route = proxy.primary_connection().await.ok();
        let mut connected = proxy
            .current_state()
            .await
            .is_ok_and(|state| state >= NM_STATE_CONNECTED_LOCAL);

        let network = self.network.clone();
        tokio::spawn(async move {
            let _connection = connection;
            loop {
                tokio::select! {
                    Some(signal) = states.next() => {
                        let Ok(args) = signal.args() else { continue };
                        let now = args.state >= NM_STATE_CONNECTED_LOCAL;
                        // Only arriving on a network is worth a reconnect, and
                        // only the moment of arrival: NetworkManager climbs
                        // local -> site -> global as it checks connectivity, and
                        // each step would otherwise ask again. Leaving is not
                        // worth acting on either - there is nothing to reconnect
                        // to until something comes back.
                        if now && !connected {
                            let _ = network.send(Instant::now());
                        }
                        connected = now;
                    }
                    // The global state stays "connected" while the route out
                    // changes underneath - docking, or a VPN coming up - so the
                    // primary connection is watched as well as the state.
                    Some(change) = primary.next() => {
                        let moved = change.get().await.ok();
                        let changed = moved != route;
                        route = moved;
                        // As with the state: only gaining a route is worth a
                        // reconnect. "/" is how NetworkManager says there is
                        // none, and losing one leaves nothing to reconnect to.
                        let have_route = route
                            .as_ref()
                            .is_some_and(|path| path.as_str() != "/");
                        if changed && have_route {
                            let _ = network.send(Instant::now());
                        }
                    }
                    else => return,
                }
            }
        });
        Ok(())
    }
}
