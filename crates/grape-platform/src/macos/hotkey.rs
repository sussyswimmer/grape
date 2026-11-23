//! Global hot keys for the Shortcuts wheel: Carbon's `RegisterEventHotKey`
//! (`docs/design/m10-shortcuts-wheel.md` §4), which asks for no permission
//! and needs no key monitor.
//!
//! Everything Carbon runs on the main thread. The registrations, the event
//! handler and the channel live in a table only the main thread has
//! ([`REGISTRY`]), so no `EventHotKeyRef` ever crosses a thread.
//! [`MacHotkeys`] itself holds a number and is `Send`: `start` and `bind`
//! hop to the main thread and wait for the answer, so never call them from
//! a thread the main thread is waiting for. `stop` doesn't wait off the
//! main thread: it queues its work there, in order with later calls, so
//! Quit can't hang on it.
//!
//! One handler on the application's event target hears every hot key going
//! down and up. Each registration's event id says what it is for
//! (`crate::mac_rules::hotkey_event_id`), so the handler looks nothing up.
//! When a key goes down it reads the pressed buttons, the cursor and the app
//! in front, before anything of Grape's shows, then `try_send`s: a full
//! channel drops the signal. An Open press while one of Grape's modal
//! windows is up is not sent at all (`crate::mac_rules::passes_press`): the
//! chord stays Grape's, and no wheel opens over the panel.
//!
//! Carbon matches modifiers exactly, and a chord fires one thing: a chord
//! another id already holds is left to it. A bind registers its new chords
//! before it lets go of the old ones, so a refused chord leaves the id as
//! it was. An unregister the OS refuses is logged by its status and tried
//! again at the next bind: a bare Esc left registered would be dead in
//! every app until Grape quits.
//!
//! Nothing here logs a chord, a bundle id or a name.

use std::cell::RefCell;
use std::ffi::{c_ulong, c_void};
use std::panic::AssertUnwindSafe;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;

use dispatch2::DispatchQueue;
use objc2::rc::autoreleasepool;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSEvent};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};

use super::shortcuts::{front_app_now, on_main};
use super::{display, NO_CODE};
use crate::keys::Chord;
use crate::mac_rules::{
    bind_plan, carbon_modifiers, cursor_to_grape, hotkey_event_id, hotkey_for_event_id,
    passes_press, virtual_key, HeldChord, HOTKEY_SIGNATURE,
};
use crate::shortcuts::{HotkeyId, HotkeyMonitor, HotkeySignal, PressContext};
use crate::PlatformError;

/// Carbon's `OSStatus`.
type OsStatus = i32;
/// Carbon's `EventTargetRef`.
type EventTargetRef = *mut c_void;
/// Carbon's `EventHotKeyRef`: one registered chord.
type EventHotKeyRef = *mut c_void;
/// Carbon's `EventHandlerRef`: one installed handler.
type EventHandlerRef = *mut c_void;
/// Carbon's `EventHandlerCallRef`.
type EventHandlerCallRef = *mut c_void;
/// Carbon's `EventRef`.
type EventRef = *mut c_void;
/// Carbon's `EventHandlerUPP`, a plain function pointer on macOS.
type EventHandlerUpp = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OsStatus;

/// `noErr`.
const NO_ERR: OsStatus = 0;
/// `eventNotHandledErr`: the event goes on to the next handler.
const EVENT_NOT_HANDLED: OsStatus = -9874;
/// `kEventClassKeyboard`.
const EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
/// `kEventHotKeyPressed`.
const EVENT_HOT_KEY_PRESSED: u32 = 5;
/// `kEventHotKeyReleased`.
const EVENT_HOT_KEY_RELEASED: u32 = 6;
/// `kEventParamDirectObject`.
const PARAM_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
/// `typeEventHotKeyID`.
const TYPE_EVENT_HOT_KEY_ID: u32 = u32::from_be_bytes(*b"hkid");

/// Carbon's `EventHotKeyID`.
#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

/// Carbon's `EventTypeSpec`.
#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

/// The two kinds of event the handler hears.
const HOT_KEY_EVENTS: [EventTypeSpec; 2] = [
    EventTypeSpec {
        event_class: EVENT_CLASS_KEYBOARD,
        event_kind: EVENT_HOT_KEY_PRESSED,
    },
    EventTypeSpec {
        event_class: EVENT_CLASS_KEYBOARD,
        event_kind: EVENT_HOT_KEY_RELEASED,
    },
];

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyId,
        target: EventTargetRef,
        options: u32,
        hot_key: *mut EventHotKeyRef,
    ) -> OsStatus;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OsStatus;
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerUpp,
        count: c_ulong,
        types: *const EventTypeSpec,
        user_data: *mut c_void,
        installed: *mut EventHandlerRef,
    ) -> OsStatus;
    fn RemoveEventHandler(handler: EventHandlerRef) -> OsStatus;
    fn GetEventKind(event: EventRef) -> u32;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        wanted_type: u32,
        actual_type: *mut u32,
        size: c_ulong,
        actual_size: *mut c_ulong,
        data: *mut c_void,
    ) -> OsStatus;
}

/// One chord the OS holds for Grape, with Carbon's handle to it.
struct Registration {
    held: HeldChord,
    hot_key: EventHotKeyRef,
}

/// The main thread's hot key state. One monitor owns it at a time.
struct Registry {
    /// The monitor that started or bound last, until it stops.
    owner: Option<u64>,
    /// The installed handler.
    handler: Option<EventHandlerRef>,
    /// Where signals go while a monitor is started.
    tx: Option<SyncSender<HotkeySignal>>,
    /// Every chord the OS holds, the stuck ones included.
    held: Vec<Registration>,
    /// Counts registrations, for their event ids.
    serial: u32,
}

thread_local! {
    /// The hot key state, on the main thread. Any other thread would see an
    /// empty table of its own, and nothing ever looks there.
    static REGISTRY: RefCell<Registry> = const {
        RefCell::new(Registry {
            owner: None,
            handler: None,
            tx: None,
            held: Vec::new(),
            serial: 0,
        })
    };
}

/// Gives each monitor a number of its own.
static NEXT_MONITOR: AtomicU64 = AtomicU64::new(1);

/// The macOS [`HotkeyMonitor`].
#[derive(Debug)]
pub(crate) struct MacHotkeys {
    /// This monitor's number in the main thread's table.
    token: u64,
    /// It started or bound, so the table may hold something of its own.
    used: bool,
}

impl MacHotkeys {
    fn new() -> Self {
        Self {
            token: NEXT_MONITOR.fetch_add(1, Ordering::Relaxed),
            used: false,
        }
    }
}

pub(crate) fn hotkey_monitor() -> Result<Box<dyn HotkeyMonitor>, PlatformError> {
    Ok(Box::new(MacHotkeys::new()))
}

impl HotkeyMonitor for MacHotkeys {
    fn start(&mut self, tx: SyncSender<HotkeySignal>) -> Result<(), PlatformError> {
        let token = self.token;
        self.used = true;
        on_main("installing the hot key handler", move |_| {
            with_registry(|registry| registry.start(token, tx))
        })
    }

    fn bind(&mut self, id: HotkeyId, chords: &[Chord]) -> Result<(), PlatformError> {
        let token = self.token;
        self.used = true;
        on_main("binding a hot key", move |_| {
            with_registry(|registry| registry.bind(token, id, chords))
        })
    }

    fn stop(&mut self) {
        if !std::mem::take(&mut self.used) {
            return;
        }
        let token = self.token;
        let work = move || {
            let _ = with_registry(|registry| {
                registry.stop(token);
                Ok(())
            });
        };
        if MainThreadMarker::new().is_some() {
            work();
        } else {
            // Queued, not awaited: the main thread may be waiting for this
            // one at Quit. A later start or bind queues behind it.
            DispatchQueue::main().exec_async(work);
        }
    }
}

impl Drop for MacHotkeys {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Runs `work` on the main thread's table. An error when the table is in
/// use, which would take a Carbon call that dispatches events while it
/// runs.
fn with_registry<R>(
    work: impl FnOnce(&mut Registry) -> Result<R, PlatformError>,
) -> Result<R, PlatformError> {
    let busy = PlatformError::Os {
        context: "reaching the hot key table",
        code: NO_CODE,
    };
    let answer = REGISTRY.try_with(|registry| match registry.try_borrow_mut() {
        Ok(mut registry) => Some(work(&mut registry)),
        Err(_) => None,
    });
    match answer {
        Ok(Some(result)) => result,
        Ok(None) | Err(_) => Err(busy),
    }
}

impl Registry {
    /// Installs the handler and keeps `tx` for it.
    fn start(&mut self, token: u64, tx: SyncSender<HotkeySignal>) -> Result<(), PlatformError> {
        if self.tx.is_some() || self.owner.is_some_and(|owner| owner != token) {
            return Err(PlatformError::AlreadyRunning);
        }
        if self.handler.is_none() {
            let mut handler: EventHandlerRef = ptr::null_mut();
            // SAFETY: the application's event target lives as long as the
            // process. `on_hot_key` has the signature Carbon calls and is a
            // plain function, so it never dangles. `HOT_KEY_EVENTS` holds
            // the two specs the count says, and Carbon copies them.
            // `handler` is a valid place for the new handler's ref.
            let status = unsafe {
                InstallEventHandler(
                    GetApplicationEventTarget(),
                    on_hot_key,
                    HOT_KEY_EVENTS.len() as c_ulong,
                    HOT_KEY_EVENTS.as_ptr(),
                    ptr::null_mut(),
                    &mut handler,
                )
            };
            if status != NO_ERR {
                return Err(PlatformError::Os {
                    context: "InstallEventHandler",
                    code: status,
                });
            }
            self.handler = Some(handler);
        }
        self.owner = Some(token);
        self.tx = Some(tx);
        Ok(())
    }

    /// Makes `id` fire for exactly `chords` (`crate::mac_rules::bind_plan`).
    fn bind(&mut self, token: u64, id: HotkeyId, chords: &[Chord]) -> Result<(), PlatformError> {
        if self.owner.is_some_and(|owner| owner != token) {
            return Err(PlatformError::AlreadyRunning);
        }
        self.owner = Some(token);
        self.retry_stuck();
        let held: Vec<HeldChord> = self.held.iter().map(|entry| entry.held).collect();
        let plan = bind_plan(&held, id, chords);
        // The new chords first: when the OS refuses one, the id keeps what
        // it had.
        let mut fresh = Vec::with_capacity(plan.register.len());
        for chord in &plan.register {
            match self.register(id, *chord) {
                Ok(hot_key) => fresh.push(Registration {
                    held: HeldChord {
                        id,
                        chord: *chord,
                        stuck: false,
                    },
                    hot_key,
                }),
                Err(status) => {
                    for registration in fresh {
                        self.let_go(registration);
                    }
                    return Err(PlatformError::Os {
                        context: "RegisterEventHotKey",
                        code: status,
                    });
                }
            }
        }
        for place in &plan.adopt {
            if let Some(entry) = self.held.get_mut(*place) {
                entry.held.stuck = false;
            }
        }
        // From the last place down, so the places before it stay right. A
        // chord that sticks goes to the end, past every place still to go.
        for place in plan.release.iter().rev() {
            if *place < self.held.len() {
                let registration = self.held.remove(*place);
                self.let_go(registration);
            }
        }
        self.held.extend(fresh);
        Ok(())
    }

    /// Lets go of every chord and stops listening.
    fn stop(&mut self, token: u64) {
        if self.owner != Some(token) {
            return;
        }
        self.tx = None;
        for registration in std::mem::take(&mut self.held) {
            self.let_go(registration);
        }
        if let Some(handler) = self.handler.take() {
            // SAFETY: `handler` is the ref InstallEventHandler gave, and it
            // is removed only here, once: `take` emptied the slot.
            let status = unsafe { RemoveEventHandler(handler) };
            if status != NO_ERR {
                log::warn!("hot keys: removing the handler failed (OSStatus {status})");
                // Still installed, so the next start must not add a second.
                self.handler = Some(handler);
            }
        }
        self.owner = None;
    }

    /// Registers one chord for `id`.
    fn register(&mut self, id: HotkeyId, chord: Chord) -> Result<EventHotKeyRef, OsStatus> {
        self.serial = self.serial.wrapping_add(1);
        let event_id = EventHotKeyId {
            signature: HOTKEY_SIGNATURE,
            id: hotkey_event_id(id, self.serial),
        };
        let mut hot_key: EventHotKeyRef = ptr::null_mut();
        // SAFETY: plain integers and a two-`u32` struct by value, as
        // Carbon's header declares them. The application's event target
        // lives as long as the process, and `hot_key` is a valid place for
        // the new registration's ref.
        let status = unsafe {
            RegisterEventHotKey(
                u32::from(virtual_key(chord.key)),
                carbon_modifiers(chord.mods),
                event_id,
                GetApplicationEventTarget(),
                0,
                &mut hot_key,
            )
        };
        match status {
            NO_ERR if !hot_key.is_null() => Ok(hot_key),
            NO_ERR => Err(NO_CODE),
            refused => Err(refused),
        }
    }

    /// Unregisters one chord. A refusal is logged and the chord kept as
    /// stuck, for the next bind to try again.
    fn let_go(&mut self, mut registration: Registration) {
        // SAFETY: `hot_key` is a ref RegisterEventHotKey gave on this
        // thread and it hasn't been unregistered: a registration leaves the
        // table only through here, and only a refused one comes back.
        let status = unsafe { UnregisterEventHotKey(registration.hot_key) };
        if status != NO_ERR {
            log::warn!(
                "hot keys: an unregister failed (OSStatus {status}); trying again at the next bind"
            );
            registration.held.stuck = true;
            self.held.push(registration);
        }
    }

    /// Tries again to unregister the chords an earlier unregister left.
    fn retry_stuck(&mut self) {
        let (stuck, live): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|entry| entry.held.stuck);
        self.held = live;
        for registration in stuck {
            self.let_go(registration);
        }
    }
}

/// Carbon's callback for a hot key going down or up. Never unwinds: Carbon's
/// frames are above it.
extern "C" fn on_hot_key(
    _call: EventHandlerCallRef,
    event: EventRef,
    _user_data: *mut c_void,
) -> OsStatus {
    match std::panic::catch_unwind(|| handle(event)) {
        Ok(true) => NO_ERR,
        Ok(false) | Err(_) => EVENT_NOT_HANDLED,
    }
}

/// Reads one hot key event and sends its signal. `false` when the event
/// isn't one of Grape's hot keys.
fn handle(event: EventRef) -> bool {
    if event.is_null() {
        return false;
    }
    // SAFETY: `event` is the event Carbon is dispatching to this handler,
    // valid for the whole call.
    let pressed = match unsafe { GetEventKind(event) } {
        EVENT_HOT_KEY_PRESSED => true,
        EVENT_HOT_KEY_RELEASED => false,
        _ => return false,
    };
    let mut hot_key = EventHotKeyId {
        signature: 0,
        id: 0,
    };
    // SAFETY: `event` as above. `hot_key` is a writable `EventHotKeyID` of
    // the size passed, which is what the `hkid` type copies out. The two
    // null pointers are outputs Carbon documents as optional.
    let status = unsafe {
        GetEventParameter(
            event,
            PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            ptr::null_mut(),
            size_of::<EventHotKeyId>() as c_ulong,
            ptr::null_mut(),
            (&raw mut hot_key).cast(),
        )
    };
    if status != NO_ERR || hot_key.signature != HOTKEY_SIGNATURE {
        return false;
    }
    let Some(id) = hotkey_for_event_id(hot_key.id) else {
        return false;
    };
    // No monitor is listening (or this isn't the main thread): the key is
    // still Grape's, so the app in front doesn't get it.
    let Some(tx) = sender() else {
        return true;
    };
    // Only an Open press asks: the answer matters to nothing else.
    if pressed && !passes_press(id, id == HotkeyId::Open && modal_window_up()) {
        log::debug!("hot keys: Open ignored while a modal window is up");
        // Still Grape's: the panel must not get the chord as a key.
        return true;
    }
    let signal = if pressed {
        HotkeySignal::Pressed(id, press_context(id))
    } else {
        HotkeySignal::Released(id)
    };
    // Drop rather than block: see the module docs.
    let _ = tx.try_send(signal);
    true
}

/// The channel of the started monitor. The table is borrowed only for the
/// clone, and never waited for.
fn sender() -> Option<SyncSender<HotkeySignal>> {
    REGISTRY
        .try_with(|registry| {
            registry
                .try_borrow()
                .ok()
                .and_then(|registry| registry.tx.clone())
        })
        .ok()
        .flatten()
}

/// `true` while Grape runs a modal window: a picker, Move's panel or its
/// alert. `false` off the main thread, where nobody can know, and after an
/// Objective-C exception.
fn modal_window_up() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    autoreleasepool(|_| {
        objc2::exception::catch(AssertUnwindSafe(|| {
            NSApplication::sharedApplication(mtm)
                .modalWindow()
                .is_some()
        }))
        .unwrap_or(false)
    })
}

/// What the OS says as a hot key goes down. An Objective-C exception
/// leaves a context with no cursor, which opens nothing.
fn press_context(id: HotkeyId) -> PressContext {
    autoreleasepool(|_| {
        objc2::exception::catch(AssertUnwindSafe(|| read_press(id))).unwrap_or(PressContext {
            cursor: None,
            front: None,
            button_down: false,
        })
    })
}

/// The pressed buttons, the cursor and the app in front.
fn read_press(id: HotkeyId) -> PressContext {
    let button_down = NSEvent::pressedMouseButtons() != 0;
    let main_thread = MainThreadMarker::new();
    if let (HotkeyId::Open, Some(mtm)) = (id, main_thread) {
        // The wheel is about to pick its display: read the screens now.
        display::refresh(mtm);
    }
    // CoreGraphics answers on any thread, as for the drag poller.
    let primary_height = CGDisplayBounds(CGMainDisplayID()).size.height;
    let cursor = (primary_height > 0.0).then(|| {
        let at = NSEvent::mouseLocation();
        cursor_to_grape(at.x, at.y, primary_height)
    });
    let front = main_thread.and_then(front_app_now);
    PressContext {
        cursor,
        front,
        button_down,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_monitor_that_did_nothing_stops_without_the_main_thread() {
        // Nothing serves the main queue under `cargo test`: a stop that
        // went there would be lost, and a waiting one would hang.
        let mut monitor = hotkey_monitor().unwrap();
        monitor.stop();
        monitor.stop();
    }

    #[test]
    fn each_monitor_has_a_number_of_its_own() {
        // The main thread's table tells its owner from any other monitor
        // by this number.
        let (first, second) = (MacHotkeys::new(), MacHotkeys::new());
        assert_ne!(first.token, second.token);
        assert!(!first.used && !second.used);
    }

    #[test]
    fn carbon_structs_are_two_words_each() {
        assert_eq!(size_of::<EventHotKeyId>(), 8);
        assert_eq!(size_of::<EventTypeSpec>(), 8);
        assert_eq!(EVENT_CLASS_KEYBOARD, 0x6B65_7962);
        assert_eq!(PARAM_DIRECT_OBJECT, 0x2D2D_2D2D);
        assert_eq!(TYPE_EVENT_HOT_KEY_ID, 0x686B_6964);
    }

    #[test]
    fn an_event_that_is_not_there_is_not_handled() {
        // Returns before any Carbon call.
        assert!(!handle(ptr::null_mut()));
        assert_eq!(
            on_hot_key(ptr::null_mut(), ptr::null_mut(), ptr::null_mut()),
            EVENT_NOT_HANDLED
        );
    }

    #[test]
    fn a_thread_that_is_not_the_main_one_has_no_channel() {
        assert!(sender().is_none());
    }

    #[test]
    fn no_modal_window_is_up_under_a_test() {
        // Off the main thread nobody asks AppKit; on it, no panel is open.
        assert!(!modal_window_up());
    }
}
