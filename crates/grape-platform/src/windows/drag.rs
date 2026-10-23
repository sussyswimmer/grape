//! Windows drag detection (`docs/PLATFORM_WINDOWS.md` → "Detecting a file
//! drag").
//!
//! The hook thread ([`super::hook`]) forwards raw mouse input to a worker
//! thread here. The worker:
//! - checks the allowlist when the button goes down: the press only counts
//!   if the top-level window under it is Explorer or the Desktop
//!   (`WindowFromPoint` → `GetAncestor(GA_ROOT)` → class name);
//! - runs the [`DragTracker`] (threshold `SM_CXDRAG` / `SM_CYDRAG`);
//! - polls `GetAsyncKeyState` at about 120 Hz while the button is down, for
//!   modifiers, Esc and a missed button-up. No keyboard hook.
//!
//! Signals go out with `try_send`: a full channel drops them rather than
//! slow anything down.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{E_FAIL, POINT};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_LBUTTON, VK_LWIN, VK_MENU, VK_RBUTTON,
    VK_RWIN, VK_SHIFT,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetSystemMetrics, WindowFromPoint, GA_ROOT, SM_CXDRAG, SM_CYDRAG,
};

use super::hook::{MouseHook, RawMouse};
use crate::tracker::DragTracker;
use crate::{DragMonitor, DragSignal, Modifiers, PlatformError};

/// Top-level window classes a drag may start from: File Explorer windows
/// and the Desktop. **[Grape]** default for open question O5.
const DRAG_SOURCE_CLASSES: [&str; 4] = ["CabinetWClass", "ExploreWClass", "Progman", "WorkerW"];

/// Raw events buffered between the hook and the worker.
const RAW_CAPACITY: usize = 1024;
/// Key polling interval while the button is down (about 120 Hz).
const POLL_EVERY: Duration = Duration::from_millis(8);
/// How long the worker sleeps between events when nothing is pressed.
const IDLE_WAIT: Duration = Duration::from_millis(250);
/// Consecutive "button up" polls before a missed release is believed. The
/// async key state can lag the hook by a moment, so one reading isn't
/// enough to end a drag.
const UP_READS_TO_END: u32 = 3;

/// `true` if a drag may start in a top-level window of this class.
pub(crate) fn is_drag_source_class(class: &str) -> bool {
    DRAG_SOURCE_CLASSES.contains(&class)
}

/// The Windows [`DragMonitor`]: a `WH_MOUSE_LL` hook thread plus a worker.
#[derive(Debug, Default)]
pub(crate) struct WindowsDragMonitor {
    running: Option<Running>,
}

#[derive(Debug)]
struct Running {
    hook: MouseHook,
    stop: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

impl DragMonitor for WindowsDragMonitor {
    fn start(&mut self, tx: SyncSender<DragSignal>) -> Result<(), PlatformError> {
        if self.running.is_some() {
            return Err(PlatformError::AlreadyRunning);
        }
        let (raw_tx, raw_rx) = mpsc::sync_channel(RAW_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("grape-drag".to_owned())
            .spawn(move || run_worker(&raw_rx, &tx, &worker_stop))
            .map_err(|_| PlatformError::Os {
                context: "spawning the drag worker thread",
                code: E_FAIL.0,
            })?;
        let hook = match MouseHook::start(raw_tx) {
            Ok(hook) => hook,
            Err(err) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                return Err(err);
            }
        };
        self.running = Some(Running { hook, stop, worker });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut running) = self.running.take() {
            // Unhooking drops the hook's sender, which wakes the worker.
            running.hook.stop();
            running.stop.store(true, Ordering::Release);
            let _ = running.worker.join();
        }
    }
}

impl Drop for WindowsDragMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_worker(raw: &Receiver<RawMouse>, tx: &SyncSender<DragSignal>, stop: &AtomicBool) {
    let (threshold_x, threshold_y) = drag_threshold();
    let mut tracker = DragTracker::new(threshold_x, threshold_y);
    let mut last_poll = Instant::now();
    let mut up_reads = 0_u32;

    while !stop.load(Ordering::Acquire) {
        let wait = if tracker.is_pressed() {
            POLL_EVERY
        } else {
            IDLE_WAIT
        };
        let mut out: Vec<DragSignal> = Vec::new();
        match raw.recv_timeout(wait) {
            Ok(RawMouse::Down { x, y }) => {
                up_reads = 0;
                out.extend(tracker.button_down(x, y, is_armed_at(x, y)));
            }
            Ok(RawMouse::Move { x, y }) => out.extend(tracker.moved(x, y, held_modifiers())),
            Ok(RawMouse::Up) => out.extend(tracker.button_up()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if tracker.is_pressed() && last_poll.elapsed() >= POLL_EVERY {
            last_poll = Instant::now();
            up_reads = if primary_button_down() {
                0
            } else {
                up_reads.saturating_add(1)
            };
            out.extend(tracker.poll(
                held_modifiers(),
                key_down(VK_ESCAPE),
                up_reads < UP_READS_TO_END,
            ));
        }
        for signal in out {
            // Drop rather than block: see the module docs.
            let _ = tx.try_send(signal);
        }
    }
}

/// `SM_CXDRAG` / `SM_CYDRAG`: how far the pointer may move before a press
/// becomes a drag.
fn drag_threshold() -> (i32, i32) {
    // SAFETY: plain queries with no pointers.
    unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) }
}

/// `true` if the top-level window under (`x`, `y`) is Explorer or the
/// Desktop.
fn is_armed_at(x: i32, y: i32) -> bool {
    root_class_at(x, y).is_some_and(|class| is_drag_source_class(&class))
}

fn root_class_at(x: i32, y: i32) -> Option<String> {
    // SAFETY: takes the point by value and only reads window-manager state.
    let hwnd = unsafe { WindowFromPoint(POINT { x, y }) };
    if hwnd.is_invalid() {
        return None;
    }
    // SAFETY: GetAncestor accepts any window handle and returns null for a
    // stale one.
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.is_invalid() {
        return None;
    }
    let mut name = [0_u16; 64];
    // SAFETY: `name` is a writable buffer; the call writes at most its
    // length (including the terminator) and returns the characters copied.
    let len = unsafe { GetClassNameW(root, &mut name) };
    let len = usize::try_from(len).ok().filter(|len| *len > 0)?;
    Some(String::from_utf16_lossy(name.get(..len)?))
}

fn key_down(key: VIRTUAL_KEY) -> bool {
    // SAFETY: a plain query of the async key state; no pointers.
    let state = unsafe { GetAsyncKeyState(i32::from(key.0)) };
    // The high bit says "down right now".
    state < 0
}

fn held_modifiers() -> Modifiers {
    Modifiers {
        shift: key_down(VK_SHIFT),
        ctrl: key_down(VK_CONTROL),
        alt: key_down(VK_MENU),
        meta: key_down(VK_LWIN) || key_down(VK_RWIN),
    }
}

/// Either physical button: `GetAsyncKeyState` reads physical buttons, and
/// with swapped buttons the primary one is the right.
fn primary_button_down() -> bool {
    key_down(VK_LBUTTON) || key_down(VK_RBUTTON)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_and_the_desktop_are_drag_sources() {
        for class in ["CabinetWClass", "ExploreWClass", "Progman", "WorkerW"] {
            assert!(is_drag_source_class(class), "{class}");
        }
        for class in [
            "Shell_TrayWnd",
            "#32770",
            "Chrome_WidgetWin_1",
            "cabinetwclass",
            "",
        ] {
            assert!(!is_drag_source_class(class), "{class}");
        }
    }

    #[test]
    fn the_drag_threshold_is_positive() {
        let (x, y) = drag_threshold();
        assert!(x > 0 && y > 0, "{x}×{y}");
    }

    #[test]
    fn the_monitor_starts_and_stops() {
        let mut monitor = WindowsDragMonitor::default();
        let (tx, _rx) = mpsc::sync_channel(16);
        monitor.start(tx.clone()).unwrap();
        assert_eq!(monitor.start(tx), Err(PlatformError::AlreadyRunning));
        monitor.stop();
        monitor.stop();
    }

    #[test]
    fn the_worker_ends_when_the_hook_goes_away() {
        let (raw_tx, raw_rx) = mpsc::sync_channel(4);
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        drop(raw_tx);
        run_worker(&raw_rx, &tx, &stop);
        drop(tx);
        assert!(rx.recv().is_err());
    }
}
