//! Screens opened from a room: Queue, Browse, Radio, and the room's music on
//! the whole screen. Each list screen is the TV app's ScreenHeader over rows
//! of MediaRows (list.slint); what a row does is kept here by its focus index.

use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use x2rock::sonos::proto::Favorite;
use x2rock::stations::Station;

use super::{App, zone};
use crate::household::{Command, Fetch, Fetched, QueueView};
use crate::services::{Section, ServiceInfo};
use x2rock::sonos::smapi::Item;
use crate::model::{self, RoomInfo};
use crate::{BarState, Glyph, HeaderButton, Icons, ListCell, ListRow, RowKind, Screen, SideItem};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Queue,
    Browse,
    Radio,
    NowPlaying,
    Search,
    Services,
    /// A page of a service: its front page at `root`, else a container.
    ServicePage { service: String, container: String, title: String },
    Alarms,
    About,
    PreferredServices,
    /// Send a report: the report's text, and a QR code that opens it as a draft.
    Report,
}

/// What a header button does.
#[derive(Clone, Debug, PartialEq)]
pub enum HeaderAction {
    Back,
    SaveQueue,
    ClearQueue,
    MusicServices,
    Radio,
}

/// What a row does.
#[derive(Clone, Debug)]
pub enum RowAction {
    QueueTrack(u32),
    Favorite(String, String),
    Playlist(String, String),
    Station(Station),
    Retry,
    OpenService(ServiceInfo),
    ServiceItem { service: String, item: Item },
    Recent(String),
    LoadMore,
    Alarm(x2rock::sonos::upnp::Alarm),
    /// A row of the alarm editor, or New alarm.
    AlarmEditor(super::alarms::AlarmRow),
    Speaker,
    /// A block of the report: a focus stop so a long one scrolls, nothing to press.
    ReportBlock,
    Preset(crate::prefs::Preset),
    TogglePreferred(String),
    Replay(x2rock::sonos::proto::HistoryItem),
}

/// Household alarms, clock text, and whether a timezone is configured.
pub type HouseholdAlarms = (Vec<x2rock::sonos::upnp::Alarm>, String, bool);

/// An action in a screen's header: action, optional icon, and label.
type HeaderEntry<'a> = (HeaderAction, Option<slint::Image>, &'a str);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenRequests {
    pub queue: Option<u64>,
    pub favorites: Option<u64>,
    pub playlists: Option<u64>,
    pub history: Option<u64>,
    pub stations: Option<u64>,
    pub services: Option<u64>,
    pub page: Option<u64>,
    pub search: Option<u64>,
    pub alarms: Option<u64>,
    pub alarm_sounds: Option<u64>,
    pub about: Option<u64>,
    pub sign_ins: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenPending {
    pub queue: bool,
    pub favorites: bool,
    pub playlists: bool,
    pub history: bool,
    pub stations: bool,
    pub services: bool,
    pub page: bool,
    pub search: bool,
    pub alarms: bool,
    pub alarm_sounds: bool,
    pub about: bool,
    pub sign_ins: bool,
}

impl ScreenPending {
    pub fn record(&mut self, requests: &ScreenRequests) {
        if requests.queue.is_some() { self.queue = true; }
        if requests.favorites.is_some() { self.favorites = true; }
        if requests.playlists.is_some() { self.playlists = true; }
        if requests.history.is_some() { self.history = true; }
        if requests.stations.is_some() { self.stations = true; }
        if requests.services.is_some() { self.services = true; }
        if requests.page.is_some() { self.page = true; }
        if requests.search.is_some() { self.search = true; }
        if requests.alarms.is_some() { self.alarms = true; }
        if requests.alarm_sounds.is_some() { self.alarm_sounds = true; }
        if requests.about.is_some() { self.about = true; }
        if requests.sign_ins.is_some() { self.sign_ins = true; }
    }
}

pub struct ScreenState {
    pub requests: ScreenRequests,
    pub interrupted: ScreenPending,
    pub kind: Kind,
    /// The room it acts on.
    pub group: String,
    /// The control that opened it, which focus goes back to.
    pub opener: (i32, i32),
    pub queue: Option<Result<QueueView, String>>,
    pub favorites: Option<Result<Vec<Favorite>, String>>,
    pub playlists: Option<Result<Vec<x2rock::sonos::proto::Playlist>, String>>,
    pub stations: Option<Result<Vec<Station>, String>>,
    /// Radio's category: an index into `radio_categories()`.
    pub category: usize,
    pub clear_armed: bool,
    pub header: Vec<HeaderAction>,
    pub actions: Vec<RowAction>,
    pub cells: Vec<(usize, usize)>,
    /// Full screen's drift: a step a minute.
    pub opened_at: Instant,
    /// Search: what is typed, what was last searched, and what has answered.
    pub query: String,
    pub searched: Option<String>,
    pub sections: Vec<Section>,
    pub searching: bool,
    pub services: Option<Result<Vec<ServiceInfo>, String>>,
    /// A service page's rows so far, its total, and its failure.
    pub page: Vec<Item>,
    pub page_total: u32,
    pub page_error: Option<String>,
    pub page_loading: bool,
    pub alarms: Option<Result<HouseholdAlarms, String>>,
    pub about: Option<Result<Vec<crate::household::SpeakerInfo>, String>>,
    /// Radio's "Play a URL": the field takes an address rather than the
    /// directory's lists.
    pub url_mode: bool,
    pub history: Option<Result<Vec<x2rock::sonos::proto::HistoryItem>, String>>,
    /// Send a report: how the sign-ins read, once asked (the outer `None`).
    pub sign_ins: Option<Option<Result<usize, String>>>,
    /// The report's link and its code, drawn once a link.
    pub report_qr: Option<(String, slint::Image)>,
    /// A book's chapters: the book, which resumes, and the chapter it resumes at.
    pub book: Option<Item>,
    pub resume_chapter: Option<String>,
    /// Focus has been put on the resumed chapter once: after that it is the viewer's.
    pub resume_placed: bool,
    /// Alarms: the alarm being made or changed, and the favourites it can play.
    pub alarm_draft: Option<super::alarms::Draft>,
    pub alarm_sounds: Option<Result<Vec<crate::household::AlarmProgram>, String>>,
}

/// Radio's ways into the directory: Popular, then the genres.
pub fn radio_categories() -> Vec<(String, Option<String>)> {
    let genres = [
        "pop", "rock", "jazz", "classical", "news", "talk", "country", "electronic", "ambient", "chillout", "hip hop", "oldies",
        "blues", "soul", "reggae", "latin", "metal",
    ];
    let mut out = vec![("Popular".to_owned(), None)];
    for g in genres {
        let mut label = g.to_owned();
        if let Some(first) = label.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        out.push((label, Some(g.to_owned())));
    }
    out
}

/// The favourites in the groups the Sonos app lists them under, each in the
/// household's own order (FavoriteGroups.kt).
pub fn favorite_groups(items: &[Favorite]) -> Vec<(&'static str, Vec<&Favorite>)> {
    const GROUPS: [&str; 6] = ["Favorite playlists", "Favorite albums", "Favorite artists", "Favorite songs", "Favorite stations", "Favorites"];
    let group_of = |f: &Favorite| match f.kind().map(|k| k.to_uppercase()).as_deref() {
        Some("PLAYLIST") => "Favorite playlists",
        Some("ALBUM") => "Favorite albums",
        Some("ARTIST") => "Favorite artists",
        Some("TRACK") => "Favorite songs",
        Some("STREAM" | "PROGRAM") => "Favorite stations",
        _ => "Favorites",
    };
    GROUPS
        .iter()
        .map(|g| (*g, items.iter().filter(|f| group_of(f) == *g).collect::<Vec<_>>()))
        .filter(|(_, v)| !v.is_empty())
        .collect()
}

/// A favourite's second line: what it is and whose, "Album · Saavn".
pub fn favorite_line(f: &Favorite) -> String {
    let kind = f.kind().and_then(|k| match k.to_lowercase().as_str() {
        "track" => Some("Track"),
        "album" => Some("Album"),
        "artist" => Some("Artist"),
        "playlist" => Some("Playlist"),
        "stream" | "program" => Some("Radio"),
        _ => None,
    });
    let whose = f.service().or(f.description.as_deref());
    [kind, whose].into_iter().flatten().collect::<Vec<_>>().join(" \u{00B7} ")
}

#[derive(Default)]
pub(super) struct Rows {
    pub(super) rows: Vec<ListRow>,
    pub(super) actions: Vec<RowAction>,
    pub(super) cells: Vec<(usize, usize)>,
    pub(super) row: usize,
}

impl Rows {
    pub(super) fn heading(&mut self, text: &str) {
        self.rows.push(ListRow { kind: RowKind::Heading, text: text.into(), ..Default::default() });
    }
    pub(super) fn message(&mut self, text: &str) {
        self.rows.push(ListRow { kind: RowKind::Message, text: text.into(), ..Default::default() });
    }
    pub(super) fn item(&mut self, mut cell: ListCell, action: RowAction) {
        cell.focus = self.actions.len() as i32;
        self.actions.push(action);
        self.cells.push((self.row, 0));
        self.row += 1;
        self.rows.push(ListRow { kind: RowKind::Item, cells: ModelRc::new(VecModel::from(vec![cell])), ..Default::default() });
    }
    /// A block of text that is a focus stop, so a long one scrolls on a remote.
    pub(super) fn block(&mut self, text: &str, action: RowAction) {
        let cell = ListCell { focus: self.actions.len() as i32, ..Default::default() };
        self.actions.push(action);
        self.cells.push((self.row, 0));
        self.row += 1;
        self.rows.push(ListRow { kind: RowKind::Block, text: text.into(), cells: ModelRc::new(VecModel::from(vec![cell])), ..Default::default() });
    }
    pub(super) fn button(&mut self, text: &str, action: RowAction) {
        let cell = ListCell { focus: self.actions.len() as i32, ..Default::default() };
        self.actions.push(action);
        self.cells.push((self.row, 0));
        self.row += 1;
        self.rows.push(ListRow { kind: RowKind::Button, text: text.into(), cells: ModelRc::new(VecModel::from(vec![cell])), ..Default::default() });
    }
}

impl App {
    pub(super) fn open_screen(&mut self, kind: Kind) {
        let Some(entry) = self.selected_entry().cloned() else { return };
        self.keyboard = None;
        let opener = self.focus;
        let mut screen = ScreenState {
            requests: ScreenRequests::default(),
            interrupted: ScreenPending::default(),
            kind: kind.clone(),
            group: entry.group_id.clone(),
            opener,
            queue: None,
            favorites: None,
            playlists: None,
            stations: None,
            category: 0,
            clear_armed: false,
            header: vec![],
            actions: vec![],
            cells: vec![],
            opened_at: Instant::now(),
            query: String::new(),
            searched: None,
            sections: vec![],
            searching: false,
            services: None,
            page: vec![],
            page_total: 0,
            page_error: None,
            page_loading: false,
            alarms: None,
            about: None,
            sign_ins: None,
            report_qr: None,
            alarm_draft: None,
            alarm_sounds: None,
            book: None,
            resume_chapter: None,
            resume_placed: false,
            url_mode: false,
            history: None,
        };
        match kind.clone() {
            Kind::Queue => {
                let id = self.next_request_id();
                screen.requests.queue = Some(id);
                self.stack.push(screen);
                self.send(Command::Fetch(Fetch::Queue { id: Some(id), group: entry.group_id }));
            }
            Kind::Browse => {
                let fav_id = self.next_request_id();
                let pl_id = self.next_request_id();
                let hist_id = self.next_request_id();
                screen.requests.favorites = Some(fav_id);
                screen.requests.playlists = Some(pl_id);
                screen.requests.history = Some(hist_id);
                self.stack.push(screen);
                self.send(Command::Fetch(Fetch::Favorites { id: Some(fav_id) }));
                self.send(Command::Fetch(Fetch::Playlists { id: Some(pl_id) }));
                self.send(Command::Fetch(Fetch::History { id: Some(hist_id) }));
            }
            Kind::Radio => {
                let id = self.next_request_id();
                screen.requests.stations = Some(id);
                self.stack.push(screen);
                self.send(Command::Fetch(Fetch::Stations { id: Some(id), tag: None, country: None }));
            }
            Kind::NowPlaying | Kind::Search => {
                self.stack.push(screen);
            }
            Kind::Alarms => {
                let id = self.next_request_id();
                screen.requests.alarms = Some(id);
                self.stack.push(screen);
                self.send(Command::Fetch(Fetch::Alarms { id: Some(id) }));
                if self.handle.is_none() {
                    // The demo: no household to ask.
                    self.fetched(Fetched::Alarms { id: Some(id), result: Ok((crate::demo::alarms(), "2026-10-29 06:30:00".into(), true)) });
                    let sounds_id = self.next_request_id();
                    if let Some(s) = self.stack.last_mut() {
                        s.requests.alarm_sounds = Some(sounds_id);
                    }
                    self.fetched(Fetched::AlarmSounds { id: Some(sounds_id), result: Ok(crate::demo::alarm_sounds()) });
                }
            }
            Kind::About => {
                let id = self.next_request_id();
                screen.requests.about = Some(id);
                self.stack.push(screen);
                self.send(Command::Fetch(Fetch::About { id: Some(id) }));
            }
            Kind::Report => {
                let about_id = self.next_request_id();
                let sign_id = self.next_request_id();
                screen.requests.about = Some(about_id);
                screen.requests.sign_ins = Some(sign_id);
                self.stack.push(screen);
                self.send(Command::Fetch(Fetch::About { id: Some(about_id) }));
                self.send(Command::Fetch(Fetch::SignIns { id: Some(sign_id) }));
                if self.handle.is_none() {
                    // The demo: no household to ask.
                    self.fetched(Fetched::About { id: Some(about_id), result: Ok(crate::demo::speakers()) });
                    self.fetched(Fetched::SignIns { id: Some(sign_id), result: Some(Ok(3)) });
                }
            }
            Kind::Services | Kind::PreferredServices => {
                let id = self.next_request_id();
                screen.requests.services = Some(id);
                self.stack.push(screen);
                self.send(Command::Fetch(Fetch::Services { id: Some(id) }));
            }
            Kind::ServicePage { service, container, .. } => {
                let id = self.next_request_id();
                screen.requests.page = Some(id);
                screen.page_loading = true;
                self.stack.push(screen);
                self.send(Command::Fetch(Fetch::ServicePage { id: Some(id), service, container, index: 0 }));
            }
        }
        self.render_screen();
        // Focus starts on the list's first row; Radio's on Popular.
        match kind {
            // Popular, one below Play a URL, where the TV app's Radio starts.
            Kind::Radio => self.set_focus(zone::SIDE, 1),
            Kind::NowPlaying => self.set_focus(zone::FULL, 0),
            // Search opens on its field, with the keyboard up to type into it.
            Kind::Search => {
                self.set_focus(zone::FIELD, 0);
                self.open_keyboard();
            }
            _ => self.set_focus(zone::LIST, 0),
        }
    }

    /// A screen that belongs to the household, not a room: opened from
    /// Settings, with no room needed.
    pub(super) fn open_household_screen(&mut self, kind: Kind) {
        if self.selected_entry().is_some() {
            self.open_screen(kind);
        } else {
            self.notice("No speakers yet");
        }
    }

    pub(super) fn close_screen(&mut self) {
        // The keyboard belongs to the screen it types into.
        self.keyboard = None;
        if let Some(screen) = self.stack.pop() {
            // Its remaining services are not asked: nobody is left to see them.
            if screen.kind == Kind::Search && screen.searching {
                self.send(Command::CancelSearch);
            }
            self.focus = screen.opener;
            if self.stack.is_empty() {
                self.render_pane();
            }
            self.clamp_focus();
        }
    }

    pub(super) fn screen_open(&self) -> bool {
        !self.stack.is_empty()
    }

    /// Something started from a screen: back to the player, unless the Stay pin
    /// is held, as Browse's pin says (AfterPlaying).
    fn played(&mut self) {
        if self.prefs.after_playing.as_deref() == Some("stay") {
            return;
        }
        while self.screen_open() {
            self.close_screen();
        }
    }

    pub(super) fn fetched(&mut self, fetched: Fetched) {
        if let Fetched::AlarmWritten { alarm, result } = fetched {
            self.alarm_written(alarm, result);
            return;
        }
        if let Fetched::Resume { book, chapter } = fetched {
            for screen in self.stack.iter_mut().filter(|s| s.book.as_ref().is_some_and(|b| b.id == book)) {
                screen.resume_chapter.clone_from(&chapter);
            }
            return;
        }
        if let Fetched::IsFavorite { item_id, yes } = fetched {
            self.favorite_known(&item_id, yes);
            self.render_panel();
            return;
        }
        if matches!(fetched, Fetched::RatingOffers { .. } | Fetched::Previous { .. }) {
            return;
        }

        // Background update with no request id (e.g. favorite added/removed or alarm written).
        if fetched.request_id().is_none() {
            match fetched {
                Fetched::Favorites { result, .. } => {
                    if let Ok(list) = &result {
                        self.favorites_cache = list.clone();
                    }
                    if let Some(screen) = self.stack.iter_mut().rfind(|s| s.kind == Kind::Browse) {
                        screen.favorites = Some(result);
                    }
                }
                Fetched::Alarms { result, .. } => {
                    if let Some(screen) = self.stack.iter_mut().rfind(|s| s.kind == Kind::Alarms) {
                        screen.alarms = Some(result);
                    }
                }
                _ => {}
            }
            self.clamp_focus();
            return;
        }

        let Some(req_id) = fetched.request_id() else { return };
        let at = self.stack.iter().rposition(|s| match &fetched {
            Fetched::Queue { group, .. } => s.requests.queue == Some(req_id) && s.group == *group,
            Fetched::Favorites { .. } => s.requests.favorites == Some(req_id),
            Fetched::Playlists { .. } => s.requests.playlists == Some(req_id),
            Fetched::History { .. } => s.requests.history == Some(req_id),
            Fetched::Stations { .. } => s.requests.stations == Some(req_id),
            Fetched::Services { .. } => s.requests.services == Some(req_id),
            Fetched::SearchSection { .. } | Fetched::SearchDone { .. } => s.requests.search == Some(req_id),
            Fetched::ServicePage { service, container, .. } => {
                s.requests.page == Some(req_id)
                    && matches!(&s.kind, Kind::ServicePage { service: s2, container: c2, .. } if s2 == service && c2 == container)
            }
            Fetched::Alarms { .. } => s.requests.alarms == Some(req_id),
            Fetched::AlarmSounds { .. } => s.requests.alarm_sounds == Some(req_id),
            Fetched::About { .. } => s.requests.about == Some(req_id),
            Fetched::SignIns { .. } => s.requests.sign_ins == Some(req_id),
            _ => false,
        });
        let Some(at) = at else {
            return;
        };
        let on_top = at + 1 == self.stack.len();
        let screen = &mut self.stack[at];
        match fetched {
            Fetched::Queue { group, result, .. } if group == screen.group => {
                // The first read opens on the track playing, as the TV app's Queue does.
                let first = screen.queue.is_none();
                let current = result.as_ref().ok().and_then(|q| q.current).map(|c| c as usize);
                screen.queue = Some(result);
                screen.requests.queue = None;
                if first && on_top && self.focus.0 == zone::LIST
                    && let Some(at) = current {
                        self.focus.1 = at.saturating_sub(1) as i32;
                    }
            }
            Fetched::Favorites { result, .. } => {
                if let Ok(list) = &result {
                    self.favorites_cache = list.clone();
                }
                screen.favorites = Some(result);
                screen.requests.favorites = None;
            }
            Fetched::Playlists { result, .. } => {
                screen.playlists = Some(result);
                screen.requests.playlists = None;
            }
            Fetched::Services { result, .. } => {
                screen.services = Some(result);
                screen.requests.services = None;
            }
            Fetched::Alarms { result, .. } => {
                screen.alarms = Some(result);
                screen.requests.alarms = None;
            }
            Fetched::AlarmSounds { result, .. } => {
                screen.alarm_sounds = Some(result);
                screen.requests.alarm_sounds = None;
            }
            Fetched::History { result, .. } => {
                screen.history = Some(result);
                screen.requests.history = None;
            }
            Fetched::About { result, .. } => {
                screen.about = Some(result);
                screen.requests.about = None;
            }
            Fetched::SignIns { result, .. } => {
                screen.sign_ins = Some(result);
                screen.requests.sign_ins = None;
            }
            Fetched::SearchSection { term, section, .. } if screen.searched.as_ref() == Some(&term) => {
                // A service with nothing to show takes no room; one that failed is named.
                if let Some(pos) = screen.sections.iter().position(|s| s.service.id == section.service.id) {
                    screen.sections[pos] = section;
                } else {
                    screen.sections.push(section);
                }
                // Preferred services lead, then signed-in ones, then the rest.
                let preferred = &self.prefs.preferred_services;
                let order = |s: &Section| (s.result.as_ref().map(|r| r.is_empty()).unwrap_or(true), preferred.iter().position(|p| *p == s.service.id).unwrap_or(usize::MAX), !s.service.linked, s.service.name.to_lowercase());
                screen.sections.sort_by_key(order);
            }
            Fetched::SearchDone { term, .. } => {
                if screen.searched.as_ref() == Some(&term) {
                    screen.searching = false;
                }
                screen.requests.search = None;
            }
            Fetched::ServicePage { service, container, index, result, .. } => {
                if let Kind::ServicePage { service: s2, container: c2, .. } = &screen.kind
                    && *s2 == service && *c2 == container {
                        match result {
                            Ok((items, total)) => {
                                if index == 0 && screen.page.is_empty() {
                                    screen.page = items;
                                    screen.page_total = total;
                                    screen.page_error = None;
                                    screen.page_loading = false;
                                    screen.requests.page = None;
                                } else if index != 0 && index == screen.page.len() as u32 {
                                    screen.page.extend(items);
                                    screen.page_total = total;
                                    screen.page_error = None;
                                    screen.page_loading = false;
                                    screen.requests.page = None;
                                }
                            }
                            Err(e) => {
                                screen.page_error = Some(e);
                                screen.page_loading = false;
                                screen.requests.page = None;
                            }
                        }
                    }
            }
            Fetched::Stations { fetch, result, .. } => {
                let categories = radio_categories();
                let wanted = match &fetch {
                    Fetch::Stations { tag, .. } => categories.get(screen.category).map(|c| &c.1) == Some(tag),
                    _ => false,
                };
                if wanted {
                    screen.stations = Some(result);
                }
                screen.requests.stations = None;
            }
            _ => {}
        }
        self.clamp_focus();
    }

    /// What the room plays, for the bar at a screen's foot.
    fn bar_state(&mut self, group: &str) -> BarState {
        let live = self.live(group);
        let info = RoomInfo::from_live(&live);
        // Not for a television input: a play key for it means nothing.
        let title = info.track_name.clone().or(info.stream_info.clone()).or(info.source.clone());
        let Some(title) = title.filter(|_| !info.on_tv_input) else { return BarState::default() };
        let subtitle = if info.track_name.is_some() { info.artist.clone() } else { info.source.clone().filter(|s| *s != title) };
        let art = self.art_image(info.art_url.as_deref());
        let Some(ui) = self.ui.upgrade() else { return BarState::default() };
        let icons = ui.global::<Icons>();
        BarState {
            shown: true,
            title: title.into(),
            subtitle: subtitle.unwrap_or_default().into(),
            has_art: art.is_some(),
            art: art.unwrap_or_default(),
            play_icon: if live.playing() {
                if live.actions.can_pause { icons.get_pause() } else { icons.get_stop() }
            } else {
                icons.get_play_arrow()
            },
        }
    }

    pub(super) fn render_screen(&mut self) {
        self.art.begin(super::ArtSurface::List);
        self.render_screen_contents();
        self.finish_art_render();
    }

    fn render_screen_contents(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(kind) = self.stack.last().map(|s| s.kind.clone()) else {
            self.models.list_rows.set(vec![]);
            ui.set_bar(BarState::default());
            ui.set_screen(Screen::Home);
            return;
        };
        let group = self.stack.last().map(|s| s.group.clone()).unwrap_or_default();
        let room = self.rooms.iter().find(|r| r.group_id == group).cloned();
        let household_screen = matches!(kind, Kind::Alarms | Kind::About | Kind::PreferredServices | Kind::Report);
        let room_name = match self.alarm_draft().and_then(|d| d.room.as_ref()) {
            // The alarm being made names its room, as every screen opened from one does.
            Some((_, name)) if kind == Kind::Alarms => name.clone(),
            _ if household_screen => String::new(),
            _ => room.as_ref().map(|r| self.room_label(r)).unwrap_or_default(),
        };
        if kind == Kind::NowPlaying {
            self.models.list_rows.set(vec![]);
            ui.set_bar(BarState::default());
            ui.set_screen(Screen::NowPlaying);
            ui.set_clock(super::panels::clock_time(chrono::Local::now()).into());
            // A step of drift a minute, round a small square.
            const DRIFT: [(f32, f32); 6] = [(0., 0.), (6., 0.), (6., 6.), (0., 6.), (-6., 6.), (-6., 0.)];
            let minutes = self.stack.last().map(|s| s.opened_at.elapsed().as_secs() / 60).unwrap_or(0) as usize;
            let (dx, dy) = DRIFT[minutes % DRIFT.len()];
            ui.set_drift_x(dx);
            ui.set_drift_y(dy);
            return;
        }
        let icons = ui.global::<Icons>();
        let mut rows = Rows::default();
        let page_title = match &kind {
            Kind::ServicePage { title, .. } => title.clone(),
            _ => String::new(),
        };
        let (title, header): (&str, Vec<HeaderEntry>) = match kind {
            Kind::Queue => {
                let armed = self.stack.last().is_some_and(|s| s.clear_armed);
                (
                    "Queue",
                    vec![
                        (HeaderAction::SaveQueue, None, "Save as playlist"),
                        (HeaderAction::ClearQueue, None, if armed { "Press again to clear" } else { "Clear" }),
                    ],
                )
            }
            Kind::Browse => (
                "Browse",
                vec![
                    (HeaderAction::MusicServices, Some(icons.get_library_music()), "Music Services"),
                    (HeaderAction::Radio, Some(icons.get_radio()), "Radio"),
                ],
            ),
            Kind::Radio => ("Radio", vec![]),
            Kind::Search => ("Search", vec![]),
            Kind::Alarms => (self.alarm_title().unwrap_or("Alarms"), vec![]),
            Kind::About => ("About this system", vec![]),
            Kind::Report => ("Send a report", vec![]),
            Kind::PreferredServices => ("Preferred services", vec![]),
            Kind::Services => ("Music Services", vec![]),
            Kind::ServicePage { .. } => (page_title.as_str(), vec![]),
            Kind::NowPlaying => unreachable!(),
        };
        let screen_snapshot = self.stack.last().map(|s| (s.queue.clone(), s.favorites.clone(), s.playlists.clone(), s.stations.clone(), s.category));
        let Some((queue, favorites, playlists, stations, category)) = screen_snapshot else { return };
        let mut loading = false;
        // Send a report's link, drawn as a code beside the list.
        let mut aside: Option<String> = None;
        let url_mode_now = self.stack.last().is_some_and(|s| s.url_mode);
        match kind {
            Kind::Queue => match queue {
                None => loading = true,
                Some(Err(e)) => {
                    rows.message(&format!("Failed to load queue: {e}"));
                    rows.button("Retry", RowAction::Retry);
                }
                Some(Ok(q)) if q.items.is_empty() => rows.message("Nothing in the queue"),
                Some(Ok(q)) => {
                    for item in &q.items {
                        let art = self.thumbnail(item.art_url.as_deref());
                        let subtitle = [item.artist.as_deref(), item.album.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" \u{2022} ");
                        rows.item(
                            ListCell {
                                title: item.title.as_str().into(),
                                subtitle: subtitle.into(),
                                number: item.index.to_string().into(),
                                has_art: art.is_some(),
                                art: art.unwrap_or_default(),
                                active: q.current == Some(item.index),
                                ..Default::default()
                            },
                            RowAction::QueueTrack(item.index),
                        );
                    }
                    if q.total as usize > q.items.len() {
                        rows.message(&format!("{} more not shown", q.total as usize - q.items.len()));
                    }
                }
            },
            Kind::Browse => {
                // What the room plays now, found among the favourites by name.
                let playing = room
                    .as_ref()
                    .map(|r| RoomInfo::from_live(&self.live(&r.group_id)))
                    .and_then(|i| i.source.clone());
                // This device's presets first, as the TV app's Browse lists them.
                if !self.prefs.presets.is_empty() {
                    rows.heading("Presets");
                    for preset in self.prefs.presets.clone() {
                        let detail = format!("{} speaker{}{}", preset.player_ids.len(), if preset.player_ids.len() == 1 { "" } else { "s" }, if preset.favorite_id.is_some() { " \u{00B7} a favourite" } else { "" });
                        rows.item(ListCell { title: preset.name.as_str().into(), subtitle: detail.into(), ..Default::default() }, RowAction::Preset(preset));
                    }
                }
                match &favorites {
                    None => loading = true,
                    Some(Err(e)) => {
                        rows.message(&format!("Couldn't load favourites: {e}"));
                        rows.button("Retry", RowAction::Retry);
                    }
                    Some(Ok(items)) => {
                        for (title, group) in favorite_groups(items) {
                            rows.heading(title);
                            for f in group {
                                let art = self.thumbnail(f.image_url.as_deref());
                                rows.item(
                                    ListCell {
                                        title: f.name.as_str().into(),
                                        subtitle: favorite_line(f).into(),
                                        has_art: art.is_some(),
                                        art: art.unwrap_or_default(),
                                        active: playing.as_deref() == Some(f.name.as_str()),
                                        ..Default::default()
                                    },
                                    RowAction::Favorite(f.id.clone(), f.name.clone()),
                                );
                            }
                        }
                    }
                }
                if let Some(Ok(lists)) = &playlists
                    && !lists.is_empty() {
                        rows.heading("Playlists");
                        for p in lists {
                            let tracks = p.track_count.map(|n| if n == 1 { "1 track".to_owned() } else { format!("{n} tracks") }).unwrap_or_default();
                            rows.item(
                                ListCell { title: p.name.as_str().into(), subtitle: tracks.into(), ..Default::default() },
                                RowAction::Playlist(p.id.clone(), p.name.clone()),
                            );
                        }
                    }
                let history = self.stack.last().and_then(|s| s.history.clone());
                if let Some(Ok(items)) = history
                    && !items.is_empty() {
                        rows.heading("Recently played");
                        for item in items.into_iter().take(20) {
                            let art = self.thumbnail(item.art_url());
                            let kind = match item.kind.to_lowercase().as_str() {
                                "album" => "Album",
                                "playlist" => "Playlist",
                                "stream" | "program" => "Radio",
                                "track" => "Track",
                                _ => "",
                            };
                            rows.item(
                                ListCell {
                                    title: item.name.as_str().into(),
                                    subtitle: kind.into(),
                                    has_art: art.is_some(),
                                    art: art.unwrap_or_default(),
                                    glyph: if kind == "Radio" { Glyph::Radio } else { Glyph::None },
                                    ..Default::default()
                                },
                                RowAction::Replay(item),
                            );
                        }
                    }
                if matches!(&favorites, Some(Ok(f)) if f.is_empty()) && matches!(&playlists, Some(Ok(p)) if p.is_empty()) {
                    rows.message("This system has no favourites or playlists yet.");
                }
            }
            Kind::Radio if url_mode_now => {}
            Kind::Radio => match &stations {
                None => loading = true,
                Some(Err(e)) => {
                    rows.message(&format!("Couldn't reach the radio directory: {e}"));
                    rows.button("Retry", RowAction::Retry);
                }
                Some(Ok(list)) if list.is_empty() => rows.message("No stations"),
                Some(Ok(list)) => {
                    for station in list {
                        let mut tags: Vec<&str> = station.tags.split(',').map(str::trim).filter(|t| !t.is_empty()).take(3).collect();
                        let format = station.format();
                        if format != "\u{2014}" {
                            tags.insert(0, &format);
                        }
                        let subtitle = [station.country.as_str()].into_iter().filter(|c| !c.is_empty()).chain(tags).collect::<Vec<_>>().join(" \u{00B7} ");
                        rows.item(
                            ListCell { title: station.name.trim().into(), subtitle: subtitle.into(), glyph: Glyph::Radio, ..Default::default() },
                            RowAction::Station(station.clone()),
                        );
                    }
                }
            },
            Kind::Search => self.search_rows(&mut rows),
            Kind::Alarms if self.alarm_draft().is_some() => {
                let screen = self.stack.last();
                let draft = screen.and_then(|s| s.alarm_draft.clone()).unwrap_or_default();
                let sounds = screen.and_then(|s| s.alarm_sounds.clone());
                self.alarm_rows(&mut rows, &draft, sounds.as_ref());
            }
            Kind::Alarms => {
                let alarms = self.stack.last().and_then(|s| s.alarms.clone());
                match alarms {
                    None => loading = true,
                    Some(Err(e)) => {
                        rows.message(&format!("Couldn't read the alarms: {e}"));
                        rows.button("Retry", RowAction::Retry);
                    }
                    Some(Ok((alarms, clock, zoned))) => {
                        // Times are the household's clock, never converted.
                        let now = clock.get(11..16).or(clock.get(0..5)).unwrap_or(&clock).to_owned();
                        rows.message(&if zoned {
                            format!("Times are this system's clock. It's {now} there now. Select turns one on or off; a hold or Menu changes or removes it.")
                        } else {
                            format!("Times are this system's clock, which has no timezone set and so runs UTC. It's {now} there now. Select turns one on or off; a hold or Menu changes or removes it.")
                        });
                        rows.item(
                            ListCell { title: "New alarm".into(), subtitle: "A room, a time, how often, and what it plays".into(), ..Default::default() },
                            RowAction::AlarmEditor(super::alarms::AlarmRow::New),
                        );
                        if alarms.is_empty() {
                            rows.message("No alarms yet.");
                        }
                        let mut alarms = alarms;
                        alarms.sort_by(|a, b| a.start.cmp(&b.start));
                        for alarm in alarms {
                            let room = self.player_name(&alarm.room_uuid);
                            let time = alarm.start.get(0..5).unwrap_or(&alarm.start).to_owned();
                            let subtitle = [recurrence_words(&alarm.recurrence), alarm_sound(&alarm), format!("Volume {}", alarm.volume), if alarm.enabled { "On".into() } else { "Off".into() }].join(" \u{00B7} ");
                            rows.item(
                                ListCell {
                                    title: format!("{time} \u{00B7} {}", if room.is_empty() { "A room not here" } else { &room }).into(),
                                    subtitle: subtitle.into(),
                                    active: alarm.enabled,
                                    ..Default::default()
                                },
                                RowAction::Alarm(alarm),
                            );
                        }
                    }
                }
            }
            Kind::PreferredServices => {
                let services = self.stack.last().and_then(|s| s.services.clone());
                match services {
                    None => loading = true,
                    Some(Err(e)) => {
                        rows.message(&format!("Couldn't list the music services: {e}"));
                        rows.button("Retry", RowAction::Retry);
                    }
                    Some(Ok(list)) => {
                        rows.message("Preferred services lead in Search and in Music Services, in the order chosen. Select one to add it or take it out.");
                        let preferred = self.prefs.preferred_services.clone();
                        let mut list = list;
                        list.sort_by_key(|s| (preferred.iter().position(|p| *p == s.id).unwrap_or(usize::MAX), s.name.to_lowercase()));
                        for service in list {
                            let rank = preferred.iter().position(|p| *p == service.id);
                            let logo = crate::logos::logo_url(&service.name);
                            let art = self.thumbnail(logo.as_deref());
                            rows.item(
                                ListCell {
                                    title: service.name.as_str().into(),
                                    subtitle: rank.map(|r| format!("Preferred \u{00B7} {}", r + 1)).unwrap_or_default().into(),
                                    has_art: art.is_some(),
                                    art: art.unwrap_or_default(),
                                    active: rank.is_some(),
                                    ..Default::default()
                                },
                                RowAction::TogglePreferred(service.id),
                            );
                        }
                    }
                }
            }
            Kind::About => {
                let about = self.stack.last().and_then(|s| s.about.clone());
                match about {
                    None => loading = true,
                    Some(Err(e)) => {
                        rows.message(&format!("Couldn't read the system: {e}"));
                        rows.button("Retry", RowAction::Retry);
                    }
                    Some(Ok(list)) => {
                        let waiting = list.iter().filter(|s| s.update_waiting).count();
                        rows.message(&match waiting {
                            0 => "Every speaker is up to date.".to_owned(),
                            1 => "1 speaker has an update waiting. Updates are applied in the Sonos app.".to_owned(),
                            n => format!("{n} speakers have an update waiting. Updates are applied in the Sonos app."),
                        });
                        for speaker in list {
                            let line = [speaker.model.as_str(), &format!("Version {}", speaker.version), if speaker.update_waiting { "Update waiting" } else { "" }]
                                .into_iter()
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                                .join(" \u{00B7} ");
                            rows.item(ListCell { title: speaker.room.as_str().into(), subtitle: line.into(), active: speaker.update_waiting, ..Default::default() }, RowAction::Speaker);
                        }
                    }
                }
            }
            Kind::Report => {
                let (about, sign_ins) = self.stack.last().map(|s| (s.about.clone(), s.sign_ins.clone())).unwrap_or_default();
                match (about, sign_ins) {
                    (Some(about), Some(sign_ins)) => {
                        // A speaker that does not answer is still listed, as About lists it.
                        let report = crate::report::report(&self.report_input(about.unwrap_or_default(), sign_ins));
                        for block in report.text.split("\n\n").filter(|b| !b.trim().is_empty()) {
                            rows.block(block.trim().replace("**", "").as_str(), RowAction::ReportBlock);
                        }
                        aside = Some(report.url);
                    }
                    _ => loading = true,
                }
            }
            Kind::Services => {
                let services = self.stack.last().and_then(|s| s.services.clone());
                match services {
                    None => loading = true,
                    Some(Err(e)) => {
                        rows.message(&format!("Couldn't list the music services: {e}"));
                        rows.button("Retry", RowAction::Retry);
                    }
                    Some(Ok(list)) => {
                        let preferred = self.prefs.preferred_services.clone();
                        let mut list = list;
                        list.sort_by_key(|s| preferred.iter().position(|p| *p == s.id).unwrap_or(usize::MAX));
                        for service in list {
                            let logo = crate::logos::logo_url(&service.name);
                            let art = self.thumbnail(logo.as_deref());
                            rows.item(
                                ListCell {
                                    title: service.name.as_str().into(),
                                    subtitle: if service.linked { "Signed in" } else { "No account needed" }.into(),
                                    has_art: art.is_some(),
                                    art: art.unwrap_or_default(),
                                    ..Default::default()
                                },
                                RowAction::OpenService(service),
                            );
                        }
                    }
                }
            }
            Kind::ServicePage { ref service, .. } => {
                let service = service.clone();
                let (items, total, error, page_loading) = self
                    .stack
                    .last()
                    .map(|s| (s.page.clone(), s.page_total, s.page_error.clone(), s.page_loading))
                    .unwrap_or_default();
                loading = page_loading && items.is_empty();
                if let Some(e) = error {
                    rows.message(&format!("Couldn't open this page: {e}"));
                    rows.button("Try again", RowAction::Retry);
                } else if items.is_empty() && !page_loading {
                    rows.message("Nothing here");
                }
                let (book, resume) = self.stack.last().map(|s| (s.book.clone(), s.resume_chapter.clone())).unwrap_or_default();
                for item in &items {
                    let art = self.thumbnail(item.art_url.as_deref());
                    // A book's chapter it resumes at carries on from the place, by
                    // playing the book; any other plays from its own start.
                    let resumes = resume.as_deref() == Some(item.id.as_str());
                    let action = match (&book, resumes) {
                        (Some(book), true) => RowAction::ServiceItem { service: service.clone(), item: book.clone() },
                        _ => RowAction::ServiceItem { service: service.clone(), item: item.clone() },
                    };
                    rows.item(
                        ListCell {
                            title: item.title.as_str().into(),
                            subtitle: item_line(item).into(),
                            has_art: art.is_some(),
                            art: art.unwrap_or_default(),
                            glyph: if item.item_type.eq_ignore_ascii_case("stream") || x2rock::sonos::smapi::is_program(&item.item_type) { Glyph::Radio } else { Glyph::None },
                            status: if resumes { "Resumes at".into() } else { Default::default() },
                            active: resumes,
                            ..Default::default()
                        },
                        action,
                    );
                }
                if (items.len() as u32) < total && !page_loading {
                    rows.button("Load more", RowAction::LoadMore);
                }
            }
            Kind::NowPlaying => {}
        }
        let url_mode = self.stack.last().is_some_and(|s| s.url_mode);
        let side: Vec<SideItem> = if kind == Kind::Radio {
            // A stream's own address, one press up from Popular: the directory is
            // the cheap path on a remote, and typing the exception.
            std::iter::once(SideItem { label: "Play a URL".into(), selected: url_mode })
                .chain(radio_categories().into_iter().enumerate().map(|(i, (label, _))| SideItem { label: label.into(), selected: !url_mode && i == category }))
                .collect()
        } else {
            vec![]
        };
        let header_buttons: Vec<HeaderButton> = header
            .iter()
            .map(|(_, icon, label)| HeaderButton { has_icon: icon.is_some(), icon: icon.clone().unwrap_or_default(), label: (*label).into() })
            .collect();
        let bar = self.bar_state(&group);
        let focus = self.focus;
        if let Some(screen) = self.stack.last_mut() {
            screen.header = std::iter::once(HeaderAction::Back).chain(header.into_iter().map(|(a, _, _)| a)).collect();
            // Focus follows its row, not its index, when rows move under it: a
            // search section answering sorts in above the one being read.
            if focus.0 == zone::LIST
                && let Some(at) = follow_row(&screen.actions, &rows.actions, focus.1) {
                    self.focus.1 = at;
                }
            // A book's chapters open on the one it resumes at, once it is known.
            // Only while focus is in the list: a viewer gone up to the header stays there.
            if let (Some(book), false, zone::LIST) = (&screen.book, screen.resume_placed, focus.0) {
                let at = rows.actions.iter().position(|a| matches!(a, RowAction::ServiceItem { item, .. } if item.id == book.id));
                if let Some(at) = at {
                    screen.resume_placed = true;
                    self.focus = (zone::LIST, at as i32);
                }
            }
            screen.actions = rows.actions;
            screen.cells = rows.cells;
        }
        ui.set_screen(Screen::List);
        ui.set_list_title(title.into());
        ui.set_list_room(room_name.into());
        self.models.list_actions.set(header_buttons);
        self.models.list_rows.set(rows.rows);
        ui.set_list_loading(loading);
        self.models.side.set(side);
        // The code is drawn once a link: a report does not change while it is shown.
        let qr = aside.and_then(|url| {
            let screen = self.stack.last_mut()?;
            if screen.report_qr.as_ref().is_none_or(|(u, _)| *u != url) {
                let (side, pixels) = crate::report::qr_pixels(&url)?;
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&pixels, side, side);
                screen.report_qr = Some((url, slint::Image::from_rgba8(buffer)));
            }
            screen.report_qr.as_ref().map(|(_, image)| image.clone())
        });
        ui.set_list_aside_shown(qr.is_some());
        ui.set_list_aside_image(qr.unwrap_or_default());
        ui.set_pin_shown(matches!(kind, Kind::Browse | Kind::Radio | Kind::Search | Kind::Services | Kind::ServicePage { .. }));
        self.render_search_bar(&ui, &kind);
        ui.set_pin_held(self.prefs.after_playing.as_deref() == Some("stay"));
        ui.set_bar(bar);
        let _ = SharedString::new();
    }

    fn screen_kind(&self) -> Option<Kind> {
        self.stack.last().map(|s| s.kind.clone())
    }

    pub(super) fn screen_zone_len(&self, z: i32) -> i32 {
        let Some(screen) = self.stack.last() else { return 0 };
        (match z {
            zone::HEADER => screen.header.len(),
            zone::LIST => screen.actions.len(),
            zone::BAR => usize::from(self.ui.upgrade().is_some_and(|ui| ui.get_bar().shown)),
            zone::PIN => usize::from(matches!(screen.kind, Kind::Browse | Kind::Radio | Kind::Search | Kind::Services | Kind::ServicePage { .. })),
            zone::FIELD => if screen.kind == Kind::Search || (screen.kind == Kind::Radio && screen.url_mode) { 2 } else { 0 },
            zone::SIDE => if screen.kind == Kind::Radio { radio_categories().len() + 1 } else { 0 },
            zone::FULL => 1,
            _ => 0,
        }) as i32
    }

    /// Up, Down, Left and Right on a screen.
    pub(super) fn navigate_screen(&mut self, action: crate::input::Action) {
        use crate::input::Action;
        let (z, i) = self.focus;
        let has_side = self.screen_zone_len(zone::SIDE) > 0;
        let has_pin = self.screen_zone_len(zone::PIN) > 0;
        let has_bar = self.screen_zone_len(zone::BAR) > 0;
        let list_len = self.screen_zone_len(zone::LIST);
        match (z, action) {
            (zone::HEADER, Action::Left) if i > 0 => self.focus.1 = i - 1,
            (zone::HEADER, Action::Right) if i + 1 < self.screen_zone_len(zone::HEADER) => self.focus.1 = i + 1,
            (zone::HEADER, Action::Right) if has_pin => self.set_focus(zone::PIN, 0),
            (zone::HEADER | zone::PIN, Action::Down) => {
                if self.screen_zone_len(zone::FIELD) > 0 {
                    self.set_focus(zone::FIELD, 0);
                } else if has_side {
                    let at = self.stack.last().map(|s| if s.url_mode { 0 } else { s.category + 1 }).unwrap_or(1) as i32;
                    self.set_focus(zone::SIDE, at);
                } else if list_len > 0 {
                    self.set_focus(zone::LIST, 0);
                } else if has_bar {
                    self.set_focus(zone::BAR, 0);
                }
            }
            (zone::PIN, Action::Left) => {
                let last = self.screen_zone_len(zone::HEADER) - 1;
                self.set_focus(zone::HEADER, last);
            }
            (zone::FIELD, Action::Left) if i > 0 => self.focus.1 = 0,
            (zone::FIELD, Action::Right) if i == 0 => self.focus.1 = 1,
            (zone::FIELD, Action::Up) => self.set_focus(zone::HEADER, 0),
            (zone::FIELD, Action::Down) if list_len > 0 => self.set_focus(zone::LIST, 0),
            (zone::SIDE, Action::Up) if i > 0 => self.focus.1 = i - 1,
            (zone::SIDE, Action::Up) => self.set_focus(zone::HEADER, 0),
            (zone::SIDE, Action::Down) if i + 1 < self.screen_zone_len(zone::SIDE) => self.focus.1 = i + 1,
            (zone::SIDE, Action::Right) if list_len > 0 => self.set_focus(zone::LIST, 0),
            (zone::LIST, _) if self.stack.last().and_then(|s| s.actions.get(i.max(0) as usize)).cloned().is_some_and(|row| self.alarm_key(&row, action)) => {}
            (zone::LIST, _) => {
                let Some(screen) = self.stack.last() else { return };
                // A list still loading, or empty, still lets Up and Down out.
                let Some(&(row, col)) = screen.cells.get(i.max(0) as usize) else {
                    match action {
                        Action::Up if self.screen_zone_len(zone::FIELD) > 0 => self.set_focus(zone::FIELD, 0),
                        Action::Up => self.set_focus(zone::HEADER, 0),
                        Action::Down if has_bar => self.set_focus(zone::BAR, 0),
                        Action::Left if has_side => {
                            let at = if screen.url_mode { 0 } else { screen.category as i32 + 1 };
                            self.set_focus(zone::SIDE, at);
                        }
                        _ => {}
                    }
                    return;
                };
                match action {
                    Action::Up | Action::Down => {
                        let target = if action == Action::Down { Some(row + 1) } else { row.checked_sub(1) };
                        let found = target.and_then(|t| {
                            let in_row: Vec<usize> = screen.cells.iter().enumerate().filter(|(_, c)| c.0 == t).map(|(k, _)| k).collect();
                            in_row.iter().copied().find(|&k| screen.cells[k].1 == col).or(in_row.last().copied())
                        });
                        let has_field = self.screen_zone_len(zone::FIELD) > 0;
                        match found {
                            Some(k) => self.focus.1 = k as i32,
                            None if action == Action::Up && has_field => self.set_focus(zone::FIELD, 0),
                            None if action == Action::Up => self.set_focus(zone::HEADER, 0),
                            None if has_bar => self.set_focus(zone::BAR, 0),
                            None => {}
                        }
                    }
                    Action::Left => match screen.cells.iter().position(|&c| c == (row, col.wrapping_sub(1))) {
                        Some(k) if col > 0 => self.focus.1 = k as i32,
                        _ if has_side => {
                            let at = if screen.url_mode { 0 } else { screen.category as i32 + 1 };
                            self.set_focus(zone::SIDE, at);
                        }
                        _ => {}
                    },
                    Action::Right => {
                        if let Some(k) = screen.cells.iter().position(|&c| c == (row, col + 1)) {
                            self.focus.1 = k as i32;
                        }
                    }
                    _ => {}
                }
            }
            (zone::BAR, Action::Up) => {
                if list_len > 0 {
                    self.set_focus(zone::LIST, list_len - 1);
                } else {
                    self.set_focus(zone::HEADER, 0);
                }
            }
            _ => {}
        }
    }

    pub(super) fn screen_click(&mut self, z: i32, index: i32) {
        let Some(screen) = self.stack.last() else { return };
        let group = screen.group.clone();
        match z {
            zone::HEADER => {
                let Some(action) = screen.header.get(index.max(0) as usize).cloned() else { return };
                match action {
                    HeaderAction::Back => {
                        if !self.alarm_back() {
                            self.close_screen();
                        }
                    }
                    HeaderAction::ClearQueue => {
                        // Two presses, as the TV app's Clear.
                        let armed = screen.clear_armed;
                        if armed {
                            let edit = Box::new(Command::ClearQueue { group: group.clone() });
                            let id = self.next_request_id();
                            if let Some(s) = self.stack.last_mut() {
                                s.requests.queue = Some(id);
                            }
                            self.send(Command::EditThenRead { edit, read: Fetch::Queue { id: Some(id), group } });
                        }
                        if let Some(s) = self.stack.last_mut() {
                            s.clear_armed = !armed;
                        }
                    }
                    HeaderAction::SaveQueue => self.notice("Saving the queue is on its way in a later build"),
                    HeaderAction::MusicServices => self.open_screen(Kind::Services),
                    HeaderAction::Radio => self.open_screen(Kind::Radio),
                }
            }
            zone::PIN => {
                let stay = self.prefs.after_playing.as_deref() == Some("stay");
                self.prefs.after_playing = if stay { None } else { Some("stay".into()) };
                self.prefs.save();
            }
            zone::BAR => self.bar_play_pause(&group),
            zone::SIDE if index == 0 => {
                if let Some(s) = self.stack.last_mut() {
                    s.url_mode = true;
                }
                self.set_focus(zone::FIELD, 0);
                self.open_keyboard();
            }
            zone::SIDE => {
                let categories = radio_categories();
                let Some((_, tag)) = categories.get(index.max(1) as usize - 1).cloned() else { return };
                let id = self.next_request_id();
                if let Some(s) = self.stack.last_mut() {
                    s.requests.stations = Some(id);
                    s.category = index as usize - 1;
                    s.stations = None;
                    s.url_mode = false;
                }
                self.send(Command::Fetch(Fetch::Stations { id: Some(id), tag, country: None }));
            }
            zone::LIST => {
                let Some(action) = screen.actions.get(index.max(0) as usize).cloned() else { return };
                match action {
                    RowAction::QueueTrack(n) => self.send(Command::PlayQueueTrack { group, index: n }),
                    RowAction::Favorite(id, name) => {
                        self.send(Command::PlayFavorite { group, id });
                        self.notice(&format!("Starting {name}"));
                        self.played();
                    }
                    RowAction::Playlist(id, name) => {
                        self.send(Command::PlayPlaylist { group, id });
                        self.notice(&format!("Starting {name}"));
                        self.played();
                    }
                    RowAction::Station(station) => {
                        let url = if station.url_resolved.is_empty() { station.url.clone() } else { station.url_resolved.clone() };
                        self.send(Command::PlayStream { group, url, title: station.name.trim().to_owned() });
                        self.notice(&format!("Starting {}", station.name.trim()));
                        self.played();
                    }
                    RowAction::Retry => self.refetch(),
                    RowAction::OpenService(service) => {
                        let title = service.name.clone();
                        self.open_screen(Kind::ServicePage { service: service.id, container: "root".into(), title });
                    }
                    RowAction::ServiceItem { service, item } => {
                        if item.opens() {
                            // A place to go: an album, a playlist, a shelf.
                            self.open_screen(Kind::ServicePage { service, container: item.id.clone(), title: item.title.clone() });
                        } else {
                            let title = item.title.clone();
                            self.send(Command::PlayServiceItem { group, service, item });
                            self.notice(&format!("Starting {title}"));
                            self.played();
                        }
                    }
                    RowAction::Alarm(alarm) => {
                        let enabled = !alarm.enabled;
                        // Nothing pushes alarms: read again once the change is made.
                        let id = self.next_request_id();
                        if let Some(s) = self.stack.last_mut() {
                            s.requests.alarms = Some(id);
                        }
                        self.send(Command::EditThenRead { edit: Box::new(Command::SetAlarmEnabled { alarm, enabled }), read: Fetch::Alarms { id: Some(id) } });
                    }
                    RowAction::Speaker | RowAction::ReportBlock => {}
                    RowAction::AlarmEditor(row) => self.alarm_click(row),
                    RowAction::Replay(item) => {
                        let name = item.name.clone();
                        self.send(Command::Replay { group, item });
                        self.notice(&format!("Starting {name}"));
                        self.played();
                    }
                    RowAction::TogglePreferred(id) => {
                        if self.prefs.preferred_services.contains(&id) {
                            self.prefs.preferred_services.retain(|p| *p != id);
                        } else {
                            self.prefs.preferred_services.push(id);
                        }
                        self.prefs.save();
                    }
                    RowAction::Preset(preset) => {
                        let host = preset.player_ids.first().cloned().map(|h| (h, preset.player_ids.len(), Instant::now()));
                        let name = preset.name.clone();
                        self.send(Command::ApplyPreset(preset));
                        self.notice(&format!("Starting {name}"));
                        // The room it forms becomes the selected room.
                        self.pending_select = host;
                        self.played();
                    }
                    RowAction::Recent(term) => {
                        if let Some(s) = self.stack.last_mut() {
                            s.query = term;
                        }
                        self.submit_search();
                    }
                    RowAction::LoadMore => {
                        let target = match self.stack.last() {
                            Some(screen) => match &screen.kind {
                                Kind::ServicePage { service, container, .. } => {
                                    Some((service.clone(), container.clone(), screen.page.len() as u32))
                                }
                                _ => None,
                            },
                            None => None,
                        };
                        if let Some((service, container, index)) = target {
                            let id = self.next_request_id();
                            if let Some(screen) = self.stack.last_mut() {
                                screen.requests.page = Some(id);
                                screen.page_loading = true;
                            }
                            self.send(Command::Fetch(Fetch::ServicePage { id: Some(id), service, container, index }));
                        }
                    }
                }
            }
            zone::FULL => self.close_screen(),
            zone::FIELD => {
                if index == 0 {
                    self.open_keyboard();
                } else if screen.kind == Kind::Radio {
                    self.play_url();
                } else {
                    self.submit_search();
                }
            }
            _ => {}
        }
    }

    /// Menu, or a hold, on a row: its item menu.
    pub(super) fn screen_hold(&mut self, z: i32, index: i32) {
        let Some(screen) = self.stack.last() else { return };
        if z != zone::LIST {
            return;
        }
        let Some(row) = screen.actions.get(index.max(0) as usize).cloned() else { return };
        let title = match &row {
            RowAction::QueueTrack(n) => screen
                .queue
                .as_ref()
                .and_then(|q| q.as_ref().ok())
                .and_then(|q| q.items.iter().find(|i| i.index == *n))
                .map(|i| i.title.clone())
                .unwrap_or_default(),
            RowAction::Favorite(_, name) | RowAction::Playlist(_, name) => name.clone(),
            RowAction::Station(station) => station.name.trim().to_owned(),
            RowAction::ServiceItem { item, .. } => item.title.clone(),
            RowAction::Recent(term) => term.clone(),
            RowAction::Preset(preset) => preset.name.clone(),
            RowAction::Replay(item) => item.name.clone(),
            // An alarm's menu is its editor, opened on its summary.
            RowAction::Alarm(alarm) => {
                let alarm = alarm.clone();
                self.start_alarm_edit(&alarm);
                return;
            }
            _ => return,
        };
        self.open_item_menu(row, title);
    }

    /// What the item menu's choice does to its row.
    pub(super) fn menu_chosen(&mut self, choice: super::panels::PanelAction, row: RowAction) {
        use super::panels::PanelAction as P;
        let Some(screen) = self.stack.last() else { return };
        let group = screen.group.clone();
        match (choice.clone(), row) {
            (P::MenuPlay | P::MenuOpen, row) => {
                // The row's own action, as Select on it.
                let index = screen.actions.iter().position(|a| format!("{a:?}") == format!("{row:?}"));
                if let Some(i) = index {
                    self.screen_click(zone::LIST, i as i32);
                }
            }
            (P::MenuPlayNext | P::MenuAddToQueue, RowAction::ServiceItem { service, item }) => {
                let next = matches!(choice, P::MenuPlayNext);
                self.send(Command::QueueServiceItem { group, service, item, next });
            }
            (P::MenuRemoveFromQueue, RowAction::QueueTrack(n)) => {
                if let Some(Ok(q)) = &screen.queue {
                    let update_id = q.update_id.clone();
                    let edit = Box::new(Command::RemoveFromQueue { group: group.clone(), index: n, update_id });
                    let id = self.next_request_id();
                    if let Some(s) = self.stack.last_mut() {
                        s.requests.queue = Some(id);
                    }
                    self.send(Command::EditThenRead { edit, read: Fetch::Queue { id: Some(id), group } });
                    self.notice("Removed from the queue");
                }
            }
            (P::MenuDeletePreset, RowAction::Preset(preset)) => {
                self.prefs.presets.retain(|p| p.id != preset.id);
                self.prefs.save();
                self.notice(&format!("Deleted {}", preset.name));
            }
            (P::MenuAddFavorite, RowAction::ServiceItem { service, item }) => {
                self.send(Command::ServiceFavorite { service, item, add: true });
            }
            (P::MenuRemoveFavorite, RowAction::ServiceItem { service, item }) => {
                self.send(Command::ServiceFavorite { service, item, add: false });
            }
            (P::MenuRemoveFavorite, RowAction::Favorite(id, name)) => {
                self.send(Command::RemoveFavorite { id, name });
            }
            (P::MenuChapters, RowAction::ServiceItem { service, item }) => {
                self.open_screen(Kind::ServicePage { service: service.clone(), container: item.id.clone(), title: item.title.clone() });
                let book = item.id.clone();
                if let Some(s) = self.stack.last_mut() {
                    s.book = Some(item);
                }
                self.send(Command::Fetch(Fetch::Resume { service, book }));
            }
            (P::MenuForget, RowAction::Recent(term)) => {
                self.prefs.recent_searches.retain(|t| *t != term);
                self.prefs.save();
            }
            _ => {}
        }
    }

    pub(super) fn refetch_screen_index(&mut self, i: usize) {
        let Some(screen) = self.stack.get(i) else { return };
        let kind = screen.kind.clone();
        let group = screen.group.clone();
        let category = screen.category;
        match kind {
            Kind::Queue => {
                let id = self.next_request_id();
                let screen = &mut self.stack[i];
                screen.requests.queue = Some(id);
                screen.queue = None;
                self.send(Command::Fetch(Fetch::Queue { id: Some(id), group }));
            }
            Kind::Browse => {
                let fav_id = self.next_request_id();
                let pl_id = self.next_request_id();
                let hist_id = self.next_request_id();
                let screen = &mut self.stack[i];
                screen.requests.favorites = Some(fav_id);
                screen.requests.playlists = Some(pl_id);
                screen.requests.history = Some(hist_id);
                screen.favorites = None;
                screen.playlists = None;
                screen.history = None;
                self.send(Command::Fetch(Fetch::Favorites { id: Some(fav_id) }));
                self.send(Command::Fetch(Fetch::Playlists { id: Some(pl_id) }));
                self.send(Command::Fetch(Fetch::History { id: Some(hist_id) }));
            }
            Kind::Radio => {
                let id = self.next_request_id();
                let screen = &mut self.stack[i];
                screen.requests.stations = Some(id);
                screen.stations = None;
                let tag = radio_categories().get(category).and_then(|c| c.1.clone());
                self.send(Command::Fetch(Fetch::Stations { id: Some(id), tag, country: None }));
            }
            Kind::Services | Kind::PreferredServices => {
                let id = self.next_request_id();
                let screen = &mut self.stack[i];
                screen.requests.services = Some(id);
                screen.services = None;
                self.send(Command::Fetch(Fetch::Services { id: Some(id) }));
            }
            Kind::ServicePage { service, container, .. } => {
                let id = self.next_request_id();
                let screen = &mut self.stack[i];
                screen.requests.page = Some(id);
                screen.page.clear();
                screen.page_error = None;
                screen.page_loading = true;
                self.send(Command::Fetch(Fetch::ServicePage { id: Some(id), service, container, index: 0 }));
            }
            Kind::Alarms => {
                let id = self.next_request_id();
                let sound_step = self.stack[i].alarm_draft.as_ref().is_some_and(|d| d.step == super::alarms::Step::Sound);
                self.stack[i].requests.alarms = Some(id);
                self.stack[i].alarms = None;
                self.send(Command::Fetch(Fetch::Alarms { id: Some(id) }));
                if sound_step {
                    let snd_id = self.next_request_id();
                    self.stack[i].requests.alarm_sounds = Some(snd_id);
                    self.stack[i].alarm_sounds = None;
                    self.send(Command::Fetch(Fetch::AlarmSounds { id: Some(snd_id) }));
                }
            }
            Kind::About => {
                let id = self.next_request_id();
                let screen = &mut self.stack[i];
                screen.requests.about = Some(id);
                screen.about = None;
                self.send(Command::Fetch(Fetch::About { id: Some(id) }));
            }
            Kind::Report => {
                let about_id = self.next_request_id();
                let sign_id = self.next_request_id();
                let screen = &mut self.stack[i];
                screen.requests.about = Some(about_id);
                screen.requests.sign_ins = Some(sign_id);
                screen.about = None;
                screen.sign_ins = None;
                self.send(Command::Fetch(Fetch::About { id: Some(about_id) }));
                self.send(Command::Fetch(Fetch::SignIns { id: Some(sign_id) }));
            }
            Kind::Search => {
                if let Some(term) = self.stack[i].searched.clone() {
                    let id = self.next_request_id();
                    let screen = &mut self.stack[i];
                    screen.requests.search = Some(id);
                    screen.sections.clear();
                    screen.searching = true;
                    self.send(Command::Fetch(Fetch::Search { id: Some(id), term }));
                }
            }
            Kind::NowPlaying => {}
        }
    }

    fn refetch(&mut self) {
        if let Some(last) = self.stack.len().checked_sub(1) {
            self.refetch_screen_index(last);
        }
    }

    pub(super) fn refetch_interrupted_screens(&mut self) {
        for i in 0..self.stack.len() {
            let interrupted = self.stack[i].interrupted;
            self.stack[i].interrupted = ScreenPending::default();
            let group = self.stack[i].group.clone();
            let category = self.stack[i].category;
            let kind = self.stack[i].kind.clone();

            if interrupted.queue {
                let id = self.next_request_id();
                self.stack[i].requests.queue = Some(id);
                self.send(Command::Fetch(Fetch::Queue { id: Some(id), group: group.clone() }));
            }
            if interrupted.favorites {
                let id = self.next_request_id();
                self.stack[i].requests.favorites = Some(id);
                self.send(Command::Fetch(Fetch::Favorites { id: Some(id) }));
            }
            if interrupted.playlists {
                let id = self.next_request_id();
                self.stack[i].requests.playlists = Some(id);
                self.send(Command::Fetch(Fetch::Playlists { id: Some(id) }));
            }
            if interrupted.history {
                let id = self.next_request_id();
                self.stack[i].requests.history = Some(id);
                self.send(Command::Fetch(Fetch::History { id: Some(id) }));
            }
            if interrupted.stations {
                let id = self.next_request_id();
                self.stack[i].requests.stations = Some(id);
                let tag = radio_categories().get(category).and_then(|c| c.1.clone());
                self.send(Command::Fetch(Fetch::Stations { id: Some(id), tag, country: None }));
            }
            if interrupted.services {
                let id = self.next_request_id();
                self.stack[i].requests.services = Some(id);
                self.send(Command::Fetch(Fetch::Services { id: Some(id) }));
            }
            if interrupted.page
                && let Kind::ServicePage { service, container, .. } = &kind
            {
                let id = self.next_request_id();
                self.stack[i].requests.page = Some(id);
                let index = self.stack[i].page.len() as u32;
                self.stack[i].page_loading = true;
                self.send(Command::Fetch(Fetch::ServicePage {
                    id: Some(id),
                    service: service.clone(),
                    container: container.clone(),
                    index,
                }));
            }
            if interrupted.search
                && self.stack[i].searching
                && let Some(term) = self.stack[i].searched.clone()
            {
                let id = self.next_request_id();
                self.stack[i].requests.search = Some(id);
                self.stack[i].sections.clear();
                self.send(Command::Fetch(Fetch::Search { id: Some(id), term }));
            }
            if interrupted.alarms {
                let id = self.next_request_id();
                self.stack[i].requests.alarms = Some(id);
                self.send(Command::Fetch(Fetch::Alarms { id: Some(id) }));
            }
            if interrupted.alarm_sounds {
                let id = self.next_request_id();
                self.stack[i].requests.alarm_sounds = Some(id);
                self.send(Command::Fetch(Fetch::AlarmSounds { id: Some(id) }));
            }
            if interrupted.about {
                let id = self.next_request_id();
                self.stack[i].requests.about = Some(id);
                self.send(Command::Fetch(Fetch::About { id: Some(id) }));
            }
            if interrupted.sign_ins {
                let id = self.next_request_id();
                self.stack[i].requests.sign_ins = Some(id);
                self.send(Command::Fetch(Fetch::SignIns { id: Some(id) }));
            }
        }
    }

    fn bar_play_pause(&mut self, group: &str) {
        let live = self.live(group);
        let command = if live.playing() { "pause" } else { "play" };
        self.send(Command::Playback { group: group.to_owned(), command });
    }

    /// The queue follows the room: a new track or an edit elsewhere moves its
    /// version, and the open Queue reads again.
    pub(super) fn queue_follows(&mut self, before: Option<String>) {
        let Some(group) = self.stack.last().filter(|s| s.kind == Kind::Queue).map(|s| s.group.clone()) else { return };
        let now = self.live(&group).metadata.as_ref().and_then(|m| m.track()).and_then(|t| t.name.clone());
        if now != before {
            let id = self.next_request_id();
            if let Some(screen) = self.stack.last_mut() {
                screen.requests.queue = Some(id);
            }
            self.send(Command::Fetch(Fetch::Queue { id: Some(id), group }));
        }
        let _ = Duration::ZERO;
        let _ = model::time_text;
    }

    pub(super) fn screen_is(&self, kind: Kind) -> bool {
        self.screen_kind() == Some(kind)
    }
}

/// A service item's second line: what it is, and the service's own summary.
impl App {
    /// What Send a report is made from: the speakers as read, with what the
    /// household says each can do, the rooms with a line-in as they are now,
    /// the connection and the last refusal.
    fn report_input(&self, speakers: Vec<crate::household::SpeakerInfo>, sign_ins: Option<Result<usize, String>>) -> crate::report::Input {
        use crate::household::Status;
        let players = self.house.groups.as_ref().map(|g| g.players.as_slice()).unwrap_or(&[]);
        let capabilities = |id: &str| players.iter().find(|p| p.id == id).map(|p| p.capabilities.clone()).unwrap_or_default();
        let speakers = speakers
            .into_iter()
            .map(|s| {
                let what = [s.model.trim_start_matches("Sonos ").to_owned(), s.version.clone(), if s.software.is_empty() { String::new() } else { format!("({})", s.software) }]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                crate::report::Speaker { room: s.room, what: (!what.is_empty()).then_some(what), capabilities: capabilities(&s.id) }
            })
            .collect();
        let line_in_rooms = players
            .iter()
            .filter(|p| p.capabilities.iter().any(|c| c == crate::report::LINE_IN))
            .filter_map(|p| {
                let group = self.groups().iter().find(|g| g.player_ids.contains(&p.id))?;
                let live = self.live(&group.id);
                let container = live.metadata.as_ref().and_then(|m| m.container.clone());
                let field = |v: Option<String>| v.unwrap_or_else(|| "none".into());
                Some(format!(
                    "{}: {} \u{00B7} container type {}, name {}, service {}",
                    group.name,
                    live.state.as_deref().unwrap_or("unknown"),
                    field(container.as_ref().and_then(|c| c.kind.clone())),
                    field(container.as_ref().and_then(|c| c.name.clone())),
                    field(container.as_ref().and_then(|c| c.service.as_ref()?.name.clone())),
                ))
            })
            .collect();
        let connection = match &self.status {
            Status::Reconnecting(reason) => Some(reason.clone()),
            _ => None,
        };
        let refused = |t: &Option<String>| t.as_deref().is_some_and(|t| t.contains("ERROR_NO_PERMISSION"));
        crate::report::Input {
            app: format!("x2rock-deck {}", env!("CARGO_PKG_VERSION")),
            device: crate::report::device(),
            authentication_required: refused(&connection) || refused(&self.last_refusal),
            connection,
            sign_ins,
            speakers,
            line_in_rooms,
            refusal: self.last_refusal.clone(),
        }
    }
}

/// Where the row focused at `index` in `old` now is in `new`, by what it is.
/// None when it is gone, or was not there: the index then stands, clamped.
fn follow_row(old: &[RowAction], new: &[RowAction], index: i32) -> Option<i32> {
    let held = format!("{:?}", old.get(usize::try_from(index).ok()?)?);
    let same = |a: &RowAction| format!("{a:?}") == held;
    // Most renders change nothing: look where it was first.
    if new.get(index as usize).is_some_and(same) {
        return Some(index);
    }
    new.iter().position(same).map(|at| at as i32)
}

pub fn item_line(item: &Item) -> String {
    let kind = match item.item_type.to_lowercase().as_str() {
        "track" => Some("Track"),
        "album" => Some("Album"),
        "artist" => Some("Artist"),
        "playlist" | "albumlist" | "tracklist" => Some("Playlist"),
        "stream" | "program" => Some("Radio"),
        "audiobook" => Some("Audiobook"),
        "show" | "podcast" => Some("Podcast"),
        _ => None,
    };
    [kind, item.summary.as_deref().filter(|s| !s.trim().is_empty())].into_iter().flatten().collect::<Vec<_>>().join(" \u{00B7} ")
}

impl App {
    /// Search's results, or, before a search, this device's recent ones.
    fn search_rows(&mut self, rows: &mut Rows) {
        let Some(screen) = self.stack.last() else { return };
        let (searched, sections, searching) = (screen.searched.clone(), screen.sections.clone(), screen.searching);
        let Some(_) = searched else {
            if !self.prefs.recent_searches.is_empty() {
                rows.heading("Recent searches");
                for term in self.prefs.recent_searches.clone() {
                    rows.item(ListCell { title: term.as_str().into(), ..Default::default() }, RowAction::Recent(term));
                }
            }
            return;
        };
        for section in &sections {
            let Ok(items) = &section.result else { continue };
            if items.is_empty() {
                continue;
            }
            let logo = crate::logos::logo_url(&section.service.name);
            let icon = self.thumbnail(logo.as_deref());
            rows.rows.push(ListRow {
                kind: RowKind::Heading,
                text: section.service.name.as_str().into(),
                has_icon: icon.is_some(),
                icon: icon.unwrap_or_default(),
                ..Default::default()
            });
            for chunk in items.chunks(3) {
                let mut cells = Vec::new();
                for (col, item) in chunk.iter().enumerate() {
                    let art = self.thumbnail(item.art_url.as_deref());
                    cells.push(ListCell {
                        title: item.title.as_str().into(),
                        subtitle: item_line(item).into(),
                        has_art: art.is_some(),
                        art: art.unwrap_or_default(),
                        focus: rows.actions.len() as i32,
                        ..Default::default()
                    });
                    rows.actions.push(RowAction::ServiceItem { service: section.service.id.clone(), item: item.clone() });
                    rows.cells.push((rows.row, col));
                }
                rows.row += 1;
                rows.rows.push(ListRow { kind: RowKind::Grid, cells: ModelRc::new(VecModel::from(cells)), ..Default::default() });
            }
        }
        if sections.iter().all(|s| s.result.as_ref().map(|r| r.is_empty()).unwrap_or(true)) && !searching {
            rows.message("Nothing found");
        }
        for section in &sections {
            if let Err(why) = &section.result
                && why != "didn't answer" {
                    rows.message(&format!("Couldn't search {}: {why}", section.service.name));
                }
        }
    }

    fn render_search_bar(&mut self, ui: &crate::AppWindow, kind: &Kind) {
        let radio_url = *kind == Kind::Radio && self.stack.last().is_some_and(|s| s.url_mode);
        let shown = *kind == Kind::Search || radio_url;
        ui.set_search_shown(shown);
        if !shown {
            return;
        }
        let Some(screen) = self.stack.last() else { return };
        ui.set_search_text(screen.query.as_str().into());
        if radio_url {
            ui.set_search_placeholder("http://\u{2026}".into());
            ui.set_search_summary("The speaker fetches it itself, so it has to be reachable from the speaker. It plays under its host's name.".into());
            ui.set_search_unanswered(SharedString::new());
            return;
        }
        ui.set_search_placeholder("Songs, albums, artists, stations".into());
        let (summary, unanswered) = match &screen.searched {
            None => (String::new(), String::new()),
            Some(_) => {
                let answered: Vec<&Section> = screen.sections.iter().filter(|s| matches!(&s.result, Ok(r) if !r.is_empty())).collect();
                let results: usize = answered.iter().map(|s| s.result.as_ref().map(|r| r.len()).unwrap_or(0)).sum();
                let services = answered.len();
                // Only one that ran out of time "didn't answer"; one that refused
                // is named under the results instead.
                let failed = screen.sections.iter().filter(|s| matches!(&s.result, Err(e) if e == "didn't answer")).count();
                let summary = if screen.searching && results == 0 {
                    "Searching\u{2026}".to_owned()
                } else {
                    format!(
                        "{results} result{} from {services} service{}{}",
                        if results == 1 { "" } else { "s" },
                        if services == 1 { "" } else { "s" },
                        if screen.searching { " so far" } else { "" }
                    )
                };
                let unanswered = match failed {
                    0 => String::new(),
                    1 => "1 service didn't answer".to_owned(),
                    n => format!("{n} services didn't answer"),
                };
                (summary, unanswered)
            }
        };
        ui.set_search_summary(summary.into());
        ui.set_search_unanswered(unanswered.into());
    }

    /// Search for what is typed, across every service, remembering the term.
    pub(super) fn submit_search(&mut self) {
        let term = match self.stack.last() {
            Some(screen) => screen.query.trim().to_owned(),
            None => return,
        };
        if term.is_empty() {
            return;
        }
        let id = self.next_request_id();
        if let Some(screen) = self.stack.last_mut() {
            screen.requests.search = Some(id);
            screen.searched = Some(term.clone());
            screen.sections.clear();
            screen.searching = true;
        }
        self.keyboard = None;
        self.prefs.recent_searches.retain(|t| !t.eq_ignore_ascii_case(&term));
        self.prefs.recent_searches.insert(0, term.clone());
        self.prefs.recent_searches.truncate(8);
        self.prefs.save();
        self.send(Command::Fetch(Fetch::Search { id: Some(id), term }));
        self.set_focus(zone::LIST, 0);
    }
}

/// An alarm's days in words.
pub fn recurrence_words(recurrence: &str) -> String {
    match recurrence {
        "ONCE" => "Once".into(),
        "DAILY" => "Every day".into(),
        "WEEKDAYS" => "Weekdays".into(),
        "WEEKENDS" => "Weekends".into(),
        other => match other.strip_prefix("ON_") {
            Some(days) => {
                const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
                days.chars().filter_map(|d| d.to_digit(10)).filter_map(|d| NAMES.get(d as usize).copied()).collect::<Vec<_>>().join(" ")
            }
            None => other.to_owned(),
        },
    }
}

/// What an alarm plays: the chime, or the favourite's title from its metadata.
pub fn alarm_sound(alarm: &x2rock::sonos::upnp::Alarm) -> String {
    if alarm.program() == "chime" {
        return "Chime".into();
    }
    let meta = &alarm.program_metadata;
    meta.find("<dc:title>")
        .and_then(|a| meta[a + 10..].find("</dc:title>").map(|b| meta[a + 10..a + 10 + b].to_owned()))
        .map(|t| t.replace("&amp;", "&"))
        .unwrap_or_else(|| "Music".into())
}

impl App {
    /// Play the typed address as a station, named by its host.
    pub(super) fn play_url(&mut self) {
        let Some(screen) = self.stack.last() else { return };
        let group = screen.group.clone();
        let Some(url) = crate::model::stream_url(&screen.query) else {
            self.notice("That isn't an http or https address");
            return;
        };
        let title = crate::model::stream_name(&url);
        self.keyboard = None;
        self.send(Command::PlayStream { group, url, title: title.clone() });
        self.notice(&format!("Starting {title}"));
        self.played();
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn focus_follows_its_row_when_rows_move_and_stands_when_it_goes() {
        let old = vec![RowAction::QueueTrack(1), RowAction::Recent("a".into()), RowAction::Recent("b".into())];
        let moved = vec![RowAction::Recent("x".into()), RowAction::QueueTrack(1), RowAction::Recent("a".into()), RowAction::Recent("b".into())];
        assert_eq!(follow_row(&old, &moved, 2), Some(3), "rows arriving above move it down");
        assert_eq!(follow_row(&old, &old, 1), Some(1));
        let gone = vec![RowAction::QueueTrack(1), RowAction::Recent("b".into())];
        assert_eq!(follow_row(&old, &gone, 1), None, "a row that went leaves the index to stand");
        assert_eq!(follow_row(&[], &old, 0), None);
    }

    use super::*;

    #[test]
    fn days_are_worded() {
        assert_eq!(recurrence_words("WEEKDAYS"), "Weekdays");
        assert_eq!(recurrence_words("ON_135"), "Mon Wed Fri");
        assert_eq!(recurrence_words("DAILY"), "Every day");
    }

    #[test]
    fn favourites_group_as_the_sonos_app_does() {
        let fav = |kind: Option<&str>| -> Favorite {
            serde_json::from_value(serde_json::json!({
                "id": "1", "name": "x",
                "resource": kind.map(|k| serde_json::json!({ "type": k })),
            }))
            .unwrap()
        };
        let items = vec![fav(Some("TRACK")), fav(Some("PLAYLIST")), fav(None), fav(Some("STREAM"))];
        let groups: Vec<&str> = favorite_groups(&items).into_iter().map(|(t, _)| t).collect();
        assert_eq!(groups, ["Favorite playlists", "Favorite songs", "Favorite stations", "Favorites"]);
    }
}
