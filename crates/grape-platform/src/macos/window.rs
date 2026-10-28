//! Grape's overlay, toast, tool panel and float shelf windows on macOS
//! (`docs/PLATFORM_MACOS.md` → "Overlay window" and "Float shelf").
//!
//! Each function takes the raw pointer that Tauri's
//! `WebviewWindow::ns_window()` returns. That pointer is autoreleased: it's
//! only good on the main thread, until the current autorelease pool drains.
//! So every function here runs on the main thread only (inside Tauri's
//! `setup` or `run_on_main_thread`), checks that it is, and retains the
//! window for the length of the call.
//!
//! The overlay and the toast never become key, so the frontmost app keeps
//! focus. A toast with nothing to reveal also ignores the mouse
//! ([`show_toast`]), so a click on it never activates Grape. That comes from `"focusable": false` in `tauri.conf.json`, not
//! from this module: tao's `TaoWindow` answers `canBecomeKeyWindow` and
//! `canBecomeMainWindow` from its `focusable` flag. [`frame_and_show`]
//! uses `orderFrontRegardless` because a plain `orderFront` from an
//! inactive app doesn't come above the active app's windows, and it sets
//! the frame and orders the window front in one main-thread call.
//!
//! The tool panel is the one hardened window that becomes key
//! (`"focusable": true`): [`show_panel`] orders it front the same way,
//! makes it key and asks macOS to activate Grape, so Enter and Esc reach
//! the page (D7: Grape has no key monitors). macOS may decline; the first
//! click then activates Grape and reaches the page (`acceptFirstMouse`).
//! [`hide_panel`] hands activation back to the app that was in front. It
//! sits at level 99, under the toast and an open wheel. Never `NSApp.hide`
//! and never tao's `set_focus`.
//!
//! The float shelf (D43) can become key too, but only a click makes it so:
//! [`show_shelf`] sets its frame and orders it front with
//! `orderFrontRegardless`, never key and never activating Grape, and
//! records the app in front; [`hide_shelf`] orders it out and, if a click
//! had made it key, hands activation back to the app the user was last in
//! (`super::activations`), since the shelf stays open while the user
//! switches apps; the app in front at the last show is only the fallback.
//! It sits at the floating level (3), under every other Grape window and
//! above normal windows. [`window_frame`] reads its frame in points:
//! Tauri's `outer_position` and `outer_size` give backing pixels on the
//! Mac.
//!
//! tao resets the level that [`harden_window`] sets in `set_always_on_top`
//! and `set_always_on_bottom` (queued on the main queue: floating, below
//! normal or normal level) and in `set_fullscreen` (level 0 before it
//! toggles full screen; the shielding level and back to normal for
//! exclusive full screen), so don't call them on a hardened window.
//! `set_visible_on_all_workspaces(false)` clears `CanJoinAllSpaces` from the
//! collection behavior. tao's `set_ignore_cursor_events` only sets
//! `ignoresMouseEvents` and leaves both alone.
//!
//! [`harden_window`] also turns `canHide` off, so `NSApp hide:` (the
//! default menu's Hide, Cmd+H, which the app keeps for Close Window's
//! Cmd+W) leaves these four windows on screen and they can still be
//! ordered front while Grape is hidden. Only Settings hides. tao never
//! turns `canHide` back on (it only turns it off, when it hides the Dock
//! icon).

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;

use objc2::rc::{autoreleasepool, Retained};
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSFloatingWindowLevel, NSMainMenuWindowLevel, NSModalPanelWindowLevel,
    NSNormalWindowLevel, NSPopUpMenuWindowLevel, NSRunningApplication, NSScreenSaverWindowLevel,
    NSStatusWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowLevel, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::actions::{activate, frontmost_other_app, hand_back};
use super::{activations, display, NO_CODE};
use crate::mac_rules::{to_cocoa, to_grape, CocoaRect};
use crate::{PlatformError, Rect};

/// Which of Grape's always-in-front windows is being hardened. They differ
/// in level, so the overlay always covers the toast, both cover the tool
/// panel, and all three cover the float shelf. The shelf also gets the
/// `Closable` style bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRole {
    /// The full-display wheel overlay: `NSPopUpMenuWindowLevel` (101).
    Overlay,
    /// The progress and done toast: one level lower (100). The overlay
    /// covers it while a wheel is open, so a progress update can't come up
    /// over the wheel, and it never covers another app's pop-up menu.
    Toast,
    /// A tool's panel: two levels under the overlay (99), so an open wheel
    /// and the toast always cover it, and the next drag always reaches the
    /// wheel.
    Panel,
    /// The float shelf (D43): `NSFloatingWindowLevel` (3), above normal
    /// windows but under the Dock, the menu bar, other apps' menus and every
    /// other Grape window, so the next drag's wheel covers it and takes the
    /// drop. It's long-lived and always inside the screen's visible frame,
    /// so it never needs to cover the Dock or the menu bar.
    Shelf,
}

impl WindowRole {
    /// The window level for this role.
    fn level(self) -> NSWindowLevel {
        match self {
            Self::Overlay => NSPopUpMenuWindowLevel,
            Self::Toast => NSPopUpMenuWindowLevel - 1,
            Self::Panel => NSPopUpMenuWindowLevel - 2,
            Self::Shelf => NSFloatingWindowLevel,
        }
    }
}

/// What [`show_panel`] or [`show_shelf`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelShown {
    /// The app that was in front (never Grape), to hand activation back to
    /// when the panel closes. For the shelf it's only the fallback when no
    /// app the user was last in still runs ([`hide_shelf`]).
    pub front_pid: Option<i32>,
}

/// Lifts the window over the menu bar, the Dock and full-screen apps, on
/// every Space, and keeps it out of Cmd-Tab and window cycling: the level
/// for `role` (101 for the overlay, 100 for the toast) and collection
/// behavior `CanJoinAllSpaces | FullScreenAuxiliary | Stationary |
/// IgnoresCycle` for both. It stays visible when Grape isn't the active
/// app. The level stays below 500: any higher and it would cover the image
/// the user is dragging.
///
/// It also turns `canHide` off for every role, so Hide (Cmd+H, `NSApp
/// hide:`) leaves the window alone: an Accessory app has no Dock icon to
/// unhide it from, and while Grape is hidden a window that can hide stays
/// off screen even after `orderFrontRegardless`, so the next wheel, toast,
/// panel or shelf would never appear.
///
/// For [`WindowRole::Shelf`] it also sets the `Closable` style bit, which
/// tao leaves off a window without decorations: with it, the default
/// menu's Close Window (Cmd+W) is a close request instead of a beep. No
/// `Titled` bit, so no title bar or close button appears. The first
/// responder is put back after the style change. Never tao's or Tauri's
/// `set_closable`, which makes tao's own view first responder instead of
/// the webview.
///
/// Call it once on the main thread, before the window is first shown.
/// Calling it again is harmless.
///
/// # Safety
/// `ns_window` must be null or the `NSWindow` pointer Tauri's `ns_window()`
/// just returned on this thread, with no autorelease pool drained since.
///
/// # Errors
/// - [`PlatformError::WrongThread`] off the main thread.
/// - [`PlatformError::Os`] for a null pointer, or if AppKit raised an
///   exception.
pub unsafe fn harden_window(ns_window: *mut c_void, role: WindowRole) -> Result<(), PlatformError> {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, _mtm) = unsafe { retain_window(ns_window) }?;
    let behavior = overlay_behavior(window.collectionBehavior());
    objc2::exception::catch(AssertUnwindSafe(|| {
        window.setLevel(role.level());
        window.setCollectionBehavior(behavior);
        window.setHidesOnDeactivate(false);
        window.setCanHide(false);
        if role == WindowRole::Shelf {
            let responder = window.firstResponder();
            window.setStyleMask(window.styleMask() | NSWindowStyleMask::Closable);
            if let Some(responder) = responder {
                window.makeFirstResponder(Some(&responder));
            }
        }
    }))
    .map_err(|_| PlatformError::Os {
        context: "setting the window's level, collection behavior and hiding",
        code: NO_CODE,
    })
}

/// Moves and sizes the window to `frame` (Grape's top-left points) and
/// brings it to the front without activating Grape, so the frontmost app
/// keeps focus. It goes in front of the other windows at its level, never
/// above a higher one: the toast stays under an open wheel. Also refreshes
/// the screen cache that [`crate::display_bounds_at`] reads.
///
/// # Safety
/// As [`harden_window`].
///
/// # Errors
/// - [`PlatformError::WrongThread`] off the main thread.
/// - [`PlatformError::Os`] for a null pointer, or when there are no screens.
pub unsafe fn frame_and_show(ns_window: *mut c_void, frame: Rect) -> Result<(), PlatformError> {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, mtm) = unsafe { retain_window(ns_window) }?;
    let rect = cocoa_frame(mtm, frame)?;
    autoreleasepool(|_| order_front(&window, rect));
    Ok(())
}

/// Shows the toast at `frame` as [`frame_and_show`] does, and in the same
/// main-thread call, before it's ordered front, sets whether it takes the
/// mouse. A toast with nothing to reveal (`clickable` false: working, info
/// and error toasts) ignores mouse events, so a click on it goes to the
/// window under it and never activates Grape: the toast is a plain
/// `NSWindow`, not a non-activating panel, and a click on any window of an
/// Accessory app activates it even when the window can't become key. A
/// toast that reveals its output keeps the click (`open -R` then
/// activates Finder). Every show sets it again, so an update of the same
/// toast (working to done) flips it.
///
/// # Safety
/// As [`harden_window`].
///
/// # Errors
/// - [`PlatformError::WrongThread`] off the main thread.
/// - [`PlatformError::Os`] for a null pointer, or when there are no screens.
pub unsafe fn show_toast(
    ns_window: *mut c_void,
    frame: Rect,
    clickable: bool,
) -> Result<(), PlatformError> {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, mtm) = unsafe { retain_window(ns_window) }?;
    let rect = cocoa_frame(mtm, frame)?;
    autoreleasepool(|_| {
        window.setIgnoresMouseEvents(!clickable);
        order_front(&window, rect);
    });
    Ok(())
}

/// [`frame_and_show`]'s and [`show_toast`]'s work: the frame, then
/// `orderFrontRegardless`.
fn order_front(window: &NSWindow, rect: NSRect) {
    window.setFrame_display(rect, false);
    // Never makeKeyAndOrderFront or activate: Grape must not take focus.
    // A plain orderFront from an inactive app stays behind the active
    // app's windows.
    window.orderFrontRegardless();
}

/// Hands activation back to the app the user was last in, other than
/// Grape, that still runs (`super::activations`), when Grape is the active
/// app and none of its windows is key: after a click on the toast that
/// revealed nothing, Grape would otherwise keep the keyboard with no
/// window to type in. With a key window (Settings, a tool panel, a clicked
/// shelf) the user is in Grape, so nothing changes. Returns whether it
/// handed back.
///
/// # Errors
/// [`PlatformError::WrongThread`] off the main thread.
pub fn hand_back_activation() -> Result<bool, PlatformError> {
    let mtm = MainThreadMarker::new().ok_or(PlatformError::WrongThread)?;
    Ok(autoreleasepool(|_| {
        let grape = NSApplication::sharedApplication(mtm);
        if !grape.isActive() || grape.keyWindow().is_some() {
            return false;
        }
        match activations::latest_running_app() {
            Some(app) => {
                hand_back(mtm, &app);
                true
            }
            None => false,
        }
    }))
}

/// Shows a tool panel at `frame` (Grape's top-left points) and asks for
/// the keyboard: it records the app in front (not Grape), sets the frame,
/// orders the window front with `orderFrontRegardless`, makes it key and
/// then asks macOS to activate Grape. macOS may decline; the panel is then
/// visible but inactive, and the first click activates it. An open panel
/// is only moved and sized again. Also refreshes the screen cache, as
/// [`frame_and_show`] does.
///
/// # Safety
/// As [`harden_window`]. The window must be one that can become key
/// (`"focusable": true`).
///
/// # Errors
/// - [`PlatformError::WrongThread`] off the main thread.
/// - [`PlatformError::Os`] for a null pointer, or when there are no screens.
pub unsafe fn show_panel(ns_window: *mut c_void, frame: Rect) -> Result<PanelShown, PlatformError> {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, mtm) = unsafe { retain_window(ns_window) }?;
    let rect = cocoa_frame(mtm, frame)?;
    let front_pid = autoreleasepool(|_| {
        let front_pid = frontmost_other_app(mtm).map(|app| app.processIdentifier());
        window.setFrame_display(rect, false);
        window.orderFrontRegardless();
        window.makeKeyWindow();
        activate(mtm);
        front_pid
    });
    Ok(PanelShown { front_pid })
}

/// Hides a tool panel (`orderOut`). If it was the key window and Grape is
/// still the active app, it hands activation back to the app with
/// `back_to_pid` when that one is still running. Returns whether it did.
/// Never `NSApp.hide`, which would hide Settings too.
///
/// # Safety
/// As [`harden_window`].
///
/// # Errors
/// - [`PlatformError::WrongThread`] off the main thread.
/// - [`PlatformError::Os`] for a null pointer.
pub unsafe fn hide_panel(
    ns_window: *mut c_void,
    back_to_pid: Option<i32>,
) -> Result<bool, PlatformError> {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, mtm) = unsafe { retain_window(ns_window) }?;
    Ok(order_out_and_hand_back(&window, mtm, || {
        back_to_pid.and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
    }))
}

/// Shows the float shelf at `frame` (Grape's top-left points) without
/// taking focus: it records the app in front (not Grape), sets the frame
/// and orders the window front with `orderFrontRegardless`. Never key,
/// never activating Grape: the Mac's `SWP_NOACTIVATE`. Used for every
/// show, resize and snap. Also refreshes the screen cache, as
/// [`frame_and_show`] does.
///
/// # Safety
/// As [`harden_window`].
///
/// # Errors
/// - [`PlatformError::WrongThread`] off the main thread.
/// - [`PlatformError::Os`] for a null pointer, or when there are no screens.
pub unsafe fn show_shelf(ns_window: *mut c_void, frame: Rect) -> Result<PanelShown, PlatformError> {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, mtm) = unsafe { retain_window(ns_window) }?;
    let rect = cocoa_frame(mtm, frame)?;
    let front_pid = autoreleasepool(|_| {
        let front_pid = frontmost_other_app(mtm).map(|app| app.processIdentifier());
        window.setFrame_display(rect, false);
        // Never makeKeyWindow, makeKeyAndOrderFront or activate.
        window.orderFrontRegardless();
        front_pid
    });
    Ok(PanelShown { front_pid })
}

/// Hides the float shelf (`orderOut`). If a click had made it the key
/// window and Grape is still the active app, it hands activation back, as
/// [`hide_panel`] does: an active Mac app with no window keeps the
/// keyboard, so the user's next keystrokes would go nowhere. It goes to
/// the app the user was last in, other than Grape, that still runs (seen
/// since [`super::follow_app_activations`]): the shelf stays open while
/// the user switches apps, so the app in front when it was shown is often
/// not the one they came from. Only when none is known does it go to the
/// app with `back_to_pid` (from [`show_shelf`]), if that one still runs.
/// Returns whether it handed back. Never `NSApp.hide`.
///
/// # Safety
/// As [`harden_window`].
///
/// # Errors
/// - [`PlatformError::WrongThread`] off the main thread.
/// - [`PlatformError::Os`] for a null pointer.
pub unsafe fn hide_shelf(
    ns_window: *mut c_void,
    back_to_pid: Option<i32>,
) -> Result<bool, PlatformError> {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, mtm) = unsafe { retain_window(ns_window) }?;
    Ok(order_out_and_hand_back(&window, mtm, || {
        activations::latest_running_app().or_else(|| back_to_pid.and_then(activations::running_app))
    }))
}

/// The window's frame now, in Grape's top-left points. Read it here, never
/// with Tauri's `outer_position` / `outer_size`, which give backing pixels
/// (points × the screen's scale) on the Mac. Also refreshes the screen
/// cache.
///
/// # Safety
/// As [`harden_window`].
///
/// # Errors
/// - [`PlatformError::WrongThread`] off the main thread.
/// - [`PlatformError::Os`] for a null pointer, or when there are no screens.
pub unsafe fn window_frame(ns_window: *mut c_void) -> Result<Rect, PlatformError> {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, mtm) = unsafe { retain_window(ns_window) }?;
    let primary_height = display::refresh(mtm).ok_or(PlatformError::Os {
        context: "NSScreen.screens",
        code: NO_CODE,
    })?;
    let frame = window.frame();
    Ok(to_grape(
        CocoaRect {
            x: frame.origin.x,
            y: frame.origin.y,
            width: frame.size.width,
            height: frame.size.height,
        },
        primary_height,
    ))
}

/// [`hide_panel`]'s and [`hide_shelf`]'s work: `orderOut`, then, if the
/// window was key and Grape is still active, activation back to the app
/// `target` picks (only asked then). Returns whether it handed back.
fn order_out_and_hand_back(
    window: &NSWindow,
    mtm: MainThreadMarker,
    target: impl FnOnce() -> Option<Retained<NSRunningApplication>>,
) -> bool {
    autoreleasepool(|_| {
        let was_key = window.isKeyWindow();
        window.orderOut(None);
        let active = NSApplication::sharedApplication(mtm).isActive();
        let target = if was_key && active { target() } else { None };
        match target {
            Some(app) => {
                hand_back(mtm, &app);
                true
            }
            None => false,
        }
    })
}

/// The window's level, collection behavior, whether it's the key window,
/// whether Grape is the active app and whether Hide (Cmd+H) hides the
/// window, for a log line. For example `level 101 (pop-up menu), behavior
/// 0x151 CanJoinAllSpaces|Stationary|IgnoresCycle|FullScreenAuxiliary, key
/// no, app active no, hides no`.
///
/// # Safety
/// As [`harden_window`].
pub unsafe fn describe_window(ns_window: *mut c_void) -> String {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    match unsafe { retain_window(ns_window) } {
        Ok((window, mtm)) => {
            let level = window.level();
            let behavior = window.collectionBehavior();
            let active = NSApplication::sharedApplication(mtm).isActive();
            format!(
                "level {level} ({}), behavior {:#x} {}, key {}, app active {}, hides {}",
                level_name(level),
                behavior.bits(),
                behavior_names(behavior),
                yes_no(window.isKeyWindow()),
                yes_no(active),
                yes_no(window.canHide()),
            )
        }
        Err(err) => format!("no window to describe: {err}"),
    }
}

/// `frame` (Grape's top-left points) as a Cocoa frame on today's screens,
/// refreshing the screen cache that [`crate::display_bounds_at`] reads.
fn cocoa_frame(mtm: MainThreadMarker, frame: Rect) -> Result<NSRect, PlatformError> {
    let primary_height = display::refresh(mtm).ok_or(PlatformError::Os {
        context: "NSScreen.screens",
        code: NO_CODE,
    })?;
    let rect = to_cocoa(frame, primary_height);
    Ok(NSRect::new(
        NSPoint::new(rect.x, rect.y),
        NSSize::new(rect.width, rect.height),
    ))
}

/// Checks the thread and the pointer, then retains the window so it
/// outlives the autorelease pool for this call.
///
/// # Safety
/// `ns_window` must be null or a live `NSWindow` pointer.
pub(super) unsafe fn retain_window(
    ns_window: *mut c_void,
) -> Result<(Retained<NSWindow>, MainThreadMarker), PlatformError> {
    let null = PlatformError::Os {
        context: "reading the NSWindow pointer",
        code: NO_CODE,
    };
    if ns_window.is_null() {
        return Err(null);
    }
    let mtm = MainThreadMarker::new().ok_or(PlatformError::WrongThread)?;
    // SAFETY: the pointer is non-null, and the caller promises it's a live
    // NSWindow. Retaining it on the main thread, where NSWindow lives, keeps
    // it alive for as long as the Retained does.
    let window = unsafe { Retained::retain(ns_window.cast::<NSWindow>()) }.ok_or(null)?;
    Ok((window, mtm))
}

/// `existing` plus what the overlay needs, minus the flags AppKit won't
/// combine with those: it raises an exception for `CanJoinAllSpaces` with
/// `MoveToActiveSpace`, for more than one of `Managed`, `Transient` and
/// `Stationary`, for both cycle flags, and for more than one full-screen
/// flag.
fn overlay_behavior(existing: NSWindowCollectionBehavior) -> NSWindowCollectionBehavior {
    let wanted = NSWindowCollectionBehavior::CanJoinAllSpaces
        | NSWindowCollectionBehavior::FullScreenAuxiliary
        | NSWindowCollectionBehavior::Stationary
        | NSWindowCollectionBehavior::IgnoresCycle;
    let clashing = NSWindowCollectionBehavior::MoveToActiveSpace
        | NSWindowCollectionBehavior::Managed
        | NSWindowCollectionBehavior::Transient
        | NSWindowCollectionBehavior::ParticipatesInCycle
        | NSWindowCollectionBehavior::FullScreenPrimary
        | NSWindowCollectionBehavior::FullScreenNone;
    (existing - clashing) | wanted
}

/// AppKit's name for a standard window level, the toast's level, or
/// "custom".
fn level_name(level: NSWindowLevel) -> &'static str {
    [
        (NSNormalWindowLevel, "normal"),
        (NSFloatingWindowLevel, "floating"),
        (NSModalPanelWindowLevel, "modal panel"),
        (NSMainMenuWindowLevel, "main menu"),
        (NSStatusWindowLevel, "status"),
        (NSPopUpMenuWindowLevel, "pop-up menu"),
        (WindowRole::Toast.level(), "toast, under pop-up menu"),
        (WindowRole::Panel.level(), "tool panel, under the toast"),
        (NSScreenSaverWindowLevel, "screen saver"),
    ]
    .into_iter()
    .find_map(|(known, name)| (known == level).then_some(name))
    .unwrap_or("custom")
}

/// The flag names in `behavior`, joined with `|`, then any unnamed bits in
/// hex; "none" for no flags.
fn behavior_names(behavior: NSWindowCollectionBehavior) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut rest = behavior;
    for (name, flag) in behavior.iter_names() {
        names.push(name.to_owned());
        rest -= flag;
    }
    if !rest.is_empty() {
        names.push(format!("{:#x}", rest.bits()));
    }
    if names.is_empty() {
        names.push("none".to_owned());
    }
    names.join("|")
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type B = NSWindowCollectionBehavior;

    #[test]
    fn the_overlay_behavior_adds_what_it_needs() {
        let behavior = overlay_behavior(B::Default);
        assert_eq!(
            behavior,
            B::CanJoinAllSpaces | B::FullScreenAuxiliary | B::Stationary | B::IgnoresCycle
        );
        assert_eq!(behavior.bits(), 0x151);
    }

    #[test]
    fn the_overlay_behavior_drops_flags_appkit_would_reject() {
        let existing = B::MoveToActiveSpace
            | B::Managed
            | B::ParticipatesInCycle
            | B::FullScreenPrimary
            | B::FullScreenDisallowsTiling;
        assert_eq!(
            overlay_behavior(existing),
            B::CanJoinAllSpaces
                | B::FullScreenAuxiliary
                | B::Stationary
                | B::IgnoresCycle
                | B::FullScreenDisallowsTiling
        );
        // Asking twice changes nothing.
        let once = overlay_behavior(existing);
        assert_eq!(overlay_behavior(once), once);
    }

    #[test]
    fn the_toast_sits_just_under_the_overlay() {
        assert_eq!(WindowRole::Overlay.level(), 101);
        assert_eq!(WindowRole::Toast.level(), 100);
        // Both above the status bar items, both below the drag image (500).
        for role in [WindowRole::Overlay, WindowRole::Toast] {
            assert!(role.level() > NSStatusWindowLevel, "{role:?}");
            assert!(role.level() < 500, "{role:?}");
        }
    }

    #[test]
    fn the_panel_sits_under_the_toast_and_the_overlay() {
        assert_eq!(WindowRole::Panel.level(), 99);
        assert!(WindowRole::Panel.level() < WindowRole::Toast.level());
        assert!(WindowRole::Toast.level() < WindowRole::Overlay.level());
        assert!(WindowRole::Panel.level() > NSStatusWindowLevel);
    }

    #[test]
    fn the_shelf_floats_under_every_other_grape_window() {
        assert_eq!(WindowRole::Shelf.level(), 3);
        assert_eq!(WindowRole::Shelf.level(), NSFloatingWindowLevel);
        assert!(WindowRole::Shelf.level() > NSNormalWindowLevel);
        assert!(WindowRole::Shelf.level() < WindowRole::Panel.level());
        assert!(WindowRole::Panel.level() < WindowRole::Toast.level());
        assert!(WindowRole::Toast.level() < WindowRole::Overlay.level());
        assert!(WindowRole::Shelf.level() < 500);
        assert_eq!(level_name(WindowRole::Shelf.level()), "floating");
    }

    #[test]
    fn levels_and_behaviors_read_well_in_a_log() {
        assert_eq!(level_name(101), "pop-up menu");
        assert_eq!(level_name(100), "toast, under pop-up menu");
        assert_eq!(level_name(99), "tool panel, under the toast");
        assert_eq!(level_name(3), "floating");
        assert_eq!(level_name(102), "custom");
        assert_eq!(
            behavior_names(overlay_behavior(B::Default)),
            "CanJoinAllSpaces|Stationary|IgnoresCycle|FullScreenAuxiliary"
        );
        assert_eq!(behavior_names(B::Default), "none");
        assert_eq!(behavior_names(B::from_bits_retain(1 << 30)), "0x40000000");
    }

    #[test]
    fn handing_activation_back_is_refused_off_the_main_thread() {
        // libtest runs every test on a thread of its own.
        assert_eq!(hand_back_activation(), Err(PlatformError::WrongThread));
    }

    #[test]
    fn a_null_window_is_refused() {
        for role in [
            WindowRole::Overlay,
            WindowRole::Toast,
            WindowRole::Panel,
            WindowRole::Shelf,
        ] {
            // SAFETY: null is allowed by the contract and never dereferenced.
            let hardened = unsafe { harden_window(std::ptr::null_mut(), role) };
            assert!(matches!(hardened, Err(PlatformError::Os { .. })));
        }
        // SAFETY: as above.
        let shown = unsafe { frame_and_show(std::ptr::null_mut(), Rect::default()) };
        assert!(matches!(shown, Err(PlatformError::Os { .. })));
        for clickable in [false, true] {
            // SAFETY: as above.
            let toast = unsafe { show_toast(std::ptr::null_mut(), Rect::default(), clickable) };
            assert!(matches!(toast, Err(PlatformError::Os { .. })));
        }
        // SAFETY: as above.
        let panel = unsafe { show_panel(std::ptr::null_mut(), Rect::default()) };
        assert!(matches!(panel, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let hidden = unsafe { hide_panel(std::ptr::null_mut(), Some(1)) };
        assert!(matches!(hidden, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let shelf = unsafe { show_shelf(std::ptr::null_mut(), Rect::default()) };
        assert!(matches!(shelf, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let shelf_hidden = unsafe { hide_shelf(std::ptr::null_mut(), Some(1)) };
        assert!(matches!(shelf_hidden, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let frame = unsafe { window_frame(std::ptr::null_mut()) };
        assert!(matches!(frame, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let described = unsafe { describe_window(std::ptr::null_mut()) };
        assert!(
            described.starts_with("no window to describe"),
            "{described}"
        );
    }
}
