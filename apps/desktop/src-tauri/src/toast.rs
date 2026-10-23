//! The toast: a small topmost window near the wheel that says what a drop
//! is doing ("Making photo.png…", "Done") and reveals the output when
//! clicked. Rust owns the copy, the placement and the timing; the page in
//! `ui/toast.html` only renders `toast://show` / `toast://hide`.
//!
//! The toast never takes focus (on Windows it gets the overlay's
//! tool-window, no-activate styles; on macOS it's ordered front without
//! becoming key) and refuses drops (DECISIONS D13).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use grape_core::GEOMETRY;
use grape_platform::{DisplayBounds, PlatformError, Rect};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::events::{ToastHide, ToastShow, ToastTone};

/// Label of the toast window in tauri.conf.json.
pub const TOAST: &str = "toast";
/// The toast's size in logical pixels (tauri.conf.json).
const WIDTH: f64 = 320.0;
const HEIGHT: f64 = 64.0;
/// Space between the wheel's glass and the toast, logical pixels.
const SPACING: f64 = 16.0;

/// How long each kind of toast stays up.
fn linger(tone: ToastTone) -> Option<Duration> {
    match tone {
        ToastTone::Working => None,
        ToastTone::Done => Some(Duration::from_millis(2500)),
        ToastTone::Info => Some(Duration::from_millis(3500)),
        ToastTone::Error => Some(Duration::from_millis(5000)),
    }
}

/// Where the toast goes: centered under the wheel, or above it when there's
/// no room below, kept inside the work area. Global physical pixels.
pub fn place(anchor: (i32, i32), display: &DisplayBounds) -> (i32, i32) {
    let scale = if display.scale.is_finite() && display.scale > 0.0 {
        display.scale
    } else {
        1.0
    };
    let px = |logical: f64| (logical * scale).round() as i32;
    let (width, height) = (px(WIDTH), px(HEIGHT));
    let reach = px(GEOMETRY.backdrop_r + SPACING);
    let work: Rect = display.work_area;

    let below = anchor.1 + reach;
    let y = if below + height <= work.y + work.height {
        below
    } else {
        anchor.1 - reach - height
    };
    let x = anchor.0 - width / 2;
    (
        x.clamp(work.x, (work.x + work.width - width).max(work.x)),
        y.clamp(work.y, (work.y + work.height - height).max(work.y)),
    )
}

/// The toast's state, managed by Tauri.
#[derive(Debug, Default)]
pub struct Toaster {
    next_id: AtomicU64,
    /// The toast on screen now, so an old timer can't hide a newer toast.
    current: Arc<AtomicU64>,
    /// What clicking each toast reveals.
    reveal: Mutex<HashMap<u64, PathBuf>>,
}

impl Toaster {
    /// A fresh id for a batch of work.
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Remembers what clicking toast `id` reveals. Only Rust knows paths;
    /// the page sends back the id.
    pub fn set_reveal(&self, id: u64, path: PathBuf) {
        let mut reveal = self.reveal.lock().unwrap_or_else(PoisonError::into_inner);
        // Only the recent toasts can still be clicked.
        reveal.retain(|&known, _| known + 16 > id);
        reveal.insert(id, path);
    }

    fn reveal_path(&self, id: u64) -> Option<PathBuf> {
        let reveal = self.reveal.lock().unwrap_or_else(PoisonError::into_inner);
        reveal.get(&id).cloned()
    }
}

/// Shows `toast` near the wheel at `anchor` on `display`, replacing
/// whatever the toast showed. Done, info and error toasts hide themselves.
pub fn show(app: &AppHandle, toast: ToastShow, anchor: (i32, i32), display: &DisplayBounds) {
    let Some(window) = app.get_webview_window(TOAST) else {
        log::error!("the toast window is missing");
        return;
    };
    let toaster = app.state::<Toaster>();
    toaster.current.store(toast.id, Ordering::Release);
    let (id, tone) = (toast.id, toast.tone);

    let (x, y) = place(anchor, display);
    if let Err(err) = app.emit_to(TOAST, "toast://show", toast) {
        log::error!("could not send toast://show: {err}");
    }
    place_and_show(app, window, x, y);

    if let Some(delay) = linger(tone) {
        let app = app.clone();
        let current = Arc::clone(&toaster.current);
        thread::spawn(move || {
            thread::sleep(delay);
            if current.load(Ordering::Acquire) == id {
                hide(&app, id);
            }
        });
    }
}

/// Moves the toast to (x, y) and shows it. Never `set_focus`: the app
/// under the cursor keeps the keyboard.
#[cfg(not(target_os = "macos"))]
fn place_and_show(_app: &AppHandle, window: tauri::WebviewWindow, x: i32, y: i32) {
    if let Err(err) = window.set_position(tauri::PhysicalPosition::new(x, y)) {
        log::warn!("could not place the toast: {err}");
    }
    if let Err(err) = window.show() {
        log::error!("could not show the toast: {err}");
    }
}

/// Moves the toast to (x, y), in points, and orders it front on the main
/// thread. Tauri's `show()` would make it the key window.
#[cfg(target_os = "macos")]
fn place_and_show(app: &AppHandle, window: tauri::WebviewWindow, x: i32, y: i32) {
    // Points on macOS (the display's scale is 1.0).
    let frame = Rect {
        x,
        y,
        width: WIDTH as i32,
        height: HEIGHT as i32,
    };
    let queued = app.run_on_main_thread(move || {
        let ns_window = match window.ns_window() {
            Ok(ns_window) => ns_window,
            Err(err) => {
                log::error!("could not show the toast: no NSWindow: {err}");
                return;
            }
        };
        // SAFETY: this runs on the main thread, and `ns_window` is the
        // toast's NSWindow, which Tauri keeps alive while `window` is.
        if let Err(err) = unsafe { grape_platform::macos::frame_and_show(ns_window, frame) } {
            log::error!("could not show the toast: {err}");
        }
    });
    if let Err(err) = queued {
        log::error!("could not show the toast: no main thread: {err}");
    }
}

/// Hides toast `id`.
pub fn hide(app: &AppHandle, id: u64) {
    if let Err(err) = app.emit_to(TOAST, "toast://hide", ToastHide { id }) {
        log::error!("could not send toast://hide: {err}");
    }
    if let Some(window) = app.get_webview_window(TOAST) {
        if let Err(err) = window.hide() {
            log::error!("could not hide the toast: {err}");
        }
    }
}

/// The page reports a click on toast `id`: reveal its output in Explorer
/// or Finder.
#[tauri::command]
pub fn toast_clicked(id: u64, toaster: State<'_, Toaster>) {
    let Some(path) = toaster.reveal_path(id) else {
        return;
    };
    match grape_platform::file_actions::reveal(&path) {
        Ok(()) => {}
        Err(PlatformError::Unsupported) => {
            log::info!("reveal {} (not wired on this OS)", path.display());
        }
        Err(err) => log::error!("could not reveal {}: {err}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(scale: f64) -> DisplayBounds {
        DisplayBounds {
            monitor: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            work_area: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1032,
            },
            scale,
        }
    }

    #[test]
    fn the_toast_sits_centered_under_the_wheel() {
        assert_eq!(place((960, 400), &display(1.0)), (960 - 160, 400 + 142));
    }

    #[test]
    fn it_goes_above_the_wheel_when_there_is_no_room_below() {
        // 900 + 142 + 64 overflows the 1032 px work area.
        assert_eq!(place((960, 900), &display(1.0)), (800, 900 - 142 - 64));
    }

    #[test]
    fn it_stays_inside_the_work_area() {
        assert_eq!(place((10, 400), &display(1.0)).0, 0);
        assert_eq!(place((1915, 400), &display(1.0)).0, 1920 - 320);
    }

    #[test]
    fn sizes_scale_with_the_monitor() {
        // At 150 %: 480×96 physical, 213 px from the anchor.
        assert_eq!(place((960, 300), &display(1.5)), (960 - 240, 300 + 213));
    }

    #[test]
    fn ids_are_fresh_and_reveals_are_kept_for_recent_toasts() {
        let toaster = Toaster::default();
        let first = toaster.next_id();
        assert_eq!(toaster.next_id(), first + 1);
        toaster.set_reveal(first, PathBuf::from("a.png"));
        assert_eq!(toaster.reveal_path(first), Some(PathBuf::from("a.png")));
        toaster.set_reveal(first + 40, PathBuf::from("b.png"));
        assert_eq!(toaster.reveal_path(first), None);
        assert_eq!(
            toaster.reveal_path(first + 40),
            Some(PathBuf::from("b.png"))
        );
    }

    #[test]
    fn working_toasts_stay_and_the_rest_linger() {
        assert_eq!(linger(ToastTone::Working), None);
        assert!(linger(ToastTone::Error) > linger(ToastTone::Done));
    }
}
