//! Windows platform layer (Phase 1). See `docs/PLATFORM_WINDOWS.md`.
//!
//! Today this module only styles the overlay window ([`harden_overlay`]).
//! Milestone M1 adds the drag monitor (`hook.rs`: `WH_MOUSE_LL` on a
//! message-pumping thread; `drag.rs`: the candidate-drag heuristic,
//! `GetAsyncKeyState` polling and the Explorer allowlist) and
//! `display_bounds_at` (`MonitorFromPoint` + `GetMonitorInfoW`). Until then
//! both return [`PlatformError::Unsupported`].
//!
//! Inside this crate, write the Win32 bindings as `::windows::…`: a bare
//! `windows::…` can be confused with this module.

mod overlay;

pub use overlay::{describe_ex_style, ex_style, harden_overlay};

use crate::{DisplayBounds, DragMonitor, PlatformError};

pub(crate) fn drag_monitor() -> Result<Box<dyn DragMonitor>, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub(crate) fn display_bounds_at(_x: i32, _y: i32) -> Result<DisplayBounds, PlatformError> {
    Err(PlatformError::Unsupported)
}
