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
//!   overlay over both on every show ([`raise_topmost`]) and puts it back
//!   on its monitor's frame if a DPI change moved it ([`reframe_overlay`]).
//! - `drag_out`: dragging files out of the shelf ([`drag_out`]): an OLE
//!   drag of a `CF_HDROP`-only data object that offers copy or link, never
//!   move.
//! - `actions`: the file actions behind [`crate::file_actions`]: the
//!   clipboard, Windows Terminal or PowerShell, the shell's move, Explorer
//!   for Reveal, and the shell's folder picker (Move's, and Settings' on
//!   its own).
//! - `share`: Share behind [`crate::file_actions::share`]: the share sheet,
//!   owned by an invisible anchor window on a thread of its own.
//! - `job`: the Job Object a helper process runs in ([`job::JobGuard`],
//!   D33): it ends the helper with Grape, caps its memory and keeps it to
//!   one process.
//! - `run_key`: whether the `Run` keys still start Grape at sign-in
//!   ([`run_value`]), read directly, for launch at login after a
//!   reinstall (D19).
//!
//! Inside this crate, write the Win32 bindings as `::windows::…`: a bare
//! `windows::…` can be confused with this module.

mod actions;
mod display;
mod drag;
mod drag_out;
mod drop_target;
mod hook;
pub mod job;
mod overlay;
mod run_key;
mod share;

pub use drag_out::{drag_out, DragOut};
pub use drop_target::{install_drop_target, refuse_drops, DropTargetCoverage};
pub use overlay::{
    describe_ex_style, ex_style, harden_overlay, harden_panel, harden_shelf, hide_panel,
    hide_shelf, other_app_foreground, raise_topmost, reframe_overlay, show_panel, show_shelf,
    HandBack, Reframed,
};

pub use crate::win_rules::RunValue;
pub use run_key::run_value;

pub(crate) use actions::{
    copy_files, copy_paths, move_to_chosen_folder, open_terminal, pick_folder, reveal,
};
pub(crate) use share::share;

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::keys::Chord;
use crate::shortcuts::{
    HotkeyMonitor, InputAccess, LocalTime, OpenTarget, PickKind, Picked, Placement, PointerWatch,
    Program, ProgramRequest, SystemControl, SystemPlan,
};
use crate::{DisplayBounds, DragMonitor, Modifiers, PlatformError};

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

// The Shortcuts wheel (`crate::shortcuts`) is on the Mac first: nothing here
// yet.

pub(crate) fn hotkey_monitor() -> Result<Box<dyn HotkeyMonitor>, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn pointer_watch() -> Result<Box<dyn PointerWatch>, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn input_access() -> InputAccess {
    InputAccess::NotNeeded
}

pub(crate) fn request_input_access(_open_settings: bool) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn held_modifiers() -> Modifiers {
    Modifiers::NONE
}

pub(crate) fn local_date_time() -> Option<LocalTime> {
    None
}

pub(crate) fn send_chord(_chord: Chord) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn send_char_chord(_ch: char, _mods: Modifiers) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn type_text(_text: &str, _cancel: &AtomicBool) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn clipboard_text() -> Result<Option<String>, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn clipboard_change_count() -> Result<i64, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn clear_clipboard() -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn open_target(_target: OpenTarget<'_>) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn program_for(_request: ProgramRequest<'_>) -> Result<Program, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn kill_group(_pid: u32) {}

pub(crate) fn system_plan(_control: SystemControl) -> Result<SystemPlan, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn place_window(_pid: i32, _placement: Placement) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn give_back_front() {}

pub(crate) fn ensure_front(_pid: i32, _timeout: Duration) -> bool {
    false
}

pub(crate) fn pick_target(
    _kind: PickKind,
    _owner: Option<isize>,
) -> Result<Option<Picked>, PlatformError> {
    Err(PlatformError::Unsupported)
}

#[cfg(test)]
mod tests {
    #[test]
    fn escape_is_up_when_nobody_presses_it() {
        // CI presses no keys.
        assert!(!super::escape_down());
    }
}
