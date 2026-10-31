//! The screen's controller: what the room list and the player pane show, where
//! focus is, and what every action does. It lives on the UI thread; the
//! household runtime reaches it through [`with_app`].
//!
//! Focus is a (zone, index) pair this file moves and the components draw
//! (components.slint, `Nav`). Slint has no directional focus of its own, and
//! the TV app wired its sidebar-to-pane moves by hand anyway, for reasons its
//! CLAUDE.md gives at length; here every move is a rule in [`App::navigate`].

mod alarms;
mod panels;
mod screens;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::sync::Kept;
use x2rock::sonos::proto::{Group, Repeat};

use crate::art::Decoded;
use crate::art::cache::{
    Cache as ArtCache, Key as ArtKey, Size as ArtSize, State as ArtState, Surface as ArtSurface,
};
use crate::household::{Command, Handle, RoomDetails, Status, Update};
use crate::input::{Action, Phase};
use crate::model::{self, GroupLive, Household, RoomInfo, SeekBurst, VolumeHold};
use crate::palette::{self, Rgb};
use crate::prefs::Prefs;
use crate::theme;
use crate::{AppWindow, Glyph, Icons, Nav, PaneButton, PaneState, RoomRow, SpeakerRow};
use panels::OpenPanel;
use screens::{Kind, ScreenState};

/// Focus zones; ui/home.slint's `Zones` holds the same numbers.
pub mod zone {
    pub const ROOMS: i32 = 1;
    pub const SETTINGS: i32 = 2;
    pub const VOLUME: i32 = 10;
    pub const SEEK: i32 = 11;
    pub const TRANSPORT: i32 = 12;
    pub const PLACES: i32 = 13;
    pub const SPEAKERS: i32 = 14;
    pub const TV_BUTTONS: i32 = 15;
    pub const ALARM: i32 = 16;
    pub const SETTINGS_PANEL: i32 = 20;
    pub const ROOM_PANEL: i32 = 21;
    pub const HEADER: i32 = 30;
    pub const LIST: i32 = 31;
    pub const BAR: i32 = 32;
    pub const PIN: i32 = 33;
    pub const SIDE: i32 = 34;
    /// Full screen: nothing to press, so one zone that any key closes.
    pub const FULL: i32 = 40;
    /// Search's field (0) and its Search button (1).
    pub const FIELD: i32 = 35;
    pub const KEYS: i32 = 50;
}

thread_local! {
    static APP: RefCell<Option<Rc<RefCell<App>>>> = const { RefCell::new(None) };
}

/// Run `f` on the app, from anywhere on the UI thread. Nothing happens before
/// the app exists or after it is gone.
pub fn with_app(f: impl FnOnce(&mut App)) {
    let app = APP.with(|cell| cell.borrow().clone());
    if let Some(app) = app {
        f(&mut app.borrow_mut());
    }
}

pub fn install(app: App) {
    APP.with(|cell| *cell.borrow_mut() = Some(Rc::new(RefCell::new(app))));
}

/// A room in the list, in its sorted place.
#[derive(Clone, Debug)]
struct RoomEntry {
    group_id: String,
    coordinator: String,
    players: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Shuffle,
    Previous,
    PlayPause,
    Next,
    Repeat,
    RateUp,
    RateDown,
    Unfavorite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Queue,
    Browse,
    Search,
    SaveFavorite,
    Group,
    Configure,
    FullScreen,
}

impl Place {
    fn label(self) -> &'static str {
        match self {
            Place::Queue => "Queue",
            Place::Browse => "Browse",
            Place::Search => "Search",
            Place::SaveFavorite => "Save favorite",
            Place::Group => "Group",
            Place::Configure => "Configure",
            Place::FullScreen => "Full screen",
        }
    }
}

/// A room picked up to move in the list (a hold on it).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Moving {
    /// Up and Down carry it; Select or Back puts it down.
    Carrying(String),
    /// Left offered to play everywhere from it: Select confirms, a second Left
    /// also evens the volume first.
    OfferingParty(String),
}

/// A Select that went down on a control: its release clicks it, unless a hold
/// fired first. A release whose press began elsewhere clicks nothing (the TV
/// app's `ignoreUnpressedRelease`).
#[derive(Clone, Copy, Debug)]
struct Press {
    zone: i32,
    index: i32,
    held: bool,
}

struct ReadyArt {
    image: slint::Image,
    colors: Option<(Rgb, Rgb)>,
}

/// Every list the window shows, kept and updated in place (see `sync`).
pub struct Models {
    pub rooms: Kept<RoomRow>,
    pub transport: Kept<PaneButton>,
    pub places: Kept<PaneButton>,
    pub speakers: Kept<SpeakerRow>,
    pub list_rows: Kept<crate::ListRow>,
    pub list_actions: Kept<crate::HeaderButton>,
    pub side: Kept<crate::SideItem>,
    pub panel_items: Kept<crate::PanelItem>,
}

impl Models {
    fn attach(ui: &AppWindow) -> Self {
        let m = Models {
            rooms: Kept::new(),
            transport: Kept::new(),
            places: Kept::new(),
            speakers: Kept::new(),
            list_rows: Kept::new(),
            list_actions: Kept::new(),
            side: Kept::new(),
            panel_items: Kept::new(),
        };
        ui.set_rooms(m.rooms.model());
        ui.set_transport(m.transport.model());
        ui.set_places(m.places.model());
        ui.set_speakers(m.speakers.model());
        ui.set_list_rows(m.list_rows.model());
        ui.set_list_actions(m.list_actions.model());
        ui.set_side(m.side.model());
        ui.set_panel_items(m.panel_items.model());
        m
    }
}

/// The track that played before, as (title, optional artist/album).
type PreviousTrack = (String, Option<String>);
/// What was asked (track and queue version), and the previous track if known.
type PreviousQuery = (String, Option<PreviousTrack>);

pub struct App {
    ui: slint::Weak<AppWindow>,
    models: Models,
    handle: Option<Handle>,
    runtime: Option<tokio::runtime::Handle>,
    prefs: Prefs,
    house: Household,
    status: Status,
    /// The room chosen: the group shown, and a speaker of it to follow the room
    /// by when a regroup gives it a new id.
    selected: Option<String>,
    anchor: Option<String>,
    focus: (i32, i32),
    /// The index each zone was last left at, so coming back lands there.
    memory: HashMap<i32, i32>,
    rooms: Vec<RoomEntry>,
    transport: Vec<Transport>,
    places: Vec<Place>,
    speakers: Vec<String>,
    pane_zones: Vec<i32>,
    press: Option<Press>,
    /// A Left whose first press landed on a room row: only its repeats pick
    /// that room up. A Left held from the pane walks to the list and must stop
    /// there, not go on to pick up, offer and confirm Play everywhere.
    left_from_room: bool,
    /// What was sent, for the tests: there is no household behind them.
    #[cfg(test)]
    pub(crate) sent: std::cell::RefCell<Vec<Command>>,
    /// Added to the clock, for the tests: holds and bursts are timed.
    #[cfg(test)]
    pub(crate) skew_ms: u64,
    #[cfg(test)]
    pub(crate) last_art_failure_log: Option<String>,
    menu_down: bool,
    hold_timer: slint::Timer,
    moving: Option<Moving>,
    volume_holds: HashMap<String, VolumeHold>,
    speaker_holds: HashMap<String, VolumeHold>,
    seek_burst: SeekBurst,
    art: ArtCache<ReadyArt>,
    art_loader: crate::art::Loader,
    art_retry: slint::Timer,
    notice_timer: slint::Timer,
    tick: slint::Timer,
    started: Instant,
    touch: bool,
    panel: Option<OpenPanel>,
    room_details: Option<RoomDetails>,
    /// Screens opened from the home screen, the top one in front.
    stack: Vec<ScreenState>,
    readonly: bool,
    /// The on-screen keyboard, while it is up: the key focused, and the field
    /// focus goes back to.
    keyboard: Option<usize>,
    keys: crate::keyboard::Layout,
    /// The household's favourites as Browse last read them: a preset names the
    /// one playing.
    pub(crate) favorites_cache: Vec<x2rock::sonos::proto::Favorite>,
    /// The keyboard's Done key's label, as last drawn.
    keyboard_label: &'static str,
    /// A speaker whose room becomes the selected one once the household says
    /// it has formed: a preset's first.
    /// The speaker, and how many speakers its room will have.
    pending_select: Option<(String, usize, Instant)>,
    /// Each group's track, and the ratings it can be given: asked once a track.
    ratings: HashMap<String, (String, Option<x2rock::play::RatingOffers>)>,
    /// What plays before each room's track, by group: asked once a track and
    /// queue version, and its answer.
    previous: HashMap<String, PreviousQuery>,
    /// The last action from each source, keys and pad, and when: the Deck in
    /// Desktop Mode can deliver one press as both (see `pad_action`).
    last_key: Option<(Action, Phase, Instant)>,
    /// The last command refused, for Send a report.
    pub(super) last_refusal: Option<String>,
    last_pad: Option<(Action, Phase, Instant)>,
    request_id: u64,
}

const HOLD: Duration = Duration::from_millis(500);
const NOTICE: Duration = Duration::from_secs(4);

impl App {
    pub fn new(ui: &AppWindow, prefs: Prefs) -> Self {
        App {
            ui: ui.as_weak(),
            models: Models::attach(ui),
            handle: None,
            runtime: None,
            prefs,
            house: Household::default(),
            status: Status::Searching,
            selected: None,
            anchor: None,
            focus: (zone::ROOMS, 0),
            memory: HashMap::new(),
            rooms: Vec::new(),
            transport: Vec::new(),
            places: Vec::new(),
            speakers: Vec::new(),
            pane_zones: Vec::new(),
            press: None,
            left_from_room: false,
            #[cfg(test)]
            sent: Default::default(),
            #[cfg(test)]
            skew_ms: 0,
            #[cfg(test)]
            last_art_failure_log: None,
            menu_down: false,
            hold_timer: slint::Timer::default(),
            moving: None,
            volume_holds: HashMap::new(),
            speaker_holds: HashMap::new(),
            seek_burst: SeekBurst::default(),
            art: ArtCache::default(),
            art_loader: crate::art::Loader::default(),
            art_retry: slint::Timer::default(),
            notice_timer: slint::Timer::default(),
            tick: slint::Timer::default(),
            started: Instant::now(),
            touch: false,
            panel: None,
            room_details: None,
            stack: Vec::new(),
            readonly: false,
            keyboard: None,
            keys: crate::keyboard::Layout::new(),
            favorites_cache: Vec::new(),
            keyboard_label: "",
            pending_select: None,
            ratings: HashMap::new(),
            previous: HashMap::new(),
            last_key: None,
            last_refusal: None,
            last_pad: None,
            request_id: 0,
        }
    }

    pub fn next_request_id(&mut self) -> u64 {
        self.request_id = self.request_id.wrapping_add(1);
        self.request_id
    }

    #[cfg(test)]
    pub fn requests(&self) -> Option<crate::app::screens::ScreenRequests> {
        self.stack.last().map(|s| s.requests)
    }

    /// Record pending fetch requests as interrupted and clear active identities
    /// on reconnect so late responses from before reconnect are ignored.
    pub fn reconnected(&mut self) {
        for screen in &mut self.stack {
            screen.interrupted.record(&screen.requests);
            screen.requests = crate::app::screens::ScreenRequests::default();
        }
    }

    pub fn connect(&mut self, handle: Handle, runtime: tokio::runtime::Handle) {
        self.handle = Some(handle);
        self.runtime = Some(runtime);
    }

    /// Reads only: lists and settings are fetched, but nothing that changes the
    /// household reaches the speakers. The screenshot of a live household.
    pub fn connect_readonly(&mut self, handle: Handle, runtime: tokio::runtime::Handle) {
        self.handle = Some(handle);
        self.runtime = Some(runtime);
        self.readonly = true;
    }

    pub fn has_rooms(&self) -> bool {
        !self.rooms.is_empty()
    }

    /// Where focus is, for the screenshot's report.
    pub fn focus_report(&self) -> String {
        let list = self.stack.last().map(|s| {
            (
                s.actions.len(),
                s.cells.get(self.focus.1.max(0) as usize).copied(),
            )
        });
        format!("focus {:?}, list {:?}", self.focus, list)
    }

    pub fn checks_for_updates(&self) -> bool {
        self.prefs.check_for_updates != Some(false)
    }

    fn now_ms(&self) -> u64 {
        let ms = self.started.elapsed().as_millis() as u64;
        #[cfg(test)]
        let ms = ms + self.skew_ms;
        ms
    }

    fn send(&self, command: Command) {
        if self.readonly && !matches!(command, Command::Fetch(_) | Command::ReadRoom { .. }) {
            let msg = format_readonly_command(&command);
            eprintln!("{msg}");
            return;
        }
        #[cfg(test)]
        self.sent.borrow_mut().push(command.clone());
        if let Some(handle) = &self.handle {
            handle.send(command);
        }
    }

    // ----- What the household says -------------------------------------------------

    pub fn update(&mut self, update: Update) {
        match update {
            Update::Status(status) => {
                let was_connected = matches!(self.status, Status::Connected);
                let now_connected = matches!(status, Status::Connected);
                if matches!(
                    status,
                    Status::Reconnecting(_) | Status::Searching | Status::Connecting
                ) {
                    self.reconnected();
                }
                self.status = status;
                if !was_connected && now_connected {
                    self.refetch_interrupted_screens();
                }
                self.render_rooms();
            }
            Update::Household(house) => {
                let track = |app: &App| {
                    app.stack
                        .last()
                        .and_then(|s| app.house.live.get(&s.group))
                        .and_then(|l| l.metadata.as_ref())
                        .and_then(|m| m.track())
                        .and_then(|t| t.name.clone())
                };
                let before = track(self);
                self.house = *house;
                self.follow_selection();
                self.queue_follows(before);
                self.render();
            }
            Update::Fetched(fetched) => {
                if let crate::household::Fetched::RatingOffers {
                    group,
                    track,
                    offers,
                } = &fetched
                {
                    self.ratings
                        .insert(group.clone(), (track.clone(), offers.clone()));
                }
                if let crate::household::Fetched::Previous { group, key, track } = &fetched
                    && self
                        .previous
                        .get(group)
                        .is_some_and(|(asked, _)| asked == key)
                {
                    self.previous
                        .insert(group.clone(), (key.clone(), track.clone()));
                }
                self.fetched(fetched);
                self.render();
            }

            Update::Notice(text) => self.notice(&text),
            Update::Refusal(text) => {
                self.notice(&text);
                self.last_refusal = Some(text);
            }
            Update::RoomDetails(details) => {
                self.room_details_arrived(details);
                self.render();
            }
        }
    }

    /// Say something once at the foot of the screen.
    pub fn notice(&mut self, text: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        ui.set_notice(text.into());
        let weak = self.ui.clone();
        self.notice_timer
            .start(slint::TimerMode::SingleShot, NOTICE, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_notice(SharedString::new());
                }
            });
    }

    /// Keep the room chosen through a regroup: the group now holding its
    /// speaker is the same room to a listener. Only if that fails does the
    /// selection fall to the first room.
    fn follow_selection(&mut self) {
        self.rooms = self.sorted_rooms();
        if self
            .pending_select
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() > Duration::from_secs(30))
        {
            // A speaker left out never joins: the room is as formed as it gets.
            self.pending_select = None;
        }
        if let Some((player, size, _)) = self.pending_select.clone() {
            // Followed until its room has formed, then left to the viewer.
            if let Some(entry) = self.rooms.iter().find(|r| r.players.contains(&player)) {
                self.selected = Some(entry.group_id.clone());
                self.anchor = Some(player);
                if entry.players.len() >= size {
                    self.pending_select = None;
                }
            }
        }
        let still = self
            .selected
            .as_ref()
            .is_some_and(|id| self.rooms.iter().any(|r| &r.group_id == id));
        if !still {
            let by_anchor = self
                .anchor
                .as_ref()
                .and_then(|anchor| self.rooms.iter().find(|r| r.players.contains(anchor)));
            self.selected = by_anchor.or(self.rooms.first()).map(|r| r.group_id.clone());
            if self.anchor.is_none() {
                self.anchor = self.rooms.first().map(|r| r.coordinator.clone());
            }
        }
        // The list selects the room focus rests on, so focus stays on the
        // selected room's row as rows come and go around it.
        if self.focus.0 == zone::ROOMS && !self.rooms.is_empty() {
            self.focus.1 = self.selected_index().unwrap_or(0) as i32;
        }
    }

    fn groups(&self) -> &[Group] {
        self.house
            .groups
            .as_ref()
            .map(|g| g.groups.as_slice())
            .unwrap_or(&[])
    }

    fn player_name(&self, id: &str) -> String {
        self.house
            .groups
            .as_ref()
            .and_then(|g| g.player(id))
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    /// Placed rooms first in their order, every other room alphabetically. A
    /// group stands where its best-placed speaker does.
    fn sorted_rooms(&self) -> Vec<RoomEntry> {
        let ranks = &self.prefs.room_ranks;
        let rank = |g: &Group| {
            g.player_ids
                .iter()
                .filter_map(|p| ranks.iter().position(|r| r == p))
                .min()
        };
        let mut groups: Vec<&Group> = self.groups().iter().collect();
        groups.sort_by(|a, b| match (rank(a), rank(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => self
                .player_name(&a.coordinator_id)
                .to_lowercase()
                .cmp(&self.player_name(&b.coordinator_id).to_lowercase()),
        });
        groups
            .into_iter()
            .map(|g| RoomEntry {
                group_id: g.id.clone(),
                coordinator: g.coordinator_id.clone(),
                players: g.player_ids.clone(),
            })
            .collect()
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.rooms.iter().position(|r| &r.group_id == id)
    }

    fn selected_entry(&self) -> Option<&RoomEntry> {
        self.selected_index().map(|i| &self.rooms[i])
    }

    fn live(&self, group: &str) -> GroupLive {
        self.house.live.get(group).cloned().unwrap_or_default()
    }

    fn room_label(&self, entry: &RoomEntry) -> String {
        model::group_label(&self.player_name(&entry.coordinator), entry.players.len())
    }

    fn has_tv(&self, entry: &RoomEntry) -> Option<String> {
        let groups = self.house.groups.as_ref()?;
        entry
            .players
            .iter()
            .find(|p| groups.player(p).is_some_and(|p| p.has_tv()))
            .cloned()
    }

    // ----- Rendering ---------------------------------------------------------------

    pub fn render(&mut self) {
        self.render_rooms();
        self.render_pane();
        self.render_screen();
        self.render_keyboard();
        self.render_panel();
        self.apply_theme();
        self.push_focus();
    }

    fn art_image(&mut self, url: Option<&str>) -> Option<slint::Image> {
        self.sized_art_image(url, ArtSize::Full)
    }

    fn thumbnail(&mut self, url: Option<&str>) -> Option<slint::Image> {
        self.sized_art_image(url, ArtSize::Thumbnail)
    }

    fn sized_art_image(&mut self, url: Option<&str>, size: ArtSize) -> Option<slint::Image> {
        let url = url?;
        let key = ArtKey::new(url, size);
        match self.art.get(&key, Instant::now()) {
            Some(ArtState::Ready(art)) => Some(art.image.clone()),
            Some(_) => None,
            None => {
                // Demo covers are already in hand at full size, without a
                // runtime. Reuse those shared pixels for its thumbnails.
                if self.runtime.is_none()
                    && size == ArtSize::Thumbnail
                    && let Some(ArtState::Ready(art)) = self
                        .art
                        .get(&ArtKey::new(url, ArtSize::Full), Instant::now())
                {
                    return Some(art.image.clone());
                }
                self.request_art(key);
                None
            }
        }
    }

    fn art_colors(&self, url: Option<&str>) -> Option<(Rgb, Rgb)> {
        match self.art.peek(&ArtKey::new(url?, ArtSize::Full)) {
            Some(ArtState::Ready(art)) => art.colors,
            _ => None,
        }
    }

    fn request_art(&mut self, key: ArtKey) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        self.art.loading(key.clone());
        let loader = self.art_loader.clone();
        runtime.spawn(async move {
            let result = loader.load(key.url.clone(), key.size).await;
            let _ =
                slint::invoke_from_event_loop(move || with_app(|app| app.art_arrived(key, result)));
        });
    }

    /// For the demo and the screenshots: a cover already in hand.
    pub fn put_art(&mut self, url: &str, decoded: Decoded) {
        self.art_arrived(ArtKey::new(url, ArtSize::Full), Ok(decoded));
    }

    fn art_arrived(&mut self, key: ArtKey, result: Result<Decoded, crate::art::LoadError>) {
        match result {
            Ok(d) => {
                let bytes = d.rgba.len();
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &d.rgba, d.width, d.height,
                );
                self.art.ready(
                    key,
                    ReadyArt {
                        image: slint::Image::from_rgba8(buffer),
                        colors: d.colors,
                    },
                    bytes,
                );
            }
            Err(e) => {
                let retry_at = Instant::now() + e.retry_after();
                let line = format_art_failure(&key.url, &e.error);
                eprintln!("{line}");
                #[cfg(test)]
                {
                    self.last_art_failure_log = Some(line);
                }
                self.art.failed(key, retry_at);
            }
        }
        self.render();
    }

    fn finish_art_render(&mut self) {
        let now = Instant::now();
        for url in self.art.end(now) {
            self.request_art(url);
        }
        if let Some(deadline) = self.art.next_retry() {
            // Slint uses whole milliseconds. Do not turn a sub-ms remainder
            // into repeated immediate wakeups just before the deadline.
            let delay = deadline
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            self.art_retry
                .start(slint::TimerMode::SingleShot, delay, move || {
                    with_app(|app| app.render());
                });
        } else {
            self.art_retry.stop();
        }
    }

    fn render_rooms(&mut self) {
        self.art.begin(ArtSurface::Rooms);
        self.render_room_rows();
        self.finish_art_render();
    }

    fn render_room_rows(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let moving_group = match &self.moving {
            Some(Moving::Carrying(g)) | Some(Moving::OfferingParty(g)) => Some(g.clone()),
            None => None,
        };
        let this_tv = self.prefs.tv_player_id.clone();
        let entries = self.rooms.clone();
        let mut rows = Vec::with_capacity(entries.len());
        for entry in &entries {
            let live = self.live(&entry.group_id);
            let info = RoomInfo::from_live(&live);
            let mut lines = model::room_row_lines(&info, live.state.as_deref());
            let moving = moving_group.as_ref() == Some(&entry.group_id);
            if moving {
                lines = match &self.moving {
                    Some(Moving::OfferingParty(_)) => vec![
                        "Play everywhere? Select".into(),
                        "Left: also even volume".into(),
                    ],
                    _ => vec![
                        "Moving \u{00B7} Up/Down, Select".into(),
                        "Left: play everywhere".into(),
                    ],
                };
            }
            let art = self.thumbnail(info.art_url.as_deref());
            let playing = live.playing();
            let has_tv = self.has_tv(entry);
            let is_this_tv = this_tv
                .as_ref()
                .is_some_and(|tv| entry.players.contains(tv));
            let tv_badge = if is_this_tv {
                2
            } else if has_tv.is_some() && !info.on_tv_input {
                1
            } else {
                0
            };
            rows.push(RoomRow {
                id: entry.group_id.as_str().into(),
                name: self.room_label(entry).into(),
                line1: lines.first().cloned().unwrap_or_default().into(),
                line2: lines.get(1).cloned().unwrap_or_default().into(),
                line3: SharedString::new(),
                has_art: art.is_some(),
                art: art.unwrap_or_default(),
                glyph: if info.on_tv_input {
                    Glyph::Tv
                } else if info.is_radio {
                    Glyph::Radio
                } else {
                    Glyph::None
                },
                playing,
                dim: !playing,
                tv_badge,
                offline: self.house.offline_in(&self.player_name(&entry.coordinator)) > 0,
                muted: info.muted,
                selected: self.selected.as_ref() == Some(&entry.group_id),
                moving,
            });
        }
        self.models.rooms.set(rows);
        let (status, loading) = match &self.status {
            Status::Searching => ("Looking for speakers\u{2026}".to_owned(), true),
            Status::Connecting => ("Connecting\u{2026}".to_owned(), true),
            Status::Connected => (String::new(), false),
            Status::Reconnecting(why) => (format!("{why}. Trying again\u{2026}"), true),
        };
        ui.set_rooms_status(status.into());
        ui.set_rooms_loading(loading && entries.is_empty());
    }

    fn render_pane(&mut self) {
        self.art.begin(ArtSurface::Pane);
        self.render_pane_contents();
        self.finish_art_render();
    }

    fn render_pane_contents(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(entry) = self.selected_entry().cloned() else {
            ui.set_pane(PaneState::default());
            self.models.transport.set(vec![]);
            self.models.places.set(vec![]);
            self.models.speakers.set(vec![]);
            self.transport.clear();
            self.places.clear();
            self.speakers.clear();
            self.pane_zones.clear();
            return;
        };
        let live = self.live(&entry.group_id);
        let info = RoomInfo::from_live(&live);
        let activity = model::activity(live.state.as_deref(), info.has_source());
        let now = Instant::now();
        let duration = live.duration_ms();
        let position = live.live_position(now);
        let tv = info.on_tv_input;
        let actions = live.actions;
        let modes = live.modes;
        // The control focus is on, by what it is: rows are rebuilt from what the
        // player allows, and an index would land on whatever moved into its place.
        let (fz, fi) = (self.focus.0, self.focus.1.max(0) as usize);
        let held_transport = (fz == zone::TRANSPORT)
            .then(|| self.transport.get(fi).copied())
            .flatten();
        let held_place = (fz == zone::PLACES)
            .then(|| self.places.get(fi).copied())
            .flatten();
        let held_speaker = (fz == zone::SPEAKERS)
            .then(|| self.speakers.get(fi).cloned())
            .flatten();

        // The transport, as the player allows it.
        self.transport.clear();
        if !tv {
            if actions.can_shuffle {
                self.transport.push(Transport::Shuffle);
            }
            if actions.can_skip_back {
                self.transport.push(Transport::Previous);
            }
            if actions.can_play || actions.can_pause || live.playing() {
                self.transport.push(Transport::PlayPause);
            }
            if actions.can_skip {
                self.transport.push(Transport::Next);
            }
            if actions.can_repeat {
                self.transport.push(Transport::Repeat);
            }
            // Ratings, where the track's service publishes them: asked once a track.
            let track = info.track_name.clone().unwrap_or_default();
            match self.ratings.get(&entry.group_id) {
                Some((asked, Some(offers))) if *asked == track => {
                    use x2rock::sonos::smapi::RatingKind;
                    if offers.kinds.contains(&RatingKind::Up) {
                        self.transport.push(Transport::RateUp);
                    }
                    if offers.kinds.contains(&RatingKind::Unfavorite) {
                        self.transport.push(Transport::Unfavorite);
                    }
                    if offers.kinds.contains(&RatingKind::Down) {
                        self.transport.push(Transport::RateDown);
                    }
                }
                Some((asked, None)) if *asked == track => {}
                _ if !track.is_empty() => {
                    self.ratings
                        .insert(entry.group_id.clone(), (track.clone(), None));
                    self.send(Command::Fetch(crate::household::Fetch::RatingOffers {
                        group: entry.group_id.clone(),
                        track,
                    }));
                }
                _ => {}
            }
        }
        // What Prev goes to: read from the queue once a track and queue version,
        // only where the player allows a skip back.
        // Not under shuffle: the slot before is the queue's order, not the order it plays in.
        if !tv && actions.can_skip_back && !modes.shuffle {
            let key = format!(
                "{}|{}",
                info.track_name.clone().unwrap_or_default(),
                live.queue_version.clone().unwrap_or_default()
            );
            if self
                .previous
                .get(&entry.group_id)
                .is_none_or(|(asked, _)| *asked != key)
            {
                self.previous
                    .insert(entry.group_id.clone(), (key.clone(), None));
                self.send(Command::Fetch(crate::household::Fetch::Previous {
                    group: entry.group_id.clone(),
                    key,
                }));
            }
        }
        let heart = self
            .ratings
            .get(&entry.group_id)
            .and_then(|(_, o)| o.as_ref())
            .is_some_and(|o| o.heart);
        let transport: Vec<PaneButton> = self
            .transport
            .iter()
            .map(|t| {
                let icons = ui.global::<Icons>();
                let icon = match t {
                    Transport::Shuffle if modes.shuffle => icons.get_shuffle_on(),
                    Transport::Shuffle => icons.get_shuffle(),
                    Transport::Previous => icons.get_skip_previous(),
                    Transport::PlayPause if live.playing() && !actions.can_pause => {
                        icons.get_stop()
                    }
                    Transport::PlayPause if live.playing() => icons.get_pause(),
                    Transport::PlayPause => icons.get_play_arrow(),
                    Transport::Next => icons.get_skip_next(),
                    Transport::Repeat => match modes.repeat() {
                        Repeat::One => icons.get_repeat_one_on(),
                        Repeat::All => icons.get_repeat_on(),
                        Repeat::Off => icons.get_repeat(),
                    },
                    // A heart service's favourite, or a thumb; the filled heart
                    // is the one already given, which a press takes back.
                    Transport::RateUp if heart => icons.get_favorite_border_outlined(),
                    Transport::RateUp => icons.get_thumb_up_outlined(),
                    Transport::Unfavorite => icons.get_favorite(),
                    Transport::RateDown if heart => icons.get_block_outlined(),
                    Transport::RateDown => icons.get_thumb_down_outlined(),
                };
                PaneButton {
                    icon,
                    label: SharedString::new(),
                    wide: *t == Transport::PlayPause,
                }
            })
            .collect();

        // Save favorite only for a track a service names, and never for the TV
        // input or a station: those are not what a favourite saved here holds.
        let savable = !tv
            && !info.is_radio
            && live
                .metadata
                .as_ref()
                .and_then(|m| m.track())
                .and_then(|t| t.id.as_ref())
                .is_some_and(|id| id.service_id.is_some());
        self.places = if tv {
            vec![Place::Browse, Place::Search, Place::Group, Place::Configure]
        } else {
            let mut places = vec![Place::Queue, Place::Browse, Place::Search];
            if savable {
                places.push(Place::SaveFavorite);
            }
            places.extend([Place::Group, Place::Configure, Place::FullScreen]);
            places
        };
        let places: Vec<PaneButton> = self
            .places
            .iter()
            .map(|p| {
                let icons = ui.global::<Icons>();
                let icon = match p {
                    Place::Queue => icons.get_queue_music(),
                    Place::Browse => icons.get_library_books(),
                    Place::Search => icons.get_search(),
                    Place::SaveFavorite => icons.get_star_border_outlined(),
                    Place::Group => icons.get_speaker_group(),
                    Place::Configure => icons.get_tune(),
                    Place::FullScreen => icons.get_fullscreen(),
                };
                // Configure names the time a running sleep timer stops the room.
                let label = match (p, live.sleep_left(Instant::now())) {
                    (Place::Configure, Some(left)) => {
                        let ends = chrono::Local::now()
                            + chrono::Duration::from_std(left).unwrap_or_default();
                        format!("Configure \u{00B7} sleep at {}", panels::clock_time(ends))
                    }
                    _ => p.label().to_owned(),
                };
                PaneButton {
                    icon,
                    label: label.into(),
                    wide: false,
                }
            })
            .collect();

        // A grouped room's speakers, each with its own volume.
        self.speakers = if entry.players.len() > 1 {
            entry.players.clone()
        } else {
            Vec::new()
        };
        let speakers: Vec<SpeakerRow> = self
            .speakers
            .iter()
            .map(|id| {
                let v = self.house.player_volumes.get(id);
                SpeakerRow {
                    name: self.player_name(id).into(),
                    volume: v.map(|v| v.volume as i32).unwrap_or(0),
                    has_volume: v.is_some(),
                    muted: v.is_some_and(|v| v.muted),
                    fixed: v.is_some_and(|v| v.fixed),
                }
            })
            .collect();

        let alarm = info.alarm_ringing;
        let seekable = !tv && duration > 0 && actions.can_seek;
        self.pane_zones = [
            Some(zone::VOLUME),
            seekable.then_some(zone::SEEK),
            (!self.transport.is_empty()).then_some(zone::TRANSPORT),
            tv.then_some(zone::TV_BUTTONS),
            Some(if alarm { zone::ALARM } else { zone::PLACES }),
            (!self.speakers.is_empty()).then_some(zone::SPEAKERS),
        ]
        .into_iter()
        .flatten()
        .collect();

        let (title, subtitle) = match (&info.track_name, &info.stream_info) {
            (Some(track), _) => (
                track.clone(),
                model::track_line(
                    info.artist.as_deref(),
                    info.album.as_deref(),
                    info.source.as_deref(),
                    info.is_radio,
                    Some(track),
                ),
            ),
            (None, Some(stream)) => (stream.clone(), info.source.clone().unwrap_or_default()),
            (None, None) => (info.source.clone().unwrap_or_default(), String::new()),
        };
        let has_content = !title.is_empty();
        let stream_line = match (&info.track_name, &info.stream_info) {
            (Some(track), Some(stream))
                if stream != track && Some(stream) != info.artist.as_ref() =>
            {
                stream.clone()
            }
            _ => String::new(),
        };

        // Between the times: the burst total, else where Next or Prev goes.
        let now_ms = self.now_ms();
        let (caption, strong) = if self.seek_burst.shown(now_ms) {
            (self.seek_burst.total_text(), true)
        } else if self.focus.0 == zone::TRANSPORT
            && self.transport.get(self.focus.1 as usize) == Some(&Transport::Next)
        {
            let next = live
                .metadata
                .as_ref()
                .and_then(|m| m.next_item.as_ref())
                .and_then(|i| i.track.as_ref());
            (
                model::track_caption(
                    "Up next",
                    next.and_then(|t| t.name.as_deref()),
                    next.and_then(|t| t.artist_name()),
                )
                .unwrap_or_default(),
                false,
            )
        } else if self.focus.0 == zone::TRANSPORT
            && self.transport.get(self.focus.1 as usize) == Some(&Transport::Previous)
        {
            let previous = self
                .previous
                .get(&entry.group_id)
                .and_then(|(_, t)| t.as_ref())
                .filter(|_| !modes.shuffle);
            (
                model::track_caption(
                    "Previous",
                    previous.map(|t| t.0.as_str()),
                    previous.and_then(|t| t.1.as_deref()),
                )
                .unwrap_or_default(),
                false,
            )
        } else if self.focus.0 == zone::SEEK {
            ("\u{2039} \u{203A}  seek".to_owned(), false)
        } else {
            (String::new(), false)
        };

        let soundbar = self.has_tv(&entry);
        let (night, speech) = soundbar
            .as_ref()
            .and_then(|p| self.house.home_theater.get(p).copied())
            .unwrap_or((false, false));
        let art = self.art_image(info.art_url.as_deref());
        let service_logo = info
            .service
            .as_deref()
            .and_then(crate::logos::logo_url)
            .and_then(|url| self.thumbnail(Some(&url)));
        let volume = live.volume.clone();
        let pane = PaneState {
            room: self.room_label(&entry).into(),
            // The word the list uses; "TV" for a soundbar playing its input, and
            // nothing for an empty room, whose body already says so.
            activity: match activity {
                model::Activity::Empty => SharedString::new(),
                model::Activity::Playing if tv => "TV".into(),
                other => other.label().into(),
            },
            playing: activity == model::Activity::Playing,
            has_volume: volume.is_some(),
            volume: volume.as_ref().map(|v| v.volume as i32).unwrap_or(0),
            muted: volume.as_ref().is_some_and(|v| v.muted),
            fixed: volume.as_ref().is_some_and(|v| v.fixed),
            has_content,
            title: title.into(),
            subtitle: subtitle.into(),
            stream_info: stream_line.into(),
            idle_word: activity.label().into(),
            has_art: art.is_some(),
            art: art.unwrap_or_default(),
            glyph: if info.is_radio {
                Glyph::Radio
            } else {
                Glyph::None
            },
            // The service's logo over the cover's corner, shown while volume or
            // seek has focus (home.slint decides when).
            has_service_logo: service_logo.is_some(),
            service_logo: service_logo.unwrap_or_default(),
            has_duration: duration > 0 && !tv,
            seekable,
            progress: if duration > 0 {
                (position as f32 / duration as f32).clamp(0.0, 1.0)
            } else {
                0.0
            },
            position_text: model::time_text(position as i64).into(),
            duration_text: model::time_text(duration as i64).into(),
            caption: caption.into(),
            caption_strong: strong,
            tv,
            tv_format: info.input_format.clone().into(),
            night,
            speech,
            alarm_ringing: alarm,
        };
        ui.set_pane(pane);
        self.models.transport.set(transport);
        self.models.places.set(places);
        self.models.speakers.set(speakers);
        // The cover's shade behind the pane; the plain surface on a TV input.
        let shade = if tv {
            None
        } else {
            self.art_colors(info.art_url.as_deref())
                .map(|(_, shade)| shade)
        };
        ui.set_pane_shade(match shade {
            Some(s) => slint::Color::from_rgb_u8(s[0], s[1], s[2]),
            None => theme::palette_for(&self.prefs.theme, None).surface,
        });

        // A focused control follows its own place in a rebuilt row; one the pane
        // no longer draws hands focus to Play/Pause (handFocusOnLeave), never to
        // whichever control slid into its index: Next leaving at a queue's end
        // used to leave a press on Repeat.
        let followed = match (held_transport, held_place, held_speaker) {
            (Some(t), _, _) => Some(self.transport.iter().position(|x| *x == t)),
            (_, Some(p), _) => Some(self.places.iter().position(|x| *x == p)),
            (_, _, Some(s)) => Some(self.speakers.iter().position(|x| *x == s)),
            _ => None,
        };
        match followed {
            Some(Some(at)) => self.focus.1 = at as i32,
            Some(None) => self.focus = self.pane_entry(),
            None => {}
        }
        if !self.panel_open()
            && !self.screen_open()
            && self.in_pane()
            && !self.zone_present(self.focus.0)
        {
            self.focus = self.pane_entry();
        }
        self.clamp_focus();
        self.start_ticking(live.playing() || self.seek_burst.shown(now_ms));
    }

    fn apply_theme(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let accent = self
            .selected_entry()
            .map(|e| self.live(&e.group_id))
            .and_then(|live| self.art_colors(RoomInfo::from_live(&live).art_url.as_deref()))
            .map(|(accent, _)| accent);
        let palette = theme::palette_for(&self.prefs.theme, accent);
        ui.global::<crate::Theme>().set_palette(palette);
        let _ = palette::on_accent;
    }

    /// The position and the equalizers move only while something plays.
    fn start_ticking(&mut self, needed: bool) {
        // Full screen's clock and drift move whether or not anything plays.
        let any_playing = needed
            || self.screen_is(Kind::NowPlaying)
            || self.house.live.values().any(|l| l.playing());
        if !any_playing {
            self.tick.stop();
            return;
        }
        if self.tick.running() {
            return;
        }
        self.tick.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(33),
            move || {
                with_app(|app| app.tick());
            },
        );
    }

    fn tick(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let ms = self.now_ms();
        // The equalizer: each column glides between its levels, smoothstepped.
        const LEVELS: [&[f32]; 4] = [
            &[3., 5., 4., 2., 4., 5., 3., 2.],
            &[5., 3., 4., 5., 2., 3., 4.],
            &[2., 4., 5., 3., 5., 2., 4., 3., 5., 1.],
            &[4., 2., 3., 4., 1., 3., 2., 5., 3.],
        ];
        let clock = (ms % 4400) as f32 / 4400.0;
        let eq: Vec<f32> = LEVELS
            .iter()
            .map(|levels| {
                let position = clock * levels.len() as f32;
                let step = (position as usize).min(levels.len() - 1);
                let t = position - step as f32;
                let t = t * t * (3.0 - 2.0 * t);
                levels[step] + (levels[(step + 1) % levels.len()] - levels[step]) * t
            })
            .collect();
        ui.global::<crate::Anim>()
            .set_eq(ModelRc::new(VecModel::from(eq)));
        // The position, once a second is enough for the clock and smooth enough
        // for the bar at this size; every frame costs nothing extra while ticking.
        if ms % 500 < 40
            || self.seek_burst.shown(ms)
            || !self.seek_burst.shown(ms.saturating_sub(40))
        {
            self.render_pane();
            if self.screen_is(Kind::NowPlaying) {
                self.render_screen();
            }
        }
    }

    // ----- Focus -------------------------------------------------------------------

    fn in_pane(&self) -> bool {
        matches!(
            self.focus.0,
            zone::VOLUME
                | zone::SEEK
                | zone::TRANSPORT
                | zone::PLACES
                | zone::SPEAKERS
                | zone::TV_BUTTONS
                | zone::ALARM
        )
    }

    fn zone_present(&self, z: i32) -> bool {
        self.pane_zones.contains(&z)
    }

    fn zone_len(&self, z: i32) -> i32 {
        (match z {
            zone::ROOMS => self.rooms.len(),
            zone::SETTINGS | zone::VOLUME | zone::SEEK => 1,
            zone::TRANSPORT => self.transport.len(),
            zone::PLACES => self.places.len(),
            zone::SPEAKERS => self.speakers.len(),
            zone::TV_BUTTONS | zone::ALARM => 2,
            zone::SETTINGS_PANEL | zone::ROOM_PANEL => {
                self.panel.as_ref().map_or(0, |p| p.actions.len())
            }
            zone::HEADER
            | zone::LIST
            | zone::BAR
            | zone::PIN
            | zone::SIDE
            | zone::FULL
            | zone::FIELD => self.screen_zone_len(z) as usize,
            zone::KEYS => self.keys.keys.len(),
            _ => 0,
        }) as i32
    }

    /// Where Right from the room list lands: Play/Pause, else Browse.
    fn pane_entry(&self) -> (i32, i32) {
        if let Some(i) = self
            .transport
            .iter()
            .position(|t| *t == Transport::PlayPause)
        {
            return (zone::TRANSPORT, i as i32);
        }
        if let Some(i) = self.places.iter().position(|p| *p == Place::Browse) {
            return (zone::PLACES, i as i32);
        }
        (zone::VOLUME, 0)
    }

    fn clamp_focus(&mut self) {
        let len = self.zone_len(self.focus.0);
        if len > 0 {
            self.focus.1 = self.focus.1.clamp(0, len - 1);
        } else if self.focus.0 == zone::ROOMS {
            self.focus.1 = 0;
        }
    }

    fn set_focus(&mut self, z: i32, index: i32) {
        self.memory.insert(self.focus.0, self.focus.1);
        self.focus = (z, index);
        self.clamp_focus();
    }

    fn push_focus(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let nav = ui.global::<Nav>();
        nav.set_zone(self.focus.0);
        nav.set_index(self.focus.1);
        nav.set_show_focus(!self.touch);
        ui.global::<crate::Theme>().set_touch_mode(self.touch);
    }

    /// The selected room's row, where Left and Back from the pane go.
    fn focus_rooms(&mut self) {
        let at = self.selected_index().unwrap_or(0) as i32;
        self.set_focus(zone::ROOMS, at);
    }

    /// Select the room at a row, as focus resting on it does.
    fn select_row(&mut self, index: usize) {
        if let Some(entry) = self.rooms.get(index) {
            self.selected = Some(entry.group_id.clone());
            self.anchor = Some(entry.coordinator.clone());
            self.seek_burst = SeekBurst::default();
        }
    }

    // ----- Actions -----------------------------------------------------------------

    /// A key from the window: Slint's key text, whether it is the platform's
    /// auto-repeat, and whether it went down or up. Whether it was used.
    pub fn key(&mut self, text: &str, repeat: bool, pressed: bool) -> bool {
        let phase = if !pressed {
            Phase::Release
        } else if repeat {
            Phase::Repeat
        } else {
            Phase::Press
        };
        // Typing: a real keyboard (or Steam's) writes into Search's field while
        // the field or the on-screen keyboard has focus.
        if self.typing()
            && pressed
            && let Some(edit) = self.typed(text)
        {
            return edit;
        }
        if let Some(action) = crate::input::action_for_key(text) {
            if Self::echoes(self.last_pad, action, phase) {
                return true;
            }
            self.last_key = Some((action, phase, Instant::now()));
            return self.action(action, phase);
        }
        let mut chars = text.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && let Some(action) = crate::input::shortcut_for_char(c)
        {
            return self.action(action, phase);
        }
        false
    }

    /// A gamepad's action. Steam's desktop layout turns the Deck's pad into
    /// keys while the pad itself may still be read, so the same press can come
    /// both ways: whichever arrives second, within a few frames, is dropped.
    pub fn pad_action(&mut self, action: Action, phase: Phase) -> bool {
        if Self::echoes(self.last_key, action, phase) {
            return true;
        }
        self.last_pad = Some((action, phase, Instant::now()));
        self.action(action, phase)
    }

    fn echoes(other: Option<(Action, Phase, Instant)>, action: Action, phase: Phase) -> bool {
        other.is_some_and(|(a, p, at)| {
            a == action && p == phase && at.elapsed() < Duration::from_millis(80)
        })
    }

    /// One key or button, pressed, repeated or released.
    pub fn action(&mut self, action: Action, phase: Phase) -> bool {
        if self.touch && !matches!(phase, Phase::Release | Phase::Cancel) {
            // The first key after touching brings focus back, and does only that.
            self.touch = false;
            self.push_focus();
            if matches!(
                action,
                Action::Up | Action::Down | Action::Left | Action::Right
            ) {
                return true;
            }
        }
        // Full screen has nothing to press: any key but Back and the media keys
        // closes it, on the key's release, so the release does not land on the
        // pane's button underneath.
        if self.screen_is(Kind::NowPlaying)
            && !matches!(
                action,
                Action::Back
                    | Action::VolumeUp
                    | Action::VolumeDown
                    | Action::PlayPause
                    | Action::Next
                    | Action::Previous
            )
        {
            if phase == Phase::Release {
                self.close_screen();
                self.render();
            }
            return true;
        }
        let handled = match action {
            Action::Select => self.select_key(phase),
            Action::Menu => {
                match phase {
                    // Once per press: a held Menu acts once (dpadMenuKey).
                    Phase::Press if !self.menu_down => {
                        self.menu_down = true;
                        let (z, i) = self.focus;
                        self.hold(z, i);
                    }
                    Phase::Release | Phase::Cancel => self.menu_down = false,
                    _ => {}
                }
                true
            }
            Action::Up | Action::Down | Action::Left | Action::Right => {
                if action == Action::Left {
                    match phase {
                        Phase::Press => {
                            self.left_from_room =
                                self.focus.0 == zone::ROOMS && self.moving.is_none()
                        }
                        Phase::Release | Phase::Cancel => self.left_from_room = false,
                        Phase::Repeat => {}
                    }
                }
                if matches!(phase, Phase::Release | Phase::Cancel) {
                    return true;
                }
                if self.panel_open() {
                    self.navigate_panel(action);
                } else if self.keyboard.is_some() {
                    self.navigate_keyboard(action);
                } else if self.screen_open() {
                    self.navigate_screen(action);
                } else {
                    self.navigate(action, phase == Phase::Repeat);
                }
                true
            }
            Action::Back => {
                if phase == Phase::Press {
                    if self.panel_open() {
                        self.close_panel();
                    } else if self.keyboard.is_some() {
                        self.close_keyboard();
                    } else if self.screen_open() {
                        // The alarm editor steps back before the screen closes.
                        if !self.alarm_back() {
                            self.close_screen();
                        }
                    } else {
                        self.back();
                    }
                }
                true
            }
            Action::PlayPause => {
                if phase == Phase::Press {
                    self.transport_press(Transport::PlayPause);
                }
                true
            }
            Action::Next | Action::Previous => {
                if phase == Phase::Press {
                    self.transport_press(if action == Action::Next {
                        Transport::Next
                    } else {
                        Transport::Previous
                    });
                }
                true
            }
            Action::VolumeUp | Action::VolumeDown => {
                if !matches!(phase, Phase::Release | Phase::Cancel) {
                    let direction = if action == Action::VolumeUp { 1 } else { -1 };
                    self.group_volume_step(direction, phase == Phase::Repeat);
                }
                true
            }
            Action::Fullscreen => {
                if phase == Phase::Press
                    && let Some(ui) = self.ui.upgrade()
                {
                    let window = ui.window();
                    window.set_fullscreen(!window.is_fullscreen());
                }
                true
            }
        };
        self.render();
        handled
    }

    fn select_key(&mut self, phase: Phase) -> bool {
        match phase {
            Phase::Press => {
                let (z, i) = self.focus;
                self.press = Some(Press {
                    zone: z,
                    index: i,
                    held: false,
                });
                self.hold_timer
                    .start(slint::TimerMode::SingleShot, HOLD, move || {
                        with_app(|app| {
                            if let Some(press) = app.press.as_mut()
                                && !press.held
                                && (press.zone, press.index) == app.focus
                            {
                                press.held = true;
                                let (z, i) = (press.zone, press.index);
                                app.hold(z, i);
                                app.render();
                            }
                        });
                    });
            }
            Phase::Repeat => {}
            Phase::Release => {
                self.hold_timer.stop();
                if let Some(press) = self.press.take()
                    && !press.held
                    && (press.zone, press.index) == self.focus
                {
                    self.click(press.zone, press.index);
                }
            }
            Phase::Cancel => {
                self.hold_timer.stop();
                self.press = None;
            }
        }
        true
    }

    /// A finger's tap: a click on that control, without moving focus there.
    pub fn tap(&mut self, z: i32, index: i32) {
        self.touch = true;
        match z {
            // The first tap selects a room; a tap on the selected one opens its panel.
            zone::ROOMS => {
                let i = index as usize;
                if self.selected_index() == Some(i) {
                    self.open_place(Place::Group);
                } else {
                    self.select_row(i);
                    self.focus = (zone::ROOMS, index);
                }
            }
            _ => self.click(z, index),
        }
        self.render();
    }

    pub fn touch_hold(&mut self, z: i32, index: i32) {
        self.touch = true;
        if z == zone::ROOMS {
            self.focus = (zone::ROOMS, index);
        }
        self.hold(z, index);
        self.render();
    }

    fn navigate(&mut self, action: Action, repeat: bool) {
        // A room carried in the list.
        if let Some(moving) = self.moving.clone() {
            // A repeat never carries the party offer on: each step is its own press.
            if !(repeat && action == Action::Left) {
                self.navigate_moving(moving, action);
            }
            return;
        }
        let (z, i) = self.focus;
        match z {
            zone::ROOMS => match action {
                Action::Up if i > 0 => {
                    self.focus.1 = i - 1;
                    self.select_row((i - 1) as usize);
                }
                Action::Down if i + 1 < self.rooms.len() as i32 => {
                    self.focus.1 = i + 1;
                    self.select_row((i + 1) as usize);
                }
                Action::Down => self.set_focus(zone::SETTINGS, 0),
                Action::Right if !self.rooms.is_empty() => {
                    let entry = self.pane_entry();
                    self.set_focus(entry.0, entry.1);
                }
                // A held Left picks the room up, as a hold of Select does.
                Action::Left if repeat && self.left_from_room => {
                    self.left_from_room = false;
                    self.pick_up(i as usize);
                }
                _ => {}
            },
            zone::SETTINGS => match action {
                Action::Up if !self.rooms.is_empty() => {
                    let last = self.rooms.len() as i32 - 1;
                    self.set_focus(zone::ROOMS, last);
                    self.select_row(last as usize);
                }
                Action::Right if !self.rooms.is_empty() => {
                    let entry = self.pane_entry();
                    self.set_focus(entry.0, entry.1);
                }
                _ => {}
            },
            _ => self.navigate_pane(action, repeat),
        }
    }

    fn navigate_pane(&mut self, action: Action, repeat: bool) {
        let (z, i) = self.focus;
        let horizontal = matches!(
            z,
            zone::TRANSPORT | zone::PLACES | zone::TV_BUTTONS | zone::ALARM
        );
        match action {
            Action::Left | Action::Right => {
                let d = if action == Action::Right { 1 } else { -1 };
                match z {
                    zone::VOLUME => {
                        let fixed = self
                            .selected_entry()
                            .map(|e| self.live(&e.group_id))
                            .and_then(|l| l.volume)
                            .is_some_and(|v| v.fixed);
                        if fixed && d < 0 {
                            self.focus_rooms();
                        } else {
                            self.group_volume_step(d, repeat);
                        }
                    }
                    zone::SEEK => self.seek_step(d as i64),
                    zone::SPEAKERS => self.speaker_step(i as usize, d, repeat),
                    _ if horizontal => {
                        let next = i + d;
                        if next < 0 {
                            self.focus_rooms();
                        } else if next < self.zone_len(z) {
                            self.focus.1 = next;
                        }
                    }
                    _ => {}
                }
            }
            Action::Up | Action::Down => {
                let down = action == Action::Down;
                if z == zone::SPEAKERS {
                    let next = i + if down { 1 } else { -1 };
                    if next >= 0 && next < self.zone_len(z) {
                        self.focus.1 = next;
                        return;
                    }
                }
                let Some(at) = self.pane_zones.iter().position(|&p| p == z) else {
                    return;
                };
                let target = if down {
                    self.pane_zones.get(at + 1)
                } else {
                    at.checked_sub(1).and_then(|a| self.pane_zones.get(a))
                };
                // Up and Down never leave the pane.
                let Some(&target) = target else { return };
                let index = self.entry_index(target, z, i, down);
                self.set_focus(target, index);
            }
            _ => {}
        }
    }

    /// Where a vertical move into `target` lands: the index last left there,
    /// Play/Pause on the transport, the first speaker from above.
    fn entry_index(&self, target: i32, _from: i32, _from_index: i32, down: bool) -> i32 {
        match target {
            zone::SPEAKERS => {
                if down {
                    0
                } else {
                    self.zone_len(target) - 1
                }
            }
            zone::TRANSPORT => self.memory.get(&target).copied().unwrap_or_else(|| {
                self.transport
                    .iter()
                    .position(|t| *t == Transport::PlayPause)
                    .unwrap_or(0) as i32
            }),
            _ => self.memory.get(&target).copied().unwrap_or(0),
        }
    }

    fn back(&mut self) {
        if let Some(moving) = self.moving.take() {
            // Back puts a carried room down where it is; from the party offer it
            // backs out of all of it.
            let _ = moving;
            return;
        }
        if self.in_pane() || self.focus.0 == zone::SETTINGS {
            self.focus_rooms();
        }
    }

    fn click(&mut self, z: i32, index: i32) {
        match z {
            // With no rooms listed, Select looks for the speakers again at once.
            zone::ROOMS if self.rooms.is_empty() => {
                self.send(Command::Reconnect);
                self.notice("Looking for speakers again");
            }
            zone::ROOMS => {
                if let Some(Moving::OfferingParty(group)) = self.moving.clone() {
                    self.moving = None;
                    self.play_everywhere(&group, false);
                    return;
                }
                if self.moving.take().is_some() {
                    return;
                }
                // Select on a room enters the pane; it does not open the panel.
                self.select_row(index as usize);
                let entry = self.pane_entry();
                self.render_pane();
                self.set_focus(entry.0, entry.1);
            }
            zone::SETTINGS => self.open_settings(),
            zone::SETTINGS_PANEL | zone::ROOM_PANEL => self.panel_click(index),
            zone::HEADER
            | zone::LIST
            | zone::BAR
            | zone::PIN
            | zone::SIDE
            | zone::FULL
            | zone::FIELD => self.screen_click(z, index),
            zone::KEYS => self.key_pressed(index as usize),
            zone::VOLUME => {
                if let Some(entry) = self.selected_entry().cloned() {
                    let muted = self.live(&entry.group_id).volume.is_some_and(|v| v.muted);
                    self.send(Command::SetGroupMute {
                        group: entry.group_id,
                        muted: !muted,
                    });
                }
            }
            zone::SPEAKERS => {
                if let Some(player) = self.speakers.get(index as usize).cloned() {
                    let muted = self
                        .house
                        .player_volumes
                        .get(&player)
                        .is_some_and(|v| v.muted);
                    self.send(Command::SetPlayerMute {
                        player,
                        muted: !muted,
                    });
                }
            }
            zone::TRANSPORT => {
                if let Some(t) = self.transport.get(index as usize).copied() {
                    self.transport_press(t);
                }
            }
            zone::PLACES => {
                if let Some(p) = self.places.get(index as usize).copied() {
                    self.open_place(p);
                }
            }
            zone::TV_BUTTONS => {
                let Some(entry) = self.selected_entry().cloned() else {
                    return;
                };
                let Some(player) = self.has_tv(&entry) else {
                    return;
                };
                let (night, speech) = self
                    .house
                    .home_theater
                    .get(&player)
                    .copied()
                    .unwrap_or((false, false));
                self.send(if index == 0 {
                    Command::SetNightSound { player, on: !night }
                } else {
                    Command::SetSpeechEnhancement {
                        player,
                        on: !speech,
                    }
                });
            }
            _ => {}
        }
    }

    fn hold(&mut self, z: i32, index: i32) {
        match z {
            // A hold on Play/Pause rates the track up, as on the TV.
            zone::TRANSPORT
                if self.transport.get(index.max(0) as usize) == Some(&Transport::PlayPause) =>
            {
                if self.transport.contains(&Transport::RateUp) {
                    self.transport_press(Transport::RateUp);
                }
            }
            zone::LIST => self.screen_hold(z, index),
            zone::ROOMS => self.pick_up(index.max(0) as usize),
            zone::VOLUME => self.open_volume_chooser(),
            _ => {}
        }
    }

    fn open_place(&mut self, place: Place) {
        match place {
            Place::FullScreen => self.open_screen(Kind::NowPlaying),
            Place::Queue => self.open_screen(Kind::Queue),
            Place::Search => self.open_screen(Kind::Search),
            Place::SaveFavorite => {
                if let Some(entry) = self.selected_entry().cloned() {
                    self.send(Command::SaveFavorite {
                        group: entry.group_id,
                    });
                }
            }
            Place::Browse => self.open_screen(Kind::Browse),
            Place::Group => self.open_room_panel(false),
            Place::Configure => self.open_room_panel(true),
        }
    }

    fn transport_press(&mut self, t: Transport) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let live = self.live(&entry.group_id);
        let group = entry.group_id.clone();
        // A control the pane does not draw is not acted on from anywhere: the
        // media keys go through here too.
        let allowed = match t {
            Transport::PlayPause => {
                live.actions.can_play || live.actions.can_pause || live.playing()
            }
            Transport::Next => live.actions.can_skip,
            Transport::Previous => live.actions.can_skip_back,
            Transport::Shuffle => live.actions.can_shuffle,
            Transport::Repeat => live.actions.can_repeat,
            Transport::RateUp | Transport::RateDown | Transport::Unfavorite => {
                self.transport.contains(&t)
            }
        };
        if !allowed {
            return;
        }
        let command = match t {
            Transport::PlayPause if live.playing() => Command::Playback {
                group,
                // The Control API has no stop: pausing a stream is how it stops.
                command: "pause",
            },
            Transport::PlayPause => Command::Playback {
                group,
                command: "play",
            },
            Transport::Next => Command::Playback {
                group,
                command: "skipToNextTrack",
            },
            Transport::Previous => Command::Playback {
                group,
                command: "skipToPreviousTrack",
            },
            Transport::Shuffle => Command::SetShuffle {
                group,
                on: !live.modes.shuffle,
            },
            Transport::RateUp | Transport::RateDown | Transport::Unfavorite => {
                use x2rock::sonos::smapi::RatingKind;
                let kind = match t {
                    Transport::RateUp => RatingKind::Up,
                    Transport::RateDown => RatingKind::Down,
                    _ => RatingKind::Unfavorite,
                };
                // Asked again after: the state the service reports has moved.
                self.ratings.remove(&group);
                Command::Rate { group, kind }
            }
            Transport::Repeat => Command::SetRepeat {
                group,
                repeat: match live.modes.repeat() {
                    Repeat::Off => Repeat::All,
                    Repeat::All if live.actions.can_repeat_one => Repeat::One,
                    _ => Repeat::Off,
                },
            },
        };
        self.send(command);
    }

    /// A level, not a delta: a newer one replaces any still waiting to be
    /// sent, so a released hold leaves at most its last level to arrive.
    fn group_volume_step(&mut self, direction: i32, repeat: bool) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        // Until the speaker says where it is, there is nothing to step from.
        let Some(level) = self.live(&entry.group_id).volume.map(|v| v.volume as i32) else {
            return;
        };
        let now = self.now_ms();
        let hold = self.volume_holds.entry(entry.group_id.clone()).or_default();
        if let Some((_, to)) = hold.step(level, direction, repeat, now) {
            self.send(Command::SetGroupVolume {
                group: entry.group_id,
                level: to as u8,
            });
        }
    }

    fn speaker_step(&mut self, index: usize, direction: i32, repeat: bool) {
        let Some(player) = self.speakers.get(index).cloned() else {
            return;
        };
        let level = self
            .house
            .player_volumes
            .get(&player)
            .map(|v| v.volume as i32)
            .unwrap_or(0);
        let now = self.now_ms();
        let hold = self.speaker_holds.entry(player.clone()).or_default();
        if let Some((from, to)) = hold.step(level, direction, repeat, now) {
            self.send(Command::AdjustPlayerVolume {
                player,
                delta: (to - from) as i8,
            });
        }
    }

    pub fn volume_step_tapped(&mut self, direction: i32) {
        self.touch = true;
        self.group_volume_step(direction, false);
        self.render();
    }

    pub fn speaker_step_tapped(&mut self, index: usize, direction: i32) {
        self.touch = true;
        self.speaker_step(index, direction, false);
        self.render();
    }

    fn seek_step(&mut self, direction: i64) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let live = self.live(&entry.group_id);
        let position = live.live_position(Instant::now()) as i64;
        let now = self.now_ms();
        let delta = self
            .seek_burst
            .step(direction, position, live.duration_ms() as i64, now);
        self.send(Command::SeekBy {
            group: entry.group_id,
            millis: delta,
        });
    }

    pub fn seek_to(&mut self, fraction: f32) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let duration = self.live(&entry.group_id).duration_ms();
        if duration == 0 {
            return;
        }
        self.touch = true;
        self.send(Command::SeekTo {
            group: entry.group_id,
            millis: (fraction.clamp(0.0, 1.0) * duration as f32) as u64,
        });
        self.render();
    }

    // ----- Moving a room -----------------------------------------------------------

    fn pick_up(&mut self, index: usize) {
        // With one room in the list there is no hold at all.
        if self.rooms.len() < 2 {
            return;
        }
        if let Some(entry) = self.rooms.get(index) {
            self.selected = Some(entry.group_id.clone());
            self.moving = Some(Moving::Carrying(entry.group_id.clone()));
        }
    }

    fn navigate_moving(&mut self, moving: Moving, action: Action) {
        match (moving, action) {
            (Moving::Carrying(group), Action::Up | Action::Down) => {
                let Some(at) = self.rooms.iter().position(|r| r.group_id == group) else {
                    return;
                };
                let to = if action == Action::Up {
                    at.checked_sub(1)
                } else {
                    (at + 1 < self.rooms.len()).then_some(at + 1)
                };
                let Some(to) = to else { return };
                self.rooms.swap(at, to);
                // A move places the moved room and everything above it as shown.
                let mut ranks: Vec<String> = Vec::new();
                for entry in self.rooms.iter().take(to + 1) {
                    ranks.extend(entry.players.iter().cloned());
                }
                for old in std::mem::take(&mut self.prefs.room_ranks) {
                    if !ranks.contains(&old) {
                        ranks.push(old);
                    }
                }
                self.prefs.room_ranks = ranks;
                self.prefs.save();
                self.rooms = self.sorted_rooms();
                self.focus.1 = self
                    .rooms
                    .iter()
                    .position(|r| r.group_id == group)
                    .unwrap_or(to) as i32;
            }
            (Moving::Carrying(group), Action::Left) => {
                self.moving = Some(Moving::OfferingParty(group))
            }
            (Moving::OfferingParty(group), Action::Left) => {
                self.moving = None;
                self.play_everywhere(&group, true);
            }
            _ => {}
        }
    }

    /// Every room into this one's group, from this one's source. With
    /// `even_volume`, every speaker is set to this room's level first, so
    /// nothing joins loud.
    fn play_everywhere(&mut self, group: &str, even_volume: bool) {
        let level = even_volume
            .then(|| self.live(group).volume.map(|v| v.volume))
            .flatten();
        self.send(Command::PlayEverywhere {
            group: group.to_owned(),
            level,
        });
        self.notice(if even_volume {
            "Playing everywhere, every speaker at this room's level"
        } else {
            "Playing everywhere"
        });
    }

    // ----- The on-screen keyboard ----------------------------------------------

    fn typing(&self) -> bool {
        self.keyboard.is_some() || (self.focus.0 == zone::FIELD && self.focus.1 == 0)
    }

    /// The text the front screen takes: Search's query, or Radio's address.
    fn query_mut(&mut self) -> Option<&mut String> {
        self.stack
            .last_mut()
            .filter(|s| s.kind == Kind::Search || (s.kind == Kind::Radio && s.url_mode))
            .map(|s| &mut s.query)
    }

    /// Done on the keyboard, or Enter: search, or play the address.
    fn submit_text(&mut self) {
        match self.stack.last().map(|s| s.kind.clone()) {
            Some(Kind::Radio) => self.play_url(),
            _ => self.submit_search(),
        }
    }

    /// A key typed while Search's field takes text: `Some(used)` if it was
    /// text, `None` to treat it as an action.
    fn typed(&mut self, text: &str) -> Option<bool> {
        use slint::platform::Key;
        let mut chars = text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        if c == char::from(Key::Backspace) {
            self.query_mut()?.pop();
        } else if c == char::from(Key::Return) || c == '\r' {
            // With the on-screen keyboard up, Return is Select on its focused
            // key: Steam's desktop layout sends the Deck's A as Return, and the
            // pad's own A may arrive too, which only the action path drops.
            if self.keyboard.is_some() {
                return None;
            }
            self.submit_text();
        } else if !c.is_control() && (c as u32) < 0xF700 {
            self.query_mut()?.push(c);
        } else {
            return None;
        }
        self.render();
        Some(true)
    }

    pub(super) fn open_keyboard(&mut self) {
        self.keyboard = Some(0);
        self.focus = (zone::KEYS, 0);
    }

    fn close_keyboard(&mut self) {
        self.keyboard = None;
        if self.zone_len(zone::FIELD) > 0 {
            self.focus = (zone::FIELD, 0);
        } else if self.zone_len(zone::SIDE) > 0 {
            self.focus = (zone::SIDE, 0);
        } else if self.zone_len(zone::LIST) > 0 {
            self.focus = (zone::LIST, 0);
        } else {
            self.focus = (zone::HEADER, 0);
        }
        self.clamp_focus();
    }

    fn navigate_keyboard(&mut self, action: Action) {
        let at = self.focus.1.max(0) as usize;
        let (dx, dy) = match action {
            Action::Left => (-1, 0),
            Action::Right => (1, 0),
            Action::Up => (0, -1),
            Action::Down => (0, 1),
            _ => return,
        };
        // Up from the top row leaves the keyboard for the field above it.
        if dy < 0 && self.keys.keys.get(at).is_some_and(|k| k.row == 0) {
            self.close_keyboard();
            return;
        }
        let next = self.keys.step(at, dx, dy);
        self.focus.1 = next as i32;
        self.keyboard = Some(next);
    }

    fn key_pressed(&mut self, index: usize) {
        let Some(key) = self.keys.keys.get(index).copied() else {
            return;
        };
        let Some(query) = self.query_mut() else {
            return;
        };
        if crate::keyboard::apply(key.kind, query).is_none() {
            self.submit_text();
        }
    }

    fn render_keyboard(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let target = self
            .stack
            .last()
            .filter(|s| s.kind == Kind::Search || (s.kind == Kind::Radio && s.url_mode))
            .map(|s| s.kind.clone());
        let shown = self.keyboard.is_some() && target.is_some();
        let done = if target == Some(Kind::Radio) {
            "Play"
        } else {
            "Search"
        };
        if shown && self.keyboard_label != done {
            self.keys = if done == "Play" {
                crate::keyboard::Layout::for_urls()
            } else {
                crate::keyboard::Layout::new()
            };
            ui.set_keyboard_rows(self.keys.rows(done));
            self.keyboard_label = done;
        }
        ui.set_search_button(done.into());
        ui.set_keyboard_shown(shown);
    }

    /// A tap on the scrim: whatever is open closes.
    pub fn dismiss(&mut self) {
        self.touch = true;
        self.close_panel();
        self.render();
    }

    pub fn resized(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        crate::scale::fit(ui.window());
    }
}

/// For the screenshot and the tests: the models' first rows, to check what the
/// screen was told.
#[allow(dead_code)]
pub fn room_names(ui: &AppWindow) -> Vec<String> {
    let rooms = ui.get_rooms();
    (0..rooms.row_count())
        .filter_map(|i| rooms.row_data(i))
        .map(|r| r.name.to_string())
        .collect()
}

pub(crate) fn format_readonly_command(command: &Command) -> String {
    let debug = format!("{command:?}");
    let safe = x2rock::sonos::plex::redact_token(&debug);
    format!("x2rock-deck: read-only, not sent: {safe}")
}

pub(crate) fn format_art_failure(url: &str, error: &anyhow::Error) -> String {
    let safe_url = x2rock::sonos::plex::redact_token(url);
    format!("x2rock-deck: no art for {safe_url}: {error:#}")
}
