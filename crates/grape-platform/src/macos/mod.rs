//! macOS platform layer: a stub until milestone M8 (DECISIONS D8).
//!
//! Every function returns [`PlatformError::Unsupported`] and no monitor ever
//! emits. It exists so the workspace compiles on a Mac. The real code
//! (`drag.rs` pasteboard polling, `actions.rs`, `haptics.rs`) follows
//! `docs/PLATFORM_MACOS.md` in M8. Don't add AppKit code before then.

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
