//! Fallback for hosts Grape doesn't target (Linux dev machines and CI).
//! Everything is unsupported, so the workspace builds and tests anywhere.

use crate::{DisplayBounds, DragMonitor, PlatformError};

pub(crate) fn drag_monitor() -> Result<Box<dyn DragMonitor>, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn display_bounds_at(_x: i32, _y: i32) -> Result<DisplayBounds, PlatformError> {
    Err(PlatformError::Unsupported)
}
