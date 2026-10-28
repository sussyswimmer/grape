//! Dragging items out of the float shelf on macOS (D43;
//! `docs/PLATFORM_MACOS.md` → "Float shelf"): an AppKit dragging session
//! into another app, a Finder folder or the Desktop.
//!
//! The drag only ever offers copy or link, never move (golden rule 2):
//! Grape's own [`ShelfDragSource`] answers Copy | Link in every dragging
//! context, and nothing once the drag is stale. It never offers Generic
//! (Finder's move on the same volume), Private, Move or Delete (the Dock's
//! Trash, where the source does the deleting; Grape has no code that
//! deletes). Modifier keys only filter that mask, so no key can add an
//! operation: Option copies, Control or Option+Command makes an alias, and
//! Command alone leaves nothing, so the drop is refused.
//!
//! The session is `-[NSView beginDraggingSessionWithItems:event:source:]`
//! on the shelf's content view (wry's view holding the WKWebView), called
//! through `msg_send!` with an optional return inside
//! `objc2::exception::catch`: Apple documents a nil session (macOS 15+
//! refuses a second drag), and the typed binding panics on nil. One
//! dragging item per shelf item: a file URL, or a directory URL for a
//! folder, made from the bytes of the path with no disk access, with the
//! icon of its type (`iconForFileType`, which reads Launch Services, never
//! the file). WebKit consumed the real mouse-down, so the event is a fresh
//! `LeftMouseDragged` at the pointer, as Electron, Chromium and drag-rs
//! build it; only its timestamp comes from `NSApp.currentEvent`, which can
//! be a private event type.
//!
//! The call returns at once and the drag begins on the next turn of the
//! run loop, so [`drag_out`] can't return how it ended, unlike Windows'.
//! The source's `draggingSession:endedAtPoint:operation:` reports it: a
//! drop on one of Grape's own windows is cancelled (wry answers every drop
//! there with Copy and ignores it), otherwise Copy, Link or nothing. The
//! left button is read before anything is built, right before the call,
//! and again when AppKit begins the drag; up by then, the mask turns None,
//! so nothing can drop it. No Esc watchdog: the session doesn't hold the
//! main thread, and Esc is AppKit's own.
//!
//! [`own_drags`] counts the drags Grape asked AppKit to start, so the drag
//! poller never takes one of them for a Finder drag and opens the wheel
//! over it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApp, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource,
    NSEvent, NSEventModifierFlags, NSEventType, NSImage, NSWindow, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSObject, NSPoint, NSProcessInfo, NSRect, NSSize, NSString, NSURL,
};

use super::drag::LEFT_BUTTON;
use super::window::retain_window;
use super::NO_CODE;
use crate::mac_rules::{
    drag_icon_type, drag_item_frames, drag_mask, drag_outcome, unexpected_operation,
};
use crate::{DragItem, DragOutcome, PlatformError};

/// How [`drag_out`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragStart {
    /// AppKit took the drag. Its `ended` is called exactly once, later, on
    /// the main thread, when the drag is over.
    Started,
    /// No drag: the button was already up ([`DragOutcome::ButtonUp`]) or
    /// there was nothing to drag ([`DragOutcome::NothingToDrag`]). `ended`
    /// is dropped uncalled.
    NotStarted(DragOutcome),
}

/// The side of a drag item's image, in points.
const ICON_SIZE: f64 = 48.0;

/// How many sources are kept alive at most: the running drag's and a few
/// that never reported their end.
const KEPT_AT_MOST: usize = 4;

/// How many drags out Grape has asked AppKit to start. Counted right before
/// the call, which is what changes the drag pasteboard.
static OWN_DRAGS: AtomicU64 = AtomicU64::new(0);

/// How many drags out of the shelf Grape has asked AppKit to start since
/// launch. The drag poller compares it across a press: if it moved, the
/// file drag it sees is Grape's own and never opens the wheel.
pub(super) fn own_drags() -> u64 {
    OWN_DRAGS.load(Ordering::SeqCst)
}

/// Starts a drag of `items` out of the shelf window `ns_window`, in their
/// order: copy or link, never move. Returns at once.
///
/// - `Ok(DragStart::Started)`: AppKit took the drag, and `ended` is called
///   exactly once, later, on the main thread, with how it ended
///   ([`DragOutcome::Copied`], [`DragOutcome::Linked`] or
///   [`DragOutcome::Cancelled`]).
/// - `Ok(DragStart::NotStarted(_))`: nothing to drag, or the left button
///   was already up (a drag then would drop at once, wherever the pointer
///   is). `ended` is dropped uncalled.
/// - `Err(_)`: no drag either, and `ended` is dropped uncalled.
///
/// Folders are told apart by [`DragItem::folder`], never by the disk.
/// Items whose path isn't absolute, or can't be a file URL, are left out.
/// `ended` must not panic or wait for the main thread.
///
/// # Safety
/// `ns_window` must be null or the `NSWindow` pointer Tauri's `ns_window()`
/// just returned on this thread, with no autorelease pool drained since.
///
/// # Errors
/// - [`PlatformError::WrongThread`] off the main thread.
/// - [`PlatformError::Os`] for a null pointer, a window without a content
///   view, an event AppKit wouldn't make, a nil session, or an exception.
pub unsafe fn drag_out(
    ns_window: *mut c_void,
    items: &[DragItem],
    ended: impl FnOnce(DragOutcome) + 'static,
) -> Result<DragStart, PlatformError> {
    // SAFETY: the caller's contract is the one `drag_with` needs.
    unsafe { drag_with(ns_window, items, Box::new(ended), left_button_down) }
}

/// `true` while the left mouse button is down, as the drag poller reads it
/// (the hardware state, with synthesized trackpad drags counted).
fn left_button_down() -> bool {
    NSEvent::pressedMouseButtons() & LEFT_BUTTON != 0
}

/// What the source calls once the drag is over.
type Ended = Box<dyn FnOnce(DragOutcome)>;

/// [`drag_out`] with the button read by `button_down`, so tests reach the
/// early returns without ever starting a drag.
///
/// # Safety
/// As [`drag_out`].
unsafe fn drag_with(
    ns_window: *mut c_void,
    items: &[DragItem],
    ended: Ended,
    button_down: fn() -> bool,
) -> Result<DragStart, PlatformError> {
    if items.is_empty() {
        return Ok(DragStart::NotStarted(DragOutcome::NothingToDrag));
    }
    // A drag started with the button up would drop at once.
    if !button_down() {
        return Ok(DragStart::NotStarted(DragOutcome::ButtonUp));
    }
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, mtm) = unsafe { retain_window(ns_window) }?;
    autoreleasepool(|_| {
        objc2::exception::catch(AssertUnwindSafe(|| {
            begin(&window, mtm, items, ended, button_down)
        }))
        .unwrap_or(Err(PlatformError::Os {
            context: "starting the drag out",
            code: NO_CODE,
        }))
    })
}

/// Builds the dragging items and the event, reads the button once more and
/// asks AppKit to begin the session. Main thread, inside an autorelease
/// pool and an exception catch.
fn begin(
    window: &NSWindow,
    mtm: MainThreadMarker,
    items: &[DragItem],
    ended: Ended,
    button_down: fn() -> bool,
) -> Result<DragStart, PlatformError> {
    let view = window.contentView().ok_or(PlatformError::Os {
        context: "reading the shelf's content view",
        code: NO_CODE,
    })?;
    let in_window = window.mouseLocationOutsideOfEventStream();
    // wry's view sits at the window's origin, unflipped; convert anyway.
    let in_view = view.convertPoint_fromView(in_window, None);

    let urls: Vec<(Retained<NSURL>, &DragItem)> = items
        .iter()
        .filter(|item| item.path.is_absolute())
        .filter_map(|item| {
            let url = if item.folder {
                NSURL::from_directory_path(&item.path)
            } else {
                NSURL::from_file_path(&item.path)
            };
            url.map(|url| (url, item))
        })
        .collect();
    if urls.is_empty() {
        return Ok(DragStart::NotStarted(DragOutcome::NothingToDrag));
    }

    let workspace = NSWorkspace::sharedWorkspace();
    let mut icons: HashMap<String, Retained<NSImage>> = HashMap::new();
    let frames = drag_item_frames(urls.len(), (in_view.x, in_view.y), ICON_SIZE);
    let mut dragging: Vec<Retained<NSDraggingItem>> = Vec::with_capacity(urls.len());
    for ((url, item), frame) in urls.iter().zip(frames) {
        // One pasteboard item per URL, so items from several folders go.
        let dragging_item = NSDraggingItem::initWithPasteboardWriter(
            NSDraggingItem::alloc(),
            ProtocolObject::from_ref(&**url),
        );
        let icon: &NSImage = icons
            .entry(drag_icon_type(&item.path, item.folder))
            .or_insert_with_key(|kind| icon_for_type(&workspace, kind));
        let rect = NSRect::new(
            NSPoint::new(frame.x, frame.y),
            NSSize::new(frame.width, frame.height),
        );
        // SAFETY: `contents` must be an NSImage (or a CGImage); this is an
        // NSImage, and the frame is in the view's coordinates. The image
        // isn't resized: the frame sizes it, and it may be shared.
        unsafe { dragging_item.setDraggingFrame_contents(rect, Some(icon)) };
        dragging.push(dragging_item);
    }
    // AppKit consumes the items and doesn't retain them: they go with this
    // array once the call is over.
    let dragging = NSArray::from_retained_slice(&dragging);

    // Only the timestamp: the current event may be of a private type.
    let timestamp = NSApp(mtm).currentEvent().map_or_else(
        || NSProcessInfo::processInfo().systemUptime(),
        |event| event.timestamp(),
    );
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        NSEventType::LeftMouseDragged,
        in_window,
        NSEventModifierFlags::empty(),
        timestamp,
        window.windowNumber(),
        None,
        0,
        1,
        1.0,
    )
    .ok_or(PlatformError::Os {
        context: "making the drag event",
        code: NO_CODE,
    })?;

    // Right before the call: building the items may have taken a moment.
    if !button_down() {
        return Ok(DragStart::NotStarted(DragOutcome::ButtonUp));
    }
    let source = ShelfDragSource::new(mtm, ended);
    // Before the call, which is what changes the drag pasteboard.
    OWN_DRAGS.fetch_add(1, Ordering::SeqCst);
    let as_source: &ProtocolObject<dyn NSDraggingSource> = ProtocolObject::from_ref(&*source);
    // SAFETY: `beginDraggingSessionWithItems:event:source:` is NSView's,
    // called on the main thread on the window's live content view, with an
    // NSArray of NSDraggingItem, an NSEvent and an object that conforms to
    // NSDraggingSource. It returns an autoreleased NSDraggingSession or nil,
    // which the optional return allows for.
    let session: Option<Retained<NSDraggingSession>> = unsafe {
        msg_send![&*view, beginDraggingSessionWithItems: &*dragging, event: &*event, source: as_source]
    };
    let Some(session) = session else {
        return Err(PlatformError::Os {
            context: "beginDraggingSession",
            code: NO_CODE,
        });
    };
    // Right after creating the session, as Apple asks.
    session.setAnimatesToStartingPositionsOnCancelOrFail(true);
    keep(source);
    Ok(DragStart::Started)
}

/// The icon of the type `kind` (an extension or a type identifier).
fn icon_for_type(workspace: &NSWorkspace, kind: &str) -> Retained<NSImage> {
    // Deprecated in macOS 12 for `iconForContentType:`, which would need a
    // new crate. It reads Launch Services, never the file: no stall on a
    // share that stopped answering and no prompt for a protected folder,
    // which `iconForFile:` (it reads the file's metadata) could cause.
    #[allow(deprecated)]
    workspace.iconForFileType(&NSString::from_str(kind))
}

/// Keeps `source` alive until a later drag starts: nobody relies on the
/// session retaining its source, and nothing is released from inside its
/// own callback. Sources that reported their end go now; the oldest go
/// past [`KEPT_AT_MOST`].
fn keep(source: Retained<ShelfDragSource>) {
    KEPT.with(|kept| {
        let Ok(mut kept) = kept.try_borrow_mut() else {
            // Never reentered; if it were, the session holds the source.
            log::warn!("drag out: the kept sources are busy; this one isn't kept");
            return;
        };
        kept.retain(|earlier| !earlier.has_ended());
        if !kept.is_empty() {
            log::warn!(
                "drag out: {} earlier drag(s) never reported their end",
                kept.len()
            );
        }
        while kept.len() >= KEPT_AT_MOST {
            kept.remove(0);
        }
        kept.push(source);
    });
}

thread_local! {
    /// The sources of the drags out that may still be running, on the main
    /// thread.
    static KEPT: RefCell<Vec<Retained<ShelfDragSource>>> = const { RefCell::new(Vec::new()) };
}

/// What [`ShelfDragSource`] holds.
struct SourceIvars {
    /// Called once, when the drag is over; `None` after that.
    ended: RefCell<Option<Ended>>,
    /// The button was up when AppKit began the drag: nothing may drop it.
    stale: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and
    // ShelfDragSource doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GrapeShelfDragSource"]
    #[ivars = SourceIvars]
    /// The dragging source of a drag out of the shelf: it offers copy or
    /// link only, and reports how the drag ended.
    struct ShelfDragSource;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for ShelfDragSource {}

    // SAFETY: each method has the signature NSDraggingSource declares, none
    // keeps the session past the call, and none panics (they run under
    // AppKit's frames).
    unsafe impl NSDraggingSource for ShelfDragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            // The same in every context, known or future.
            NSDragOperation(drag_mask(self.ivars().stale.get()))
        }

        #[unsafe(method(draggingSession:willBeginAtPoint:))]
        fn will_begin(&self, _session: &NSDraggingSession, _point: NSPoint) {
            // The drag begins one run-loop turn after the call: read the
            // button again. Up, nothing may drop it, whether AppKit ends the
            // session at once or leaves it on the pointer.
            if !left_button_down() {
                self.ivars().stale.set(true);
                log::warn!("drag out: the button was up when the drag began; nothing can drop it");
            }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended_at(
            &self,
            _session: &NSDraggingSession,
            point: NSPoint,
            operation: NSDragOperation,
        ) {
            self.end(point, operation);
        }

        #[unsafe(method(ignoreModifierKeysForDraggingSession:))]
        fn ignore_modifier_keys(&self, _session: &NSDraggingSession) -> bool {
            // Modifiers only filter the mask, so they can't add an
            // operation.
            false
        }
    }
);

impl ShelfDragSource {
    fn new(mtm: MainThreadMarker, ended: Ended) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SourceIvars {
            ended: RefCell::new(Some(ended)),
            stale: Cell::new(false),
        });
        // SAFETY: NSObject's designated initializer, on a fresh allocation.
        unsafe { msg_send![super(this), init] }
    }

    /// Its drag reported its end.
    fn has_ended(&self) -> bool {
        self.ivars()
            .ended
            .try_borrow()
            .is_ok_and(|ended| ended.is_none())
    }

    /// The drag is over: works out how from where it was let go and what
    /// AppKit says the target did, then calls `ended` once.
    fn end(&self, point: NSPoint, operation: NSDragOperation) {
        let mtm = self.mtm();
        // The frontmost window under the point, of any app.
        let number = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(point, 0, mtm);
        let on_grape = NSApp(mtm).windowWithWindowNumber(number).is_some();
        let unexpected = unexpected_operation(operation.0);
        if !unexpected.is_empty() {
            log::warn!("drag out: AppKit reported an operation never offered: {unexpected}");
        }
        let outcome = drag_outcome(operation.0, on_grape);
        // Taken out first, so the borrow is over before `ended` runs.
        let ended = self
            .ivars()
            .ended
            .try_borrow_mut()
            .ok()
            .and_then(|mut ended| ended.take());
        match ended {
            Some(ended) => ended(outcome),
            None => log::warn!("drag out: a drag reported its end twice"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::rc::Rc;

    use super::*;
    use crate::mac_rules::{
        DRAG_COPY, DRAG_DELETE, DRAG_GENERIC, DRAG_LINK, DRAG_MOVE, DRAG_PRIVATE,
    };

    fn items() -> Vec<DragItem> {
        vec![
            DragItem {
                path: PathBuf::from("/a/one.txt"),
                folder: false,
            },
            DragItem {
                path: PathBuf::from("/b/Invoices"),
                folder: true,
            },
        ]
    }

    fn up() -> bool {
        false
    }

    fn down() -> bool {
        true
    }

    /// An `ended` that records whether it ran.
    fn recorder() -> (Rc<Cell<bool>>, Ended) {
        let called = Rc::new(Cell::new(false));
        let for_ended = Rc::clone(&called);
        (called, Box::new(move |_| for_ended.set(true)))
    }

    #[test]
    fn nothing_to_drag_starts_nothing() {
        let before = own_drags();
        let (called, ended) = recorder();
        // SAFETY: null is allowed by the contract and never dereferenced.
        let started = unsafe { drag_with(std::ptr::null_mut(), &[], ended, down) };
        assert_eq!(
            started,
            Ok(DragStart::NotStarted(DragOutcome::NothingToDrag))
        );
        assert!(!called.get());
        assert_eq!(own_drags(), before);
    }

    #[test]
    fn a_button_already_up_starts_nothing() {
        let before = own_drags();
        let (called, ended) = recorder();
        // SAFETY: as above.
        let started = unsafe { drag_with(std::ptr::null_mut(), &items(), ended, up) };
        assert_eq!(started, Ok(DragStart::NotStarted(DragOutcome::ButtonUp)));
        assert!(!called.get());
        assert_eq!(own_drags(), before);
    }

    #[test]
    fn a_null_window_is_refused() {
        let before = own_drags();
        let (called, ended) = recorder();
        // SAFETY: as above.
        let started = unsafe { drag_with(std::ptr::null_mut(), &items(), ended, down) };
        assert!(
            matches!(started, Err(PlatformError::Os { .. })),
            "{started:?}"
        );
        assert!(!called.get());
        assert_eq!(own_drags(), before);
    }

    #[test]
    fn the_rules_use_appkits_operation_bits() {
        assert_eq!(NSDragOperation::Copy.0, DRAG_COPY);
        assert_eq!(NSDragOperation::Link.0, DRAG_LINK);
        assert_eq!(NSDragOperation::Generic.0, DRAG_GENERIC);
        assert_eq!(NSDragOperation::Private.0, DRAG_PRIVATE);
        assert_eq!(NSDragOperation::Move.0, DRAG_MOVE);
        assert_eq!(NSDragOperation::Delete.0, DRAG_DELETE);
        assert_eq!(
            NSDragOperation(drag_mask(false)),
            NSDragOperation::Copy | NSDragOperation::Link
        );
        assert_eq!(NSDragOperation(drag_mask(true)), NSDragOperation::None);
    }
}
