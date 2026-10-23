//! The pure rules under the macOS layer: Cocoa's bottom-left coordinates to
//! Grape's top-left points and back, the screen under a point, and the text
//! and arguments the file actions use.
//!
//! No AppKit here, so these build and are tested on every OS, not only on a
//! Mac. The `macos` module is the only caller outside the tests.
//!
//! Grape's global coordinates on macOS are points, with the origin at the
//! top-left of the primary (menu-bar) screen and y growing downwards: the
//! Quartz display space that `CGDisplayBounds` uses. Cocoa puts the origin
//! at the bottom-left of that screen with y growing upwards. `H`, the
//! primary screen's height, flips between them: `y_grape = H - y_cocoa` for
//! a point, and `y_grape = H - (y_cocoa + height)` for a rectangle's top.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{DisplayBounds, Rect};

/// A rectangle in Cocoa's global space: points, origin at the bottom-left of
/// the primary screen, y growing upwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CocoaRect {
    /// Left edge.
    pub(crate) x: f64,
    /// Bottom edge.
    pub(crate) y: f64,
    /// Width in points.
    pub(crate) width: f64,
    /// Height in points.
    pub(crate) height: f64,
}

/// One screen in Grape's top-left points, as the main thread last read it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScreenInfo {
    /// The whole screen.
    pub(crate) monitor: Rect,
    /// The screen minus the menu bar and the Dock.
    pub(crate) work_area: Rect,
    /// Pixels per point (2.0 on a Retina screen). Kept for the logs; Grape's
    /// coordinates stay in points.
    pub(crate) backing_scale: f64,
}

impl ScreenInfo {
    /// The screen with Cocoa `frame` and `visible` (its `visibleFrame`),
    /// given the primary screen's height.
    pub(crate) fn from_cocoa(
        frame: CocoaRect,
        visible: CocoaRect,
        backing_scale: f64,
        primary_height: f64,
    ) -> Self {
        Self {
            monitor: to_grape(frame, primary_height),
            work_area: to_grape(visible, primary_height),
            backing_scale,
        }
    }

    /// What `display_bounds_at` reports for this screen. The scale is always
    /// 1.0 on macOS: one unit is one point is one CSS pixel.
    pub(crate) fn bounds(&self) -> DisplayBounds {
        DisplayBounds {
            monitor: self.monitor,
            work_area: self.work_area,
            scale: 1.0,
        }
    }
}

/// A Cocoa rectangle in Grape's top-left points, rounded to whole points.
pub(crate) fn to_grape(rect: CocoaRect, primary_height: f64) -> Rect {
    Rect {
        x: round(rect.x),
        y: round(primary_height - (rect.y + rect.height)),
        width: round(rect.width),
        height: round(rect.height),
    }
}

/// A rectangle in Grape's top-left points as Cocoa wants it (for
/// `setFrame:display:`).
pub(crate) fn to_cocoa(rect: Rect, primary_height: f64) -> CocoaRect {
    CocoaRect {
        x: f64::from(rect.x),
        y: primary_height - (f64::from(rect.y) + f64::from(rect.height)),
        width: f64::from(rect.width),
        height: f64::from(rect.height),
    }
}

/// `NSEvent.mouseLocation` (Cocoa, bottom-left) as a Grape point.
pub(crate) fn cursor_to_grape(x: f64, y: f64, primary_height: f64) -> (i32, i32) {
    (round(x), round(primary_height - y))
}

/// The screen holding the point (`x`, `y`), or else the nearest one. `None`
/// only when there are no screens. A screen holds its top and left edges
/// but not its bottom and right ones, so a point on a shared edge belongs
/// to exactly one screen.
pub(crate) fn screen_at(screens: &[ScreenInfo], x: i32, y: i32) -> Option<&ScreenInfo> {
    screens
        .iter()
        .find(|screen| contains(screen.monitor, x, y))
        .or_else(|| {
            screens
                .iter()
                .min_by_key(|screen| distance_squared(screen.monitor, x, y))
        })
}

/// The Move picker's message: "Move photo.png to…" or "Move 3 items to…".
pub(crate) fn picker_message(paths: &[PathBuf]) -> String {
    match paths {
        [one] => match one.file_name() {
            Some(name) => format!("Move {} to…", name.to_string_lossy()),
            None => "Move to…".to_owned(),
        },
        many => format!("Move {} items to…", many.len()),
    }
}

/// The paths one per line with `\n` between them, as Copy Path puts them
/// on the pasteboard. A name that isn't valid UTF-8 (rare on macOS, where
/// APFS and HFS+ both require it) gets U+FFFD for the bad bytes, because the
/// pasteboard holds Unicode text.
pub(crate) fn path_lines(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `open -a Terminal <dir>`: the folder goes as its own argument, never
/// through a shell.
pub(crate) fn terminal_args(dir: &Path) -> [OsString; 3] {
    ["-a".into(), "Terminal".into(), dir.as_os_str().to_owned()]
}

/// `open -R <path>`: Finder opens the folder holding `path` with it
/// selected. The path goes as its own argument, never through a shell.
pub(crate) fn reveal_args(path: &Path) -> [OsString; 2] {
    ["-R".into(), path.as_os_str().to_owned()]
}

fn contains(rect: Rect, x: i32, y: i32) -> bool {
    let (x, y) = (i64::from(x), i64::from(y));
    let (left, top) = (i64::from(rect.x), i64::from(rect.y));
    x >= left && x < left + i64::from(rect.width) && y >= top && y < top + i64::from(rect.height)
}

/// How far (`x`, `y`) is from the nearest point of `rect`, squared.
fn distance_squared(rect: Rect, x: i32, y: i32) -> i64 {
    let gap = |value: i32, start: i32, len: i32| -> i64 {
        let (value, start) = (i64::from(value), i64::from(start));
        let end = start + i64::from(len.max(1)) - 1;
        if value < start {
            start - value
        } else if value > end {
            value - end
        } else {
            0
        }
    };
    let dx = gap(x, rect.x, rect.width);
    let dy = gap(y, rect.y, rect.height);
    dx * dx + dy * dy
}

/// Rounds to the nearest whole point. `as` saturates out-of-range values
/// and turns NaN into 0, which is fine for screen geometry.
fn round(value: f64) -> i32 {
    value.round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cocoa(x: f64, y: f64, width: f64, height: f64) -> CocoaRect {
        CocoaRect {
            x,
            y,
            width,
            height,
        }
    }

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// A 1512×982 Retina laptop (primary) with a 2560×1440 external screen
    /// at 1.0 to its left and slightly above it, and a 1920×1080 one above
    /// the laptop. In Cocoa, "above" means larger y.
    fn layout() -> (f64, Vec<ScreenInfo>) {
        let h = 982.0;
        let screens = vec![
            // Primary: menu bar 33 pt tall, Dock 70 pt at the bottom.
            ScreenInfo::from_cocoa(
                cocoa(0.0, 0.0, 1512.0, 982.0),
                cocoa(0.0, 70.0, 1512.0, 879.0),
                2.0,
                h,
            ),
            // Left of the primary, its bottom 200 pt above the primary's.
            ScreenInfo::from_cocoa(
                cocoa(-2560.0, 200.0, 2560.0, 1440.0),
                cocoa(-2560.0, 200.0, 2560.0, 1415.0),
                1.0,
                h,
            ),
            // Straight above the primary.
            ScreenInfo::from_cocoa(
                cocoa(0.0, 982.0, 1920.0, 1080.0),
                cocoa(0.0, 982.0, 1920.0, 1055.0),
                1.0,
                h,
            ),
        ];
        (h, screens)
    }

    #[test]
    fn the_primary_screen_starts_at_the_origin() {
        let (_, screens) = layout();
        assert_eq!(screens[0].monitor, rect(0, 0, 1512, 982));
        // The menu bar is at the top and the Dock at the bottom.
        assert_eq!(screens[0].work_area, rect(0, 33, 1512, 879));
        assert!((screens[0].backing_scale - 2.0).abs() < 1e-9);
    }

    #[test]
    fn screens_left_of_and_above_the_primary_get_negative_coordinates() {
        let (_, screens) = layout();
        // Its top is at Cocoa y 1640, which is 658 pt above the primary's top.
        assert_eq!(screens[1].monitor, rect(-2560, -658, 2560, 1440));
        assert_eq!(screens[1].work_area, rect(-2560, -633, 2560, 1415));
        assert_eq!(screens[2].monitor, rect(0, -1080, 1920, 1080));
        assert_eq!(screens[2].work_area, rect(0, -1055, 1920, 1055));
    }

    #[test]
    fn the_cursor_flips_against_the_primary_height() {
        let h = 982.0;
        // Cocoa reports the top row of the primary as y = H.
        assert_eq!(cursor_to_grape(0.0, 982.0, h), (0, 0));
        assert_eq!(cursor_to_grape(1511.0, 1.0, h), (1511, 981));
        // Retina screens report half points.
        assert_eq!(cursor_to_grape(100.4, 500.6, h), (100, 481));
        // On the screen above, y is negative.
        assert_eq!(cursor_to_grape(10.0, 1500.0, h), (10, -518));
        // On the screen to the left, x is negative.
        assert_eq!(cursor_to_grape(-1280.0, 900.0, h), (-1280, 82));
    }

    #[test]
    fn the_flip_round_trips() {
        let h = 982.0;
        for frame in [
            rect(0, 0, 1512, 982),
            rect(-2560, -658, 2560, 1440),
            rect(200, -1000, 360, 360),
        ] {
            assert_eq!(to_grape(to_cocoa(frame, h), h), frame);
        }
        let wheel = to_cocoa(rect(100, 50, 300, 200), h);
        assert_eq!(wheel, cocoa(100.0, 732.0, 300.0, 200.0));
    }

    #[test]
    fn a_point_picks_the_screen_that_holds_it() {
        let (_, screens) = layout();
        let at = |x, y| screen_at(&screens, x, y).map(|screen| screen.monitor);
        assert_eq!(at(700, 500), Some(screens[0].monitor));
        assert_eq!(at(-1, 0), Some(screens[1].monitor));
        assert_eq!(at(-2560, -658), Some(screens[1].monitor));
        assert_eq!(at(500, -1), Some(screens[2].monitor));
        // The top edge belongs to the screen below it, the bottom edge
        // doesn't.
        assert_eq!(at(500, 0), Some(screens[0].monitor));
        assert_eq!(at(500, -1080), Some(screens[2].monitor));
    }

    #[test]
    fn a_point_off_every_screen_picks_the_nearest() {
        let (_, screens) = layout();
        let at = |x, y| screen_at(&screens, x, y).map(|screen| screen.monitor);
        // Below the primary.
        assert_eq!(at(700, 2000), Some(screens[0].monitor));
        // Right of the screen above, level with it.
        assert_eq!(at(1950, -500), Some(screens[2].monitor));
        // Far left.
        assert_eq!(at(-9000, 0), Some(screens[1].monitor));
        assert_eq!(screen_at(&[], 0, 0), None);
    }

    #[test]
    fn bounds_are_always_scale_one() {
        let (_, screens) = layout();
        let bounds = screens[0].bounds();
        assert_eq!(bounds.monitor, screens[0].monitor);
        assert_eq!(bounds.work_area, screens[0].work_area);
        assert!((bounds.scale - 1.0).abs() < 1e-9);
    }

    #[test]
    fn picker_messages_name_one_item_or_count_many() {
        assert_eq!(
            picker_message(&[PathBuf::from("/Users/max/photo.png")]),
            "Move photo.png to…"
        );
        assert_eq!(
            picker_message(&[PathBuf::from("/a"), PathBuf::from("/b")]),
            "Move 2 items to…"
        );
        assert_eq!(picker_message(&[PathBuf::from("/")]), "Move to…");
    }

    #[test]
    fn copy_path_puts_one_path_per_line() {
        let paths = [
            PathBuf::from("/Users/max/a b.png"),
            PathBuf::from("/Volumes/USB/c"),
        ];
        assert_eq!(path_lines(&paths), "/Users/max/a b.png\n/Volumes/USB/c");
        assert_eq!(path_lines(&paths[..1]), "/Users/max/a b.png");
        assert_eq!(path_lines(&[]), "");
    }

    #[test]
    fn open_gets_each_path_as_one_argument() {
        let dir = Path::new("/Users/max/My Files; $(whoami)");
        assert_eq!(
            terminal_args(dir),
            [
                OsString::from("-a"),
                OsString::from("Terminal"),
                OsString::from("/Users/max/My Files; $(whoami)"),
            ]
        );
        let file = Path::new("/Users/max/a \"b\" $c.png");
        assert_eq!(
            reveal_args(file),
            [
                OsString::from("-R"),
                OsString::from("/Users/max/a \"b\" $c.png")
            ]
        );
    }
}
