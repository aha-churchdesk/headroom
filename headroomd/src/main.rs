//! headroomd — the Headroom menu bar app, with the queue daemon inside it.
//!
//! By default it runs the daemon on a background thread and the menu bar on
//! the main one. If a daemon is already running (a headless one, or another
//! login), the menu bar attaches to it as a client instead. `--headless`
//! runs only the daemon; `--probe` prints what the sampler sees and exits.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use headroom_core::store::Store;
use headroomd::app::{App, Next};
use headroomd::{daemon, sample::Sampler, shell, single, view};
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tao::window::WindowBuilder;
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use wry::WebViewBuilder;

const POPOVER_W: f64 = 400.0;
const POPOVER_H: f64 = 600.0;
const GLYPH: u32 = 18;

/// How often the queue is listed (tray title and an open popover), and how
/// often the slower things are looked at while the popover is open.
const LIST_EVERY: Duration = Duration::from_secs(1);
const SETTINGS_EVERY: Duration = Duration::from_secs(3);
const HOOKS_EVERY: Duration = Duration::from_secs(5);
/// How often the memory and CPU graphs are saved; a crash loses at most this.
const TREND_EVERY: Duration = Duration::from_secs(300);

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn probe() {
    let s = Sampler::new().system();
    let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
    println!("pressure   {:?}", s.pressure);
    println!(
        "memory     {:.1} / {:.1} GB used, {:.1} GB available",
        gb(s.mem_used),
        gb(s.mem_total),
        gb(s.mem_available)
    );
    println!("swap       {:.1} GB", gb(s.swap_used));
    println!("load       {:.1} on {} cores", s.load1, s.cores);
}

fn open_store() -> Store {
    match Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("headroomd: can't open the data directory: {e}");
            std::process::exit(1);
        }
    }
}

fn install_signals() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
}

/// The daemon's own stop flag, set from the signal handler's static. A
/// separate Arc so tests can stop a daemon they started in-process.
fn stop_on_signal() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let watch = stop.clone();
    std::thread::spawn(move || {
        while !STOP.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        watch.store(true, Ordering::Relaxed);
    });
    stop
}

fn headless() {
    let store = open_store();
    install_signals();
    if let Err(e) = daemon::run(store, stop_on_signal()) {
        eprintln!("headroomd: {e}");
        std::process::exit(1);
    }
}

fn activate_app() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    if let Some(mtm) = MainThreadMarker::new() {
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    }
}

fn glyph(filled: u32, ink: Option<[u8; 3]>) -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(shell::memory_glyph_rgba(GLYPH, filled, ink), GLYPH, GLYPH)
        .expect("icon")
}

enum UserEvent {
    Ipc(String),
    Tray(TrayIconEvent),
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn main() -> wry::Result<()> {
    if std::env::args().any(|a| a == "--probe") {
        probe();
        return Ok(());
    }
    if std::env::args().any(|a| a == "--headless") {
        headless();
        return Ok(());
    }

    let store = open_store();
    // One menu bar. The daemon has its own lock, so this app can still sit
    // beside a headless daemon and drive it.
    let Some(_ui_lock) = single::acquire_named(store.root(), "ui.lock") else {
        eprintln!("headroomd: the menu bar is already running");
        return Ok(());
    };
    install_signals();
    let stop = stop_on_signal();
    let attached = Arc::new(AtomicBool::new(false));
    let daemon_thread = {
        let (stop, root, attached) = (stop.clone(), store.root().to_path_buf(), attached.clone());
        std::thread::spawn(
            move || match Store::at(root).and_then(|s| daemon::run(s, stop)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    attached.store(true, Ordering::Relaxed);
                    eprintln!("headroomd: a daemon is already running; showing its queue");
                }
                Err(e) => eprintln!("headroomd: daemon stopped: {e}"),
            },
        )
    };

    let exe = std::env::current_exe().unwrap_or_default();
    let cli = exe.with_file_name("headroom");
    let app = Rc::new(RefCell::new(App::new(
        store,
        home(),
        cli.to_string_lossy().into_owned(),
        exe.to_string_lossy().into_owned(),
    )));

    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    // No Dock icon and no app-switcher entry: this lives in the menu bar.
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let window = WindowBuilder::new()
        .with_title("Headroom")
        .with_decorations(false)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_visible(false)
        .with_inner_size(LogicalSize::new(POPOVER_W, POPOVER_H))
        .build(&event_loop)
        .expect("window");

    // Page intents and tray clicks wake the loop through its proxy, so the
    // loop can sleep a whole second between listings and still answer a
    // click at once.
    let proxy = Mutex::new(event_loop.create_proxy());
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = proxy
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send_event(UserEvent::Tray(e));
    }));
    let ipc_proxy = event_loop.create_proxy();
    let webview = WebViewBuilder::new()
        .with_html(include_str!("../../ui/index.html"))
        .with_ipc_handler(move |req| {
            let _ = ipc_proxy.send_event(UserEvent::Ipc(req.body().to_string()));
        })
        .build(&window)?;

    let tray = TrayIconBuilder::new()
        .with_icon(glyph(0, None))
        .with_icon_as_template(true)
        .with_tooltip("Headroom")
        .build()
        .expect("tray icon");

    let push = {
        let app = app.clone();
        move |wv: &wry::WebView| {
            if let Some(s) = app.borrow().state() {
                let _ = wv.evaluate_script(&format!("window.__setState({s})"));
            }
        }
    };
    // The icon is redrawn only when it changes; the title and tooltip are cheap.
    let mut last_icon: Option<(u32, Option<[u8; 3]>)> = None;
    let mut update_tray = {
        let app = app.clone();
        move || {
            let a = app.borrow();
            let Some(l) = a.listing() else { return };
            let (segs, ink, title, tip) = view::tray(l);
            if last_icon != Some((segs, ink)) {
                let _ = tray.set_icon_with_as_template(Some(glyph(segs, ink)), ink.is_none());
                last_icon = Some((segs, ink));
            }
            tray.set_title(Some(title));
            let _ = tray.set_tooltip(Some(tip));
        }
    };

    let mut visible = false;
    let mut last_scale: Option<f64> = None;
    // Showing a window causes focus churn; ignore focus loss for a moment.
    let mut shown_at = Instant::now();
    let mut clicks = shell::TrayClicks::default();
    let mut next_list = Instant::now();
    let mut next_settings = Instant::now();
    let mut next_hooks = Instant::now();
    let mut next_trend = Instant::now() + TREND_EVERY;
    let mut daemon_thread = Some(daemon_thread);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(next_list.min(Instant::now() + LIST_EVERY));

        let mut quit = STOP.load(Ordering::Relaxed);
        match event {
            Event::NewEvents(StartCause::Init) => {
                app.borrow_mut().poll();
                update_tray();
            }
            Event::WindowEvent {
                event: WindowEvent::Focused(false),
                ..
            } => {
                if shell::should_dismiss(visible, shown_at.elapsed()) {
                    window.set_visible(false);
                    visible = false;
                    clicks.dismissed(Instant::now());
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                window.set_visible(false);
                visible = false;
            }
            Event::UserEvent(UserEvent::Ipc(body)) => {
                let mut a = app.borrow_mut();
                if a.command(&body) == Next::Quit {
                    quit = true;
                }
                if let Some(e) = a.last_error.take() {
                    eprintln!("headroomd: {e}");
                }
                drop(a);
                push(&webview);
                update_tray();
            }
            // tray-icon reports a click on press and on release; acting on
            // both would open and close the popover in one click.
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click {
                rect,
                button,
                button_state,
                ..
            })) => {
                let btn = match button {
                    MouseButton::Left => shell::Button::Left,
                    MouseButton::Right => shell::Button::Right,
                    _ => shell::Button::Middle,
                };
                let st = match button_state {
                    MouseButtonState::Up => shell::ButtonState::Up,
                    MouseButtonState::Down => shell::ButtonState::Down,
                };
                if let Some(toggle) = clicks.click(btn, st, visible, Instant::now()) {
                    visible = toggle == shell::Toggle::Show;
                    if visible {
                        // Clamp to the screen the icon is on (not the one
                        // the window was last on), in physical pixels.
                        let screens: Vec<shell::Screen> = window
                            .available_monitors()
                            .map(|m| {
                                let (p, sz) = (m.position(), m.size());
                                shell::Screen {
                                    x: p.x as f64,
                                    y: p.y as f64,
                                    w: sz.width as f64,
                                    h: sz.height as f64,
                                    scale: m.scale_factor(),
                                }
                            })
                            .collect();
                        let (tx, ty) = (rect.position.x, rect.position.y);
                        let (tw, th) = (rect.size.width as f64, rect.size.height as f64);
                        let screen =
                            shell::screen_for(&screens, tx, ty, tw, th).unwrap_or(shell::Screen {
                                x: 0.0,
                                y: 0.0,
                                w: 0.0,
                                h: 0.0,
                                scale: 1.0,
                            });
                        let (pw, ph) = (POPOVER_W * screen.scale, POPOVER_H * screen.scale);
                        let (x, y) = shell::popover_origin(
                            tx, ty, tw, th, pw, ph, screen.x, screen.y, screen.w, screen.h,
                        );
                        window.set_outer_position(PhysicalPosition::new(x, y));
                        if last_scale != Some(screen.scale) {
                            window.set_inner_size(LogicalSize::new(POPOVER_W, POPOVER_H));
                            last_scale = Some(screen.scale);
                        }
                        {
                            let mut a = app.borrow_mut();
                            a.poll();
                            a.refresh_settings();
                            a.refresh_others();
                            a.refresh_hooks();
                        }
                        push(&webview);
                        window.set_visible(true);
                        activate_app();
                        window.set_focus();
                        shown_at = Instant::now();
                    } else {
                        window.set_visible(false);
                    }
                }
            }
            _ => {}
        }

        let now = Instant::now();
        if now >= next_list {
            next_list = now + LIST_EVERY;
            app.borrow_mut()
                .set_attached(attached.load(Ordering::Relaxed));
            app.borrow_mut().poll();
            update_tray();
            if now >= next_trend {
                next_trend = now + TREND_EVERY;
                app.borrow().save_trend();
            }
            if visible {
                if now >= next_settings {
                    next_settings = now + SETTINGS_EVERY;
                    let mut a = app.borrow_mut();
                    a.refresh_settings();
                    a.refresh_others();
                }
                if now >= next_hooks {
                    next_hooks = now + HOOKS_EVERY;
                    app.borrow_mut().refresh_hooks();
                }
                push(&webview);
            }
            if let Some(e) = app.borrow_mut().last_error.take() {
                // The daemon being away is the normal state for a second
                // at start-up; anything else is worth a line.
                if e != "headroomd isn't running" {
                    eprintln!("headroomd: {e}");
                }
            }
        }

        if quit {
            app.borrow().save_trend();
            // Stop our daemon (if we run one) so it removes its socket.
            stop.store(true, Ordering::Relaxed);
            if let Some(t) = daemon_thread.take() {
                let _ = t.join();
            }
            *control_flow = ControlFlow::Exit;
        }
    });
}
