//! Keys and gamepads, as one set of actions.
//!
//! The Deck in Desktop Mode turns its pad into keys (D-pad and left stick are
//! arrows, A is Enter, B is Escape); in Gaming Mode Steam Input hands a non-Steam
//! app an Xbox-style gamepad. A keyboard and a USB or Bluetooth pad are the same
//! two cases on any desktop. All of them become an [`Action`] with a [`Phase`],
//! so the screen only ever answers actions.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use slint::platform::Key;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Up,
    Down,
    Left,
    Right,
    /// Enter, A: the TV remote's Select.
    Select,
    /// Escape, Backspace, B: the remote's Back.
    Back,
    /// The remote's Menu: Tab or M, Y.
    Menu,
    PlayPause,
    VolumeUp,
    VolumeDown,
    Next,
    Previous,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Press,
    /// The platform's auto-repeat, or ours for a held pad direction or volume button.
    Repeat,
    Release,
    /// The input device disconnected or was cancelled while held; clears held state without activating.
    Cancel,
}

/// What a key means, from Slint's key text: a control character for a named
/// key, the character itself otherwise.
pub fn action_for_key(text: &str) -> Option<Action> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let is = |key: Key| c == char::from(key);
    Some(if is(Key::UpArrow) {
        Action::Up
    } else if is(Key::DownArrow) {
        Action::Down
    } else if is(Key::LeftArrow) {
        Action::Left
    } else if is(Key::RightArrow) {
        Action::Right
    } else if is(Key::Return) || c == '\r' {
        Action::Select
    } else if is(Key::Escape) || is(Key::Backspace) {
        Action::Back
    } else if is(Key::Tab) || is(Key::Menu) {
        Action::Menu
    } else if is(Key::F11) {
        Action::Fullscreen
    } else if is(Key::PageUp) {
        Action::Previous
    } else if is(Key::PageDown) {
        Action::Next
    } else {
        return None;
    })
}

/// The shortcut keys that only mean something while no text is being typed:
/// Space plays and pauses, - and = are volume, M is Menu.
pub fn shortcut_for_char(c: char) -> Option<Action> {
    Some(match c {
        ' ' => Action::PlayPause,
        '-' | '_' => Action::VolumeDown,
        '=' | '+' => Action::VolumeUp,
        'm' | 'M' => Action::Menu,
        '[' => Action::Previous,
        ']' => Action::Next,
        _ => return None,
    })
}

/// Held directions and volume buttons repeat after 400ms, then every 90ms.
#[derive(Default)]
struct Repeater {
    held: Vec<(Action, Instant)>,
}

impl Repeater {
    const DELAY: Duration = Duration::from_millis(400);
    const RATE: Duration = Duration::from_millis(90);

    fn press(&mut self, action: Action, now: Instant) {
        if !self.held.iter().any(|(held, _)| *held == action) {
            self.held.push((action, now + Self::DELAY));
        }
    }

    fn release(&mut self, action: Action) {
        self.held.retain(|(held, _)| *held != action);
    }

    fn due(&mut self, now: Instant, out: &mut Vec<(Action, Phase)>) {
        for (action, next) in &mut self.held {
            if now >= *next {
                out.push((*action, Phase::Repeat));
                // Do not replay a backlog after the thread was descheduled.
                *next = now + Self::RATE;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum PadEvent {
    Button(gilrs::Button, bool),
    Axis(gilrs::Axis, f32),
    Disconnect,
}

#[derive(Default)]
struct Pad {
    buttons: Vec<gilrs::Button>,
    stick: Option<Action>,
    repeater: Repeater,
}

impl Pad {
    fn actions(&self) -> Vec<Action> {
        let mut actions = Vec::new();
        for action in self
            .buttons
            .iter()
            .filter_map(|b| button_action(*b))
            .chain(self.stick)
        {
            if !actions.contains(&action) {
                actions.push(action);
            }
        }
        actions
    }

    fn update(&mut self, event: PadEvent) {
        match event {
            PadEvent::Button(button, true) => {
                if button_action(button).is_some() && !self.buttons.contains(&button) {
                    self.buttons.push(button);
                }
            }
            PadEvent::Button(button, false) => self.buttons.retain(|b| *b != button),
            PadEvent::Axis(axis, value) => {
                use gilrs::Axis;
                let toward = match axis {
                    Axis::LeftStickX if value > 0.5 => Some(Action::Right),
                    Axis::LeftStickX if value < -0.5 => Some(Action::Left),
                    Axis::LeftStickY if value > 0.5 => Some(Action::Up),
                    Axis::LeftStickY if value < -0.5 => Some(Action::Down),
                    Axis::LeftStickX | Axis::LeftStickY => None,
                    _ => return,
                };
                let same_axis = |a: Action| match axis {
                    Axis::LeftStickX => matches!(a, Action::Left | Action::Right),
                    _ => matches!(a, Action::Up | Action::Down),
                };
                if toward.is_some() || self.stick.is_some_and(same_axis) {
                    self.stick = toward;
                }
            }
            PadEvent::Disconnect => {
                self.buttons.clear();
                self.stick = None;
            }
        }
    }
}

/// Pure input state: explicit pad ids, events and time; no gilrs device or clock.
#[derive(Default)]
struct Gamepads {
    pads: BTreeMap<usize, Pad>,
}

impl Gamepads {
    fn step(&mut self, event: Option<(usize, PadEvent)>, now: Instant) -> Vec<(Action, Phase)> {
        let mut out = Vec::new();
        if let Some((id, event)) = event {
            let pad = self.pads.entry(id).or_default();
            let before = pad.actions();
            pad.update(event);
            let after = pad.actions();
            let phase = if matches!(event, PadEvent::Disconnect) {
                Phase::Cancel
            } else {
                Phase::Release
            };
            for action in before.iter().filter(|a| !after.contains(a)) {
                out.push((*action, phase));
                pad.repeater.release(*action);
            }
            for action in after.iter().filter(|a| !before.contains(a)) {
                out.push((*action, Phase::Press));
                if is_direction(*action) || matches!(action, Action::VolumeUp | Action::VolumeDown)
                {
                    pad.repeater.press(*action, now);
                }
            }
            if matches!(event, PadEvent::Disconnect) {
                self.pads.remove(&id);
            }
        }
        // Run after every event, including ignored/noisy events, and after timeouts.
        for pad in self.pads.values_mut() {
            pad.repeater.due(now, &mut out);
        }
        out
    }

    fn wait(&self, now: Instant) -> Option<Duration> {
        self.pads
            .values()
            .flat_map(|p| p.repeater.held.iter().map(|(_, next)| *next))
            .min()
            .map(|next| next.saturating_duration_since(now))
    }
}

/// Gamepads on their own thread: evdev on Linux, XInput on Windows.
pub fn spawn_gamepads(send: impl Fn(Action, Phase) + Send + 'static) {
    std::thread::Builder::new()
        .name("gamepads".into())
        .spawn(move || {
            let mut gilrs = match gilrs::Gilrs::new() {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("x2rock-deck: no gamepad support here ({e})");
                    return;
                }
            };
            let mut state = Gamepads::default();
            loop {
                let event = gilrs
                    .next_event_blocking(state.wait(Instant::now()))
                    .and_then(|event| {
                        use gilrs::EventType;
                        let input = match event.event {
                            EventType::ButtonPressed(button, _) => PadEvent::Button(button, true),
                            EventType::ButtonReleased(button, _) => PadEvent::Button(button, false),
                            EventType::AxisChanged(axis, value, _) => PadEvent::Axis(axis, value),
                            EventType::Disconnected => PadEvent::Disconnect,
                            _ => return None,
                        };
                        Some((usize::from(event.id), input))
                    });
                for (action, phase) in state.step(event, Instant::now()) {
                    send(action, phase);
                }
            }
        })
        .expect("the gamepad thread starts");
}

fn is_direction(action: Action) -> bool {
    matches!(
        action,
        Action::Up | Action::Down | Action::Left | Action::Right
    )
}

/// The pad's buttons. gilrs names them by position: South is A on an Xbox pad
/// and on the Deck.
fn button_action(button: gilrs::Button) -> Option<Action> {
    use gilrs::Button::*;
    Some(match button {
        DPadUp => Action::Up,
        DPadDown => Action::Down,
        DPadLeft => Action::Left,
        DPadRight => Action::Right,
        South => Action::Select,
        East => Action::Back,
        North => Action::Menu,
        West => Action::PlayPause,
        LeftTrigger => Action::VolumeDown,
        RightTrigger => Action::VolumeUp,
        LeftTrigger2 => Action::Previous,
        RightTrigger2 => Action::Next,
        Select => Action::Fullscreen,
        Start => Action::Menu,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_to_the_remotes_actions() {
        assert_eq!(
            action_for_key(&String::from(char::from(Key::UpArrow))),
            Some(Action::Up)
        );
        assert_eq!(action_for_key("\n"), Some(Action::Select));
        assert_eq!(
            action_for_key(&String::from(char::from(Key::Escape))),
            Some(Action::Back)
        );
        assert_eq!(action_for_key("a"), None);
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn button(id: usize, b: gilrs::Button, pressed: bool) -> Option<(usize, PadEvent)> {
        Some((id, PadEvent::Button(b, pressed)))
    }

    #[test]
    fn held_direction_and_volume_repeat_until_released() {
        use gilrs::Button::*;
        for (b, action) in [
            (DPadDown, Action::Down),
            (LeftTrigger, Action::VolumeDown),
            (RightTrigger, Action::VolumeUp),
        ] {
            let t0 = Instant::now();
            let mut s = Gamepads::default();
            assert_eq!(s.step(button(1, b, true), t0), [(action, Phase::Press)]);
            assert_eq!(s.wait(t0), Some(Repeater::DELAY));
            assert!(s.step(None, at(t0, 399)).is_empty());
            assert_eq!(s.step(None, at(t0, 400)), [(action, Phase::Repeat)]);
            assert_eq!(s.wait(at(t0, 450)), Some(Duration::from_millis(40)));
            assert!(s.step(None, at(t0, 489)).is_empty());
            assert_eq!(s.step(None, at(t0, 490)), [(action, Phase::Repeat)]);
            assert_eq!(
                s.step(button(1, b, false), at(t0, 500)),
                [(action, Phase::Release)]
            );
            assert!(s.step(None, at(t0, 900)).is_empty());
            assert_eq!(s.wait(at(t0, 900)), None);
        }
    }

    #[test]
    fn two_pads_keep_independent_holds_and_sticks() {
        use gilrs::{Axis::*, Button::*};
        let t0 = Instant::now();
        let mut s = Gamepads::default();
        assert_eq!(
            s.step(Some((1, PadEvent::Axis(LeftStickX, 1.0))), t0),
            [(Action::Right, Phase::Press)]
        );
        assert_eq!(
            s.step(Some((2, PadEvent::Axis(LeftStickY, 1.0))), at(t0, 10)),
            [(Action::Up, Phase::Press)]
        );
        assert_eq!(
            s.step(Some((2, PadEvent::Axis(LeftStickY, 0.0))), at(t0, 20)),
            [(Action::Up, Phase::Release)]
        );
        assert!(s.step(button(2, DPadRight, false), at(t0, 30)).is_empty());
        assert_eq!(s.step(None, at(t0, 400)), [(Action::Right, Phase::Repeat)]);
        assert!(
            s.step(Some((1, PadEvent::Axis(LeftStickY, 0.0))), at(t0, 401))
                .is_empty()
        );
        assert_eq!(
            s.step(Some((1, PadEvent::Axis(LeftStickX, 0.0))), at(t0, 402)),
            [(Action::Right, Phase::Release)]
        );
    }

    #[test]
    fn disconnect_cancels_all_buttons_and_stick_and_clears_repeats() {
        use gilrs::{Axis::*, Button::*};
        let t0 = Instant::now();
        let mut s = Gamepads::default();
        s.step(button(1, South, true), t0);
        s.step(button(1, LeftTrigger, true), t0);
        s.step(Some((1, PadEvent::Axis(LeftStickX, -1.0))), t0);
        s.step(button(2, DPadDown, true), t0);
        assert_eq!(
            s.step(Some((1, PadEvent::Disconnect)), at(t0, 100)),
            [
                (Action::Select, Phase::Cancel),
                (Action::VolumeDown, Phase::Cancel),
                (Action::Left, Phase::Cancel),
            ]
        );
        assert_eq!(s.step(None, at(t0, 400)), [(Action::Down, Phase::Repeat)]);
        assert!(
            s.step(Some((1, PadEvent::Axis(LeftStickX, 0.0))), at(t0, 401))
                .is_empty()
        );
        assert_eq!(
            s.step(button(1, South, true), at(t0, 402)),
            [(Action::Select, Phase::Press)]
        );
    }

    #[test]
    fn disconnect_before_hold_emits_cancel_and_no_repeats() {
        use gilrs::Button::*;
        let t0 = Instant::now();
        let mut s = Gamepads::default();
        assert_eq!(
            s.step(button(1, South, true), t0),
            [(Action::Select, Phase::Press)]
        );
        assert_eq!(
            s.step(Some((1, PadEvent::Disconnect)), at(t0, 100)),
            [(Action::Select, Phase::Cancel)]
        );
        assert!(s.step(None, at(t0, 500)).is_empty());
        assert!(s.step(None, at(t0, 600)).is_empty());
    }

    #[test]
    fn continuous_axis_events_do_not_starve_repeats() {
        use gilrs::{Axis::*, Button::*};
        let t0 = Instant::now();
        let mut s = Gamepads::default();
        s.step(button(1, DPadDown, true), t0);
        let mut repeats = Vec::new();
        for ms in (10..=1000).step_by(10) {
            let out = s.step(Some((2, PadEvent::Axis(RightStickX, 0.01))), at(t0, ms));
            if !out.is_empty() {
                assert_eq!(out, [(Action::Down, Phase::Repeat)]);
                repeats.push(ms);
            }
        }
        assert_eq!(repeats, [400, 490, 580, 670, 760, 850, 940]);
    }

    #[test]
    fn overlapping_controls_do_not_release_or_restart_the_same_action() {
        use gilrs::{Axis::*, Button::*};
        let t0 = Instant::now();
        let mut s = Gamepads::default();
        s.step(button(1, DPadRight, true), t0);
        assert!(s.step(button(1, DPadRight, true), at(t0, 50)).is_empty());
        assert!(
            s.step(Some((1, PadEvent::Axis(LeftStickX, 1.0))), at(t0, 100))
                .is_empty()
        );
        assert!(s.step(button(1, DPadRight, false), at(t0, 200)).is_empty());
        assert_eq!(s.step(None, at(t0, 400)), [(Action::Right, Phase::Repeat)]);
        assert_eq!(
            s.step(Some((1, PadEvent::Axis(LeftStickX, 0.0))), at(t0, 401)),
            [(Action::Right, Phase::Release)]
        );
    }
}
