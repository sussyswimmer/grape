//! Fallback for hosts Grape doesn't target (Linux dev machines and CI).
//! Everything is unsupported, so the workspace builds and tests anywhere.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::file_actions::{MoveOutcome, ShareOutcome, TerminalApp};
use crate::keys::Chord;
use crate::shortcuts::{
    FrontApp, HotkeyMonitor, InputAccess, LocalTime, OpenTarget, PickKind, Picked, Placement,
    PointerWatch, Program, ProgramRequest, SystemControl, SystemPlan,
};
use crate::{DisplayBounds, DragMonitor, Modifiers, PlatformError};

pub(crate) fn drag_monitor() -> Result<Box<dyn DragMonitor>, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn display_bounds_at(_x: i32, _y: i32) -> Result<DisplayBounds, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn copy_files(_paths: &[PathBuf]) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn copy_paths(_paths: &[PathBuf]) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn open_terminal(_dir: &Path, _app: TerminalApp) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn move_to_chosen_folder(_paths: &[PathBuf]) -> Result<MoveOutcome, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn pick_folder(
    _owner: Option<isize>,
    _title: &str,
    _start: Option<&Path>,
) -> Result<Option<PathBuf>, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn share(
    _paths: &[PathBuf],
    _near: Option<(i32, i32)>,
) -> Result<ShareOutcome, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn reveal(_path: &Path) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

// The Shortcuts wheel (`crate::shortcuts`).

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

pub(crate) fn open_programs(_target: OpenTarget<'_>) -> Result<Vec<Program>, PlatformError> {
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

pub(crate) fn give_back_front(_pid: Option<i32>) {}

pub(crate) fn front_app() -> Result<Option<FrontApp>, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn ensure_front(_pid: i32, _timeout: Duration) -> bool {
    false
}

pub(crate) fn pick_target(
    _kind: PickKind,
    _owner: Option<isize>,
) -> Result<Option<Picked>, PlatformError> {
    Err(PlatformError::Unsupported)
}
