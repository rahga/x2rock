//! `x2rock tui`: the whole household on one screen, for a terminal.
//!
//! **Why a third front end at all.** The CLI answers one question and exits;
//! the bar widget is always there, but it is Omarchy's, it wants Quickshell,
//! and a bar popup cannot take the keyboard - which is why its favorites
//! picker had to become a panel of its own. What neither covers is what an ssh
//! session or a bare console wants: every room at once, each changeable
//! without naming it first.
//!
//! **It needs the daemon**, unlike the CLI, which promises to need nothing
//! running. That is deliberate and it says so plainly rather than drawing an
//! empty screen - see [`source`]. Reads come from MPRIS, and so do the writes
//! MPRIS can express; grouping, party, TV input and every volume step shell out
//! to this binary's CLI, for the reasons in [`action`].
//!
//! The seam worth keeping is inside: a keypress becomes an [`Intent`] without
//! touching the bus, so what every key does is a unit test rather than
//! something only a real household can answer, and the effects live in one
//! place ([`execute`]) instead of being scattered through the key handling.

mod action;
mod model;
mod source;
mod view;

use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use model::RoomSnapshot;
use source::Source;

/// Points one keypress moves a volume - a group's, or one speaker's own.
/// Coarser than the widget's two per wheel tick, because a tap is a coarser
/// gesture than a notch, and a held key is folded into one change anyway.
const STEP: i16 = 5;
/// How long volume keys are held back so a run of them goes out as one change.
/// Under the one-command-per-100ms the player wants (docs/architecture.md), and
/// short enough that a single tap still feels like it landed. The bar moves on
/// the keypress regardless; this is only about what is sent.
const COALESCE: Duration = Duration::from_millis(120);
/// How long an error stays up whatever is pressed. Keys typed while a slow
/// action ran are delivered the instant it returns, and without this they would
/// wipe the message before anyone had read it.
const HOLD: Duration = Duration::from_secs(1);
/// How long a write may take before it is given up on. Longer than any action
/// here takes against speakers that are answering - party across a household,
/// which reconnects to each coordinator in turn, is a few seconds - and short
/// enough that a speaker that has stopped answering is reported rather than
/// waited on. The child is killed with the wait; see [`action::Cli`].
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an empty household is disbelieved.
///
/// A regroup, and a daemon restart, drop every player and publish them again
/// a few seconds later, one Sonos round-trip at a time - and the read that
/// lands in between finds nothing. Drawing that flashed "no rooms" and closed
/// the grouping overlay on the very regroup the overlay had just asked for
/// (seen on a five-room household, 2026-09-09). The last rooms are held for
/// this long instead; a daemon that is really gone shows as empty once the
/// heartbeat re-reads past it.
const REPUBLISH_GRACE: Duration = Duration::from_secs(8);
/// How often the household is re-read with nothing having been pushed.
///
/// **Push is what makes the screen quick; this is what makes it true.** A
/// screen left open in an office for a day is a different thing from a remote
/// picked up for ten seconds: one dropped signal, or one read that lost the
/// race with its own settling window, and it would sit there wrong and
/// confident until somebody touched a speaker. The read is local IPC against
/// state the daemon already holds - it asks the speakers nothing - so the cost
/// of being sure is close to nothing.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// How long without a successful read before the screen stops claiming to know
/// anything. Three heartbeats, so one slow answer is not an alarm.
const STALE: Duration = Duration::from_secs(90);

/// Draw, wait, act, repeat - until `q`.
pub async fn run(ip: Option<IpAddr>) -> Result<()> {
    // Connected before the screen is taken over, so "the daemon is not running"
    // prints into the terminal it was typed in. Behind the alternate screen it
    // would flash for as long as the teardown takes and then be gone.
    let source = Source::connect().await?;
    let rooms = source.snapshot().await?;
    let cli = action::Cli::new(ip);

    let (tx, updates) = mpsc::unbounded_channel();
    let watcher = tokio::spawn(source.clone().watch(tx));

    // `init` also installs a panic hook that restores the terminal, so a panic
    // in here leaves a usable shell rather than a raw-mode one.
    let mut terminal = ratatui::init();
    let outcome = drive(
        &mut terminal,
        &source,
        &cli,
        App::new(rooms),
        updates,
        watcher,
    )
    .await;
    ratatui::restore();
    outcome
}

async fn drive(
    terminal: &mut ratatui::DefaultTerminal,
    source: &Source,
    cli: &action::Cli,
    mut app: App,
    mut updates: mpsc::UnboundedReceiver<Vec<RoomSnapshot>>,
    watcher: JoinHandle<Result<()>>,
) -> Result<()> {
    let mut keys = EventStream::new();
    // Only so the channel closing can be explained by whatever closed it. An
    // `Option` because the arm that takes it is inside the loop.
    let mut watcher = Some(watcher);
    // The heartbeat answers on a channel of its own rather than sharing the
    // watcher's. Holding a sender for that one would keep it open, and its
    // closing is how the watcher's death is noticed.
    let (heard, mut heartbeats) = mpsc::unbounded_channel();
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    // A CLI action can outlast a beat. Delay the next one rather than firing
    // the backlog the moment it returns.
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // In raw mode Ctrl-C is a keypress, not a signal, and is handled as one.
    // SIGTERM is not: `pkill x2rock` aimed at the daemon hits this binary too,
    // and without this the pane is left in raw mode on the alternate screen.
    let stop = crate::stop_signal();
    tokio::pin!(stop);
    // Volume keys waiting to be sent as one change, and when. See COALESCE.
    let mut pending: Option<(Nudge, tokio::time::Instant)> = None;
    // Writes run in tasks of their own and report back here, so the keyboard
    // stays live while a speaker is slow to answer - or never does, which is
    // what a resume from suspend can leave behind, and the one case where a
    // person most wants `q` to work. Counted, so that the "working" line comes
    // down when the last of them is in and not the first.
    let (finished, mut finishes) = mpsc::unbounded_channel::<Result<()>>();
    let mut in_flight: usize = 0;

    loop {
        terminal.draw(|frame| view::draw(frame, &app))?;
        let intent = tokio::select! {
            event = keys.next() => match event {
                // Press only. A terminal that reports releases and repeats
                // would otherwise act three times on one tap of the volume key.
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => app.on_key(key),
                // A resize needs no state change: the loop redraws at the top
                // of every pass, and that is the whole response.
                Some(Ok(_)) => Intent::Nothing,
                Some(Err(e)) => return Err(e).context("reading the keyboard"),
                None => return Ok(()),
            },
            _ = &mut stop => Intent::Quit,
            _ = tokio::time::sleep_until(pending.as_ref().map_or_else(tokio::time::Instant::now, |p| p.1)), if pending.is_some() => {
                if let Some((nudge, _)) = pending.take() {
                    flush(source, cli, nudge, &mut in_flight, &finished);
                }
                continue;
            },
            outcome = finishes.recv() => {
                if let Some(outcome) = outcome {
                    in_flight = in_flight.saturating_sub(1);
                    match outcome {
                        // The CLI's own sentence, which names the room and the fix.
                        Err(e) => app.status = Some(Status::bad(format!("{e:#}"))),
                        // What it was waiting for has happened, and the daemon's
                        // event is what shows it - so once nothing else is still
                        // on its way, the line has nothing left to say.
                        Ok(()) if in_flight == 0 => {
                            app.status.take_if(|status| status.kind == Kind::Busy);
                        }
                        Ok(()) => {}
                    }
                }
                continue;
            },
            _ = beat.tick() => {
                // Sent off rather than awaited here. A daemon that has wedged
                // answers nothing, and a read awaited in this loop would take
                // the keyboard down with it - the one failure where a person
                // most wants to be able to quit. It answers on the heartbeat
                // channel if it answers at all; a read that never comes back is
                // left to show as age instead.
                let source = source.clone();
                let heard = heard.clone();
                tokio::spawn(async move {
                    if let Ok(Ok(rooms)) = tokio::time::timeout(HEARTBEAT, source.snapshot()).await
                    {
                        let _ = heard.send(rooms);
                    }
                });
                continue;
            },
            refreshed = heartbeats.recv() => {
                if let Some(rooms) = refreshed {
                    app.apply(rooms);
                }
                continue;
            },
            update = updates.recv() => match update {
                Some(rooms) => {
                    app.apply(rooms);
                    continue;
                }
                // A daemon that stops does not close this channel - its players
                // leaving is an event, and the room list simply empties. The
                // channel closes when the *watcher* stops, so its own error is
                // the one worth printing; the fallback covers the bus going away
                // underneath it, which carries no error of its own.
                None => {
                    if let Some(watcher) = watcher.take()
                        && let Ok(Err(e)) = watcher.await
                    {
                        return Err(e).context("watching the daemon");
                    }
                    bail!("lost the session bus, so nothing is arriving any more")
                }
            },
        };
        match intent {
            Intent::Quit => {
                // The last few taps of a volume key are still on their way, and
                // this is the one place waiting for them is right: the process
                // is about to end, and a task would end with it.
                if let Some((nudge, _)) = pending.take()
                    && nudge.by != 0
                {
                    let step = execute(source, cli, Intent::Nudge(nudge));
                    let _ = tokio::time::timeout(WRITE_TIMEOUT, step).await;
                }
                return Ok(());
            }
            Intent::Nudge(nudge) => {
                let due = tokio::time::Instant::now() + COALESCE;
                pending = Some(match pending.take() {
                    None => (nudge, due),
                    Some((mut held, _)) => match held.absorb(nudge) {
                        // Same control: one change, sent when the run ends.
                        None => (held, due),
                        // A different control. What was held goes now, since
                        // the order the two were pressed in is the order the
                        // speakers should see them.
                        Some(other) => {
                            flush(source, cli, held, &mut in_flight, &finished);
                            (other, due)
                        }
                    },
                });
                continue;
            }
            _ => {}
        }
        // The CLI intents take about a second, and a second of nothing reads as
        // a dropped keypress; the loop redraws with this before anything else
        // happens.
        if let Some(waiting) = intent.waiting() {
            app.status = Some(Status::busy(waiting));
        }
        in_flight += 1;
        dispatch(source, cli, intent, finished.clone());
    }
}

/// Carry out an intent in a task of its own, and report how it went.
fn dispatch(
    source: &Source,
    cli: &action::Cli,
    intent: Intent,
    finished: mpsc::UnboundedSender<Result<()>>,
) {
    let source = source.clone();
    let cli = *cli;
    tokio::spawn(async move {
        let outcome =
            match tokio::time::timeout(WRITE_TIMEOUT, execute(&source, &cli, intent)).await {
                Ok(outcome) => outcome,
                Err(_) => Err(anyhow::anyhow!(
                    "gave up after {}s; the speakers did not answer",
                    WRITE_TIMEOUT.as_secs()
                )),
            };
        let _ = finished.send(outcome);
    });
}

/// Send one folded run of volume keys, unless it folded to nothing: `+5` then
/// `-5` is no change, and a command that changes nothing still costs a round
/// trip to the speaker.
fn flush(
    source: &Source,
    cli: &action::Cli,
    nudge: Nudge,
    in_flight: &mut usize,
    finished: &mpsc::UnboundedSender<Result<()>>,
) {
    if nudge.by == 0 {
        return;
    }
    *in_flight += 1;
    dispatch(source, cli, Intent::Nudge(nudge), finished.clone());
}

/// So many points on one volume: a group's (by its coordinator's room name), or
/// with `player` one speaker's own beneath its group's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nudge {
    pub room: String,
    pub by: i16,
    pub player: bool,
}

impl Nudge {
    /// Fold another step into this one if it is for the same control, or hand
    /// it back if it is not - in which case this one should go first.
    fn absorb(&mut self, other: Nudge) -> Option<Nudge> {
        if other.room == self.room && other.player == self.player {
            self.by += other.by;
            None
        } else {
            Some(other)
        }
    }
}

/// What a keypress asked for.
///
/// The field of the player intents is a bus name, because that is what MPRIS
/// is addressed by; the CLI intents carry a *room name*, because that is what
/// `-r` takes. Keeping both in one enum is what lets the key handling stay
/// ignorant of which of the two routes an action ends up on.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Nothing,
    Quit,
    PlayPause(String),
    Next(String),
    Previous(String),
    /// An MPRIS `LoopStatus`, verbatim: "None", "Track" or "Playlist".
    SetLoop(String, &'static str),
    SetShuffle(String, bool),
    /// A volume step. Not sent straight away: see [`COALESCE`].
    Nudge(Nudge),
    /// Mute a group (true) or unmute it, by its coordinator's room name.
    Mute(String, bool),
    /// Crossfade on or off, by its coordinator's room name. The one play mode
    /// that goes through the CLI, MPRIS having no property for it.
    Crossfade(String, bool),
    Group {
        coordinator: String,
        others: Vec<String>,
    },
    Ungroup(String),
    Party(String),
    PartyOff,
    Tv(String),
}

impl Intent {
    /// What to say while this runs, for the ones that take long enough to need
    /// saying. Only the slow CLI intents: each opens its own socket, resolves
    /// the topology and waits for the speakers. The bus intents land in
    /// milliseconds, where a "working" line would be a flicker, and a volume
    /// step already moved the bar when the key went down.
    fn waiting(&self) -> Option<&'static str> {
        match self {
            Self::Group { .. } => Some("grouping…"),
            Self::Ungroup(_) => Some("ungrouping…"),
            Self::Party(_) => Some("gathering every room…"),
            Self::PartyOff => Some("putting every room on its own…"),
            Self::Tv(_) => Some("switching to the TV input…"),
            _ => None,
        }
    }
}

/// Carry out one intent, by whichever route can express it.
async fn execute(source: &Source, cli: &action::Cli, intent: Intent) -> Result<()> {
    match intent {
        Intent::Nothing | Intent::Quit => Ok(()),
        // Folded by `drive` first: this is a run of keys, not one of them.
        Intent::Nudge(nudge) => cli.nudge_volume(&nudge.room, nudge.by, nudge.player).await,
        Intent::Mute(room, on) => cli.mute(&room, on).await,
        Intent::Crossfade(room, on) => cli.crossfade(&room, on).await,
        Intent::PlayPause(bus) => source
            .player(&bus)
            .await?
            .play_pause()
            .await
            .context("play/pause"),
        Intent::Next(bus) => source
            .player(&bus)
            .await?
            .next()
            .await
            .context("skipping forward"),
        Intent::Previous(bus) => source
            .player(&bus)
            .await?
            .previous()
            .await
            .context("skipping back"),
        Intent::SetLoop(bus, status) => source
            .player(&bus)
            .await?
            .set_loop_status(status)
            .await
            .context("setting repeat"),
        Intent::SetShuffle(bus, on) => source
            .player(&bus)
            .await?
            .set_shuffle(on)
            .await
            .context("setting shuffle"),
        Intent::Group {
            coordinator,
            others,
        } => cli.group(&coordinator, &others).await,
        Intent::Ungroup(room) => cli.ungroup(&room).await,
        Intent::Party(room) => cli.party(&room).await,
        Intent::PartyOff => cli.party_off().await,
        Intent::Tv(room) => cli.tv(&room).await,
    }
}

/// What is in front of the room list, if anything.
pub enum Overlay {
    None,
    /// The selected group's members and everyone who could join it.
    Group {
        cursor: usize,
    },
    Help,
    /// A question that has to be answered before something whole-house happens.
    Confirm {
        prompt: String,
        intent: Intent,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Something is in flight.
    Busy,
    /// A key that did nothing, and why.
    Note,
    /// It was tried and it failed.
    Bad,
}

pub struct Status {
    pub text: String,
    pub kind: Kind,
    /// When it went up, so an error can insist on being seen - see [`HOLD`].
    since: Instant,
}

impl Status {
    fn new(text: impl Into<String>, kind: Kind) -> Self {
        Self {
            text: text.into(),
            kind,
            since: Instant::now(),
        }
    }

    fn busy(text: impl Into<String>) -> Self {
        Self::new(text, Kind::Busy)
    }

    fn note(text: impl Into<String>) -> Self {
        Self::new(text, Kind::Note)
    }

    fn bad(text: impl Into<String>) -> Self {
        Self::new(text, Kind::Bad)
    }

    /// Whether a keypress right now should leave this alone.
    fn insists(&self) -> bool {
        self.kind == Kind::Bad && self.since.elapsed() < HOLD
    }
}

/// A row in the grouping overlay.
pub enum GroupRow {
    /// A room already playing with the selected group, and its own volume
    /// beneath the group mix.
    Member {
        room: String,
        volume: u8,
        muted: bool,
        fixed: bool,
        coordinator: bool,
    },
    /// A room somewhere else in the household, one keypress from joining.
    Outsider { room: String },
}

pub struct App {
    rooms: Vec<RoomSnapshot>,
    cursor: usize,
    overlay: Overlay,
    status: Option<Status>,
    /// When the daemon last answered. Not when anything last *changed*: a house
    /// where nothing is happening is quiet, not stale, and the two look
    /// identical from a screen that only timestamps events.
    contacted: Instant,
    /// When the bus first came back empty while rooms were still showing - see
    /// [`REPUBLISH_GRACE`].
    emptied: Option<Instant>,
}

impl App {
    fn new(rooms: Vec<RoomSnapshot>) -> Self {
        Self {
            rooms,
            cursor: 0,
            overlay: Overlay::None,
            status: None,
            contacted: Instant::now(),
            emptied: None,
        }
    }

    /// How long the screen has been unable to confirm what it is showing, once
    /// that is long enough to matter. `None` while it is current, because a
    /// clock that always reads zero is furniture.
    pub fn stale_for(&self) -> Option<Duration> {
        let since = self.contacted.elapsed();
        (since >= STALE).then_some(since)
    }

    /// A screen to draw in a test, and a way to age it. Only for the view's
    /// own tests, which need an `App` without a bus behind it.
    #[cfg(test)]
    pub fn new_for_test(rooms: Vec<RoomSnapshot>) -> Self {
        Self::new(rooms)
    }

    #[cfg(test)]
    pub fn set_contacted_for_test(&mut self, at: Instant) {
        self.contacted = at;
    }

    #[cfg(test)]
    fn set_emptied_for_test(&mut self, at: Instant) {
        self.emptied = Some(at);
    }

    pub fn rooms(&self) -> &[RoomSnapshot] {
        &self.rooms
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    pub fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }

    pub fn selected(&self) -> Option<&RoomSnapshot> {
        self.rooms.get(self.cursor)
    }

    /// Fold a fresh snapshot in, keeping the cursor on the room it was on.
    ///
    /// **Follow the room, not the row.** A regroup republishes every player, so
    /// the room someone was pointing at can stop being a row of its own and
    /// become a member of another group's row. Holding the index instead would
    /// move the selection to whatever landed there, with nobody having pressed
    /// anything.
    pub fn apply(&mut self, rooms: Vec<RoomSnapshot>) {
        // Anything that arrives is proof the daemon answered, whether it was
        // pushed or asked for.
        self.contacted = Instant::now();
        // An empty bus right after a full one is a republish in progress until
        // it has gone on too long to be one. The rooms on screen are kept, and
        // the footer says why nothing is moving.
        if rooms.is_empty() && !self.rooms.is_empty() {
            let since = *self.emptied.get_or_insert_with(Instant::now);
            if since.elapsed() < REPUBLISH_GRACE {
                if self.status.is_none() {
                    self.status = Some(Status::busy("the daemon is republishing…"));
                }
                return;
            }
        } else if self.emptied.take().is_some() {
            self.status.take_if(|status| status.kind == Kind::Busy);
        }
        let was = self.selected().map(|room| room.room.clone());
        self.rooms = rooms;
        if let Some(at) = was.and_then(|room| self.locate(&room)) {
            self.cursor = at;
        }
        self.cursor = self.cursor.min(self.rooms.len().saturating_sub(1));
        // The overlay lists the rooms of a group that has just changed shape,
        // so its cursor is measured against a list that no longer exists.
        if let Overlay::Group { cursor } = self.overlay {
            let rows = self.group_rows().len();
            self.overlay = match rows {
                0 => Overlay::None,
                rows => Overlay::Group {
                    cursor: cursor.min(rows - 1),
                },
            };
        }
    }

    /// The row that is this room, or failing that the row it is now playing in.
    fn locate(&self, room: &str) -> Option<usize> {
        self.rooms
            .iter()
            .position(|candidate| candidate.room == room)
            .or_else(|| {
                self.rooms
                    .iter()
                    .position(|candidate| candidate.members.iter().any(|member| member == room))
            })
    }

    /// The grouping overlay's rows: this group's coordinator, then the rest of
    /// its members in the order the daemon lists them, then every other room in
    /// the household. Rebuilt from the snapshot on demand rather than stored, so
    /// a regroup landing while it is open cannot leave it describing the old
    /// topology.
    ///
    /// The coordinator is found by name, not position: Sonos lists a group's
    /// players in its own order, and reading the first as the coordinator
    /// called the wrong room by that name whenever it was not.
    pub fn group_rows(&self) -> Vec<GroupRow> {
        let Some(selected) = self.selected() else {
            return Vec::new();
        };
        let member = |room: &String| {
            let at = selected
                .members
                .iter()
                .position(|candidate| candidate == room);
            GroupRow::Member {
                room: room.clone(),
                volume: at
                    .and_then(|at| selected.member_volumes.get(at))
                    .copied()
                    .unwrap_or(0),
                muted: at
                    .and_then(|at| selected.member_muted.get(at))
                    .copied()
                    .unwrap_or(false),
                fixed: at
                    .and_then(|at| selected.member_fixed.get(at))
                    .copied()
                    .unwrap_or(false),
                coordinator: selected.is_coordinator(room),
            }
        };
        let mut rows: Vec<GroupRow> = selected
            .members
            .iter()
            .filter(|room| selected.is_coordinator(room))
            .chain(selected.others())
            .map(member)
            .collect();
        for other in &self.rooms {
            if other.bus_name == selected.bus_name {
                continue;
            }
            rows.extend(
                other
                    .members
                    .iter()
                    .map(|room| GroupRow::Outsider { room: room.clone() }),
            );
        }
        rows
    }

    /// Where the overlay's cursor is, for drawing it.
    pub fn group_cursor(&self) -> usize {
        match self.overlay {
            Overlay::Group { cursor } => cursor,
            _ => 0,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Intent {
        // Every keypress answers whatever the last one had to say, so a stale
        // sentence never sits under a new action - except an error that has
        // only just gone up, which the keys buffered behind a slow action would
        // otherwise clear before it was ever seen.
        if !self.status.as_ref().is_some_and(Status::insists) {
            self.status = None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Intent::Quit;
        }
        if let Overlay::Confirm { intent, .. } = &self.overlay {
            let intent = intent.clone();
            self.overlay = Overlay::None;
            // Anything but yes is no. A question that took the whole keyboard
            // to dismiss would be worse than the one it is guarding.
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => intent,
                _ => Intent::Nothing,
            };
        }
        if matches!(self.overlay, Overlay::Help) {
            self.overlay = Overlay::None;
            return Intent::Nothing;
        }
        if let Overlay::Group { cursor } = self.overlay {
            return self.on_group_key(key, cursor);
        }
        self.on_room_key(key)
    }

    fn on_room_key(&mut self, key: KeyEvent) -> Intent {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Intent::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(self.rooms.len().saturating_sub(1));
                Intent::Nothing
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                Intent::Nothing
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                Intent::Nothing
            }
            KeyCode::Char(' ') => self.transport(Transport::PlayPause),
            KeyCode::Char('n') => self.transport(Transport::Next),
            KeyCode::Char('p') => self.transport(Transport::Previous),
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(STEP),
            KeyCode::Left | KeyCode::Char('-') => self.nudge_volume(-STEP),
            KeyCode::Char('m') => self.mute(),
            KeyCode::Char('r') => self.cycle_repeat(),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('x') => self.toggle_crossfade(),
            KeyCode::Char('g') => {
                if !self.rooms.is_empty() {
                    self.overlay = Overlay::Group { cursor: 0 };
                }
                Intent::Nothing
            }
            KeyCode::Char('t') => self.tv(),
            KeyCode::Char('P') => self.party(),
            _ => Intent::Nothing,
        }
    }

    /// Transport, where the source has any to offer.
    ///
    /// A room on its TV input has none - the television is the source and its
    /// own remote owns the buttons - and a queue that cannot be skipped says so
    /// per direction. Both are silent rather than explained: the row draws those
    /// controls dimmed, which has already said it.
    fn transport(&mut self, what: Transport) -> Intent {
        let Some(room) = self.selected() else {
            return Intent::Nothing;
        };
        if !room.transport_available() {
            return Intent::Nothing;
        }
        let bus = room.bus_name.clone();
        match what {
            Transport::PlayPause if room.can_play || room.can_pause => Intent::PlayPause(bus),
            Transport::Next if room.can_go_next => Intent::Next(bus),
            Transport::Previous if room.can_go_previous => Intent::Previous(bus),
            _ => Intent::Nothing,
        }
    }

    /// Step the group volume, and step it on screen too.
    ///
    /// What is *sent* is the step, not the result - see [`action::Cli::nudge_volume`]
    /// for why an absolute level computed from the screen is wrong. The screen
    /// still moves on the keypress, because the daemon's confirming event is a
    /// settling delay behind it and a bar that waited for it would stutter.
    fn nudge_volume(&mut self, by: i16) -> Intent {
        let Some(room) = self.rooms.get_mut(self.cursor) else {
            return Intent::Nothing;
        };
        // Silent, as transport is on TV input: the row has already said "fixed
        // volume" where the bar would be.
        if !room.volume_available() {
            return Intent::Nothing;
        }
        // A step on a muted room unmutes it - the Sonos app's slider does, and
        // so does the player itself on a relative set (checked: `vol +1` on a
        // muted room came back unmuted). Shown at once, like the step, so the
        // bar lights up under the key.
        room.muted = false;
        room.volume = stepped(room.volume, by);
        Intent::Nudge(Nudge {
            room: room.room.clone(),
            by,
            player: false,
        })
    }

    /// Mute or unmute the group. Flipped on screen as well as sent, for the
    /// reason repeat and shuffle are: the second press has to see the first,
    /// or two quick presses both mute.
    fn mute(&mut self) -> Intent {
        let Some(room) = self.rooms.get_mut(self.cursor) else {
            return Intent::Nothing;
        };
        // The CLI refuses mute on a fixed volume in the same breath as a step.
        if !room.volume_available() {
            return Intent::Nothing;
        }
        room.muted = !room.muted;
        Intent::Mute(room.room.clone(), room.muted)
    }

    /// off → all → one → off, skipping what the source cannot do. One key for
    /// three states, as the widget uses one button, and for the same reason:
    /// repeat-one is not worth its own place on the keyboard.
    ///
    /// Moved locally as well as sent: the next press computes from what is on
    /// screen, and two quick presses that both read the old state would both
    /// send the same value.
    fn cycle_repeat(&mut self) -> Intent {
        let Some(room) = self.rooms.get_mut(self.cursor) else {
            return Intent::Nothing;
        };
        if !room.can_repeat || !room.transport_available() {
            return Intent::Nothing;
        }
        let next = match room.loop_status.as_str() {
            // An empty status is a player that has not said; treating it as off
            // means the first press turns repeat on, rather than setting off to
            // off and looking broken.
            "" | "None" => "Playlist",
            "Playlist" if room.can_repeat_one => "Track",
            _ => "None",
        };
        room.loop_status = next.to_owned();
        Intent::SetLoop(room.bus_name.clone(), next)
    }

    /// Crossfade, flipped on screen as it is sent like the other two modes,
    /// and withdrawn like them where the source cannot do it. The guard is
    /// `canCrossfade`, which this household's firmware was checked to send
    /// alongside the other flags before it was trusted: a guard on a field the
    /// player leaves out would withdraw the key everywhere.
    fn toggle_crossfade(&mut self) -> Intent {
        let Some(room) = self.rooms.get_mut(self.cursor) else {
            return Intent::Nothing;
        };
        if !room.can_crossfade || !room.transport_available() {
            return Intent::Nothing;
        }
        room.crossfade = !room.crossfade;
        Intent::Crossfade(room.room.clone(), room.crossfade)
    }

    fn toggle_shuffle(&mut self) -> Intent {
        let Some(room) = self.rooms.get_mut(self.cursor) else {
            return Intent::Nothing;
        };
        if !room.can_shuffle || !room.transport_available() {
            return Intent::Nothing;
        }
        room.shuffle = !room.shuffle;
        Intent::SetShuffle(room.bus_name.clone(), room.shuffle)
    }

    fn tv(&mut self) -> Intent {
        let Some(room) = self.selected() else {
            return Intent::Nothing;
        };
        if !room.has_tv {
            return Intent::Nothing;
        }
        if room.on_tv {
            // Worth a sentence rather than silence: the key is offered on this
            // row, so nothing happening looks like a fault instead of an
            // answer.
            self.status = Some(Status::note("already on its TV input"));
            return Intent::Nothing;
        }
        Intent::Tv(room.room.clone())
    }

    /// Party takes every room in the house, so it asks first - the same reason
    /// the widget puts it behind its own button rather than under a bare click.
    ///
    /// Once the house *is* one group there is one row left and nothing left to
    /// gather, so the key turns around and offers to end it. That is the only
    /// useful shape it can have there, and it saves the one whole-house action
    /// that has no row of its own.
    fn party(&mut self) -> Intent {
        if self.rooms.len() == 1 && self.rooms[0].is_group() {
            self.overlay = Overlay::Confirm {
                prompt: "End the party and put every room on its own?".into(),
                intent: Intent::PartyOff,
            };
            return Intent::Nothing;
        }
        if self.rooms.len() < 2 {
            return Intent::Nothing;
        }
        let Some(host) = self.selected().map(|room| room.room.clone()) else {
            return Intent::Nothing;
        };
        self.overlay = Overlay::Confirm {
            prompt: format!("Group every room to {host}, playing what it plays?"),
            intent: Intent::Party(host),
        };
        Intent::Nothing
    }

    fn on_group_key(&mut self, key: KeyEvent, cursor: usize) -> Intent {
        let rows = self.group_rows();
        match key.code {
            KeyCode::Esc | KeyCode::Char('g') | KeyCode::Char('q') => {
                self.overlay = Overlay::None;
                Intent::Nothing
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.overlay = Overlay::Group {
                    cursor: (cursor + 1).min(rows.len().saturating_sub(1)),
                };
                Intent::Nothing
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.overlay = Overlay::Group {
                    cursor: cursor.saturating_sub(1),
                };
                Intent::Nothing
            }
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => {
                self.nudge_member(cursor, STEP)
            }
            KeyCode::Left | KeyCode::Char('-') => self.nudge_member(cursor, -STEP),
            KeyCode::Enter => self.join_or_leave(cursor),
            _ => Intent::Nothing,
        }
    }

    /// Step one speaker's own volume beneath its group's - the balance the
    /// group slider cannot express, which is what this overlay is for. On
    /// screen for the reason [`Self::nudge_volume`] gives, and sent as a step
    /// for the reason it gives too: a muted member reads as zero.
    fn nudge_member(&mut self, cursor: usize, by: i16) -> Intent {
        let rows = self.group_rows();
        let Some(GroupRow::Member {
            room, fixed: false, ..
        }) = rows.get(cursor)
        else {
            return Intent::Nothing;
        };
        let room = room.clone();
        if let Some(selected) = self.rooms.get_mut(self.cursor)
            && let Some(at) = selected.members.iter().position(|member| *member == room)
            && let Some(slot) = selected.member_volumes.get_mut(at)
        {
            *slot = (i16::from(*slot) + by).clamp(0, 100) as u8;
            // As for the group: a step unmutes the speaker it steps.
            if let Some(flag) = selected.member_muted.get_mut(at) {
                *flag = false;
            }
        }
        Intent::Nudge(Nudge {
            room,
            by,
            player: true,
        })
    }

    fn join_or_leave(&mut self, cursor: usize) -> Intent {
        let rows = self.group_rows();
        let Some(coordinator) = self.selected().map(|room| room.room.clone()) else {
            return Intent::Nothing;
        };
        match rows.get(cursor) {
            Some(GroupRow::Outsider { room }) => Intent::Group {
                coordinator,
                others: vec![room.clone()],
            },
            Some(GroupRow::Member {
                room,
                coordinator: false,
                ..
            }) => Intent::Ungroup(room.clone()),
            // The CLI refuses this and is right to: removing the coordinator is
            // not leaving, because the group *is* the coordinator. Said here
            // rather than let through to come back as an error, since the answer
            // is a different key and not a retry.
            Some(GroupRow::Member { room, .. }) => {
                self.status = Some(Status::note(format!(
                    "{room} coordinates this group; leave from another room, or end the party"
                )));
                Intent::Nothing
            }
            None => Intent::Nothing,
        }
    }
}

/// An MPRIS volume moved by so many points, snapped to whole percent - the
/// resolution a speaker actually stores, and the only one at which repeated
/// steps do not accumulate the error of 0.05 not being representable.
fn stepped(volume: f64, by: i16) -> f64 {
    ((volume * 100.0).round() + f64::from(by)).clamp(0.0, 100.0) / 100.0
}

enum Transport {
    PlayPause,
    Next,
    Previous,
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::State;

    /// A lone room, playing, at half volume, able to do everything.
    fn room(name: &str) -> RoomSnapshot {
        RoomSnapshot {
            bus_name: format!("org.mpris.MediaPlayer2.x2rock-{}", name.to_lowercase()),
            room: name.to_owned(),
            state: State::Playing,
            volume: 0.5,
            members: vec![name.to_owned()],
            member_volumes: vec![50],
            can_go_next: true,
            can_go_previous: true,
            can_pause: true,
            can_play: true,
            can_repeat: true,
            can_repeat_one: true,
            can_shuffle: true,
            can_crossfade: true,
            loop_status: "None".to_owned(),
            ..RoomSnapshot::default()
        }
    }

    fn press(app: &mut App, c: char) -> Intent {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn key(app: &mut App, code: KeyCode) -> Intent {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn nudge(room: &str, by: i16, player: bool) -> Intent {
        Intent::Nudge(Nudge {
            room: room.to_owned(),
            by,
            player,
        })
    }

    /// A volume key sends the step, never the level it thinks it produced. The
    /// screen is not the truth about a muted room, and the rule for stateless
    /// controls is relative or nothing.
    #[test]
    fn a_volume_key_sends_a_step_and_moves_the_bar() {
        let mut app = App::new(vec![room("Kitchen")]);
        assert_eq!(press(&mut app, '+'), nudge("Kitchen", 5, false));
        assert_eq!(press(&mut app, '+'), nudge("Kitchen", 5, false));
        assert_eq!(app.selected().map(|room| room.volume), Some(0.6));
        assert_eq!(press(&mut app, '-'), nudge("Kitchen", -5, false));
    }

    /// The bar stops at the ends; the step is still sent, because the speaker
    /// may not be where the bar is - a muted room reads as zero and is not.
    #[test]
    fn the_bar_stops_at_the_ends_and_repeated_steps_do_not_drift() {
        let mut app = App::new(vec![RoomSnapshot {
            volume: 0.98,
            ..room("Kitchen")
        }]);
        press(&mut app, '+');
        press(&mut app, '+');
        assert_eq!(app.selected().map(|room| room.volume), Some(1.0));
        assert_eq!(stepped(0.5, 5), 0.55);
        assert_eq!(stepped(stepped(0.5, 5), 5), 0.6);
    }

    /// A run of taps on one control is one command; a tap on a different
    /// control is a second command, after the first.
    #[test]
    fn steps_on_one_control_fold_and_on_two_do_not() {
        let mut held = Nudge {
            room: "Kitchen".into(),
            by: 5,
            player: false,
        };
        assert_eq!(
            held.absorb(Nudge {
                room: "Kitchen".into(),
                by: 5,
                player: false
            }),
            None
        );
        assert_eq!(held.by, 10);
        let other = Nudge {
            room: "Kitchen".into(),
            by: 5,
            player: true,
        };
        assert_eq!(held.absorb(other.clone()), Some(other));
        assert_eq!(held.by, 10);
    }

    /// A volume key on a muted room unmutes it, on screen as at the speaker,
    /// and the step still goes out - the player's relative set is what does
    /// the unmuting.
    #[test]
    fn a_volume_step_unmutes_the_room_it_steps() {
        let mut app = App::new(vec![RoomSnapshot {
            muted: true,
            ..room("Kitchen")
        }]);
        assert_eq!(press(&mut app, '+'), nudge("Kitchen", 5, false));
        assert!(app.selected().is_some_and(|room| !room.muted));
        assert_eq!(app.selected().map(|room| room.volume), Some(0.55));

        let mut grouped = App::new(vec![RoomSnapshot {
            members: vec!["Kitchen".into(), "Office".into()],
            member_volumes: vec![40, 60],
            member_muted: vec![false, true],
            ..room("Kitchen")
        }]);
        press(&mut grouped, 'g');
        key(&mut grouped, KeyCode::Down);
        assert_eq!(key(&mut grouped, KeyCode::Right), nudge("Office", 5, true));
        assert_eq!(
            grouped.selected().map(|room| room.member_muted.clone()),
            Some(vec![false, false])
        );
    }

    /// `m` flips mute and shows the flip at once, so a second press unmutes
    /// rather than muting again.
    #[test]
    fn mute_toggles_and_shows_at_once() {
        let mut app = App::new(vec![room("Kitchen")]);
        assert_eq!(press(&mut app, 'm'), Intent::Mute("Kitchen".into(), true));
        assert!(app.selected().is_some_and(|room| room.muted));
        assert_eq!(press(&mut app, 'm'), Intent::Mute("Kitchen".into(), false));
        assert!(app.selected().is_some_and(|room| !room.muted));
    }

    /// Crossfade flips like shuffle, is refused like shuffle where the source
    /// says it cannot, and is withdrawn with the rest of the modes on TV input.
    #[test]
    fn crossfade_toggles_and_is_withdrawn_where_it_cannot_apply() {
        let mut app = App::new(vec![room("Kitchen")]);
        assert_eq!(
            press(&mut app, 'x'),
            Intent::Crossfade("Kitchen".into(), true)
        );
        assert_eq!(
            press(&mut app, 'x'),
            Intent::Crossfade("Kitchen".into(), false)
        );
        let mut radio = App::new(vec![RoomSnapshot {
            can_crossfade: false,
            ..room("Kitchen")
        }]);
        assert_eq!(press(&mut radio, 'x'), Intent::Nothing);
        let mut tv = App::new(vec![RoomSnapshot {
            on_tv: true,
            has_tv: true,
            ..room("Living Room")
        }]);
        assert_eq!(press(&mut tv, 'x'), Intent::Nothing);
    }

    /// A fixed volume has nothing for the volume keys to do - a Port's level is
    /// set on the amplifier - so they do nothing, as the row already says.
    #[test]
    fn the_volume_keys_do_nothing_on_a_fixed_volume() {
        let mut app = App::new(vec![RoomSnapshot {
            fixed_volume: true,
            ..room("Study")
        }]);
        assert_eq!(press(&mut app, '+'), Intent::Nothing);
        assert_eq!(press(&mut app, '-'), Intent::Nothing);
        assert_eq!(press(&mut app, 'm'), Intent::Nothing);
        // Everything else is still there.
        assert_eq!(
            press(&mut app, ' '),
            Intent::PlayPause("org.mpris.MediaPlayer2.x2rock-study".into())
        );
    }

    /// The same for one fixed-volume speaker inside a group: its row in the
    /// overlay takes no step, and its neighbours still do.
    #[test]
    fn a_fixed_volume_member_takes_no_step_in_the_overlay() {
        let mut app = App::new(vec![RoomSnapshot {
            members: vec!["Kitchen".into(), "Study".into()],
            member_volumes: vec![40, 0],
            member_fixed: vec![false, true],
            ..room("Kitchen")
        }]);
        press(&mut app, 'g');
        key(&mut app, KeyCode::Down);
        assert_eq!(key(&mut app, KeyCode::Right), Intent::Nothing);
        key(&mut app, KeyCode::Up);
        assert_eq!(key(&mut app, KeyCode::Right), nudge("Kitchen", 5, true));
    }

    /// A room on its TV input has no transport to drive, and the keys say so by
    /// doing nothing - the row has already drawn them as unavailable.
    #[test]
    fn the_transport_keys_do_nothing_on_tv_input() {
        let mut app = App::new(vec![RoomSnapshot {
            on_tv: true,
            has_tv: true,
            ..room("Living Room")
        }]);
        assert_eq!(press(&mut app, ' '), Intent::Nothing);
        assert_eq!(press(&mut app, 'n'), Intent::Nothing);
        assert_eq!(press(&mut app, 'p'), Intent::Nothing);
        assert_eq!(press(&mut app, 'r'), Intent::Nothing);
        assert_eq!(press(&mut app, 's'), Intent::Nothing);
    }

    /// A stream that cannot be skipped keeps its play/pause: `can_pause` alone
    /// is not the test for transport, which is why the TV flag exists.
    #[test]
    fn a_stream_keeps_play_pause_while_losing_next() {
        let mut app = App::new(vec![RoomSnapshot {
            is_live_stream: true,
            can_go_next: false,
            can_go_previous: false,
            ..room("Office")
        }]);
        assert_eq!(
            press(&mut app, ' '),
            Intent::PlayPause("org.mpris.MediaPlayer2.x2rock-office".into())
        );
        assert_eq!(press(&mut app, 'n'), Intent::Nothing);
    }

    /// One key, three states, in the widget's order.
    #[test]
    fn repeat_cycles_off_all_one_and_back() {
        let mut app = App::new(vec![room("Kitchen")]);
        let bus = "org.mpris.MediaPlayer2.x2rock-kitchen".to_owned();
        assert_eq!(
            press(&mut app, 'r'),
            Intent::SetLoop(bus.clone(), "Playlist")
        );
        assert_eq!(press(&mut app, 'r'), Intent::SetLoop(bus.clone(), "Track"));
        assert_eq!(press(&mut app, 'r'), Intent::SetLoop(bus, "None"));
    }

    /// A source with repeat but no repeat-one is a two-state cycle, not a
    /// three-state one that silently fails on the third.
    #[test]
    fn repeat_skips_one_where_the_source_cannot_do_it() {
        let mut app = App::new(vec![RoomSnapshot {
            can_repeat_one: false,
            ..room("Kitchen")
        }]);
        let bus = "org.mpris.MediaPlayer2.x2rock-kitchen".to_owned();
        assert_eq!(
            press(&mut app, 'r'),
            Intent::SetLoop(bus.clone(), "Playlist")
        );
        assert_eq!(press(&mut app, 'r'), Intent::SetLoop(bus, "None"));
    }

    /// A player that has said nothing about repeat is off, so the first press
    /// turns it on rather than setting off to off.
    #[test]
    fn an_unstated_repeat_turns_on_first() {
        let mut app = App::new(vec![RoomSnapshot {
            loop_status: String::new(),
            ..room("Kitchen")
        }]);
        assert_eq!(
            press(&mut app, 'r'),
            Intent::SetLoop("org.mpris.MediaPlayer2.x2rock-kitchen".into(), "Playlist")
        );
    }

    #[test]
    fn shuffle_toggles_and_is_refused_where_the_source_cannot() {
        let mut app = App::new(vec![room("Kitchen")]);
        let bus = "org.mpris.MediaPlayer2.x2rock-kitchen".to_owned();
        assert_eq!(press(&mut app, 's'), Intent::SetShuffle(bus.clone(), true));
        assert_eq!(press(&mut app, 's'), Intent::SetShuffle(bus, false));

        let mut radio = App::new(vec![RoomSnapshot {
            can_shuffle: false,
            ..room("Kitchen")
        }]);
        assert_eq!(press(&mut radio, 's'), Intent::Nothing);
    }

    /// Whole-house, so it asks - and the answer is a key, not the intent
    /// itself.
    #[test]
    fn party_asks_before_it_takes_the_house() {
        let mut app = App::new(vec![room("Kitchen"), room("Office")]);
        assert_eq!(press(&mut app, 'P'), Intent::Nothing);
        assert!(matches!(app.overlay(), Overlay::Confirm { .. }));
        assert_eq!(press(&mut app, 'y'), Intent::Party("Kitchen".into()));
        assert!(matches!(app.overlay(), Overlay::None));
    }

    #[test]
    fn anything_but_yes_cancels_the_question() {
        let mut app = App::new(vec![room("Kitchen"), room("Office")]);
        press(&mut app, 'P');
        assert_eq!(key(&mut app, KeyCode::Esc), Intent::Nothing);
        assert!(matches!(app.overlay(), Overlay::None));
    }

    /// One row left, everyone in it: there is nothing left to gather, so the
    /// same key ends the party instead.
    #[test]
    fn party_turns_around_once_the_house_is_one_group() {
        let mut app = App::new(vec![RoomSnapshot {
            members: vec!["Kitchen".into(), "Office".into()],
            member_volumes: vec![40, 60],
            ..room("Kitchen")
        }]);
        assert_eq!(press(&mut app, 'P'), Intent::Nothing);
        assert_eq!(press(&mut app, 'y'), Intent::PartyOff);
    }

    /// A single-speaker household has no party to hold.
    #[test]
    fn party_is_not_offered_to_one_room() {
        let mut app = App::new(vec![room("Kitchen")]);
        assert_eq!(press(&mut app, 'P'), Intent::Nothing);
        assert!(matches!(app.overlay(), Overlay::None));
    }

    #[test]
    fn tv_is_offered_where_there_is_one_and_not_where_the_room_is_already_on_it() {
        let mut plain = App::new(vec![room("Kitchen")]);
        assert_eq!(press(&mut plain, 't'), Intent::Nothing);
        assert!(plain.status().is_none());

        let mut bar = App::new(vec![RoomSnapshot {
            has_tv: true,
            ..room("Living Room")
        }]);
        assert_eq!(press(&mut bar, 't'), Intent::Tv("Living Room".into()));

        let mut already = App::new(vec![RoomSnapshot {
            has_tv: true,
            on_tv: true,
            ..room("Living Room")
        }]);
        assert_eq!(press(&mut already, 't'), Intent::Nothing);
        assert_eq!(
            already.status().map(|status| status.kind),
            Some(Kind::Note),
            "a key offered on this row that does nothing needs to say why"
        );
    }

    /// A household with Kitchen coordinating Office, and Bedroom on its own.
    /// Office is listed *first*, as Sonos is free to list it: the overlay has
    /// to find the coordinator by name, and this is the fixture that would
    /// catch it reading position instead.
    fn grouped() -> App {
        App::new(vec![
            RoomSnapshot {
                members: vec!["Office".into(), "Kitchen".into()],
                member_volumes: vec![60, 40],
                ..room("Kitchen")
            },
            room("Bedroom"),
        ])
    }

    /// The coordinator leads the overlay whatever position Sonos gave it, and
    /// carries its own volume rather than the volume of whoever was first.
    #[test]
    fn the_overlay_leads_with_the_coordinator_wherever_sonos_listed_it() {
        let app = grouped();
        let rows = app.group_rows();
        let Some(GroupRow::Member {
            room,
            volume,
            coordinator: true,
            ..
        }) = rows.first()
        else {
            panic!("the first row should be the coordinator");
        };
        assert_eq!(room, "Kitchen");
        assert_eq!(*volume, 40);
        let Some(GroupRow::Member {
            room,
            volume,
            coordinator: false,
            ..
        }) = rows.get(1)
        else {
            panic!("the second row should be the member");
        };
        assert_eq!(room, "Office");
        assert_eq!(*volume, 60);
        assert!(matches!(rows.get(2), Some(GroupRow::Outsider { room }) if room == "Bedroom"));
    }

    #[test]
    fn a_member_leaves_and_a_room_elsewhere_joins() {
        let mut app = grouped();
        press(&mut app, 'g');
        key(&mut app, KeyCode::Down);
        assert_eq!(
            key(&mut app, KeyCode::Enter),
            Intent::Ungroup("Office".into())
        );

        let mut app = grouped();
        press(&mut app, 'g');
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        assert_eq!(
            key(&mut app, KeyCode::Enter),
            Intent::Group {
                coordinator: "Kitchen".into(),
                others: vec!["Bedroom".into()],
            }
        );
    }

    /// The CLI refuses to remove a coordinator, because the group is the
    /// coordinator. Answered here instead of sent and returned as an error.
    #[test]
    fn the_coordinator_cannot_leave_its_own_group() {
        let mut app = grouped();
        press(&mut app, 'g');
        assert_eq!(key(&mut app, KeyCode::Enter), Intent::Nothing);
        let status = app.status().expect("a refusal that explains itself");
        assert_eq!(status.kind, Kind::Note);
        assert!(status.text.contains("Kitchen coordinates"));
    }

    /// The overlay's arrows step one speaker beneath the group mix, which is
    /// the balance the group volume cannot express - and step the speaker's own
    /// slot, found by name, not whichever slot shares its row number.
    #[test]
    fn the_overlay_steps_a_speakers_own_volume() {
        let mut app = grouped();
        press(&mut app, 'g');
        key(&mut app, KeyCode::Down);
        assert_eq!(key(&mut app, KeyCode::Right), nudge("Office", 5, true));
        assert_eq!(key(&mut app, KeyCode::Right), nudge("Office", 5, true));
        let rows = app.group_rows();
        assert!(matches!(
            rows.get(1),
            Some(GroupRow::Member { volume: 70, .. })
        ));
        assert!(matches!(
            rows.first(),
            Some(GroupRow::Member { volume: 40, .. })
        ));
    }

    /// `esc` means "back", and only means "quit" when there is nothing to go
    /// back from.
    #[test]
    fn esc_closes_an_overlay_before_it_quits() {
        let mut app = grouped();
        press(&mut app, 'g');
        assert_eq!(key(&mut app, KeyCode::Esc), Intent::Nothing);
        assert!(matches!(app.overlay(), Overlay::None));
        assert_eq!(key(&mut app, KeyCode::Esc), Intent::Quit);
    }

    /// A regroup republishes every player: Office stops being a row and
    /// becomes a member of Kitchen's. The cursor follows the room rather than
    /// staying on the index and pointing at whatever landed there.
    #[test]
    fn the_cursor_follows_its_room_into_another_group() {
        let mut app = App::new(vec![room("Bedroom"), room("Kitchen"), room("Office")]);
        app.cursor = 2;
        assert_eq!(
            app.selected().map(|room| room.room.as_str()),
            Some("Office")
        );

        app.apply(vec![
            room("Bedroom"),
            RoomSnapshot {
                members: vec!["Kitchen".into(), "Office".into()],
                member_volumes: vec![40, 60],
                ..room("Kitchen")
            },
        ]);
        assert_eq!(
            app.selected().map(|room| room.room.as_str()),
            Some("Kitchen"),
            "Office is now playing in Kitchen's group, so that is where the cursor is"
        );
    }

    /// The overlay is a view of a group that has just changed shape, so its
    /// cursor is measured against a list that no longer exists.
    #[test]
    fn a_regroup_under_the_open_overlay_brings_its_cursor_back_in_range() {
        let mut app = grouped();
        press(&mut app, 'g');
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.group_cursor(), 2);

        // Bedroom is gone and Office has left: one member, nobody else.
        app.apply(vec![room("Kitchen")]);
        assert_eq!(app.group_cursor(), 0);
    }

    /// The bus goes empty for a moment on every regroup, while the daemon
    /// drops its players and publishes them again. That moment must not clear
    /// the screen or close the overlay that asked for the regroup; only an
    /// emptiness that has outlasted a republish is believed.
    #[test]
    fn a_momentary_empty_bus_is_a_republish_not_an_empty_house() {
        let mut app = grouped();
        press(&mut app, 'g');
        app.apply(Vec::new());
        assert_eq!(app.rooms().len(), 2, "the last rooms stay up");
        assert!(matches!(app.overlay(), Overlay::Group { .. }));
        assert_eq!(app.status().map(|status| status.kind), Some(Kind::Busy));

        // The players come back: the note comes down with them.
        app.apply(vec![room("Kitchen"), room("Bedroom")]);
        assert!(app.status().is_none());

        // Empty for longer than a republish takes: believed.
        app.apply(Vec::new());
        app.set_emptied_for_test(Instant::now() - REPUBLISH_GRACE - Duration::from_secs(1));
        app.apply(Vec::new());
        assert!(app.rooms().is_empty());
    }

    /// Losing every player for good must not leave the cursor pointing past
    /// the end of an empty list.
    #[test]
    fn an_empty_household_leaves_nothing_selected() {
        let mut app = grouped();
        app.cursor = 1;
        app.set_emptied_for_test(Instant::now() - REPUBLISH_GRACE - Duration::from_secs(1));
        app.apply(Vec::new());
        assert_eq!(app.cursor, 0);
        assert!(app.selected().is_none());
        assert_eq!(press(&mut app, ' '), Intent::Nothing);
        assert_eq!(press(&mut app, 'g'), Intent::Nothing);
        assert!(matches!(app.overlay(), Overlay::None));
    }

    /// A quiet house and a dead one look identical on a screen that only
    /// timestamps changes, so what is timed is the last answer, not the last
    /// change - and it says nothing at all until saying it would mean
    /// something.
    #[test]
    fn the_screen_reports_how_long_it_has_been_since_an_answer() {
        let mut app = App::new(vec![room("Kitchen")]);
        assert_eq!(app.stale_for(), None);

        app.contacted = Instant::now() - STALE - Duration::from_secs(1);
        let since = app
            .stale_for()
            .expect("a screen that cannot vouch for itself");
        assert!(since >= STALE);

        // Anything arriving is an answer, pushed or asked for.
        app.apply(vec![room("Kitchen")]);
        assert_eq!(app.stale_for(), None);
    }

    /// Silence is not staleness. A house where nobody has touched anything all
    /// afternoon is still being answered for.
    #[test]
    fn a_quiet_household_is_not_reported_as_stale() {
        let mut app = App::new(vec![RoomSnapshot {
            state: State::Stopped,
            ..room("Kitchen")
        }]);
        app.contacted = Instant::now() - STALE + Duration::from_secs(5);
        assert_eq!(app.stale_for(), None);
    }

    /// Keys typed during a slow action arrive the instant it returns. A fresh
    /// error survives them; an old one, or a mere note, clears as any status
    /// does.
    #[test]
    fn a_fresh_error_survives_the_next_keypress_and_an_old_one_does_not() {
        let mut app = App::new(vec![room("Kitchen")]);
        app.status = Some(Status::bad("Kitchen did not answer"));
        key(&mut app, KeyCode::Down);
        assert!(app.status().is_some(), "wiped before anyone could read it");

        app.status = Some(Status {
            since: Instant::now() - HOLD - Duration::from_millis(1),
            ..Status::bad("Kitchen did not answer")
        });
        key(&mut app, KeyCode::Down);
        assert!(app.status().is_none());

        app.status = Some(Status::note("already on its TV input"));
        key(&mut app, KeyCode::Down);
        assert!(app.status().is_none(), "a note is answered by the next key");
    }

    /// Ctrl-C is not a signal in raw mode; it arrives as a keypress and has to
    /// be honoured as one, from wherever the user is.
    #[test]
    fn ctrl_c_quits_from_inside_an_overlay() {
        let mut app = grouped();
        press(&mut app, 'g');
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Intent::Quit
        );
    }
}
