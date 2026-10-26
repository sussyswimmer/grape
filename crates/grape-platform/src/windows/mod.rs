//! Windows platform layer (Phase 1). See `docs/PLATFORM_WINDOWS.md`.
//!
//! - `hook`: the `WH_MOUSE_LL` hook on its own message-pumping thread.
//! - `drag`: the drag worker (candidate-drag heuristic, `GetAsyncKeyState`
//!   polling, Explorer/Desktop allowlist) behind [`crate::drag_monitor`].
//! - `display`: [`crate::display_bounds_at`] (`MonitorFromPoint` +
//!   `GetMonitorInfoW` + `GetDpiForMonitor`).
//! - `drop_target`: Grape's own `IDropTarget` for the overlay, which never
//!   reports a move ([`install_drop_target`]), and a refusing one for
//!   windows that take no drops, the shelf included ([`refuse_drops`]).
//! - `overlay`: keeps the overlay out of the taskbar and Alt-Tab
//!   ([`harden_overlay`]), and the float shelf and the tool panel too,
//!   though a click activates them ([`harden_shelf`], [`harden_panel`]);
//!   shows them without activating them and hides them ([`show_shelf`],
//!   [`hide_shelf`], [`show_panel`], [`hide_panel`], which hands the
//!   foreground back to [`other_app_foreground`]'s window); lifts the
//!   overlay over both on every show ([`raise_topmost`]).
//! - `drag_out`: dragging files out of the shelf ([`drag_out`]): an OLE
//!   drag of a `CF_HDROP`-only data object that offers copy or link, never
//!   move.
//! - `actions`: the file actions behind [`crate::file_actions`]: the
//!   clipboard, Windows Terminal or PowerShell, the shell's move, Explorer
//!   for Reveal, and the shell's folder picker (Move's, and Settings' on
//!   its own).
//! - `share`: Share behind [`crate::file_actions::share`]: the share sheet,
//!   owned by an invisible anchor window on a thread of its own.
//!
//! Inside this crate, write the Win32 bindings as `::windows::…`: a bare
//! `windows::…` can be confused with this module.

mod actions;
mod display;
mod drag;
mod drag_out;
mod drop_target;
mod hook;
mod overlay;
mod share;

pub use drag_out::{drag_out, DragOut, DragOutcome};
pub use drop_target::{install_drop_target, refuse_drops, DropTargetCoverage};
pub use overlay::{
    describe_ex_style, ex_style, harden_overlay, harden_panel, harden_shelf, hide_panel,
    hide_shelf, other_app_foreground, raise_topmost, show_panel, show_shelf, HandBack,
};

pub(crate) use actions::{
    copy_files, copy_paths, move_to_chosen_folder, open_terminal, pick_folder, reveal,
};
pub(crate) use share::share;

use crate::{DisplayBounds, DragMonitor, PlatformError};

pub(crate) fn drag_monitor() -> Result<Box<dyn DragMonitor>, PlatformError> {
    Ok(Box::new(drag::WindowsDragMonitor::default()))
}

pub(crate) fn display_bounds_at(x: i32, y: i32) -> Result<DisplayBounds, PlatformError> {
    display::display_bounds_at(x, y)
}

/// `true` while either physical mouse button is down (`GetAsyncKeyState`;
/// with swapped buttons the primary one is the right). Any thread may ask.
/// The state can lag the real button by a moment, so read it again before
/// acting on an "up".
pub fn primary_button_down() -> bool {
    drag::primary_button_down()
}

/// `true` while Esc is down (`GetAsyncKeyState(VK_ESCAPE)`). Any thread
/// may ask. Reading it never consumes the key: the app in front gets it
/// too.
pub fn escape_down() -> bool {
    drag::key_down(::windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE)
}

#[cfg(test)]
mod tests {
    #[test]
    fn escape_is_up_when_nobody_presses_it() {
        // CI presses no keys.
        assert!(!super::escape_down());
    }
}
