//! `x2rock daemon`: publish every group in the household as an MPRIS2 player and
//! keep it current from the player's own events, reconnecting as needed.
//!
//! The speaker being unreachable is a normal state for a laptop that moves between
//! networks, not an error: back off, stay quiet, try again.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mpris_server::Server;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::mpris::{RoomPlayer, bus_suffix};
use crate::netid;
use crate::restart::{Restart, Restarts};
use crate::session::{self, Session};
use crate::sonos::local::Connection;
use crate::sonos::proto::{self, Event, Groups, Player};
use crate::state::State;
use mpris_server::Property;

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long an unchanging status may stay silent before it is re-logged once, so
/// a daemon stuck in one state still proves it is alive without filling the
/// journal. At the 60s retry floor an away-all-day laptop would otherwise write
/// ~2880 identical lines a day.
const HEARTBEAT: Duration = Duration::from_secs(3600);

fn log(message: &str) {
    eprintln!("x2rock: {message}");
}

/// `X2ROCK_LOG_VERBOSE`, read once for the whole run: the reconnect machinery
/// out loud - every status pass with coalescing off, and the backoff ramp.
///
/// The flag is a knob for the run rather than a per-call cost, so it lives here
/// instead of being threaded through every call site.
fn verbose() -> bool {
    static VERBOSE: OnceLock<bool> = OnceLock::new();
    *VERBOSE.get_or_init(|| std::env::var_os("X2ROCK_LOG_VERBOSE").is_some())
}

/// `X2ROCK_LOG_EVENTS`, read once for the whole run: every event body exactly
/// as it arrived.
///
/// Deliberately not folded into [`verbose`], and not implied by it. The two
/// answer different questions at wildly different rates - verbose is a few
/// lines per reconnect, this is every event on every group - so a household
/// that is actually playing something buries the retry ramp under bodies. They
/// are asked for separately because they are read separately.
fn log_events() -> bool {
    static EVENTS: OnceLock<bool> = OnceLock::new();
    *EVENTS.get_or_init(|| std::env::var_os("X2ROCK_LOG_EVENTS").is_some())
}

/// What [`StatusLog::decide`] resolved to: log fresh, log a heartbeat carrying
/// the count held since the last line, or (the `None` case) stay silent.
#[derive(Debug, PartialEq, Eq)]
enum Emit {
    Fresh,
    Heartbeat(u32),
}

/// Coalesces repeated status lines. The daemon's retry loop revisits the same
/// state every backoff cycle; without this it logs that state on every pass.
///
/// A line is logged when the *status key* changes and suppressed while it holds,
/// with one heartbeat re-log per [`HEARTBEAT`]. The key carries the network
/// fingerprint, so switching networks always counts as a change and flushes
/// immediately - a move is exactly the event a reader wants to see.
struct StatusLog {
    key: Option<String>,
    last_logged: Instant,
    suppressed: u32,
    /// `X2ROCK_LOG_VERBOSE`: log every pass, coalescing off. For diagnosing the
    /// reconnect/backoff/network machinery, where the repetition and the ramp
    /// are the point rather than the noise.
    verbose: bool,
}

impl StatusLog {
    fn new(verbose: bool) -> Self {
        Self {
            key: None,
            last_logged: Instant::now(),
            suppressed: 0,
            verbose,
        }
    }

    /// Log `message` unless it repeats the current status within the heartbeat
    /// window. On the heartbeat re-log, the count of everything held since the
    /// last line rides along, journald-style, so the silence is accounted for.
    fn note(&mut self, key: String, message: &str) {
        match self.decide(key, Instant::now()) {
            None => {}
            Some(Emit::Fresh) => log(message),
            Some(Emit::Heartbeat(n)) => log(&format!(
                "{message} (unchanged, {n}\u{00d7} in the last hour)"
            )),
        }
    }

    /// The side-effect-free heart of [`note`], with the clock passed in so the
    /// window and the count can be tested without waiting an hour.
    fn decide(&mut self, key: String, now: Instant) -> Option<Emit> {
        if self.verbose {
            // Every pass logs; the state is still tracked so turning coalescing
            // back on (a restart without the env) resumes cleanly.
            self.key = Some(key);
            self.last_logged = now;
            self.suppressed = 0;
            return Some(Emit::Fresh);
        }
        if self.key.as_ref() == Some(&key) {
            if now.duration_since(self.last_logged) < HEARTBEAT {
                self.suppressed += 1;
                return None;
            }
            let held = self.suppressed + 1;
            self.last_logged = now;
            self.suppressed = 0;
            return Some(Emit::Heartbeat(held));
        }
        self.key = Some(key);
        self.last_logged = now;
        self.suppressed = 0;
        Some(Emit::Fresh)
    }

    /// A genuine change of state that another line already announces - a fresh
    /// connection, whose rooms the publisher logs by name. Clears the coalescing
    /// so the next failure logs at once instead of being taken for the old one.
    fn reset(&mut self) {
        self.key = None;
        self.suppressed = 0;
    }
}

/// Run until the process is stopped. Never returns `Ok` in practice; the `Result`
/// is for fatal setup errors such as an unreadable state file.
pub async fn run(explicit_ip: Option<IpAddr>) -> Result<()> {
    let mut state = State::load()?;

    // Neither source is required: without them a dead socket is still found by
    // the keepalive's silence limit, just a minute and a half later. So a
    // missing one is worth a line and nothing more.
    let restarts = Restarts::new();
    if let Err(e) = restarts.watch_suspend().await {
        log(&format!(
            "not watching for resume from suspend ({e:#}); \
             a dead socket will be found by the keepalive instead"
        ));
    }
    if let Err(e) = restarts.watch_network().await {
        log(&format!(
            "not watching for network changes ({e:#}); \
             a move between networks will be found by the keepalive instead"
        ));
    }
    // One long-lived receiver, so a restart arriving while connecting or backing
    // off is still waiting to be seen rather than lost between subscriptions.
    let mut restarts = restarts.subscribe();

    let mut backoff = MIN_BACKOFF;
    let mut status = StatusLog::new(verbose());
    loop {
        // Computed here, not read out of the error, so a network switch is a
        // status change even when the failure text is identical - and connect()
        // returns nothing to read a fingerprint out of anyway.
        let fingerprint = netid::network_fingerprint();
        match session::connect(explicit_ip, &mut state).await {
            Ok(session) => {
                backoff = MIN_BACKOFF;
                // The publisher logs the rooms by name, so this transition is
                // already announced; clear coalescing so the next failure - even
                // the same one as before - logs afresh rather than as a repeat.
                status.reset();
                // The sockets date from here, so anything reported as having
                // changed before now is already answered by them.
                let established = Instant::now();
                match serve(session, established, &mut restarts).await {
                    Ok(()) => log("connection closed"),
                    Err(e) => log(&format!("{e:#}")),
                }
            }
            // The retry cadence is not part of the status: it ramps 1s..60s and
            // then holds, and folding it in would defeat the coalescing during
            // the ramp. The heartbeat's "N× in the last hour" conveys that the
            // daemon is still trying.
            // "no player: " frames a generic connect failure by its consequence,
            // but the unregistered-network line must not lead with the *name* of
            // the other error code - the skill teaches agents to tell the two
            // apart in this very log.
            Err(e) => {
                let line = match crate::hint::of(&e).0 {
                    "unregistered_network" => format!("{e:#}"),
                    _ => format!("no player: {e:#}"),
                };
                status.note(format!("{fingerprint:?}|{e:#}"), &line);
            }
        }
        // Restored only under verbose: coalescing drops it because the backoff
        // ramps and would defeat the dedup, but when diagnosing reconnects the
        // ramp is exactly what you want to watch.
        if verbose() {
            log(&format!("retrying in {}s", backoff.as_secs()));
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);

        // `discover` may have run while we were waiting. On a network this daemon
        // started out knowing nothing about, that file is the only way it ever
        // learns of a player: nothing here scans an unrecognised network on its
        // own. Re-reading costs one small file per retry and saves a restart.
        // A read error keeps the copy we have, which is better than none, and the
        // next pass tries again.
        match State::load() {
            Ok(fresh) => state = fresh,
            Err(e) => log(&format!("could not re-read remembered players ({e:#})")),
        }
    }
}

/// Resolve when something has invalidated the connections, or never if nothing
/// is watching. Lagging behind a burst is not itself interesting - the next
/// receive still yields a real reason, and one reconnect answers all of them.
async fn restarted(restarts: &mut broadcast::Receiver<Restart>) -> Restart {
    loop {
        match restarts.recv().await {
            Ok(restart) => return restart,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => std::future::pending::<()>().await,
        }
    }
}

/// What losing a socket means for the household.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Loss {
    /// The primary or a coordinator: the players it feeds go stale, so reconnect.
    Fatal,
    /// A member reached only for its own volume balance: log it and carry on.
    Tolerated,
}

/// Forward one connection's events onto the shared bus until it closes or is
/// aborted. Subscribing here (via `connection.events()`) before any `subscribe`
/// call is sent on that same connection is what keeps the initial snapshot from
/// being missed.
fn spawn_forwarder(
    connection: &Connection,
    tx: mpsc::UnboundedSender<Arc<Event>>,
    loss: Loss,
) -> JoinHandle<()> {
    let mut events = connection.events();
    let ip = connection.ip();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let lost = event.kind == Event::LOST;
                    if lost {
                        // Only this task knows which socket it was: they are all
                        // multiplexed onto one channel from here on.
                        log(&format!("{ip}: connection lost"));
                        // A member's socket was best effort when it was opened
                        // and stays that way when it goes: its balance slider
                        // freezes, and nothing else. Forwarding its LOST would
                        // have `follow` tear down every player for one flaky
                        // portable.
                        if loss == Loss::Tolerated {
                            return;
                        }
                    }
                    if tx.send(event).is_err() || lost {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    log(&format!(
                        "{ip}: dropped {n} events; state will catch up on the next"
                    ));
                }
                Err(RecvError::Closed) => return,
            }
        }
    })
}

/// Serve one connection until it dies, closing every socket opened for it on
/// the way out.
async fn serve(
    session: Session,
    established: Instant,
    restarts: &mut broadcast::Receiver<Restart>,
) -> Result<()> {
    // Group-targeted commands and subscriptions go to that group's own
    // coordinator, which is not necessarily the player reached first - so a
    // connection is opened per distinct coordinator and kept here, keyed by IP.
    let mut pool: HashMap<IpAddr, Connection> =
        HashMap::from([(session.connection.ip(), session.connection.clone())]);
    let result = follow(&session, &mut pool, established, restarts).await;

    // However this ended, close the sockets rather than leaving their reader
    // tasks parked on one - after a suspend they would otherwise sit on a dead
    // socket until the keepalive's silence limit, which is the delay this is
    // all here to avoid. Closing also releases the forwarders feeding on them.
    for open in pool.values() {
        open.close();
    }
    result
}

/// Publish the household and keep it current until something interrupts.
async fn follow(
    session: &Session,
    pool: &mut HashMap<IpAddr, Connection>,
    established: Instant,
    restarts: &mut broadcast::Receiver<Restart>,
) -> Result<()> {
    let Session { connection, groups } = session;
    let household = connection.household_id().await?;

    let (mut tx, mut events) = mpsc::unbounded_channel();
    // Listen before subscribing, so the initial snapshot is not missed.
    let mut forwarders = vec![spawn_forwarder(connection, tx.clone(), Loss::Fatal)];
    connection
        .subscribe_household("groups:1", &household)
        .await?;

    let mut rooms = publish(connection, groups, pool, &tx, &mut forwarders).await?;

    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Some(event) => event,
                None => bail!("event channel closed"),
            },
            // The sockets did not survive whatever this was, even though they
            // still look open, so do not wait for them to say so. Unless they
            // postdate it: a change is reported a moment after it happened, and
            // a resume spends that moment retrying, so the reconnect it asked
            // for can already have landed. Sockets newer than the change are
            // the answer to it, not something to throw away.
            restart = restarted(restarts) => {
                if restart.at > established {
                    bail!("{}; reconnecting", restart.reason);
                }
                continue;
            }
        };
        if event.kind == Event::LOST {
            // Which one was said when it happened; any of them is fatal here.
            bail!("a player connection was lost; reconnecting");
        }

        match (event.namespace.as_str(), event.kind.as_str()) {
            ("groups:1", "groups") => {
                let groups: Groups = match serde_json::from_value(event.body.clone()) {
                    Ok(groups) => groups,
                    Err(e) => {
                        log(&format!("ignoring unparseable groups event: {e}"));
                        continue;
                    }
                };
                if same_topology(&rooms, &groups) {
                    continue;
                }
                log("group topology changed; republishing");
                for handle in forwarders.drain(..) {
                    handle.abort();
                }
                // The primary carries the household subscription and is still in
                // use; the rest are about to be replaced.
                for (ip, open) in std::mem::take(pool) {
                    if ip != connection.ip() {
                        open.close();
                    }
                }
                pool.insert(connection.ip(), connection.clone());
                // A fresh bus, not a drained one. Aborting a forwarder is not
                // synchronous: one mid-poll on another worker can still push
                // the LOST that `close` just provoked, and on the old channel
                // that would arrive after republish and tear the new players
                // straight down again. Holding an old sender, it now delivers
                // nowhere.
                (tx, events) = mpsc::unbounded_channel();
                forwarders.push(spawn_forwarder(connection, tx.clone(), Loss::Fatal));
                // Dropping the old servers releases their bus names first.
                drop(rooms);
                rooms = publish(connection, &groups, pool, &tx, &mut forwarders).await?;
            }
            // Player-scoped, so it is matched by player rather than by group.
            ("playerVolume:1", _) => {
                let Some(player_id) = event.player_id.as_deref() else {
                    continue;
                };
                let volume: proto::Volume = match serde_json::from_value(event.body.clone()) {
                    Ok(volume) => volume,
                    Err(e) => {
                        log(&format!("ignoring unparseable playerVolume event: {e}"));
                        continue;
                    }
                };
                for server in &rooms {
                    let properties = server.imp().apply_member_volume(player_id, &volume);
                    if properties.is_empty() {
                        continue;
                    }
                    if let Err(e) = server.properties_changed(properties).await {
                        log(&format!("{}: {e:#}", server.imp().room));
                    }
                }
            }
            ("playback:1" | "playbackMetadata:1" | "groupVolume:1", _) => {
                let Some(server) = rooms
                    .iter()
                    .find(|s| Some(&s.imp().group_id) == event.group_id.as_ref())
                else {
                    continue;
                };
                if let Err(e) = apply(server, &event).await {
                    log(&format!("{}: {e:#}", server.imp().room));
                }
            }
            _ => {}
        }
    }
}

/// Publish an MPRIS player per group, seeded with current state, and subscribe
/// to the events that will keep it current. Each group's calls and
/// subscriptions go to that group's coordinator (opening a new connection and
/// forwarder the first time a coordinator is seen, reused after that).
async fn publish(
    connection: &Connection,
    groups: &Groups,
    pool: &mut HashMap<IpAddr, Connection>,
    tx: &mpsc::UnboundedSender<Arc<Event>>,
    forwarders: &mut Vec<JoinHandle<()>>,
) -> Result<Vec<Server<RoomPlayer>>> {
    let mut servers = Vec::with_capacity(groups.groups.len());
    for group in &groups.groups {
        let coordinator_ip = groups.player(&group.coordinator_id).and_then(Player::ip);
        let conn = connection_to(
            coordinator_ip,
            connection,
            pool,
            tx,
            forwarders,
            Loss::Fatal,
        )
        .await?;

        let room = groups
            .player(&group.coordinator_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| group.name.clone());
        let members: Vec<(String, String)> = groups
            .members(group)
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        // The TV socket belongs to a player, which need not be the one
        // coordinating: a soundbar that joined a Play:5's group still has its
        // HDMI, and `x2rock tv` finds it among the members the same way.
        let has_tv_input = groups
            .members(group)
            .iter()
            .any(|p| p.capabilities.iter().any(|c| c == "HT_PLAYBACK"));
        let player = RoomPlayer::new(
            conn.clone(),
            group.id.clone(),
            room.clone(),
            members.clone(),
            has_tv_input,
        );
        player.apply_playback(&conn.playback_status(&group.id).await?);
        player.apply_metadata(&conn.metadata(&group.id).await?);
        player.apply_volume(&conn.group_volume(&group.id).await?);

        let suffix = bus_suffix(&room);
        let server = Server::new(&suffix, player)
            .await
            .with_context(|| format!("publishing org.mpris.MediaPlayer2.{suffix}"))?;
        for namespace in ["playback:1", "playbackMetadata:1", "groupVolume:1"] {
            conn.subscribe_group(namespace, &group.id).await?;
        }
        // Per-member volume is player-scoped, not group-scoped: a group shares
        // one volume, and this is the balance underneath it. Player-scoped
        // commands are refused by anyone but that player - ERROR_INVALID_OBJECT_ID,
        // "Incorrect playerId" - so each member is subscribed on its own socket
        // rather than the coordinator's.
        // Best effort, deliberately. A member that cannot be reached costs its
        // own balance slider; it must not cost the whole household its MPRIS
        // players, which is what a `?` here did - one bad member failed publish,
        // which tore down every socket and reconnected into the same failure.
        for (id, name) in &members {
            let Some(member_ip) = groups.player(id).and_then(Player::ip) else {
                log(&format!("{name}: no address, so no per-room volume for it"));
                continue;
            };
            match connection_to(
                Some(member_ip),
                connection,
                pool,
                tx,
                forwarders,
                Loss::Tolerated,
            )
            .await
            {
                Ok(member) => {
                    if let Err(e) = member.subscribe_player("playerVolume:1", id).await {
                        log(&format!("{name}: no per-room volume ({e:#})"));
                    }
                }
                Err(e) => log(&format!("{name}: could not be reached ({e:#})")),
            }
        }
        log(&format!("{room} -> org.mpris.MediaPlayer2.{suffix}"));
        servers.push(server);
    }
    Ok(servers)
}

/// A connection to one player, reusing the pool and opening only what is new.
/// Falls back to the connection already in hand when the address is unknown.
/// `loss` applies to a socket opened here; a pooled one keeps the terms it was
/// opened on, which is safe because coordinators are reached before members.
async fn connection_to(
    ip: Option<IpAddr>,
    primary: &Connection,
    pool: &mut HashMap<IpAddr, Connection>,
    tx: &mpsc::UnboundedSender<Arc<Event>>,
    forwarders: &mut Vec<JoinHandle<()>>,
    loss: Loss,
) -> Result<Connection> {
    let Some(ip) = ip.filter(|ip| *ip != primary.ip()) else {
        return Ok(primary.clone());
    };
    if let Some(existing) = pool.get(&ip) {
        return Ok(existing.clone());
    }
    let opened = Connection::open(ip).await?;
    forwarders.push(spawn_forwarder(&opened, tx.clone(), loss));
    pool.insert(ip, opened.clone());
    Ok(opened)
}

/// The initial `groups` snapshot describes what we just published; republishing
/// for it would flap every bus name once per connection.
fn same_topology(rooms: &[Server<RoomPlayer>], groups: &Groups) -> bool {
    rooms.len() == groups.groups.len()
        && groups.groups.iter().all(|g| {
            rooms
                .iter()
                // Resolved the way `publish` resolved them: a player id the
                // snapshot cannot name (transiently, while a player rejoins)
                // is absent from both sides, rather than making every
                // snapshot look like a change.
                .any(|s| {
                    s.imp().group_id == g.id
                        && s.imp()
                            .member_ids()
                            .iter()
                            .eq(groups.members(g).iter().map(|p| &p.id))
                })
        })
}

async fn apply(server: &Server<RoomPlayer>, event: &Arc<Event>) -> Result<()> {
    let player = server.imp();
    let body = event.body.clone();
    // Under X2ROCK_LOG_EVENTS, the body exactly as it arrived. A partial
    // `playbackStatus` - one with no `playbackState` - is now folded in
    // silently and leaves no other trace, so this is the only way to catch one
    // in the act. Logged before the match, so a body that still fails to parse
    // is shown too.
    if log_events() {
        log(&format!(
            "{}: {} {}",
            player.room, event.namespace, event.body
        ));
    }
    let properties = match event.namespace.as_str() {
        "playback:1" => {
            // The namespace delivers failures as well as statuses, and a
            // `playbackError` deserializes *cleanly* into a `PlaybackStatus`
            // whose every field is `None` - so unless it is told apart here it
            // folds in as "nothing changed" and the only notice that a stream
            // died is thrown away. Logged rather than published: MPRIS has no
            // property for "that did not play", and the journal is where the
            // answer to "why did the music stop overnight" has to be.
            if let Some(error) = proto::playback_error(&event.body) {
                log(&format!("{}: playback failed: {error}", player.room));
                return Ok(());
            }
            let mut properties = player.apply_playback(&serde_json::from_value(body)?);
            // The queue's version has to be fetched rather than read off the
            // event, because the players do not send one - see
            // `RoomPlayer::refresh_queue_version`. A fresher Metadata supersedes
            // whatever apply_playback built, rather than being sent beside it.
            if let Some(metadata) = player.refresh_queue_version().await {
                properties.retain(|p| !matches!(p, Property::Metadata(_)));
                properties.push(metadata);
            }
            properties
        }
        "playbackMetadata:1" => {
            let status: proto::MetadataStatus = serde_json::from_value(body)?;
            remember(&status);
            player.apply_metadata(&status)
        }
        "groupVolume:1" => player.apply_volume(&serde_json::from_value(body)?),
        _ => return Ok(()),
    };
    server.properties_changed(properties).await?;
    Ok(())
}

/// Note what just started playing, so it can be played again later.
///
/// **Nothing here may fail the caller.** This is the daemon, whose job is MPRIS
/// and transport, and a history is a convenience: every error is logged and
/// swallowed, and the `?`-free body is deliberate. A full disk must cost a line
/// in the journal, not a room that will no longer pause.
///
/// Cheap by construction too: the store is only rewritten when the object id
/// actually changes, so a track playing for four minutes writes once.
fn remember(status: &proto::MetadataStatus) {
    let Some(track) = status.current_item.as_ref().and_then(|i| i.track.as_ref()) else {
        return;
    };
    let (Some(id), Some(name)) = (track.id.as_ref(), track.name.as_deref()) else {
        return;
    };
    let Ok(mut bookmark) = crate::bookmarks::Bookmark::from_id(name, id) else {
        // A live stream has no id worth storing; that is normal, not an error.
        return;
    };
    bookmark.artist = track.artist.as_ref().and_then(|a| a.name.clone());
    bookmark.art_url = track.image_url.clone();
    bookmark.kind = Some("track".into());
    bookmark.service_name = status
        .container
        .as_ref()
        .and_then(|c| c.service.as_ref())
        .and_then(|s| s.name.clone());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut list = match crate::bookmarks::Bookmarks::load() {
        Ok(list) => list,
        Err(e) => {
            log(&format!("not recording history: {e:#}"));
            return;
        }
    };
    if !list.note(bookmark, now) {
        return;
    }
    if let Err(e) = list.save() {
        log(&format!("could not write history: {e:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_status_is_silent_until_the_heartbeat_then_counts_the_silence() {
        let t0 = Instant::now();
        let mut s = StatusLog::new(false);
        // First sighting of a status logs.
        assert_eq!(s.decide("unreg|net-a".into(), t0), Some(Emit::Fresh));
        // Same status inside the window says nothing, however many passes.
        assert_eq!(
            s.decide("unreg|net-a".into(), t0 + Duration::from_secs(60)),
            None
        );
        assert_eq!(
            s.decide("unreg|net-a".into(), t0 + Duration::from_secs(120)),
            None
        );
        // Past the window, one line, counting itself plus the two it held.
        assert_eq!(
            s.decide(
                "unreg|net-a".into(),
                t0 + HEARTBEAT + Duration::from_secs(1)
            ),
            Some(Emit::Heartbeat(3))
        );
        // And the window starts over from that heartbeat.
        assert_eq!(
            s.decide(
                "unreg|net-a".into(),
                t0 + HEARTBEAT + Duration::from_secs(2)
            ),
            None
        );
    }

    #[test]
    fn a_network_switch_flushes_at_once_without_waiting_for_the_heartbeat() {
        let t0 = Instant::now();
        let mut s = StatusLog::new(false);
        assert_eq!(s.decide("unreg|net-a".into(), t0), Some(Emit::Fresh));
        assert_eq!(
            s.decide("unreg|net-a".into(), t0 + Duration::from_secs(1)),
            None
        );
        // The fingerprint is in the key, so moving to another network is a
        // different status and logs immediately - a move is worth seeing.
        assert_eq!(
            s.decide("unreg|net-b".into(), t0 + Duration::from_secs(2)),
            Some(Emit::Fresh)
        );
    }

    #[test]
    fn reset_makes_the_next_identical_status_log_rather_than_coalesce() {
        let t0 = Instant::now();
        let mut s = StatusLog::new(false);
        assert_eq!(s.decide("fail|net-a".into(), t0), Some(Emit::Fresh));
        // A connection came and went; the failure that follows must not be read
        // as a continuation of the run before it.
        s.reset();
        assert_eq!(
            s.decide("fail|net-a".into(), t0 + Duration::from_secs(1)),
            Some(Emit::Fresh)
        );
    }

    #[test]
    fn verbose_logs_every_pass_with_no_coalescing() {
        let t0 = Instant::now();
        let mut s = StatusLog::new(true);
        // The same status, back to back inside the window, still logs each time -
        // never suppressed, never folded into a heartbeat.
        assert_eq!(s.decide("unreg|net-a".into(), t0), Some(Emit::Fresh));
        assert_eq!(
            s.decide("unreg|net-a".into(), t0 + Duration::from_secs(1)),
            Some(Emit::Fresh)
        );
        assert_eq!(
            s.decide("unreg|net-a".into(), t0 + Duration::from_secs(2)),
            Some(Emit::Fresh)
        );
    }
}
