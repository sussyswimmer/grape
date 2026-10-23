//! macOS platform layer (DECISIONS D18). See `docs/PLATFORM_MACOS.md`.
//!
//! - `drag`: the drag monitor behind [`crate::drag_monitor`]. A thread polls
//!   `NSEvent.pressedMouseButtons`, `NSEvent.modifierFlags` and the drag
//!   pasteboard's `changeCount`. No event taps and no key monitors, so no
//!   permission prompt (D7). Esc is never reported.
//! - `display`: [`crate::display_bounds_at`] from a cache of the screens that
//!   the main thread fills ([`refresh_screens`]), with CoreGraphics as the
//!   fallback when the cache is empty.
//! - `window`: overlay and toast windows from Tauri's raw `NSWindow`
//!   pointer: [`harden_window`], [`frame_and_show`], [`describe_window`].
//! - `actions`: the file actions behind [`crate::file_actions`]: the general
//!   pasteboard, Terminal, Finder, AirDrop and the Move panel; and the
//!   haptic tick behind [`crate::haptic_tick`].
//!
//! Coordinates are points with the origin at the top-left of the primary
//! (menu-bar) screen, y down; `DisplayBounds::scale` is always 1.0. The
//! pure flips and picks live in `crate::mac_rules`, where every OS tests
//! them.
//!
//! Threads: `NSScreen`, `NSWindow`, `NSOpenPanel` and `NSAlert` are
//! main-thread only, and the bindings enforce it with `MainThreadMarker`.
//! `NSEvent`'s class methods, `NSPasteboard` and `NSFileManager` are not
//! marked main-thread only, so the drag thread and the callers' worker
//! threads use them directly, each inside an autorelease pool.

mod actions;
mod display;
mod drag;
mod window;

pub use display::refresh_screens;
pub use window::{describe_window, frame_and_show, harden_window};

pub(crate) use actions::{
    copy_files, copy_paths, haptic_tick, move_to_chosen_folder, open_terminal, reveal, share,
};

use crate::{DisplayBounds, DragMonitor, PlatformError};

/// The code in a [`PlatformError::Os`] when the call only reported failure
/// (a `NO`, a nil) and gave no code of its own.
const NO_CODE: i32 = -1;

/// Cocoa's `NSFeatureUnsupportedError`: the code for a feature this Mac
/// can't use right now (AirDrop turned off, say).
const FEATURE_UNSUPPORTED: i32 = 3328;

pub(crate) fn drag_monitor() -> Result<Box<dyn DragMonitor>, PlatformError> {
    Ok(Box::new(drag::MacDragMonitor::default()))
}

pub(crate) fn display_bounds_at(x: i32, y: i32) -> Result<DisplayBounds, PlatformError> {
    display::display_bounds_at(x, y)
}

/// A [`PlatformError::Os`] for a failed `std::io` call, with its errno.
fn io_error(context: &'static str, err: &std::io::Error) -> PlatformError {
    PlatformError::Os {
        context,
        code: err.raw_os_error().unwrap_or(NO_CODE),
    }
}
