//! The pure rules under the macOS layer: Cocoa's bottom-left coordinates to
//! Grape's top-left points and back, the screen under a point, the text and
//! arguments the file actions use, how Move judges each item, and the float
//! shelf's drag out (what it offers, how it ended, whose drag it is, its
//! icons and frames).
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

use crate::file_actions::TerminalApp;
use crate::{DisplayBounds, DragOutcome, Rect};

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

/// iTerm2's bundle id: Launch Services finds the app by it, and `open -b`
/// opens it.
pub(crate) const ITERM_BUNDLE_ID: &str = "com.googlecode.iterm2";

/// The app Terminal opens on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacTerminal {
    /// Terminal ([`terminal_args`]).
    Terminal,
    /// iTerm2 ([`iterm_args`]).
    ITerm2,
}

/// The app Terminal opens on macOS for the user's `app`, and, when that
/// isn't `app`, why, for the log. iTerm2 opens only while it's installed,
/// which `iterm_installed` answers; nothing else asks it. Windows Terminal
/// and PowerShell aren't on a Mac, so they get Terminal, the default.
pub(crate) fn terminal_plan(
    app: TerminalApp,
    iterm_installed: impl FnOnce() -> bool,
) -> (MacTerminal, Option<&'static str>) {
    match app {
        TerminalApp::Terminal => (MacTerminal::Terminal, None),
        TerminalApp::ITerm2 => {
            if iterm_installed() {
                (MacTerminal::ITerm2, None)
            } else {
                (
                    MacTerminal::Terminal,
                    Some("iTerm2 (com.googlecode.iterm2) isn't installed, so Terminal opens"),
                )
            }
        }
        TerminalApp::WindowsTerminal | TerminalApp::PowerShell => (
            MacTerminal::Terminal,
            Some("Windows Terminal and PowerShell aren't offered on macOS, so Terminal opens"),
        ),
    }
}

/// `open -a Terminal <dir>`: the folder goes as its own argument, never
/// through a shell.
pub(crate) fn terminal_args(dir: &Path) -> [OsString; 3] {
    ["-a".into(), "Terminal".into(), dir.as_os_str().to_owned()]
}

/// `open -b com.googlecode.iterm2 <dir>`: iTerm2 opens a window in the
/// folder. The folder goes as its own argument, never through a shell.
pub(crate) fn iterm_args(dir: &Path) -> [OsString; 3] {
    [
        "-b".into(),
        ITERM_BUNDLE_ID.into(),
        dir.as_os_str().to_owned(),
    ]
}

/// `open -R <path>`: Finder opens the folder holding `path` with it
/// selected. The path goes as its own argument, never through a shell.
pub(crate) fn reveal_args(path: &Path) -> [OsString; 2] {
    ["-R".into(), path.as_os_str().to_owned()]
}

/// Where one item asked to Move sits, as device numbers (`st_dev`). `None`
/// is a device that couldn't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ItemDevices {
    /// The item itself, from `lstat`: a link counts where the link is, and a
    /// mount point reports the volume mounted there.
    pub(crate) item: Option<u64>,
    /// The folder holding it, with links followed.
    pub(crate) folder: Option<u64>,
}

impl ItemDevices {
    /// `true` for the root of a mounted volume (a USB stick, a DMG, a
    /// network share): its own device isn't its folder's. Moving one would
    /// copy what's on it and then erase it, so it never moves. A folder
    /// that can't be read counts as a root too, so in doubt nothing moves.
    /// An item that can't be read is left to the move, which reports it.
    ///
    /// The firmlinked folders at the top of the startup disk (`/Users`,
    /// `/Applications`) count as roots as well; they couldn't move off the
    /// read-only system volume anyway.
    pub(crate) fn is_volume_root(self) -> bool {
        match (self.item, self.folder) {
            (Some(item), Some(folder)) => item != folder,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// `true` when moving the item to a folder on device `target` may cross
    /// volumes: the item or its folder is on another device. Anything that
    /// can't be read counts as another volume, so the user is asked.
    pub(crate) fn crosses_to(self, target: Option<u64>) -> bool {
        match (self.item, self.folder, target) {
            (Some(item), Some(folder), Some(target)) => item != target || folder != target,
            _ => true,
        }
    }
}

/// What the item's old place says, read right after its own move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OldPlace {
    /// The item is still there.
    Present,
    /// The item is gone, and its folder can still be read.
    Gone,
    /// The item or its folder can't be read.
    Unreadable,
}

/// One Move item, as the disk stood right after its own `moveItemAtURL`
/// and before the next item moved, so a later item with the same name
/// can't change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MoveSnapshot {
    /// Grape asked for the move. It doesn't when the name is already taken
    /// in the folder, the item has no name (`/`) or it's a volume's root.
    pub(crate) tried: bool,
    /// `NSFileManager` reported an error.
    pub(crate) failed: bool,
    /// `to/name` exists now and didn't before this item's move.
    pub(crate) arrived: bool,
    /// The old place.
    pub(crate) old_place: OldPlace,
}

/// How one Move item ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveVerdict {
    /// It left its old place.
    Moved,
    /// It may be in both places, or its old place can't be read.
    Unsure,
    /// It's where it was.
    Stayed,
}

/// Sorts one Move item by its [`MoveSnapshot`].
///
/// An item Grape didn't try to move stayed. One that's gone from a
/// folder that can still be read moved, unless its move failed and it
/// didn't arrive either: then Grape can't tell where it went. One still
/// in its old place while `to/name` arrived is in both places, partly or
/// wholly: a move to another volume copies first, and a folder can fail
/// halfway. An old place that can't be read can't be judged. Those three
/// are unsure. Everything else stayed.
pub(crate) fn judge_move(snapshot: MoveSnapshot) -> MoveVerdict {
    if !snapshot.tried {
        return MoveVerdict::Stayed;
    }
    match snapshot.old_place {
        OldPlace::Gone if snapshot.arrived || !snapshot.failed => MoveVerdict::Moved,
        OldPlace::Gone | OldPlace::Unreadable => MoveVerdict::Unsure,
        OldPlace::Present if snapshot.arrived => MoveVerdict::Unsure,
        OldPlace::Present => MoveVerdict::Stayed,
    }
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

/// About how many bytes the Mac HEIC worker holds at most for a `width` ×
/// `height` image of `depth` bits a sample (`kCGImagePropertyDepth`;
/// `None` counts as 8): ImageIO's decoded picture (4 bytes a pixel at 8
/// bits, 8 above, since float is off), plus the larger of its YUV planes
/// while it decodes (2 bytes a pixel, 3 above 8 bits) and Grape's 8-bit
/// RGBA bitmap while the picture is drawn (4). Checked against the
/// request's `max_decoder_memory` before anything is decoded (D33), as
/// libheif holds to it on Windows.
pub(crate) fn heif_decode_bytes(width: u32, height: u32, depth: Option<i64>) -> u64 {
    let per_pixel: u64 = if depth.is_some_and(|bits| bits > 8) {
        8 + 4
    } else {
        4 + 4
    };
    (u64::from(width) * u64::from(height)).saturating_mul(per_pixel)
}

/// AppKit's `NSDragOperation` bits, as plain numbers so every OS tests the
/// rules below. A drag out of the shelf only ever offers the first two.
pub(crate) const DRAG_COPY: usize = 1;
/// An alias (Finder) or a link.
pub(crate) const DRAG_LINK: usize = 2;
/// "Whatever the target likes": Finder moves on the same volume.
pub(crate) const DRAG_GENERIC: usize = 4;
/// A private arrangement between source and target.
pub(crate) const DRAG_PRIVATE: usize = 8;
/// The target takes the files away.
pub(crate) const DRAG_MOVE: usize = 16;
/// The Dock's Trash: the source would delete them.
pub(crate) const DRAG_DELETE: usize = 32;

/// What a drag out of the shelf offers, in every dragging context: copy or
/// link, never Generic, Private, Move or Delete (golden rule 2). Nothing
/// once the drag is `stale` (its button was let go before AppKit began
/// it), so nothing can drop it.
pub(crate) fn drag_mask(stale: bool) -> usize {
    if stale {
        0
    } else {
        DRAG_COPY | DRAG_LINK
    }
}

/// How a drag out ended, from the operation AppKit reports and whether the
/// drop landed on one of Grape's own windows. Grape's webviews answer every
/// drop with a copy and ignore it, so a drop there is cancelled. Otherwise
/// Copy wins (as on Windows), then Link; anything else is cancelled.
pub(crate) fn drag_outcome(operation: usize, on_grape: bool) -> DragOutcome {
    if on_grape {
        DragOutcome::Cancelled
    } else if operation & DRAG_COPY != 0 {
        DragOutcome::Copied
    } else if operation & DRAG_LINK != 0 {
        DragOutcome::Linked
    } else {
        DragOutcome::Cancelled
    }
}

/// The names of the bits in `operation` that a drag out never offers, for
/// a log line: "Generic|Move", say, or a hex rest. Empty when there are
/// none.
pub(crate) fn unexpected_operation(operation: usize) -> String {
    let mut names: Vec<String> = [
        (DRAG_GENERIC, "Generic"),
        (DRAG_PRIVATE, "Private"),
        (DRAG_MOVE, "Move"),
        (DRAG_DELETE, "Delete"),
    ]
    .into_iter()
    .filter(|(bit, _)| operation & bit != 0)
    .map(|(_, name)| name.to_owned())
    .collect();
    let known = DRAG_COPY | DRAG_LINK | DRAG_GENERIC | DRAG_PRIVATE | DRAG_MOVE | DRAG_DELETE;
    let rest = operation & !known;
    if rest != 0 {
        names.push(format!("{rest:#x}"));
    }
    names.join("|")
}

/// Whether Grape started a drag out of the shelf during the press that
/// began when its count of them was `at_press`: the count is `now` and
/// differs. Counts wrap, so any difference counts.
pub(crate) fn own_drag_in_press(at_press: u64, now: u64) -> bool {
    at_press != now
}

/// The type whose icon a drag item shows (`NSWorkspace.iconForFileType`):
/// `public.folder` for a folder, the extension for a file that has one in
/// UTF-8, else `public.data`. Never looks at the disk.
pub(crate) fn drag_icon_type(path: &Path, folder: bool) -> String {
    if folder {
        return "public.folder".to_owned();
    }
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if !ext.is_empty() => ext.to_owned(),
        _ => "public.data".to_owned(),
    }
}

/// The dragging frames of `count` items, in the dragging view's own
/// (unflipped) coordinates: each a `size` square centered on `at`, the
/// pointer, and item `i` moved `3 × min(i, 4)` points right and down, so a
/// few show as a small stack.
pub(crate) fn drag_item_frames(count: usize, at: (f64, f64), size: f64) -> Vec<CocoaRect> {
    (0..count)
        .map(|index| {
            let offset = 3.0 * index.min(4) as f64;
            CocoaRect {
                x: at.0 - size / 2.0 + offset,
                y: at.1 - size / 2.0 - offset,
                width: size,
                height: size,
            }
        })
        .collect()
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

    fn devices(item: Option<u64>, folder: Option<u64>) -> ItemDevices {
        ItemDevices { item, folder }
    }

    #[test]
    fn a_volume_root_is_on_another_device_than_its_folder() {
        // /Users/max/a.png, a link on the Desktop, a folder inside a USB
        // stick: each on its folder's device.
        assert!(!devices(Some(1), Some(1)).is_volume_root());
        assert!(!devices(Some(7), Some(7)).is_volume_root());
        // /Volumes/USB, a mounted DMG or an SMB share: /Volumes is on the
        // startup disk, the item on its own volume.
        assert!(devices(Some(7), Some(1)).is_volume_root());
        // A folder that can't be read: in doubt, it stays.
        assert!(devices(Some(7), None).is_volume_root());
        // An item that can't be read is the move's to report.
        assert!(!devices(None, Some(1)).is_volume_root());
        assert!(!devices(None, None).is_volume_root());
    }

    #[test]
    fn crossing_volumes_checks_the_item_and_its_folder() {
        let target = Some(1);
        assert!(!devices(Some(1), Some(1)).crosses_to(target));
        // Its folder is elsewhere.
        assert!(devices(Some(7), Some(7)).crosses_to(target));
        // Its folder is on the target's volume but it isn't: a volume
        // mounted inside a folder there.
        assert!(devices(Some(7), Some(1)).crosses_to(target));
        // Its folder is elsewhere but it isn't.
        assert!(devices(Some(1), Some(7)).crosses_to(target));
        // Anything unreadable asks.
        assert!(devices(None, Some(1)).crosses_to(target));
        assert!(devices(Some(1), None).crosses_to(target));
        assert!(devices(Some(1), Some(1)).crosses_to(None));
    }

    fn snapshot(tried: bool, failed: bool, arrived: bool, old_place: OldPlace) -> MoveSnapshot {
        MoveSnapshot {
            tried,
            failed,
            arrived,
            old_place,
        }
    }

    #[test]
    fn a_move_is_judged_by_the_normal_rules() {
        use MoveVerdict::{Moved, Stayed, Unsure};
        use OldPlace::{Gone, Present, Unreadable};
        let cases = [
            // It left and arrived.
            (snapshot(true, false, true, Gone), Moved),
            // It left without an error, though it's not at `to/name`.
            (snapshot(true, false, false, Gone), Moved),
            // An error, but it left and arrived (a warning after the move).
            (snapshot(true, true, true, Gone), Moved),
            // An error, it left and never arrived: gone before the move,
            // or lost on the way.
            (snapshot(true, true, false, Gone), Unsure),
            // In both places: a copy to another volume or a folder split
            // halfway.
            (snapshot(true, true, true, Present), Unsure),
            (snapshot(true, false, true, Present), Unsure),
            // The old place can't be read.
            (snapshot(true, false, false, Unreadable), Unsure),
            (snapshot(true, true, true, Unreadable), Unsure),
            // It failed and is still where it was.
            (snapshot(true, true, false, Present), Stayed),
            // Never tried: a taken name, `/` or a volume root.
            (snapshot(false, false, false, Present), Stayed),
            (snapshot(false, false, false, Gone), Stayed),
            (snapshot(false, false, false, Unreadable), Stayed),
        ];
        for (snapshot, verdict) in cases {
            assert_eq!(judge_move(snapshot), verdict, "{snapshot:?}");
        }
    }

    #[test]
    fn a_failed_move_is_not_moved_by_a_same_named_item_arriving_later() {
        // /a/x.png can't move and is gone from /a (deleted while it was
        // being copied, say). Its snapshot, taken before /b/x.png moved:
        let first = snapshot(true, true, false, OldPlace::Gone);
        assert_eq!(judge_move(first), MoveVerdict::Unsure);
        // /b/x.png then takes the free name and moves.
        let second = snapshot(true, false, true, OldPlace::Gone);
        assert_eq!(judge_move(second), MoveVerdict::Moved);
        // Re-read after both moves, the first would look as if it arrived
        // and be claimed as moved. That's what the snapshot prevents.
        let reread = MoveSnapshot {
            arrived: true,
            ..first
        };
        assert_eq!(judge_move(reread), MoveVerdict::Moved);
    }

    #[test]
    fn a_locked_item_stays_while_a_same_named_item_arrives() {
        // /a/x.png is locked: its move fails and it's still in /a. Its
        // snapshot, taken before /b/x.png moved:
        let locked = snapshot(true, true, false, OldPlace::Present);
        assert_eq!(judge_move(locked), MoveVerdict::Stayed);
        // Re-read after /b/x.png arrived, it would look as if it were in
        // both places.
        let reread = MoveSnapshot {
            arrived: true,
            ..locked
        };
        assert_eq!(judge_move(reread), MoveVerdict::Unsure);
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
        assert_eq!(
            iterm_args(dir),
            [
                OsString::from("-b"),
                OsString::from("com.googlecode.iterm2"),
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

    #[test]
    fn iterm2_opens_only_when_it_is_chosen_and_installed() {
        assert_eq!(
            terminal_plan(TerminalApp::ITerm2, || true),
            (MacTerminal::ITerm2, None)
        );
        let (app, note) = terminal_plan(TerminalApp::ITerm2, || false);
        assert_eq!(app, MacTerminal::Terminal);
        assert!(note.is_some_and(|note| note.contains("com.googlecode.iterm2")));
    }

    #[test]
    fn terminal_never_asks_whether_iterm2_is_installed() {
        let asked = std::cell::Cell::new(false);
        let installed = || {
            asked.set(true);
            true
        };
        assert_eq!(
            terminal_plan(TerminalApp::Terminal, installed),
            (MacTerminal::Terminal, None)
        );
        assert!(!asked.get());
    }

    #[test]
    fn the_mac_heif_decoder_budget_takes_8_bit_and_refuses_deep_giants() {
        const BUDGET: u64 = 1 << 30; // grape_heif_proto::Limits::DEFAULT
                                     // A 48 MP iPhone photo, 8-bit: about 390 MB.
        assert_eq!(heif_decode_bytes(8064, 6048, Some(8)), 8064 * 6048 * 8);
        assert!(heif_decode_bytes(8064, 6048, Some(8)) <= BUDGET);
        // The decoded-pixel cap (134,217,728) at 8 bits still fits.
        assert!(heif_decode_bytes(16384, 8192, None) <= BUDGET);
        // A 100 MP 10-bit HEIF doesn't: refused before ImageIO decodes it.
        assert_eq!(heif_decode_bytes(11648, 8736, Some(10)), 11648 * 8736 * 12);
        assert!(heif_decode_bytes(11648, 8736, Some(10)) > BUDGET);
        // No overflow at the extremes: it saturates.
        assert_eq!(heif_decode_bytes(u32::MAX, u32::MAX, Some(16)), u64::MAX);
    }

    #[test]
    fn the_windows_apps_get_terminal_on_a_mac() {
        for app in [TerminalApp::WindowsTerminal, TerminalApp::PowerShell] {
            let (opens, note) = terminal_plan(app, || panic!("only iTerm2 asks"));
            assert_eq!(opens, MacTerminal::Terminal);
            assert!(note.is_some());
        }
    }

    #[test]
    fn a_drag_out_offers_copy_or_link_and_nothing_else() {
        let mask = drag_mask(false);
        assert_eq!(mask, DRAG_COPY | DRAG_LINK);
        for never in [DRAG_GENERIC, DRAG_PRIVATE, DRAG_MOVE, DRAG_DELETE] {
            assert_eq!(mask & never, 0, "{never}");
        }
        // Stale: nothing can drop it.
        assert_eq!(drag_mask(true), 0);
    }

    #[test]
    fn a_drag_out_ends_copied_linked_or_cancelled() {
        assert_eq!(drag_outcome(DRAG_COPY, false), DragOutcome::Copied);
        assert_eq!(
            drag_outcome(DRAG_COPY | DRAG_LINK, false),
            DragOutcome::Copied
        );
        assert_eq!(drag_outcome(DRAG_LINK, false), DragOutcome::Linked);
        assert_eq!(drag_outcome(0, false), DragOutcome::Cancelled);
        for other in [DRAG_GENERIC, DRAG_PRIVATE, DRAG_MOVE, DRAG_DELETE] {
            assert_eq!(
                drag_outcome(other, false),
                DragOutcome::Cancelled,
                "{other}"
            );
        }
        // On one of Grape's own windows nothing happened, whatever wry said.
        for operation in [0, DRAG_COPY, DRAG_LINK, DRAG_COPY | DRAG_LINK, DRAG_MOVE] {
            assert_eq!(
                drag_outcome(operation, true),
                DragOutcome::Cancelled,
                "{operation}"
            );
        }
    }

    #[test]
    fn unexpected_operations_are_named_for_the_log() {
        assert_eq!(unexpected_operation(0), "");
        assert_eq!(unexpected_operation(DRAG_COPY | DRAG_LINK), "");
        assert_eq!(
            unexpected_operation(DRAG_GENERIC | DRAG_MOVE),
            "Generic|Move"
        );
        assert_eq!(
            unexpected_operation(DRAG_PRIVATE | DRAG_DELETE | DRAG_COPY),
            "Private|Delete"
        );
        assert_eq!(unexpected_operation(64 | DRAG_COPY), "0x40");
    }

    #[test]
    fn a_press_is_grapes_own_once_the_count_moved() {
        assert!(!own_drag_in_press(0, 0));
        assert!(!own_drag_in_press(7, 7));
        assert!(own_drag_in_press(7, 8));
        assert!(own_drag_in_press(u64::MAX, 0));
    }

    #[test]
    fn drag_icons_come_from_the_type_never_the_file() {
        assert_eq!(
            drag_icon_type(Path::new("/a/Invoices"), true),
            "public.folder"
        );
        assert_eq!(drag_icon_type(Path::new("/a/b.c.d"), true), "public.folder");
        assert_eq!(drag_icon_type(Path::new("/a/photo.JPG"), false), "JPG");
        assert_eq!(drag_icon_type(Path::new("/a/README"), false), "public.data");
        assert_eq!(
            drag_icon_type(Path::new("/a/.bashrc"), false),
            "public.data"
        );
        assert_eq!(
            drag_icon_type(Path::new("/a/trailing."), false),
            "public.data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_extension_that_isnt_utf8_gets_the_data_icon() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new("/a").join(OsStr::from_bytes(b"photo.jp\xe9"));
        assert_eq!(drag_icon_type(&path, false), "public.data");
    }

    #[test]
    fn drag_frames_stack_from_the_pointer() {
        let frames = drag_item_frames(7, (100.0, 200.0), 48.0);
        assert_eq!(frames.len(), 7);
        // The first is centered on the pointer.
        assert_eq!(frames[0], cocoa(76.0, 176.0, 48.0, 48.0));
        // Then 3 points right and down (y goes up in Cocoa) each.
        assert_eq!(frames[1], cocoa(79.0, 173.0, 48.0, 48.0));
        assert_eq!(frames[4], cocoa(88.0, 164.0, 48.0, 48.0));
        // From the fifth on they sit on the fifth.
        assert_eq!(frames[5], frames[4]);
        assert_eq!(frames[6], frames[4]);
        for frame in &frames {
            assert!((frame.width - 48.0).abs() < 1e-9 && (frame.height - 48.0).abs() < 1e-9);
        }
        assert!(drag_item_frames(0, (0.0, 0.0), 48.0).is_empty());
    }
}
