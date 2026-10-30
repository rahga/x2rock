//! Making and changing alarms on the Alarms screen, as the TV app's
//! AlarmsScreen does: a room, a time, how often and what it plays, one step to
//! a screen, then the summary with its volume. An alarm opened with Menu or a
//! hold opens on its summary, each change is its own step and comes back to it,
//! and Remove takes two presses. Times are the household's clock, never
//! converted.

use std::time::{Duration, Instant};

use super::screens::{RowAction, Rows};
use super::{App, zone};
use crate::ListCell;
use crate::household::{AlarmFields, AlarmProgram, AlarmWrite, Command, Fetch};

/// A step of the alarm being put together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Room,
    Time,
    Repeat,
    Sound,
    Confirm,
}

const STEPS: [Step; 5] = [Step::Room, Step::Time, Step::Repeat, Step::Sound, Step::Confirm];

/// How often, in the order the TV app offers it.
pub const RECURRENCES: [&str; 4] = ["DAILY", "WEEKDAYS", "WEEKENDS", "ONCE"];

/// What an alarm being changed offers to change, in the order a new one asks.
const CHANGES: [(Step, &str); 4] = [
    (Step::Room, "Change the room"),
    (Step::Time, "Change the time"),
    (Step::Repeat, "Change how often"),
    (Step::Sound, "Change what it plays"),
];

/// A second press of Remove within this removes the alarm.
const REMOVE_CONFIRM: Duration = Duration::from_secs(4);

/// The alarm being made, or with `editing` an existing one being changed.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub step: Step,
    /// The speaker, and its name.
    pub room: Option<(String, String)>,
    pub hour: u8,
    pub minute: u8,
    pub recurrence: String,
    /// `None` for the chime.
    pub program: Option<AlarmProgram>,
    pub volume: u8,
    /// The alarm's id, when this changes one rather than making one.
    pub editing: Option<u32>,
    /// When Remove was first pressed.
    pub remove_armed: Option<Instant>,
    /// A write is out: a second press says so rather than sending another.
    pub writing: bool,
}

impl Default for Draft {
    fn default() -> Self {
        Draft {
            step: Step::Room,
            room: None,
            hour: 7,
            minute: 0,
            recurrence: "DAILY".into(),
            program: None,
            volume: 20,
            editing: None,
            remove_armed: None,
            writing: false,
        }
    }
}

impl Draft {
    /// `HH:MM`, on the household's clock.
    pub fn time(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }

    /// One step back; `None` leaves the editor. Changing an alarm, a step goes
    /// back to the summary and the summary out.
    fn back(&self) -> Option<Draft> {
        let at = STEPS.iter().position(|s| *s == self.step).unwrap_or(0);
        match (self.editing, self.step) {
            (Some(_), Step::Confirm) => None,
            (Some(_), _) => Some(Draft { step: Step::Confirm, remove_armed: None, ..self.clone() }),
            (None, _) if at == 0 => None,
            (None, _) => Some(Draft { step: STEPS[at - 1], remove_armed: None, ..self.clone() }),
        }
    }

    /// Past a step chosen: the next one, or back to the summary when changing.
    fn advanced(mut self) -> Draft {
        self.step = if self.editing.is_some() {
            Step::Confirm
        } else {
            let at = STEPS.iter().position(|s| *s == self.step).unwrap_or(0);
            STEPS[(at + 1).min(STEPS.len() - 1)]
        };
        self
    }

    fn fields(&self) -> Option<AlarmFields> {
        Some(AlarmFields {
            start: format!("{}:00", self.time()),
            recurrence: self.recurrence.clone(),
            room: self.room.as_ref()?.0.clone(),
            program: self.program.clone(),
            volume: self.volume,
        })
    }
}

/// What a row of the editor does.
#[derive(Clone, Debug, PartialEq)]
pub enum AlarmRow {
    New,
    Room(String, String),
    Hour,
    Minute,
    Repeat(String),
    Sound(Option<AlarmProgram>),
    Volume,
    Make,
    Save,
    Change(Step),
    Remove,
}

impl App {
    fn draft_mut(&mut self) -> Option<&mut Draft> {
        self.stack.last_mut().and_then(|s| s.alarm_draft.as_mut())
    }

    pub(super) fn alarm_draft(&self) -> Option<&Draft> {
        self.stack.last().and_then(|s| s.alarm_draft.as_ref())
    }

    /// The editor's title: "New alarm" or "Change alarm".
    pub(super) fn alarm_title(&self) -> Option<&'static str> {
        self.alarm_draft().map(|d| if d.editing.is_some() { "Change alarm" } else { "New alarm" })
    }

    /// Open alarm `alarm` to change it, on its summary, as it was read.
    pub(super) fn start_alarm_edit(&mut self, alarm: &x2rock::sonos::upnp::Alarm) {
        let mut parts = alarm.start.split(':').map(|p| p.parse::<u8>().unwrap_or(0));
        let (hour, minute) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        let program = (!alarm.program_uri.starts_with("x-rincon-buzzer")).then(|| AlarmProgram {
            title: super::screens::alarm_sound(alarm),
            uri: alarm.program_uri.clone(),
            metadata: alarm.program_metadata.clone(),
        });
        let draft = Draft {
            step: Step::Confirm,
            room: Some((alarm.room_uuid.clone(), self.player_name(&alarm.room_uuid))),
            hour,
            minute,
            recurrence: alarm.recurrence.clone(),
            program,
            volume: alarm.volume,
            editing: Some(alarm.id),
            ..Draft::default()
        };
        self.open_draft(draft);
    }

    fn open_draft(&mut self, draft: Draft) {
        let wants_sounds = draft.step == Step::Sound;
        if let Some(s) = self.stack.last_mut() {
            s.alarm_draft = Some(draft);
        }
        if wants_sounds {
            self.read_alarm_sounds();
        }
        self.focus = (zone::LIST, 0);
        self.render_screen();
        self.focus = (zone::LIST, self.alarm_first_focus());
    }

    /// The summary opens on Make or Save, past the volume; every other step on
    /// its first choice.
    fn alarm_first_focus(&self) -> i32 {
        let Some(screen) = self.stack.last() else { return 0 };
        screen
            .actions
            .iter()
            .position(|a| matches!(a, RowAction::AlarmEditor(AlarmRow::Make | AlarmRow::Save)))
            .unwrap_or(0) as i32
    }

    /// Asked again while there are none, so one failed read does not leave
    /// only the chime for good.
    pub(super) fn read_alarm_sounds(&mut self) {
        let have = self.stack.last().is_some_and(|s| matches!(&s.alarm_sounds, Some(Ok(list)) if !list.is_empty()));
        if !have {
            let id = self.next_request_id();
            let Some(s) = self.stack.last_mut() else { return };
            s.alarm_sounds = None;
            s.requests.alarm_sounds = Some(id);
            self.send(Command::Fetch(Fetch::AlarmSounds { id: Some(id) }));
        }
    }

    /// Back inside the editor: true when it took the press.
    pub(super) fn alarm_back(&mut self) -> bool {
        let Some(draft) = self.alarm_draft().cloned() else { return false };
        let back = draft.back();
        if let Some(s) = self.stack.last_mut() {
            s.alarm_draft = back.clone();
        }
        self.focus = (zone::LIST, 0);
        self.render_screen();
        if back.is_some() {
            self.focus = (zone::LIST, self.alarm_first_focus());
        }
        true
    }

    /// Up and Down on the hour or minute, Left and Right on the volume: true
    /// when the row took the key.
    pub(super) fn alarm_key(&mut self, row: &RowAction, action: crate::input::Action) -> bool {
        use crate::input::Action;
        let RowAction::AlarmEditor(row) = row else { return false };
        let Some(d) = self.draft_mut() else { return false };
        match (row, action) {
            (AlarmRow::Hour, Action::Up | Action::Down) => {
                let step: i32 = if action == Action::Up { 1 } else { -1 };
                d.hour = (i32::from(d.hour) + step).rem_euclid(24) as u8;
            }
            (AlarmRow::Minute, Action::Up | Action::Down) => {
                let step: i32 = if action == Action::Up { 1 } else { -1 };
                d.minute = (i32::from(d.minute) + step).rem_euclid(60) as u8;
            }
            (AlarmRow::Volume, Action::Left | Action::Right) => {
                let step: i32 = if action == Action::Right { 1 } else { -1 };
                d.volume = (i32::from(d.volume) + step).clamp(0, 100) as u8;
            }
            _ => return false,
        }
        true
    }

    /// Select on a row of the editor, or on New alarm.
    pub(super) fn alarm_click(&mut self, row: AlarmRow) {
        if row == AlarmRow::New {
            self.open_draft(Draft::default());
            return;
        }
        let Some(draft) = self.alarm_draft().cloned() else { return };
        let was = draft.step;
        let next = match row {
            AlarmRow::New | AlarmRow::Volume => return,
            AlarmRow::Room(id, name) => Draft { room: Some((id, name)), ..draft }.advanced(),
            AlarmRow::Hour | AlarmRow::Minute => draft.advanced(),
            AlarmRow::Repeat(recurrence) => Draft { recurrence, ..draft }.advanced(),
            AlarmRow::Sound(program) => Draft { program, ..draft }.advanced(),
            AlarmRow::Change(step) => Draft { step, remove_armed: None, ..draft },
            AlarmRow::Make | AlarmRow::Save => {
                if draft.writing {
                    self.notice("Saving\u{2026}");
                    return;
                }
                let Some(fields) = draft.fields() else { return };
                let write = match draft.editing {
                    Some(id) => AlarmWrite::Change { id, fields },
                    None => AlarmWrite::Create(fields),
                };
                self.send(Command::WriteAlarm(write));
                Draft { writing: true, ..draft }
            }
            AlarmRow::Remove => {
                let Some(id) = draft.editing else { return };
                match draft.remove_armed {
                    Some(at) if at.elapsed() < REMOVE_CONFIRM => {
                        self.send(Command::WriteAlarm(AlarmWrite::Remove(id)));
                        Draft { writing: true, remove_armed: None, ..draft }
                    }
                    _ => Draft { remove_armed: Some(Instant::now()), ..draft },
                }
            }
        };
        let moved = next.step != was;
        self.open_draft_step(next, moved);
    }

    fn open_draft_step(&mut self, draft: Draft, moved: bool) {
        if moved {
            self.open_draft(draft);
        } else if let Some(s) = self.stack.last_mut() {
            s.alarm_draft = Some(draft);
        }
    }

    /// A write answered: the editor closes on success, and on a refusal stays
    /// open with it said.
    /// Only the editor that sent the write is closed or told: Back while it was
    /// out and another alarm opened since used to be closed by it. A refusal
    /// with no such editor open is still said.
    pub(super) fn alarm_written(&mut self, alarm: Option<u32>, result: Result<(), String>) {
        let screen = self.stack.iter_mut().rev().find(|s| s.kind == super::screens::Kind::Alarms);
        let Some(screen) = screen.filter(|s| s.alarm_draft.as_ref().is_some_and(|d| d.writing && d.editing == alarm)) else {
            if let Err(e) = result {
                self.notice(&e);
            }
            return;
        };
        match result {
            Ok(()) => {
                screen.alarm_draft = None;
                self.focus = (zone::LIST, 0);
            }
            Err(e) => {
                if let Some(d) = screen.alarm_draft.as_mut() {
                    d.writing = false;
                }
                self.notice(&e);
            }
        }
    }

    /// The rows of the step on screen.
    pub(super) fn alarm_rows(&self, rows: &mut Rows, draft: &Draft, sounds: Option<&Result<Vec<AlarmProgram>, String>>) {
        let cell = |title: String, subtitle: &str| ListCell { title: title.into(), subtitle: subtitle.into(), ..Default::default() };
        match draft.step {
            Step::Room => {
                rows.heading("Which room?");
                for (id, name) in self.alarm_rooms() {
                    let chosen = draft.room.as_ref().is_some_and(|r| r.0 == id);
                    rows.item(ListCell { active: chosen, ..cell(name.clone(), "") }, RowAction::AlarmEditor(AlarmRow::Room(id, name)));
                }
            }
            Step::Time => {
                rows.heading("What time?");
                rows.message("Up and Down change it, Left and Right move between them, Select goes on.");
                let hour = ListCell { focus: rows.actions.len() as i32, ..cell(format!("{:02}", draft.hour), "Hour") };
                rows.actions.push(RowAction::AlarmEditor(AlarmRow::Hour));
                rows.cells.push((rows.row, 0));
                let minute = ListCell { focus: rows.actions.len() as i32, ..cell(format!("{:02}", draft.minute), "Minute") };
                rows.actions.push(RowAction::AlarmEditor(AlarmRow::Minute));
                rows.cells.push((rows.row, 1));
                rows.row += 1;
                rows.rows.push(crate::ListRow {
                    kind: crate::RowKind::Grid,
                    cells: slint::ModelRc::new(slint::VecModel::from(vec![hour, minute])),
                    ..Default::default()
                });
            }
            Step::Repeat => {
                rows.heading("How often?");
                for r in RECURRENCES {
                    let chosen = draft.recurrence == r;
                    rows.item(ListCell { active: chosen, ..cell(super::screens::recurrence_words(r), "") }, RowAction::AlarmEditor(AlarmRow::Repeat(r.into())));
                }
            }
            Step::Sound => {
                rows.heading("What should it play?");
                rows.item(ListCell { active: draft.program.is_none(), ..cell("Chime".into(), "The speaker's own") }, RowAction::AlarmEditor(AlarmRow::Sound(None)));
                match sounds {
                    None => rows.message("Reading the Sonos favourites\u{2026}"),
                    Some(Err(e)) => rows.message(&format!("Couldn't read the favourites: {e}")),
                    Some(Ok(list)) => {
                        for program in list {
                            let chosen = draft.program.as_ref().is_some_and(|p| p.uri == program.uri);
                            rows.item(ListCell { active: chosen, ..cell(program.title.clone(), "A Sonos favourite") }, RowAction::AlarmEditor(AlarmRow::Sound(Some(program.clone()))));
                        }
                    }
                }
            }
            Step::Confirm => {
                let room = draft.room.as_ref().map(|r| r.1.as_str()).unwrap_or("");
                rows.heading(&format!("{} \u{00B7} {room}", draft.time()));
                let sound = draft.program.as_ref().map_or("Chime", |p| p.title.as_str());
                rows.message(&format!("{} \u{00B7} {sound}", super::screens::recurrence_words(&draft.recurrence)));
                rows.item(cell(format!("Volume {}", draft.volume), "Left and Right to change"), RowAction::AlarmEditor(AlarmRow::Volume));
                match draft.editing {
                    None => rows.button("Make this alarm", RowAction::AlarmEditor(AlarmRow::Make)),
                    Some(_) => {
                        rows.button("Save changes", RowAction::AlarmEditor(AlarmRow::Save));
                        for (step, label) in CHANGES {
                            rows.button(label, RowAction::AlarmEditor(AlarmRow::Change(step)));
                        }
                        let armed = draft.remove_armed.is_some_and(|at| at.elapsed() < REMOVE_CONFIRM);
                        rows.button(if armed { "Press again to remove" } else { "Remove this alarm" }, RowAction::AlarmEditor(AlarmRow::Remove));
                    }
                }
            }
        }
    }

    /// Every speaker by its own name, alphabetically: an alarm rings on one.
    fn alarm_rooms(&self) -> Vec<(String, String)> {
        let mut rooms: Vec<(String, String)> = self
            .house
            .groups
            .as_ref()
            .map(|g| g.players.iter().map(|p| (p.id.clone(), p.name.clone())).collect())
            .unwrap_or_default();
        rooms.sort_by_key(|r| r.1.to_lowercase());
        rooms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_alarm_steps_forward_and_back_and_a_change_returns_to_its_summary() {
        let new = Draft::default();
        assert_eq!(new.clone().advanced().step, Step::Time);
        assert_eq!(new.back(), None, "Back from the first step leaves");
        let at_sound = Draft { step: Step::Sound, ..Draft::default() };
        assert_eq!(at_sound.back().map(|d| d.step), Some(Step::Repeat));
        let editing = Draft { step: Step::Time, editing: Some(3), ..Draft::default() };
        assert_eq!(editing.clone().advanced().step, Step::Confirm, "a change comes back to the summary");
        assert_eq!(editing.back().map(|d| d.step), Some(Step::Confirm));
        assert_eq!(Draft { step: Step::Confirm, editing: Some(3), ..Draft::default() }.back(), None);
    }

    #[test]
    fn the_fields_written_are_the_drafts() {
        let d = Draft { room: Some(("RINCON_K1".into(), "Kitchen".into())), hour: 6, minute: 45, volume: 12, ..Draft::default() };
        let f = d.fields().unwrap();
        assert_eq!(f.start, "06:45:00");
        assert_eq!((f.room.as_str(), f.volume, f.recurrence.as_str()), ("RINCON_K1", 12, "DAILY"));
        assert!(f.program.is_none(), "the chime");
        assert!(Draft::default().fields().is_none(), "no room, nothing to write");
    }
}
