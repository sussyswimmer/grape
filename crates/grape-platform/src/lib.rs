//! OS glue for Grape: drag detection, modifier and cursor state, display
//! bounds, file actions. See `CLAUDE.md` in this crate.
//!
//! Phase 1 is Windows first (DECISIONS D8). The entry points below dispatch
//! by `cfg` to the `windows` module, the `macos` stub, or an `unsupported`
//! fallback for other hosts (Linux dev machines and CI). Since milestone M1,
//! Windows has a real drag monitor, display bounds and an overlay drop
//! target; everywhere else the entry points return
//! [`PlatformError::Unsupported`].
//!
//! Coordinates leaving this crate are global, top-left origin, physical
//! pixels.

#![warn(missing_docs)]

use std::path::PathBuf;
use std::sync::mpsc::SyncSender;

pub mod file_actions;
pub mod mock;
pub mod tracker;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod unsupported;
#[cfg(target_os = "windows")]
pub mod windows;

// `crate::` keeps the local `windows` module apart from the `windows` crate.
#[cfg(target_os = "macos")]
use crate::macos as imp;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use crate::unsupported as imp;
#[cfg(target_os = "windows")]
use crate::windows as imp;

/// One observation from the OS about a drag in progress.
///
/// The app's `WheelController` turns a stream of these into the state
/// machine in `docs/INTERACTION_SPEC.md` §1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragSignal {
    /// A candidate drag started: the primary button is down and the pointer
    /// moved past the OS drag threshold.
    Started {
        /// The modifiers already held when the drag started. They weren't
        /// pressed *during* the drag, so they don't open a wheel on their
        /// own (`docs/INTERACTION_SPEC.md` §2): holding Shift before
        /// dragging keeps meaning "move" in Explorer.
        modifiers: Modifiers,
    },
    /// The pointer moved. Global, top-left origin, physical pixels.
    Moved {
        /// Horizontal position.
        x: i32,
        /// Vertical position (grows downwards).
        y: i32,
    },
    /// The set of held modifier keys changed.
    Modifiers(Modifiers),
    /// Esc went down during the drag.
    Escape,
    /// The drag ended (the primary button was released).
    Ended,
}

/// What the overlay's drop target saw while a drag was over it.
///
/// On Windows these come from Grape's own `IDropTarget`
/// ([`windows::install_drop_target`]), which always reports a copy, never a
/// move (golden rule 2). Positions are global, top-left origin, physical
/// pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropEvent {
    /// A drag entered the overlay. `paths` is empty when the drag carries
    /// no files (selected text, an image from a browser, …).
    Enter {
        /// The dragged files and folders.
        paths: Vec<PathBuf>,
        /// Horizontal position.
        x: i32,
        /// Vertical position (grows downwards).
        y: i32,
    },
    /// A file drag moved over the overlay.
    Over {
        /// Horizontal position.
        x: i32,
        /// Vertical position (grows downwards).
        y: i32,
    },
    /// The drag left the overlay without dropping.
    Leave,
    /// The files were dropped on the overlay.
    Drop {
        /// The dropped files and folders. Empty for a non-file drop.
        paths: Vec<PathBuf>,
        /// Horizontal position.
        x: i32,
        /// Vertical position (grows downwards).
        y: i32,
    },
}

/// Which modifier keys are held.
///
/// `meta` is Command on macOS and the Windows key on Windows. Mapping keys to
/// wheels is the app's job; this is only the raw state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Control (Ctrl).
    pub ctrl: bool,
    /// Alt (Option on macOS).
    pub alt: bool,
    /// Command on macOS, the Windows key on Windows.
    pub meta: bool,
}

impl Modifiers {
    /// No modifiers held.
    pub const NONE: Self = Self {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };
    /// Only Shift held.
    pub const SHIFT: Self = Self {
        shift: true,
        ..Self::NONE
    };
    /// Only Control held.
    pub const CTRL: Self = Self {
        ctrl: true,
        ..Self::NONE
    };
    /// Only Alt held.
    pub const ALT: Self = Self {
        alt: true,
        ..Self::NONE
    };
    /// Only Meta (Command / Windows key) held.
    pub const META: Self = Self {
        meta: true,
        ..Self::NONE
    };

    /// How many modifiers are held. The wheel only picks a mode when this is
    /// exactly 1 (`docs/INTERACTION_SPEC.md` §2).
    pub fn count(self) -> usize {
        [self.shift, self.ctrl, self.alt, self.meta]
            .into_iter()
            .filter(|held| *held)
            .count()
    }

    /// `true` when no modifier is held.
    pub fn is_empty(self) -> bool {
        self.count() == 0
    }
}

/// A rectangle in global, top-left origin, physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
}

/// The display under a point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayBounds {
    /// The whole monitor. The overlay is sized to this while the wheel is
    /// open, so every drop lands on Grape.
    pub monitor: Rect,
    /// The monitor minus the taskbar or menu bar. The wheel anchor is
    /// clamped to this.
    pub work_area: Rect,
    /// Physical pixels per logical pixel (1.0 at 100 % scaling, 1.5 at 150 %).
    pub scale: f64,
}

/// Watches the OS for file drags and modifier changes.
///
/// Implementations must never block the user's input: hook callbacks and
/// polls only read state, `try_send` it and return. A full channel drops the
/// signal instead of waiting.
pub trait DragMonitor: Send {
    /// Starts watching and sends signals to `tx`.
    ///
    /// # Errors
    /// [`PlatformError::AlreadyRunning`] if the monitor is already started,
    /// or an OS error if the hook or poll could not be installed.
    fn start(&mut self, tx: SyncSender<DragSignal>) -> Result<(), PlatformError>;

    /// Stops watching. Safe to call more than once, and before `start`.
    fn stop(&mut self);
}

/// Errors from the platform layer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    /// This OS (or this milestone) has no implementation yet.
    #[error("not supported on this platform yet")]
    Unsupported,
    /// `DragMonitor::start` was called on a running monitor.
    #[error("the drag monitor is already running")]
    AlreadyRunning,
    /// A window was touched from a thread that does not own it.
    #[error("the window belongs to another thread; call this on the UI thread")]
    WrongThread,
    /// An OS call failed.
    #[error("{context} failed (OS error {code:#010x})")]
    Os {
        /// The call that failed.
        context: &'static str,
        /// The OS error code (an `HRESULT` on Windows).
        code: i32,
    },
}

/// Creates the drag monitor for this OS.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows; macOS stays
/// unsupported until M8.
pub fn drag_monitor() -> Result<Box<dyn DragMonitor>, PlatformError> {
    imp::drag_monitor()
}

/// The display under the global point (`x`, `y`), in physical pixels.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows, or an OS error.
pub fn display_bounds_at(x: i32, y: i32) -> Result<DisplayBounds, PlatformError> {
    imp::display_bounds_at(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_count() {
        assert_eq!(Modifiers::NONE.count(), 0);
        assert!(Modifiers::NONE.is_empty());
        for single in [
            Modifiers::SHIFT,
            Modifiers::CTRL,
            Modifiers::ALT,
            Modifiers::META,
        ] {
            assert_eq!(single.count(), 1);
            assert!(!single.is_empty());
        }
        let all = Modifiers {
            shift: true,
            ctrl: true,
            alt: true,
            meta: true,
        };
        assert_eq!(all.count(), 4);
        assert_eq!(Modifiers::default(), Modifiers::NONE);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn entry_points_are_unsupported_off_windows() {
        assert!(matches!(drag_monitor(), Err(PlatformError::Unsupported)));
        assert_eq!(display_bounds_at(0, 0), Err(PlatformError::Unsupported));
    }

    #[test]
    fn os_error_shows_hex_code() {
        let err = PlatformError::Os {
            context: "SetWindowPos",
            code: i32::from_ne_bytes(0x8007_0578_u32.to_ne_bytes()),
        };
        assert_eq!(err.to_string(), "SetWindowPos failed (OS error 0x80070578)");
    }
}
