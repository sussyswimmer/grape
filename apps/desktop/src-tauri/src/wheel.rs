//! Wires the [`WheelController`] to the OS and the overlay window.
//!
//! - The drag monitor from `grape-platform` sends [`DragSignal`]s.
//! - On Windows, Grape's own drop target on the overlay sends
//!   [`DropEvent`]s (DECISIONS D13). wry's handler is replaced; if it ever
//!   reports an event anyway, that's logged as a gap in coverage.
//! - Both feed one channel. A controller thread handles each input and
//!   applies the [`Effect`]s: overlay frame and visibility, and the
//!   `wheel://open`, `wheel://highlight` and `wheel://close` events.
//! - The overlay calls `wheel_painted` once a wheel is on screen, for the
//!   latency log.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Instant;

use grape_core::{wheel_layout, Geometry, Point, Segment, WheelMode, GEOMETRY};
use grape_platform::{DragMonitor, DragSignal, DropEvent, PlatformError};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewWindow};

use crate::controller::{Effect, Input, KeyMap, WheelController, WheelView};
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
/// re-registered before each show.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Clone)]
struct DropSink(SyncSender<DropEvent>);

/// `wheel://open` payload: what to draw and where.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenPayload {
    seq: u64,
    mode: WheelMode,
    /// Logical pixels from the overlay's top-left.
    anchor: Point,
    segments: Vec<Segment>,
    geometry: Geometry,
}

impl OpenPayload {
    fn new(view: &WheelView) -> Self {
        Self {
            seq: view.seq,
            mode: view.mode,
            anchor: view.anchor,
            segments: wheel_layout(view.count),
            geometry: GEOMETRY,
        }
    }
}

/// `wheel://highlight` payload.
#[derive(Debug, Clone, Serialize)]
struct HighlightPayload {
    index: Option<usize>,
}

/// Starts drag detection and the controller. Call from `setup`, on the
/// main thread (it registers the overlay's drop target there). Nothing in
/// here is fatal: without a monitor Grape just never opens the wheel.
pub fn start(app: &AppHandle) {
    let (tx, rx) = mpsc::sync_channel(INPUT_CAPACITY);
    app.manage(WheelInputs(tx.clone()));

    app.manage(Monitor(Mutex::new(start_monitor(&tx))));

    let (drop_tx, drop_rx) = mpsc::sync_channel(SOURCE_CAPACITY);
    let drop_sink = DropSink(drop_tx);
    forward(drop_rx, tx, Input::Drop, "grape-drop-forward");
    install_drop_target(app, &drop_sink);
    app.manage(drop_sink);

    let app = app.clone();
    let spawned = thread::Builder::new()
        .name("grape-wheel".to_owned())
        .spawn(move || run_controller(&app, &rx));
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

fn run_controller(app: &AppHandle, rx: &Receiver<Input>) {
    let screens = |x: i32, y: i32| match grape_platform::display_bounds_at(x, y) {
        Ok(bounds) => Some(bounds),
        Err(err) => {
            log::warn!("no display bounds at ({x}, {y}): {err}");
            None
        }
    };
    let mut controller = WheelController::new(screens, KeyMap::for_this_os());
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
            apply(app, effect);
        }
    }
}

fn apply(app: &AppHandle, effect: Effect) {
    match effect {
        Effect::Open(view) => open_overlay(app, &view),
        Effect::Swap(view) => emit(app, "wheel://open", OpenPayload::new(&view)),
        Effect::Highlight(index) => emit(app, "wheel://highlight", HighlightPayload { index }),
        Effect::Close => {
            emit(app, "wheel://close", ());
            if let Some(overlay) = overlay(app) {
                if let Err(err) = overlay.hide() {
                    log::error!("could not hide the overlay: {err}");
                }
            }
        }
        Effect::Dropped(report) => log::info!("{report}"),
        Effect::Log(line) => log::info!("{line}"),
    }
}

/// Frames the overlay on the monitor, sends the wheel and shows it. Never
/// focuses it: the app under the cursor keeps the keyboard.
fn open_overlay(app: &AppHandle, view: &WheelView) {
    let Some(overlay) = overlay(app) else {
        return;
    };
    let began = Instant::now();
    // Re-register first: WebView2 may have created child windows since.
    if let Some(sink) = app.try_state::<DropSink>() {
        install_drop_target(app, &sink);
    }
    // Position before size: moving to a monitor with another DPI makes
    // Windows rescale the window, so the size is set after.
    let frame = view.monitor;
    let framed = overlay
        .set_position(PhysicalPosition::new(frame.x, frame.y))
        .and_then(|()| {
            overlay.set_size(PhysicalSize::new(
                frame.width.unsigned_abs(),
                frame.height.unsigned_abs(),
            ))
        });
    if let Err(err) = framed {
        log::error!("could not frame the overlay on the monitor: {err}");
    }
    // Draw while still hidden, so the wheel is there when the window shows.
    emit(app, "wheel://open", OpenPayload::new(view));
    if let Err(err) = overlay.show() {
        log::error!("could not show the overlay: {err}");
        return;
    }
    log::info!(
        "overlay shown in {:.1} ms",
        began.elapsed().as_secs_f64() * 1000.0
    );
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
pub fn on_overlay_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != OVERLAY {
        return;
    }
    if let tauri::WindowEvent::DragDrop(drag) = event {
        log::warn!("overlay: wry's drop handler saw {drag:?}; Grape's drop target missed a window");
    }
}
