//! The screen under a point, in Grape's top-left points.
//!
//! `NSScreen` is main-thread only, but `display_bounds_at` is called from the
//! app's controller thread. So the main thread reads the screens into a
//! process-wide cache ([`refresh_screens`]), and other threads read the
//! cache. A drag start, every overlay show and an empty cache each ask the
//! main thread for a fresh read ([`request_refresh`]); none of them wait
//! for it. While the cache is empty, CoreGraphics answers instead: its
//! display bounds are already top-left points and safe on any thread, but
//! it has no work area, so the whole screen stands in for it.

use std::sync::{Mutex, PoisonError};

use dispatch2::DispatchQueue;
use objc2::rc::autoreleasepool;
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_core_graphics::{CGDisplayBounds, CGError, CGGetActiveDisplayList};
use objc2_foundation::NSRect;

use super::NO_CODE;
use crate::mac_rules::{screen_at, CocoaRect, ScreenInfo};
use crate::{DisplayBounds, PlatformError, Rect};

/// The screens as the main thread last read them, in Grape's coordinates.
static SCREENS: Mutex<Vec<ScreenInfo>> = Mutex::new(Vec::new());

/// More displays than any Mac drives at once.
const MAX_DISPLAYS: usize = 32;

/// Reads the screens into the cache that [`crate::display_bounds_at`] uses.
/// Grape calls it itself when a drag starts and when the overlay is shown;
/// call it too when the screens change (Tauri's `ScaleFactorChanged`, say).
///
/// # Errors
/// [`PlatformError::WrongThread`] off the main thread.
pub fn refresh_screens() -> Result<(), PlatformError> {
    let mtm = MainThreadMarker::new().ok_or(PlatformError::WrongThread)?;
    refresh(mtm);
    Ok(())
}

/// Reads the screens into the cache and returns the primary screen's
/// height, or `None` when there are no screens (the cache is then empty).
pub(crate) fn refresh(mtm: MainThreadMarker) -> Option<f64> {
    let read = autoreleasepool(|_| {
        let screens = NSScreen::screens(mtm);
        // Screen 0 is the one with the menu bar: Cocoa's origin.
        let primary_height = screens.firstObject()?.frame().size.height;
        let infos: Vec<ScreenInfo> = screens
            .iter()
            .map(|screen| {
                ScreenInfo::from_cocoa(
                    cocoa(screen.frame()),
                    cocoa(screen.visibleFrame()),
                    screen.backingScaleFactor(),
                    primary_height,
                )
            })
            .collect();
        Some((primary_height, infos))
    });
    let mut cache = SCREENS.lock().unwrap_or_else(PoisonError::into_inner);
    match read {
        Some((primary_height, screens)) => {
            *cache = screens;
            Some(primary_height)
        }
        None => {
            cache.clear();
            None
        }
    }
}

/// Asks the main thread to refresh the cache, and returns at once.
pub(crate) fn request_refresh() {
    DispatchQueue::main().exec_async(|| {
        let _ = refresh_screens();
    });
}

pub(crate) fn display_bounds_at(x: i32, y: i32) -> Result<DisplayBounds, PlatformError> {
    let cached = screen_at(
        &SCREENS.lock().unwrap_or_else(PoisonError::into_inner),
        x,
        y,
    )
    .map(ScreenInfo::bounds);
    if let Some(bounds) = cached {
        return Ok(bounds);
    }
    request_refresh();
    core_graphics_bounds_at(x, y)
}

/// The display under (`x`, `y`), or the nearest one, from CoreGraphics.
/// The work area is the whole display: CoreGraphics doesn't know where the
/// menu bar and the Dock are.
fn core_graphics_bounds_at(x: i32, y: i32) -> Result<DisplayBounds, PlatformError> {
    let mut ids = [0_u32; MAX_DISPLAYS];
    let mut count = 0_u32;
    // SAFETY: `ids` has room for MAX_DISPLAYS entries, which is the maximum
    // passed, and `count` is a valid u32 to write the number found to.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        return Err(PlatformError::Os {
            context: "CGGetActiveDisplayList",
            code: err.0,
        });
    }
    let found = usize::try_from(count).map_or(0, |count| count.min(MAX_DISPLAYS));
    let screens: Vec<ScreenInfo> = ids
        .iter()
        .take(found)
        .map(|&id| {
            let monitor = grape_rect(CGDisplayBounds(id));
            ScreenInfo {
                monitor,
                work_area: monitor,
                backing_scale: 1.0,
            }
        })
        .filter(|screen| screen.monitor.width > 0 && screen.monitor.height > 0)
        .collect();
    screen_at(&screens, x, y)
        .map(ScreenInfo::bounds)
        .ok_or(PlatformError::Os {
            context: "CGGetActiveDisplayList",
            code: if found == 0 {
                CGError::NoneAvailable.0
            } else {
                NO_CODE
            },
        })
}

fn cocoa(rect: NSRect) -> CocoaRect {
    CocoaRect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

/// A CoreGraphics rectangle, already in top-left points, rounded.
fn grape_rect(rect: NSRect) -> Rect {
    Rect {
        x: rect.origin.x.round() as i32,
        y: rect.origin.y.round() as i32,
        width: rect.size.width.round() as i32,
        height: rect.size.height.round() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_bounds_never_fail_quietly() {
        // CI's Mac may or may not have a display. Either way the answer is
        // a real screen in points, or an error that says why.
        match display_bounds_at(0, 0) {
            Ok(bounds) => {
                assert!(bounds.monitor.width > 0 && bounds.monitor.height > 0);
                assert!(bounds.work_area.width > 0 && bounds.work_area.height > 0);
                assert!((bounds.scale - 1.0).abs() < 1e-9);
            }
            Err(err) => assert!(matches!(err, PlatformError::Os { .. }), "{err}"),
        }
    }

    #[test]
    fn the_cache_is_filled_only_on_the_main_thread() {
        // Test threads aren't the main thread.
        assert_eq!(refresh_screens(), Err(PlatformError::WrongThread));
    }
}
