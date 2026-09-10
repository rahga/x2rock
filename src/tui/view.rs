//! Drawing, and nothing else. No state moves in here.
//!
//! **Three lines a room, and the third one is optional.** What a room is doing
//! is a title, a line about the track and a line of context, and the context
//! line is the one that is often empty - a queue playing a local file has no
//! station, no input format and no members. Reserving it anyway would space the
//! list out around information that is not there, so it is dropped per room.
//!
//! Widths are measured with [`Line::width`], never `str::len`. A room named 寝室
//! is two columns per character and one byte-counted pad would push the volume
//! off the right edge.

use ratatui::Frame;
use std::time::Duration;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use super::model::{RoomSnapshot, State};
use super::{App, GroupRow, Kind, Overlay};

/// Cells in a volume bar. Ten reads as a percentage at a glance without the
/// number, which is what the number beside it is for anyway.
const BAR: usize = 10;

pub fn draw(frame: &mut Frame, app: &App) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(header(app, head.width as usize), head);
    rooms(frame, app, body);
    frame.render_widget(footer(app, foot.width as usize), foot);

    match app.overlay() {
        Overlay::None => {}
        Overlay::Group { .. } => grouping(frame, app, body),
        Overlay::Help => help(frame, body),
        Overlay::Confirm { prompt, .. } => confirm(frame, prompt, body),
    }
}

fn header(app: &App, width: usize) -> Paragraph<'static> {
    let rooms = app.rooms().len();
    let playing = app
        .rooms()
        .iter()
        .filter(|room| room.state == State::Playing)
        .count();
    let count = match (playing, rooms) {
        (0, 1) => "1 room".to_owned(),
        (0, rooms) => format!("{rooms} rooms"),
        (playing, rooms) => format!("{playing} of {rooms} playing"),
    };
    let mut right = vec![Span::styled(count, Style::new().dim())];
    // Loud, and only when there is something to be loud about. This is the one
    // thing on the screen a reader cannot check by looking at the rest of it:
    // every other line looks exactly the same whether it is current or an hour
    // old.
    if let Some(since) = app.stale_for() {
        right.push(Span::styled(
            format!(" · no answer for {}", ago(since)),
            Style::new().fg(Color::Red),
        ));
    }
    Paragraph::new(shoulders(
        vec![Span::styled("x2rock", Style::new().bold())],
        right,
        width,
    ))
}

/// The keys, or whatever the last one had to say instead.
fn footer(app: &App, width: usize) -> Paragraph<'static> {
    if let Some(status) = app.status() {
        let style = match status.kind {
            Kind::Busy => Style::new().dim(),
            Kind::Note => Style::new(),
            Kind::Bad => Style::new().fg(Color::Red),
        };
        // Only the first line: an error from the CLI can be a paragraph, and
        // the footer is one row. The full text is what the CLI prints when the
        // same command is typed.
        let text = status.text.lines().next().unwrap_or_default().to_owned();
        return Paragraph::new(Line::styled(clip(&text, width), style));
    }
    // Widest that fits. Three tiers rather than two, because the full line
    // wants about 110 columns and the terminal it is being read in is usually
    // 80 or 100 - falling straight from everything to "q quit" would leave the
    // common width with the least help.
    let keys = [
        "space play/pause · n/p skip · ←→ volume · m mute · r repeat · s shuffle · x crossfade · g group · t tv · P party · ? keys · q quit",
        "space play/pause · n/p skip · ←→ volume · g group · ? keys · q quit",
        "? keys · q quit",
    ];
    let keys = keys
        .into_iter()
        .find(|line| Span::raw(*line).width() <= width)
        .unwrap_or("q quit");
    Paragraph::new(Line::styled(keys, Style::new().dim()))
}

fn rooms(frame: &mut Frame, app: &App, area: Rect) {
    if app.rooms().is_empty() {
        // Reachable only between a daemon losing every player and publishing
        // them again, which is a second at most - but a blank screen during it
        // would read as an empty household.
        frame.render_widget(
            Paragraph::new(Line::styled(
                "no rooms published; waiting for the daemon",
                Style::new().dim(),
            )),
            area,
        );
        return;
    }
    // Two columns of highlight marker, and the last column left alone: a line
    // that ends exactly at the edge wraps in some terminals.
    let width = (area.width as usize).saturating_sub(3);
    let items: Vec<ListItem> = app.rooms().iter().map(|room| item(room, width)).collect();
    let list = List::new(items)
        .highlight_symbol("▌ ")
        .highlight_style(Style::new().bold());
    let mut state = ListState::default().with_selected(Some(app.cursor()));
    frame.render_stateful_widget(list, area, &mut state);
}

/// One room: name and volume, what is playing, and the context line.
fn item(room: &RoomSnapshot, width: usize) -> ListItem<'static> {
    let mut lines = Vec::new();

    let name = format!("{} {}", glyph(room.state), label(room));
    lines.push(shoulders(
        vec![Span::raw(name)],
        volume(room.volume, room.muted, room.fixed_volume),
        width,
    ));

    let now = room.now_line();
    if !now.is_empty() {
        lines.push(Line::raw(clip(&now, width)));
    }

    let context = context(room);
    let modes = modes(room);
    if !context.is_empty() || !modes.is_empty() {
        lines.push(shoulders(
            vec![Span::styled(context, Style::new().dim())],
            modes,
            width,
        ));
    }

    // One blank line between rooms. Part of the item rather than a spacer
    // widget, because the list scrolls by item and a gap that scrolls
    // separately would drift.
    lines.push(Line::raw(""));
    ListItem::new(lines)
}

/// A group answers to its coordinator's name, which alone would look like a
/// lone room; the count is what says otherwise, and the members are named on
/// the context line below.
fn label(room: &RoomSnapshot) -> String {
    match room.members.len() {
        0 | 1 => room.room.clone(),
        members => format!("{} + {}", room.room, members - 1),
    }
}

fn glyph(state: State) -> &'static str {
    match state {
        State::Playing => "▶",
        State::Paused => "‖",
        State::Stopped => "·",
    }
}

/// What is worth saying about the room beyond the track: that it is not
/// playing, the station, the format the TV is sending, and who else is in the
/// group - whichever of those this room has something to say about.
///
/// The state is a *word* here and a glyph above deliberately. A glyph is enough
/// while something is playing, because the track line is also saying so; paused
/// and idle are the two a reader has to be sure of, and "paused" cannot be
/// misread the way `‖` can.
fn context(room: &RoomSnapshot) -> String {
    let mut parts = Vec::new();
    if room.state != State::Playing {
        parts.push(room.state.label().to_owned());
    }
    if !room.station_label().is_empty() {
        parts.push(room.station_label().to_owned());
    }
    if !room.input_format.is_empty() {
        parts.push(room.input_format.clone());
    }
    if room.is_group() {
        let others: Vec<&str> = room.others().map(String::as_str).collect();
        parts.push(format!("with {}", others.join(", ")));
    }
    parts.join(" · ")
}

/// The badges on the right of the context line. Repeat and shuffle only where
/// the source can do them, which is also why they are not drawn as off: a radio
/// stream has no shuffle to be off.
fn modes(room: &RoomSnapshot) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut badge = |text: &str, style: Style| {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(text.to_owned(), style));
    };
    if room.on_tv {
        badge("TV", Style::new().fg(Color::Cyan));
    }
    match room.loop_status.as_str() {
        "Playlist" => badge("repeat all", Style::new().dim()),
        "Track" => badge("repeat one", Style::new().dim()),
        _ => {}
    }
    if room.shuffle {
        badge("shuffle", Style::new().dim());
    }
    if room.crossfade {
        badge("crossfade", Style::new().dim());
    }
    spans
}

/// `██████░░░░  62%`, or `muted ████░░░░░░  40%` with the bar dimmed. The Sonos
/// app's own picture of mute: the slider stays where it was and dims, so the
/// level the room will come back at is still readable, and the word says why
/// nothing is heard. The level is the one the daemon publishes regardless of
/// mute; the MPRIS volume alone would put the bar at zero.
///
/// The word goes to the *left* of the bar, growing into the gap, so that the
/// bar and the percentage keep their columns down the screen - this whole block
/// is right-aligned, and a word on the end would push those two left on the one
/// row that has it.
fn volume(volume: f64, muted: bool, fixed: bool) -> Vec<Span<'static>> {
    // No bar at all: the level is set on an amplifier this cannot see, and a
    // bar would be a control drawn where there is nothing to control.
    if fixed {
        return vec![Span::styled("fixed volume", Style::new().dim())];
    }
    let percent = (volume.clamp(0.0, 1.0) * 100.0).round() as usize;
    let filled = (percent * BAR + 50) / 100;
    let lit = if muted {
        Style::new().dim()
    } else {
        Style::new()
    };
    let mut spans = Vec::with_capacity(4);
    if muted {
        spans.push(Span::styled("muted ", Style::new().fg(Color::Yellow)));
    }
    spans.push(Span::styled("█".repeat(filled), lit));
    spans.push(Span::styled("░".repeat(BAR - filled), Style::new().dim()));
    spans.push(Span::styled(format!(" {percent:>3}%"), lit));
    spans
}

/// The grouping and balance overlay: who is playing together, each with the
/// volume that is theirs alone, and everyone else a keypress from joining.
fn grouping(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.group_rows();
    let title = app
        .selected()
        .map(|room| format!(" Grouping — {} ", room.room))
        .unwrap_or_else(|| " Grouping ".to_owned());
    let height = (rows.len() as u16 + 2).min(area.height);
    let area = centered(area, 52, height);
    let width = (area.width as usize).saturating_sub(4);

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            GroupRow::Member {
                room,
                volume: level,
                muted,
                fixed,
                coordinator,
            } => {
                let name = if *coordinator {
                    format!("{room} (coordinates)")
                } else {
                    room.clone()
                };
                ListItem::new(shoulders(
                    vec![Span::raw(name)],
                    volume(f64::from(*level) / 100.0, *muted, *fixed),
                    width,
                ))
            }
            GroupRow::Outsider { room } => ListItem::new(shoulders(
                vec![Span::styled(room.clone(), Style::new().dim())],
                vec![Span::styled("join", Style::new().dim())],
                width,
            )),
        })
        .collect();

    let block = Block::bordered()
        .title(title)
        .title_bottom(" ←→ balance · enter join/leave · esc back ");
    frame.render_widget(Clear, area);
    let list = List::new(items)
        .block(block)
        .highlight_symbol("▌")
        .highlight_style(Style::new().bold());
    let mut state = ListState::default().with_selected(Some(app.group_cursor()));
    frame.render_stateful_widget(list, area, &mut state);
}

fn confirm(frame: &mut Frame, prompt: &str, area: Rect) {
    let area = centered(area, Span::raw(prompt).width() as u16 + 4, 5);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(prompt.to_owned()),
            Line::raw(""),
            Line::styled("y to confirm · esc to cancel", Style::new().dim()),
        ])
        .block(Block::bordered().title(" Party ")),
        area,
    );
}

fn help(frame: &mut Frame, area: Rect) {
    let keys = [
        ("j / k, ↑ / ↓", "move between rooms"),
        ("space", "play or pause"),
        ("n / p", "next, previous"),
        ("← / →, - / +", "the group's volume"),
        ("m", "mute, or unmute"),
        ("r", "repeat: off, all, one"),
        ("s", "shuffle"),
        ("x", "crossfade"),
        ("g", "grouping, and each speaker's own volume"),
        ("t", "switch a soundbar to its TV input"),
        ("P", "party: the whole house, or end it"),
        ("q", "quit"),
    ];
    let area = centered(area, 52, keys.len() as u16 + 2);
    // Two borders and a column of air on the right, so what each key does does
    // not sit against the frame.
    let width = (area.width as usize).saturating_sub(3);
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, what)| {
            shoulders(
                vec![Span::raw(format!(" {key}"))],
                vec![Span::styled(what.to_owned(), Style::new().dim())],
                width,
            )
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
        area,
    );
}

/// A span of time, at the coarsest resolution that still says something. The
/// reader wants to know whether this is a hiccup or an outage, not the seconds.
fn ago(since: Duration) -> String {
    let seconds = since.as_secs();
    match seconds {
        0..120 => format!("{seconds}s"),
        120..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// A box in the middle of `area`, never bigger than it.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// One line with something at each end, the gap sized by rendered width.
///
/// **The right-hand side is what fits, and the left gives way to it.** A volume
/// or a row of badges is a fixed thing a reader is looking for; a room name or a
/// station is not, and is the half that can lose its tail to an ellipsis. Doing
/// that here rather than at each call site is also what keeps the reserved
/// widths from being guessed twice and disagreeing.
fn shoulders(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let reserved = Line::from(right.clone()).width();
    let free = width.saturating_sub(reserved + 1);
    let mut spans = Vec::with_capacity(left.len() + right.len() + 1);
    let mut used = 0;
    for span in left {
        if used >= free {
            break;
        }
        let text = clip(&span.content, free - used);
        if text.is_empty() {
            break;
        }
        used += Span::raw(text.clone()).width();
        spans.push(Span::styled(text, span.style));
    }
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(used + reserved).max(1)),
    ));
    spans.extend(right);
    Line::from(spans)
}

/// Cut to `max` columns, with an ellipsis where something was cut. By column
/// and not by byte or by `char`: the strings here are track titles from a
/// music service, which is exactly where the wide and the multi-byte turn up.
fn clip(text: &str, max: usize) -> String {
    if Span::raw(text).width() <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let wide = Span::raw(c.to_string()).width();
        if used + wide + 1 > max {
            break;
        }
        used += wide;
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> RoomSnapshot {
        RoomSnapshot {
            room: "Kitchen".to_owned(),
            state: State::Playing,
            members: vec!["Kitchen".to_owned()],
            ..RoomSnapshot::default()
        }
    }

    /// The volume is the half a reader is looking for, so the name is the half
    /// that loses its tail - and the line still ends exactly where it should.
    #[test]
    fn a_long_name_gives_way_to_the_volume_rather_than_pushing_it_off() {
        let line = shoulders(
            vec![Span::raw("A Room With A Really Very Long Name Indeed")],
            volume(0.62, false, false),
            30,
        );
        assert_eq!(line.width(), 30);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.ends_with(" 62%"), "{rendered:?}");
        assert!(rendered.contains('…'), "{rendered:?}");
    }

    /// Nothing on the right means the whole width is the left's to use.
    #[test]
    fn an_empty_right_shoulder_still_leaves_a_line_that_fits() {
        let line = shoulders(vec![Span::raw("Kitchen")], Vec::new(), 20);
        assert_eq!(line.width(), 20);
    }

    /// Columns, not bytes and not characters. A byte count would cut this name
    /// to a third of the space it was given; the budget is a ceiling rather
    /// than a target, because two-column characters cannot always land on it.
    #[test]
    fn clipping_counts_columns() {
        assert_eq!(clip("寝室", 8), "寝室");
        assert!(Span::raw(clip("寝室で音楽", 6)).width() <= 6);
        assert!(Span::raw(clip("寝室で音楽", 7)).width() <= 7);
        assert_eq!(clip("寝室で音楽", 6), "寝室…");
    }

    /// The context line carries the state as a word only when the room is not
    /// playing, since the track line says it otherwise.
    #[test]
    fn the_context_line_names_a_state_that_is_not_playing() {
        assert_eq!(context(&room()), "");
        assert_eq!(
            context(&RoomSnapshot {
                state: State::Paused,
                ..room()
            }),
            "paused"
        );
        assert_eq!(
            context(&RoomSnapshot {
                state: State::Stopped,
                station_name: "SomaFM".to_owned(),
                ..room()
            }),
            "idle · SomaFM"
        );
    }

    /// A group answers to its coordinator's name, so the count is the only
    /// thing that says it is more than one room.
    #[test]
    fn a_group_is_named_for_its_coordinator_and_counts_the_rest() {
        assert_eq!(label(&room()), "Kitchen");
        assert_eq!(
            label(&RoomSnapshot {
                members: vec!["Kitchen".to_owned(), "Office".to_owned()],
                ..room()
            }),
            "Kitchen + 1"
        );
        // Listed second, as Sonos is free to list it: the guests are whoever
        // is not the coordinator, not whoever is not first.
        assert_eq!(
            context(&RoomSnapshot {
                members: vec!["Office".to_owned(), "Kitchen".to_owned()],
                ..room()
            }),
            "with Office"
        );
    }

    /// The one line on the screen that cannot be checked against the rest of
    /// it, drawn through the real widget stack so that "it is in the header"
    /// is a fact rather than a plan.
    #[test]
    fn a_screen_that_cannot_vouch_for_itself_says_so_in_the_header() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::time::Instant;

        let mut app = App::new_for_test(vec![RoomSnapshot {
            room: "Kitchen".to_owned(),
            members: vec!["Kitchen".to_owned()],
            ..RoomSnapshot::default()
        }]);
        let mut terminal = Terminal::new(TestBackend::new(70, 8)).expect("a test terminal");

        terminal.draw(|frame| draw(frame, &app)).expect("a frame");
        let fresh = terminal.backend().to_string();
        assert!(fresh.contains("1 room"), "{fresh}");
        assert!(!fresh.contains("no answer"), "{fresh}");

        app.set_contacted_for_test(Instant::now() - Duration::from_secs(600));
        terminal.draw(|frame| draw(frame, &app)).expect("a frame");
        let stale = terminal.backend().to_string();
        assert!(stale.contains("no answer for 10m"), "{stale}");
    }

    /// Muted keeps the level in view and says so to the left of the bar, where
    /// the word cannot push the bar and the percentage out of the columns they
    /// share with every other row. A bar at zero would be indistinguishable
    /// from turned down, and a bar with no number would hide where the room
    /// comes back to.
    #[test]
    fn a_muted_room_says_so_before_its_bar_and_keeps_its_level() {
        let text: String = volume(0.4, true, false)
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.starts_with("muted ████░░░░░░"), "{text:?}");
        assert!(text.ends_with("  40%"), "{text:?}");
        // The bar and the number occupy the same columns as an unmuted room's,
        // the word having grown leftward into the gap.
        let plain: Vec<Span> = volume(0.4, false, false);
        let muted: Vec<Span> = volume(0.4, true, false);
        assert_eq!(
            Line::from(plain).width() + Span::raw("muted ").width(),
            Line::from(muted).width()
        );
        let unmuted: String = volume(0.4, false, false)
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(unmuted.ends_with("  40%"), "{unmuted:?}");
        assert!(!unmuted.contains("muted"), "{unmuted:?}");
    }

    /// A fixed volume has no bar, whatever else is true of the room: there is
    /// nothing for a bar to show and nothing for its keys to do.
    #[test]
    fn a_fixed_volume_draws_no_bar() {
        let text: String = volume(0.4, true, true)
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "fixed volume");
    }

    /// Coarse on purpose: the question is hiccup or outage.
    #[test]
    fn an_age_is_reported_at_the_resolution_that_matters() {
        assert_eq!(ago(Duration::from_secs(95)), "95s");
        assert_eq!(ago(Duration::from_secs(600)), "10m");
        assert_eq!(ago(Duration::from_secs(3600)), "1h 0m");
        assert_eq!(ago(Duration::from_secs(4980)), "1h 23m");
    }

    /// Ten cells and a percentage of the same number, so the bar and the digits
    /// cannot disagree.
    #[test]
    fn the_volume_bar_rounds_to_its_own_number() {
        let filled = |level| {
            volume(level, false, false)
                .first()
                .map(|span| span.content.chars().count())
                .unwrap_or_default()
        };
        assert_eq!(filled(0.0), 0);
        assert_eq!(filled(0.62), 6);
        assert_eq!(filled(1.0), BAR);
    }
}
