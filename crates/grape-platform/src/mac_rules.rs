//! The pure rules under the macOS layer: Cocoa's bottom-left coordinates to
//! Grape's top-left points and back, the screen under a point, the text and
//! arguments the file actions use, how Move judges each item, and the float
//! shelf's drag out (what it offers, how it ended, whose drag it is, its
//! icons and frames) and hand-back (the apps the user was last in), the
//! paired calls behind relaunch at login, and the Shortcuts wheel's tables
//! (virtual keys, Carbon and CGEvent masks), typing chunks and programs.
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

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::file_actions::TerminalApp;
use crate::keys::{Chord, KeyCode};
use crate::shortcuts::{
    HotkeyId, LocalTime, OpenTarget, PointerSignal, Program, ProgramRequest, SystemControl,
    SystemPlan,
};
use crate::{DisplayBounds, DragOutcome, Modifiers, Rect};

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

/// About how many bytes the Mac image helper worker holds at most for a `width` ×
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

/// How many of the apps the user was last in [`note_activation`] keeps.
pub(crate) const RECENT_APPS: usize = 8;

/// Notes that the app with `pid` became active: it moves to the front of
/// `recent` (most recent first, each pid once), and the oldest goes past
/// [`RECENT_APPS`]. Grape's own activations (`grape`) and pids that name no
/// process (0 or less) aren't noted, so the list only holds the apps the
/// user came from. The float shelf hands activation back to the first one
/// that still runs.
pub(crate) fn note_activation(recent: &mut Vec<i32>, pid: i32, grape: i32) {
    if pid == grape || pid <= 0 {
        return;
    }
    recent.retain(|&noted| noted != pid);
    recent.insert(0, pid);
    recent.truncate(RECENT_APPS);
}

/// A call that changes AppKit's relaunch-at-login counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelaunchCall {
    /// `-[NSApplication disableRelaunchOnLogin]`: one up.
    Disable,
    /// `-[NSApplication enableRelaunchOnLogin]`: one down.
    Enable,
}

/// The call that makes macOS relaunch Grape at login (`relaunch`) or not,
/// given whether Grape's one disable is in effect now (`disabled`), or none
/// when it already matches. AppKit keeps a counter, not a flag, and
/// relaunches only at zero, so Grape makes at most one disable and pairs it
/// with exactly one enable: repeating either call never stacks.
pub(crate) fn relaunch_call(disabled: bool, relaunch: bool) -> Option<RelaunchCall> {
    match (disabled, relaunch) {
        (false, false) => Some(RelaunchCall::Disable),
        (true, true) => Some(RelaunchCall::Enable),
        (false, true) | (true, false) => None,
    }
}

/// The US ANSI virtual key code (`kVK_ANSI_A` and the rest) of a key. A
/// virtual key is a place on the keyboard on every layout, as a
/// [`KeyCode`] is, so a chord is posted from where the user set it.
pub(crate) fn virtual_key(key: KeyCode) -> u16 {
    match key {
        KeyCode::KeyA => 0,
        KeyCode::KeyB => 11,
        KeyCode::KeyC => 8,
        KeyCode::KeyD => 2,
        KeyCode::KeyE => 14,
        KeyCode::KeyF => 3,
        KeyCode::KeyG => 5,
        KeyCode::KeyH => 4,
        KeyCode::KeyI => 34,
        KeyCode::KeyJ => 38,
        KeyCode::KeyK => 40,
        KeyCode::KeyL => 37,
        KeyCode::KeyM => 46,
        KeyCode::KeyN => 45,
        KeyCode::KeyO => 31,
        KeyCode::KeyP => 35,
        KeyCode::KeyQ => 12,
        KeyCode::KeyR => 15,
        KeyCode::KeyS => 1,
        KeyCode::KeyT => 17,
        KeyCode::KeyU => 32,
        KeyCode::KeyV => 9,
        KeyCode::KeyW => 13,
        KeyCode::KeyX => 7,
        KeyCode::KeyY => 16,
        KeyCode::KeyZ => 6,
        KeyCode::Digit0 => 29,
        KeyCode::Digit1 => 18,
        KeyCode::Digit2 => 19,
        KeyCode::Digit3 => 20,
        KeyCode::Digit4 => 21,
        KeyCode::Digit5 => 23,
        KeyCode::Digit6 => 22,
        KeyCode::Digit7 => 26,
        KeyCode::Digit8 => 28,
        KeyCode::Digit9 => 25,
        KeyCode::F1 => 122,
        KeyCode::F2 => 120,
        KeyCode::F3 => 99,
        KeyCode::F4 => 118,
        KeyCode::F5 => 96,
        KeyCode::F6 => 97,
        KeyCode::F7 => 98,
        KeyCode::F8 => 100,
        KeyCode::F9 => 101,
        KeyCode::F10 => 109,
        KeyCode::F11 => 103,
        KeyCode::F12 => 111,
        KeyCode::F13 => 105,
        KeyCode::F14 => 107,
        KeyCode::F15 => 113,
        KeyCode::F16 => 106,
        KeyCode::F17 => 64,
        KeyCode::F18 => 79,
        KeyCode::F19 => 80,
        KeyCode::F20 => 90,
        KeyCode::Space => 49,
        KeyCode::Enter => 36,
        KeyCode::Tab => 48,
        KeyCode::Escape => 53,
        KeyCode::Backspace => 51,
        KeyCode::Delete => 117,
        KeyCode::ArrowUp => 126,
        KeyCode::ArrowDown => 125,
        KeyCode::ArrowLeft => 123,
        KeyCode::ArrowRight => 124,
        KeyCode::Home => 115,
        KeyCode::End => 119,
        KeyCode::PageUp => 116,
        KeyCode::PageDown => 121,
        KeyCode::Minus => 27,
        KeyCode::Equal => 24,
        KeyCode::BracketLeft => 33,
        KeyCode::BracketRight => 30,
        KeyCode::Backslash => 42,
        KeyCode::Semicolon => 41,
        KeyCode::Quote => 39,
        KeyCode::Comma => 43,
        KeyCode::Period => 47,
        KeyCode::Slash => 44,
        KeyCode::Backquote => 50,
    }
}

/// The virtual key that types `ch` with no modifier on the US layout:
/// letters (either case), digits and the punctuation keys. The fallback for
/// a character chord when the current layout has no such key or can't be
/// read.
pub(crate) fn us_key_for_char(ch: char) -> Option<u16> {
    let key = match ch.to_ascii_lowercase() {
        'a' => KeyCode::KeyA,
        'b' => KeyCode::KeyB,
        'c' => KeyCode::KeyC,
        'd' => KeyCode::KeyD,
        'e' => KeyCode::KeyE,
        'f' => KeyCode::KeyF,
        'g' => KeyCode::KeyG,
        'h' => KeyCode::KeyH,
        'i' => KeyCode::KeyI,
        'j' => KeyCode::KeyJ,
        'k' => KeyCode::KeyK,
        'l' => KeyCode::KeyL,
        'm' => KeyCode::KeyM,
        'n' => KeyCode::KeyN,
        'o' => KeyCode::KeyO,
        'p' => KeyCode::KeyP,
        'q' => KeyCode::KeyQ,
        'r' => KeyCode::KeyR,
        's' => KeyCode::KeyS,
        't' => KeyCode::KeyT,
        'u' => KeyCode::KeyU,
        'v' => KeyCode::KeyV,
        'w' => KeyCode::KeyW,
        'x' => KeyCode::KeyX,
        'y' => KeyCode::KeyY,
        'z' => KeyCode::KeyZ,
        '0' => KeyCode::Digit0,
        '1' => KeyCode::Digit1,
        '2' => KeyCode::Digit2,
        '3' => KeyCode::Digit3,
        '4' => KeyCode::Digit4,
        '5' => KeyCode::Digit5,
        '6' => KeyCode::Digit6,
        '7' => KeyCode::Digit7,
        '8' => KeyCode::Digit8,
        '9' => KeyCode::Digit9,
        '-' => KeyCode::Minus,
        '=' => KeyCode::Equal,
        '[' => KeyCode::BracketLeft,
        ']' => KeyCode::BracketRight,
        '\\' => KeyCode::Backslash,
        ';' => KeyCode::Semicolon,
        '\'' => KeyCode::Quote,
        ',' => KeyCode::Comma,
        '.' => KeyCode::Period,
        '/' => KeyCode::Slash,
        '`' => KeyCode::Backquote,
        _ => return None,
    };
    Some(virtual_key(key))
}

/// Carbon's `cmdKey`.
const CARBON_COMMAND: u32 = 0x0100;
/// Carbon's `shiftKey`.
const CARBON_SHIFT: u32 = 0x0200;
/// Carbon's `optionKey`.
const CARBON_OPTION: u32 = 0x0800;
/// Carbon's `controlKey`.
const CARBON_CONTROL: u32 = 0x1000;

/// The modifiers as `RegisterEventHotKey` takes them.
pub(crate) fn carbon_modifiers(mods: Modifiers) -> u32 {
    let mut mask = 0;
    if mods.meta {
        mask |= CARBON_COMMAND;
    }
    if mods.shift {
        mask |= CARBON_SHIFT;
    }
    if mods.alt {
        mask |= CARBON_OPTION;
    }
    if mods.ctrl {
        mask |= CARBON_CONTROL;
    }
    mask
}

/// `kCGEventFlagMaskShift`.
const CG_SHIFT: u64 = 0x2_0000;
/// `kCGEventFlagMaskControl`.
const CG_CONTROL: u64 = 0x4_0000;
/// `kCGEventFlagMaskAlternate` (Option).
const CG_OPTION: u64 = 0x8_0000;
/// `kCGEventFlagMaskCommand`.
const CG_COMMAND: u64 = 0x10_0000;

/// The four modifier flags a posted key event may carry. They are cleared
/// from the event's own flags before a chord's are set, so a key the user
/// still holds never leaks into the chord.
pub(crate) const CG_CHORD_FLAGS: u64 = CG_SHIFT | CG_CONTROL | CG_OPTION | CG_COMMAND;

/// The modifiers as a `CGEvent`'s flags.
pub(crate) fn cg_flags(mods: Modifiers) -> u64 {
    let mut flags = 0;
    if mods.shift {
        flags |= CG_SHIFT;
    }
    if mods.ctrl {
        flags |= CG_CONTROL;
    }
    if mods.alt {
        flags |= CG_OPTION;
    }
    if mods.meta {
        flags |= CG_COMMAND;
    }
    flags
}

/// The most UTF-16 units one typed key event carries.
pub(crate) const TEXT_CHUNK_UNITS: usize = 20;

/// One piece of a text to type, in order.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum TextPiece {
    /// UTF-16 units for one key event: 1 to [`TEXT_CHUNK_UNITS`] of them.
    Units(Vec<u16>),
    /// Press Return: a `\n`, a `\r\n` or a lone `\r`.
    Return,
    /// Press Tab.
    Tab,
}

/// Prints how many units, never the units: they are what the user typed.
impl std::fmt::Debug for TextPiece {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Units(units) => write!(f, "Units({})", units.len()),
            Self::Return => f.write_str("Return"),
            Self::Tab => f.write_str("Tab"),
        }
    }
}

/// A zero-width joiner: it glues the characters on both sides of it into
/// one (a family emoji is people joined this way).
const ZERO_WIDTH_JOINER: char = '\u{200D}';

/// `true` when `ch` changes the character before it: a combining mark
/// ([`is_combining_mark`]), a variation selector, a zero-width joiner, an
/// emoji's skin tone (U+1F3FB to U+1F3FF) or a tag character (the letters
/// of a flag such as Scotland's).
fn joins_previous(ch: char) -> bool {
    matches!(
        ch,
        ZERO_WIDTH_JOINER
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{E0100}'..='\u{E01EF}'
            | '\u{1F3FB}'..='\u{1F3FF}'
            | '\u{E0020}'..='\u{E007F}'
    ) || is_combining_mark(ch)
}

/// `true` for the combining marks (Unicode's categories Mn, Mc and Me) of
/// the general blocks (U+0300, U+1AB0, U+1DC0, U+20D0 with the keycap mark
/// U+20E3, U+FE20) and of Cyrillic, Hebrew, Arabic, the Indic scripts from
/// Devanagari to Sinhala, Thai, Lao and kana. Only the marks: a letter
/// listed here could start no chunk. A range may run over code points that
/// name no character. Other scripts' marks are not here.
fn is_combining_mark(ch: char) -> bool {
    matches!(
        u32::from(ch),
        0x0300..=0x036F | 0x0483..=0x0489
            // Hebrew, Arabic.
            | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7
            | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC
            | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
            // Devanagari, Bengali, Gurmukhi, Gujarati, Oriya.
            | 0x0900..=0x0903 | 0x093A..=0x093C | 0x093E..=0x094F | 0x0951..=0x0957
            | 0x0962..=0x0963 | 0x0981..=0x0983 | 0x09BC | 0x09BE..=0x09CD | 0x09D7
            | 0x09E2..=0x09E3 | 0x09FE..=0x0A03 | 0x0A3C..=0x0A51 | 0x0A70..=0x0A71
            | 0x0A75 | 0x0A81..=0x0A83 | 0x0ABC | 0x0ABE..=0x0ACD | 0x0AE2..=0x0AE3
            | 0x0AFA..=0x0B03 | 0x0B3C | 0x0B3E..=0x0B57 | 0x0B62..=0x0B63
            // Tamil, Telugu, Kannada, Malayalam, Sinhala.
            | 0x0B82 | 0x0BBE..=0x0BCD | 0x0BD7 | 0x0C00..=0x0C04 | 0x0C3C
            | 0x0C3E..=0x0C56 | 0x0C62..=0x0C63 | 0x0C81..=0x0C83 | 0x0CBC
            | 0x0CBE..=0x0CD6 | 0x0CE2..=0x0CE3 | 0x0CF3..=0x0D03 | 0x0D3B..=0x0D3C
            | 0x0D3E..=0x0D4D | 0x0D57 | 0x0D62..=0x0D63 | 0x0D81..=0x0D83
            | 0x0DCA..=0x0DDF | 0x0DF2..=0x0DF3
            // Thai and Lao, with the vowel that is written as a mark.
            | 0x0E31 | 0x0E33..=0x0E3A | 0x0E47..=0x0E4E
            | 0x0EB1 | 0x0EB3..=0x0EBC | 0x0EC8..=0x0ECE
            | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF
            | 0x3099..=0x309A | 0xFE20..=0xFE2F
    )
}

/// `true` for a regional indicator: two of them in a row are one flag.
fn is_regional_indicator(ch: char) -> bool {
    matches!(ch, '\u{1F1E6}'..='\u{1F1FF}')
}

/// `text` as the pieces `type_text` sends, in order. Line ends and tabs are
/// keys of their own. The rest goes in chunks of at most
/// [`TEXT_CHUNK_UNITS`] UTF-16 units that never end inside a surrogate pair,
/// nor just before something that joins the character before it
/// ([`joins_previous`]), nor just after a joiner, nor between the two
/// regional indicators of a flag: an app that takes each event as one
/// insertion would otherwise show the halves apart. Only a run of such
/// characters longer than a chunk is cut, and then between two characters.
pub(crate) fn text_chunks(text: &str) -> Vec<TextPiece> {
    let mut pieces = Vec::new();
    let mut run: Vec<char> = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let key = match ch {
            '\r' => {
                // `\r\n` is one line end.
                chars.next_if_eq(&'\n');
                TextPiece::Return
            }
            '\n' => TextPiece::Return,
            '\t' => TextPiece::Tab,
            _ => {
                run.push(ch);
                continue;
            }
        };
        chunk_run(&run, &mut pieces);
        run.clear();
        pieces.push(key);
    }
    chunk_run(&run, &mut pieces);
    pieces
}

/// Cuts a run with no line end or tab in it into [`TextPiece::Units`].
fn chunk_run(run: &[char], pieces: &mut Vec<TextPiece>) {
    let mut rest = run;
    while !rest.is_empty() {
        let mut units = 0;
        // How many characters fit, and how many fit without a cut that
        // would part a character from what joins it.
        let mut fits = 0;
        let mut clean = None;
        // Regional indicators in a row, up to and with `ch`.
        let mut indicators = 0_usize;
        for (index, ch) in rest.iter().enumerate() {
            if units + ch.len_utf16() > TEXT_CHUNK_UNITS {
                break;
            }
            units += ch.len_utf16();
            fits = index + 1;
            indicators = if is_regional_indicator(*ch) {
                indicators + 1
            } else {
                0
            };
            let parts_a_character = rest.get(fits).is_some_and(|next| {
                joins_previous(*next)
                    || *ch == ZERO_WIDTH_JOINER
                    // An odd one is the first half of a flag.
                    || (indicators % 2 == 1 && is_regional_indicator(*next))
            });
            if !parts_a_character {
                clean = Some(fits);
            }
        }
        // One character is two units at most, so at least one always fits.
        let (chunk, after) = rest.split_at(clean.unwrap_or(fits));
        let mut encoded = Vec::with_capacity(TEXT_CHUNK_UNITS);
        let mut pair = [0_u16; 2];
        for ch in chunk {
            encoded.extend_from_slice(ch.encode_utf16(&mut pair));
        }
        pieces.push(TextPiece::Units(encoded));
        rest = after;
    }
}

/// `osascript`: runs AppleScript.
const OSASCRIPT: &str = "/usr/bin/osascript";
/// `open`: asks Launch Services to open an app, a file, a folder or a URL.
const OPEN: &str = "/usr/bin/open";

/// The Keyboard Maestro engine runs the macro named by the script's first
/// argument. The name is never part of the script.
const KEYBOARD_MAESTRO_LINE: &str =
    "tell application \"Keyboard Maestro Engine\" to do script (item 1 of argv)";

/// Mutes the sound output, or brings it back. Asks for no permission.
const MUTE_SCRIPT: &str = "set volume output muted (not (output muted of (get volume settings)))";

/// Turns the sound up by one of the volume keys' sixteen steps.
const VOLUME_UP_SCRIPT: &str = "set level to (output volume of (get volume settings)) + 6\n\
     if level > 100 then set level to 100\n\
     set volume output volume level";

/// Turns the sound down by the same step.
const VOLUME_DOWN_SCRIPT: &str = "set level to (output volume of (get volume settings)) - 6\n\
     if level < 0 then set level to 0\n\
     set volume output volume level";

/// Switches between the light and the dark appearance. macOS asks once
/// whether Grape may control System Events.
const DARK_MODE_SCRIPT: &str = "tell application \"System Events\" to tell appearance preferences \
     to set dark mode to not dark mode";

/// The Accessibility list in System Settings.
const INPUT_ACCESS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// A program with these arguments, nothing on its standard input and no
/// script file.
fn program<const N: usize>(path: &str, args: [&OsStr; N]) -> Program {
    Program {
        program: PathBuf::from(path),
        args: args.into_iter().map(OsStr::to_owned).collect(),
        stdin: None,
        script: None,
    }
}

/// `osascript -` with `source` on its standard input: a script never shows
/// in the process list. `osascript` reads all of it before it runs any of
/// it, so nothing the script starts can read the script.
fn applescript(source: &str) -> Program {
    Program {
        stdin: Some(source.to_owned()),
        ..program(OSASCRIPT, ["-".as_ref()])
    }
}

/// The program that does `request`. Each word the user typed is one
/// argument, or goes to standard input or into a script file; nothing is
/// spliced into a script.
///
/// - AppleScript: `osascript -`, the source on standard input.
/// - Shell: `zsh -l <file>`, the source in a private file the runner writes
///   ([`Program::script`]). A login shell, so the user's own `PATH`. Never
///   `zsh -s` with the source on standard input: a shell reads that a
///   command at a time, and the commands inherit it, so one that reads its
///   input (`cat`, `read`, `ssh`, `ffmpeg`) would swallow the rest of the
///   script. This way they read `/dev/null`.
/// - Apple Shortcut: `shortcuts run <name>`.
/// - Keyboard Maestro: an `osascript` whose script reads the macro's name
///   from its arguments.
/// - Speak: `say <text>`. A text starting with `-` gets a space in front,
///   so `say` doesn't read it as an option.
pub(crate) fn program_for(request: ProgramRequest<'_>) -> Program {
    match request {
        ProgramRequest::AppleScript(source) => applescript(source),
        ProgramRequest::Shell(source) => Program {
            script: Some(source.to_owned()),
            ..program("/bin/zsh", ["-l".as_ref()])
        },
        ProgramRequest::AppleShortcut(name) => {
            program("/usr/bin/shortcuts", ["run".as_ref(), name.as_ref()])
        }
        ProgramRequest::KeyboardMaestro(name) => program(
            OSASCRIPT,
            [
                "-e".as_ref(),
                "on run argv".as_ref(),
                "-e".as_ref(),
                KEYBOARD_MAESTRO_LINE.as_ref(),
                "-e".as_ref(),
                "end run".as_ref(),
                "--".as_ref(),
                name.as_ref(),
            ],
        ),
        ProgramRequest::Speak(text) => {
            if text.starts_with('-') {
                let spaced = format!(" {text}");
                program("/usr/bin/say", [spaced.as_ref()])
            } else {
                program("/usr/bin/say", [text.as_ref()])
            }
        }
    }
}

/// The programs that open `target`, to try in order until one succeeds:
/// never none. An app with an id gets `open -b <id>` and then
/// `open -a <path>`; anything else is one `open`. Each path, id and URL is
/// one argument.
pub(crate) fn open_programs(target: OpenTarget<'_>) -> Vec<Program> {
    match target {
        OpenTarget::App { path, id } => {
            let by_path = program(OPEN, ["-a".as_ref(), path.as_os_str()]);
            match id {
                Some(id) => vec![program(OPEN, ["-b".as_ref(), id.as_ref()]), by_path],
                None => vec![by_path],
            }
        }
        OpenTarget::Path(path) => vec![program(OPEN, [path.as_os_str()])],
        OpenTarget::Url(url) => vec![program(OPEN, [url.as_ref()])],
    }
}

/// How macOS does `control`: a program, or the system's own chord by the
/// character its key types (Lock screen is Control+Command+`q`, Screenshot
/// area is Command+Shift+`4`).
pub(crate) fn system_plan(control: SystemControl) -> SystemPlan {
    let program = match control {
        SystemControl::SleepDisplay => program("/usr/bin/pmset", ["displaysleepnow".as_ref()]),
        SystemControl::LockScreen => {
            return SystemPlan::CharChord {
                ch: 'q',
                mods: Modifiers {
                    ctrl: true,
                    meta: true,
                    ..Modifiers::NONE
                },
            }
        }
        SystemControl::Screensaver => program(OPEN, ["-a".as_ref(), "ScreenSaverEngine".as_ref()]),
        SystemControl::MissionControl => program(OPEN, ["-a".as_ref(), "Mission Control".as_ref()]),
        SystemControl::ToggleMute => applescript(MUTE_SCRIPT),
        SystemControl::VolumeUp => applescript(VOLUME_UP_SCRIPT),
        SystemControl::VolumeDown => applescript(VOLUME_DOWN_SCRIPT),
        SystemControl::ToggleDarkMode => applescript(DARK_MODE_SCRIPT),
        SystemControl::ScreenshotArea => {
            return SystemPlan::CharChord {
                ch: '4',
                mods: Modifiers {
                    shift: true,
                    meta: true,
                    ..Modifiers::NONE
                },
            }
        }
    };
    SystemPlan::Program(program)
}

/// `open <url>` for the Accessibility list in System Settings, where the
/// user gives Grape input access.
pub(crate) fn input_access_settings_program() -> Program {
    program(OPEN, [INPUT_ACCESS_URL.as_ref()])
}

/// Whether a hot key going down is passed on to the app. An Open press
/// while one of Grape's modal windows is up (a picker, Move's panel or
/// alert) is not: the wheel would cover the panel, and binding its Esc
/// would wait for the main queue, which serves nothing until the panel
/// closes. Esc and Return always pass: they close a wheel that is open.
pub(crate) fn passes_press(id: HotkeyId, modal_up: bool) -> bool {
    !(modal_up && id == HotkeyId::Open)
}

/// Who got to a hop to the main thread first: the main thread, which then
/// does the work, or the caller, whose patience ran out. Each side calls
/// [`take`](Self::take) once.
#[derive(Debug, Default)]
pub(crate) struct HopClaim(AtomicBool);

impl HopClaim {
    /// `true` for the first to ask, and only for it. The main thread does
    /// the work only when it gets `true`. A caller that gets `true` when it
    /// gives up knows the work will never run; with `false` the work has
    /// started, so its answer is coming.
    pub(crate) fn take(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }
}

/// The four-character code on every hot key Grape registers.
pub(crate) const HOTKEY_SIGNATURE: u32 = u32::from_be_bytes(*b"GRPE");

/// The bits of a hot key's event id that hold its serial number.
const HOTKEY_SERIAL_MASK: u32 = 0x00FF_FFFF;

/// The `EventHotKeyID.id` of one registration: what the hot key is for in
/// the top byte and a serial number below it, so each registration has an
/// id of its own. The serial wraps after 16 million registrations, which
/// only matters to a log reader.
pub(crate) fn hotkey_event_id(id: HotkeyId, serial: u32) -> u32 {
    let kind: u32 = match id {
        HotkeyId::Open => 1,
        HotkeyId::Escape => 2,
        HotkeyId::Confirm => 3,
    };
    (kind << 24) | (serial & HOTKEY_SERIAL_MASK)
}

/// What the hot key with this event id is for. `None` for an id Grape
/// never gave out.
pub(crate) fn hotkey_for_event_id(event_id: u32) -> Option<HotkeyId> {
    match event_id >> 24 {
        1 => Some(HotkeyId::Open),
        2 => Some(HotkeyId::Escape),
        3 => Some(HotkeyId::Confirm),
        _ => None,
    }
}

/// One chord the OS has registered for Grape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeldChord {
    /// What it fires.
    pub(crate) id: HotkeyId,
    /// The chord.
    pub(crate) chord: Chord,
    /// Grape let go of it but the OS refused the unregister, so the OS
    /// still has it.
    pub(crate) stuck: bool,
}

/// What a bind does to the chords the OS holds for Grape.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct BindPlan {
    /// The chords to register, in the order they were asked for.
    pub(crate) register: Vec<Chord>,
    /// Places in the held list to unregister, once every new chord is
    /// registered.
    pub(crate) release: Vec<usize>,
    /// Places in the held list of stuck chords that are wanted again: they
    /// are still registered, so they are kept as they are.
    pub(crate) adopt: Vec<usize>,
}

/// How to make `id` fire for exactly `chords`, given what the OS `held`
/// for Grape before. A chord asked for twice counts once. A chord another
/// id holds is left to that id: one chord fires one thing, and registering
/// it a second time would only be refused. Chords `id` already has stay
/// registered, so a bind that changes nothing touches nothing.
pub(crate) fn bind_plan(held: &[HeldChord], id: HotkeyId, chords: &[Chord]) -> BindPlan {
    let mut wanted: Vec<Chord> = Vec::new();
    for chord in chords {
        let taken = held
            .iter()
            .any(|entry| entry.id != id && entry.chord == *chord);
        if !taken && !wanted.contains(chord) {
            wanted.push(*chord);
        }
    }
    let mut plan = BindPlan::default();
    for (place, entry) in held.iter().enumerate() {
        if entry.id != id {
            continue;
        }
        match (wanted.contains(&entry.chord), entry.stuck) {
            (true, true) => plan.adopt.push(place),
            (false, false) => plan.release.push(place),
            // Kept as it is, or already let go of.
            (true, false) | (false, true) => {}
        }
    }
    plan.register = wanted
        .into_iter()
        .filter(|chord| {
            !held
                .iter()
                .any(|entry| entry.id == id && entry.chord == *chord)
        })
        .collect();
    plan
}

/// What the pointer watch remembers between two polls, and what each poll
/// has to say.
#[derive(Debug, Default)]
pub(crate) struct PointerEdges {
    /// Where the pointer was and whether the primary button was down at
    /// the last poll since the watch was turned on.
    seen: Option<((i32, i32), bool)>,
}

impl PointerEdges {
    /// Forgets the last poll: the watch was turned off, or on again.
    pub(crate) fn restart(&mut self) {
        self.seen = None;
    }

    /// One poll while the watch is on: the pointer is `at` and the primary
    /// button is `down`. The first poll says where the pointer is and
    /// nothing about the button, so a button already down sends no `Down`.
    /// Later polls say `Moved` when the position changed, then `Down` or
    /// `Up` when the button did.
    pub(crate) fn poll(&mut self, at: (i32, i32), down: bool) -> Vec<PointerSignal> {
        let (x, y) = at;
        let mut signals = Vec::new();
        match self.seen {
            None => signals.push(PointerSignal::Moved { x, y }),
            Some((last, was_down)) => {
                if last != at {
                    signals.push(PointerSignal::Moved { x, y });
                }
                if down && !was_down {
                    signals.push(PointerSignal::Down { x, y });
                } else if was_down && !down {
                    signals.push(PointerSignal::Up { x, y });
                }
            }
        }
        self.seen = Some((at, down));
        signals
    }
}

/// `true` for the number pad's virtual keys (`kVK_ANSI_Keypad…` and the
/// JIS pad's comma).
fn is_keypad_key(key: u16) -> bool {
    matches!(
        key,
        65 | 67 | 69 | 71 | 75 | 76 | 78 | 81..=89 | 91 | 92 | 95
    )
}

/// The modifier state `UCKeyTranslate` takes for a key pressed with
/// Command or with nothing: Carbon's mask shifted down a byte.
pub(crate) fn layout_modifier_state(command: bool) -> u32 {
    if command {
        CARBON_COMMAND >> 8
    } else {
        0
    }
}

/// The virtual key a character chord is posted from. `typed(key, command)`
/// is the one character `key` types on the current layout, with Command
/// held or with nothing, if it types exactly one.
///
/// A chord with Command looks first at what the keys type with Command
/// held, which is what an app compares a shortcut with: a layout such as
/// Dvorak-QWERTY or Russian types Latin QWERTY letters then. Next comes
/// what the keys type alone, and last the US table, for a layout that
/// can't be read or has no such key. The number pad is never used: on a
/// layout whose digits need Shift, Command+Shift+`4` would otherwise come
/// from the pad, which isn't the system's chord.
pub(crate) fn char_key(
    ch: char,
    command: bool,
    typed: impl Fn(u16, bool) -> Option<char>,
) -> Option<u16> {
    let wanted = ch.to_ascii_lowercase();
    let passes: &[bool] = if command { &[true, false] } else { &[false] };
    for &with_command in passes {
        let found = (0_u16..128)
            .filter(|key| !is_keypad_key(*key))
            .find(|key| typed(*key, with_command).is_some_and(|got| got == ch || got == wanted));
        if found.is_some() {
            return found;
        }
    }
    us_key_for_char(ch)
}

/// `localtime_r`'s fields as a [`LocalTime`]: `tm_year` counts from 1900
/// and `tm_mon` from 0. `None` when a field is out of its range.
pub(crate) fn local_time(
    tm_year: i32,
    tm_mon: i32,
    tm_mday: i32,
    tm_hour: i32,
    tm_min: i32,
) -> Option<LocalTime> {
    let field = |value: i32, least: i32, most: i32| {
        u8::try_from(value)
            .ok()
            .filter(|_| (least..=most).contains(&value))
    };
    Some(LocalTime {
        year: tm_year.checked_add(1900)?,
        month: field(tm_mon.checked_add(1)?, 1, 12)?,
        day: field(tm_mday, 1, 31)?,
        hour: field(tm_hour, 0, 23)?,
        minute: field(tm_min, 0, 59)?,
    })
}

/// The process group [`crate::shortcuts::kill_group`] may signal for the
/// program `pid`: never group 0 (the caller's own), never group 1
/// (launchd's), and never Grape's own group, which a program that wasn't
/// given a group of its own would still be in.
pub(crate) fn killable_group(pid: u32, own_group: i32) -> Option<i32> {
    let group = i32::try_from(pid).ok()?;
    (group > 1 && group != own_group).then_some(group)
}

/// A window's frame as Accessibility gives it (top-left points already),
/// rounded to whole points.
pub(crate) fn ax_frame(x: f64, y: f64, width: f64, height: f64) -> Rect {
    Rect {
        x: round(x),
        y: round(y),
        width: round(width),
        height: round(height),
    }
}

/// The middle of `rect`, for finding the display a window is on.
pub(crate) fn center(rect: Rect) -> (i32, i32) {
    (
        rect.x.saturating_add(rect.width / 2),
        rect.y.saturating_add(rect.height / 2),
    )
}

/// The name a picked app shows, from Finder's display name for it. Finder
/// adds `.app` when the user asked to see every extension; the name goes
/// without it. An empty display name falls back to the bundle's file name.
pub(crate) fn app_name(display: &str, path: &Path) -> String {
    fn without_app(name: &str) -> &str {
        let cut = name.len().saturating_sub(4);
        match name.get(cut..) {
            Some(end) if end.eq_ignore_ascii_case(".app") => &name[..cut],
            _ => name,
        }
    }
    let shown = without_app(display.trim()).trim();
    if !shown.is_empty() {
        return shown.to_owned();
    }
    let file = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    without_app(&file).to_owned()
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

    #[test]
    fn the_app_last_activated_comes_first_once() {
        const GRAPE: i32 = 500;
        let mut recent = Vec::new();
        // Finder, then Mail: Mail is where the user came from.
        note_activation(&mut recent, 101, GRAPE);
        note_activation(&mut recent, 202, GRAPE);
        assert_eq!(recent, [202, 101]);
        // A click on the shelf activates Grape: still Mail first.
        note_activation(&mut recent, GRAPE, GRAPE);
        assert_eq!(recent, [202, 101]);
        // Back to Finder: it moves to the front, and is listed once.
        note_activation(&mut recent, 101, GRAPE);
        assert_eq!(recent, [101, 202]);
        // No process: never listed.
        note_activation(&mut recent, 0, GRAPE);
        note_activation(&mut recent, -1, GRAPE);
        assert_eq!(recent, [101, 202]);
    }

    #[test]
    fn the_oldest_app_goes_once_the_list_is_full() {
        let mut recent = Vec::new();
        for pid in 1..=20 {
            note_activation(&mut recent, pid, 500);
        }
        assert_eq!(recent.len(), RECENT_APPS);
        assert_eq!(recent[0], 20);
        assert_eq!(recent[RECENT_APPS - 1], 20 - RECENT_APPS as i32 + 1);
    }

    #[test]
    fn relaunch_calls_stay_paired() {
        // A counter that starts at zero, as AppKit's does.
        let mut counter = 0_i32;
        let mut disabled = false;
        for relaunch in [false, false, true, true, false, true, true, false] {
            match relaunch_call(disabled, relaunch) {
                Some(RelaunchCall::Disable) => {
                    counter += 1;
                    disabled = true;
                }
                Some(RelaunchCall::Enable) => {
                    counter -= 1;
                    disabled = false;
                }
                None => {}
            }
            assert!((0..=1).contains(&counter), "{counter}");
            // AppKit relaunches at zero only.
            assert_eq!(counter == 0, relaunch);
        }
        assert_eq!(relaunch_call(false, true), None);
        assert_eq!(relaunch_call(true, false), None);
    }
    /// The US ANSI `kVK_*` codes, written out a second time by W3C code.
    const KEY_TABLE: &str = "
        KeyA=0 KeyS=1 KeyD=2 KeyF=3 KeyH=4 KeyG=5 KeyZ=6 KeyX=7 KeyC=8 KeyV=9 KeyB=11
        KeyQ=12 KeyW=13 KeyE=14 KeyR=15 KeyY=16 KeyT=17 Digit1=18 Digit2=19 Digit3=20
        Digit4=21 Digit6=22 Digit5=23 Equal=24 Digit9=25 Digit7=26 Minus=27 Digit8=28
        Digit0=29 BracketRight=30 KeyO=31 KeyU=32 BracketLeft=33 KeyI=34 KeyP=35 Enter=36
        KeyL=37 KeyJ=38 Quote=39 KeyK=40 Semicolon=41 Backslash=42 Comma=43 Slash=44
        KeyN=45 KeyM=46 Period=47 Tab=48 Space=49 Backquote=50 Backspace=51 Escape=53
        F17=64 F18=79 F19=80 F20=90 F5=96 F6=97 F7=98 F3=99 F8=100 F9=101 F11=103 F13=105
        F16=106 F14=107 F10=109 F12=111 F15=113 Home=115 PageUp=116 Delete=117 F4=118
        End=119 F2=120 PageDown=121 F1=122 ArrowLeft=123 ArrowRight=124 ArrowDown=125
        ArrowUp=126";

    fn held(shift: bool, ctrl: bool, alt: bool, meta: bool) -> Modifiers {
        Modifiers {
            shift,
            ctrl,
            alt,
            meta,
        }
    }

    #[test]
    fn every_key_has_its_own_virtual_key() {
        let mut seen = std::collections::HashSet::new();
        for &key in KeyCode::ALL {
            let code = virtual_key(key);
            assert!(code < 128, "{}", key.id());
            assert!(seen.insert(code), "{} shares code {code}", key.id());
        }
        assert_eq!(seen.len(), KeyCode::ALL.len());
    }

    #[test]
    fn the_virtual_keys_are_the_us_ansi_table() {
        let mut checked = 0;
        for pair in KEY_TABLE.split_whitespace() {
            let (id, code) = pair.split_once('=').unwrap();
            let key = KeyCode::from_id(id).unwrap();
            assert_eq!(virtual_key(key), code.parse::<u16>().unwrap(), "{id}");
            checked += 1;
        }
        assert_eq!(checked, KeyCode::ALL.len());
        // The ones the wheel itself binds and presses.
        assert_eq!(virtual_key(KeyCode::Escape), 53);
        assert_eq!(virtual_key(KeyCode::Enter), 36);
        assert_eq!(virtual_key(KeyCode::Tab), 48);
        assert_eq!(virtual_key(KeyCode::KeyG), 5);
        assert_eq!(virtual_key(KeyCode::KeyA), 0);
    }

    #[test]
    fn a_character_finds_its_us_key_in_either_case() {
        assert_eq!(us_key_for_char('c'), Some(8));
        assert_eq!(us_key_for_char('C'), Some(8));
        assert_eq!(us_key_for_char('v'), Some(9));
        assert_eq!(us_key_for_char('q'), Some(12));
        assert_eq!(us_key_for_char('4'), Some(21));
        assert_eq!(us_key_for_char('`'), Some(50));
        assert_eq!(us_key_for_char('\\'), Some(42));
        for (letter, key) in ('a'..='z').zip(KeyCode::ALL) {
            assert_eq!(us_key_for_char(letter), Some(virtual_key(*key)));
            assert_eq!(
                us_key_for_char(letter.to_ascii_uppercase()),
                Some(virtual_key(*key))
            );
        }
        for digit in '0'..='9' {
            let key = KeyCode::from_id(&format!("Digit{digit}")).unwrap();
            assert_eq!(us_key_for_char(digit), Some(virtual_key(key)));
        }
        // Each punctuation key is found by the sign on it.
        let signs = r"-=[]\;',./`";
        for sign in signs.chars() {
            let key = KeyCode::ALL
                .iter()
                .find(|key| key.label() == sign.to_string())
                .unwrap();
            assert_eq!(us_key_for_char(sign), Some(virtual_key(*key)), "{sign}");
        }
        assert_eq!(signs.chars().count(), 11);
    }

    #[test]
    fn a_character_the_us_layout_types_with_shift_or_not_at_all_has_no_key() {
        for ch in [
            ' ', '\n', '\t', '!', '@', '_', '+', '{', '"', '?', '~', 'é', 'ß', '字', '↑', '😀',
        ] {
            assert_eq!(us_key_for_char(ch), None, "{ch:?}");
        }
    }

    #[test]
    fn carbon_and_cg_masks_are_the_systems_own() {
        assert_eq!(carbon_modifiers(Modifiers::NONE), 0);
        assert_eq!(carbon_modifiers(Modifiers::META), 0x0100);
        assert_eq!(carbon_modifiers(Modifiers::SHIFT), 0x0200);
        assert_eq!(carbon_modifiers(Modifiers::ALT), 0x0800);
        assert_eq!(carbon_modifiers(Modifiers::CTRL), 0x1000);
        // The default hot key: Control+Option.
        assert_eq!(carbon_modifiers(held(false, true, true, false)), 0x1800);
        assert_eq!(carbon_modifiers(held(true, true, true, true)), 0x1B00);

        assert_eq!(cg_flags(Modifiers::NONE), 0);
        assert_eq!(cg_flags(Modifiers::SHIFT), 0x2_0000);
        assert_eq!(cg_flags(Modifiers::CTRL), 0x4_0000);
        assert_eq!(cg_flags(Modifiers::ALT), 0x8_0000);
        assert_eq!(cg_flags(Modifiers::META), 0x10_0000);
        assert_eq!(cg_flags(held(true, false, false, true)), 0x12_0000);
        assert_eq!(cg_flags(held(true, true, true, true)), CG_CHORD_FLAGS);
        assert_eq!(CG_CHORD_FLAGS, 0x1E_0000);
    }

    #[test]
    fn each_set_of_modifiers_has_its_own_mask() {
        let mut carbon = std::collections::HashSet::new();
        let mut cg = std::collections::HashSet::new();
        for bits in 0_u8..16 {
            let mods = held(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
            assert!(carbon.insert(carbon_modifiers(mods)));
            assert!(cg.insert(cg_flags(mods)));
            assert_eq!(cg_flags(mods) & !CG_CHORD_FLAGS, 0);
            assert_eq!(carbon_modifiers(mods).count_ones() as usize, mods.count());
            assert_eq!(cg_flags(mods).count_ones() as usize, mods.count());
        }
    }

    fn units(text: &str) -> TextPiece {
        TextPiece::Units(text.encode_utf16().collect())
    }

    /// What the pieces type, with every line end as `\n`.
    fn typed(pieces: &[TextPiece]) -> String {
        let mut all = Vec::new();
        for piece in pieces {
            match piece {
                TextPiece::Units(units) => all.extend_from_slice(units),
                TextPiece::Return => all.push(u16::from(b'\n')),
                TextPiece::Tab => all.push(u16::from(b'\t')),
            }
        }
        String::from_utf16(&all).unwrap()
    }

    /// Fails unless every chunk is whole text of the right length that
    /// parts no character from what joins it.
    fn assert_whole_chunks(pieces: &[TextPiece]) {
        for piece in pieces {
            let TextPiece::Units(chunk) = piece else {
                continue;
            };
            assert!((1..=TEXT_CHUNK_UNITS).contains(&chunk.len()));
            // A chunk that is valid UTF-16 on its own splits no pair.
            let text = String::from_utf16(chunk).unwrap();
            assert!(!text.contains(['\n', '\r', '\t']));
            let first = text.chars().next().unwrap();
            let last = text.chars().next_back().unwrap();
            assert!(!joins_previous(first), "a chunk starts with a joining mark");
            assert_ne!(last, ZERO_WIDTH_JOINER, "a chunk ends with a joiner");
        }
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(text_chunks(""), vec![]);
        assert_eq!(text_chunks("hello"), vec![units("hello")]);
        let twenty = "abcdefghijklmnopqrst";
        assert_eq!(text_chunks(twenty), vec![units(twenty)]);
        assert_eq!(
            text_chunks("abcdefghijklmnopqrstu"),
            vec![units(twenty), units("u")]
        );
    }

    #[test]
    fn line_ends_and_tabs_are_keys_of_their_own() {
        assert_eq!(
            text_chunks("a\nb\tc"),
            vec![
                units("a"),
                TextPiece::Return,
                units("b"),
                TextPiece::Tab,
                units("c")
            ]
        );
        // `\r\n` and a lone `\r` are one Return each.
        assert_eq!(
            text_chunks("a\r\nb\rc\n\r\n\n\r"),
            vec![
                units("a"),
                TextPiece::Return,
                units("b"),
                TextPiece::Return,
                units("c"),
                TextPiece::Return,
                TextPiece::Return,
                TextPiece::Return,
                TextPiece::Return,
            ]
        );
        assert_eq!(text_chunks("\n"), vec![TextPiece::Return]);
        assert_eq!(text_chunks("\t\t"), vec![TextPiece::Tab, TextPiece::Tab]);
        // A long line still chunks on each side of the key.
        let pieces = text_chunks("abcdefghijklmnopqrstuvwxy\tz");
        assert_eq!(
            pieces,
            vec![
                units("abcdefghijklmnopqrst"),
                units("uvwxy"),
                TextPiece::Tab,
                units("z")
            ]
        );
    }

    #[test]
    fn a_chunk_never_ends_inside_a_surrogate_pair() {
        // Nineteen letters, then an emoji that would straddle the limit.
        let text = "abcdefghijklmnopqrs😀!";
        assert_eq!(
            text_chunks(text),
            vec![units("abcdefghijklmnopqrs"), units("😀!")]
        );
        // Ten emoji fill a chunk exactly; the eleventh starts the next.
        let emoji = "😀".repeat(11);
        assert_eq!(
            text_chunks(&emoji),
            vec![units(&"😀".repeat(10)), units("😀")]
        );
        for lead in 0..TEXT_CHUNK_UNITS + 2 {
            let text = format!("{}{}", "x".repeat(lead), "🎉🇫🇷🇯🇵👍🏽".repeat(4));
            let pieces = text_chunks(&text);
            assert_whole_chunks(&pieces);
            assert_eq!(typed(&pieces), text);
        }
    }

    #[test]
    fn a_chunk_never_parts_a_character_from_its_marks() {
        // The accent would be the twenty-first unit: the `e` goes with it.
        let text = "abcdefghijklmnopqrse\u{0301}!";
        assert_eq!(
            text_chunks(text),
            vec![units("abcdefghijklmnopqrs"), units("e\u{0301}!")]
        );
        // A heart with its variation selector.
        let text = "abcdefghijklmnopqrs\u{2764}\u{FE0F}";
        assert_eq!(
            text_chunks(text),
            vec![units("abcdefghijklmnopqrs"), units("\u{2764}\u{FE0F}")]
        );
        // Two marks on one letter stay with it.
        let text = "abcdefghijklmnopqro\u{0302}\u{0303}";
        assert_eq!(
            text_chunks(text),
            vec![units("abcdefghijklmnopqr"), units("o\u{0302}\u{0303}")]
        );
    }

    /// `cluster` right after as many letters as put its first character
    /// at a chunk's very end.
    fn at_the_edge(cluster: &str) -> (String, String) {
        let first = cluster.chars().next().unwrap().len_utf16();
        let lead = "x".repeat(TEXT_CHUNK_UNITS - first);
        (format!("{lead}{cluster}"), lead)
    }

    #[test]
    fn a_chunk_never_parts_an_emoji_from_its_skin_tone_keycap_or_tags() {
        // Each would fit up to its first character; the whole goes on.
        for cluster in [
            // A thumb and an astronaut with a skin tone.
            "\u{1F44D}\u{1F3FD}",
            "\u{1F469}\u{1F3FD}\u{200D}\u{1F680}",
            // The keycap 1: a digit, a variation selector, the keycap mark.
            "1\u{FE0F}\u{20E3}",
            // Scotland's flag: a black flag and six tag characters.
            "\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}",
            // An ideograph with a variation selector of the second plane.
            "\u{845B}\u{E0100}",
        ] {
            let (text, lead) = at_the_edge(cluster);
            assert_eq!(
                text_chunks(&text),
                vec![units(&lead), units(cluster)],
                "{}",
                cluster.escape_unicode()
            );
        }
        // The keycap mark alone would be the twenty-first unit.
        let keycap = "1\u{FE0F}\u{20E3}";
        let lead = "x".repeat(TEXT_CHUNK_UNITS - 2);
        assert_eq!(
            text_chunks(&format!("{lead}{keycap}")),
            vec![units(&lead), units(keycap)]
        );
    }

    #[test]
    fn a_chunk_never_parts_a_letter_from_its_marks_in_other_scripts() {
        for cluster in [
            // Devanagari ki, and ka with a nukta and a vowel sign.
            "\u{0915}\u{093F}",
            "\u{0915}\u{093C}\u{0940}",
            // Bengali, Tamil and Malayalam vowel signs.
            "\u{0995}\u{09BF}",
            "\u{0B95}\u{0BCA}",
            "\u{0D15}\u{0D4A}",
            // Thai: a vowel above and a tone mark; then sara am.
            "\u{0E01}\u{0E35}\u{0E49}",
            "\u{0E19}\u{0E49}\u{0E33}",
            // Lao.
            "\u{0E81}\u{0EB5}\u{0EC9}",
            // Hebrew bet with a sheva and a dagesh; Arabic beh with a fatha.
            "\u{05D1}\u{05B0}\u{05BC}",
            "\u{0628}\u{064E}",
            // Cyrillic with a titlo; kana with a combining voiced mark.
            "\u{0430}\u{0483}",
            "\u{304B}\u{3099}",
            // The general blocks past U+036F.
            "a\u{1AB0}",
            "a\u{1DC4}",
            "a\u{20D7}",
            "a\u{FE20}",
        ] {
            let (text, lead) = at_the_edge(cluster);
            assert_eq!(
                text_chunks(&text),
                vec![units(&lead), units(cluster)],
                "{}",
                cluster.escape_unicode()
            );
        }
    }

    #[test]
    fn a_flag_is_never_cut_in_two() {
        let (norway, sweden) = ("\u{1F1F3}\u{1F1F4}", "\u{1F1F8}\u{1F1EA}");
        // The first indicator would be units 19 and 20.
        let (text, lead) = at_the_edge(norway);
        assert_eq!(text_chunks(&text), vec![units(&lead), units(norway)]);
        // Between two flags is a fine place to cut, and only there.
        let pair = format!("{norway}{sweden}");
        for (lead, first, second) in [
            (16, norway.to_owned(), sweden.to_owned()),
            (17, String::new(), pair.clone()),
            (18, String::new(), pair.clone()),
            (19, String::new(), pair.clone()),
        ] {
            let letters = "x".repeat(lead);
            assert_eq!(
                text_chunks(&format!("{letters}{pair}")),
                vec![units(&format!("{letters}{first}")), units(&second)],
                "lead {lead}"
            );
        }
        // More flags than fit: five to a chunk, never four and a half.
        let flags = norway.repeat(12);
        assert_eq!(
            text_chunks(&flags),
            vec![
                units(&norway.repeat(5)),
                units(&norway.repeat(5)),
                units(&norway.repeat(2))
            ]
        );
        let offset = format!("x{flags}");
        let pieces = text_chunks(&offset);
        assert_eq!(typed(&pieces), offset);
        assert_eq!(pieces[0], units(&format!("x{}", norway.repeat(4))));
    }

    #[test]
    fn letters_next_to_the_marks_in_the_table_still_start_a_chunk() {
        // A letter taken for a mark would stick to the chunk before it.
        let twenty = "abcdefghijklmnopqrst";
        for letter in [
            // Devanagari ka and avagraha, Bengali khanda ta, Tamil visarga,
            // Kannada's sign at U+0C80, Malayalam's dot reph.
            '\u{0915}',
            '\u{093D}',
            '\u{09CE}',
            '\u{0B83}',
            '\u{0C80}',
            '\u{0D4E}',
            // Thai ko kai and sara aa, Lao ko.
            '\u{0E01}',
            '\u{0E32}',
            '\u{0E81}',
            // Hebrew alef and maqaf, Arabic alef and beh, Cyrillic a, kana.
            '\u{05D0}',
            '\u{05BE}',
            '\u{0627}',
            '\u{0628}',
            '\u{0430}',
            '\u{304B}',
            // A digit, a thumb and a black flag before what joins them.
            '1',
            '\u{1F44D}',
            '\u{1F3F4}',
        ] {
            assert_eq!(
                text_chunks(&format!("{twenty}{letter}")),
                vec![units(twenty), units(&letter.to_string())],
                "{}",
                letter.escape_unicode()
            );
        }
    }

    #[test]
    fn indic_text_is_cut_near_the_limit_between_its_syllables() {
        // "Namaste duniya, yah ek pariksha hai." with its vowel signs,
        // viramas and anusvaras, over and over.
        let sentence = "\u{0928}\u{092E}\u{0938}\u{094D}\u{0924}\u{0947} \
                        \u{0926}\u{0941}\u{0928}\u{093F}\u{092F}\u{093E}, \
                        \u{092F}\u{0939} \u{090F}\u{0915} \
                        \u{092A}\u{0930}\u{0940}\u{0915}\u{094D}\u{0937}\u{093E} \
                        \u{0939}\u{0948}\u{0902}\u{0964} ";
        let marks = [
            '\u{0902}', '\u{093E}', '\u{093F}', '\u{0940}', '\u{0941}', '\u{0947}', '\u{0948}',
            '\u{094D}',
        ];
        let text = sentence.repeat(12);
        let pieces = text_chunks(&text);
        assert_eq!(typed(&pieces), text);
        let (last, full) = pieces.split_last().unwrap();
        for piece in full.iter().chain([last]) {
            let TextPiece::Units(chunk) = piece else {
                panic!("only text here");
            };
            let chunk = String::from_utf16(chunk).unwrap();
            assert!(!chunk.starts_with(marks), "a chunk starts with a mark");
        }
        // No mark run here is longer than two, so every chunk but the last
        // is nearly full: the letters are places to cut.
        for piece in full {
            let TextPiece::Units(chunk) = piece else {
                panic!("only text here");
            };
            assert!(chunk.len() >= TEXT_CHUNK_UNITS - 2, "{}", chunk.len());
        }
    }

    #[test]
    fn a_family_stays_one_chunk() {
        // Four people and three joiners: eleven units.
        let family = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
        assert_eq!(family.encode_utf16().count(), 11);
        for lead in 0..TEXT_CHUNK_UNITS + 2 {
            let text = format!("{}{family}{family}", "x".repeat(lead));
            let pieces = text_chunks(&text);
            assert_whole_chunks(&pieces);
            assert_eq!(typed(&pieces), text);
            let whole = pieces
                .iter()
                .filter(|piece| {
                    let TextPiece::Units(chunk) = piece else {
                        return false;
                    };
                    String::from_utf16(chunk).unwrap().contains(family)
                })
                .count();
            assert_eq!(whole, 2, "lead {lead}");
        }
    }

    #[test]
    fn a_run_of_marks_longer_than_a_chunk_is_cut_between_characters() {
        let text = format!("a{}", "\u{0301}".repeat(45));
        let pieces = text_chunks(&text);
        assert_eq!(typed(&pieces), text);
        let lengths: Vec<usize> = pieces
            .iter()
            .map(|piece| match piece {
                TextPiece::Units(chunk) => chunk.len(),
                TextPiece::Return | TextPiece::Tab => 0,
            })
            .collect();
        assert_eq!(lengths, vec![20, 20, 6]);
        // Joined emoji past a chunk's length are cut too, never mid-pair.
        let chain = format!("👩{}", "\u{200D}👩".repeat(12));
        let pieces = text_chunks(&chain);
        assert_eq!(typed(&pieces), chain);
        for piece in &pieces {
            let TextPiece::Units(chunk) = piece else {
                panic!("only text here");
            };
            assert!(chunk.len() <= TEXT_CHUNK_UNITS);
            assert!(String::from_utf16(chunk).is_ok());
        }
    }

    #[test]
    fn four_thousand_units_chunk_whole() {
        let unit = "Grüße, 世界! e\u{0301}\u{0302} 😀👍🏽 🇫🇷 👨\u{200D}👩\u{200D}👧 \u{2764}\u{FE0F}\tok\r\n";
        let mut text = String::new();
        while text.encode_utf16().count() + unit.encode_utf16().count() <= 4000 {
            text.push_str(unit);
        }
        while text.encode_utf16().count() < 4000 {
            text.push('z');
        }
        assert_eq!(text.encode_utf16().count(), 4000);
        let pieces = text_chunks(&text);
        assert_whole_chunks(&pieces);
        assert_eq!(typed(&pieces), text.replace("\r\n", "\n"));
        let returns = pieces
            .iter()
            .filter(|piece| **piece == TextPiece::Return)
            .count();
        assert_eq!(returns, text.matches("\r\n").count());
        assert!(returns > 50);
    }

    #[test]
    fn a_text_piece_prints_its_length_only() {
        let pieces = text_chunks("s3cret\n\tmarker");
        assert_eq!(format!("{pieces:?}"), "[Units(6), Return, Tab, Units(6)]");
    }

    fn args(program: &Program) -> Vec<&str> {
        program
            .args
            .iter()
            .map(|arg| arg.to_str().unwrap())
            .collect()
    }

    /// A script, a name or a text nobody should ever see in a process list
    /// when it is a script, and that would do harm if a shell read it.
    const TYPED: &str = "display dialog \"hi\"; $(rm -rf ~) 'x' -- && s3cret";

    #[test]
    fn an_applescript_goes_in_on_standard_input() {
        let apple = program_for(ProgramRequest::AppleScript(TYPED));
        assert_eq!(apple.program, Path::new("/usr/bin/osascript"));
        assert_eq!(args(&apple), ["-"]);
        assert_eq!(apple.stdin.as_deref(), Some(TYPED));
        assert_eq!(apple.script, None);

        // Even an empty script goes to standard input, so nothing waits for
        // a terminal.
        let empty = program_for(ProgramRequest::AppleScript(""));
        assert_eq!(empty.stdin.as_deref(), Some(""));
        assert_eq!(args(&empty), ["-"]);
    }

    #[test]
    fn a_shell_script_goes_in_a_file_and_never_on_standard_input() {
        // On standard input its own commands could read it: `cat` on the
        // first line would swallow the second.
        let shell = program_for(ProgramRequest::Shell(TYPED));
        assert_eq!(shell.program, Path::new("/bin/zsh"));
        // The runner appends the file's path: no `-s`, no `-c`.
        assert_eq!(args(&shell), ["-l"]);
        assert_eq!(shell.stdin, None);
        assert_eq!(shell.script.as_deref(), Some(TYPED));

        let empty = program_for(ProgramRequest::Shell(""));
        assert_eq!(empty.script.as_deref(), Some(""));
        assert_eq!(empty.stdin, None);
    }

    #[test]
    fn names_are_one_argument_each() {
        let shortcut = program_for(ProgramRequest::AppleShortcut(TYPED));
        assert_eq!(shortcut.program, Path::new("/usr/bin/shortcuts"));
        assert_eq!(args(&shortcut), ["run", TYPED]);
        assert_eq!((shortcut.stdin, shortcut.script), (None, None));

        let macro_run = program_for(ProgramRequest::KeyboardMaestro(TYPED));
        assert_eq!(macro_run.program, Path::new("/usr/bin/osascript"));
        assert_eq!(
            args(&macro_run),
            [
                "-e",
                "on run argv",
                "-e",
                "tell application \"Keyboard Maestro Engine\" to do script (item 1 of argv)",
                "-e",
                "end run",
                "--",
                TYPED,
            ]
        );
        assert_eq!(
            (macro_run.stdin.clone(), macro_run.script.clone()),
            (None, None)
        );
        // The name is the last argument and in no script line.
        let lines = &macro_run.args[..macro_run.args.len() - 1];
        assert!(lines
            .iter()
            .all(|line| !line.to_string_lossy().contains("s3cret")));
    }

    #[test]
    fn speak_keeps_a_leading_dash_from_being_an_option() {
        let plain = program_for(ProgramRequest::Speak("Build finished"));
        assert_eq!(plain.program, Path::new("/usr/bin/say"));
        assert_eq!(args(&plain), ["Build finished"]);
        assert_eq!((plain.stdin.clone(), plain.script.clone()), (None, None));
        assert_eq!(
            args(&program_for(ProgramRequest::Speak("-v Fred hello"))),
            [" -v Fred hello"]
        );
        assert_eq!(args(&program_for(ProgramRequest::Speak("--"))), [" --"]);
        // A dash further in is just text.
        assert_eq!(
            args(&program_for(ProgramRequest::Speak("well - done"))),
            ["well - done"]
        );
        assert_eq!(args(&program_for(ProgramRequest::Speak(TYPED))), [TYPED]);
    }

    #[test]
    fn no_request_puts_a_script_in_the_arguments() {
        for request in [
            ProgramRequest::AppleScript(TYPED),
            ProgramRequest::Shell(TYPED),
        ] {
            let program = program_for(request);
            assert!(program
                .args
                .iter()
                .all(|arg| !arg.to_string_lossy().contains("s3cret")));
            // In one place only: standard input or the file.
            let places = [program.stdin.as_deref(), program.script.as_deref()];
            assert_eq!(places.iter().flatten().collect::<Vec<_>>(), [&TYPED]);
            assert!(program.program.has_root());
        }
        // Grape's own scripts go in on standard input.
        for control in SYSTEM_CONTROLS {
            let SystemPlan::Program(program) = system_plan(control) else {
                continue;
            };
            assert!(program.program.has_root());
            assert_eq!(program.script, None);
            if program.program == Path::new("/usr/bin/osascript") {
                assert_eq!(args(&program), ["-"]);
                assert!(program.stdin.is_some_and(|source| !source.is_empty()));
            } else {
                assert_eq!(program.stdin, None);
            }
        }
    }

    #[test]
    fn only_a_shell_script_asks_for_a_file() {
        // The file is for a program that reads its script as it runs it.
        let with_file: Vec<bool> = [
            ProgramRequest::AppleScript(TYPED),
            ProgramRequest::Shell(TYPED),
            ProgramRequest::AppleShortcut(TYPED),
            ProgramRequest::KeyboardMaestro(TYPED),
            ProgramRequest::Speak(TYPED),
        ]
        .into_iter()
        .map(|request| program_for(request).script.is_some())
        .collect();
        assert_eq!(with_file, [false, true, false, false, false]);
        let path = Path::new("/Applications/Notes.app");
        let openers = [
            OpenTarget::App {
                path,
                id: Some("com.example.notes"),
            },
            OpenTarget::App { path, id: None },
            OpenTarget::Path(path),
            OpenTarget::Url("https://example.com/"),
        ];
        for target in openers {
            let programs = open_programs(target);
            assert!(!programs.is_empty());
            for program in programs {
                assert_eq!((program.stdin, program.script), (None, None));
            }
        }
        assert_eq!(input_access_settings_program().script, None);
    }

    #[test]
    fn an_app_opens_by_id_first_and_then_by_path() {
        let path = Path::new("/Applications/My Notes; $(whoami).app");
        let both = open_programs(OpenTarget::App {
            path,
            id: Some("com.example.notes"),
        });
        assert_eq!(both.len(), 2);
        assert_eq!(args(&both[0]), ["-b", "com.example.notes"]);
        assert_eq!(
            args(&both[1]),
            ["-a", "/Applications/My Notes; $(whoami).app"]
        );
        let by_path = open_programs(OpenTarget::App { path, id: None });
        assert_eq!(by_path.len(), 1);
        assert_eq!(
            args(&by_path[0]),
            ["-a", "/Applications/My Notes; $(whoami).app"]
        );
        for program in both.iter().chain(&by_path) {
            assert_eq!(program.program, Path::new("/usr/bin/open"));
            assert_eq!(program.stdin, None);
        }
    }

    #[test]
    fn a_path_or_a_url_is_the_only_argument_of_open() {
        let file = Path::new("/Users/max/a \"b\" $c.pdf");
        let opened = open_programs(OpenTarget::Path(file));
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].program, Path::new("/usr/bin/open"));
        assert_eq!(args(&opened[0]), ["/Users/max/a \"b\" $c.pdf"]);
        assert_eq!(opened[0].stdin, None);

        let url = "https://example.com/?q=a b&c=$(d)";
        let opened = open_programs(OpenTarget::Url(url));
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].program, Path::new("/usr/bin/open"));
        assert_eq!(args(&opened[0]), [url]);
        assert_eq!(opened[0].stdin, None);
    }

    const SYSTEM_CONTROLS: [SystemControl; 9] = [
        SystemControl::SleepDisplay,
        SystemControl::LockScreen,
        SystemControl::Screensaver,
        SystemControl::MissionControl,
        SystemControl::ToggleMute,
        SystemControl::VolumeUp,
        SystemControl::VolumeDown,
        SystemControl::ToggleDarkMode,
        SystemControl::ScreenshotArea,
    ];

    fn plan_program(control: SystemControl) -> Program {
        match system_plan(control) {
            SystemPlan::Program(program) => program,
            SystemPlan::CharChord { .. } => panic!("{control:?} is a chord"),
        }
    }

    #[test]
    fn system_controls_that_are_programs() {
        let sleep = plan_program(SystemControl::SleepDisplay);
        assert_eq!(sleep.program, Path::new("/usr/bin/pmset"));
        assert_eq!(args(&sleep), ["displaysleepnow"]);
        assert_eq!(sleep.stdin, None);

        let saver = plan_program(SystemControl::Screensaver);
        assert_eq!(saver.program, Path::new("/usr/bin/open"));
        assert_eq!(args(&saver), ["-a", "ScreenSaverEngine"]);

        let mission = plan_program(SystemControl::MissionControl);
        assert_eq!(mission.program, Path::new("/usr/bin/open"));
        assert_eq!(args(&mission), ["-a", "Mission Control"]);
    }

    #[test]
    fn system_controls_that_are_scripts() {
        let script = |control| {
            let program = plan_program(control);
            assert_eq!(program.program, Path::new("/usr/bin/osascript"));
            assert_eq!(args(&program), ["-"]);
            program.stdin.unwrap()
        };
        assert_eq!(
            script(SystemControl::ToggleMute),
            "set volume output muted (not (output muted of (get volume settings)))"
        );
        assert_eq!(
            script(SystemControl::VolumeUp),
            "set level to (output volume of (get volume settings)) + 6\n\
             if level > 100 then set level to 100\n\
             set volume output volume level"
        );
        assert_eq!(
            script(SystemControl::VolumeDown),
            "set level to (output volume of (get volume settings)) - 6\n\
             if level < 0 then set level to 0\n\
             set volume output volume level"
        );
        assert_eq!(
            script(SystemControl::ToggleDarkMode),
            "tell application \"System Events\" to tell appearance preferences \
             to set dark mode to not dark mode"
        );
        // Only dark mode talks to another app (one Automation prompt).
        for control in [
            SystemControl::ToggleMute,
            SystemControl::VolumeUp,
            SystemControl::VolumeDown,
        ] {
            assert!(!script(control).contains("tell application"));
        }
    }

    #[test]
    fn system_controls_that_are_the_systems_own_chords() {
        assert_eq!(
            system_plan(SystemControl::LockScreen),
            SystemPlan::CharChord {
                ch: 'q',
                mods: held(false, true, false, true),
            }
        );
        assert_eq!(
            system_plan(SystemControl::ScreenshotArea),
            SystemPlan::CharChord {
                ch: '4',
                mods: held(true, false, false, true),
            }
        );
        // Each character has a key on the US layout to fall back on.
        let chords = SYSTEM_CONTROLS
            .into_iter()
            .filter_map(|control| match system_plan(control) {
                SystemPlan::CharChord { ch, .. } => Some(ch),
                SystemPlan::Program(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(chords, vec!['q', '4']);
        assert!(chords.iter().all(|ch| us_key_for_char(*ch).is_some()));
    }

    #[test]
    fn no_chord_grape_posts_by_character_can_open_the_wheel() {
        use crate::keys::{chords_posted_on_mac, HotkeyProblem};

        // The system controls' chords, and Copy and Paste's.
        let mut posted: Vec<(char, Modifiers)> = SYSTEM_CONTROLS
            .into_iter()
            .filter_map(|control| match system_plan(control) {
                SystemPlan::CharChord { ch, mods } => Some((ch, mods)),
                SystemPlan::Program(_) => None,
            })
            .collect();
        posted.extend([('c', Modifiers::META), ('v', Modifiers::META)]);
        assert_eq!(posted.len(), chords_posted_on_mac().len());
        for (ch, mods) in posted {
            // The place the character has on the US layout.
            let virtual_code = us_key_for_char(ch).unwrap();
            let key = KeyCode::ALL
                .iter()
                .copied()
                .find(|key| virtual_key(*key) == virtual_code)
                .unwrap();
            let chord = Chord { key, mods };
            assert!(chords_posted_on_mac().contains(&chord), "{ch}");
            assert_eq!(
                chord.hotkey_problem(true),
                Some(HotkeyProblem::Reserved),
                "{ch}"
            );
        }
    }

    #[test]
    fn an_open_press_is_dropped_while_a_modal_window_is_up() {
        assert!(!passes_press(HotkeyId::Open, true));
        assert!(passes_press(HotkeyId::Open, false));
        // Esc and Return still close a wheel that is open.
        for id in [HotkeyId::Escape, HotkeyId::Confirm] {
            assert!(passes_press(id, true));
            assert!(passes_press(id, false));
        }
    }

    #[test]
    fn a_hop_belongs_to_whoever_asks_first() {
        // The main thread got there first: the caller waits for the answer.
        let claim = HopClaim::default();
        assert!(claim.take());
        assert!(!claim.take());
        // The caller gave up first: the work never runs.
        let claim = std::sync::Arc::new(HopClaim::default());
        assert!(claim.take());
        let theirs = std::sync::Arc::clone(&claim);
        let ran = std::thread::spawn(move || theirs.take()).join().unwrap();
        assert!(!ran);
        // Asked from two threads at once, exactly one gets it.
        for _ in 0..200 {
            let claim = std::sync::Arc::new(HopClaim::default());
            let theirs = std::sync::Arc::clone(&claim);
            let other = std::thread::spawn(move || theirs.take());
            let mine = claim.take();
            assert_ne!(mine, other.join().unwrap());
        }
    }

    #[test]
    fn the_accessibility_list_opens_by_its_url() {
        let program = input_access_settings_program();
        assert_eq!(program.program, Path::new("/usr/bin/open"));
        assert_eq!(
            args(&program),
            ["x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"]
        );
        assert_eq!(program.stdin, None);
    }

    #[test]
    fn a_hot_key_event_id_says_what_it_is_for() {
        let ids = [HotkeyId::Open, HotkeyId::Escape, HotkeyId::Confirm];
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            for serial in [0, 1, 2, 0x00FF_FFFF, 0x0100_0000, u32::MAX] {
                let event_id = hotkey_event_id(id, serial);
                assert_eq!(hotkey_for_event_id(event_id), Some(id), "{id:?} {serial}");
                seen.insert(event_id);
            }
        }
        // The serial wraps inside its 24 bits and never reaches the kind.
        assert_eq!(
            hotkey_event_id(HotkeyId::Open, 0x0100_0000),
            hotkey_event_id(HotkeyId::Open, 0)
        );
        assert_eq!(seen.len(), 3 * 4);
        // Registrations made one after another never share an id.
        let run: std::collections::HashSet<u32> = (0..64)
            .map(|serial| hotkey_event_id(ids[serial as usize % 3], serial))
            .collect();
        assert_eq!(run.len(), 64);
        for stranger in [0, 0x00FF_FFFF, 4 << 24, u32::MAX] {
            assert_eq!(hotkey_for_event_id(stranger), None);
        }
        assert_eq!(HOTKEY_SIGNATURE, 0x4752_5045);
    }

    fn chord(key: KeyCode, mods: Modifiers) -> Chord {
        Chord { key, mods }
    }

    fn holding(id: HotkeyId, chord: Chord) -> HeldChord {
        HeldChord {
            id,
            chord,
            stuck: false,
        }
    }

    #[test]
    fn a_first_bind_registers_each_chord_once() {
        let g = chord(KeyCode::KeyG, held(false, true, true, false));
        let f5 = chord(KeyCode::F5, Modifiers::NONE);
        assert_eq!(
            bind_plan(&[], HotkeyId::Open, &[g, f5, g]),
            BindPlan {
                register: vec![g, f5],
                release: vec![],
                adopt: vec![],
            }
        );
        assert_eq!(bind_plan(&[], HotkeyId::Open, &[]), BindPlan::default());
    }

    #[test]
    fn a_bind_replaces_what_the_id_had_and_keeps_what_stays() {
        let old = chord(KeyCode::KeyG, held(false, true, true, false));
        let new = chord(KeyCode::KeyJ, held(false, true, false, false));
        let esc = chord(KeyCode::Escape, Modifiers::NONE);
        let table = [holding(HotkeyId::Open, old), holding(HotkeyId::Escape, esc)];
        assert_eq!(
            bind_plan(&table, HotkeyId::Open, &[new]),
            BindPlan {
                register: vec![new],
                release: vec![0],
                adopt: vec![],
            }
        );
        // The same chords again: nothing to do.
        assert_eq!(
            bind_plan(&table, HotkeyId::Open, &[old]),
            BindPlan::default()
        );
        // One stays, one goes, one comes.
        let both = [holding(HotkeyId::Open, old), holding(HotkeyId::Open, new)];
        let third = chord(KeyCode::F13, Modifiers::NONE);
        assert_eq!(
            bind_plan(&both, HotkeyId::Open, &[third, new]),
            BindPlan {
                register: vec![third],
                release: vec![0],
                adopt: vec![],
            }
        );
        // An empty list unbinds the id and leaves the others alone.
        assert_eq!(
            bind_plan(&table, HotkeyId::Escape, &[]),
            BindPlan {
                register: vec![],
                release: vec![1],
                adopt: vec![],
            }
        );
    }

    #[test]
    fn a_chord_another_id_holds_is_left_to_it() {
        // The wheel opens with Control+Option+Esc, so that subset of Esc
        // stays the Open hot key's.
        let open = chord(KeyCode::Escape, held(false, true, true, false));
        let table = [holding(HotkeyId::Open, open)];
        let subsets = open.with_modifier_subsets();
        let plan = bind_plan(&table, HotkeyId::Escape, &subsets);
        assert_eq!(plan.register.len(), 3);
        assert!(!plan.register.contains(&open));
        assert!(plan.release.is_empty() && plan.adopt.is_empty());
        // Even one the OS is still to let go of.
        let stuck = [HeldChord {
            id: HotkeyId::Confirm,
            chord: open,
            stuck: true,
        }];
        assert_eq!(
            bind_plan(&stuck, HotkeyId::Escape, &[open]),
            BindPlan::default()
        );
    }

    #[test]
    fn a_stuck_chord_is_adopted_when_wanted_again_and_left_alone_when_not() {
        let esc = chord(KeyCode::Escape, Modifiers::NONE);
        let ctrl_esc = chord(KeyCode::Escape, Modifiers::CTRL);
        let table = [
            HeldChord {
                id: HotkeyId::Escape,
                chord: esc,
                stuck: true,
            },
            holding(HotkeyId::Escape, ctrl_esc),
        ];
        assert_eq!(
            bind_plan(&table, HotkeyId::Escape, &[esc, ctrl_esc]),
            BindPlan {
                register: vec![],
                release: vec![],
                adopt: vec![0],
            }
        );
        // Unbinding lets go of the live one; the stuck one is retried
        // apart from the plan.
        assert_eq!(
            bind_plan(&table, HotkeyId::Escape, &[]),
            BindPlan {
                register: vec![],
                release: vec![1],
                adopt: vec![],
            }
        );
    }

    #[test]
    fn the_first_poll_says_where_the_pointer_is_and_nothing_else() {
        for down in [false, true] {
            let mut edges = PointerEdges::default();
            assert_eq!(
                edges.poll((10, 20), down),
                vec![PointerSignal::Moved { x: 10, y: 20 }]
            );
            // A pointer at rest says nothing more.
            assert_eq!(edges.poll((10, 20), down), vec![]);
        }
    }

    #[test]
    fn a_click_is_a_down_and_an_up_where_they_happened() {
        let mut edges = PointerEdges::default();
        edges.poll((0, 0), false);
        assert_eq!(
            edges.poll((0, 0), true),
            vec![PointerSignal::Down { x: 0, y: 0 }]
        );
        assert_eq!(edges.poll((0, 0), true), vec![]);
        // Moving with the button held, then letting go somewhere else: the
        // move comes first, the release at the new place.
        assert_eq!(
            edges.poll((3, 4), true),
            vec![PointerSignal::Moved { x: 3, y: 4 }]
        );
        assert_eq!(
            edges.poll((5, -6), false),
            vec![
                PointerSignal::Moved { x: 5, y: -6 },
                PointerSignal::Up { x: 5, y: -6 },
            ]
        );
        assert_eq!(edges.poll((5, -6), false), vec![]);
    }

    #[test]
    fn a_button_already_down_sends_no_down_but_its_release_is_seen() {
        let mut edges = PointerEdges::default();
        assert_eq!(
            edges.poll((7, 7), true),
            vec![PointerSignal::Moved { x: 7, y: 7 }]
        );
        assert_eq!(
            edges.poll((7, 7), false),
            vec![PointerSignal::Up { x: 7, y: 7 }]
        );
    }

    #[test]
    fn turning_the_watch_on_again_starts_over() {
        let mut edges = PointerEdges::default();
        edges.poll((1, 1), false);
        edges.poll((1, 1), true);
        edges.restart();
        // The same place and a held button: one Moved, no Down, and no Up
        // for the press that happened before.
        assert_eq!(
            edges.poll((1, 1), true),
            vec![PointerSignal::Moved { x: 1, y: 1 }]
        );
        edges.restart();
        assert_eq!(
            edges.poll((1, 1), false),
            vec![PointerSignal::Moved { x: 1, y: 1 }]
        );
        assert_eq!(edges.poll((1, 1), false), vec![]);
    }

    /// A layout as a list of (virtual key, what it types alone, what it
    /// types with Command).
    fn keyboard(keys: &[(u16, char, char)]) -> impl Fn(u16, bool) -> Option<char> + '_ {
        move |key, command| {
            keys.iter()
                .find(|(code, _, _)| *code == key)
                .map(|(_, alone, with_command)| if command { *with_command } else { *alone })
        }
    }

    #[test]
    fn a_character_chord_comes_from_the_key_that_types_it() {
        // AZERTY: the US `Q` key types `a`, and the US `A` key types `q`.
        let azerty = keyboard(&[(0, 'q', 'q'), (12, 'a', 'a'), (8, 'c', 'c')]);
        assert_eq!(char_key('q', true, &azerty), Some(0));
        assert_eq!(char_key('q', false, &azerty), Some(0));
        assert_eq!(char_key('c', true, &azerty), Some(8));
        assert_eq!(char_key('C', true, &azerty), Some(8));
        // A layout that can't be read: the US table.
        let unread = |_: u16, _: bool| None;
        assert_eq!(char_key('q', true, unread), Some(12));
        assert_eq!(char_key('4', true, unread), Some(21));
        assert_eq!(char_key('é', true, unread), None);
    }

    #[test]
    fn a_command_chord_reads_what_the_keys_type_with_command() {
        // Dvorak-QWERTY: the US `I` key (34) types `c` alone, and the US
        // `C` key (8) types `j` alone but `c` with Command.
        let dvorak_qwerty = keyboard(&[(34, 'c', 'i'), (8, 'j', 'c')]);
        assert_eq!(char_key('c', true, &dvorak_qwerty), Some(8));
        assert_eq!(char_key('c', false, &dvorak_qwerty), Some(34));
        // Russian: no key types `c` alone, the US `C` key does with
        // Command.
        let russian = keyboard(&[(8, 'с', 'c'), (9, 'м', 'v')]);
        assert_eq!(char_key('c', true, &russian), Some(8));
        assert_eq!(char_key('v', true, &russian), Some(9));
        // With Command it finds nothing, alone it does.
        let odd = keyboard(&[(40, 'q', '\u{F700}')]);
        assert_eq!(char_key('q', true, &odd), Some(40));
    }

    #[test]
    fn a_character_chord_never_comes_from_the_number_pad() {
        // AZERTY types `4` with Shift on the main row, so alone only the
        // pad's key (86) types it: the chord falls back to the US `4` key.
        let azerty = keyboard(&[(21, '\'', '\''), (86, '4', '4')]);
        assert_eq!(char_key('4', true, &azerty), Some(21));
        assert_eq!(char_key('4', false, &azerty), Some(21));
        // The main row wins over the pad where both type it.
        let us = keyboard(&[(21, '4', '4'), (86, '4', '4')]);
        assert_eq!(char_key('4', true, &us), Some(21));
        for pad in [65, 67, 69, 71, 75, 76, 78, 81, 82, 89, 91, 92, 95] {
            assert!(is_keypad_key(pad), "{pad}");
        }
        // No key of Grape's own table is on the pad.
        for key in KeyCode::ALL {
            assert!(!is_keypad_key(virtual_key(*key)), "{key:?}");
        }
        assert_eq!(layout_modifier_state(false), 0);
        assert_eq!(layout_modifier_state(true), 1);
    }

    #[test]
    fn the_clock_fields_become_a_local_time() {
        assert_eq!(
            local_time(126, 10, 23, 14, 5),
            Some(LocalTime {
                year: 2026,
                month: 11,
                day: 23,
                hour: 14,
                minute: 5,
            })
        );
        assert_eq!(
            local_time(100, 0, 1, 0, 0),
            Some(LocalTime {
                year: 2000,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
            })
        );
        assert!(local_time(126, 11, 31, 23, 59).is_some());
        for bad in [
            local_time(126, 12, 1, 0, 0),
            local_time(126, -1, 1, 0, 0),
            local_time(126, 0, 0, 0, 0),
            local_time(126, 0, 32, 0, 0),
            local_time(126, 0, 1, 24, 0),
            local_time(126, 0, 1, -1, 0),
            local_time(126, 0, 1, 0, 60),
            local_time(i32::MAX, 0, 1, 0, 0),
            local_time(126, i32::MAX, 1, 0, 0),
        ] {
            assert_eq!(bad, None);
        }
    }

    #[test]
    fn only_a_group_of_its_own_is_ended() {
        assert_eq!(killable_group(4321, 77), Some(4321));
        assert_eq!(killable_group(2, 77), Some(2));
        // Never the caller's own group, launchd's, or Grape's.
        assert_eq!(killable_group(0, 77), None);
        assert_eq!(killable_group(1, 77), None);
        assert_eq!(killable_group(77, 77), None);
        // A pid that isn't one: as a negative number it would name every
        // process.
        assert_eq!(killable_group(u32::MAX, 77), None);
        assert_eq!(killable_group(1 << 31, 77), None);
    }

    #[test]
    fn a_window_frame_is_rounded_and_has_a_center() {
        let frame = ax_frame(-1279.6, 24.5, 640.4, 480.0);
        assert_eq!(
            frame,
            Rect {
                x: -1280,
                y: 25,
                width: 640,
                height: 480,
            }
        );
        assert_eq!(center(frame), (-960, 265));
        // Nonsense from an app stays a number.
        assert_eq!(ax_frame(f64::NAN, f64::INFINITY, -1.0, 0.0).x, 0);
        let far = Rect {
            x: i32::MAX,
            y: i32::MAX,
            width: i32::MAX,
            height: 3,
        };
        assert_eq!(center(far), (i32::MAX, i32::MAX));
    }

    #[test]
    fn a_picked_app_is_named_without_its_extension() {
        let path = Path::new("/Applications/Notes.app");
        assert_eq!(app_name("Notes", path), "Notes");
        assert_eq!(app_name("Notes.app", path), "Notes");
        assert_eq!(app_name("Notes.APP", path), "Notes");
        assert_eq!(app_name("  Notizen  ", path), "Notizen");
        // A name that only looks like the extension, or ends in other
        // letters, stays whole.
        assert_eq!(app_name("Snapp", path), "Snapp");
        assert_eq!(app_name("メモ", path), "メモ");
        assert_eq!(app_name("é.app", path), "é");
        // No display name: the bundle's own file name.
        assert_eq!(app_name("", path), "Notes");
        assert_eq!(app_name(".app", path), "Notes");
        assert_eq!(app_name("", Path::new("/")), "");
    }
}
