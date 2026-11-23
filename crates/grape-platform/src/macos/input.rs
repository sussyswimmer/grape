//! The Shortcuts wheel's hands and eyes on macOS
//! (`docs/design/m10-shortcuts-wheel.md` §4): whether Grape may press keys,
//! pressing a chord, typing text, the clipboard, the clock, and the pointer
//! while a wheel is open.
//!
//! - Input access is Accessibility (`AXIsProcessTrusted`). Asking shows the
//!   system's own prompt, which also puts Grape in the list.
//! - Keys are `CGEvent`s from a private event source, posted at the HID
//!   tap: a key down, a short wait, a key up. An event keeps its own flags
//!   except Shift, Control, Option and Command, which are exactly the
//!   chord's, so a key the user still holds never joins in. Without input
//!   access macOS drops the events and reports nothing.
//! - A chord Grape authors by letter is posted from the key that types the
//!   letter on the current keyboard layout (`crate::mac_rules::char_key`).
//!   The layout is read on the main thread, where the Text Input Sources
//!   calls must run.
//! - Text goes out in the pieces `crate::mac_rules::text_chunks` cuts, each
//!   chunk as the Unicode string of one key event.
//! - The clipboard is the general pasteboard, on the caller's thread.
//! - The pointer is polled on a `"grape-pointer"` thread every 8 ms while
//!   the watch is on (`NSEvent`'s class methods: no permission, and it sees
//!   a click the overlay swallows). The thread is parked while the watch is
//!   off and holds an `NSProcessInfo` activity only while it is on.
//!
//! Nothing here logs what is typed, pressed or read.

use std::ffi::{c_ulong, c_void};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSPasteboard, NSPasteboardTypeString};
use objc2_core_foundation::{CFBoolean, CFData, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGMainDisplayID,
};
use objc2_foundation::{ns_string, NSActivityOptions, NSProcessInfo};

use super::drag::LEFT_BUTTON;
use super::shortcuts::{on_main_within, run};
use super::{io_error, NO_CODE};
use crate::keys::{Chord, KeyCode};
use crate::mac_rules::{
    cg_flags, char_key, cursor_to_grape, input_access_settings_program, layout_modifier_state,
    local_time, text_chunks, us_key_for_char, virtual_key, PointerEdges, TextPiece, CG_CHORD_FLAGS,
};
use crate::shortcuts::{InputAccess, LocalTime, PointerSignal, PointerWatch};
use crate::{Modifiers, PlatformError};

/// How long a character chord waits for the main thread to read the
/// keyboard layout.
const LAYOUT_WAIT: Duration = Duration::from_secs(1);
/// The wait between a chord's key down and its key up.
const KEY_GAP: Duration = Duration::from_millis(4);
/// The wait between two key events while typing.
const TYPE_GAP: Duration = Duration::from_millis(3);
/// How often the pointer is read while the watch is on (about 120 Hz).
const POINTER_EVERY: Duration = Duration::from_millis(8);

/// `kUCKeyActionDown`.
const KEY_ACTION_DOWN: u16 = 0;
/// `kUCKeyTranslateNoDeadKeysMask`: a dead key answers with its own sign.
const NO_DEAD_KEYS: u32 = 1;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: &CFDictionary) -> u8;
    #[link_name = "kAXTrustedCheckOptionPrompt"]
    static AX_TRUSTED_CHECK_OPTION_PROMPT: &'static CFString;
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> Option<NonNull<CFType>>;
    fn TISGetInputSourceProperty(source: &CFType, key: &CFString) -> Option<NonNull<CFType>>;
    #[link_name = "kTISPropertyUnicodeKeyLayoutData"]
    static TIS_PROPERTY_UNICODE_KEY_LAYOUT_DATA: &'static CFString;
}

// Carbon's headers declare these two as well. CoreServices is linked too,
// so they are found whichever of the two frameworks exports them.
#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn UCKeyTranslate(
        layout: *const c_void,
        virtual_key: u16,
        action: u16,
        modifier_state: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: *mut u32,
        max_length: c_ulong,
        actual_length: *mut c_ulong,
        string: *mut u16,
    ) -> i32;
    fn LMGetKbdType() -> u8;
}

pub(crate) fn input_access() -> InputAccess {
    // SAFETY: takes nothing and only reads this process's own trust.
    if unsafe { AXIsProcessTrusted() } != 0 {
        InputAccess::Granted
    } else {
        InputAccess::Denied
    }
}

pub(crate) fn request_input_access(open_settings: bool) -> Result<(), PlatformError> {
    // SAFETY: an immutable CFString constant that ApplicationServices
    // defines for the life of the process.
    let prompt = unsafe { AX_TRUSTED_CHECK_OPTION_PROMPT };
    let options =
        CFDictionary::<CFString, CFBoolean>::from_slices(&[prompt], &[CFBoolean::new(true)]);
    // SAFETY: `options` is a CFDictionary with the one key the call
    // documents and a CFBoolean for it, alive until this function returns.
    // The call shows the system's prompt when Grape isn't trusted and
    // returns at once either way.
    let _trusted = unsafe { AXIsProcessTrustedWithOptions(options.as_opaque()) };
    if open_settings {
        run(
            &input_access_settings_program(),
            "opening the Accessibility settings",
        )
    } else {
        Ok(())
    }
}

/// Shift, Control, Option and Command, as the drag poller reads them. Caps
/// Lock and Fn are ignored.
pub(crate) fn held_modifiers() -> Modifiers {
    let flags = NSEvent::modifierFlags_class();
    Modifiers {
        shift: flags.contains(NSEventModifierFlags::Shift),
        ctrl: flags.contains(NSEventModifierFlags::Control),
        alt: flags.contains(NSEventModifierFlags::Option),
        meta: flags.contains(NSEventModifierFlags::Command),
    }
}

pub(crate) fn local_date_time() -> Option<LocalTime> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let seconds = libc::time_t::try_from(now.as_secs()).ok()?;
    // SAFETY: `tm` is plain integers and a pointer, for which all zeroes is
    // a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live values of the right types for the
    // whole call, and `localtime_r` keeps neither.
    let filled = unsafe { libc::localtime_r(&seconds, &mut tm) };
    if filled.is_null() {
        return None;
    }
    local_time(tm.tm_year, tm.tm_mon, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

pub(crate) fn send_chord(chord: Chord) -> Result<(), PlatformError> {
    press(virtual_key(chord.key), chord.mods)
}

pub(crate) fn send_char_chord(ch: char, mods: Modifiers) -> Result<(), PlatformError> {
    let command = mods.meta;
    let context = "reading the keyboard layout";
    // Text Input Sources belongs to the main thread. A main thread that
    // doesn't answer (one of Grape's modal panels is up) fails the chord:
    // a guess from the US table could press another key on this layout.
    let read = on_main_within(context, LAYOUT_WAIT, move |_| Ok(layout_key(ch, command))).ok_or(
        PlatformError::Os {
            context,
            code: NO_CODE,
        },
    )?;
    // If it can't be asked at all, the US table still knows the key.
    let key = read
        .unwrap_or_else(|_| us_key_for_char(ch))
        .ok_or(PlatformError::Os {
            context: "finding the key for a character",
            code: NO_CODE,
        })?;
    press(key, mods)
}

pub(crate) fn type_text(text: &str, cancel: &AtomicBool) -> Result<(), PlatformError> {
    let pieces = text_chunks(text);
    if pieces.is_empty() {
        return Ok(());
    }
    let source = private_source()?;
    for (index, piece) in pieces.iter().enumerate() {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        if index > 0 {
            thread::sleep(TYPE_GAP);
        }
        let events = match piece {
            // The key code doesn't matter: the event's string is the text.
            TextPiece::Units(units) => {
                key_pair(&source, 0, Modifiers::NONE, Some(units.as_slice()))
            }
            TextPiece::Return => {
                key_pair(&source, virtual_key(KeyCode::Enter), Modifiers::NONE, None)
            }
            TextPiece::Tab => key_pair(&source, virtual_key(KeyCode::Tab), Modifiers::NONE, None),
        }?;
        post_pair(&events, TYPE_GAP);
    }
    Ok(())
}

pub(crate) fn clipboard_text() -> Result<Option<String>, PlatformError> {
    autoreleasepool(|_| {
        // SAFETY: NSPasteboardTypeString is an immutable NSString constant
        // that AppKit defines for the life of the process.
        let string_type = unsafe { NSPasteboardTypeString };
        Ok(NSPasteboard::generalPasteboard()
            .stringForType(string_type)
            .map(|text| text.to_string()))
    })
}

pub(crate) fn clipboard_change_count() -> Result<i64, PlatformError> {
    let count = autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount());
    i64::try_from(count).map_err(|_| PlatformError::Os {
        context: "NSPasteboard changeCount",
        code: NO_CODE,
    })
}

pub(crate) fn clear_clipboard() -> Result<(), PlatformError> {
    autoreleasepool(|_| {
        NSPasteboard::generalPasteboard().clearContents();
    });
    Ok(())
}

/// Presses and lets go of `key` with `mods`.
fn press(key: u16, mods: Modifiers) -> Result<(), PlatformError> {
    let source = private_source()?;
    // Both events are made before either is posted, so a key never goes
    // down without its up.
    let events = key_pair(&source, key, mods, None)?;
    post_pair(&events, KEY_GAP);
    Ok(())
}

/// An event source with no keys held, so nothing the user holds is read
/// into the events made from it.
fn private_source() -> Result<CFRetained<CGEventSource>, PlatformError> {
    CGEventSource::new(CGEventSourceStateID::Private).ok_or(PlatformError::Os {
        context: "CGEventSourceCreate",
        code: NO_CODE,
    })
}

/// A key down and a key up of `key`. Of Shift, Control, Option and Command
/// they carry exactly `mods`; with `text` they type those UTF-16 units
/// whatever the key is.
fn key_pair(
    source: &CGEventSource,
    key: u16,
    mods: Modifiers,
    text: Option<&[u16]>,
) -> Result<[CFRetained<CGEvent>; 2], PlatformError> {
    let make = |down: bool| {
        let event =
            CGEvent::new_keyboard_event(Some(source), key, down).ok_or(PlatformError::Os {
                context: "CGEventCreateKeyboardEvent",
                code: NO_CODE,
            })?;
        let own = CGEvent::flags(Some(&event)).0;
        let flags = (own & !CG_CHORD_FLAGS) | cg_flags(mods);
        CGEvent::set_flags(Some(&event), CGEventFlags(flags));
        if let Some(units) = text {
            // SAFETY: `units` points at `units.len()` UTF-16 units that
            // live past the call, and the event copies them.
            unsafe {
                CGEvent::keyboard_set_unicode_string(
                    Some(&event),
                    units.len() as c_ulong,
                    units.as_ptr(),
                );
            }
        }
        Ok(event)
    };
    Ok([make(true)?, make(false)?])
}

/// Posts a key down, waits `gap`, and posts its key up.
fn post_pair(events: &[CFRetained<CGEvent>; 2], gap: Duration) {
    let [down, up] = events;
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(down));
    thread::sleep(gap);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(up));
}

/// The virtual key that types `ch` on the current keyboard layout, or on
/// the US one when the layout can't be read or has no such key. Main thread
/// only: Text Input Sources isn't safe anywhere else.
fn layout_key(ch: char, command: bool) -> Option<u16> {
    // SAFETY: takes nothing and returns an input source the caller owns
    // (the Copy rule), or null.
    let source = unsafe { TISCopyCurrentKeyboardLayoutInputSource() };
    // SAFETY: a non-null answer is that owned input source, a CF object.
    // `CFRetained` releases it when this function returns, after the last
    // use of the layout bytes it owns.
    let source = source.map(|source| unsafe { CFRetained::from_raw(source) });
    let layout = source
        .as_deref()
        .and_then(unicode_layout)
        .filter(|data| data.length() > 0)
        .map(CFData::byte_ptr)
        .filter(|bytes| !bytes.is_null());
    // SAFETY: takes nothing and reads the keyboard's type from low memory.
    let keyboard = u32::from(unsafe { LMGetKbdType() });
    let typed = |key: u16, with_command: bool| -> Option<char> {
        let layout = layout?;
        let mut dead_keys = 0_u32;
        let mut units = [0_u16; 4];
        let mut length: c_ulong = 0;
        // SAFETY: `layout` points at the layout's `UCKeyboardLayout` bytes,
        // which `source` keeps alive (see above). The three out pointers
        // are to live locals, and `units` has room for the four units the
        // call is told it may write.
        let status = unsafe {
            UCKeyTranslate(
                layout.cast(),
                key,
                KEY_ACTION_DOWN,
                layout_modifier_state(with_command),
                keyboard,
                NO_DEAD_KEYS,
                &mut dead_keys,
                units.len() as c_ulong,
                &mut length,
                units.as_mut_ptr(),
            )
        };
        if status != 0 {
            return None;
        }
        let written = units.get(..usize::try_from(length).ok()?)?;
        let mut chars = char::decode_utf16(written.iter().copied());
        match (chars.next(), chars.next()) {
            (Some(Ok(one)), None) => Some(one),
            _ => None,
        }
    };
    char_key(ch, command, typed)
}

/// The Unicode key layout of the input source `source`, if it has one (an
/// old layout may not). The data belongs to `source` and lives as long as
/// it does.
fn unicode_layout(source: &CFType) -> Option<&CFData> {
    // SAFETY: an immutable CFString constant that Carbon defines for the
    // life of the process.
    let key = unsafe { TIS_PROPERTY_UNICODE_KEY_LAYOUT_DATA };
    // SAFETY: `source` is a live input source and `key` one of its
    // property keys. The answer is null or a CF object.
    let data = unsafe { TISGetInputSourceProperty(source, key) }?;
    // SAFETY: the Get rule: the object belongs to `source` and stays valid
    // while `source` does, which is the lifetime this function returns it
    // with. Its type is checked before it is read as a CFData.
    let data: &CFType = unsafe { data.as_ref() };
    data.downcast_ref::<CFData>()
}

/// The macOS [`PointerWatch`]: one polling thread.
#[derive(Debug, Default)]
pub(crate) struct MacPointerWatch {
    running: Option<Running>,
}

#[derive(Debug)]
struct Running {
    shared: Arc<Shared>,
    worker: JoinHandle<()>,
}

/// What the watch and its thread share.
#[derive(Debug, Default)]
struct Shared {
    stop: AtomicBool,
    /// The watch is on.
    active: AtomicBool,
    /// Counts the times the watch was turned on, so the thread starts over
    /// even when it never saw it off in between.
    turns: AtomicU64,
}

pub(crate) fn pointer_watch() -> Result<Box<dyn PointerWatch>, PlatformError> {
    Ok(Box::new(MacPointerWatch::default()))
}

impl PointerWatch for MacPointerWatch {
    fn start(&mut self, tx: SyncSender<PointerSignal>) -> Result<(), PlatformError> {
        if self.running.is_some() {
            return Err(PlatformError::AlreadyRunning);
        }
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("grape-pointer".to_owned())
            .spawn(move || watch(&tx, &worker_shared))
            .map_err(|err| io_error("spawning the pointer thread", &err))?;
        self.running = Some(Running { shared, worker });
        Ok(())
    }

    fn set_active(&mut self, on: bool) {
        let Some(running) = &self.running else {
            return;
        };
        let shared = &running.shared;
        if !on {
            shared.active.store(false, Ordering::Release);
        } else if !shared.active.load(Ordering::Acquire) {
            shared.turns.fetch_add(1, Ordering::AcqRel);
            shared.active.store(true, Ordering::Release);
        }
        running.worker.thread().unpark();
    }

    fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.shared.stop.store(true, Ordering::Release);
            // Wake it from its wait, so stopping never waits one out.
            running.worker.thread().unpark();
            let _ = running.worker.join();
        }
    }
}

impl Drop for MacPointerWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The pointer thread: parked while the watch is off, polling while on.
fn watch(tx: &SyncSender<PointerSignal>, shared: &Shared) {
    let mut edges = PointerEdges::default();
    let mut turn = 0;
    let mut primary_height = 0.0;
    // Held only while the watch is on.
    let mut awake: Option<Awake> = None;
    while !shared.stop.load(Ordering::Acquire) {
        if !shared.active.load(Ordering::Acquire) {
            awake = None;
            // `set_active` and `stop` unpark. An early wake only looks
            // again.
            thread::park();
            continue;
        }
        let now = shared.turns.load(Ordering::Acquire);
        if now != turn {
            // Turned on since the last poll: start over, even if this
            // thread never saw the watch off in between.
            turn = now;
            edges.restart();
            primary_height = main_screen_height();
        }
        if awake.is_none() {
            awake = Some(autoreleasepool(|_| Awake::begin()));
        }
        let signals = autoreleasepool(|_| {
            let at = NSEvent::mouseLocation();
            let down = NSEvent::pressedMouseButtons() & LEFT_BUTTON != 0;
            edges.poll(cursor_to_grape(at.x, at.y, primary_height), down)
        });
        for signal in signals {
            // Drop rather than block: a full channel loses the signal.
            let _ = tx.try_send(signal);
        }
        thread::park_timeout(POINTER_EVERY);
    }
}

/// The primary (menu-bar) screen's height in points, from CoreGraphics,
/// which answers on any thread.
fn main_screen_height() -> f64 {
    CGDisplayBounds(CGMainDisplayID()).size.height
}

/// An `NSProcessInfo` activity that keeps App Nap from stretching the
/// pointer polls while a wheel is open, as the drag poller's does. It ends
/// when dropped, on the thread that began it.
struct Awake {
    process: Retained<NSProcessInfo>,
    activity: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl Awake {
    fn begin() -> Self {
        let process = NSProcessInfo::processInfo();
        let activity = process.beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
            ns_string!("Following the pointer over a wheel"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn the_watch_starts_and_stops() {
        let mut watch = MacPointerWatch::default();
        // Stopping and switching before starting are harmless.
        watch.stop();
        watch.set_active(true);
        let (tx, rx) = mpsc::sync_channel(16);
        watch.start(tx.clone()).unwrap();
        assert_eq!(watch.start(tx), Err(PlatformError::AlreadyRunning));
        // Off until it is turned on: nothing is sent.
        assert!(rx.recv_timeout(Duration::from_millis(60)).is_err());
        watch.stop();
        watch.stop();
    }

    #[test]
    fn turning_the_watch_on_says_where_the_pointer_is_once() {
        let mut watch = MacPointerWatch::default();
        let (tx, rx) = mpsc::sync_channel(64);
        watch.start(tx).unwrap();
        watch.set_active(true);
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(first, PointerSignal::Moved { .. }));
        watch.set_active(false);
        // Whatever was in flight, then silence.
        thread::sleep(Duration::from_millis(60));
        while rx.try_recv().is_ok() {}
        assert!(rx.recv_timeout(Duration::from_millis(60)).is_err());
        // On again: the position again, though nothing moved.
        watch.set_active(true);
        let again = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(again, PointerSignal::Moved { .. }));
        watch.stop();
    }

    #[test]
    fn the_clock_is_read_to_the_minute() {
        let now = local_date_time().unwrap();
        assert!(now.year >= 2025);
        assert!((1..=12).contains(&now.month) && (1..=31).contains(&now.day));
        assert!(now.hour < 24 && now.minute < 60);
    }

    #[test]
    fn input_access_is_one_of_the_two_answers_and_never_prompts() {
        // CI's Mac hasn't granted the test binary anything; a developer's
        // may have.
        assert_ne!(input_access(), InputAccess::NotNeeded);
    }

    #[test]
    fn the_held_modifiers_are_read_without_a_permission() {
        // Nobody holds a key on CI's Mac, but a developer might.
        let _ = held_modifiers();
    }
}
