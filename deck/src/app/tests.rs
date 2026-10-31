//! The TV app's navigation rules, driven through the controller against the
//! demo household on the headless platform. One test, since Slint's platform
//! is set once a process.

use super::{App, Place, Transport, with_app, zone};
use crate::input::{Action, Phase};
use crate::prefs::Prefs;
use crate::{AppWindow, demo, household, shot};
use slint::Model;

fn press(action: Action) {
    with_app(|app| {
        app.action(action, Phase::Press);
        app.action(action, Phase::Release);
    });
}

fn focus() -> (i32, i32) {
    let mut f = (0, 0);
    with_app(|app| f = app.focus);
    f
}

fn transport_at(t: Transport) -> i32 {
    let mut i = -1;
    with_app(|app| {
        i = app
            .transport
            .iter()
            .position(|x| *x == t)
            .map(|p| p as i32)
            .unwrap_or(-1)
    });
    i
}

fn place_at(p: Place) -> i32 {
    let mut i = -1;
    with_app(|app| {
        i = app
            .places
            .iter()
            .position(|x| *x == p)
            .map(|p| p as i32)
            .unwrap_or(-1)
    });
    i
}

/// What was sent that would change the household: reads left out.
fn sent() -> Vec<household::Command> {
    let mut s = vec![];
    with_app(|app| s = app.sent.borrow().clone());
    s.retain(|c| {
        !matches!(
            c,
            household::Command::Fetch(_) | household::Command::ReadRoom { .. }
        )
    });
    s
}

fn selected_name() -> String {
    let mut n = String::new();
    with_app(|app| {
        n = app
            .selected_entry()
            .map(|e| app.room_label(e))
            .unwrap_or_default()
    });
    n
}

#[test]
fn navigation_follows_the_tv_apps_rules() {
    let (_window, _queue) = shot::install_headless().expect("a headless platform");
    let ui = AppWindow::new().expect("the window");
    crate::wire(&ui);
    super::install(App::new(&ui, Prefs::ephemeral("DEFAULT")));
    with_app(|app| {
        app.update(household::Update::Status(household::Status::Connected));
        app.update(household::Update::Household(Box::new(demo::household())));
    });

    // The first room is selected, and focus starts on it.
    assert_eq!(focus(), (zone::ROOMS, 0));
    assert_eq!(selected_name(), "Bedroom");

    // Down selects the room focus rests on.
    press(Action::Down);
    assert_eq!(focus(), (zone::ROOMS, 1));
    assert_eq!(selected_name(), "Dining Room + 1");

    // Right from a room enters the pane at Play/Pause.
    press(Action::Right);
    assert_eq!(
        focus(),
        (zone::TRANSPORT, transport_at(Transport::PlayPause))
    );

    // Up and Down move between the pane's rows and never leave it.
    press(Action::Up);
    assert_eq!(focus().0, zone::SEEK);
    press(Action::Up);
    assert_eq!(focus().0, zone::VOLUME);
    press(Action::Up);
    assert_eq!(focus().0, zone::VOLUME, "Up from the top stays in the pane");
    press(Action::Down);
    press(Action::Down);
    assert_eq!(
        focus(),
        (zone::TRANSPORT, transport_at(Transport::PlayPause)),
        "Play/Pause is remembered"
    );

    // Left from a row's leftmost control returns to the selected room.
    while focus().1 > 0 {
        press(Action::Left);
    }
    press(Action::Left);
    assert_eq!(focus(), (zone::ROOMS, 1));

    // Select on a room enters the pane; it does not open the panel.
    press(Action::Select);
    assert_eq!(
        focus(),
        (zone::TRANSPORT, transport_at(Transport::PlayPause))
    );

    // Back from the pane goes to the room list.
    press(Action::Back);
    assert_eq!(focus(), (zone::ROOMS, 1));

    // Group opens the room panel on the room's own row; Back returns to Group.
    press(Action::Right);
    press(Action::Down);
    while focus().1 < place_at(Place::Group) {
        press(Action::Right);
    }
    let group_button = focus();
    press(Action::Select);
    assert_eq!(focus().0, zone::ROOM_PANEL);
    let mut on_own_row = false;
    with_app(|app| {
        on_own_row = matches!(
            app.panel.as_ref().and_then(|p| p.actions.get(app.focus.1 as usize)),
            Some(super::panels::PanelAction::Leave(id)) if id == "RINCON_D1"
        )
    });
    assert!(on_own_row, "the Group panel opens on the room's own row");
    press(Action::Select);
    assert!(
        sent().is_empty(),
        "Select on the coordinator's row sends nothing: {:?}",
        sent()
    );
    press(Action::Back);
    assert_eq!(
        focus(),
        group_button,
        "closing the panel hands focus back to its opener"
    );

    // Settings sits at the foot of the list; Back from it returns to the rooms.
    press(Action::Back);
    for _ in 0..10 {
        press(Action::Down);
    }
    assert_eq!(focus().0, zone::SETTINGS);
    press(Action::Select);
    assert_eq!(
        focus(),
        (zone::SETTINGS_PANEL, 0),
        "Settings opens on the theme"
    );
    press(Action::Right);
    let mut theme = String::new();
    with_app(|app| theme = app.prefs.theme.clone());
    assert_eq!(theme, "OCEAN", "Right on the theme row steps the theme");
    press(Action::Back);
    assert_eq!(focus().0, zone::SETTINGS);

    // A hold on a room picks it up; Down carries it; Select puts it down.
    press(Action::Up);
    let carried = selected_name();
    with_app(|app| {
        app.select_key(Phase::Press);
        let (z, i) = app.focus;
        app.hold(z, i);
    });
    let mut moving = false;
    with_app(|app| moving = app.moving.is_some());
    assert!(moving, "a hold on a room picks it up");
    press(Action::Up);
    with_app(|app| {
        app.select_key(Phase::Release);
    });
    press(Action::Select);
    with_app(|app| moving = app.moving.is_some());
    assert!(!moving, "Select puts it down");
    let mut first = String::new();
    with_app(|app| {
        first = app
            .rooms
            .get(app.rooms.len() - 2)
            .map(|e| app.room_label(e))
            .unwrap_or_default()
    });
    assert_eq!(first, carried, "the carried room moved up one place");

    // Full screen closes on any key's release but Back's. From a music room:
    // a TV room's pane has no Full screen, as on the TV.
    while focus().1 > 0 {
        press(Action::Up);
    }
    assert_eq!(selected_name(), "Bedroom");
    press(Action::Right);
    press(Action::Down);
    assert_eq!(focus().0, zone::PLACES);
    while focus().1 < place_at(Place::FullScreen) {
        press(Action::Right);
    }
    press(Action::Select);
    assert_eq!(focus().0, zone::FULL);
    press(Action::Down);
    assert_eq!(
        focus(),
        (zone::PLACES, place_at(Place::FullScreen)),
        "a key's release closes full screen onto its button"
    );

    // A control that leaves the row under focus hands it to Play/Pause, not
    // to whatever slid into its place: Next at a queue's end, then Repeat.
    press(Action::Up);
    while focus().0 != zone::TRANSPORT {
        press(Action::Up);
    }
    while focus().1 < transport_at(Transport::Next) {
        press(Action::Right);
    }
    assert_eq!(focus(), (zone::TRANSPORT, transport_at(Transport::Next)));
    let mut house = demo::household();
    house
        .live
        .get_mut("RINCON_B1:1")
        .expect("Bedroom")
        .actions
        .can_repeat = true;
    with_app(|app| app.update(household::Update::Household(Box::new(house.clone()))));
    assert_eq!(focus(), (zone::TRANSPORT, transport_at(Transport::Next)));
    house
        .live
        .get_mut("RINCON_B1:1")
        .expect("Bedroom")
        .actions
        .can_skip = false;
    with_app(|app| app.update(household::Update::Household(Box::new(house.clone()))));
    assert_eq!(transport_at(Transport::Next), -1, "Next is gone");
    assert_eq!(
        focus(),
        (zone::TRANSPORT, transport_at(Transport::PlayPause)),
        "focus went to Play/Pause"
    );
    // And a control that stays keeps focus when others arrive before it.
    let bedroom = house.live.get_mut("RINCON_B1:1").expect("Bedroom");
    bedroom.actions.can_shuffle = true;
    bedroom.actions.can_skip_back = true;
    with_app(|app| app.update(household::Update::Household(Box::new(house))));
    assert_eq!(
        transport_at(Transport::PlayPause),
        2,
        "Shuffle and Previous arrived before it"
    );
    assert_eq!(
        focus(),
        (zone::TRANSPORT, 2),
        "Play/Pause keeps focus as they arrive"
    );
    with_app(|app| app.update(household::Update::Household(Box::new(demo::household()))));
    while focus().1 > 0 {
        press(Action::Left);
    }
    press(Action::Down);
    while focus().1 < place_at(Place::FullScreen) {
        press(Action::Right);
    }

    // Rows that come and go above the selected room leave focus on it.
    while focus().0 != zone::ROOMS {
        press(Action::Left);
    }
    press(Action::Down);
    press(Action::Down);
    let chosen = selected_name();
    let mut house = demo::household();
    if let Some(groups) = house.groups.as_mut() {
        groups.groups.retain(|g| g.name != "Bedroom");
    }
    with_app(|app| app.update(household::Update::Household(Box::new(house))));
    assert_eq!(selected_name(), chosen);
    let mut on_selected = false;
    with_app(|app| on_selected = app.selected_index() == Some(app.focus.1 as usize));
    assert!(
        on_selected,
        "focus stays on the selected room's row as a row above it goes"
    );
    with_app(|app| app.update(household::Update::Household(Box::new(demo::household()))));
    with_app(|app| on_selected = app.selected_index() == Some(app.focus.1 as usize));
    assert!(on_selected, "and as it comes back");
    press(Action::Right);
    press(Action::Down);
    while focus().1 < place_at(Place::FullScreen) {
        press(Action::Right);
    }

    // A list goes to the screen that asked for it: Browse's favourites,
    // answering after Music Services opened over it, are Browse's.
    with_app(|app| {
        app.open_screen(super::screens::Kind::Browse);
        app.open_screen(super::screens::Kind::Services);
        let favorite: x2rock::sonos::proto::Favorite =
            serde_json::from_value(serde_json::json!({ "id": "1", "name": "Jazz" })).unwrap();
        let id = app.stack[0].requests.favorites;
        app.update(household::Update::Fetched(household::Fetched::Favorites {
            id,
            result: Ok(vec![favorite]),
        }));
        assert!(
            app.stack[1].favorites.is_none(),
            "Music Services took Browse's favourites"
        );
        assert!(
            app.stack[0].favorites.is_some(),
            "Browse has its favourites"
        );
        app.close_screen();
        app.close_screen();
    });
    assert!(focus().0 != zone::LIST);

    // A panel row keeps focus as a row arrives above it: a sleep timer's
    // Cancel, while focus is on Bass.
    with_app(|app| {
        use super::panels::PanelAction;
        app.open_place(Place::Configure);
        let bass = app
            .panel
            .as_ref()
            .and_then(|p| p.actions.iter().position(|a| *a == PanelAction::Bass))
            .expect("a Bass row");
        app.focus.1 = bass as i32;
        let details = app.room_details.as_mut().expect("the demo's settings");
        details.sleep_left = Some(std::time::Duration::from_secs(600));
        details.read_at = Some(std::time::Instant::now());
        app.render_panel();
        let at = app
            .panel
            .as_ref()
            .and_then(|p| p.actions.get(app.focus.1 as usize).cloned());
        assert_eq!(at, Some(PanelAction::Bass), "focus stayed on Bass");
        assert!(
            app.panel
                .as_ref()
                .is_some_and(|p| p.actions.contains(&PanelAction::CancelSleep))
        );
        app.close_panel();
    });

    // On the on-screen keyboard, Return (Steam's desktop A) presses the focused
    // key, and the pad's own A arriving with it is dropped: one letter, no search.
    with_app(|app| {
        app.open_screen(super::screens::Kind::Search);
        // Something typed first: an empty query submits nothing either way.
        app.key("x", false, true);
        app.key("x", false, false);
        let enter = char::from(slint::platform::Key::Return).to_string();
        app.key(&enter, false, true);
        app.pad_action(Action::Select, Phase::Press);
        app.key(&enter, false, false);
        app.pad_action(Action::Select, Phase::Release);
        let screen = app.stack.last().expect("Search");
        assert_eq!(
            screen.query.chars().count(),
            2,
            "one key typed after the x: {:?}",
            screen.query
        );
        assert!(screen.searched.is_none(), "nothing searched");
        app.close_screen();
    });

    // Closing Search ends a search still under way, and only one.
    with_app(|app| {
        app.open_screen(super::screens::Kind::Search);
        app.sent.borrow_mut().clear();
        app.close_screen();
    });
    assert!(sent().is_empty(), "nothing to end: {:?}", sent());
    with_app(|app| {
        app.open_screen(super::screens::Kind::Search);
        if let Some(screen) = app.stack.last_mut() {
            screen.searched = Some("jazz".into());
            screen.searching = true;
        }
        app.close_screen();
    });
    assert!(
        matches!(sent().as_slice(), [household::Command::CancelSearch]),
        "{:?}",
        sent()
    );
    with_app(|app| app.sent.borrow_mut().clear());

    // A line-in is offered only with Beta features on, the room's own as
    // "Line-In" and another's by its room; Select puts this room on it.
    with_app(|app| {
        use super::panels::PanelAction;
        let mut house = demo::household();
        if let Some(groups) = house.groups.as_mut() {
            for p in groups.players.iter_mut().filter(|p| p.id == "RINCON_K1") {
                p.capabilities.push("LINE_IN".into());
            }
        }
        app.update(household::Update::Household(Box::new(house)));
        let line_in = |app: &App| {
            app.panel.as_ref().and_then(|p| {
                p.actions
                    .iter()
                    .position(|a| *a == PanelAction::LineIn("RINCON_K1".into()))
            })
        };
        app.prefs.beta_features = false;
        app.open_place(Place::Configure);
        assert!(line_in(app).is_none(), "no line-in without Beta features");
        app.close_panel();
        app.prefs.beta_features = true;
        app.open_place(Place::Configure);
        let at = line_in(app).expect("Kitchen's line-in offered");
        use slint::Model;
        let label = app
            .models
            .panel_items
            .model()
            .iter()
            .find(|i| i.focus == at as i32)
            .map(|i| i.label.to_string())
            .unwrap_or_default();
        app.sent.borrow_mut().clear();
        app.focus.1 = at as i32;
        app.panel_click(at as i32);
        app.close_panel();
        app.prefs.beta_features = false;
        app.update(household::Update::Household(Box::new(demo::household())));
        assert_eq!(label, "Line-In on Kitchen");
    });
    let line_in_sent = sent();
    assert!(
        matches!(line_in_sent.as_slice(), [household::Command::UseLineIn { player, .. }] if player == "RINCON_K1"),
        "{line_in_sent:?}"
    );

    // A new alarm, a step to a screen: room, time, how often, sound, then the
    // summary, whose Make sends it; a refusal keeps the editor open, and
    // success closes it.
    with_app(|app| {
        app.sent.borrow_mut().clear();
        app.open_screen(super::screens::Kind::Alarms);
        let id = app.requests().expect("requests").alarms;
        let read = Ok((vec![], "2026-10-29 06:30:00".to_owned(), true));
        app.update(household::Update::Fetched(household::Fetched::Alarms {
            id,
            result: read,
        }));
    });
    let editor_rows = || {
        let mut a = vec![];
        with_app(|app| {
            a = app
                .stack
                .last()
                .map(|s| s.actions.clone())
                .unwrap_or_default()
        });
        a
    };
    let step = || {
        let mut s = None;
        with_app(|app| s = app.alarm_draft().map(|d| d.step));
        s
    };
    let at = |want: &dyn Fn(&super::screens::RowAction) -> bool| {
        editor_rows().iter().position(want).expect("a row") as i32
    };
    use super::alarms::{AlarmRow, Step};
    use super::screens::RowAction as R;
    with_app(|app| app.focus = (zone::LIST, 0));
    let new = at(&|a| matches!(a, R::AlarmEditor(AlarmRow::New)));
    with_app(|app| app.focus = (zone::LIST, new));
    press(Action::Select);
    assert_eq!(step(), Some(Step::Room));
    let kitchen = at(&|a| matches!(a, R::AlarmEditor(AlarmRow::Room(id, _)) if id == "RINCON_K1"));
    with_app(|app| app.focus = (zone::LIST, kitchen));
    press(Action::Select);
    assert_eq!(step(), Some(Step::Time));
    assert_eq!(
        focus(),
        (
            zone::LIST,
            at(&|a| matches!(a, R::AlarmEditor(AlarmRow::Hour)))
        ),
        "the hour first"
    );
    press(Action::Up);
    press(Action::Up);
    press(Action::Right);
    press(Action::Down);
    press(Action::Select);
    assert_eq!(step(), Some(Step::Repeat));
    // Back goes a step back, keeping what was chosen.
    press(Action::Back);
    assert_eq!(step(), Some(Step::Time));
    let mut time = String::new();
    with_app(|app| time = app.alarm_draft().map(|d| d.time()).unwrap_or_default());
    assert_eq!(time, "09:59");
    press(Action::Select);
    let weekdays = at(&|a| matches!(a, R::AlarmEditor(AlarmRow::Repeat(r)) if r == "WEEKDAYS"));
    with_app(|app| app.focus = (zone::LIST, weekdays));
    press(Action::Select);
    assert_eq!(step(), Some(Step::Sound));
    with_app(|app| {
        let id = app.requests().expect("requests").alarm_sounds;
        app.update(household::Update::Fetched(
            household::Fetched::AlarmSounds {
                id,
                result: Ok(vec![]),
            },
        ));
    });
    press(Action::Select);
    assert_eq!(step(), Some(Step::Confirm));
    assert_eq!(
        focus(),
        (
            zone::LIST,
            at(&|a| matches!(a, R::AlarmEditor(AlarmRow::Make)))
        ),
        "the summary opens on Make"
    );
    press(Action::Up);
    press(Action::Right);
    press(Action::Right);
    press(Action::Down);
    press(Action::Select);
    let made = sent();
    let expected = household::AlarmFields {
        start: "09:59:00".into(),
        recurrence: "WEEKDAYS".into(),
        room: "RINCON_K1".into(),
        program: None,
        volume: 22,
    };
    assert!(
        matches!(made.as_slice(), [household::Command::WriteAlarm(household::AlarmWrite::Create(f))] if *f == expected),
        "{made:?}"
    );
    // A second press while the write is out sends nothing more.
    press(Action::Select);
    assert_eq!(sent().len(), 1, "one write");
    with_app(|app| {
        app.update(household::Update::Fetched(
            household::Fetched::AlarmWritten {
                alarm: None,
                result: Err("Kitchen already has an alarm at 09:59".into()),
            },
        ))
    });
    assert_eq!(
        step(),
        Some(Step::Confirm),
        "a refusal keeps the editor open"
    );
    press(Action::Select);
    assert_eq!(sent().len(), 2, "after a refusal, Make sends again");
    with_app(|app| {
        app.update(household::Update::Fetched(
            household::Fetched::AlarmWritten {
                alarm: Some(3),
                result: Ok(()),
            },
        ))
    });
    assert_eq!(
        step(),
        Some(Step::Confirm),
        "another write's answer leaves it open"
    );
    with_app(|app| {
        app.update(household::Update::Fetched(
            household::Fetched::AlarmWritten {
                alarm: None,
                result: Ok(()),
            },
        ))
    });
    assert_eq!(step(), None, "success closes it");

    // A hold on an alarm opens it on its summary; Remove takes two presses.
    let alarm = x2rock::sonos::upnp::Alarm {
        id: 7,
        start: "06:45:00".into(),
        duration: "00:15:00".into(),
        recurrence: "DAILY".into(),
        enabled: true,
        room_uuid: "RINCON_K1".into(),
        program_uri: household::CHIME.into(),
        program_metadata: String::new(),
        play_mode: "NORMAL".into(),
        volume: 12,
        include_linked_zones: false,
    };
    with_app(|app| {
        app.sent.borrow_mut().clear();
        let id = app.requests().expect("requests").alarms;
        let read = Ok((vec![alarm.clone()], "2026-10-29 06:30:00".to_owned(), true));
        app.update(household::Update::Fetched(household::Fetched::Alarms {
            id,
            result: read,
        }));
        let row = app
            .stack
            .last()
            .and_then(|s| {
                s.actions
                    .iter()
                    .position(|a| matches!(a, R::Alarm(a) if a.id == 7))
            })
            .expect("the alarm") as i32;
        app.focus = (zone::LIST, row);
        app.screen_hold(zone::LIST, row);
    });
    assert_eq!(step(), Some(Step::Confirm));
    assert_eq!(
        focus(),
        (
            zone::LIST,
            at(&|a| matches!(a, R::AlarmEditor(AlarmRow::Save)))
        ),
        "it opens on Save"
    );
    let remove = at(&|a| matches!(a, R::AlarmEditor(AlarmRow::Remove)));
    with_app(|app| app.focus = (zone::LIST, remove));
    press(Action::Select);
    assert!(sent().is_empty(), "one press only arms Remove");
    press(Action::Select);
    let removed = sent();
    assert!(
        matches!(
            removed.as_slice(),
            [household::Command::WriteAlarm(
                household::AlarmWrite::Remove(7)
            )]
        ),
        "{removed:?}"
    );
    with_app(|app| {
        app.update(household::Update::Fetched(
            household::Fetched::AlarmWritten {
                alarm: Some(7),
                result: Ok(()),
            },
        ))
    });
    assert_eq!(step(), None);

    // A save answered after Back, with another alarm opened since, leaves
    // that one's editor alone.
    let mut eight = alarm.clone();
    eight.id = 8;
    eight.start = "08:00:00".into();
    let hold = |id: u32| {
        with_app(|app| {
            let row = app
                .stack
                .last()
                .and_then(|s| {
                    s.actions
                        .iter()
                        .position(|a| matches!(a, R::Alarm(a) if a.id == id))
                })
                .expect("the alarm") as i32;
            app.focus = (zone::LIST, row);
            app.screen_hold(zone::LIST, row);
        })
    };
    with_app(|app| {
        let id = app.requests().expect("requests").alarms;
        let read = Ok((
            vec![alarm.clone(), eight.clone()],
            "2026-10-29 06:30:00".to_owned(),
            true,
        ));
        app.update(household::Update::Fetched(household::Fetched::Alarms {
            id,
            result: read,
        }));
    });
    hold(7);
    press(Action::Select);
    press(Action::Back);
    assert_eq!(step(), None);
    hold(8);
    with_app(|app| {
        app.update(household::Update::Fetched(
            household::Fetched::AlarmWritten {
                alarm: Some(7),
                result: Ok(()),
            },
        ))
    });
    let mut open = None;
    with_app(|app| open = app.alarm_draft().and_then(|d| d.editing));
    assert_eq!(open, Some(8), "alarm 8's editor stays open");
    press(Action::Back);
    with_app(|app| app.close_screen());

    // The item menu's Sonos favourites: a Browse favourite is removed on the
    // second press; a service's song is offered Add once known not to be one.
    with_app(|app| {
        use super::panels::PanelAction;
        use super::screens::RowAction;
        app.sent.borrow_mut().clear();
        app.open_screen(super::screens::Kind::Browse);
        let favorite: x2rock::sonos::proto::Favorite =
            serde_json::from_value(serde_json::json!({ "id": "4", "name": "Jazz" })).unwrap();
        let id = app.requests().expect("requests").favorites;
        app.update(household::Update::Fetched(household::Fetched::Favorites {
            id,
            result: Ok(vec![favorite]),
        }));
        let row = app
            .stack
            .last()
            .and_then(|s| {
                s.actions
                    .iter()
                    .position(|a| matches!(a, RowAction::Favorite(id, _) if id == "4"))
            })
            .expect("the favourite") as i32;
        app.screen_hold(zone::LIST, row);
        let remove = app
            .panel
            .as_ref()
            .and_then(|p| {
                p.actions
                    .iter()
                    .position(|a| *a == PanelAction::MenuRemoveFavorite)
            })
            .expect("Remove offered") as i32;
        app.panel_click(remove);
        assert!(app.panel.is_some(), "the first press only arms it");
        app.panel_click(remove);
        assert!(app.panel.is_none());

        let item = x2rock::sonos::smapi::Item {
            id: "track:9".into(),
            title: "Song".into(),
            item_type: "track".into(),
            summary: None,
            art_url: None,
            container: false,
        };
        app.open_item_menu(
            RowAction::ServiceItem {
                service: "160".into(),
                item: item.clone(),
            },
            "Song".into(),
        );
        let offered = |app: &App, want: PanelAction| {
            app.panel
                .as_ref()
                .is_some_and(|p| p.actions.contains(&want))
        };
        assert!(
            !offered(app, PanelAction::MenuAddFavorite),
            "nothing offered before it is known"
        );
        app.update(household::Update::Fetched(household::Fetched::IsFavorite {
            item_id: "track:9".into(),
            yes: false,
        }));
        assert!(offered(app, PanelAction::MenuAddFavorite));
        let add = app
            .panel
            .as_ref()
            .and_then(|p| {
                p.actions
                    .iter()
                    .position(|a| *a == PanelAction::MenuAddFavorite)
            })
            .unwrap() as i32;
        app.panel_click(add);
        app.close_screen();
    });
    let favorites_sent = sent();
    assert!(
        matches!(favorites_sent.as_slice(), [
            household::Command::RemoveFavorite { id, .. },
            household::Command::ServiceFavorite { add: true, .. },
        ] if id == "4"),
        "{favorites_sent:?}"
    );

    // A book's Chapters: the chapter it resumes at plays the book, which
    // carries on from the place; another plays from its own start.
    with_app(|app| {
        use super::panels::PanelAction;
        use super::screens::RowAction;
        let item = |id: &str, kind: &str| x2rock::sonos::smapi::Item {
            id: id.into(),
            title: id.into(),
            item_type: kind.into(),
            summary: None,
            art_url: None,
            container: false,
        };
        let book = item("book:1", "audiobook");
        app.open_screen(super::screens::Kind::Browse);
        app.open_item_menu(
            RowAction::ServiceItem {
                service: "239".into(),
                item: book.clone(),
            },
            "Dune".into(),
        );
        let chapters = app
            .panel
            .as_ref()
            .and_then(|p| {
                p.actions
                    .iter()
                    .position(|a| *a == PanelAction::MenuChapters)
            })
            .expect("Chapters offered") as i32;
        app.panel_click(chapters);
        let id = app.requests().expect("requests").page;
        let page = household::Fetched::ServicePage {
            id,
            service: "239".into(),
            container: "book:1".into(),
            index: 0,
            result: Ok((vec![item("ch:1", "track"), item("ch:2", "track")], 2)),
        };
        app.update(household::Update::Fetched(page));
        app.update(household::Update::Fetched(household::Fetched::Resume {
            book: "book:1".into(),
            chapter: Some("ch:2".into()),
        }));
        app.render_screen();
        let actions = app
            .stack
            .last()
            .map(|s| s.actions.clone())
            .unwrap_or_default();
        let plays = |a: &RowAction| match a {
            RowAction::ServiceItem { item, .. } => item.id.clone(),
            _ => String::new(),
        };
        assert_eq!(
            actions.iter().map(plays).collect::<Vec<_>>(),
            ["ch:1", "book:1"],
            "the resumed chapter plays the book"
        );
        assert_eq!(
            app.focus,
            (zone::LIST, 1),
            "it opens on the chapter it resumes at"
        );
        app.close_screen();
        app.close_screen();
    });

    // Left held from the pane walks to the room list and stops there: its
    // repeats never pick the room up, let alone confirm Play everywhere.
    with_app(|app| {
        app.sent.borrow_mut().clear();
        app.action(Action::Left, Phase::Press);
        for _ in 0..20 {
            app.action(Action::Left, Phase::Repeat);
        }
        app.action(Action::Left, Phase::Release);
    });
    assert_eq!(focus().0, zone::ROOMS);
    with_app(|app| moving = app.moving.is_some());
    assert!(
        !moving,
        "a Left held from the pane does not pick the room up"
    );
    assert!(sent().is_empty(), "nothing sent: {:?}", sent());

    // Held on the room itself it picks it up, and goes no further.
    with_app(|app| {
        app.action(Action::Left, Phase::Press);
        for _ in 0..20 {
            app.action(Action::Left, Phase::Repeat);
        }
        app.action(Action::Left, Phase::Release);
    });
    let mut carrying = false;
    with_app(|app| carrying = matches!(app.moving, Some(super::Moving::Carrying(_))));
    assert!(carrying, "a Left held on a room picks it up, and only that");
    assert!(sent().is_empty(), "nothing sent: {:?}", sent());
    press(Action::Back);

    held_volume_is_paced_capped_and_ends_on_release();

    // Out-of-order and stale responses: matched by request id, dropped after
    // reconnect, duplicate service page offsets ignored, and late search
    // sections from an earlier query with the same term ignored.
    //
    // These scenarios run sequentially on the Slint UI thread within this test
    // because the Slint platform backend can only be initialized once per process,
    // which prevents parallel test functions from creating window instances.
    test_same_search_query_twice_ignores_late_response();
    test_close_and_reopen_service_page_ignores_old_page();
    test_duplicate_page_offsets_dropped_and_loading_preserved();
    test_reconnect_invalidates_pre_reconnect_fetches_and_recovers_on_connected();
    test_screen_reads_of_different_kinds_do_not_void_each_other();
    test_reconnect_re_requests_pending_alarm_sounds();
    test_reconnect_preserves_loaded_service_page_without_refetch();
    test_reconnect_partially_answered_search_does_not_duplicate_sections();
    test_rejected_favorites_response_does_not_overwrite_cache();

    // Closing a list (or opening full screen) must release the model's cover
    // references too, otherwise evicting the cache cannot release those pixels.
    with_app(|app| {
        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(1, 1);
        let image = slint::Image::from_rgba8(buffer);
        for now_playing in [false, true] {
            app.stack.clear();
            if now_playing {
                app.open_screen(super::screens::Kind::NowPlaying);
            }
            app.models.list_rows.set(vec![crate::ListRow {
                icon: image.clone(),
                has_icon: true,
                ..Default::default()
            }]);
            ui.set_bar(crate::BarState {
                art: image.clone(),
                has_art: true,
                ..Default::default()
            });
            app.render_screen();
            assert_eq!(
                app.models.list_rows.model().row_count(),
                0,
                "hidden rows must release their cover images"
            );
            assert_eq!(
                ui.get_bar().art.size().width,
                0,
                "the hidden player bar must release its cover too"
            );
        }

        // A failed displayed cover schedules its own wakeup, even with no
        // playback ticking; taking it out of the model stops that wakeup.
        app.tick.stop();
        let now = std::time::Instant::now();
        let failed_key = super::ArtKey::new("synthetic-failed-cover", super::ArtSize::Full);
        app.art
            .failed(failed_key.clone(), now + std::time::Duration::from_secs(60));
        app.art.begin(super::ArtSurface::Pane);
        app.art.get(&failed_key, now);
        app.finish_art_render();
        assert!(
            app.art_retry.running(),
            "idle playback must not prevent art retries"
        );
        assert!(
            !app.tick.running(),
            "art retries must not depend on playback ticking"
        );
        // The deadline can pass between looking up a cover and scheduling
        // its timer. Slint must get a nonzero interval even at that boundary.
        app.art.failed(failed_key.clone(), now);
        app.art.begin(super::ArtSurface::Pane);
        app.art
            .get(&failed_key, now - std::time::Duration::from_nanos(1));
        app.finish_art_render();
        assert!(app.art_retry.interval() >= std::time::Duration::from_millis(1));
        app.art.begin(super::ArtSurface::Pane);
        app.finish_art_render();
        assert!(
            !app.art_retry.running(),
            "hidden failures must not keep waking the UI"
        );
    });
    drop(ui);
}

fn test_same_search_query_twice_ignores_late_response() {
    with_app(|app| {
        let dummy_section = |title: &str| crate::services::Section {
            service: crate::services::ServiceInfo {
                id: "svc1".into(),
                name: "Service 1".into(),
                linked: true,
            },
            result: Ok(vec![x2rock::sonos::smapi::Item {
                id: title.into(),
                title: title.into(),
                item_type: "track".into(),
                summary: None,
                art_url: None,
                container: false,
            }]),
        };

        app.open_screen(super::screens::Kind::Search);
        if let Some(s) = app.stack.last_mut() {
            s.query = "beatles".into();
        }
        app.submit_search();
        let id1 = app.requests().expect("search id 1").search;

        if let Some(s) = app.stack.last_mut() {
            s.query = "beatles".into();
        }
        app.submit_search();
        let id2 = app.requests().expect("search id 2").search;
        assert_ne!(id1, id2, "distinct request ids for two searches");

        // Late response from search 1 arrives:
        app.fetched(household::Fetched::SearchSection {
            id: id1,
            term: "beatles".into(),
            section: dummy_section("late track"),
        });
        assert_eq!(
            app.stack.last().map(|s| s.sections.len()),
            Some(0),
            "late section from first search ignored"
        );

        // Response from search 2 arrives:
        app.fetched(household::Fetched::SearchSection {
            id: id2,
            term: "beatles".into(),
            section: dummy_section("fresh track"),
        });
        assert_eq!(
            app.stack.last().map(|s| s.sections.len()),
            Some(1),
            "section for current search accepted"
        );
        app.close_screen();
    });
}

fn test_close_and_reopen_service_page_ignores_old_page() {
    with_app(|app| {
        let item = |id: &str| x2rock::sonos::smapi::Item {
            id: id.into(),
            title: id.into(),
            item_type: "track".into(),
            summary: None,
            art_url: None,
            container: false,
        };

        app.open_screen(super::screens::Kind::ServicePage {
            service: "svc1".into(),
            container: "top".into(),
            title: "Top".into(),
        });
        let page_id1 = app.requests().expect("page id 1").page;
        app.close_screen();

        app.open_screen(super::screens::Kind::ServicePage {
            service: "svc1".into(),
            container: "top".into(),
            title: "Top".into(),
        });
        let page_id2 = app.requests().expect("page id 2").page;
        assert_ne!(page_id1, page_id2);

        // Late response from first page arrives:
        app.fetched(household::Fetched::ServicePage {
            id: page_id1,
            service: "svc1".into(),
            container: "top".into(),
            index: 0,
            result: Ok((vec![item("t1"), item("t2")], 2)),
        });
        assert_eq!(
            app.stack.last().map(|s| s.page.len()),
            Some(0),
            "late response from closed page ignored"
        );

        // Response for second page arrives:
        app.fetched(household::Fetched::ServicePage {
            id: page_id2,
            service: "svc1".into(),
            container: "top".into(),
            index: 0,
            result: Ok((vec![item("t1"), item("t2")], 4)),
        });
        assert_eq!(
            app.stack.last().map(|s| s.page.len()),
            Some(2),
            "response for open page accepted"
        );
        app.close_screen();
    });
}

fn test_duplicate_page_offsets_dropped_and_loading_preserved() {
    with_app(|app| {
        let item = |id: &str| x2rock::sonos::smapi::Item {
            id: id.into(),
            title: id.into(),
            item_type: "track".into(),
            summary: None,
            art_url: None,
            container: false,
        };

        app.open_screen(super::screens::Kind::ServicePage {
            service: "svc1".into(),
            container: "top".into(),
            title: "Top".into(),
        });
        let page_id = app.requests().expect("page id").page;
        assert!(
            app.stack.last().is_some_and(|s| s.page_loading),
            "initially loading"
        );

        // First page (index 0) arrives:
        app.fetched(household::Fetched::ServicePage {
            id: page_id,
            service: "svc1".into(),
            container: "top".into(),
            index: 0,
            result: Ok((vec![item("t1"), item("t2")], 4)),
        });
        assert_eq!(app.stack.last().map(|s| s.page.len()), Some(2));
        assert_eq!(
            app.stack
                .last()
                .and_then(|s| s.page.first().map(|i| i.id.as_str())),
            Some("t1")
        );
        assert!(
            !app.stack.last().is_some_and(|s| s.page_loading),
            "loading finished after index 0"
        );

        // Trigger LoadMore:
        let next_page_id = app.next_request_id();
        if let Some(screen) = app.stack.last_mut() {
            screen.requests.page = Some(next_page_id);
            screen.page_loading = true;
        }
        app.send(household::Command::Fetch(household::Fetch::ServicePage {
            id: Some(next_page_id),
            service: "svc1".into(),
            container: "top".into(),
            index: 2,
        }));
        assert!(
            app.stack.last().is_some_and(|s| s.page_loading),
            "loading while load-more in flight"
        );

        // Duplicate index 0 arrives: ignored, original t1 preserved, page_loading remains true:
        app.fetched(household::Fetched::ServicePage {
            id: Some(next_page_id),
            service: "svc1".into(),
            container: "top".into(),
            index: 0,
            result: Ok((vec![item("t1_dup"), item("t2_dup")], 4)),
        });
        assert_eq!(
            app.stack.last().map(|s| s.page.len()),
            Some(2),
            "duplicate index 0 ignored"
        );
        assert_eq!(
            app.stack
                .last()
                .and_then(|s| s.page.first().map(|i| i.id.as_str())),
            Some("t1"),
            "original item preserved"
        );
        assert!(
            app.stack.last().is_some_and(|s| s.page_loading),
            "loading remains true while load-more is still pending"
        );

        // Valid continuation arrives (index 2 == page.len()):
        app.fetched(household::Fetched::ServicePage {
            id: Some(next_page_id),
            service: "svc1".into(),
            container: "top".into(),
            index: 2,
            result: Ok((vec![item("t3"), item("t4")], 4)),
        });
        assert_eq!(
            app.stack.last().map(|s| s.page.len()),
            Some(4),
            "valid continuation appended"
        );
        assert_eq!(
            app.stack
                .last()
                .and_then(|s| s.page.get(2).map(|i| i.id.as_str())),
            Some("t3")
        );
        assert!(
            !app.stack.last().is_some_and(|s| s.page_loading),
            "loading finished after continuation"
        );

        // Duplicate continuation arrives (index 2 < page.len()):
        app.fetched(household::Fetched::ServicePage {
            id: Some(next_page_id),
            service: "svc1".into(),
            container: "top".into(),
            index: 2,
            result: Ok((vec![item("t3_dup"), item("t4_dup")], 4)),
        });
        assert_eq!(
            app.stack.last().map(|s| s.page.len()),
            Some(4),
            "duplicate continuation ignored"
        );
        app.close_screen();
    });
}

fn test_reconnect_invalidates_pre_reconnect_fetches_and_recovers_on_connected() {
    with_app(|app| {
        app.open_screen(super::screens::Kind::Queue);
        let queue_id1 = app.requests().expect("requests").queue;
        assert!(queue_id1.is_some());
        let group = app.stack.last().map(|s| s.group.clone()).unwrap();

        // Connection drops: drives through Update::Status
        app.update(household::Update::Status(household::Status::Reconnecting(
            "network lost".into(),
        )));
        assert_eq!(
            app.requests().expect("requests").queue,
            None,
            "request id cleared on reconnecting status"
        );

        // Stale pre-reconnect response arrives:
        let qview = household::QueueView {
            items: vec![],
            total: 0,
            update_id: "u1".into(),
            current: None,
        };
        app.update(household::Update::Fetched(household::Fetched::Queue {
            id: queue_id1,
            group: group.clone(),
            result: Ok(qview.clone()),
        }));
        assert!(
            app.stack.last().and_then(|s| s.queue.as_ref()).is_none(),
            "stale queue response dropped"
        );

        // Connection restored:
        app.update(household::Update::Status(household::Status::Connected));
        let queue_id2 = app.requests().expect("requests").queue;
        assert!(queue_id2.is_some(), "re-requested on Connected");
        assert_ne!(
            queue_id1, queue_id2,
            "fresh request id allocated on reconnect recovery"
        );

        // Fresh response arrives:
        app.update(household::Update::Fetched(household::Fetched::Queue {
            id: queue_id2,
            group,
            result: Ok(qview),
        }));
        assert!(
            app.stack.last().and_then(|s| s.queue.as_ref()).is_some(),
            "queue populated on reconnect recovery"
        );
        app.close_screen();
    });
}

fn test_screen_reads_of_different_kinds_do_not_void_each_other() {
    with_app(|app| {
        // Alarms screen: alarm toggle does not void reading alarm sounds.
        app.open_screen(super::screens::Kind::Alarms);
        let alarms_read = Ok((vec![], "2026-10-29 06:30:00".to_owned(), true));
        let alarms_id = app.requests().expect("requests").alarms;
        app.update(household::Update::Fetched(household::Fetched::Alarms {
            id: alarms_id,
            result: alarms_read,
        }));

        let alarm = x2rock::sonos::upnp::Alarm {
            id: 5,
            start: "07:00:00".into(),
            duration: "01:00:00".into(),
            recurrence: "DAILY".into(),
            enabled: false,
            room_uuid: "RINCON_K1".into(),
            program_uri: household::CHIME.into(),
            program_metadata: String::new(),
            play_mode: "NORMAL".into(),
            volume: 20,
            include_linked_zones: false,
        };

        // Toggle alarm: sets requests.alarms to toggle_id:
        let toggle_id = app.next_request_id();
        if let Some(s) = app.stack.last_mut() {
            s.requests.alarms = Some(toggle_id);
        }
        app.send(household::Command::EditThenRead {
            edit: Box::new(household::Command::SetAlarmEnabled {
                alarm: alarm.clone(),
                enabled: true,
            }),
            read: household::Fetch::Alarms {
                id: Some(toggle_id),
            },
        });

        // While toggle is in flight, read alarm sounds (e.g. editor sound step):
        if let Some(s) = app.stack.last_mut() {
            s.alarm_sounds = None;
        }
        app.read_alarm_sounds();
        let sounds_id = app.requests().expect("requests").alarm_sounds;
        assert_eq!(
            app.requests().expect("requests").alarms,
            Some(toggle_id),
            "alarms toggle request id preserved"
        );
        assert!(sounds_id.is_some(), "alarm sounds got an id");

        // Both arrive in any order:
        app.update(household::Update::Fetched(
            household::Fetched::AlarmSounds {
                id: sounds_id,
                result: Ok(vec![]),
            },
        ));
        assert!(
            app.stack
                .last()
                .and_then(|s| s.alarm_sounds.as_ref())
                .is_some(),
            "alarm sounds accepted"
        );

        let mut enabled_alarm = alarm.clone();
        enabled_alarm.enabled = true;
        app.update(household::Update::Fetched(household::Fetched::Alarms {
            id: Some(toggle_id),
            result: Ok((vec![enabled_alarm], "2026-10-29 06:30:00".to_owned(), true)),
        }));
        assert!(
            app.stack.last().and_then(|s| s.alarms.as_ref()).is_some(),
            "alarms toggle answer accepted"
        );
        app.close_screen();
    });
}

fn test_reconnect_re_requests_pending_alarm_sounds() {
    with_app(|app| {
        app.open_screen(super::screens::Kind::Alarms);
        let alarm = x2rock::sonos::upnp::Alarm {
            id: 1,
            start: "07:00:00".into(),
            duration: "01:00:00".into(),
            recurrence: "DAILY".into(),
            enabled: true,
            room_uuid: "RINCON_1".into(),
            program_uri: household::CHIME.into(),
            program_metadata: String::new(),
            play_mode: "NORMAL".into(),
            volume: 25,
            include_linked_zones: false,
        };
        app.start_alarm_edit(&alarm);
        if let Some(s) = app.stack.last_mut() {
            s.alarm_sounds = None;
        }
        app.alarm_click(super::alarms::AlarmRow::Change(super::alarms::Step::Sound));
        let sound_id1 = app.requests().expect("requests").alarm_sounds;
        assert!(sound_id1.is_some(), "sound fetch initiated on Sound step");

        // Connection drops:
        app.update(household::Update::Status(household::Status::Reconnecting(
            "network drop".into(),
        )));
        assert_eq!(
            app.requests().expect("requests").alarm_sounds,
            None,
            "id cleared on reconnecting"
        );

        // Stale pre-reconnect response arrives:
        let program = household::AlarmProgram {
            title: "Stale Favourite".into(),
            uri: "x-rincon-cpcontainer:1".into(),
            metadata: String::new(),
        };
        app.update(household::Update::Fetched(
            household::Fetched::AlarmSounds {
                id: sound_id1,
                result: Ok(vec![program]),
            },
        ));
        assert!(
            app.stack
                .last()
                .and_then(|s| s.alarm_sounds.as_ref())
                .is_none(),
            "stale sound response ignored"
        );

        // Reconnect:
        app.update(household::Update::Status(household::Status::Connected));
        let sound_id2 = app.requests().expect("requests").alarm_sounds;
        assert!(
            sound_id2.is_some(),
            "alarm sounds re-requested on Connected"
        );
        assert_ne!(sound_id1, sound_id2, "fresh request id allocated");
        let sounds_sent = app.sent.borrow().iter().any(|cmd| matches!(cmd, household::Command::Fetch(household::Fetch::AlarmSounds { id: Some(id) }) if *id == sound_id2.unwrap()));
        assert!(sounds_sent, "new AlarmSounds fetch command sent to speaker");

        // Fresh response arrives:
        let fresh_program = household::AlarmProgram {
            title: "Fresh Favourite".into(),
            uri: "x-rincon-cpcontainer:2".into(),
            metadata: String::new(),
        };
        app.update(household::Update::Fetched(
            household::Fetched::AlarmSounds {
                id: sound_id2,
                result: Ok(vec![fresh_program]),
            },
        ));
        assert_eq!(
            app.stack
                .last()
                .and_then(|s| s.alarm_sounds.as_ref())
                .and_then(|r| r.as_ref().ok())
                .map(|l| l.len()),
            Some(1),
            "sounds populated after recovery"
        );
        app.close_screen();
    });
}

fn test_reconnect_preserves_loaded_service_page_without_refetch() {
    with_app(|app| {
        let item = |id: &str| x2rock::sonos::smapi::Item {
            id: id.into(),
            title: id.into(),
            item_type: "track".into(),
            summary: None,
            art_url: None,
            container: false,
        };

        app.open_screen(super::screens::Kind::ServicePage {
            service: "svc1".into(),
            container: "top".into(),
            title: "Top".into(),
        });
        let page_id = app.requests().expect("requests").page;

        // First page arrives:
        app.fetched(household::Fetched::ServicePage {
            id: page_id,
            service: "svc1".into(),
            container: "top".into(),
            index: 0,
            result: Ok((vec![item("t1"), item("t2")], 2)),
        });
        assert_eq!(app.stack.last().map(|s| s.page.len()), Some(2));
        assert!(
            app.requests().expect("requests").page.is_none(),
            "fetch completed"
        );

        // Clear sent commands:
        app.sent.borrow_mut().clear();

        // Disconnect and reconnect:
        app.update(household::Update::Status(household::Status::Reconnecting(
            "network drop".into(),
        )));
        assert_eq!(
            app.stack.last().map(|s| s.page.len()),
            Some(2),
            "items preserved while reconnecting"
        );

        app.update(household::Update::Status(household::Status::Connected));
        assert_eq!(
            app.stack.last().map(|s| s.page.len()),
            Some(2),
            "items preserved on connected"
        );
        assert!(
            app.requests().expect("requests").page.is_none(),
            "no fetch pending on connected"
        );

        let refetched = app.sent.borrow().iter().any(|cmd| {
            matches!(
                cmd,
                household::Command::Fetch(household::Fetch::ServicePage { .. })
            )
        });
        assert!(
            !refetched,
            "no ServicePage fetch sent on reconnect for already loaded page"
        );
        app.close_screen();
    });
}

fn test_reconnect_partially_answered_search_does_not_duplicate_sections() {
    with_app(|app| {
        app.open_screen(super::screens::Kind::Search);
        if let Some(s) = app.stack.last_mut() {
            s.query = "partial search".into();
        }
        app.submit_search();
        let old_id = app.requests().expect("requests").search;
        let section = |svc_id: &str, track: &str| crate::services::Section {
            service: crate::services::ServiceInfo {
                id: svc_id.into(),
                name: svc_id.into(),
                linked: true,
            },
            result: Ok(vec![x2rock::sonos::smapi::Item {
                id: track.into(),
                title: track.into(),
                item_type: "track".into(),
                summary: None,
                art_url: None,
                container: false,
            }]),
        };

        // Deliver one section before disconnect; SearchDone has not arrived:
        app.update(household::Update::Fetched(
            household::Fetched::SearchSection {
                id: old_id,
                term: "partial search".into(),
                section: section("svc1", "track1"),
            },
        ));
        assert_eq!(app.stack.last().map(|s| s.sections.len()), Some(1));

        // Connection drops and reconnects while search was in flight:
        app.update(household::Update::Status(household::Status::Reconnecting(
            "network drop".into(),
        )));
        app.update(household::Update::Status(household::Status::Connected));
        let fresh_id = app.requests().expect("requests").search;
        assert_ne!(old_id, fresh_id);

        // Restarted search delivers section for svc1 and then SearchDone:
        app.update(household::Update::Fetched(
            household::Fetched::SearchSection {
                id: fresh_id,
                term: "partial search".into(),
                section: section("svc1", "track1"),
            },
        ));
        app.update(household::Update::Fetched(household::Fetched::SearchDone {
            id: fresh_id,
            term: "partial search".into(),
        }));
        let sections = app.stack.last().map(|s| s.sections.len()).unwrap_or(0);
        assert_eq!(
            sections, 1,
            "reconnected search does not duplicate service sections"
        );
        app.close_screen();
    });
}

fn test_rejected_favorites_response_does_not_overwrite_cache() {
    with_app(|app| {
        let favorite = |id: &str| x2rock::sonos::proto::Favorite {
            id: id.into(),
            name: format!("Favorite {id}"),
            description: None,
            image_url: None,
            service: None,
            resource: None,
        };

        app.open_screen(super::screens::Kind::Browse);
        let old_id = app.requests().expect("requests").favorites;
        app.close_screen();

        app.open_screen(super::screens::Kind::Browse);
        let fresh_id = app.requests().expect("requests").favorites;
        assert_ne!(old_id, fresh_id);

        // Current request answers:
        app.update(household::Update::Fetched(household::Fetched::Favorites {
            id: fresh_id,
            result: Ok(vec![favorite("fresh-id")]),
        }));
        assert_eq!(
            app.favorites_cache.first().map(|f| f.id.as_str()),
            Some("fresh-id")
        );
        assert_eq!(
            app.stack
                .last()
                .and_then(|s| s.favorites.as_ref())
                .and_then(|r| r.as_ref().ok())
                .and_then(|l| l.first())
                .map(|f| f.id.as_str()),
            Some("fresh-id")
        );

        // Stale late response arrives:
        app.update(household::Update::Fetched(household::Fetched::Favorites {
            id: old_id,
            result: Ok(vec![favorite("stale-id")]),
        }));
        // Neither screen nor shared cache should be overwritten by rejected response:
        assert_eq!(
            app.favorites_cache.first().map(|f| f.id.as_str()),
            Some("fresh-id"),
            "rejected response does not overwrite shared favorites cache"
        );
        assert_eq!(
            app.stack
                .last()
                .and_then(|s| s.favorites.as_ref())
                .and_then(|r| r.as_ref().ok())
                .and_then(|l| l.first())
                .map(|f| f.id.as_str()),
            Some("fresh-id"),
            "rejected response does not overwrite Browse screen"
        );
        app.close_screen();
    });
}

/// Where the selected room's volume would end up after what was sent.
fn volume_reached(from: i32) -> i32 {
    sent().iter().fold(from, |level, c| match c {
        household::Command::SetGroupVolume { level, .. } => *level as i32,
        _ => level,
    })
}

/// A held LB, and a held `=` with the keyboard's own fast repeat: each hold
/// climbs no more than 20 however long it lasts, and stops on release.
fn held_volume_is_paced_capped_and_ends_on_release() {
    let mut from = 0;
    with_app(|app| {
        app.sent.borrow_mut().clear();
        let group = app.selected_entry().expect("a room").group_id.clone();
        from = app.live(&group).volume.expect("a level").volume as i32;
        app.action(Action::VolumeUp, Phase::Press);
        // The pad repeats after 400ms, then every 90ms: 1.5s held.
        app.skew_ms += 400;
        for _ in 0..12 {
            app.action(Action::VolumeUp, Phase::Repeat);
            app.skew_ms += 90;
        }
    });
    let held = volume_reached(from);
    assert!(
        held > from + 10 && held <= from + 20,
        "1.5s held from {from} reaches {held}: {:?}",
        sent()
    );
    with_app(|app| {
        for _ in 0..30 {
            app.action(Action::VolumeUp, Phase::Repeat);
            app.skew_ms += 90;
        }
        app.action(Action::VolumeUp, Phase::Release);
    });
    assert_eq!(volume_reached(from), from + 20, "4s held: {:?}", sent());
    assert!(
        sent()
            .iter()
            .all(|c| matches!(c, household::Command::SetGroupVolume { .. })),
        "levels, which replace one another in the lane: {:?}",
        sent()
    );

    // The keyboard: `=` auto-repeating every 30ms for 3s, after a pause.
    with_app(|app| {
        app.sent.borrow_mut().clear();
        app.skew_ms += 5_000;
        app.key("=", false, true);
        for _ in 0..100 {
            app.skew_ms += 30;
            app.key("=", true, true);
        }
        app.key("=", false, false);
    });
    let typed = sent().len();
    assert!(
        (2..=5).contains(&typed),
        "{typed} levels for a 3s key hold: {:?}",
        sent()
    );
    assert!(volume_reached(from) <= from + 20, "{:?}", sent());
}

#[test]
fn a_failed_plex_cover_does_not_log_its_token() {
    with_app(|app| {
        let url =
            "http://192.168.1.50:32400/photo/:/transcode?X-Plex-Token=supersecret123&width=300";
        let key = super::ArtKey::new(url, super::ArtSize::Full);
        let error = crate::art::LoadError::permanent(anyhow::anyhow!("raster size exceeds limit"));
        app.art_arrived(key, Err(error));
        let log = app.last_art_failure_log.as_deref().expect("failure logged");
        assert!(!log.contains("supersecret123"));
        assert!(log.contains("<token>"));
        assert_eq!(
            log,
            "x2rock-deck: no art for http://192.168.1.50:32400/photo/:/transcode?X-Plex-Token=<token>&width=300: raster size exceeds limit"
        );
    });
}

#[test]
fn readonly_command_logging_redacts_plex_token() {
    let item = x2rock::sonos::smapi::Item {
        id: "1".into(),
        title: "Track".into(),
        item_type: "track".into(),
        summary: None,
        art_url: Some("http://192.168.1.50:32400/photo/:/transcode?X-Plex-Token=secret456".into()),
        container: false,
    };
    let cmd = household::Command::ServiceFavorite {
        service: "Plex".into(),
        item,
        add: true,
    };
    let log = super::format_readonly_command(&cmd);
    assert!(!log.contains("secret456"));
    assert!(log.contains("<token>"));
}
