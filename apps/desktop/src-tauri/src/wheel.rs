//! Wires the [`WheelController`] to the OS and the overlay window.
//!
//! - The drag monitor from `grape-platform` sends [`DragSignal`]s.
//! - On Windows, Grape's own drop target on the overlay sends
//!   [`DropEvent`]s (DECISIONS D13). wry's handler is replaced; if it ever
//!   reports an event anyway, that's logged as a gap in coverage.
//! - On macOS, wry's drop handler is the drop target (it answers Copy).
//!   Its window-relative positions are offset by the overlay's frame to
//!   become [`DropEvent`]s in global points (DECISIONS D18).
//! - Both feed one channel. A controller thread handles each input and
//!   applies the [`Effect`]s: overlay frame and visibility, the
//!   `wheel://open`, `wheel://highlight` and `wheel://close` events
//!   ([`crate::events`]), and run requests: file actions in
//!   [`crate::actions`], a v1 panel tool's panel in [`crate::panel`], and
//!   conversions and the other tools in [`crate::jobs`].
//! - The overlay calls `wheel_painted` once a wheel is on screen, for the
//!   latency log.
//! - Settings reach the controller as [`Input::Configure`] (keys, sounds)
//!   and a shared haptics flag read on every highlight
//!   ([`crate::settings`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Instant;

use grape_core::{SelectionItem, WheelEntry};
use grape_platform::{DisplayBounds, DragMonitor, DragSignal, DropEvent, PlatformError, Rect};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
#[cfg(not(target_os = "macos"))]
use tauri::{PhysicalPosition, PhysicalSize};

use crate::actions;
use crate::controller::{ControllerConfig, Effect, Env, Input, RunRequest, WheelController};
use crate::jobs;
use crate::panel::{self, model::Route};
use crate::ui_windows::OVERLAY;

/// Inputs buffered for the controller thread.
const INPUT_CAPACITY: usize = 1024;
/// Signals or drop events buffered before their forwarder.
const SOURCE_CAPACITY: usize = 256;

/// Sends inputs to the controller. Managed by Tauri, so the tray and the
/// `wheel_painted` command can reach it.
#[derive(Debug, Clone)]
pub struct WheelInputs(SyncSender<Input>);

impl WheelInputs {
    /// Queues an input; drops it if the controller is far behind.
    pub fn send(&self, input: Input) {
        if self.0.try_send(input).is_err() {
            log::warn!("wheel controller queue full or gone; an input was dropped");
        }
    }
}

/// The running drag monitor, stopped when the app exits.
struct Monitor(Mutex<Option<Box<dyn DragMonitor>>>);

/// Where the overlay's drop target sends events. Kept so the target can be
/// re-registered before each show (Windows) or so wry's drop events can be
/// passed on (macOS).
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
#[derive(Clone)]
struct DropSink(SyncSender<DropEvent>);

/// Where the overlay was last framed, in global points. wry reports drop
/// positions relative to the window on macOS.
#[cfg(target_os = "macos")]
#[derive(Default)]
struct OverlayFrame(Mutex<Option<Rect>>);

/// The real OS for the controller: display bounds from `grape-platform`,
/// and the filesystem to tell folders from files.
struct OsEnv;

impl Env for OsEnv {
    fn display_at(&self, x: i32, y: i32) -> Option<DisplayBounds> {
        match grape_platform::display_bounds_at(x, y) {
            Ok(bounds) => Some(bounds),
            Err(err) => {
                log::warn!("no display bounds at ({x}, {y}): {err}");
                None
            }
        }
    }

    fn classify(&self, paths: &[PathBuf]) -> Vec<SelectionItem> {
        paths
            .iter()
            .map(|path| {
                // Only metadata is read, never contents. A path that can't
                // be read counts as a file; planning reports it later.
                if path.is_dir() {
                    SelectionItem::folder(path.clone())
                } else {
                    SelectionItem::file(path.clone())
                }
            })
            .collect()
    }
}

/// Starts drag detection and the controller with `config` (the user's
/// keys and sounds). `haptics` is the user's haptics setting, read on every
/// highlight. Call from `setup`, on the main thread (it registers the
/// overlay's drop target there). Nothing in here is fatal: without a
/// monitor Grape just never opens the wheel.
pub fn start(app: &AppHandle, config: ControllerConfig, haptics: Arc<AtomicBool>) {
    let (tx, rx) = mpsc::sync_channel(INPUT_CAPACITY);
    app.manage(WheelInputs(tx.clone()));

    app.manage(Monitor(Mutex::new(start_monitor(&tx))));

    #[cfg(target_os = "macos")]
    app.manage(OverlayFrame::default());

    let (drop_tx, drop_rx) = mpsc::sync_channel(SOURCE_CAPACITY);
    let drop_sink = DropSink(drop_tx);
    forward(drop_rx, tx, Input::Drop, "grape-drop-forward");
    install_drop_target(app, &drop_sink);
    app.manage(drop_sink);

    let app = app.clone();
    let spawned = thread::Builder::new()
        .name("grape-wheel".to_owned())
        .spawn(move || run_controller(&app, &rx, config, &haptics));
    if let Err(err) = spawned {
        log::error!("could not start the wheel controller thread: {err}");
    }
}

/// Stops the drag monitor (unhooks the mouse). Call on exit.
pub fn stop(app: &AppHandle) {
    if let Some(monitor) = app.try_state::<Monitor>() {
        let mut monitor = monitor.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(mut monitor) = monitor.take() {
            monitor.stop();
            log::info!("drag monitor stopped");
        }
    }
}

/// The overlay reports that wheel `seq` is on screen.
#[tauri::command]
pub fn wheel_painted(seq: u64, inputs: State<'_, WheelInputs>) {
    inputs.send(Input::Painted { seq });
}

fn start_monitor(tx: &SyncSender<Input>) -> Option<Box<dyn DragMonitor>> {
    let mut monitor = match grape_platform::drag_monitor() {
        Ok(monitor) => monitor,
        Err(PlatformError::Unsupported) => {
            log::warn!("drag detection isn't available on this OS yet; the wheel won't open");
            return None;
        }
        Err(err) => {
            log::error!("could not create the drag monitor: {err}");
            return None;
        }
    };
    let (signal_tx, signal_rx) = mpsc::sync_channel::<DragSignal>(SOURCE_CAPACITY);
    if let Err(err) = monitor.start(signal_tx) {
        log::error!("could not start the drag monitor: {err}");
        return None;
    }
    forward(signal_rx, tx.clone(), Input::Signal, "grape-signal-forward");
    log::info!("drag monitor running");
    Some(monitor)
}

/// Moves everything from `rx` into the controller's queue on its own
/// thread, until `rx` closes.
fn forward<T: Send + 'static>(
    rx: Receiver<T>,
    tx: SyncSender<Input>,
    wrap: fn(T) -> Input,
    name: &str,
) {
    let spawned = thread::Builder::new().name(name.to_owned()).spawn(move || {
        for item in rx {
            if tx.send(wrap(item)).is_err() {
                break;
            }
        }
    });
    if let Err(err) = spawned {
        log::error!("could not start {name}: {err}");
    }
}

fn run_controller(
    app: &AppHandle,
    rx: &Receiver<Input>,
    config: ControllerConfig,
    haptics: &AtomicBool,
) {
    let mut controller = WheelController::new(OsEnv, config);
    loop {
        let input = match controller.next_deadline() {
            Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let now = Instant::now();
        let effects = match input {
            Ok(input) => controller.handle(input, now),
            Err(RecvTimeoutError::Timeout) => controller.tick(now),
            Err(RecvTimeoutError::Disconnected) => break,
        };
        for effect in effects {
            apply(app, effect, haptics);
        }
    }
}

fn apply(app: &AppHandle, effect: Effect, haptics: &AtomicBool) {
    match effect {
        Effect::Open(open) => emit(app, "wheel://open", open),
        Effect::ShowOverlay { monitor } => show_overlay(app, monitor),
        Effect::Highlight(highlight) => {
            // A light trackpad tick on macOS; nothing elsewhere. It has
            // its own setting, whatever the sounds setting says.
            if highlight.index.is_some() && haptics.load(Ordering::Acquire) {
                grape_platform::haptic_tick();
            }
            emit(app, "wheel://highlight", highlight);
        }
        Effect::Close(close) => emit(app, "wheel://close", close),
        Effect::HideOverlay => {
            if let Some(overlay) = overlay(app) {
                if let Err(err) = overlay.hide() {
                    log::error!("could not hide the overlay: {err}");
                }
            }
        }
        Effect::Run(request) => run(app, &request),
        Effect::Log(line) => log::info!("{line}"),
    }
}

/// Runs a drop: file actions in [`actions`], a v1 panel tool this build
/// can run opens its panel ([`panel`]), and conversions and the other
/// tools go to [`jobs`]. The wheel is done either way.
fn run(app: &AppHandle, request: &RunRequest) {
    log::info!(
        "wheel {}: run {:?} on {} item(s)",
        request.seq,
        request.entry,
        request.items.len()
    );
    match request.entry {
        WheelEntry::Action(action) => {
            actions::run(app, action, &request.items, request.anchor, request.display)
        }
        entry => match panel::route_for(app, entry, &request.items) {
            Route::Panel(tool) => panel::open(app, tool, request),
            Route::Direct => {
                jobs::submit(app, entry, &request.items, request.anchor, request.display)
            }
        },
    }
}

/// Frames the overlay on the monitor and shows it. The wheel was already
/// sent, so it's drawn by the time the window appears. Never focuses it:
/// the app under the cursor keeps the keyboard.
#[cfg(not(target_os = "macos"))]
fn show_overlay(app: &AppHandle, frame: Rect) {
    let Some(overlay) = overlay(app) else {
        return;
    };
    let began = Instant::now();
    // Re-register first: WebView2 may have created child windows since.
    if let Some(sink) = app.try_state::<DropSink>() {
        install_drop_target(app, &sink);
    }
    // Position, size, position: moving to a monitor with another DPI makes
    // Windows rescale the window (WM_DPICHANGED), so the size is set after
    // the move, and the position again in case the rescale shifted it.
    let position = PhysicalPosition::new(frame.x, frame.y);
    let framed = overlay
        .set_position(position)
        .and_then(|()| {
            overlay.set_size(PhysicalSize::new(
                frame.width.unsigned_abs(),
                frame.height.unsigned_abs(),
            ))
        })
        .and_then(|()| overlay.set_position(position));
    if let Err(err) = framed {
        log::error!("could not frame the overlay on the monitor: {err}");
    }
    if let Err(err) = overlay.show() {
        log::error!("could not show the overlay: {err}");
        return;
    }
    #[cfg(target_os = "windows")]
    raise_overlay(app, overlay);
    log::info!(
        "overlay shown in {:.1} ms",
        began.elapsed().as_secs_f64() * 1000.0
    );
}

/// Lifts the overlay to the top of the topmost band, on the main thread
/// after the show (FIFO), so an open tool panel or the float shelf, both
/// topmost too, never covers the wheel. Every show: one `SetWindowPos`.
#[cfg(target_os = "windows")]
fn raise_overlay(app: &AppHandle, overlay: WebviewWindow) {
    let queued = app.run_on_main_thread(move || {
        let raised = overlay
            .hwnd()
            .map_err(|err| err.to_string())
            .and_then(|hwnd| {
                grape_platform::windows::raise_topmost(hwnd.0 as isize)
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = raised {
            log::error!("could not raise the overlay over the topmost windows: {err}");
        }
    });
    if let Err(err) = queued {
        log::error!("could not raise the overlay: no main thread: {err}");
    }
}

/// Frames the overlay on the monitor (in points) and orders it to the
/// front in one main-thread call. `orderFrontRegardless` brings it forward
/// although Grape isn't the active app; `focusable: false` keeps it from
/// ever becoming key, so the app under the cursor keeps the keyboard.
#[cfg(target_os = "macos")]
fn show_overlay(app: &AppHandle, frame: Rect) {
    let Some(overlay) = overlay(app) else {
        return;
    };
    if let Some(last) = app.try_state::<OverlayFrame>() {
        *last.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(frame);
    }
    let began = Instant::now();
    let queued = app.run_on_main_thread(move || {
        let ns_window = match overlay.ns_window() {
            Ok(ns_window) => ns_window,
            Err(err) => {
                log::error!("could not show the overlay: no NSWindow: {err}");
                return;
            }
        };
        // SAFETY: this runs on the main thread, and `ns_window` is the
        // overlay's NSWindow, which Tauri keeps alive while `overlay` is.
        match unsafe { grape_platform::macos::frame_and_show(ns_window, frame) } {
            Ok(()) => log::info!(
                "overlay shown in {:.1} ms",
                began.elapsed().as_secs_f64() * 1000.0
            ),
            Err(err) => log::error!("could not show the overlay: {err}"),
        }
    });
    if let Err(err) = queued {
        log::error!("could not show the overlay: no main thread: {err}");
    }
}

fn overlay(app: &AppHandle) -> Option<WebviewWindow> {
    let overlay = app.get_webview_window(OVERLAY);
    if overlay.is_none() {
        log::error!("the overlay window is missing");
    }
    overlay
}

fn emit<P: Serialize + Clone>(app: &AppHandle, event: &str, payload: P) {
    if let Err(err) = app.emit_to(OVERLAY, event, payload) {
        log::error!("could not send {event} to the overlay: {err}");
    }
}

/// Registers Grape's drop target on the overlay (Windows only), on the main
/// thread that owns the window. Logs how many of its windows are covered.
fn install_drop_target(app: &AppHandle, sink: &DropSink) {
    #[cfg(target_os = "windows")]
    {
        let Some(overlay) = overlay(app) else {
            return;
        };
        let sink = sink.0.clone();
        let queued = app.run_on_main_thread(move || {
            let hwnd = match overlay.hwnd() {
                Ok(hwnd) => hwnd.0 as isize,
                Err(err) => {
                    log::error!("overlay drop target: no window handle: {err}");
                    return;
                }
            };
            match grape_platform::windows::install_drop_target(hwnd, sink) {
                Ok(coverage) => log::info!(
                    "overlay drop target on {} of {} window(s)",
                    coverage.registered,
                    coverage.windows
                ),
                Err(err) => log::error!("overlay drop target: {err}"),
            }
        });
        if let Err(err) = queued {
            log::error!("overlay drop target: could not reach the main thread: {err}");
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (app, sink);
}

/// wry's own drop handler should never fire once Grape's target is
/// registered. If it does, some overlay window was missed: log it, because
/// wry doesn't set the drop effect (DECISIONS D13).
#[cfg(not(target_os = "macos"))]
pub fn on_overlay_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != OVERLAY {
        return;
    }
    if let tauri::WindowEvent::DragDrop(drag) = event {
        log::warn!(
            "overlay: wry's drop handler saw a drag ({}); Grape's drop target missed a window",
            drag_kind(drag)
        );
    }
}

/// A wry drop event for the log: its kind and how many paths it carries,
/// never the paths themselves.
#[cfg(any(not(target_os = "macos"), test))]
fn drag_kind(drag: &tauri::DragDropEvent) -> String {
    use tauri::DragDropEvent;

    match drag {
        DragDropEvent::Enter { paths, .. } => format!("enter, {} item(s)", paths.len()),
        DragDropEvent::Over { .. } => "over".to_owned(),
        DragDropEvent::Drop { paths, .. } => format!("drop, {} item(s)", paths.len()),
        DragDropEvent::Leave => "leave".to_owned(),
        _ => "another kind".to_owned(),
    }
}

/// On macOS wry's drop handler is the overlay's drop target. Its events
/// become [`DropEvent`]s in global points, like Grape's own target sends
/// on Windows.
#[cfg(target_os = "macos")]
pub fn on_overlay_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    use tauri::DragDropEvent;

    if window.label() != OVERLAY {
        return;
    }
    let tauri::WindowEvent::DragDrop(drag) = event else {
        return;
    };
    let app = window.app_handle();
    let frame = app.try_state::<OverlayFrame>().and_then(|last| {
        let frame = *last.0.lock().unwrap_or_else(PoisonError::into_inner);
        frame
    });
    let at = |position: &tauri::PhysicalPosition<f64>| match frame {
        Some(frame) => Some(to_global(frame, position.x, position.y)),
        None => {
            log::warn!("overlay: a drop event before the overlay was framed");
            None
        }
    };
    let event = match drag {
        DragDropEvent::Enter { paths, position } => match at(position) {
            Some((x, y)) => DropEvent::Enter {
                paths: paths.clone(),
                x,
                y,
            },
            None => return,
        },
        DragDropEvent::Over { position } => match at(position) {
            Some((x, y)) => DropEvent::Over { x, y },
            None => return,
        },
        DragDropEvent::Drop { paths, position } => match at(position) {
            Some((x, y)) => DropEvent::Drop {
                paths: paths.clone(),
                x,
                y,
            },
            None => return,
        },
        DragDropEvent::Leave => DropEvent::Leave,
        _ => return,
    };
    if let Some(sink) = app.try_state::<DropSink>() {
        if sink.0.try_send(event).is_err() {
            log::warn!("overlay: drop queue full or gone; a drop event was dropped");
        }
    }
}

/// A position relative to the overlay's top-left corner, in points, as
/// global points. wry types it as physical, but on macOS it's in points.
#[cfg(any(target_os = "macos", test))]
fn to_global(frame: Rect, x: f64, y: f64) -> (i32, i32) {
    // `as` saturates, and positions inside a window are far from the limits.
    let (x, y) = (x.round() as i32, y.round() as i32);
    (frame.x.saturating_add(x), frame.y.saturating_add(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missed_drop_is_logged_by_kind_and_count_never_by_path() {
        use std::path::PathBuf;

        use tauri::DragDropEvent;

        let paths = vec![
            PathBuf::from(r"C:\Users\someone\Secret Plans\budget.xlsx"),
            PathBuf::from("/Users/someone/photo.jpg"),
        ];
        let position = tauri::PhysicalPosition::new(10.0, 20.0);
        let cases = [
            (
                DragDropEvent::Enter {
                    paths: paths.clone(),
                    position,
                },
                "enter, 2 item(s)",
            ),
            (DragDropEvent::Over { position }, "over"),
            (
                DragDropEvent::Drop {
                    paths: paths.clone(),
                    position,
                },
                "drop, 2 item(s)",
            ),
            (DragDropEvent::Leave, "leave"),
        ];
        for (drag, kind) in cases {
            let text = drag_kind(&drag);
            assert_eq!(text, kind);
            for leak in ["someone", "Secret", "budget", "photo", "Users", "xlsx"] {
                assert!(!text.contains(leak), "{text}");
            }
        }
    }

    #[test]
    fn drop_positions_are_offset_by_the_overlay_frame() {
        let frame = Rect {
            x: -1440,
            y: -200,
            width: 1440,
            height: 900,
        };
        assert_eq!(to_global(frame, 0.0, 0.0), (-1440, -200));
        assert_eq!(to_global(frame, 720.4, 450.6), (-720, 251));
    }
}
