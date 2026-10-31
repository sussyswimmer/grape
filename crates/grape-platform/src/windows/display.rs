//! The monitor under a point: `MonitorFromPoint` + `GetMonitorInfoW` for
//! the rectangles and `GetDpiForMonitor` for the scale
//! (`docs/PLATFORM_WINDOWS.md` → "Overlay window").

use ::windows::Win32::Foundation::{E_FAIL, POINT, RECT};
use ::windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use ::windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

use crate::{DisplayBounds, PlatformError, Rect};

/// Windows' reference DPI: 100 % scaling.
const BASE_DPI: f64 = 96.0;

pub(crate) fn display_bounds_at(x: i32, y: i32) -> Result<DisplayBounds, PlatformError> {
    // SAFETY: takes the point by value. DEFAULTTONEAREST always returns a
    // monitor, even for a point between or off all monitors.
    let monitor = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };

    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..MONITORINFO::default()
    };
    // SAFETY: `info` is a writable MONITORINFO with `cbSize` set, as the
    // call requires.
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return Err(PlatformError::Os {
            context: "GetMonitorInfoW",
            code: E_FAIL.0,
        });
    }

    let (mut dpi_x, mut dpi_y) = (0_u32, 0_u32);
    // SAFETY: both out-pointers are valid u32s for the call.
    unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.map_err(
        |err| PlatformError::Os {
            context: "GetDpiForMonitor",
            code: err.code().0,
        },
    )?;

    Ok(DisplayBounds {
        monitor: to_rect(info.rcMonitor),
        work_area: to_rect(info.rcWork),
        scale: scale_for_dpi(dpi_x),
    })
}

fn to_rect(r: RECT) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        width: r.right - r.left,
        height: r.bottom - r.top,
    }
}

/// Physical pixels per logical pixel. A DPI of 0 (a failed query that still
/// reported success) counts as 100 %.
fn scale_for_dpi(dpi: u32) -> f64 {
    if dpi == 0 {
        1.0
    } else {
        f64::from(dpi) / BASE_DPI
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_follow_the_dpi() {
        assert!((scale_for_dpi(96) - 1.0).abs() < 1e-9);
        assert!((scale_for_dpi(144) - 1.5).abs() < 1e-9);
        assert!((scale_for_dpi(192) - 2.0).abs() < 1e-9);
        assert!((scale_for_dpi(0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rects_become_origin_and_size() {
        let r = RECT {
            left: -1920,
            top: 10,
            right: 0,
            bottom: 1090,
        };
        assert_eq!(
            to_rect(r),
            Rect {
                x: -1920,
                y: 10,
                width: 1920,
                height: 1080,
            }
        );
    }

    #[test]
    fn the_primary_monitor_has_bounds() {
        let bounds = display_bounds_at(0, 0).unwrap();
        assert!(bounds.monitor.width > 0 && bounds.monitor.height > 0);
        assert!(bounds.work_area.width > 0 && bounds.work_area.height > 0);
        assert!(bounds.work_area.width <= bounds.monitor.width);
        assert!(bounds.scale >= 1.0);
    }
}
