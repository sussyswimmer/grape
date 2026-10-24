//! Fallback for hosts Grape doesn't target (Linux dev machines and CI).
//! Everything is unsupported, so the workspace builds and tests anywhere.

use std::path::{Path, PathBuf};

use crate::file_actions::MoveOutcome;
use crate::{DisplayBounds, DragMonitor, PlatformError};

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

pub(crate) fn open_terminal(_dir: &Path) -> Result<(), PlatformError> {
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

pub(crate) fn share(_paths: &[PathBuf]) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn reveal(_path: &Path) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}
