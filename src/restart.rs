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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    // The debouncer only speaks `SETTLE` after the last thing it heard, so these
    // run on a paused clock. Tokio advances it only when every task is stuck,
    // which means the settle window is exercised in full and the suite still
    // finishes instantly. `network` is reached directly: the D-Bus watchers that
    // normally feed it need a system bus, and what is worth pinning here is what
    // the debouncer does with what they send, not their plumbing.

    #[tokio::test(start_paused = true)]
    async fn a_burst_settles_into_one_restart_stamped_when_it_last_moved() {
        let restarts = Restarts::new();
        let mut events = restarts.subscribe();

        // One network switch as NetworkManager actually reports it: disconnect,
        // connect, address, primary connection.
        let first = Instant::now();
        let last = first + Duration::from_millis(400);
        for at in [first, first + Duration::from_millis(150), last] {
            restarts.network.send(at).unwrap();
        }

        let restart = events.recv().await.unwrap();
        assert_eq!(restart.reason, Reason::NetworkChanged);
        // The moment the network last moved, not the moment the burst went
        // quiet. That is the whole point of carrying `at`: a connection made
        // after this instant is the answer to this restart, and settling adds a
        // couple of seconds during which one can be made.
        assert_eq!(restart.at, last);

        // And the rest of the burst does not each arrive as its own restart,
        // which is what would flap every MPRIS bus name on the way through.
        assert!(timeout(SETTLE * 3, events.recv()).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn even_a_lone_change_waits_out_the_settle_window() {
        let restarts = Restarts::new();
        let mut events = restarts.subscribe();
        restarts.network.send(Instant::now()).unwrap();

        // Nothing yet, and nothing wrong: the first signal of a burst and a
        // lone one are the same signal until the window closes on it.
        assert!(timeout(SETTLE / 2, events.recv()).await.is_err());
        // Then it lands, so a single change is not swallowed by the wait.
        assert!(events.recv().await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_later_change_is_its_own_restart_rather_than_folded_into_the_last() {
        let restarts = Restarts::new();
        let mut events = restarts.subscribe();

        let docked = Instant::now();
        restarts.network.send(docked).unwrap();
        assert_eq!(events.recv().await.unwrap().at, docked);

        // A VPN coming up minutes later is a second move, not more of the first.
        let vpn_up = docked + Duration::from_secs(300);
        restarts.network.send(vpn_up).unwrap();
        assert_eq!(events.recv().await.unwrap().at, vpn_up);
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_with_nothing_subscribed_does_not_end_the_debouncer() {
        let restarts = Restarts::new();

        // Nobody is subscribed, so the broadcast send fails. That is a
        // non-event - a restart asks whoever is serving a connection to drop it,
        // and nobody is - but the loop has to survive it, or the first network
        // change before the daemon settles would silently disarm the rest.
        restarts.network.send(Instant::now()).unwrap();
        tokio::time::sleep(SETTLE * 2).await;

        let mut events = restarts.subscribe();
        let moved = Instant::now();
        restarts.network.send(moved).unwrap();
        assert_eq!(events.recv().await.unwrap().at, moved);
    }

    #[test]
    fn the_reasons_read_as_the_journal_prints_them() {
        // daemon.rs logs these straight into "<reason>; reconnecting", so the
        // wording is the log line rather than an internal label.
        assert_eq!(Reason::Resumed.to_string(), "resumed from suspend");
        assert_eq!(Reason::NetworkChanged.to_string(), "the network changed");
    }
}
