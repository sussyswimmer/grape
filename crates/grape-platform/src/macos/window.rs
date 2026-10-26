//! Grape's overlay, toast and tool panel windows on macOS
//! (`docs/PLATFORM_MACOS.md` → "Overlay window").
//!
//! Each function takes the raw pointer that Tauri's
//! `WebviewWindow::ns_window()` returns. That pointer is autoreleased: it's
//! only good on the main thread, until the current autorelease pool drains.
//! So every function here runs on the main thread only (inside Tauri's
//! `setup` or `run_on_main_thread`), checks that it is, and retains the
//! window for the length of the call.
//!
//! The overlay and the toast never become key, so the frontmost app keeps
//! focus. That comes from `"focusable": false` in `tauri.conf.json`, not
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
//! tao resets the level that [`harden_window`] sets in `set_always_on_top`
//! and `set_always_on_bottom` (queued on the main queue: floating, below
//! normal or normal level) and in `set_fullscreen` (level 0 before it
//! toggles full screen; the shielding level and back to normal for
//! exclusive full screen), so don't call them on a hardened window.
//! `set_visible_on_all_workspaces(false)` clears `CanJoinAllSpaces` from the
//! collection behavior. tao's `set_ignore_cursor_events` only sets
//! `ignoresMouseEvents` and leaves both alone.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;

use objc2::rc::{autoreleasepool, Retained};
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSFloatingWindowLevel, NSMainMenuWindowLevel, NSModalPanelWindowLevel,
    NSNormalWindowLevel, NSPopUpMenuWindowLevel, NSRunningApplication, NSScreenSaverWindowLevel,
    NSStatusWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowLevel,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::actions::{activate, frontmost_other_app, hand_back};
use super::{display, NO_CODE};
use crate::mac_rules::to_cocoa;
use crate::{PlatformError, Rect};

/// Which of Grape's always-in-front windows is being hardened. They differ
/// only in level, so the overlay always covers the toast, and both cover
/// the tool panel.
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
}

impl WindowRole {
    /// The window level for this role.
    fn level(self) -> NSWindowLevel {
        match self {
            Self::Overlay => NSPopUpMenuWindowLevel,
            Self::Toast => NSPopUpMenuWindowLevel - 1,
            Self::Panel => NSPopUpMenuWindowLevel - 2,
        }
    }
}

/// What [`show_panel`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelShown {
    /// The app that was in front (never Grape), to hand activation back to
    /// when the panel closes.
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
    }))
    .map_err(|_| PlatformError::Os {
        context: "setting the window's level and collection behavior",
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
    autoreleasepool(|_| {
        window.setFrame_display(rect, false);
        // Never makeKeyAndOrderFront or activate: Grape must not take focus.
        // A plain orderFront from an inactive app stays behind the active
        // app's windows.
        window.orderFrontRegardless();
    });
    Ok(())
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
/// Never `NSApp.hide`, which would hide the overlay and the toast too.
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
    let handed = autoreleasepool(|_| {
        let was_key = window.isKeyWindow();
        window.orderOut(None);
        let active = NSApplication::sharedApplication(mtm).isActive();
        let target = back_to_pid
            .filter(|_| was_key && active)
            .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier);
        match target {
            Some(app) => {
                hand_back(mtm, &app);
                true
            }
            None => false,
        }
    });
    Ok(handed)
}

/// The window's level, collection behavior, whether it's the key window
/// and whether Grape is the active app, for a log line. For example
/// `level 101 (pop-up menu), behavior 0x151 CanJoinAllSpaces|Stationary|
/// IgnoresCycle|FullScreenAuxiliary, key no, app active no`.
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
                "level {level} ({}), behavior {:#x} {}, key {}, app active {}",
                level_name(level),
                behavior.bits(),
                behavior_names(behavior),
                yes_no(window.isKeyWindow()),
                yes_no(active),
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
unsafe fn retain_window(
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
    fn a_null_window_is_refused() {
        for role in [WindowRole::Overlay, WindowRole::Toast, WindowRole::Panel] {
            // SAFETY: null is allowed by the contract and never dereferenced.
            let hardened = unsafe { harden_window(std::ptr::null_mut(), role) };
            assert!(matches!(hardened, Err(PlatformError::Os { .. })));
        }
        // SAFETY: as above.
        let shown = unsafe { frame_and_show(std::ptr::null_mut(), Rect::default()) };
        assert!(matches!(shown, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let panel = unsafe { show_panel(std::ptr::null_mut(), Rect::default()) };
        assert!(matches!(panel, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let hidden = unsafe { hide_panel(std::ptr::null_mut(), Some(1)) };
        assert!(matches!(hidden, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let described = unsafe { describe_window(std::ptr::null_mut()) };
        assert!(
            described.starts_with("no window to describe"),
            "{described}"
        );
    }
}
