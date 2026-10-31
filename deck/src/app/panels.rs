//! The two panels over the home screen: Settings, down the right, and the
//! room panel, centred, in its two halves, Group and Configure (RoomPanel.kt).
//! Rust lists the rows each time anything changes, so a room that leaves a
//! group leaves its list at once and focus stays at the same place in it.

use std::time::Instant;

use slint::{ComponentHandle, SharedString};

use super::{App, RoomEntry, zone};
use crate::household::{Command, RoomDetails};
use crate::model::{self, RoomInfo};
use crate::theme;
use crate::{Icons, Overlay, PanelItem, PanelKind};

/// What a focusable row does.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelAction {
    Theme,
    PreferredServices,
    ResetRoomOrder,
    Alarms,
    About,
    ToggleBeta,
    Report,
    Party,
    Split,
    Leave(String),
    Join(String),
    SavePreset,
    TvInput(String),
    /// A speaker's line-in, by its player id.
    LineIn(String),
    CancelSleep,
    Sleep(u32),
    Bass,
    Treble,
    Loudness,
    Trueplay,
    Crossfade,
    StatusLight,
    TouchControls,
    RemoteFeedback,
    IrRepeater,
    SubOn,
    SurroundsOn,
    /// A Sub or surround level: the EQ type it writes.
    Satellite(&'static str),
    ThisIsMyTv(String),
    // The item menu's, on the row it was opened from.
    MenuPlay,
    MenuOpen,
    MenuPlayNext,
    MenuAddToQueue,
    MenuRemoveFromQueue,
    MenuForget,
    MenuDeletePreset,
    MenuAddFavorite,
    /// A book's chapters, as a page of their own.
    MenuChapters,
    /// Two presses: the first arms it.
    MenuRemoveFavorite,
    ToggleUpdateCheck,
    ImportSignIns,
    /// The exact-volume chooser's levels.
    Level(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKindOpen {
    Settings,
    Group,
    Configure,
    /// Menu, or a hold, on a list's row (ItemMenu.kt).
    ItemMenu,
    /// A hold, or Menu, on the volume: an exact level (SetVolumeOverlay).
    Volume,
}

pub struct OpenPanel {
    pub kind: PanelKindOpen,
    pub group: Option<String>,
    /// What each focus index does, and where it sits: (row, column).
    pub actions: Vec<PanelAction>,
    pub cells: Vec<(usize, usize)>,
    /// The control that opened it, which focus goes back to.
    pub opener: (i32, i32),
    /// The item menu's row, and its title.
    pub row: Option<super::screens::RowAction>,
    pub title: String,
    /// The item menu's item: whether it is a Sonos favourite, once asked.
    pub favorite: Option<bool>,
    /// Remove from Sonos Favorites pressed once: the next press removes.
    pub remove_armed: bool,
}

pub const SLEEP_CHOICES: [u32; 6] = [15, 30, 45, 60, 90, 120];

pub fn sleep_choice_label(minutes: u32) -> String {
    match minutes {
        m if m < 60 => format!("{m} minutes"),
        60 => "1 hour".into(),
        m if m % 60 == 0 => format!("{} hours", m / 60),
        m => format!("{}\u{00BD} hours", m / 60),
    }
}

/// A clock time as the TV shows one: "9:41 PM".
pub fn clock_time(at: chrono::DateTime<chrono::Local>) -> String {
    at.format("%-I:%M %p").to_string()
}

/// Rows, built up with their focus indices.
#[derive(Default)]
struct Rows {
    items: Vec<PanelItem>,
    actions: Vec<PanelAction>,
    cells: Vec<(usize, usize)>,
    row: usize,
}

impl Rows {
    fn plain(&mut self, kind: PanelKind, label: &str) {
        self.items.push(PanelItem {
            kind,
            label: label.into(),
            focus: -1,
            focus2: -1,
            ..Default::default()
        });
    }

    fn focusable(&mut self, mut item: PanelItem, action: PanelAction) {
        item.focus = self.actions.len() as i32;
        item.focus2 = -1;
        self.actions.push(action);
        self.cells.push((self.row, 0));
        self.row += 1;
        self.items.push(item);
    }

    fn button(&mut self, label: &str, detail: &str, action: PanelAction) {
        self.focusable(
            PanelItem {
                kind: PanelKind::Button,
                label: label.into(),
                detail: detail.into(),
                ..Default::default()
            },
            action,
        );
    }

    fn pair(&mut self, left: (&str, PanelAction), right: Option<(&str, PanelAction)>) {
        let focus = self.actions.len() as i32;
        self.actions.push(left.1);
        self.cells.push((self.row, 0));
        let mut item = PanelItem {
            kind: PanelKind::Pair,
            label: left.0.into(),
            focus,
            focus2: -1,
            ..Default::default()
        };
        if let Some((label, action)) = right {
            item.focus2 = self.actions.len() as i32;
            item.label2 = label.into();
            self.actions.push(action);
            self.cells.push((self.row, 1));
        }
        self.row += 1;
        self.items.push(item);
    }
}

fn on_off(label: &str, on: bool) -> String {
    format!("{label}   {}", if on { "On" } else { "Off" })
}

fn signed(level: i8) -> String {
    if level > 0 {
        format!("+{level}")
    } else {
        level.to_string()
    }
}

impl App {
    pub(super) fn open_settings(&mut self) {
        let opener = self.focus;
        self.panel = Some(OpenPanel {
            kind: PanelKindOpen::Settings,
            group: None,
            actions: vec![],
            cells: vec![],
            opener,
            row: None,
            title: String::new(),
            favorite: None,
            remove_armed: false,
        });
        self.set_focus(zone::SETTINGS_PANEL, 0);
    }

    pub(super) fn open_room_panel(&mut self, configure: bool) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let opener = self.focus;
        self.panel = Some(OpenPanel {
            kind: if configure {
                PanelKindOpen::Configure
            } else {
                PanelKindOpen::Group
            },
            group: Some(entry.group_id.clone()),
            actions: vec![],
            cells: vec![],
            opener,
            row: None,
            title: String::new(),
            favorite: None,
            remove_armed: false,
        });
        if configure {
            self.room_details = None;
            if self.handle.is_none() {
                // The demo: no household to ask, so its own made-up settings.
                self.room_details = Some(crate::demo::room_details(
                    &entry.group_id,
                    &entry.coordinator,
                ));
            }
            self.send(Command::ReadRoom {
                group: entry.group_id.clone(),
                player: entry.coordinator.clone(),
            });
        }
        self.render_panel();
        // The Group half opens on the room's own row, the Configure half on its first.
        let start = match configure {
            false => self
                .panel
                .as_ref()
                .and_then(|p| {
                    p.actions.iter().position(
                        |a| matches!(a, PanelAction::Leave(id) if *id == entry.coordinator),
                    )
                })
                .unwrap_or(0),
            true => 0,
        };
        self.set_focus(zone::ROOM_PANEL, start as i32);
    }

    /// The item menu for a list's row: what can be done with it here.
    pub(super) fn open_item_menu(&mut self, row: super::screens::RowAction, title: String) {
        let group = self.stack.last().map(|s| s.group.clone());
        let opener = self.focus;
        // Whether a song, an album or a playlist is a Sonos favourite already
        // decides which of Add and Remove the menu offers: asked as it opens.
        if let super::screens::RowAction::ServiceItem { service, item } = &row
            && favoritable(item)
        {
            self.send(Command::Fetch(crate::household::Fetch::IsFavorite {
                service: service.clone(),
                item: item.clone(),
            }));
        }
        self.panel = Some(OpenPanel {
            kind: PanelKindOpen::ItemMenu,
            group,
            actions: vec![],
            cells: vec![],
            opener,
            row: Some(row),
            title,
            favorite: None,
            remove_armed: false,
        });
        self.render_panel();
        self.set_focus(zone::ROOM_PANEL, 0);
    }

    /// An exact level for the room, picked from the levels most often wanted.
    pub(super) fn open_volume_chooser(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        if self.live(&entry.group_id).volume.is_some_and(|v| v.fixed) {
            return;
        }
        let opener = self.focus;
        self.panel = Some(OpenPanel {
            kind: PanelKindOpen::Volume,
            group: Some(entry.group_id.clone()),
            actions: vec![],
            cells: vec![],
            opener,
            row: None,
            title: String::new(),
            favorite: None,
            remove_armed: false,
        });
        self.render_panel();
        self.set_focus(zone::ROOM_PANEL, 0);
    }

    pub(super) fn close_panel(&mut self) {
        if let Some(panel) = self.panel.take() {
            self.focus = panel.opener;
            self.clamp_focus();
        }
    }

    pub(super) fn panel_open(&self) -> bool {
        self.panel.is_some()
    }

    pub(super) fn room_details_arrived(&mut self, details: RoomDetails) {
        let wanted = self.panel.as_ref().and_then(|p| p.group.as_ref()) == Some(&details.group);
        if wanted {
            self.room_details = Some(details);
        }
    }

    /// Up, Down, Left and Right inside a panel.
    pub(super) fn navigate_panel(&mut self, action: crate::input::Action) {
        use crate::input::Action;
        let Some(panel) = self.panel.as_ref() else {
            return;
        };
        let i = self.focus.1.max(0) as usize;
        let Some(&(row, col)) = panel.cells.get(i) else {
            return;
        };
        let current = panel.actions.get(i).cloned();
        match action {
            Action::Left | Action::Right => {
                let d = if action == Action::Right { 1 } else { -1 };
                match current {
                    Some(
                        PanelAction::Theme
                        | PanelAction::Bass
                        | PanelAction::Treble
                        | PanelAction::Satellite(_),
                    ) => self.panel_step(i as i32, d),
                    _ => {
                        let target = panel
                            .cells
                            .iter()
                            .position(|&c| c == (row, (col as i32 + d).max(0) as usize));
                        if let Some(t) = target {
                            self.focus.1 = t as i32;
                        }
                    }
                }
            }
            Action::Up | Action::Down => {
                let target_row = if action == Action::Down {
                    row + 1
                } else if row > 0 {
                    row - 1
                } else {
                    return;
                };
                let in_row: Vec<usize> = panel
                    .cells
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.0 == target_row)
                    .map(|(k, _)| k)
                    .collect();
                if in_row.is_empty() {
                    return;
                }
                let pick = in_row
                    .iter()
                    .copied()
                    .find(|&k| panel.cells[k].1 == col)
                    .unwrap_or(in_row[in_row.len() - 1].min(*in_row.last().unwrap()));
                let pick = if panel.cells[pick].1 > col {
                    in_row[0]
                } else {
                    pick
                };
                self.focus.1 = pick as i32;
            }
            _ => {}
        }
    }

    /// Left or Right on a row that steps a value, by key or by its marks.
    pub fn panel_step(&mut self, index: i32, direction: i32) {
        let Some(action) = self
            .panel
            .as_ref()
            .and_then(|p| p.actions.get(index.max(0) as usize).cloned())
        else {
            return;
        };
        match action {
            PanelAction::Theme => {
                let themes = theme::THEMES;
                let at = themes
                    .iter()
                    .position(|t| *t == self.prefs.theme)
                    .unwrap_or(0) as i32;
                let next = (at + direction).rem_euclid(themes.len() as i32) as usize;
                self.prefs.theme = themes[next].to_owned();
                self.prefs.save();
            }
            PanelAction::Bass | PanelAction::Treble => {
                let Some(details) = self.room_details.as_mut() else {
                    return;
                };
                let Some(tone) = details.tone.as_mut() else {
                    return;
                };
                let player = details.player.clone();
                let bass = action == PanelAction::Bass;
                let level = if bass {
                    &mut tone.bass
                } else {
                    &mut tone.treble
                };
                let next = (*level as i32 + direction).clamp(-10, 10) as i8;
                if next == *level {
                    return;
                }
                *level = next;
                self.send(if bass {
                    Command::SetBass {
                        player,
                        level: next,
                    }
                } else {
                    Command::SetTreble {
                        player,
                        level: next,
                    }
                });
            }
            PanelAction::Satellite(eq_type) => {
                let Some(details) = self.room_details.as_mut() else {
                    return;
                };
                let Some(s) = details.satellites.as_mut() else {
                    return;
                };
                let player = s.player.clone();
                let level = match eq_type {
                    "SubGain" => &mut s.sub_level,
                    "SurroundLevel" => &mut s.tv_level,
                    _ => &mut s.music_level,
                };
                let Some(level) = level.as_mut() else { return };
                let next = (*level + direction).clamp(-15, 15);
                if next == *level {
                    return;
                }
                *level = next;
                self.send(Command::SetEqValue {
                    player,
                    eq_type,
                    value: next,
                });
            }
            _ => {}
        }
        self.render();
    }

    /// Select on a panel row.
    pub(super) fn panel_click(&mut self, index: i32) {
        let Some(panel) = self.panel.as_ref() else {
            return;
        };
        let Some(action) = panel.actions.get(index.max(0) as usize).cloned() else {
            return;
        };
        let group = panel.group.clone();
        let entry = group
            .as_ref()
            .and_then(|g| self.rooms.iter().find(|r| &r.group_id == g).cloned());
        match action {
            PanelAction::Theme
            | PanelAction::Bass
            | PanelAction::Treble
            | PanelAction::Satellite(_) => {}
            PanelAction::RemoteFeedback | PanelAction::IrRepeater => {
                let Some(details) = self.room_details.as_mut() else {
                    return;
                };
                let Some((soundbar, remote)) = details.remote.as_mut() else {
                    return;
                };
                let command = if action == PanelAction::RemoteFeedback {
                    remote.feedback = !remote.feedback;
                    Command::SetRemoteFeedback {
                        player: soundbar.clone(),
                        on: remote.feedback,
                    }
                } else {
                    // Only between On and Off: another word is said, not toggled.
                    let on = match remote.repeater.to_ascii_lowercase().as_str() {
                        "on" => false,
                        "off" => true,
                        _ => return,
                    };
                    remote.repeater = if on { "On".into() } else { "Off".into() };
                    Command::SetIrRepeater {
                        player: soundbar.clone(),
                        on,
                    }
                };
                self.send(command);
            }
            PanelAction::SubOn | PanelAction::SurroundsOn => {
                let Some(details) = self.room_details.as_mut() else {
                    return;
                };
                let Some(s) = details.satellites.as_mut() else {
                    return;
                };
                let player = s.player.clone();
                let (flag, eq_type) = if action == PanelAction::SubOn {
                    (&mut s.sub_on, "SubEnable")
                } else {
                    (&mut s.surrounds_on, "SurroundEnable")
                };
                let Some(on) = flag.as_mut() else { return };
                *on = !*on;
                let value = i32::from(*on);
                self.send(Command::SetEqValue {
                    player,
                    eq_type,
                    value,
                });
            }
            PanelAction::PreferredServices => {
                self.close_panel();
                self.open_household_screen(super::screens::Kind::PreferredServices);
            }
            PanelAction::ResetRoomOrder => {
                self.prefs.room_ranks.clear();
                self.prefs.save();
                self.rooms = self.sorted_rooms();
                // It leaves on its own press: focus goes to the row above.
                self.focus.1 = (index - 1).max(0);
            }
            PanelAction::Alarms => {
                self.close_panel();
                self.open_household_screen(super::screens::Kind::Alarms);
            }
            PanelAction::About => {
                self.close_panel();
                self.open_household_screen(super::screens::Kind::About);
            }
            PanelAction::ToggleBeta => {
                self.prefs.beta_features = !self.prefs.beta_features;
                self.prefs.save();
            }
            PanelAction::Report => {
                self.close_panel();
                self.open_household_screen(super::screens::Kind::Report);
            }
            PanelAction::ImportSignIns => {
                self.send(Command::ImportSignIns);
                self.notice("Reading the speakers' sign-ins\u{2026}");
            }
            PanelAction::ToggleUpdateCheck => {
                let on = self.prefs.check_for_updates != Some(false);
                self.prefs.check_for_updates = Some(!on);
                self.prefs.save();
            }
            PanelAction::Party => {
                if let Some(e) = entry {
                    self.send(Command::PlayEverywhere {
                        group: e.group_id,
                        level: None,
                    });
                }
            }
            PanelAction::Split => self.send(Command::SplitAll),
            PanelAction::Leave(player) => {
                // The coordinator *is* the group: its row is drawn, never acted on.
                if let Some(e) = entry.filter(|e| e.coordinator != player) {
                    self.send(Command::ModifyGroup {
                        group: e.group_id,
                        add: vec![],
                        remove: vec![player],
                    });
                }
            }
            PanelAction::Join(other) => {
                if let (Some(e), Some(o)) = (
                    entry,
                    self.rooms.iter().find(|r| r.group_id == other).cloned(),
                ) {
                    self.send(Command::ModifyGroup {
                        group: e.group_id,
                        add: o.players,
                        remove: vec![],
                    });
                }
            }
            PanelAction::SavePreset => {
                if let Some(e) = entry {
                    self.save_preset(&e);
                }
            }
            PanelAction::LineIn(player) => {
                if let Some(e) = entry {
                    self.send(Command::UseLineIn {
                        group: e.group_id,
                        player,
                    });
                }
            }
            PanelAction::TvInput(soundbar) => {
                if let Some(e) = entry {
                    self.send(Command::UseTvInput {
                        group: e.group_id,
                        soundbar,
                    });
                }
            }
            PanelAction::CancelSleep => {
                if let Some(e) = entry {
                    self.send(Command::SetSleep {
                        group: e.group_id,
                        minutes: None,
                    });
                    if let Some(d) = self.room_details.as_mut() {
                        d.sleep_left = None;
                    }
                }
            }
            PanelAction::Sleep(minutes) => {
                if let Some(e) = entry {
                    self.send(Command::SetSleep {
                        group: e.group_id,
                        minutes: Some(minutes),
                    });
                    if let Some(d) = self.room_details.as_mut() {
                        d.sleep_left =
                            Some(std::time::Duration::from_secs(u64::from(minutes) * 60));
                        d.read_at = Some(Instant::now());
                    }
                    self.notice(&format!("Sleeping in {}", sleep_choice_label(minutes)));
                }
            }
            PanelAction::Loudness | PanelAction::Trueplay => {
                let Some(details) = self.room_details.as_mut() else {
                    return;
                };
                let player = details.player.clone();
                let Some(tone) = details.tone.as_mut() else {
                    return;
                };
                let command = if action == PanelAction::Loudness {
                    tone.loudness = !tone.loudness;
                    Command::SetLoudness {
                        player,
                        on: tone.loudness,
                    }
                } else {
                    tone.trueplay = !tone.trueplay;
                    Command::SetTrueplay {
                        player,
                        on: tone.trueplay,
                    }
                };
                self.send(command);
            }
            PanelAction::Crossfade => {
                if let Some(e) = entry {
                    let on = self.live(&e.group_id).modes.crossfade;
                    self.send(Command::SetCrossfade {
                        group: e.group_id,
                        on: !on,
                    });
                }
            }
            PanelAction::StatusLight => {
                let Some(details) = self.room_details.as_mut() else {
                    return;
                };
                if let Some(on) = details.status_light.as_mut() {
                    *on = !*on;
                    let (player, on) = (details.player.clone(), *on);
                    self.send(Command::SetStatusLight { player, on });
                }
            }
            PanelAction::TouchControls => {
                let Some(details) = self.room_details.as_mut() else {
                    return;
                };
                if let Some(locked) = details.buttons_locked.as_mut() {
                    *locked = !*locked;
                    let (player, locked) = (details.player.clone(), *locked);
                    self.send(Command::SetButtonsLocked { player, locked });
                }
            }
            PanelAction::MenuPlay
            | PanelAction::MenuOpen
            | PanelAction::MenuPlayNext
            | PanelAction::MenuAddToQueue
            | PanelAction::MenuRemoveFromQueue
            | PanelAction::MenuDeletePreset
            | PanelAction::MenuAddFavorite
            | PanelAction::MenuChapters
            | PanelAction::MenuRemoveFavorite
            | PanelAction::MenuForget => {
                // Every controller's list: removing takes a second press.
                if action == PanelAction::MenuRemoveFavorite
                    && let Some(panel) = self.panel.as_mut().filter(|p| !p.remove_armed)
                {
                    panel.remove_armed = true;
                    return;
                }
                let row = self.panel.as_ref().and_then(|p| p.row.clone());
                self.close_panel();
                if let Some(row) = row {
                    self.menu_chosen(action, row);
                }
            }
            PanelAction::Level(level) => {
                if let Some(e) = entry {
                    self.send(Command::SetGroupVolume {
                        group: e.group_id,
                        level,
                    });
                }
                self.close_panel();
            }
            PanelAction::ThisIsMyTv(soundbar) => {
                // King of the hill: naming a room moves the crown off whichever held it.
                self.prefs.tv_player_id = Some(soundbar);
                self.prefs.save();
                self.notice("This is now this screen's TV");
            }
        }
    }

    /// The open panel's rows, rebuilt from the household as it is now.
    pub(super) fn render_panel(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(kind) = self.panel.as_ref().map(|p| p.kind) else {
            ui.set_overlay(Overlay::None);
            return;
        };
        let (title, rows) = match kind {
            PanelKindOpen::Settings => ("Settings".to_owned(), self.settings_rows(&ui)),
            PanelKindOpen::ItemMenu => {
                let title = self
                    .panel
                    .as_ref()
                    .map(|p| p.title.clone())
                    .unwrap_or_default();
                let row = self.panel.as_ref().and_then(|p| p.row.clone());
                (title, self.menu_rows(row))
            }
            PanelKindOpen::Volume => {
                let group = self
                    .panel
                    .as_ref()
                    .and_then(|p| p.group.clone())
                    .unwrap_or_default();
                let room = self
                    .rooms
                    .iter()
                    .find(|r| r.group_id == group)
                    .map(|r| self.room_label(r))
                    .unwrap_or_default();
                let mut rows = Rows::default();
                if let Some(v) = self.live(&group).volume {
                    rows.plain(PanelKind::Note, &format!("Now {}", v.volume));
                }
                // The levels a room is most often set to: quiet to loud, short of loud.
                for pair in [5u8, 10, 15, 20, 25, 30, 40].chunks(2) {
                    let left = (pair[0].to_string(), PanelAction::Level(pair[0]));
                    let right = pair.get(1).map(|l| (l.to_string(), PanelAction::Level(*l)));
                    rows.pair(
                        (&left.0, left.1),
                        right.as_ref().map(|(l, a)| (l.as_str(), a.clone())),
                    );
                }
                (format!("Volume in {room}"), rows)
            }
            PanelKindOpen::Group | PanelKindOpen::Configure => {
                let group = self.panel.as_ref().and_then(|p| p.group.clone());
                let Some(entry) =
                    group.and_then(|g| self.rooms.iter().find(|r| r.group_id == g).cloned())
                else {
                    // The room it was opened on is gone: so is the panel.
                    self.close_panel();
                    ui.set_overlay(Overlay::None);
                    return;
                };
                let live = self.live(&entry.group_id);
                let info = RoomInfo::from_live(&live);
                let state = if info.on_tv_input {
                    info.source.clone().unwrap_or_else(|| "TV Audio".into())
                } else {
                    model::activity(live.state.as_deref(), info.has_source())
                        .label()
                        .to_owned()
                };
                let title = format!("{} \u{00B7} {}", self.room_label(&entry), state);
                let rows = if kind == PanelKindOpen::Group {
                    self.group_rows(&ui, &entry)
                } else {
                    self.configure_rows(&entry)
                };
                let _ = PanelKindOpen::ItemMenu;
                (title, rows)
            }
        };
        let focused = self.focus.1;
        if let Some(panel) = self.panel.as_mut() {
            // A row still there keeps focus as rows arrive or leave above it; one
            // that went hands it to the row taking its place, as the TV app does.
            let in_panel = matches!(self.focus.0, zone::ROOM_PANEL | zone::SETTINGS_PANEL);
            let held = panel
                .actions
                .get(focused.max(0) as usize)
                .cloned()
                .filter(|_| in_panel);
            if let Some(at) = held.and_then(|h| {
                (rows.actions.get(focused.max(0) as usize) != Some(&h))
                    .then(|| rows.actions.iter().position(|a| *a == h))
                    .flatten()
            }) {
                self.focus.1 = at as i32;
            }
            panel.actions = rows.actions;
            panel.cells = rows.cells;
        }
        ui.set_panel_title(title.into());
        self.models.panel_items.set(rows.items);
        ui.set_overlay(if kind == PanelKindOpen::Settings {
            Overlay::Settings
        } else {
            Overlay::RoomPanel
        });
        self.clamp_focus();
    }

    fn settings_rows(&self, _ui: &crate::AppWindow) -> Rows {
        let mut rows = Rows::default();
        rows.plain(PanelKind::Heading, "Theme");
        rows.focusable(
            PanelItem {
                kind: PanelKind::Theme,
                label: theme::display_name(&self.prefs.theme).into(),
                swatch: theme::swatch(&self.prefs.theme),
                ..Default::default()
            },
            PanelAction::Theme,
        );
        rows.plain(PanelKind::Heading, "Services");
        rows.button("Preferred services", "", PanelAction::PreferredServices);
        rows.button(
            "Read sign-ins from the speakers",
            "For services that need an account",
            PanelAction::ImportSignIns,
        );
        if !self.prefs.room_ranks.is_empty() {
            rows.button(
                "Reset room order",
                "Back to alphabetical",
                PanelAction::ResetRoomOrder,
            );
        }
        rows.plain(PanelKind::Heading, "Household");
        rows.button("Alarms", "", PanelAction::Alarms);
        rows.button("About this system", "", PanelAction::About);
        rows.plain(PanelKind::Heading, "This app");
        rows.button(
            &on_off(
                "Look for updates",
                self.prefs.check_for_updates != Some(false),
            ),
            &format!("x2rock {}", env!("CARGO_PKG_VERSION")),
            PanelAction::ToggleUpdateCheck,
        );
        rows.plain(
            PanelKind::Note,
            "GPL-3.0-only. Uses Slint (GPLv3), Roboto and Material Icons (Apache-2.0).",
        );
        rows.plain(PanelKind::Heading, "Beta");
        rows.button(
            &on_off("Beta features", self.prefs.beta_features),
            "",
            PanelAction::ToggleBeta,
        );
        rows.plain(
            PanelKind::Note,
            "Features not yet tried on real speakers, such as Line-In and a portable's battery. Each says Beta where it appears.",
        );
        if self.prefs.beta_features {
            rows.button("Send a report", "", PanelAction::Report);
        }
        rows
    }

    fn group_rows(&self, ui: &crate::AppWindow, entry: &RoomEntry) -> Rows {
        let mut rows = Rows::default();
        let icons = ui.global::<Icons>();
        let others: Vec<RoomEntry> = self
            .rooms
            .iter()
            .filter(|r| r.group_id != entry.group_id)
            .cloned()
            .collect();
        let partying = others.is_empty() && entry.players.len() > 1;
        if !others.is_empty() || partying {
            rows.plain(PanelKind::Section, "Everywhere");
            rows.focusable(
                PanelItem {
                    kind: PanelKind::Button,
                    label: if partying {
                        "Split every room apart"
                    } else {
                        "Play everywhere"
                    }
                    .into(),
                    icon: if partying {
                        icons.get_call_split()
                    } else {
                        icons.get_speaker_group()
                    },
                    has_icon: true,
                    ..Default::default()
                },
                if partying {
                    PanelAction::Split
                } else {
                    PanelAction::Party
                },
            );
        }
        rows.plain(PanelKind::Section, "Rooms in this group");
        // Coordinator first, so this is the room the panel is named after.
        let mut members = entry.players.clone();
        members.sort_by_key(|p| *p != entry.coordinator);
        for player in members {
            let coordinator = player == entry.coordinator;
            rows.focusable(
                PanelItem {
                    kind: PanelKind::Room,
                    label: self.player_name(&player).into(),
                    // The coordinator *is* the group: it has nothing to leave.
                    mark: if coordinator { 0 } else { 1 },
                    ..Default::default()
                },
                PanelAction::Leave(player),
            );
        }
        if !others.is_empty() {
            rows.plain(PanelKind::Section, "Rooms not in this group");
            for other in &others {
                let live = self.live(&other.group_id);
                let info = RoomInfo::from_live(&live);
                let line = model::room_row_lines(&info, live.state.as_deref())
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                rows.focusable(
                    PanelItem {
                        kind: PanelKind::Room,
                        label: self.room_label(other).into(),
                        detail: line.into(),
                        mark: 2,
                        ..Default::default()
                    },
                    PanelAction::Join(other.group_id.clone()),
                );
            }
        } else if entry.players.len() > 1 {
            rows.plain(PanelKind::Note, "Every room is in this group.");
        }
        rows.plain(PanelKind::Section, "Presets");
        rows.focusable(
            PanelItem {
                kind: PanelKind::Button,
                label: "Save as preset".into(),
                icon: icons.get_bookmark_add(),
                has_icon: true,
                ..Default::default()
            },
            PanelAction::SavePreset,
        );
        rows
    }

    /// Every speaker with a line-in, as Source rows: the room's own first, as
    /// "Line-In", then the others' as "Line-In on Den".
    fn line_ins(&self, entry: &RoomEntry) -> Vec<(String, String)> {
        let players = self
            .house
            .groups
            .as_ref()
            .map(|g| g.players.as_slice())
            .unwrap_or(&[]);
        let mut rows: Vec<(bool, String, String)> = players
            .iter()
            .filter(|p| p.capabilities.iter().any(|c| c == crate::report::LINE_IN))
            .map(|p| {
                let own = entry.players.contains(&p.id);
                (
                    !own,
                    p.id.clone(),
                    if own {
                        "Line-In".to_owned()
                    } else {
                        format!("Line-In on {}", p.name)
                    },
                )
            })
            .collect();
        rows.sort_by(|a, b| (a.0, &a.2).cmp(&(b.0, &b.2)));
        rows.into_iter().map(|(_, id, label)| (id, label)).collect()
    }

    fn configure_rows(&self, entry: &RoomEntry) -> Rows {
        let mut rows = Rows::default();
        let live = self.live(&entry.group_id);
        let info = RoomInfo::from_live(&live);
        let soundbar = self.has_tv(entry);
        let line_ins = if self.prefs.beta_features {
            self.line_ins(entry)
        } else {
            vec![]
        };
        if soundbar.is_some() || !line_ins.is_empty() {
            rows.plain(PanelKind::Section, "Source");
        }
        if let Some(bar) = &soundbar {
            rows.button(
                if info.on_tv_input {
                    "TV Input (current source)"
                } else {
                    "TV Input"
                },
                "",
                PanelAction::TvInput(bar.clone()),
            );
        }
        // Beta, and untested on hardware: what a room on its line-in reports has
        // never been seen, so these rows never say "current source". Another
        // room's input is offered too, as Sonos's own steps imply.
        for (player, label) in line_ins {
            rows.focusable(
                PanelItem {
                    kind: PanelKind::Button,
                    label: label.into(),
                    beta: true,
                    ..Default::default()
                },
                PanelAction::LineIn(player),
            );
        }
        let details = self
            .room_details
            .clone()
            .filter(|d| d.group == entry.group_id);
        let loading = details.is_none();
        if loading {
            rows.plain(PanelKind::Note, "Reading this room's settings\u{2026}");
        }
        if let Some(d) = &details {
            if d.sleep_read {
                rows.plain(PanelKind::Section, "Sleep timer");
                // The pushed timer where there is one; the panel's own read otherwise.
                let pushed = live.sleep_left(Instant::now());
                let read = d
                    .sleep_left
                    .zip(d.read_at)
                    .map(|(left, at)| left.saturating_sub(at.elapsed()));
                if let Some(left) = pushed.or(read).filter(|l| !l.is_zero()) {
                    let ends =
                        chrono::Local::now() + chrono::Duration::from_std(left).unwrap_or_default();
                    rows.button(
                        &format!("Cancel \u{00B7} stops at {}", clock_time(ends)),
                        "",
                        PanelAction::CancelSleep,
                    );
                }
                for pair in SLEEP_CHOICES.chunks(2) {
                    let left = (sleep_choice_label(pair[0]), PanelAction::Sleep(pair[0]));
                    let right = pair
                        .get(1)
                        .map(|m| (sleep_choice_label(*m), PanelAction::Sleep(*m)));
                    rows.pair(
                        (&left.0, left.1),
                        right.as_ref().map(|(l, a)| (l.as_str(), a.clone())),
                    );
                }
            }
            let crossfade = live.actions.can_crossfade.then_some(live.modes.crossfade);
            if d.tone.is_some() || crossfade.is_some() || d.satellites.is_some() {
                rows.plain(PanelKind::Section, "Sound");
            }
            if let Some(tone) = d.tone {
                for (label, level, action) in [
                    ("Bass", tone.bass, PanelAction::Bass),
                    ("Treble", tone.treble, PanelAction::Treble),
                ] {
                    rows.focusable(
                        PanelItem {
                            kind: PanelKind::Stepper,
                            label: label.into(),
                            value: signed(level).into(),
                            ..Default::default()
                        },
                        action,
                    );
                }
                rows.button(
                    &on_off("Loudness", tone.loudness),
                    "",
                    PanelAction::Loudness,
                );
                if tone.trueplay_available {
                    rows.button(
                        &on_off("TruePlay", tone.trueplay),
                        "",
                        PanelAction::Trueplay,
                    );
                }
            }
            if let Some(on) = crossfade {
                rows.button(&on_off("Crossfade", on), "", PanelAction::Crossfade);
            }
            // A Sub and surrounds, as the Sonos app's Sub audio and Surround audio.
            if let Some(s) = d.satellites.clone() {
                let level_row =
                    |rows: &mut Rows, label: &str, level: Option<i32>, eq_type: &'static str| {
                        if let Some(level) = level {
                            let value = signed(level.clamp(-128, 127) as i8);
                            rows.focusable(
                                PanelItem {
                                    kind: PanelKind::Stepper,
                                    label: label.into(),
                                    value: value.into(),
                                    ..Default::default()
                                },
                                PanelAction::Satellite(eq_type),
                            );
                        }
                    };
                if let Some(on) = s.sub_on {
                    rows.button(&on_off("Sub", on), "", PanelAction::SubOn);
                }
                level_row(&mut rows, "Sub level", s.sub_level, "SubGain");
                if let Some(on) = s.surrounds_on {
                    rows.button(&on_off("Surrounds", on), "", PanelAction::SurroundsOn);
                }
                level_row(&mut rows, "Surrounds for TV", s.tv_level, "SurroundLevel");
                level_row(
                    &mut rows,
                    "Surrounds for music",
                    s.music_level,
                    "MusicSurroundLevel",
                );
            }
            let battery = d.battery.as_ref().filter(|_| self.prefs.beta_features);
            if d.status_light.is_some()
                || d.buttons_locked.is_some()
                || d.volume_limit.is_some()
                || battery.is_some()
                || soundbar.is_some()
            {
                rows.plain(PanelKind::Section, "Room settings");
            }
            // A portable's pack, as a line: read, not set. Health only where it is
            // not the usual GREEN. Beta: never seen on a portable here.
            if let Some(pack) = battery {
                rows.plain(PanelKind::Note, &battery_line(pack));
            }
            // Said only where one is set: it explains a room that seems too quiet.
            if let Some(limit) = d.volume_limit {
                rows.plain(
                    PanelKind::Note,
                    &format!("Volume limit {limit}% \u{00B7} set in the Sonos app"),
                );
            }
            if let Some(on) = d.status_light {
                rows.button(&on_off("Status light", on), "", PanelAction::StatusLight);
            }
            if let Some(locked) = d.buttons_locked {
                // Worded as the Sonos app's Button Control: On means the buttons work.
                rows.button(
                    &on_off("Touch controls", !locked),
                    "",
                    PanelAction::TouchControls,
                );
            }
            // The soundbar's own TV-remote settings.
            if let Some((_, remote)) = &d.remote {
                rows.button(
                    &on_off("Remote feedback light", remote.feedback),
                    "",
                    PanelAction::RemoteFeedback,
                );
                match remote.repeater.to_ascii_lowercase().as_str() {
                    "on" | "off" => rows.button(
                        &on_off("IR repeater", remote.repeater.eq_ignore_ascii_case("on")),
                        "",
                        PanelAction::IrRepeater,
                    ),
                    // A third word the service documents and nobody here has seen.
                    _ => rows.plain(
                        PanelKind::Note,
                        &format!("IR repeater: {}", remote.repeater),
                    ),
                }
            }
        }
        if let Some(bar) = soundbar {
            if details.is_none() {
                rows.plain(PanelKind::Section, "Room settings");
            }
            rows.button("This is my TV", "", PanelAction::ThisIsMyTv(bar));
        }
        let _ = SharedString::new();
        rows
    }
}

impl App {
    fn menu_rows(&self, row: Option<super::screens::RowAction>) -> Rows {
        use super::screens::RowAction;
        let mut rows = Rows::default();
        let room = self
            .stack
            .last()
            .and_then(|s| self.rooms.iter().find(|r| r.group_id == s.group))
            .map(|r| self.room_label(r))
            .unwrap_or_default();
        let detail = |what: &str| {
            if room.is_empty() {
                String::new()
            } else {
                format!("{what} {room}")
            }
        };
        match row {
            Some(RowAction::ServiceItem { item, .. }) => {
                if item.opens() {
                    rows.button("Open", "", PanelAction::MenuOpen);
                } else {
                    rows.button("Play now", &detail("in"), PanelAction::MenuPlay);
                }
                // A book resumes where it was left; its chapters are a page away.
                if x2rock::play::is_resumable(&item.item_type) {
                    rows.button("Chapters", "", PanelAction::MenuChapters);
                }
                // A stream, or a radio program, is never queue material, and an
                // artist holds albums rather than tracks.
                let queue_material = !item.item_type.eq_ignore_ascii_case("stream")
                    && !x2rock::sonos::smapi::is_program(&item.item_type)
                    && !x2rock::bookmarks::container_of_containers(&item.item_type);
                if queue_material {
                    rows.plain(PanelKind::Section, "Queue");
                    rows.button("Play next", &detail("in"), PanelAction::MenuPlayNext);
                    rows.button(
                        "Add to end of queue",
                        &detail("in"),
                        PanelAction::MenuAddToQueue,
                    );
                }
                // Sonos's own favourites, once it is known whether this is one.
                match self
                    .panel
                    .as_ref()
                    .and_then(|p| p.favorite)
                    .filter(|_| favoritable(&item))
                {
                    Some(false) => {
                        rows.button("Add to Sonos Favorites", "", PanelAction::MenuAddFavorite)
                    }
                    Some(true) => {
                        rows.button(self.remove_label(), "", PanelAction::MenuRemoveFavorite)
                    }
                    None => {}
                }
            }
            Some(RowAction::QueueTrack(_)) => {
                rows.button("Play from here", &detail("in"), PanelAction::MenuPlay);
                rows.button("Remove from queue", "", PanelAction::MenuRemoveFromQueue);
            }
            Some(RowAction::Recent(_)) => {
                rows.button("Search again", "", PanelAction::MenuPlay);
                rows.button("Forget this search", "", PanelAction::MenuForget);
            }
            Some(RowAction::Preset(_)) => {
                rows.button("Start", "", PanelAction::MenuPlay);
                rows.button("Delete preset", "", PanelAction::MenuDeletePreset);
            }
            Some(RowAction::Favorite(..)) => {
                rows.button("Play", &detail("in"), PanelAction::MenuPlay);
                rows.button(self.remove_label(), "", PanelAction::MenuRemoveFavorite);
            }
            Some(RowAction::Playlist(..) | RowAction::Station(_) | RowAction::Replay(_)) => {
                rows.button("Play", &detail("in"), PanelAction::MenuPlay);
            }
            _ => {}
        }
        rows
    }
}

/// "Battery 80% · charging · Beta", as the TV app says a portable's pack.
fn battery_line(pack: &x2rock::sonos::upnp::Battery) -> String {
    let mut parts = vec![
        pack.level
            .map_or_else(|| "Battery".to_owned(), |l| format!("Battery {l}%")),
    ];
    match pack.charging() {
        Some(true) => parts.push("charging".into()),
        Some(false) => parts.push("on battery".into()),
        None => {}
    }
    if let Some(health) = pack
        .health
        .as_ref()
        .filter(|h| !h.eq_ignore_ascii_case("GREEN"))
    {
        parts.push(health.clone());
    }
    parts.push("Beta".into());
    parts.join(" \u{00B7} ")
}

/// A song, an album or a playlist: what a Sonos favourite saved from here can hold.
fn favoritable(item: &x2rock::sonos::smapi::Item) -> bool {
    item.item_type.eq_ignore_ascii_case("track")
        || x2rock::bookmarks::container_holds_tracks(&item.item_type)
}

impl App {
    fn remove_label(&self) -> &'static str {
        if self.panel.as_ref().is_some_and(|p| p.remove_armed) {
            "Press again to remove"
        } else {
            "Remove from Sonos Favorites"
        }
    }

    /// The menu's item answered: whether it is a Sonos favourite.
    pub(super) fn favorite_known(&mut self, item_id: &str, yes: bool) {
        if let Some(panel) = self.panel.as_mut() {
            let same = matches!(&panel.row, Some(super::screens::RowAction::ServiceItem { item, .. }) if item.id == item_id);
            if same {
                panel.favorite = Some(yes);
            }
        }
    }

    /// The room's speakers, each at its level, and the Sonos favourite playing
    /// if what plays is one, found by the container's name as Browse marks it.
    fn save_preset(&mut self, entry: &RoomEntry) {
        let mut players = entry.players.clone();
        players.sort_by_key(|p| *p != entry.coordinator);
        let volumes: std::collections::BTreeMap<String, u8> = players
            .iter()
            .filter_map(|p| {
                self.house
                    .player_volumes
                    .get(p)
                    .filter(|v| !v.fixed)
                    .map(|v| (p.clone(), v.volume))
            })
            .collect();
        let playing = RoomInfo::from_live(&self.live(&entry.group_id)).source;
        let favorite = playing
            .and_then(|name| self.favorites_cache.iter().find(|f| f.name == name))
            .cloned();
        let rooms = players
            .iter()
            .map(|p| self.player_name(p))
            .collect::<Vec<_>>()
            .join(" + ");
        let name = match &favorite {
            Some(f) => format!("{rooms} \u{00B7} {}", f.name),
            None => rooms,
        };
        let id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        self.prefs.presets.push(crate::prefs::Preset {
            id,
            name: name.clone(),
            player_ids: players,
            volumes,
            favorite_id: favorite.as_ref().map(|f| f.id.clone()),
        });
        self.prefs.save();
        self.notice(&if favorite.is_some() {
            format!("Saved \"{name}\" to Browse")
        } else {
            format!("Saved \"{name}\" to Browse \u{2014} rooms and levels only, since what plays isn't a favourite")
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batterys_line_says_what_the_pack_says() {
        let pack = x2rock::sonos::upnp::Battery {
            level: Some(80),
            health: Some("GREEN".into()),
            temperature: None,
            power_source: Some("USB_POWER".into()),
        };
        assert_eq!(
            battery_line(&pack),
            "Battery 80% \u{00B7} charging \u{00B7} Beta"
        );
        let worn = x2rock::sonos::upnp::Battery {
            level: None,
            health: Some("RED".into()),
            temperature: None,
            power_source: Some("BATTERY".into()),
        };
        assert_eq!(
            battery_line(&worn),
            "Battery \u{00B7} on battery \u{00B7} RED \u{00B7} Beta"
        );
    }

    #[test]
    fn sleep_lengths_are_worded_as_the_tv_app_words_them() {
        let labels: Vec<String> = SLEEP_CHOICES
            .iter()
            .map(|m| sleep_choice_label(*m))
            .collect();
        assert_eq!(
            labels,
            [
                "15 minutes",
                "30 minutes",
                "45 minutes",
                "1 hour",
                "1\u{00BD} hours",
                "2 hours"
            ]
        );
    }
}
