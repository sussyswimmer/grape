//! macOS drag detection (`docs/PLATFORM_MACOS.md` → "Detecting a file drag
//! without special permissions").
//!
//! A thread polls three things AppKit gives any app without a prompt (D7):
//! `NSEvent.pressedMouseButtons` (the left button), `NSEvent.modifierFlags`
//! and the drag pasteboard's `changeCount`. It polls every 100 ms while the
//! button is up and every 8 ms while it's down. With every window hidden,
//! macOS may App Nap a menu-bar app and stretch those waits, so the thread
//! holds an `NSProcessInfo` activity (user-initiated, idle system sleep
//! allowed) for its whole life. It waits with `park_timeout`, and stopping
//! unparks it, so Quit never waits out a wait.
//!
//! A press only becomes a drag for Grape once a drag session has started
//! (the drag pasteboard's `changeCount` moved since the button was last
//! seen up) and that drag carries files (the pasteboard lists the file-URL
//! type). Text, images from a browser and other non-file drags never open
//! the wheel. Grape reads only the pasteboard's change count and type list,
//! never its contents: the overlay gets the paths from the drop itself.
//!
//! A session that Grape started itself, a drag out of the float shelf
//! (`drag_out`), never opens the wheel: a press during which Grape asked
//! AppKit for one ([`own_drags`] moved since the button was last seen up)
//! never becomes a drag here. The count is read after the pasteboard, so a
//! drag out that begins in between can't pass for Finder's.
//!
//! The drag starts the moment such a session is seen, wherever the pointer
//! is: AppKit only starts a session once the pointer has passed its own
//! drag threshold, so Grape adds none ([`DragTracker::start_now`]). The
//! keys held when Grape first sees that session are the baseline, as on
//! Windows where they're read at the threshold: a Command held for a Finder
//! Command-drag doesn't open a wheel. A key pressed after that is reported
//! by the next poll, even while the pointer rests. A key tapped and
//! released before the session is seen is missed. Esc is never reported: reading keys needs a permission prompt.
//! AppKit ends the drag session on Esc, and the app reads that from the
//! overlay's drop events instead.
//!
//! Signals go out with `try_send`: a full channel drops them rather than
//! slow anything down.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSEvent, NSEventModifierFlags, NSPasteboard, NSPasteboardNameDrag, NSPasteboardTypeFileURL,
};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{ns_string, NSActivityOptions, NSProcessInfo};

use super::drag_out::own_drags;
use super::{display, io_error};
use crate::mac_rules::{cursor_to_grape, own_drag_in_press};
use crate::tracker::DragTracker;
use crate::{DragMonitor, DragSignal, Modifiers, PlatformError};

/// Polling interval while the left button is down (about 120 Hz).
const POLL_EVERY: Duration = Duration::from_millis(8);
/// Polling interval while it's up.
const IDLE_WAIT: Duration = Duration::from_millis(100);
/// `pressedMouseButtons` bit for the left button.
pub(super) const LEFT_BUTTON: usize = 1;

/// The macOS [`DragMonitor`]: one polling thread.
#[derive(Debug, Default)]
pub(crate) struct MacDragMonitor {
    running: Option<Running>,
}

#[derive(Debug)]
struct Running {
    stop: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

impl DragMonitor for MacDragMonitor {
    fn start(&mut self, tx: SyncSender<DragSignal>) -> Result<(), PlatformError> {
        if self.running.is_some() {
            return Err(PlatformError::AlreadyRunning);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("grape-drag".to_owned())
            .spawn(move || run(&tx, &worker_stop))
            .map_err(|err| io_error("spawning the drag worker thread", &err))?;
        self.running = Some(Running { stop, worker });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop.store(true, Ordering::Release);
            // Wake it from its wait, so stopping never waits one out.
            running.worker.thread().unpark();
            let _ = running.worker.join();
        }
    }
}

impl Drop for MacDragMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(tx: &SyncSender<DragSignal>, stop: &AtomicBool) {
    // Held until this thread ends, so App Nap never stretches the waits.
    let _awake = autoreleasepool(|_| Awake::begin());
    // The pasteboard object isn't Send, so it's made here, on this thread.
    let mut poller = autoreleasepool(|_| Poller::new());
    while !stop.load(Ordering::Acquire) {
        let pressed = autoreleasepool(|_| poller.tick(tx));
        // `stop` unparks the thread. An early wake only polls early.
        thread::park_timeout(if pressed { POLL_EVERY } else { IDLE_WAIT });
    }
}

/// An `NSProcessInfo` activity that keeps App Nap away while Grape watches
/// for drags: user-initiated, but the Mac may still sleep when idle. It
/// ends when dropped, on the thread that began it.
struct Awake {
    process: Retained<NSProcessInfo>,
    activity: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl Awake {
    fn begin() -> Self {
        let process = NSProcessInfo::processInfo();
        let activity = process.beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
            ns_string!("Watching for file drags"),
        );
        Self { process, activity }
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        autoreleasepool(|_| {
            // SAFETY: `activity` is the token this process's
            // `beginActivityWithOptions:reason:` returned, and it's ended
            // only here, once.
            unsafe { self.process.endActivity(&self.activity) };
        });
    }
}

/// The polling state on the drag thread.
struct Poller {
    pasteboard: Retained<NSPasteboard>,
    tracker: DragTracker,
    /// The drag pasteboard's change count, last read with the button up.
    /// It's the baseline for the next press: a drag can only start once the
    /// button is down, and a quick drag may already have bumped the count by
    /// the time the press is seen.
    idle_count: isize,
    /// [`own_drags`], read with `idle_count`: Grape's own drags out start
    /// only while the button is down, so any during the next press makes
    /// the count differ from this.
    idle_own: u64,
    press: Option<Press>,
}

/// One press of the left button, as the poller saw it.
struct Press {
    /// The change count from before the press.
    idle_count: isize,
    /// [`own_drags`] from before the press.
    own_at_press: u64,
    /// Grape started a drag out of the shelf during this press: it never
    /// becomes a drag here.
    own: bool,
    /// The primary screen's height, for the cursor flip.
    primary_height: f64,
    /// A drag that carries files started during this press.
    file_drag: bool,
    /// The last cursor position given to the tracker.
    last: Option<(i32, i32)>,
}

impl Poller {
    fn new() -> Self {
        let pasteboard = drag_pasteboard();
        let idle_count = pasteboard.changeCount();
        Self {
            pasteboard,
            // The thresholds go unused: drags start with `start_now`.
            tracker: DragTracker::new(0, 0),
            idle_count,
            idle_own: own_drags(),
            press: None,
        }
    }

    /// One poll. Sends what it saw and returns whether the button is down.
    fn tick(&mut self, tx: &SyncSender<DragSignal>) -> bool {
        let button_down = NSEvent::pressedMouseButtons() & LEFT_BUTTON != 0;
        let held = held_modifiers();
        let mut out: Vec<DragSignal> = Vec::new();

        if button_down {
            if self.press.is_none() {
                let primary_height = primary_height();
                let (x, y) = cursor(primary_height);
                self.press = Some(Press {
                    idle_count: self.idle_count,
                    own_at_press: self.idle_own,
                    own: false,
                    primary_height,
                    file_drag: false,
                    last: None,
                });
                out.extend(self.tracker.button_down(x, y, true));
            }
            if let Some(press) = &mut self.press {
                if press.file_drag {
                    let at = cursor(press.primary_height);
                    if press.last != Some(at) {
                        press.last = Some(at);
                        out.extend(self.tracker.moved(at.0, at.1, held));
                    }
                } else if !press.own
                    && self.pasteboard.changeCount() != press.idle_count
                    && carries_files(&self.pasteboard)
                {
                    if own_drag_in_press(press.own_at_press, own_drags()) {
                        // Grape's own drag out of the shelf (the count is
                        // read after the pasteboard): this press never
                        // becomes a wheel drag.
                        press.own = true;
                    } else {
                        // AppKit started a file drag, so the pointer is
                        // already past its threshold: start now, even at
                        // rest. The keys held now are the baseline.
                        press.file_drag = true;
                        let at = cursor(press.primary_height);
                        press.last = Some(at);
                        let signals = self.tracker.start_now(at.0, at.1, held);
                        if signals
                            .iter()
                            .any(|signal| matches!(signal, DragSignal::Started { .. }))
                        {
                            display::request_refresh();
                        }
                        out.extend(signals);
                    }
                }
            }
        } else {
            if self.press.take().is_some() {
                out.extend(self.tracker.button_up());
            }
            self.idle_count = self.pasteboard.changeCount();
            self.idle_own = own_drags();
        }

        // Reports keys changed since the baseline, once the drag started.
        if self.tracker.is_pressed() {
            out.extend(self.tracker.poll(held, false, button_down));
        }
        for signal in out {
            // Drop rather than block: see the module docs.
            let _ = tx.try_send(signal);
        }
        button_down
    }
}

/// The system's drag pasteboard, which every drag session writes.
fn drag_pasteboard() -> Retained<NSPasteboard> {
    // SAFETY: NSPasteboardNameDrag is an immutable NSString constant that
    // AppKit defines for the life of the process.
    let name = unsafe { NSPasteboardNameDrag };
    NSPasteboard::pasteboardWithName(name)
}

/// `true` when the pasteboard lists the file-URL type. Reads the type list
/// only, never the data.
fn carries_files(pasteboard: &NSPasteboard) -> bool {
    // SAFETY: NSPasteboardTypeFileURL is an immutable NSString constant
    // that AppKit defines for the life of the process.
    let file_url = unsafe { NSPasteboardTypeFileURL };
    pasteboard
        .types()
        .is_some_and(|types| types.containsObject(file_url))
}

/// The cursor in Grape's top-left points.
fn cursor(primary_height: f64) -> (i32, i32) {
    let at = NSEvent::mouseLocation();
    cursor_to_grape(at.x, at.y, primary_height)
}

/// The primary (menu-bar) screen's height in points. CoreGraphics answers
/// on any thread; `NSScreen` would need the main thread.
fn primary_height() -> f64 {
    CGDisplayBounds(CGMainDisplayID()).size.height
}

/// Shift, Control, Option and Command. Caps Lock and Fn are ignored.
fn held_modifiers() -> Modifiers {
    let flags = NSEvent::modifierFlags_class();
    Modifiers {
        shift: flags.contains(NSEventModifierFlags::Shift),
        ctrl: flags.contains(NSEventModifierFlags::Control),
        alt: flags.contains(NSEventModifierFlags::Option),
        meta: flags.contains(NSEventModifierFlags::Command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn the_monitor_starts_and_stops() {
        let mut monitor = MacDragMonitor::default();
        // Stopping before starting is harmless.
        monitor.stop();
        let (tx, _rx) = mpsc::sync_channel(16);
        monitor.start(tx.clone()).unwrap();
        assert_eq!(monitor.start(tx), Err(PlatformError::AlreadyRunning));
        monitor.stop();
        monitor.stop();
    }

    #[test]
    fn a_poll_with_the_button_up_sends_nothing() {
        // Nobody holds the mouse on CI's Mac.
        if NSEvent::pressedMouseButtons() & LEFT_BUTTON != 0 {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(16);
        let mut poller = autoreleasepool(|_| Poller::new());
        assert!(!autoreleasepool(|_| poller.tick(&tx)));
        drop(tx);
        assert!(rx.recv().is_err());
    }
}
