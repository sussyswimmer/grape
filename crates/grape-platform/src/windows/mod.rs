//! Windows platform layer (Phase 1). See `docs/PLATFORM_WINDOWS.md`.
//!
//! - `hook`: the `WH_MOUSE_LL` hook on its own message-pumping thread.
//! - `drag`: the drag worker (candidate-drag heuristic, `GetAsyncKeyState`
//!   polling, Explorer/Desktop allowlist) behind [`crate::drag_monitor`].
//! - `display`: [`crate::display_bounds_at`] (`MonitorFromPoint` +
//!   `GetMonitorInfoW` + `GetDpiForMonitor`).
//! - `drop_target`: Grape's own `IDropTarget` for the overlay, which never
//!   reports a move ([`install_drop_target`]), and a refusing one for
//!   windows that take no drops ([`refuse_drops`]).
//! - `overlay`: keeps the overlay out of the taskbar and Alt-Tab
//!   ([`harden_overlay`]).
//! - `actions`: the file actions behind [`crate::file_actions`]: the
//!   clipboard, Windows Terminal or PowerShell, and the shell's move.
//!
//! Inside this crate, write the Win32 bindings as `::windows::…`: a bare
//! `windows::…` can be confused with this module.

mod actions;
mod display;
mod drag;
mod drop_target;
mod hook;
mod overlay;

pub use drop_target::{install_drop_target, refuse_drops, DropTargetCoverage};
pub use overlay::{describe_ex_style, ex_style, harden_overlay};

pub(crate) use actions::{copy_files, copy_paths, move_to_chosen_folder, open_terminal};

use crate::{DisplayBounds, DragMonitor, PlatformError};

pub(crate) fn drag_monitor() -> Result<Box<dyn DragMonitor>, PlatformError> {
    Ok(Box::new(drag::WindowsDragMonitor::default()))
}

pub(crate) fn display_bounds_at(x: i32, y: i32) -> Result<DisplayBounds, PlatformError> {
    display::display_bounds_at(x, y)
}
