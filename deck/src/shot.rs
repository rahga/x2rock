//! Screenshots with no display: the software renderer drawing into a buffer,
//! saved as a PNG. For checking the look against the TV app's own screenshots,
//! in CI and on a machine with no Sonos and no window to open.
//!
//!     x2rock-deck --screenshot home.png --size 1920x1080 --theme OCEAN --keys right,down

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::Rgb8Pixel;
use slint::platform::{EventLoopProxy, Platform, WindowAdapter, WindowEvent};
use slint::EventLoopError;
use slint::{ComponentHandle, PhysicalSize};

use crate::app::{self, App, with_app};
use crate::input::{Action, Phase};
use crate::prefs::Prefs;
use crate::{AppWindow, demo, scale};

pub type Queued = Box<dyn FnOnce() + Send>;

struct Headless {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
    queue: Arc<Mutex<Vec<Queued>>>,
}

/// `invoke_from_event_loop` with no event loop: queued, and run by [`pump`].
struct Proxy(Arc<Mutex<Vec<Queued>>>);

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), EventLoopError> {
        Ok(())
    }

    fn invoke_from_event_loop(&self, event: Queued) -> Result<(), EventLoopError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy(self.queue.clone())))
    }
}

/// The headless platform, installed: its window and its queue.
pub type HeadlessPlatform = (Rc<MinimalSoftwareWindow>, Arc<Mutex<Vec<Queued>>>);

/// Install the headless platform. Once a process.
pub fn install_headless() -> Result<HeadlessPlatform> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let queue: Arc<Mutex<Vec<Queued>>> = Arc::default();
    slint::platform::set_platform(Box::new(Headless { window: window.clone(), start: Instant::now(), queue: queue.clone() }))
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok((window, queue))
}

/// Run what has been queued for the UI thread, and the timers.
pub fn pump(queue: &Arc<Mutex<Vec<Queued>>>) {
    let queued: Vec<Queued> = std::mem::take(&mut *queue.lock().unwrap());
    for f in queued {
        f();
    }
    slint::platform::update_timers_and_animations();
}

pub struct Options {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub theme: String,
    pub keys: Vec<Step>,
    /// Render the household on this network instead of the demo's, waiting up
    /// to this long for it. Read-only: nothing is sent to the speakers.
    pub live: Option<Duration>,
}

/// One scripted step: a key, or text typed as a keyboard would.
pub enum Step {
    Key(Action),
    Type(String),
}

pub fn parse_keys(list: &str) -> Result<Vec<Step>> {
    list.split(',')
        .filter(|k| !k.is_empty())
        .map(|k| {
            if let Some(text) = k.trim().strip_prefix("type=") {
                return Ok(Step::Type(text.to_owned()));
            }
            if k.trim() == "return" {
                return Ok(Step::Type("\n".to_owned()));
            }
            Ok(Step::Key(match k.trim().to_lowercase().as_str() {
                "up" => Action::Up,
                "down" => Action::Down,
                "left" => Action::Left,
                "right" => Action::Right,
                "select" | "enter" => Action::Select,
                "back" => Action::Back,
                "menu" => Action::Menu,
                other => anyhow::bail!("unknown key {other}"),
            }))
        })
        .collect()
}

pub fn run(options: Options) -> Result<()> {
    let (window, queue) = install_headless()?;

    let ui = AppWindow::new()?;
    crate::wire(&ui);
    app::install(App::new(&ui, Prefs::ephemeral(&options.theme)));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        // The play and search routes are deep futures; 2 MiB is not enough for
        // them in a debug build.
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()?;
    match options.live {
        None => with_app(|app| {
            for (url, cover) in demo::covers() {
                app.put_art(url, cover);
            }
            app.update(crate::household::Update::Status(crate::household::Status::Connected));
            app.update(crate::household::Update::Household(Box::new(demo::household())));
        }),
        Some(wait) => {
            // The household's own runtime, read-only: lists are fetched, nothing
            // that changes the household is sent.
            let handle = crate::household::start(runtime.handle(), |update| {
                let _ = slint::invoke_from_event_loop(move || with_app(|app| app.update(update)));
            });
            with_app(|app| app.connect_readonly(handle, runtime.handle().clone()));
            let until = Instant::now() + wait;
            while Instant::now() < until {
                pump(&queue);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    window.set_size(PhysicalSize::new(options.width, options.height));
    if std::env::var_os("X2ROCK_SHOT_NATIVE").is_none() {
        window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale::factor_for(options.width, options.height) });
    }
    ui.show()?;
    for step in &options.keys {
        match step {
            Step::Key(action) => with_app(|app| {
                app.action(*action, Phase::Press);
                app.action(*action, Phase::Release);
            }),
            Step::Type(text) => with_app(|app| {
                for c in text.chars() {
                    app.key(&c.to_string(), false, true);
                }
            }),
        }
        pump(&queue);
        // Live, a key can open a screen whose list comes from the network.
        if options.live.is_some() {
            for _ in 0..30 {
                std::thread::sleep(Duration::from_millis(50));
                pump(&queue);
            }
        }
    }
    if std::env::var_os("X2ROCK_DECK_DEBUG").is_some() {
        with_app(|app| eprintln!("x2rock-deck: {}", app.focus_report()));
    }
    // Let animations settle, and any art or list arrive: the pane's tint takes
    // 600ms, a list from the internet longer.
    let settle = if options.live.is_some() { 120 } else { 20 };
    for _ in 0..settle {
        pump(&queue);
        std::thread::sleep(Duration::from_millis(50));
    }

    let (w, h) = (options.width as usize, options.height as usize);
    let mut buffer = vec![Rgb8Pixel::default(); w * h];
    // Twice: content shown conditionally is made at the first draw, and what
    // it does when made (a focused row scrolling into view) runs on its timers
    // before the second, as a real window's next frame.
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, w);
    });
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(20));
        pump(&queue);
    }
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, w);
    });
    let bytes: Vec<u8> = buffer.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    image::save_buffer(&options.path, &bytes, options.width, options.height, image::ColorType::Rgb8)
        .with_context(|| format!("writing {}", options.path))?;
    ui.hide()?;
    Ok(())
}
