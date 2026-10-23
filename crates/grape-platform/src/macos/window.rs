//! Grape's overlay and toast windows on macOS (`docs/PLATFORM_MACOS.md` →
//! "Overlay window").
//!
//! Each function takes the raw pointer that Tauri's
//! `WebviewWindow::ns_window()` returns. That pointer is autoreleased: it's
//! only good on the main thread, until the current autorelease pool drains.
//! So every function here runs on the main thread only (inside Tauri's
//! `setup` or `run_on_main_thread`), checks that it is, and retains the
//! window for the length of the call.
//!
//! Tauri's own `set_always_on_top` and `set_ignore_cursor_events` queue
//! their AppKit calls for later and would undo [`harden_window`]'s level,
//! so don't call them on a hardened window.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;

use objc2::rc::{autoreleasepool, Retained};
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSFloatingWindowLevel, NSMainMenuWindowLevel, NSModalPanelWindowLevel,
    NSNormalWindowLevel, NSPopUpMenuWindowLevel, NSScreenSaverWindowLevel, NSStatusWindowLevel,
    NSWindow, NSWindowCollectionBehavior, NSWindowLevel,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::{display, NO_CODE};
use crate::mac_rules::to_cocoa;
use crate::{PlatformError, Rect};

/// Lifts the window over the menu bar, the Dock and full-screen apps, on
/// every Space, and keeps it out of Cmd-Tab and window cycling: level
/// `NSPopUpMenuWindowLevel` (101) and collection behavior
/// `CanJoinAllSpaces | FullScreenAuxiliary | Stationary | IgnoresCycle`. It
/// stays visible when Grape isn't the active app. The level stays below
/// 500: any higher and it would cover the image the user is dragging.
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
pub unsafe fn harden_window(ns_window: *mut c_void) -> Result<(), PlatformError> {
    // SAFETY: the caller's contract is the one `retain_window` needs.
    let (window, _mtm) = unsafe { retain_window(ns_window) }?;
    let behavior = overlay_behavior(window.collectionBehavior());
    objc2::exception::catch(AssertUnwindSafe(|| {
        window.setLevel(NSPopUpMenuWindowLevel);
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
/// keeps focus. Also refreshes the screen cache that
/// [`crate::display_bounds_at`] reads.
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
    let primary_height = display::refresh(mtm).ok_or(PlatformError::Os {
        context: "NSScreen.screens",
        code: NO_CODE,
    })?;
    let rect = to_cocoa(frame, primary_height);
    autoreleasepool(|_| {
        window.setFrame_display(
            NSRect::new(
                NSPoint::new(rect.x, rect.y),
                NSSize::new(rect.width, rect.height),
            ),
            false,
        );
        // Never makeKeyAndOrderFront or activate: Grape must not take focus.
        window.orderFrontRegardless();
    });
    Ok(())
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

/// AppKit's name for a standard window level, or "custom".
fn level_name(level: NSWindowLevel) -> &'static str {
    [
        (NSNormalWindowLevel, "normal"),
        (NSFloatingWindowLevel, "floating"),
        (NSModalPanelWindowLevel, "modal panel"),
        (NSMainMenuWindowLevel, "main menu"),
        (NSStatusWindowLevel, "status"),
        (NSPopUpMenuWindowLevel, "pop-up menu"),
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
    fn levels_and_behaviors_read_well_in_a_log() {
        assert_eq!(level_name(101), "pop-up menu");
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
        // SAFETY: null is allowed by the contract and never dereferenced.
        let hardened = unsafe { harden_window(std::ptr::null_mut()) };
        assert!(matches!(hardened, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let shown = unsafe { frame_and_show(std::ptr::null_mut(), Rect::default()) };
        assert!(matches!(shown, Err(PlatformError::Os { .. })));
        // SAFETY: as above.
        let described = unsafe { describe_window(std::ptr::null_mut()) };
        assert!(
            described.starts_with("no window to describe"),
            "{described}"
        );
    }
}
