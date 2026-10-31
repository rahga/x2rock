//! The household, live: one tokio runtime on its own thread that finds the
//! speakers, subscribes, folds every pushed event into a [`Household`] and
//! hands each new snapshot to the UI. Commands go the other way on a channel.
//!
//! The shape is the x2rock daemon's `follow` loop (its `daemon.rs`), with the
//! MPRIS players replaced by snapshots: `groups:1` on one player, each group's
//! playback, metadata and volume on its coordinator's own socket, each
//! speaker's volume on that speaker's socket. Push, not poll: a command's
//! effect arrives as an event, and nothing re-reads after acting.
//!
//! Commands run in order per room or speaker, and reads beside them (see
//! `order` and [`crate::lanes`]).

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast::error::RecvError, mpsc, watch};

use x2rock::session::{self, Session};
use x2rock::sonos::local::{ApiError, Connection};
use x2rock::sonos::proto::{self, Event, Groups, HomeTheaterUpdate, Repeat};
use x2rock::sonos::upnp::Upnp;
use x2rock::state::State;
use x2rock::{discover, netid};

use crate::lanes::{self, Order};
use crate::model::{GroupLive, Household};

/// Where finding the household has got to, for the room list to say.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Searching,
    Connecting,
    Connected,
    /// Lost, and trying again; the reason is said once.
    Reconnecting(String),
}

/// What the runtime tells the UI.
pub enum Update {
    Status(Status),
    Household(Box<Household>),
    /// Something to say once, at the foot of the screen.
    Notice(String),
    /// A command the household refused or that failed: said like a notice, and
    /// kept as the last refusal for Send a report.
    Refusal(String),
    /// What a room's panel reads when it opens: none of it is pushed.
    RoomDetails(RoomDetails),
    /// A list a screen asked for.
    Fetched(Fetched),
}

/// A list a screen asks for: none of these is pushed.
#[derive(Clone, Debug, PartialEq)]
pub enum Fetch {
    Queue {
        id: Option<u64>,
        group: String,
    },
    Favorites {
        id: Option<u64>,
    },
    Playlists {
        id: Option<u64>,
    },
    /// Radio Browser: by votes with neither, else by tag or country code.
    Stations {
        id: Option<u64>,
        tag: Option<String>,
        country: Option<String>,
    },
    /// The music services this household can use.
    Services {
        id: Option<u64>,
    },
    /// One search across every service: answered a section at a time.
    Search {
        id: Option<u64>,
        term: String,
    },
    /// A page of a service; `root` is its front page.
    ServicePage {
        id: Option<u64>,
        service: String,
        container: String,
        index: u32,
    },
    /// Every room's alarms, and the household's clock.
    Alarms {
        id: Option<u64>,
    },
    /// Every speaker: its model, its version, and whether an update waits.
    About {
        id: Option<u64>,
    },
    /// How the household's stored sign-ins read, reading them if nothing has:
    /// for Send a report.
    SignIns {
        id: Option<u64>,
    },
    /// Which ratings the group's track can be given now.
    RatingOffers {
        group: String,
        track: String,
    },
    /// What the household played lately.
    History {
        id: Option<u64>,
    },
    /// The Sonos favourites an alarm can play.
    AlarmSounds {
        id: Option<u64>,
    },
    /// Whether a service's song, album or playlist is a Sonos favourite.
    IsFavorite {
        service: String,
        item: x2rock::sonos::smapi::Item,
    },
    /// What plays before the room's current track, read from its queue: the
    /// player names what comes next but never what came before. `key` is the
    /// track and queue version it was asked for.
    Previous {
        group: String,
        key: String,
    },
    /// Where a book's listener left off: the chapter, and the place in it.
    Resume {
        service: String,
        book: String,
    },
}

/// One speaker, as About this system lists it.
#[derive(Clone, Debug)]
pub struct SpeakerInfo {
    pub id: String,
    pub room: String,
    /// The firmware's own number, "97.1-80312", beside `version`'s "18.8".
    pub software: String,
    pub model: String,
    pub version: String,
    pub update_waiting: bool,
    pub satellite: bool,
}

/// A queue as the Queue screen shows it.
#[derive(Clone, Debug, Default)]
pub struct QueueView {
    pub items: Vec<x2rock::sonos::upnp::QueueItem>,
    pub total: u32,
    pub update_id: String,
    /// 1-based, while the room plays from its queue.
    pub current: Option<u32>,
}

/// What came back; the error already worded for the screen.
#[derive(Clone, Debug)]
pub enum Fetched {
    Queue {
        id: Option<u64>,
        group: String,
        result: Result<QueueView, String>,
    },
    Favorites {
        id: Option<u64>,
        result: Result<Vec<proto::Favorite>, String>,
    },
    Playlists {
        id: Option<u64>,
        result: Result<Vec<proto::Playlist>, String>,
    },
    Stations {
        id: Option<u64>,
        fetch: Fetch,
        result: Result<Vec<x2rock::stations::Station>, String>,
    },
    Services {
        id: Option<u64>,
        result: Result<Vec<crate::services::ServiceInfo>, String>,
    },
    SearchSection {
        id: Option<u64>,
        term: String,
        section: crate::services::Section,
    },
    SearchDone {
        id: Option<u64>,
        term: String,
    },
    ServicePage {
        id: Option<u64>,
        service: String,
        container: String,
        index: u32,
        result: Result<(Vec<x2rock::sonos::smapi::Item>, u32), String>,
    },
    /// The alarms, and the household's clock with whether it knows its timezone.
    Alarms {
        id: Option<u64>,
        result: Result<(Vec<x2rock::sonos::upnp::Alarm>, String, bool), String>,
    },
    About {
        id: Option<u64>,
        result: Result<Vec<SpeakerInfo>, String>,
    },
    SignIns {
        id: Option<u64>,
        result: Option<Result<usize, String>>,
    },
    AlarmSounds {
        id: Option<u64>,
        result: Result<Vec<AlarmProgram>, String>,
    },
    /// Unanswered (no UPnP, no services) says nothing, and the menu offers neither.
    IsFavorite {
        item_id: String,
        yes: bool,
    },
    /// The track before the current one in the room's queue: title and artist.
    Previous {
        group: String,
        key: String,
        track: Option<(String, Option<String>)>,
    },
    /// The chapter a book resumes at, if its service keeps a place.
    Resume {
        book: String,
        chapter: Option<String>,
    },
    /// How an alarm write went: the editor closes on success, and stays open,
    /// with the refusal said, otherwise.
    /// `alarm` is the alarm written, `None` for a new one: the answer closes
    /// only the editor that sent it.
    AlarmWritten {
        alarm: Option<u32>,
        result: Result<(), String>,
    },
    RatingOffers {
        group: String,
        track: String,
        offers: Option<x2rock::play::RatingOffers>,
    },
    History {
        id: Option<u64>,
        result: Result<Vec<proto::HistoryItem>, String>,
    },
}

impl Fetched {
    pub fn request_id(&self) -> Option<u64> {
        match self {
            Fetched::Queue { id, .. }
            | Fetched::Favorites { id, .. }
            | Fetched::Playlists { id, .. }
            | Fetched::Stations { id, .. }
            | Fetched::Services { id, .. }
            | Fetched::SearchSection { id, .. }
            | Fetched::SearchDone { id, .. }
            | Fetched::ServicePage { id, .. }
            | Fetched::Alarms { id, .. }
            | Fetched::About { id, .. }
            | Fetched::SignIns { id, .. }
            | Fetched::AlarmSounds { id, .. }
            | Fetched::History { id, .. } => *id,
            Fetched::IsFavorite { .. }
            | Fetched::Previous { .. }
            | Fetched::Resume { .. }
            | Fetched::AlarmWritten { .. }
            | Fetched::RatingOffers { .. } => None,
        }
    }
}

/// A room's own settings, read over UPnP for its panel. Each part is `None`
/// where the speaker did not answer for it (UPnP off, or not that kind of
/// speaker), so the panel leaves that row out rather than guessing.
#[derive(Clone, Debug, Default)]
pub struct RoomDetails {
    pub group: String,
    /// The speaker these were read from: the room's coordinator.
    pub player: String,
    pub sleep_left: Option<Duration>,
    pub sleep_read: bool,
    pub tone: Option<x2rock::sonos::upnp::Tone>,
    pub status_light: Option<bool>,
    pub buttons_locked: Option<bool>,
    /// A Sub's and surrounds' settings, where the room has them.
    pub satellites: Option<Satellites>,
    /// The speaker's volume limit, where one below 100 is set: said, since it
    /// explains a room that seems too quiet, and not set from here.
    pub volume_limit: Option<u8>,
    /// The room's soundbar's TV-remote settings, by the soundbar's player id:
    /// written to the soundbar wherever it sits in the group.
    pub remote: Option<(String, x2rock::sonos::upnp::RemoteSettings)>,
    /// A portable's pack; a mains speaker reports none.
    pub battery: Option<x2rock::sonos::upnp::Battery>,
    pub read_at: Option<Instant>,
}

/// The Sonos app's Sub audio and Surround audio: RenderingControl EQ types on
/// the room's own player, the soundbar. Levels run -15..15.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Satellites {
    /// The speaker they are read from and written to: the room's own.
    pub player: String,
    pub sub_on: Option<bool>,
    pub sub_level: Option<i32>,
    pub surrounds_on: Option<bool>,
    /// While the room plays its TV: the Sonos app's "TV Level".
    pub tv_level: Option<i32>,
    /// For music: the Sonos app's "Music Level".
    pub music_level: Option<i32>,
}

/// What an alarm plays other than the chime: a Sonos favourite's resource,
/// its metadata carried as the player gave it.
#[derive(Clone, Debug, PartialEq)]
pub struct AlarmProgram {
    pub title: String,
    pub uri: String,
    pub metadata: String,
}

/// What the alarm editor sets. An alarm changed keeps everything else as the
/// player has it, read fresh just before the write.
#[derive(Clone, Debug, PartialEq)]
pub struct AlarmFields {
    /// `HH:MM:SS` on the household's clock.
    pub start: String,
    pub recurrence: String,
    /// The speaker it rings on.
    pub room: String,
    /// `None` for the chime.
    pub program: Option<AlarmProgram>,
    pub volume: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AlarmWrite {
    /// A new alarm, armed.
    Create(AlarmFields),
    Change {
        id: u32,
        fields: AlarmFields,
    },
    Remove(u32),
}

impl AlarmWrite {
    /// The alarm written, `None` for a new one.
    pub fn alarm(&self) -> Option<u32> {
        match self {
            AlarmWrite::Create(_) => None,
            AlarmWrite::Change { id, .. } | AlarmWrite::Remove(id) => Some(*id),
        }
    }
}

/// The chime every speaker has, as an alarm's program.
pub const CHIME: &str = "x-rincon-buzzer:0";

/// What the UI asks of the household. Group commands name the group;
/// speaker commands the player.
#[derive(Clone, Debug)]
pub enum Command {
    Playback {
        group: String,
        command: &'static str,
    },
    SeekTo {
        group: String,
        millis: u64,
    },
    SeekBy {
        group: String,
        millis: i64,
    },
    SetGroupVolume {
        group: String,
        level: u8,
    },
    SetGroupMute {
        group: String,
        muted: bool,
    },
    AdjustPlayerVolume {
        player: String,
        delta: i8,
    },
    SetPlayerMute {
        player: String,
        muted: bool,
    },
    SetShuffle {
        group: String,
        on: bool,
    },
    SetRepeat {
        group: String,
        repeat: Repeat,
    },
    SetNightSound {
        player: String,
        on: bool,
    },
    SetSpeechEnhancement {
        player: String,
        on: bool,
    },
    /// Bring players into a group, or take them out of it.
    ModifyGroup {
        group: String,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Every other room into this group; with `level`, every speaker set to it
    /// first, so nothing joins loud.
    PlayEverywhere {
        group: String,
        level: Option<u8>,
    },
    /// Every group back to rooms of their own.
    SplitAll,
    /// The group onto its soundbar's TV input: sent to the coordinator, naming
    /// the soundbar.
    UseTvInput {
        group: String,
        soundbar: String,
    },
    /// The group onto a speaker's analog line-in: `playback:1 loadLineIn` to the
    /// coordinator, naming the speaker whose input it is. Untested on hardware.
    UseLineIn {
        group: String,
        player: String,
    },
    SetSleep {
        group: String,
        minutes: Option<u32>,
    },
    SetCrossfade {
        group: String,
        on: bool,
    },
    SetBass {
        player: String,
        level: i8,
    },
    SetTreble {
        player: String,
        level: i8,
    },
    SetLoudness {
        player: String,
        on: bool,
    },
    SetTrueplay {
        player: String,
        on: bool,
    },
    SetStatusLight {
        player: String,
        on: bool,
    },
    SetButtonsLocked {
        player: String,
        locked: bool,
    },
    /// A soundbar flashing its light at the TV remote.
    SetRemoteFeedback {
        player: String,
        on: bool,
    },
    /// A soundbar passing the TV remote's IR on to the TV.
    SetIrRepeater {
        player: String,
        on: bool,
    },
    /// A numeric EQ setting on a soundbar: SubEnable, SubGain, SurroundEnable,
    /// SurroundLevel or MusicSurroundLevel.
    SetEqValue {
        player: String,
        eq_type: &'static str,
        value: i32,
    },
    /// What the room plays, saved to the household's Sonos favourites unless
    /// it is one already.
    SaveFavorite {
        group: String,
    },
    /// Read the household's stored music-service sign-ins into this machine.
    ImportSignIns,
    /// A service's song, album or playlist into the household's Sonos
    /// favourites, or out of them; then the favourites read again.
    ServiceFavorite {
        service: String,
        item: x2rock::sonos::smapi::Item,
        add: bool,
    },
    /// A Sonos favourite out, by the id `getFavorites` gives it; then the
    /// favourites read again.
    RemoveFavorite {
        id: String,
        name: String,
    },
    /// Play a recently played item again.
    Replay {
        group: String,
        item: proto::HistoryItem,
    },
    /// Rate the group's track: up, down, or a heart taken back.
    Rate {
        group: String,
        kind: x2rock::sonos::smapi::RatingKind,
    },
    /// Bring a preset back: levels first, then the rooms together, then its
    /// favourite.
    ApplyPreset(crate::prefs::Preset),
    /// Arm or disarm an alarm: written back whole with only that changed.
    SetAlarmEnabled {
        alarm: x2rock::sonos::upnp::Alarm,
        enabled: bool,
    },
    /// Make, change or remove an alarm: answered as `AlarmWritten`, then the
    /// alarms read again.
    WriteAlarm(AlarmWrite),
    /// Read a room's settings for its panel; answered as RoomDetails.
    ReadRoom {
        group: String,
        player: String,
    },
    Fetch(Fetch),
    /// Play the queue from this track (1-based), switching the room back to
    /// its queue first if a station had replaced it.
    PlayQueueTrack {
        group: String,
        index: u32,
    },
    RemoveFromQueue {
        group: String,
        index: u32,
        update_id: String,
    },
    ClearQueue {
        group: String,
    },
    PlayFavorite {
        group: String,
        id: String,
    },
    PlayPlaylist {
        group: String,
        id: String,
    },
    /// A stream by its URL, as a station: in a playback session, so the room's
    /// queue is left as it was.
    PlayStream {
        group: String,
        url: String,
        title: String,
    },
    /// A service's item, by the CLI's play routes.
    PlayServiceItem {
        group: String,
        service: String,
        item: x2rock::sonos::smapi::Item,
    },
    /// A service's item into the queue: next, or at the end.
    QueueServiceItem {
        group: String,
        service: String,
        item: x2rock::sonos::smapi::Item,
        next: bool,
    },
    /// Look again from scratch: the network changed, or the viewer asked.
    Reconnect,
    /// The search under way is no longer wanted: the Search screen closed.
    CancelSearch,
    /// An edit, then a read of what it changed, in that order: nothing pushes
    /// a queue or an alarm, and a read runs beside the edits rather than in
    /// their lane, so one sent after the edit could land first and show the
    /// list as it was.
    EditThenRead {
        edit: Box<Command>,
        read: Fetch,
    },
}

/// The UI's end of the runtime.
#[derive(Clone)]
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
}

impl Handle {
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

/// Start following the household on `runtime`. `updates` is called on the
/// runtime's threads; the caller moves each update to the UI thread itself.
pub fn start(
    runtime: &tokio::runtime::Handle,
    updates: impl Fn(Update) + Send + Sync + 'static,
) -> Handle {
    let (commands, mut inbox) = mpsc::unbounded_channel();
    let updates: Arc<dyn Fn(Update) + Send + Sync> = Arc::new(updates);
    // Weak, so the inbox still closes when the UI's handle goes.
    let requeue = commands.downgrade();
    runtime.spawn(async move { run(updates, &mut inbox, requeue).await });
    Handle { commands }
}

type Sink = Arc<dyn Fn(Update) + Send + Sync>;

/// Connect, follow until something breaks, and connect again, backing off up
/// to a minute between attempts that fail.
async fn run(
    updates: Sink,
    inbox: &mut mpsc::UnboundedReceiver<Command>,
    requeue: mpsc::WeakUnboundedSender<Command>,
) {
    session::silence_progress();
    let mut backoff = Duration::from_secs(2);
    let mut first = true;
    // A press made while not connected is answered now, not sent once connected
    // seconds later, after the viewer has given up on it. A read waits, through
    // failed attempts too, for the connection that answers it.
    let mut reads = Vec::new();
    let not_now = |command: Command, reads: &mut Vec<Command>| match command {
        Command::Reconnect => {}
        read @ (Command::Fetch(_) | Command::ReadRoom { .. } | Command::CancelSearch) => {
            hold(reads, read)
        }
        // The alarm editor waits for its answer: it must get one.
        Command::WriteAlarm(write) => updates(Update::Fetched(Fetched::AlarmWritten {
            alarm: write.alarm(),
            result: Err("Not connected to the speakers yet".into()),
        })),
        _ => updates(Update::Notice("Not connected to the speakers yet".into())),
    };
    loop {
        updates(Update::Status(if first {
            Status::Searching
        } else {
            Status::Connecting
        }));
        let connected = {
            let connecting = connect();
            tokio::pin!(connecting);
            loop {
                tokio::select! {
                    result = &mut connecting => break result,
                    command = inbox.recv() => match command {
                        None => return,
                        Some(command) => not_now(command, &mut reads),
                    },
                }
            }
        };
        let outcome = match connected {
            Ok(session) => {
                if let Some(requeue) = requeue.upgrade() {
                    for read in reads.drain(..) {
                        let _ = requeue.send(read);
                    }
                }
                backoff = Duration::from_secs(2);
                updates(Update::Status(Status::Connected));
                let result = follow(&session, &updates, inbox).await;
                session.close().await;
                result
            }
            Err(e) => Err(e),
        };
        first = false;
        let reason = match outcome {
            Ok(()) => "Looking again".to_owned(),
            Err(e) => plain(&e),
        };
        updates(Update::Status(Status::Reconnecting(reason)));
        // Wait out the backoff, but a Reconnect asked for meanwhile goes now.
        let deadline = tokio::time::sleep(backoff);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                command = inbox.recv() => match command {
                    Some(Command::Reconnect) => break,
                    Some(command) => not_now(command, &mut reads),
                    None => return,
                },
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// A read kept for the connection that will answer it. Of searches, only the
/// latest is worth asking, and none once Search has closed: the screen sent its
/// one CancelSearch while there was nothing yet to cancel.
fn hold(reads: &mut Vec<Command>, read: Command) {
    let search = |c: &Command| matches!(c, Command::Fetch(Fetch::Search { .. }));
    if search(&read) || matches!(read, Command::CancelSearch) {
        reads.retain(|r| !search(r));
    }
    if !matches!(read, Command::CancelSearch) {
        reads.push(read);
    }
}

/// A household to talk to: the remembered one on this network, else whatever
/// a sweep of the local subnet finds. The CLI never scans on its own; an app
/// someone has just opened must, or it would open on an empty list.
async fn connect() -> Result<Session> {
    let mut state = State::load().unwrap_or_default();
    let fingerprint = netid::network_fingerprint();
    if let Some(fp) = fingerprint.as_deref()
        && !state.households_on(fp).is_empty()
        && let Ok(session) = session::connect(None, &mut state, None, None).await
    {
        return Ok(session);
    }
    // SSDP first: quick, and it reaches a subnet wider than the sweep covers.
    // Behind a firewall that drops its replies it finds nothing, and the sweep
    // of the local subnet is the second try.
    let mut addresses = discover::ssdp(Duration::from_secs(2))
        .await
        .unwrap_or_default();
    if addresses.is_empty() {
        addresses = discover::scan_local_subnet()
            .await
            .context("looking for speakers")?
            .found;
    }
    if addresses.is_empty() {
        bail!("No Sonos speakers answered on this network");
    }
    let mut found =
        session::discover_households(&addresses, &mut state, fingerprint.as_deref()).await?;
    // The household with the most rooms, where a network has more than one.
    found.sort_by_key(|f| std::cmp::Reverse(f.session.groups.players.len()));
    let mut found = found.into_iter();
    let chosen = found
        .next()
        .ok_or_else(|| anyhow!("No Sonos household answered"))?;
    for other in found {
        other.session.connection.close();
    }
    Ok(chosen.session)
}

/// Every socket's events, tagged with the address they came from: group events
/// are taken from the coordinator's socket alone (a member leaving a party
/// answers for the group it is leaving).
type Tagged = (IpAddr, Arc<Event>);

struct Wiring {
    events: mpsc::UnboundedSender<Tagged>,
    /// Sockets with a forwarder on them, by address.
    forwarded: Arc<std::sync::Mutex<HashSet<IpAddr>>>,
    /// Groups subscribed, by id, and the coordinator address they were subscribed on.
    groups: HashMap<String, IpAddr>,
    /// Players whose own volume is subscribed.
    players: HashSet<String>,
}

fn forward(connection: &Connection, events: mpsc::UnboundedSender<Tagged>) {
    let mut rx = connection.events();
    let ip = connection.ip();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let lost = event.kind == Event::LOST;
                    if events.send((ip, event)).is_err() || lost {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        }
    });
}

enum WireUpdate {
    Group {
        id: String,
        ip: IpAddr,
        live: Box<crate::model::GroupLive>,
    },
    Player {
        id: String,
        volume: Option<proto::Volume>,
        ht: Option<(bool, bool)>,
    },
    Done,
}

async fn reach_speaker(
    session: &Session,
    ip: IpAddr,
    events_tx: &mpsc::UnboundedSender<Tagged>,
    forwarded: &Arc<std::sync::Mutex<HashSet<IpAddr>>>,
    failed: &Arc<std::sync::Mutex<HashSet<IpAddr>>>,
) -> Result<Connection> {
    let needs_probe = !forwarded.lock().unwrap().contains(&ip);
    if needs_probe {
        if failed.lock().unwrap().contains(&ip) {
            bail!("{ip} did not answer");
        }
        let probe = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::TcpStream::connect((ip, 1443)),
        )
        .await;
        if !matches!(probe, Ok(Ok(_))) {
            failed.lock().unwrap().insert(ip);
            bail!("{ip} did not answer");
        }
    }
    let connection = session.player(Some(ip)).await?;
    if forwarded.lock().unwrap().insert(connection.ip()) {
        forward(&connection, events_tx.clone());
    }
    Ok(connection)
}

fn unwired_targets(
    wiring: &Wiring,
    groups: &Groups,
) -> (Vec<(proto::Group, IpAddr)>, Vec<proto::Player>) {
    let mut groups_to_wire = Vec::new();
    for g in &groups.groups {
        let coordinator_ip = groups.player(&g.coordinator_id).and_then(proto::Player::ip);
        if let Some(ip) = coordinator_ip
            && wiring.groups.get(&g.id) != Some(&ip)
        {
            groups_to_wire.push((g.clone(), ip));
        }
    }
    let mut players_to_wire = Vec::new();
    for p in &groups.players {
        if p.ip().is_some() && !wiring.players.contains(&p.id) {
            players_to_wire.push(p.clone());
        }
    }
    (groups_to_wire, players_to_wire)
}

fn spawn_wire(
    session: Session,
    events_tx: mpsc::UnboundedSender<Tagged>,
    wire_tx: mpsc::UnboundedSender<WireUpdate>,
    unwired_groups: Vec<(proto::Group, IpAddr)>,
    unwired_players: Vec<proto::Player>,
    forwarded: Arc<std::sync::Mutex<HashSet<IpAddr>>>,
) -> tokio::task::AbortHandle {
    let task = tokio::spawn(async move {
        let failed = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let now = Instant::now();

        // Wire groups concurrently:
        let mut group_futures = Vec::new();
        for (group, ip) in unwired_groups {
            let session = session.clone();
            let events_tx = events_tx.clone();
            let forwarded = forwarded.clone();
            let failed = failed.clone();
            let wire_tx = wire_tx.clone();
            group_futures.push(async move {
                let res: Result<()> = async {
                    let conn = reach_speaker(&session, ip, &events_tx, &forwarded, &failed).await?;
                    let (playback, metadata, volume) = tokio::try_join!(
                        conn.playback_status(&group.id),
                        conn.metadata(&group.id),
                        conn.group_volume(&group.id),
                    )?;
                    let mut live = crate::model::GroupLive::default();
                    live.apply_playback(&playback, now);
                    live.metadata = Some(metadata);
                    live.volume = Some(volume);
                    tokio::try_join!(
                        conn.subscribe_group("playback:1", &group.id),
                        conn.subscribe_group("playbackMetadata:1", &group.id),
                        conn.subscribe_group("groupVolume:1", &group.id),
                    )?;
                    let _ = conn.subscribe_group("sleepTimer:1", &group.id).await;
                    let _ = wire_tx.send(WireUpdate::Group {
                        id: group.id.clone(),
                        ip,
                        live: Box::new(live),
                    });
                    Ok(())
                }
                .await;
                if let Err(e) = res {
                    eprintln!("x2rock-deck: {}: not followed ({e:#})", group.name);
                }
            });
        }
        futures_util::future::join_all(group_futures).await;

        // Wire players concurrently:
        let mut player_futures = Vec::new();
        for player in unwired_players {
            let Some(ip) = player.ip() else { continue };
            let session = session.clone();
            let events_tx = events_tx.clone();
            let forwarded = forwarded.clone();
            let failed = failed.clone();
            let wire_tx = wire_tx.clone();
            player_futures.push(async move {
                let res: Result<()> = async {
                    let conn = reach_speaker(&session, ip, &events_tx, &forwarded, &failed).await?;
                    let volume = conn.player_volume(&player.id).await.ok();
                    conn.subscribe_player("playerVolume:1", &player.id).await?;
                    let mut ht = None;
                    if player.has_tv() {
                        if let Ok(settings) = conn.player_settings(&player.id).await
                            && let Some(h) = settings.home_theater
                        {
                            ht = Some((h.night_mode, h.enhance_dialog));
                        }
                        let _ = conn.subscribe_player("homeTheater:1", &player.id).await;
                    }
                    let _ = wire_tx.send(WireUpdate::Player {
                        id: player.id.clone(),
                        volume,
                        ht,
                    });
                    Ok(())
                }
                .await;
                if let Err(e) = res {
                    eprintln!("x2rock-deck: {}: no speaker volume ({e:#})", player.name);
                }
            });
        }
        futures_util::future::join_all(player_futures).await;

        let _ = wire_tx.send(WireUpdate::Done);
    });
    task.abort_handle()
}

/// Whether the topology has a group or a reachable speaker that is not yet
/// subscribed.
fn unwired(wiring: &Wiring, household: &Household) -> bool {
    let Some(groups) = household.groups.as_ref() else {
        return false;
    };
    let group_unwired = groups.groups.iter().any(|g| {
        let ip = groups.player(&g.coordinator_id).and_then(proto::Player::ip);
        ip.is_some() && wiring.groups.get(&g.id) != ip.as_ref()
    });
    group_unwired
        || groups
            .players
            .iter()
            .any(|p| p.ip().is_some() && !wiring.players.contains(&p.id))
}

/// Publish the household and keep it current until a socket that matters is
/// lost. Returns `Ok` when asked to look again.
async fn follow(
    session: &Session,
    updates: &Sink,
    inbox: &mut mpsc::UnboundedReceiver<Command>,
) -> Result<()> {
    let primary = session.connection.clone();
    let household_id = primary.household_id().await?;
    let (events_tx, events) = mpsc::unbounded_channel::<Tagged>();
    let forwarded = Arc::new(std::sync::Mutex::new(HashSet::new()));
    forwarded.lock().unwrap().insert(primary.ip());
    forward(&primary, events_tx.clone());
    primary
        .subscribe_household("groups:1", &household_id)
        .await?;
    // Bonded speakers dropping off and coming back: best effort, a household
    // without bonds has nothing to say here.
    let _ = primary.subscribe_household("zones:1", &household_id).await;

    let wiring = Wiring {
        events: events_tx,
        forwarded,
        groups: HashMap::new(),
        players: HashSet::new(),
    };
    let household = Household {
        groups: Some(session.groups.clone()),
        ..Default::default()
    };
    follow_loop(
        session,
        primary.ip(),
        household,
        wiring,
        events,
        updates,
        inbox,
    )
    .await
}

async fn follow_loop(
    session: &Session,
    primary_ip: IpAddr,
    mut household: Household,
    mut wiring: Wiring,
    mut events: mpsc::UnboundedReceiver<Tagged>,
    updates: &Sink,
    inbox: &mut mpsc::UnboundedReceiver<Command>,
) -> Result<()> {
    updates(Update::Household(Box::new(household.clone())));

    let (wire_tx, mut wire_rx) = mpsc::unbounded_channel::<WireUpdate>();
    let mut wiring_task = Latest::default();

    if let Some(groups) = household.groups.as_ref() {
        let (groups_to_wire, players_to_wire) = unwired_targets(&wiring, groups);
        if !groups_to_wire.is_empty() || !players_to_wire.is_empty() {
            wiring_task.replace(Some(spawn_wire(
                session.clone(),
                wiring.events.clone(),
                wire_tx.clone(),
                groups_to_wire,
                players_to_wire,
                wiring.forwarded.clone(),
            )));
        }
    }

    // A group or speaker that could not be subscribed is tried again, with the
    // reconnect's backoff, until it is or it leaves the topology: Sonos lists
    // an unplugged speaker for minutes, and no topology change may come to try
    // it again once it is back (the TV app's frozen Kitchen).
    let mut retry_backoff = Duration::from_secs(2);
    let mut next_retry =
        unwired(&wiring, &household).then(|| tokio::time::Instant::now() + retry_backoff);
    // The music services, opened on first use and kept for the session.
    let services: crate::services::Shared = Arc::default();
    // Commands, in order per target (see `order`). Each starts from the
    // household as it is when it starts, not as it was when it was sent: a
    // play queued behind a regrouping finds the group as regrouped.
    let (latest, snapshot) = watch::channel(household.clone());
    let run = {
        let session = session.clone();
        let updates = updates.clone();
        let services = services.clone();
        move |command: Command| {
            let mut session = session.clone();
            let household = snapshot.borrow().clone();
            // Commands that name a room resolve it against the topology as it
            // is now, not as it was when the session opened.
            if let Some(groups) = household.groups.clone() {
                session.groups = groups;
            }
            Box::pin(perform(
                session,
                household,
                services.clone(),
                updates.clone(),
                command,
            ))
        }
    };
    let lanes = lanes::spawn(run.clone());
    // The search under way, on a task of its own: a newer search, or the
    // Search screen closing, ends it, and its remaining services go unasked.
    let mut search = Latest::default();

    loop {
        tokio::select! {
            Some(update) = wire_rx.recv() => {
                match update {
                    WireUpdate::Group { id, ip, live } => {
                        wiring.groups.insert(id.clone(), ip);
                        household.live.insert(id, *live);
                        latest.send_replace(household.clone());
                        updates(Update::Household(Box::new(household.clone())));
                    }
                    WireUpdate::Player { id, volume, ht } => {
                        wiring.players.insert(id.clone());
                        if let Some(vol) = volume {
                            household.player_volumes.insert(id.clone(), vol);
                        }
                        if let Some(ht) = ht {
                            household.home_theater.insert(id, ht);
                        }
                        latest.send_replace(household.clone());
                        updates(Update::Household(Box::new(household.clone())));
                    }
                    WireUpdate::Done => {
                        if unwired(&wiring, &household) {
                            retry_backoff = (retry_backoff * 2).min(Duration::from_secs(60));
                            next_retry = Some(tokio::time::Instant::now() + retry_backoff);
                        } else {
                            retry_backoff = Duration::from_secs(2);
                            next_retry = None;
                        }
                    }
                }
            }
            tagged = events.recv() => {
                let Some((ip, event)) = tagged else { bail!("event channel closed") };
                if event.kind == Event::LOST {
                    let coordinator = wiring.groups.values().any(|c| *c == ip);
                    if ip == primary_ip || coordinator {
                        bail!("Lost the connection to a speaker");
                    }
                    // A speaker reached only for its own volume: subscribed again
                    // once it can be reached, on a socket of its own.
                    wiring.forwarded.lock().unwrap().remove(&ip);
                    if let Some(groups) = household.groups.as_ref() {
                        for player in groups.players.iter().filter(|p| p.ip() == Some(ip)) {
                            wiring.players.remove(&player.id);
                        }
                    }
                    next_retry = next_retry.or_else(|| Some(tokio::time::Instant::now() + retry_backoff));
                    continue;
                }
                if apply(&mut household, &wiring, ip, &event)? {
                    if event.namespace == "groups:1" {
                        let groups = household.groups.clone().unwrap_or_default_groups();
                        let current: HashSet<&str> = groups.groups.iter().map(|g| g.id.as_str()).collect();
                        wiring.groups.retain(|id, _| current.contains(id.as_str()));
                        household.live.retain(|id, _| current.contains(id.as_str()));
                        let (groups_to_wire, players_to_wire) = unwired_targets(&wiring, &groups);
                        wiring_task.replace(Some(spawn_wire(
                            session.clone(),
                            wiring.events.clone(),
                            wire_tx.clone(),
                            groups_to_wire,
                            players_to_wire,
                            wiring.forwarded.clone(),
                        )));
                        if !unwired(&wiring, &household) {
                            next_retry = None;
                            retry_backoff = Duration::from_secs(2);
                        } else if next_retry.is_none() {
                            next_retry = Some(tokio::time::Instant::now() + retry_backoff);
                        }
                    }
                    latest.send_replace(household.clone());
                    updates(Update::Household(Box::new(household.clone())));
                }
            }
            _ = tokio::time::sleep_until(next_retry.unwrap_or_else(tokio::time::Instant::now)), if next_retry.is_some() => {
                next_retry = None;
                let groups = household.groups.clone().unwrap_or_default_groups();
                let (groups_to_wire, players_to_wire) = unwired_targets(&wiring, &groups);
                wiring_task.replace(Some(spawn_wire(
                    session.clone(),
                    wiring.events.clone(),
                    wire_tx.clone(),
                    groups_to_wire,
                    players_to_wire,
                    wiring.forwarded.clone(),
                )));
            }
            command = inbox.recv() => {
                let Some(command) = command else { return Ok(()) };
                if matches!(command, Command::Reconnect) {
                    return Ok(());
                }
                match command {
                    Command::CancelSearch => search.replace(None),
                    Command::Fetch(Fetch::Search { .. }) => search.replace(Some(tokio::spawn(run(command)).abort_handle())),
                    // Queued, not awaited: the lane runs it once what it follows is done.
                    command => {
                        let (order, replaces) = order(&command);
                        lanes.submit(order, replaces, command);
                    }
                }
            }
        }
    }
}

/// The one task of a kind worth finishing: a newer one, or none, ends it.
#[derive(Default)]
struct Latest(Option<tokio::task::AbortHandle>);

impl Latest {
    fn replace(&mut self, next: Option<tokio::task::AbortHandle>) {
        if let Some(old) = std::mem::replace(&mut self.0, next) {
            old.abort();
        }
    }
}

/// Ended with the session it ran on.
impl Drop for Latest {
    fn drop(&mut self) {
        self.replace(None);
    }
}

/// What a mutation changes, and so the lane it waits in.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Target {
    /// A speaker's own volume and settings.
    Player(String),
    /// A group's playback, queue and volume.
    Group(String),
    /// The household's favourites, alarms and sign-ins.
    Lists,
}

/// Where a command runs, and, for one that sets an absolute level, which: a
/// newer level for the same target takes the place of one not yet sent.
///
/// Reads run at once. A speaker's settings wait for that speaker's earlier
/// ones; a group's transport, queue and volume for that group's. A change of
/// who is grouped with whom is a barrier: it waits for everything sent before
/// it and holds back everything sent after, since it changes which group a
/// room is in, and a preset or Play everywhere sets speakers' levels as well.
/// Deltas (volume up and down), toggles and transport are never replaced.
fn order(command: &Command) -> (Order<Target>, Option<&'static str>) {
    use Command::*;
    let player = |p: &String| Order::Lane(Target::Player(p.clone()));
    let group = |g: &String| Order::Lane(Target::Group(g.clone()));
    match command {
        Fetch(_) | ReadRoom { .. } | Reconnect | CancelSearch => (Order::Free, None),
        ModifyGroup { .. } | PlayEverywhere { .. } | SplitAll | ApplyPreset(_) => {
            (Order::Barrier, None)
        }
        SetBass { player: p, .. } => (player(p), Some("bass")),
        SetTreble { player: p, .. } => (player(p), Some("treble")),
        SetEqValue {
            player: p, eq_type, ..
        } => (player(p), Some(*eq_type)),
        AdjustPlayerVolume { player: p, .. }
        | SetPlayerMute { player: p, .. }
        | SetNightSound { player: p, .. }
        | SetSpeechEnhancement { player: p, .. }
        | SetLoudness { player: p, .. }
        | SetTrueplay { player: p, .. }
        | SetStatusLight { player: p, .. }
        | SetButtonsLocked { player: p, .. }
        | SetRemoteFeedback { player: p, .. }
        | SetIrRepeater { player: p, .. } => (player(p), None),
        SetGroupVolume { group: g, .. } => (group(g), Some("volume")),
        Playback { group: g, .. }
        | SeekTo { group: g, .. }
        | SeekBy { group: g, .. }
        | SetGroupMute { group: g, .. }
        | SetShuffle { group: g, .. }
        | SetRepeat { group: g, .. }
        | UseTvInput { group: g, .. }
        | UseLineIn { group: g, .. }
        | SetSleep { group: g, .. }
        | SetCrossfade { group: g, .. }
        | SaveFavorite { group: g }
        | Replay { group: g, .. }
        | Rate { group: g, .. }
        | PlayQueueTrack { group: g, .. }
        | RemoveFromQueue { group: g, .. }
        | ClearQueue { group: g }
        | PlayFavorite { group: g, .. }
        | PlayPlaylist { group: g, .. }
        | PlayStream { group: g, .. }
        | PlayServiceItem { group: g, .. }
        | QueueServiceItem { group: g, .. } => (group(g), None),
        ImportSignIns
        | ServiceFavorite { .. }
        | RemoveFavorite { .. }
        | SetAlarmEnabled { .. }
        | WriteAlarm(_) => (Order::Lane(Target::Lists), None),
        // The edit's lane; never replaced, or its read would go with it.
        EditThenRead { edit, .. } => (order(edit).0, None),
    }
}

/// One command, run to its end, with its answer or refusal sent on.
async fn perform(
    session: Session,
    household: Household,
    services: crate::services::Shared,
    updates: Sink,
    command: Command,
) {
    if let Command::ReadRoom { group, player } = command {
        let details = read_room(&session, &household, group, player).await;
        updates(Update::RoomDetails(details));
        return;
    }
    if let Command::Replay { group, item } = command {
        let replayed = async {
            let groups = household
                .groups
                .as_ref()
                .ok_or_else(|| anyhow!("no topology yet"))?;
            let g = groups
                .groups
                .iter()
                .find(|g| g.id == group)
                .ok_or_else(|| anyhow!("That room has just changed; try again"))?;
            let target = session::target_for(groups, g);
            let player = coordinator(&session, &household, &group).await?;
            Box::pin(x2rock::play::replay_item(
                &session,
                &player,
                &target,
                Some(&target.name),
                &item,
            ))
            .await
        }
        .await;
        if let Err(e) = replayed {
            updates(Update::Refusal(plain(&e)));
        }
        return;
    }
    if let Command::ImportSignIns = command {
        let imported =
            async { Box::pin(services.open(&session).await?.import(&session)).await }.await;
        updates(Update::Notice(match imported {
            Ok(names) if names.is_empty() => "The speakers hold no sign-ins this could read".into(),
            Ok(names) => format!("Signed in to {} through the speakers", names.join(", ")),
            Err(e) => plain(&e),
        }));
        return;
    }
    if let Command::Rate { group, kind } = command {
        let rated = async {
            let player = coordinator(&session, &household, &group).await?;
            let room = coordinator_id(&household, &group)
                .ok()
                .and_then(|id| {
                    household
                        .groups
                        .as_ref()?
                        .player(&id)
                        .map(|p| p.name.clone())
                })
                .unwrap_or_default();
            Box::pin(x2rock::play::rate(&player, &group, &room, kind, false)).await
        }
        .await;
        updates(Update::Notice(match rated {
            Ok(o) => o.done.to_owned(),
            Err(e) => plain(&e),
        }));
        return;
    }
    if let Command::SaveFavorite { group } = command {
        let saved = async {
            let groups = household
                .groups
                .as_ref()
                .ok_or_else(|| anyhow!("no topology yet"))?;
            let g = groups
                .groups
                .iter()
                .find(|g| g.id == group)
                .ok_or_else(|| anyhow!("That room has just changed; try again"))?;
            let target = session::target_for(groups, g);
            let player = coordinator(&session, &household, &group).await?;
            Box::pin(x2rock::play::save_playing_favorite(
                &session, &player, &target, false,
            ))
            .await
        }
        .await;
        updates(Update::Notice(match saved {
            Ok(o) if o.saved => format!("Saved {} to Sonos favorites", o.name),
            Ok(o) => format!("{} is already a Sonos favorite", o.name),
            Err(e) => format!("Couldn't save it: {}", plain(&e)),
        }));
        return;
    }
    if let Command::Fetch(fetch) = command {
        Box::pin(fetch_into(&session, &household, &services, fetch, &updates)).await;
        return;
    }
    if let Command::ServiceFavorite { service, item, add } = command {
        let done = async {
            let s = services.open(&session).await?;
            let upnp = Upnp::new(session.connection.ip());
            Ok::<_, anyhow::Error>(if add {
                if s.add_favorite(&upnp, &service, &item).await? {
                    format!("Saved {} to Sonos favorites", item.title)
                } else {
                    format!("{} is already a Sonos favorite", item.title)
                }
            } else {
                s.remove_favorite(&upnp, &service, &item).await?;
                format!("Removed {} from Sonos favorites", item.title)
            })
        }
        .await;
        match done {
            Ok(said) => updates(Update::Notice(said)),
            Err(e) => updates(Update::Refusal(plain(&e))),
        }
        Box::pin(fetch_into(
            &session,
            &household,
            &services,
            Fetch::Favorites { id: None },
            &updates,
        ))
        .await;
        return;
    }
    if let Command::RemoveFavorite { id, name } = command {
        // One already gone is as good as removed.
        match Upnp::new(session.connection.ip())
            .destroy_favorite(&format!("FV:2/{id}"))
            .await
        {
            Ok(_) => updates(Update::Notice(format!(
                "Removed {name} from Sonos favorites"
            ))),
            Err(e) => updates(Update::Refusal(plain(&e))),
        }
        Box::pin(fetch_into(
            &session,
            &household,
            &services,
            Fetch::Favorites { id: None },
            &updates,
        ))
        .await;
        return;
    }
    if let Command::WriteAlarm(write) = command {
        let alarm = write.alarm();
        let result = write_alarm(&session, &household, write)
            .await
            .map_err(|e| plain(&e));
        updates(Update::Fetched(Fetched::AlarmWritten { alarm, result }));
        Box::pin(fetch_into(
            &session,
            &household,
            &services,
            Fetch::Alarms { id: None },
            &updates,
        ))
        .await;
        return;
    }
    if let Command::EditThenRead { edit, read } = command {
        if let Err(e) = Box::pin(execute(&session, &household, *edit)).await {
            updates(Update::Refusal(plain(&e)));
        }
        Box::pin(fetch_into(&session, &household, &services, read, &updates)).await;
        return;
    }
    if let Command::PlayServiceItem { .. } | Command::QueueServiceItem { .. } = command {
        let played = async {
            let services = services.open(&session).await?;
            let room_of = |group: &str| {
                coordinator_id(&household, group)
                    .ok()
                    .and_then(|id| {
                        household
                            .groups
                            .as_ref()?
                            .player(&id)
                            .map(|p| p.name.clone())
                    })
                    .ok_or_else(|| anyhow!("That room has just changed; try again"))
            };
            match command {
                Command::PlayServiceItem {
                    group,
                    service,
                    item,
                } => {
                    services
                        .play(&session, &room_of(&group)?, &service, &item)
                        .await
                }
                Command::QueueServiceItem {
                    group,
                    service,
                    item,
                    next,
                } => {
                    services
                        .enqueue(&session, &room_of(&group)?, &service, &item, next)
                        .await?;
                    updates(Update::Notice(format!(
                        "Added {} {}",
                        item.title,
                        if next {
                            "to play next"
                        } else {
                            "to the end of the queue"
                        }
                    )));
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        .await;
        if let Err(e) = played {
            updates(Update::Refusal(plain(&e)));
        }
        return;
    }
    if let Err(e) = Box::pin(execute(&session, &household, command)).await {
        updates(Update::Refusal(plain(&e)));
    }
}

trait GroupsDefault {
    fn unwrap_or_default_groups(self) -> Groups;
}

impl GroupsDefault for Option<Groups> {
    fn unwrap_or_default_groups(self) -> Groups {
        self.unwrap_or(Groups {
            groups: Vec::new(),
            players: Vec::new(),
        })
    }
}

/// Fold one event in. Whether anything the screen shows changed.
fn apply(household: &mut Household, wiring: &Wiring, ip: IpAddr, event: &Event) -> Result<bool> {
    let now = Instant::now();
    match event.namespace.as_str() {
        "groups:1" => {
            // A body that does not parse as a topology (a confirmation) is not one.
            let Ok(groups) = Groups::deserialize(&event.body) else {
                return Ok(false);
            };
            household.groups = Some(groups);
            Ok(true)
        }
        "playback:1" | "playbackMetadata:1" | "groupVolume:1" | "sleepTimer:1" => {
            let Some(group) = event.group_id.as_deref() else {
                return Ok(false);
            };
            // Taken from the group's coordinator alone.
            if wiring.groups.get(group) != Some(&ip) {
                return Ok(false);
            }
            let live: &mut GroupLive = household.live.entry(group.to_owned()).or_default();
            match event.namespace.as_str() {
                "playback:1" => {
                    if let Some(error) = proto::playback_error(&event.body) {
                        eprintln!("x2rock-deck: playback failed: {error}");
                        return Ok(false);
                    }
                    let Ok(status) = proto::PlaybackStatus::deserialize(&event.body) else {
                        return Ok(false);
                    };
                    live.apply_playback(&status, now);
                }
                "playbackMetadata:1" => {
                    let Ok(status) = proto::MetadataStatus::deserialize(&event.body) else {
                        return Ok(false);
                    };
                    live.metadata = Some(status);
                }
                "sleepTimer:1" => live.apply_sleep(&event.body, now),
                _ => {
                    let Ok(volume) = proto::Volume::deserialize(&event.body) else {
                        return Ok(false);
                    };
                    live.volume = Some(volume);
                }
            }
            Ok(true)
        }
        "zones:1" => {
            let Ok(zones) = proto::Zones::deserialize(&event.body) else {
                return Ok(false);
            };
            proto::Bond::merge(&mut household.bonds, zones);
            Ok(true)
        }
        "playerVolume:1" => {
            let Some(player) = event.player_id.as_deref() else {
                return Ok(false);
            };
            let Ok(volume) = proto::Volume::deserialize(&event.body) else {
                return Ok(false);
            };
            household.player_volumes.insert(player.to_owned(), volume);
            Ok(true)
        }
        "homeTheater:1" => {
            let Some(player) = event.player_id.as_deref() else {
                return Ok(false);
            };
            let Ok(update) = HomeTheaterUpdate::deserialize(&event.body) else {
                return Ok(false);
            };
            let entry = household
                .home_theater
                .entry(player.to_owned())
                .or_insert((false, false));
            if let Some(night) = update.night_mode {
                entry.0 = night;
            }
            if let Some(dialog) = update.enhance_dialog {
                entry.1 = dialog;
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// The coordinator's socket for a group, from the topology as last heard.
async fn coordinator(session: &Session, household: &Household, group: &str) -> Result<Connection> {
    let groups = household
        .groups
        .as_ref()
        .ok_or_else(|| anyhow!("no topology yet"))?;
    let group = groups
        .groups
        .iter()
        .find(|g| g.id == group)
        .ok_or_else(|| anyhow!("That room has just changed; try again"))?;
    let ip = groups
        .player(&group.coordinator_id)
        .and_then(proto::Player::ip);
    session.player(ip).await
}

async fn speaker(session: &Session, household: &Household, player: &str) -> Result<Connection> {
    let ip = household
        .groups
        .as_ref()
        .and_then(|g| g.player(player))
        .and_then(proto::Player::ip);
    session.player(ip).await
}

fn speaker_ip(household: &Household, player: &str) -> Result<IpAddr> {
    household
        .groups
        .as_ref()
        .and_then(|g| g.player(player))
        .and_then(proto::Player::ip)
        .ok_or_else(|| anyhow!("That speaker has no address"))
}

/// Levels before anyone joins, so nothing joins loud: a speaker whose level
/// could not be set stays out, as does one the preset holds no level for (a
/// fixed line-out is the amplifier's and always comes). Then every speaker into
/// the first's group, the others out of it, and the favourite started there.
async fn apply_preset(
    session: &Session,
    household: &Household,
    preset: crate::prefs::Preset,
) -> Result<()> {
    let groups = household
        .groups
        .as_ref()
        .ok_or_else(|| anyhow!("no topology yet"))?;
    let fixed = |id: &str| household.player_volumes.get(id).is_some_and(|v| v.fixed);
    let fixed = &fixed;
    let volumes = &preset.volumes;
    let sets = preset.player_ids.iter().map(|id| async move {
        let ok = match volumes.get(id) {
            Some(level) => match session
                .player(groups.player(id).and_then(proto::Player::ip))
                .await
            {
                Ok(conn) => conn.set_player_volume(id, *level).await.is_ok(),
                Err(_) => false,
            },
            None => fixed(id),
        };
        (id.clone(), ok)
    });
    let results = futures_util::future::join_all(sets).await;
    let joining: Vec<String> = results
        .iter()
        .filter(|(_, ok)| *ok)
        .map(|(id, _)| id.clone())
        .collect();
    let left_out: Vec<String> = results
        .iter()
        .filter(|(_, ok)| !*ok)
        .filter_map(|(id, _)| groups.player(id).map(|p| p.name.clone()))
        .collect();
    let Some(host) = joining.first().cloned() else {
        bail!("Couldn't set the volume in {}", left_out.join(", "));
    };
    let group = groups
        .group_of(&host)
        .ok_or_else(|| anyhow!("That room has just changed; try again"))?;
    let add: Vec<String> = joining
        .iter()
        .filter(|p| !group.player_ids.contains(p))
        .cloned()
        .collect();
    let remove: Vec<String> = group
        .player_ids
        .iter()
        .filter(|p| !joining.contains(p))
        .cloned()
        .collect();
    let conn = coordinator(session, household, &group.id).await?;
    let formed = if add.is_empty() && remove.is_empty() {
        group.id.clone()
    } else {
        conn.modify_group_members(&group.id, &add, &remove)
            .await?
            .group
            .id
    };
    if let Some(favorite) = &preset.favorite_id {
        conn.call(
            json!({ "namespace": "favorites:1", "command": "loadFavorite", "groupId": formed }),
            json!({ "favoriteId": favorite, "playOnCompletion": true, "action": "REPLACE" }),
        )
        .await?;
    }
    if !left_out.is_empty() {
        bail!("Left out {}: couldn't set its volume", left_out.join(", "));
    }
    Ok(())
}

async fn execute(session: &Session, household: &Household, command: Command) -> Result<()> {
    match command {
        Command::Playback { group, command } => {
            coordinator(session, household, &group)
                .await?
                .playback(&group, command)
                .await
        }
        Command::SeekTo { group, millis } => {
            coordinator(session, household, &group)
                .await?
                .seek_to(&group, millis)
                .await
        }
        Command::SeekBy { group, millis } => {
            coordinator(session, household, &group)
                .await?
                .seek_by(&group, millis)
                .await
        }
        Command::SetGroupVolume { group, level } => {
            coordinator(session, household, &group)
                .await?
                .set_group_volume(&group, level)
                .await
        }
        Command::SetGroupMute { group, muted } => {
            coordinator(session, household, &group)
                .await?
                .set_group_mute(&group, muted)
                .await
        }
        Command::AdjustPlayerVolume { player, delta } => {
            speaker(session, household, &player)
                .await?
                .adjust_player_volume(&player, delta)
                .await
        }
        Command::SetPlayerMute { player, muted } => {
            let conn = speaker(session, household, &player).await?;
            conn.call(
                json!({ "namespace": "playerVolume:1", "command": "setMute", "playerId": player }),
                json!({ "muted": muted }),
            )
            .await?;
            Ok(())
        }
        Command::SetShuffle { group, on } => {
            coordinator(session, household, &group)
                .await?
                .set_shuffle(&group, on)
                .await
        }
        Command::SetRepeat { group, repeat } => {
            coordinator(session, household, &group)
                .await?
                .set_repeat(&group, repeat)
                .await
        }
        Command::SetNightSound { player, on } => {
            Upnp::new(speaker_ip(household, &player)?)
                .set_eq("NightMode", on)
                .await
        }
        Command::SetSpeechEnhancement { player, on } => {
            Upnp::new(speaker_ip(household, &player)?)
                .set_eq("DialogLevel", on)
                .await
        }
        Command::ModifyGroup { group, add, remove } => {
            coordinator(session, household, &group)
                .await?
                .modify_group_members(&group, &add, &remove)
                .await?;
            Ok(())
        }
        Command::PlayEverywhere { group, level } => {
            let groups = household
                .groups
                .as_ref()
                .ok_or_else(|| anyhow!("no topology yet"))?;
            let host = groups
                .groups
                .iter()
                .find(|g| g.id == group)
                .ok_or_else(|| anyhow!("That room has just changed; try again"))?;
            let others: Vec<String> = groups
                .players
                .iter()
                .map(|p| p.id.clone())
                .filter(|p| !host.player_ids.contains(p))
                .collect();
            if let Some(level) = level {
                // Levels before anyone joins, so nothing joins loud. A speaker
                // that does not answer keeps its own level and still joins.
                for player in groups.players.iter() {
                    if let Ok(conn) = session.player(player.ip()).await {
                        let _ = conn.set_player_volume(&player.id, level).await;
                    }
                }
            }
            if others.is_empty() {
                return Ok(());
            }
            coordinator(session, household, &group)
                .await?
                .modify_group_members(&group, &others, &[])
                .await?;
            Ok(())
        }
        Command::SplitAll => {
            let groups = household
                .groups
                .as_ref()
                .ok_or_else(|| anyhow!("no topology yet"))?;
            for g in groups.groups.iter().filter(|g| g.player_ids.len() > 1) {
                let members: Vec<String> = g
                    .player_ids
                    .iter()
                    .filter(|p| **p != g.coordinator_id)
                    .cloned()
                    .collect();
                coordinator(session, household, &g.id)
                    .await?
                    .modify_group_members(&g.id, &[], &members)
                    .await?;
            }
            Ok(())
        }
        Command::UseLineIn { group, player } => {
            // Fire and forget, as every playback command here: the effect is pushed.
            coordinator(session, household, &group)
                .await?
                .call(
                    json!({ "namespace": "playback:1", "command": "loadLineIn", "groupId": group }),
                    json!({ "deviceId": player, "playOnCompletion": true }),
                )
                .await?;
            Ok(())
        }
        Command::UseTvInput { group, soundbar } => {
            let groups = household
                .groups
                .as_ref()
                .ok_or_else(|| anyhow!("no topology yet"))?;
            let g = groups
                .groups
                .iter()
                .find(|g| g.id == group)
                .ok_or_else(|| anyhow!("That room has just changed; try again"))?;
            let coordinator_ip = speaker_ip(household, &g.coordinator_id)?;
            Upnp::new(coordinator_ip)
                .use_tv_input(&soundbar, speaker_ip(household, &soundbar)?)
                .await
        }
        Command::SetSleep { group, minutes } => {
            let groups = household
                .groups
                .as_ref()
                .ok_or_else(|| anyhow!("no topology yet"))?;
            let g = groups
                .groups
                .iter()
                .find(|g| g.id == group)
                .ok_or_else(|| anyhow!("That room has just changed; try again"))?;
            Upnp::new(speaker_ip(household, &g.coordinator_id)?)
                .set_sleep_timer(minutes.map(|m| Duration::from_secs(u64::from(m) * 60)))
                .await
        }
        Command::SetCrossfade { group, on } => {
            coordinator(session, household, &group)
                .await?
                .set_crossfade(&group, on)
                .await
        }
        Command::SetBass { player, level } => {
            Upnp::new(speaker_ip(household, &player)?)
                .set_bass(level)
                .await
        }
        Command::SetTreble { player, level } => {
            Upnp::new(speaker_ip(household, &player)?)
                .set_treble(level)
                .await
        }
        Command::SetRemoteFeedback { player, on } => {
            Upnp::new(speaker_ip(household, &player)?)
                .set_led_feedback(on)
                .await
        }
        Command::SetIrRepeater { player, on } => {
            Upnp::new(speaker_ip(household, &player)?)
                .set_ir_repeater(on)
                .await
        }
        Command::SetEqValue {
            player,
            eq_type,
            value,
        } => {
            // Only where the room has the speaker: the types answer on players that have none.
            let has = if eq_type.starts_with("Sub") {
                household.has_sub(&player)
            } else {
                household.has_surrounds(&player)
            };
            if !has {
                bail!(
                    "{eq_type} is for a room with {}",
                    if eq_type.starts_with("Sub") {
                        "a Sub"
                    } else {
                        "surrounds"
                    }
                );
            }
            Upnp::new(speaker_ip(household, &player)?)
                .set_eq_value(eq_type, value)
                .await
        }
        Command::SetLoudness { player, on } => {
            Upnp::new(speaker_ip(household, &player)?)
                .set_loudness(on)
                .await
        }
        Command::SetTrueplay { player, on } => {
            Upnp::new(speaker_ip(household, &player)?)
                .set_trueplay(on)
                .await
        }
        Command::SetStatusLight { player, on } => {
            Upnp::new(speaker_ip(household, &player)?).set_led(on).await
        }
        Command::SetButtonsLocked { player, locked } => {
            Upnp::new(speaker_ip(household, &player)?)
                .set_buttons_locked(locked)
                .await
        }
        Command::PlayQueueTrack { group, index } => {
            let conn = coordinator(session, household, &group).await?;
            let upnp = Upnp::new(coordinator_ip(household, &group)?);
            let coordinator_id = coordinator_id(household, &group)?;
            if !upnp.playing_from_queue().await? {
                upnp.use_queue(&coordinator_id).await?;
            }
            upnp.seek_track(index).await?;
            conn.playback(&group, "play").await
        }
        Command::RemoveFromQueue {
            group,
            index,
            update_id,
        } => {
            Upnp::new(coordinator_ip(household, &group)?)
                .remove_track(index, Some(&update_id))
                .await
        }
        Command::ClearQueue { group } => {
            Upnp::new(coordinator_ip(household, &group)?)
                .clear_queue()
                .await
        }
        Command::PlayFavorite { group, id } => {
            // REPLACE, as the TV app learned: without it an album was appended.
            coordinator(session, household, &group)
                .await?
                .call(
                    json!({ "namespace": "favorites:1", "command": "loadFavorite", "groupId": group }),
                    json!({ "favoriteId": id, "playOnCompletion": true, "action": "REPLACE" }),
                )
                .await?;
            Ok(())
        }
        Command::PlayPlaylist { group, id } => {
            coordinator(session, household, &group)
                .await?
                .load_playlist(&group, &id)
                .await
        }
        Command::PlayStream { group, url, title } => {
            let conn = coordinator(session, household, &group).await?;
            let opened = conn
                .call(
                    json!({ "namespace": "playbackSession:1", "command": "createSession", "groupId": group }),
                    json!({ "appId": APP_ID, "appContext": "deck" }),
                )
                .await?;
            let session_id = opened["sessionId"]
                .as_str()
                .ok_or_else(|| anyhow!("the player opened a session but did not name it"))?;
            conn.call(
                json!({ "namespace": "playbackSession:1", "command": "loadStreamUrl", "sessionId": session_id }),
                json!({ "streamUrl": url, "playOnCompletion": true, "stationMetadata": { "name": title, "type": "station" } }),
            )
            .await?;
            Ok(())
        }
        Command::ApplyPreset(preset) => Box::pin(apply_preset(session, household, preset)).await,
        Command::SetAlarmEnabled { mut alarm, enabled } => {
            alarm.enabled = enabled;
            Upnp::new(session.connection.ip())
                .update_alarm(&alarm)
                .await
        }
        Command::Replay { .. }
        | Command::ImportSignIns
        | Command::Rate { .. }
        | Command::SaveFavorite { .. }
        | Command::ReadRoom { .. }
        | Command::Fetch(_)
        | Command::PlayServiceItem { .. }
        | Command::QueueServiceItem { .. }
        | Command::Reconnect
        | Command::CancelSearch
        | Command::EditThenRead { .. }
        | Command::WriteAlarm(_)
        | Command::ServiceFavorite { .. }
        | Command::RemoveFavorite { .. } => Ok(()),
    }
}

/// The app id a playback session is opened under: the x2rock CLI's.
const APP_ID: &str = "com.rahga.x2rock";

fn coordinator_id(household: &Household, group: &str) -> Result<String> {
    household
        .groups
        .as_ref()
        .and_then(|g| g.groups.iter().find(|g| g.id == group))
        .map(|g| g.coordinator_id.clone())
        .ok_or_else(|| anyhow!("That room has just changed; try again"))
}

fn coordinator_ip(household: &Household, group: &str) -> Result<IpAddr> {
    speaker_ip(household, &coordinator_id(household, group)?)
}

/// A fetch, answered: most in one update, a search a section at a time.
async fn fetch_into(
    session: &Session,
    household: &Household,
    shared: &crate::services::Shared,
    fetch: Fetch,
    updates: &Sink,
) {
    match fetch {
        Fetch::Services { id } => {
            let result = shared
                .open(session)
                .await
                .map(|s| s.usable())
                .map_err(|e| plain(&e));
            updates(Update::Fetched(Fetched::Services { id, result }));
        }
        Fetch::SignIns { id } => {
            let read = async {
                let s = shared.open(session).await?;
                if s.last_read().is_none() {
                    let _ = Box::pin(s.import(session)).await;
                }
                Ok::<_, anyhow::Error>(s.last_read())
            }
            .await;
            updates(Update::Fetched(Fetched::SignIns {
                id,
                result: read.unwrap_or_else(|e| Some(Err(plain(&e)))),
            }));
        }
        Fetch::Resume { service, book } => {
            let chapter = async {
                shared
                    .open(session)
                    .await?
                    .resume_chapter(&service, &book)
                    .await
            }
            .await
            .ok()
            .flatten();
            updates(Update::Fetched(Fetched::Resume { book, chapter }));
        }
        Fetch::IsFavorite { service, item } => {
            let known = async {
                shared
                    .open(session)
                    .await?
                    .is_favorite(&Upnp::new(session.connection.ip()), &service, &item)
                    .await
            }
            .await;
            if let Ok(yes) = known {
                updates(Update::Fetched(Fetched::IsFavorite {
                    item_id: item.id,
                    yes,
                }));
            }
        }
        Fetch::Search { id, term } => {
            match shared.open(session).await {
                Ok(services) => {
                    services
                        .search(&term, |section| {
                            updates(Update::Fetched(Fetched::SearchSection {
                                id,
                                term: term.clone(),
                                section,
                            }))
                        })
                        .await;
                }
                Err(e) => updates(Update::Notice(format!("Couldn't search: {}", plain(&e)))),
            }
            updates(Update::Fetched(Fetched::SearchDone { id, term }));
        }
        Fetch::ServicePage {
            id,
            service,
            container,
            index,
        } => {
            let result = match shared.open(session).await {
                Ok(s) => s
                    .browse(&service, &container, index, 50)
                    .await
                    .map_err(|e| plain(&e)),
                Err(e) => Err(plain(&e)),
            };
            updates(Update::Fetched(Fetched::ServicePage {
                id,
                service,
                container,
                index,
                result,
            }));
        }
        other => updates(Update::Fetched(
            Box::pin(fetched(session, household, other)).await,
        )),
    }
}

async fn fetched(session: &Session, household: &Household, fetch: Fetch) -> Fetched {
    let household_id = session.connection.household_id().await.unwrap_or_default();
    match fetch {
        Fetch::Queue { id, group } => {
            let result = async {
                let upnp = Upnp::new(coordinator_ip(household, &group)?);
                let (queue, from_queue) =
                    tokio::try_join!(upnp.queue(), upnp.playing_from_queue())?;
                let current = if from_queue {
                    upnp.current_track().await.ok()
                } else {
                    None
                };
                Ok::<_, anyhow::Error>(QueueView {
                    items: queue.items,
                    total: queue.total,
                    update_id: queue.update_id,
                    current,
                })
            }
            .await
            .map_err(|e| plain(&e));
            Fetched::Queue { id, group, result }
        }
        Fetch::Previous { group, key } => {
            // The slot playing, then one browse of the slot before it; nothing
            // when the queue is not the room's source.
            let track = async {
                let upnp = Upnp::new(coordinator_ip(household, &group)?);
                if !upnp.playing_from_queue().await? {
                    return Ok::<_, anyhow::Error>(None);
                }
                let slot = upnp.current_track().await?;
                Ok(upnp
                    .queue_item(slot.saturating_sub(1))
                    .await?
                    .map(|t| (t.title, t.artist)))
            }
            .await
            .ok()
            .flatten();
            Fetched::Previous { group, key, track }
        }
        Fetch::Favorites { id } => Fetched::Favorites {
            id,
            result: session
                .connection
                .favorites(&household_id)
                .await
                .map(|f| f.items)
                .map_err(|e| plain(&e)),
        },
        Fetch::Playlists { id } => Fetched::Playlists {
            id,
            result: session
                .connection
                .playlists(&household_id)
                .await
                .map(|p| p.playlists)
                .map_err(|e| plain(&e)),
        },
        Fetch::Stations { id, tag, country } => {
            let result = x2rock::stations::search(None, tag.as_deref(), country.as_deref(), 60)
                .await
                .map_err(|e| plain(&e));
            Fetched::Stations {
                id,
                fetch: Fetch::Stations { id, tag, country },
                result,
            }
        }
        Fetch::Alarms { id } => {
            let upnp = Upnp::new(session.connection.ip());
            let result = async {
                let (alarms, (clock, zone)) =
                    tokio::try_join!(upnp.alarms(), upnp.household_time())?;
                Ok::<_, anyhow::Error>((alarms, clock, zone >= 0))
            }
            .await
            .map_err(|e| plain(&e));
            Fetched::Alarms { id, result }
        }
        Fetch::About { id } => {
            let result = async {
                let players = Upnp::new(session.connection.ip()).system_players().await?;
                let reads = players.iter().map(|p| async move {
                    let ip = p.ip?;
                    let upnp = Upnp::new(ip);
                    let (info, update) = tokio::join!(upnp.device_info(), upnp.software_update());
                    let info = info.ok();
                    Some(SpeakerInfo {
                        id: p.uuid.clone(),
                        room: p.room.clone(),
                        software: info
                            .as_ref()
                            .map(|i| i.software_version.clone())
                            .unwrap_or_else(|| p.software_version.clone()),
                        model: info
                            .as_ref()
                            .map(|i| i.model_name.clone())
                            .unwrap_or_default(),
                        version: info
                            .as_ref()
                            .map(|i| i.display_version.clone())
                            .unwrap_or_else(|| p.software_version.clone()),
                        update_waiting: update.ok().is_some_and(|u| u.download_bytes > 0),
                        satellite: p.satellite || p.invisible,
                    })
                });
                let mut list: Vec<SpeakerInfo> = futures_util::future::join_all(reads)
                    .await
                    .into_iter()
                    .flatten()
                    .collect();
                list.sort_by_key(|s| (s.room.to_lowercase(), s.satellite));
                Ok::<_, anyhow::Error>(list)
            }
            .await
            .map_err(|e| plain(&e));
            Fetched::About { id, result }
        }
        Fetch::AlarmSounds { id } => {
            // Shortcuts have nothing to play: an alarm cannot ring one.
            let result = Upnp::new(session.connection.ip())
                .browse_content("FV:2")
                .await
                .map(|items| {
                    items
                        .into_iter()
                        .filter(|i| !i.shortcut)
                        .filter_map(|i| {
                            Some(AlarmProgram {
                                title: i.title,
                                uri: i.uri?,
                                metadata: i.metadata,
                            })
                        })
                        .collect()
                })
                .map_err(|e| plain(&e));
            Fetched::AlarmSounds { id, result }
        }
        Fetch::History { id } => Fetched::History {
            id,
            result: session
                .connection
                .history(&household_id)
                .await
                .map(|h| h.resources)
                .map_err(|e| plain(&e)),
        },
        Fetch::RatingOffers { group, track } => {
            let offers = match coordinator(session, household, &group).await {
                Ok(player) => Box::pin(x2rock::play::rating_offers(&player, &group))
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };
            Fetched::RatingOffers {
                group,
                track,
                offers,
            }
        }
        Fetch::Services { .. }
        | Fetch::SignIns { .. }
        | Fetch::IsFavorite { .. }
        | Fetch::Resume { .. }
        | Fetch::Search { .. }
        | Fetch::ServicePage { .. } => unreachable!("answered by fetch_into"),
    }
}

/// Make, change or remove an alarm. A change is written over a fresh read of
/// the alarm, so a field this editor does not show stays as the player has it.
async fn write_alarm(session: &Session, household: &Household, write: AlarmWrite) -> Result<()> {
    use x2rock::sonos::upnp::{Alarm, Fault, FaultKind};
    let upnp = Upnp::new(session.connection.ip());
    let code = |e: &anyhow::Error| match e.downcast_ref::<Fault>().map(|f| &f.kind) {
        Some(FaultKind::Action(code)) => Some(code.clone()),
        _ => None,
    };
    // The one refusal worth wording: a speaker has one alarm to a start time.
    let worded = |e: anyhow::Error, f: &AlarmFields| {
        if code(&e).as_deref() == Some("801") {
            let room = household
                .groups
                .as_ref()
                .and_then(|g| g.player(&f.room))
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "That room".into());
            anyhow!(
                "{room} already has an alarm at {}",
                f.start.get(..5).unwrap_or(&f.start)
            )
        } else {
            e
        }
    };
    let fill = |alarm: &mut Alarm, f: &AlarmFields| {
        alarm.start.clone_from(&f.start);
        alarm.recurrence.clone_from(&f.recurrence);
        alarm.room_uuid.clone_from(&f.room);
        alarm.program_uri = f
            .program
            .as_ref()
            .map_or_else(|| CHIME.to_owned(), |p| p.uri.clone());
        alarm.program_metadata = f
            .program
            .as_ref()
            .map(|p| p.metadata.clone())
            .unwrap_or_default();
        alarm.volume = f.volume;
    };
    match write {
        AlarmWrite::Create(fields) => {
            // The TV app's defaults for what it does not ask: a quarter of an hour, in order.
            let mut alarm = Alarm {
                id: 0,
                start: String::new(),
                duration: "00:15:00".into(),
                recurrence: String::new(),
                enabled: true,
                room_uuid: String::new(),
                program_uri: String::new(),
                program_metadata: String::new(),
                play_mode: "NORMAL".into(),
                volume: 20,
                include_linked_zones: false,
            };
            fill(&mut alarm, &fields);
            upnp.create_alarm(&alarm)
                .await
                .map(|_| ())
                .map_err(|e| worded(e, &fields))
        }
        AlarmWrite::Change { id, fields } => {
            let mut alarm = upnp
                .alarms()
                .await?
                .into_iter()
                .find(|a| a.id == id)
                .ok_or_else(|| anyhow!("That alarm is gone"))?;
            fill(&mut alarm, &fields);
            upnp.update_alarm(&alarm)
                .await
                .map_err(|e| worded(e, &fields))
        }
        // One already gone (800) is as good as removed.
        AlarmWrite::Remove(id) => match upnp.destroy_alarm(id).await {
            Err(e) if code(&e).as_deref() == Some("800") => Ok(()),
            other => other,
        },
    }
}

/// A room's settings, read together; whatever fails is left `None`.
async fn read_room(
    session: &Session,
    household: &Household,
    group: String,
    player: String,
) -> RoomDetails {
    let mut details = RoomDetails {
        group,
        player: player.clone(),
        ..Default::default()
    };
    let Ok(ip) = speaker_ip(household, &player) else {
        return details;
    };
    let upnp = Upnp::new(ip);
    let (sleep, tone, led, locked) = tokio::join!(
        upnp.sleep_timer(),
        upnp.tone(),
        upnp.led(),
        upnp.buttons_locked()
    );
    details.sleep_read = sleep.is_ok();
    details.sleep_left = sleep.ok().flatten();
    details.tone = tone.ok();
    details.status_light = led.ok();
    details.buttons_locked = locked.ok();
    // The group's speaker whose room has a Sub or surrounds, which need not be
    // the coordinator: a Beam with a Sub joined to a One SL's group.
    let host = household.groups.as_ref().and_then(|g| {
        let members = &g
            .groups
            .iter()
            .find(|grp| grp.id == details.group)?
            .player_ids;
        members
            .iter()
            .find(|m| household.has_sub(m) || household.has_surrounds(m))
            .cloned()
    });
    let host_ip = host.as_deref().and_then(|h| speaker_ip(household, h).ok());
    if let (Some(host), Some(host_ip)) = (host, host_ip) {
        let (sub, surrounds) = (household.has_sub(&host), household.has_surrounds(&host));
        let host_upnp = Upnp::new(host_ip);
        let read = |eq_type: &'static str, wanted: bool| {
            let upnp = &host_upnp;
            async move {
                if wanted {
                    upnp.eq_value(eq_type).await.ok()
                } else {
                    None
                }
            }
        };
        let (sub_on, sub_level, surrounds_on, tv_level, music_level) = tokio::join!(
            read("SubEnable", sub),
            read("SubGain", sub),
            read("SurroundEnable", surrounds),
            read("SurroundLevel", surrounds),
            read("MusicSurroundLevel", surrounds),
        );
        details.satellites = Some(Satellites {
            player: host,
            sub_on: sub_on.map(|v| v != 0),
            sub_level,
            surrounds_on: surrounds_on.map(|v| v != 0),
            tv_level,
            music_level,
        });
    }
    // The limit is the Control API's, on the speaker's own socket.
    let limit = async {
        let conn = session.player(Some(ip)).await?;
        conn.player_settings(&player).await
    };
    // The room's soundbar, which need not be the coordinator.
    let soundbar = household.groups.as_ref().and_then(|g| {
        let members = &g
            .groups
            .iter()
            .find(|grp| grp.id == details.group)?
            .player_ids;
        g.players
            .iter()
            .find(|p| members.contains(&p.id) && p.has_tv())
            .map(|p| (p.id.clone(), p.ip()))
    });
    let remote = async {
        let (id, Some(ip)) = soundbar.clone()? else {
            return None;
        };
        Upnp::new(ip).remote_settings().await.ok().map(|r| (id, r))
    };
    let battery = async { upnp.battery().await.ok().flatten() };
    let (limit, remote, battery) = tokio::join!(limit, remote, battery);
    details.battery = battery;
    details.volume_limit = limit
        .ok()
        .and_then(|s| s.volume_scaling_factor)
        .map(|f| (f * 100.0).round().clamp(0.0, 100.0) as u8)
        .filter(|l| *l < 100);
    details.remote = remote;
    details.read_at = Some(Instant::now());
    details
}

/// A failure in words a listener can use. A refusal the player gave is worded
/// where its code has been met (the TV app's `plainRefusal`); any other code
/// stays as the player sent it.
pub fn plain(e: &anyhow::Error) -> String {
    if let Some(api) = ApiError::of(e) {
        return match api.code.as_deref() {
            Some("ERROR_DISALLOWED_BY_POLICY") => "This source doesn't allow it".into(),
            Some("ERROR_PLAYBACK_NO_CONTENT") => "Nothing is loaded".into(),
            Some("ERROR_ACCOUNT_INVALID_ID") => {
                "It isn't available from this system's accounts".into()
            }
            _ => format!("{e:#}"),
        };
    }
    let text = format!("{e:#}");
    text.lines().next().unwrap_or("").to_owned()
}

#[cfg(test)]
mod wiring_tests {
    use super::*;

    #[test]
    fn a_group_or_speaker_not_subscribed_is_found_for_retrying() {
        let household = crate::demo::household();
        let groups = household.groups.clone().unwrap();
        let (events, _rx) = mpsc::unbounded_channel();
        let mut wiring = Wiring {
            events,
            forwarded: Arc::new(std::sync::Mutex::new(HashSet::new())),
            groups: HashMap::new(),
            players: HashSet::new(),
        };
        for g in &groups.groups {
            let ip = groups
                .player(&g.coordinator_id)
                .and_then(proto::Player::ip)
                .unwrap();
            wiring.groups.insert(g.id.clone(), ip);
        }
        wiring.players = groups.players.iter().map(|p| p.id.clone()).collect();
        assert!(!unwired(&wiring, &household), "everything subscribed");
        wiring.players.remove("RINCON_K1");
        assert!(
            unwired(&wiring, &household),
            "a speaker whose volume is not followed"
        );
        wiring.players.insert("RINCON_K1".into());
        wiring.groups.remove("RINCON_K1:4");
        assert!(unwired(&wiring, &household), "a group not followed");
        // Subscribed on another coordinator's address: a regroup moved it.
        wiring
            .groups
            .insert("RINCON_K1:4".into(), "10.0.0.9".parse().unwrap());
        assert!(
            unwired(&wiring, &household),
            "a group followed on its old coordinator"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_replaced_by_newer_ones_and_deltas_toggles_and_regroupings_are_not() {
        let p = || "RINCON_1".to_owned();
        let g = || "RINCON_1:7".to_owned();
        let kitchen = Order::Lane(Target::Player(p()));
        let room = Order::Lane(Target::Group(g()));
        assert_eq!(
            order(&Command::SetBass {
                player: p(),
                level: 3
            }),
            (kitchen.clone(), Some("bass"))
        );
        assert_eq!(
            order(&Command::SetEqValue {
                player: p(),
                eq_type: "SubGain",
                value: 2
            }),
            (kitchen.clone(), Some("SubGain"))
        );
        assert_eq!(
            order(&Command::SetGroupVolume {
                group: g(),
                level: 30
            }),
            (room.clone(), Some("volume"))
        );
        assert_eq!(
            order(&Command::AdjustPlayerVolume {
                player: p(),
                delta: -2
            }),
            (kitchen.clone(), None),
            "a delta is never dropped"
        );
        assert_eq!(
            order(&Command::SetLoudness {
                player: p(),
                on: true
            }),
            (kitchen, None),
            "a toggle is never dropped"
        );
        assert_eq!(
            order(&Command::Playback {
                group: g(),
                command: "play"
            }),
            (room.clone(), None)
        );
        assert_eq!(
            order(&Command::ModifyGroup {
                group: g(),
                add: vec![],
                remove: vec![]
            }),
            (Order::Barrier, None)
        );
        assert_eq!(order(&Command::SplitAll), (Order::Barrier, None));
        assert_eq!(
            order(&Command::Fetch(Fetch::Alarms { id: None })),
            (Order::Free, None)
        );
        let edit = Command::EditThenRead {
            edit: Box::new(Command::SetGroupVolume {
                group: g(),
                level: 1,
            }),
            read: Fetch::Queue {
                id: None,
                group: g(),
            },
        };
        assert_eq!(
            order(&edit),
            (room, None),
            "the edit's lane, and never replaced, or its read would go too"
        );
    }

    #[tokio::test]
    async fn a_newer_search_or_none_ends_the_one_under_way_before_its_remaining_sections() {
        use tokio::sync::{mpsc, oneshot};
        let (sections, mut said) = mpsc::unbounded_channel();
        // A search with one section answered and one still to come, when its gate opens.
        let search = |name: &'static str| {
            let sections = sections.clone();
            let (open, gate) = oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                let _ = sections.send((name, 1));
                let _ = gate.await;
                let _ = sections.send((name, 2));
            });
            (task, open)
        };
        let mut latest = Latest::default();
        let (first, first_gate) = search("first");
        latest.replace(Some(first.abort_handle()));
        assert_eq!(said.recv().await, Some(("first", 1)));
        let (second, second_gate) = search("second");
        latest.replace(Some(second.abort_handle()));
        // The timeout only turns a hang into a failure.
        async fn ended(task: tokio::task::JoinHandle<()>) -> Result<(), tokio::task::JoinError> {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("ended, not left running")
        }
        assert!(
            ended(first)
                .await
                .expect_err("the first search was ended")
                .is_cancelled()
        );
        assert!(
            first_gate.send(()).is_err(),
            "nothing is left to send the first search's second section"
        );
        assert_eq!(said.recv().await, Some(("second", 1)));
        latest.replace(None);
        assert!(
            ended(second)
                .await
                .expect_err("closing the Search screen ends it")
                .is_cancelled()
        );
        assert!(second_gate.send(()).is_err());
        drop(sections);
        assert_eq!(said.recv().await, None, "no section after either was ended");
    }

    #[test]
    fn a_search_kept_for_the_connection_is_the_latest_and_none_once_search_has_closed() {
        let search = |term: &str| {
            Command::Fetch(Fetch::Search {
                id: None,
                term: term.into(),
            })
        };
        let is_search = |c: &Command, term: &str| matches!(c, Command::Fetch(Fetch::Search { term: t, .. }) if t == term);
        let mut reads = Vec::new();
        hold(&mut reads, Command::Fetch(Fetch::Alarms { id: None }));
        hold(&mut reads, search("jazz"));
        hold(&mut reads, search("jazz piano"));
        assert_eq!(reads.len(), 2, "{reads:?}");
        assert!(
            is_search(&reads[1], "jazz piano"),
            "the newer search replaces the older: {reads:?}"
        );
        hold(&mut reads, Command::CancelSearch);
        assert!(
            matches!(reads.as_slice(), [Command::Fetch(Fetch::Alarms { .. })]),
            "other reads stay, the search goes: {reads:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_command_sent_while_a_probe_is_outstanding_is_dispatched_without_waiting() {
        let unreachable_ip: IpAddr = "192.0.2.1".parse().unwrap();
        let groups = proto::Groups {
            groups: vec![proto::Group {
                id: "GROUP_1".into(),
                name: "Living Room".into(),
                coordinator_id: "PLAYER_1".into(),
                playback_state: "IDLE".into(),
                player_ids: vec!["PLAYER_1".into()],
            }],
            players: vec![proto::Player {
                id: "PLAYER_1".into(),
                name: "Living Room".into(),
                websocket_url: format!("wss://{unreachable_ip}:1443/websocket/api"),
                capabilities: vec![],
            }],
        };
        let connection = Connection::dummy("127.0.0.1".parse().unwrap());
        let session = Session::new(connection, groups);
        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        let (updates_tx, _updates_rx) = mpsc::unbounded_channel();
        let updates: Sink = Arc::new(move |u| {
            let _ = updates_tx.send(u);
        });

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let wiring = Wiring {
            events: events_tx,
            forwarded: Arc::new(std::sync::Mutex::new(HashSet::new())),
            groups: HashMap::new(),
            players: HashSet::new(),
        };
        let household = Household {
            groups: Some(session.groups.clone()),
            ..Default::default()
        };
        let follow_task = tokio::spawn(async move {
            follow_loop(
                &session,
                "127.0.0.1".parse().unwrap(),
                household,
                wiring,
                events_rx,
                &updates,
                &mut inbox_rx,
            )
            .await
        });

        tokio::task::yield_now().await;
        assert_eq!(tokio::time::Instant::now().elapsed(), Duration::ZERO);

        inbox_tx.send(Command::Reconnect).unwrap();

        let result = tokio::time::timeout(Duration::from_millis(500), follow_task)
            .await
            .expect("loop stalled: command was not dispatched within 500ms")
            .unwrap();
        assert!(result.is_ok());
    }
}
