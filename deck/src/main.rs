// A window, not a console, on Windows: a release build opened from Explorer
// would otherwise bring a console window up beside it.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! x2rock for the Steam Deck: a Sonos controller in the look of x2rock for
//! Android TV, for gamepads, keyboards and touch.
//!
//!     x2rock-deck                 the household on this network
//!     x2rock-deck --demo          a household that is not there
//!     x2rock-deck --fullscreen    as Gaming Mode wants it (the default under Steam)
//!     x2rock-deck --screenshot out.png [--size 1280x800] [--theme OCEAN] [--keys right,down] [--live SECS]

mod app;
mod art;
/// What build.rs found to build in: a speaker salt, and the version that says so.
mod built_in {
    include!(concat!(env!("OUT_DIR"), "/built_in.rs"));
}
mod demo;
mod household;
mod input;
mod keyboard;
mod lanes;
mod logos;
mod model;
mod palette;
mod prefs;
mod report;
mod scale;
mod services;
mod shot;
mod steam;
mod sync;
mod theme;
mod update;

use slint::ComponentHandle;

use app::{App, with_app};
use input::{Action, Phase};

slint::include_modules!();

/// The window's callbacks, to the app on the UI thread.
pub fn wire(ui: &AppWindow) {
    ui.on_key(|text, repeat, pressed| {
        let mut used = false;
        with_app(|app| used = app.key(&text, repeat, pressed));
        used
    });
    ui.on_resized(|| with_app(|app| app.resized()));
    ui.on_volume_step(|d| with_app(|app| app.volume_step_tapped(d)));
    ui.on_speaker_step(|i, d| with_app(|app| app.speaker_step_tapped(i.max(0) as usize, d)));
    ui.on_seek_to(|f| with_app(|app| app.seek_to(f)));
    ui.on_panel_step(|i, d| with_app(|app| app.panel_step(i, d)));
    ui.on_dismiss(|| with_app(|app| app.dismiss()));
    let nav = ui.global::<Nav>();
    nav.on_tap(|z, i| with_app(|app| app.tap(z, i)));
    nav.on_hold(|z, i| with_app(|app| app.touch_hold(z, i)));
}

fn value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() -> anyhow::Result<()> {
    if let Some(salt) = built_in::SPEAKER_SALT {
        x2rock::salt::set_built_in(salt);
    }
    let args: Vec<String> = std::env::args().collect();

    if let Some(path) = value(&args, "--screenshot") {
        let (width, height) = value(&args, "--size")
            .and_then(|s| {
                s.split_once('x')
                    .map(|(w, h)| (w.parse().ok(), h.parse().ok()))
            })
            .and_then(|(w, h)| Some((w?, h?)))
            .unwrap_or((1920, 1080));
        return shot::run(shot::Options {
            path,
            width,
            height,
            theme: value(&args, "--theme")
                .unwrap_or_else(|| "DEFAULT".into())
                .to_uppercase(),
            keys: shot::parse_keys(&value(&args, "--keys").unwrap_or_default())?,
            live: value(&args, "--live")
                .map(|s| std::time::Duration::from_secs(s.parse().unwrap_or(10))),
        });
    }

    // Under Steam, give the library its artwork (once; Linux, where Steam keeps it there).
    if cfg!(target_os = "linux") {
        steam::place_artwork();
    }

    let demo = args.iter().any(|a| a == "--demo");
    let under_steam =
        std::env::var_os("SteamDeck").is_some() || std::env::var_os("SteamGameId").is_some();
    let fullscreen = args.iter().any(|a| a == "--fullscreen")
        || (under_steam && !args.iter().any(|a| a == "--windowed"));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        // The play and search routes are deep futures; 2 MiB is not enough for
        // them in a debug build.
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()?;
    let ui = AppWindow::new()?;
    // Only once a window has made the platform, and before it is shown: the
    // window's attributes are built when it first shows.
    slint::set_xdg_app_id("x2rock-deck")?;
    wire(&ui);
    let mut prefs = prefs::Prefs::load();
    if let Some(theme) = value(&args, "--theme") {
        prefs.theme = theme.to_uppercase();
    }
    app::install(App::new(&ui, prefs));

    if demo {
        with_app(|app| {
            for (url, cover) in demo::covers() {
                app.put_art(url, cover);
            }
            app.update(household::Update::Status(household::Status::Connected));
            app.update(household::Update::Household(Box::new(demo::household())));
        });
    } else {
        let handle = household::start(runtime.handle(), |update| {
            let _ = slint::invoke_from_event_loop(move || with_app(|app| app.update(update)));
        });
        // A scripted run (X2ROCK_DECK_KEYS) is read-only, as a live screenshot
        // is: keys pressed before the rooms arrive land somewhere else, and one
        // once pressed Play in a real household.
        if std::env::var_os("X2ROCK_DECK_KEYS").is_some() {
            with_app(|app| app.connect_readonly(handle, runtime.handle().clone()));
        } else {
            with_app(|app| app.connect(handle, runtime.handle().clone()));
        }
        // Once a launch, unless Settings says not to.
        let mut check = true;
        with_app(|app| check = app.checks_for_updates());
        if check {
            runtime.spawn(async {
                if let Ok(Some(version)) = update::newer_release().await {
                    let _ = slint::invoke_from_event_loop(move || {
                        with_app(|app| {
                            app.notice(&format!(
                                "x2rock-deck {version} is out: {}",
                                update::RELEASES
                            ))
                        })
                    });
                }
            });
        }
    }

    input::spawn_gamepads(|action: Action, phase: Phase| {
        let _ = slint::invoke_from_event_loop(move || {
            with_app(|app| {
                app.pad_action(action, phase);
            })
        });
    });

    if fullscreen {
        ui.window().set_fullscreen(true);
    }
    with_app(|app| app.render());
    // For checking the real renderer, which no screenshot tool may reach:
    // X2ROCK_DECK_SNAPSHOT=out.png saves the window after three seconds, and quits.
    let snapshot_timer = slint::Timer::default();
    // X2ROCK_DECK_TAP="x,y" taps there, in the TV app's dp, after two seconds:
    // the check that touch lands where it is drawn on a scaled canvas.
    let tap_timer = slint::Timer::default();
    if let Some((x, y)) = std::env::var("X2ROCK_DECK_TAP").ok().and_then(|t| {
        let (x, y) = t.split_once(',')?;
        Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
    }) {
        let weak = ui.as_weak();
        tap_timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_secs(2),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let zoom = ui.get_zoom();
                let position = slint::LogicalPosition::new(x * zoom, y * zoom);
                use slint::platform::{PointerEventButton, WindowEvent};
                let window = ui.window();
                window.dispatch_event(WindowEvent::PointerMoved { position });
                window.dispatch_event(WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                });
                window.dispatch_event(WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                });
            },
        );
    }
    // X2ROCK_DECK_KEYS="right,down,select" presses those after two seconds, a
    // third of a second apart, in a real window: with the snapshot below, the
    // check of what the real event loop does with them.
    let key_timer = slint::Timer::default();
    let script: Vec<shot::Step> = std::env::var("X2ROCK_DECK_KEYS")
        .ok()
        .map(|k| shot::parse_keys(&k))
        .transpose()?
        .unwrap_or_default();
    let script_len = script.len() as u64;
    if !script.is_empty() {
        let steps = std::rc::Rc::new(std::cell::RefCell::new(script.into_iter()));
        key_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(333),
            move || {
                // Not before the rooms are listed: keys pressed earlier land elsewhere.
                let mut ready = false;
                with_app(|app| ready = app.has_rooms());
                if !ready {
                    return;
                }
                match steps.borrow_mut().next() {
                    Some(shot::Step::Key(action)) => with_app(|app| {
                        app.action(action, Phase::Press);
                        app.action(action, Phase::Release);
                    }),
                    Some(shot::Step::Type(text)) => with_app(|app| {
                        for c in text.chars() {
                            app.key(&c.to_string(), false, true);
                        }
                    }),
                    None => {}
                }
            },
        );
    }
    if let Some(path) = std::env::var_os("X2ROCK_DECK_SNAPSHOT") {
        let weak = ui.as_weak();
        let wait = std::time::Duration::from_millis(3000 + script_len * 333 + 1500);
        snapshot_timer.start(slint::TimerMode::SingleShot, wait, move || {
            if let Some(ui) = weak.upgrade() {
                match ui.window().take_snapshot() {
                    Ok(shot) => {
                        let _ = image::save_buffer(
                            &path,
                            shot.as_bytes(),
                            shot.width(),
                            shot.height(),
                            image::ColorType::Rgba8,
                        );
                    }
                    Err(e) => eprintln!("x2rock-deck: no snapshot ({e})"),
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    ui.run()?;
    Ok(())
}
