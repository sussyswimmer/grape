//! macOS platform layer (DECISIONS D18). See `docs/PLATFORM_MACOS.md`.
//!
//! - `drag`: the drag monitor behind [`crate::drag_monitor`]. A thread polls
//!   `NSEvent.pressedMouseButtons`, `NSEvent.modifierFlags` and the drag
//!   pasteboard's `changeCount`, and starts the drag as soon as a file drag
//!   session appears. It holds an `NSProcessInfo` activity so App Nap
//!   doesn't stretch its polls. No event taps and no key monitors, so no
//!   permission prompt (D7). Esc is never reported.
//! - `display`: [`crate::display_bounds_at`] from a cache of the screens that
//!   the main thread fills ([`refresh_screens`]), with CoreGraphics as the
//!   fallback when the cache is empty.
//! - `window`: overlay, toast and tool panel windows from Tauri's raw
//!   `NSWindow` pointer: [`harden_window`] (level 101 for
//!   [`WindowRole::Overlay`], 100 for [`WindowRole::Toast`], 99 for
//!   [`WindowRole::Panel`], so an open wheel covers the toast and both
//!   cover the panel), [`frame_and_show`], [`describe_window`], and the
//!   panel's [`show_panel`] (the one hardened window that becomes key, and
//!   Grape asks to be activated) and [`hide_panel`] (activation handed
//!   back).
//! - `actions`: the file actions behind [`crate::file_actions`]: the general
//!   pasteboard, Terminal or iTerm2, Finder, AirDrop and the Move panel (a
//!   volume's root never moves, and each item is judged right after its own
//!   move); the same panel for Settings' folders; and the haptic tick behind
//!   [`crate::haptic_tick`].
//! - `heif`: the image helper worker's codec, [`heif::ImageIo`] (DECISIONS
//!   D33, D41): ImageIO behind `grape_heif_proto::Codec` for HEIC and AVIF
//!   (AVIF written by [`crate::avif`]), run only in the worker process
//!   (`Grape --grape-heif-worker`), never in Grape's own.
//!
//! Coordinates are points with the origin at the top-left of the primary
//! (menu-bar) screen, y down; `DisplayBounds::scale` is always 1.0. The
//! pure flips and picks live in `crate::mac_rules`, where every OS tests
//! them.
//!
//! Threads: `NSScreen`, `NSWindow`, `NSOpenPanel` and `NSAlert` are
//! main-thread only, and the bindings enforce it with `MainThreadMarker`.
//! `NSEvent`'s class methods, `NSPasteboard`, `NSFileManager` and
//! `NSWorkspace` are not marked main-thread only, so the drag thread and the
//! callers' worker threads use them directly, each inside an autorelease
//! pool.

mod actions;
mod display;
mod drag;
pub mod heif;
mod window;

pub use display::refresh_screens;
pub use window::{
    describe_window, frame_and_show, harden_window, hide_panel, show_panel, PanelShown, WindowRole,
};

pub(crate) use actions::{
    copy_files, copy_paths, haptic_tick, move_to_chosen_folder, open_terminal, pick_folder, reveal,
    share,
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

/// `true` while any mouse button is down (`NSEvent.pressedMouseButtons`,
/// the hardware state, not the event stream). Any thread may ask. The tool
/// panel refuses its page's Esc then: it belongs to a drag (D31).
pub fn mouse_button_down() -> bool {
    objc2_app_kit::NSEvent::pressedMouseButtons() != 0
}

/// A [`PlatformError::Os`] for a failed `std::io` call, with its errno.
fn io_error(context: &'static str, err: &std::io::Error) -> PlatformError {
    PlatformError::Os {
        context,
        code: err.raw_os_error().unwrap_or(NO_CODE),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn no_mouse_button_is_down_when_nobody_holds_one() {
        // Nobody holds the mouse on CI's Mac.
        assert!(!super::mouse_button_down());
    }
}
