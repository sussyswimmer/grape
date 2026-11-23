//! What the Shortcuts wheel asks of the OS
//! (`docs/design/m10-shortcuts-wheel.md` §4): global hot keys, the pointer
//! while the wheel is open, pressing keys and typing in the app in front,
//! the clipboard, opening things, the programs that run scripts, system
//! controls, moving another app's window, and the app and file pickers.
//!
//! macOS first. Everywhere else every function returns
//! [`PlatformError::Unsupported`], except the few that can't fail:
//! [`input_access`] is [`InputAccess::NotNeeded`], [`held_modifiers`] is
//! [`Modifiers::NONE`], [`local_date_time`] is `None`, [`ensure_front`] is
//! `false`, and [`kill_group`] and [`give_back_front`] do nothing.
//!
//! Much of what passes through here is something the user typed or picked:
//! text, scripts, URLs, paths, app ids. The types that hold any of it have a
//! `Debug` that prints kinds and counts only, so none of it can reach a log.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use crate::keys::Chord;
use crate::{imp, Modifiers, PlatformError, Rect};

/// What a hot key is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyId {
    /// Opens the Shortcuts wheel.
    Open,
    /// Closes an open wheel (Esc). Bound only while one is open.
    Escape,
    /// Picks the highlighted segment (Return). Bound only while a wheel is
    /// open.
    Confirm,
}

/// The app in front when a hot key was pressed.
#[derive(Clone, PartialEq, Eq)]
pub struct FrontApp {
    /// Its process id.
    pub pid: i32,
    /// Its bundle id, when it has one.
    pub id: Option<String>,
}

/// Prints the pid only: an app's id never reaches a log.
impl fmt::Debug for FrontApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrontApp")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// What the OS said at the moment a hot key went down, before anything of
/// Grape's showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PressContext {
    /// Where the cursor was (global, top-left origin), if it could be read.
    pub cursor: Option<(i32, i32)>,
    /// The app in front. `None` when that was Grape itself, or nothing.
    pub front: Option<FrontApp>,
    /// A mouse button was down.
    pub button_down: bool,
}

/// One hot key going down or up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeySignal {
    /// The hot key went down.
    Pressed(HotkeyId, PressContext),
    /// The hot key came up.
    Released(HotkeyId),
}

/// Global hot keys.
///
/// Implementations must never block the user's input: the handler reads
/// state, `try_send`s it and returns. A full channel drops the signal.
pub trait HotkeyMonitor: Send {
    /// Starts listening and sends signals to `tx`.
    ///
    /// # Errors
    /// [`PlatformError::AlreadyRunning`] if the monitor is already started,
    /// or an OS error if the handler couldn't be installed.
    fn start(&mut self, tx: SyncSender<HotkeySignal>) -> Result<(), PlatformError>;

    /// Binds `id` to these chords (any of them fires it), replacing what it
    /// had. An empty list unbinds it. Returns only once the OS has done it.
    ///
    /// # Errors
    /// An OS error when the OS wouldn't give Grape one of the chords. `id`
    /// then keeps what it had.
    fn bind(&mut self, id: HotkeyId, chords: &[Chord]) -> Result<(), PlatformError>;

    /// Stops listening and lets go of every chord. Safe to call more than
    /// once, and before `start`.
    fn stop(&mut self);
}

/// Creates the hot key monitor for this OS.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS.
pub fn hotkey_monitor() -> Result<Box<dyn HotkeyMonitor>, PlatformError> {
    imp::hotkey_monitor()
}

/// What the pointer did while a Shortcuts wheel was open. Positions are
/// global, top-left origin: physical pixels on Windows, points on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerSignal {
    /// The pointer is here now.
    Moved {
        /// Horizontal position.
        x: i32,
        /// Vertical position (grows downwards).
        y: i32,
    },
    /// The primary button went down.
    Down {
        /// Horizontal position.
        x: i32,
        /// Vertical position (grows downwards).
        y: i32,
    },
    /// The primary button came up.
    Up {
        /// Horizontal position.
        x: i32,
        /// Vertical position (grows downwards).
        y: i32,
    },
}

/// Follows the pointer while a Shortcuts wheel is open: no drag is in
/// progress then, so the drag monitor says nothing.
pub trait PointerWatch: Send {
    /// Gets ready to send signals to `tx`. Nothing is sent until
    /// [`set_active`](Self::set_active) turns the watch on.
    ///
    /// # Errors
    /// [`PlatformError::AlreadyRunning`] if the watch is already started,
    /// or an OS error.
    fn start(&mut self, tx: SyncSender<PointerSignal>) -> Result<(), PlatformError>;

    /// Turns the watch on or off. While on: one `Moved` at once, then
    /// `Moved` only when the position changed, and `Down`/`Up` for
    /// primary-button changes seen while on (a button already down sends no
    /// `Down`).
    fn set_active(&mut self, on: bool);

    /// Stops the watch. Safe to call more than once, and before `start`.
    fn stop(&mut self);
}

/// Creates the pointer watch for this OS.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS.
pub fn pointer_watch() -> Result<Box<dyn PointerWatch>, PlatformError> {
    imp::pointer_watch()
}

/// Whether Grape may press keys and move other apps' windows (Accessibility
/// on macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAccess {
    /// The user allowed it.
    Granted,
    /// Not allowed yet, or not for this build of Grape.
    Denied,
    /// This OS asks for no such permission (or has no Shortcuts wheel yet).
    NotNeeded,
}

/// Whether Grape may press keys right now. Never fails and never prompts.
pub fn input_access() -> InputAccess {
    imp::input_access()
}

/// Asks the OS for input access: the system's own prompt, which also puts
/// Grape in the list. With `open_settings` it opens that list in System
/// Settings too.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS, or an OS error if the
/// settings pane couldn't be opened.
pub fn request_input_access(open_settings: bool) -> Result<(), PlatformError> {
    imp::request_input_access(open_settings)
}

/// The modifier keys held right now. Never fails: [`Modifiers::NONE`] where
/// they can't be read.
pub fn held_modifiers() -> Modifiers {
    imp::held_modifiers()
}

/// A date and time on the user's own clock, to the minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// The year, such as 2026.
    pub year: i32,
    /// The month, 1 to 12.
    pub month: u8,
    /// The day of the month, 1 to 31.
    pub day: u8,
    /// The hour, 0 to 23.
    pub hour: u8,
    /// The minute, 0 to 59.
    pub minute: u8,
}

/// Now, in the user's time zone. `None` where the clock can't be read that
/// way.
pub fn local_date_time() -> Option<LocalTime> {
    imp::local_date_time()
}

/// Presses and lets go of `chord` in the app in front, from the chord's own
/// place on the keyboard. Needs input access.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS, or an OS error if the
/// key events couldn't be made.
pub fn send_chord(chord: Chord) -> Result<(), PlatformError> {
    imp::send_chord(chord)
}

/// Presses the key that types `ch` on the current keyboard layout, with
/// `mods`: for chords Grape authors by letter (Copy is Command+`c`). Needs
/// input access.
///
/// # Errors
/// As [`send_chord`], and when no key types `ch`.
pub fn send_char_chord(ch: char, mods: Modifiers) -> Result<(), PlatformError> {
    imp::send_char_chord(ch, mods)
}

/// Types `text` into the app in front; `\n` is Return and `\t` is Tab.
/// `cancel` is checked between chunks, and a set flag ends the typing
/// early without an error. Needs input access.
///
/// # Errors
/// As [`send_chord`].
pub fn type_text(text: &str, cancel: &AtomicBool) -> Result<(), PlatformError> {
    imp::type_text(text, cancel)
}

/// The text on the clipboard, or `None` when it holds no text.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS, or an OS error.
pub fn clipboard_text() -> Result<Option<String>, PlatformError> {
    imp::clipboard_text()
}

/// A number that moves every time the clipboard's contents change, to tell
/// when a Copy has landed.
///
/// # Errors
/// As [`clipboard_text`].
pub fn clipboard_change_count() -> Result<i64, PlatformError> {
    imp::clipboard_change_count()
}

/// Empties the clipboard.
///
/// # Errors
/// As [`clipboard_text`].
pub fn clear_clipboard() -> Result<(), PlatformError> {
    imp::clear_clipboard()
}

/// Something to open.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OpenTarget<'a> {
    /// An app: by its id when it has one, else by where it is.
    App {
        /// Where the app is.
        path: &'a Path,
        /// Its bundle id.
        id: Option<&'a str>,
    },
    /// A file or a folder, in the app that opens its kind.
    Path(&'a Path),
    /// A URL, in the app registered for its scheme.
    Url(&'a str),
}

/// Prints the kind only.
impl fmt::Debug for OpenTarget<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::App { .. } => "App",
            Self::Path(_) => "Path",
            Self::Url(_) => "Url",
        })
    }
}

/// Opens `target`. Waits for the opener to answer, so a target that can't
/// be opened is an error. Call it from a worker thread.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS, or an OS error when the
/// opener couldn't start or said no.
pub fn open_target(target: OpenTarget<'_>) -> Result<(), PlatformError> {
    imp::open_target(target)
}

/// A program to run, with what goes to its standard input.
#[derive(Clone, PartialEq, Eq)]
pub struct Program {
    /// The executable.
    pub program: PathBuf,
    /// Its arguments, each one its own: nothing goes through a shell.
    pub args: Vec<OsString>,
    /// What to write to its standard input before closing it. A script goes
    /// here, never in `args`, so it doesn't show in the process list.
    pub stdin: Option<String>,
}

/// Prints how many arguments there are and whether there is input, never
/// what they say.
impl fmt::Debug for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Program")
            .field("args", &self.args.len())
            .field("stdin", &self.stdin.is_some())
            .finish_non_exhaustive()
    }
}

/// Something a step hands to a program of the OS.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProgramRequest<'a> {
    /// Run this AppleScript source.
    AppleScript(&'a str),
    /// Run this shell script in the user's login shell.
    Shell(&'a str),
    /// Run the Apple Shortcut with this name.
    AppleShortcut(&'a str),
    /// Run the Keyboard Maestro macro with this name.
    KeyboardMaestro(&'a str),
    /// Say this text aloud.
    Speak(&'a str),
}

/// Prints the kind only.
impl fmt::Debug for ProgramRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AppleScript(_) => "AppleScript",
            Self::Shell(_) => "Shell",
            Self::AppleShortcut(_) => "AppleShortcut",
            Self::KeyboardMaestro(_) => "KeyboardMaestro",
            Self::Speak(_) => "Speak",
        })
    }
}

/// The program that does `request` on this OS. Nothing runs here: the app
/// runs it, in a process group of its own.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS.
pub fn program_for(request: ProgramRequest<'_>) -> Result<Program, PlatformError> {
    imp::program_for(request)
}

/// Ends a program started in its own process group, and everything it
/// started. `pid` is the program's, which is also its group's. Does nothing
/// except on macOS.
pub fn kill_group(pid: u32) {
    imp::kill_group(pid);
}

/// Something the system itself does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SystemControl {
    /// Turn the displays off.
    SleepDisplay,
    /// Lock the screen.
    LockScreen,
    /// Start the screen saver.
    Screensaver,
    /// Show every window (Mission Control).
    MissionControl,
    /// Mute the sound, or bring it back.
    ToggleMute,
    /// Turn the sound up a step.
    VolumeUp,
    /// Turn the sound down a step.
    VolumeDown,
    /// Switch between the light and the dark appearance.
    ToggleDarkMode,
    /// Start a screenshot of an area the user drags out.
    ScreenshotArea,
}

/// How a [`SystemControl`] is done on this OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPlan {
    /// Run this program.
    Program(Program),
    /// Press the system's own chord for it, through [`send_char_chord`].
    CharChord {
        /// The character the chord's key types.
        ch: char,
        /// The modifiers held with it.
        mods: Modifiers,
    },
}

/// How `control` is done on this OS. Nothing runs here.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS.
pub fn system_plan(control: SystemControl) -> Result<SystemPlan, PlatformError> {
    imp::system_plan(control)
}

/// Where a Window step puts the focused window on its display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Placement {
    /// The left half of the work area.
    LeftHalf,
    /// The right half.
    RightHalf,
    /// The top half.
    TopHalf,
    /// The bottom half.
    BottomHalf,
    /// The whole work area.
    Fill,
    /// The middle of the work area, at the window's own size.
    Center,
    /// The top-left quarter.
    TopLeft,
    /// The top-right quarter.
    TopRight,
    /// The bottom-left quarter.
    BottomLeft,
    /// The bottom-right quarter.
    BottomRight,
}

/// Where `placement` puts a window of `size` (width, height) on a display's
/// `work_area`. Pure.
///
/// The two halves of a pair and the four quarters tile the work area
/// exactly: when a side is odd, the right or bottom part is the one unit
/// larger. Only [`Placement::Center`] reads `size`; a window larger than the
/// work area is made as small as the work area, so it never leaves it. A
/// work area with a negative side counts as empty.
pub fn placement_frame(placement: Placement, work_area: Rect, size: (i32, i32)) -> Rect {
    let width = work_area.width.max(0);
    let height = work_area.height.max(0);
    let (left, top) = (width / 2, height / 2);
    let (right, bottom) = (width - left, height - top);
    let (x, y) = (work_area.x, work_area.y);
    let (mid_x, mid_y) = (x.saturating_add(left), y.saturating_add(top));
    let rect = |x, y, width, height| Rect {
        x,
        y,
        width,
        height,
    };
    match placement {
        Placement::LeftHalf => rect(x, y, left, height),
        Placement::RightHalf => rect(mid_x, y, right, height),
        Placement::TopHalf => rect(x, y, width, top),
        Placement::BottomHalf => rect(x, mid_y, width, bottom),
        Placement::Fill => rect(x, y, width, height),
        Placement::Center => {
            let own_width = size.0.clamp(0, width);
            let own_height = size.1.clamp(0, height);
            rect(
                x.saturating_add((width - own_width) / 2),
                y.saturating_add((height - own_height) / 2),
                own_width,
                own_height,
            )
        }
        Placement::TopLeft => rect(x, y, left, top),
        Placement::TopRight => rect(mid_x, y, right, top),
        Placement::BottomLeft => rect(x, mid_y, left, bottom),
        Placement::BottomRight => rect(mid_x, mid_y, right, bottom),
    }
}

/// Moves the focused window of the app `pid` to `placement` on the display
/// it is on. Needs input access.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS, or an OS error when the
/// app has no window to move or the window refused.
pub fn place_window(pid: i32, placement: Placement) -> Result<(), PlatformError> {
    imp::place_window(pid, placement)
}

/// Gives the front back to the app the user was in, if Grape took it. Queue
/// it on the main thread after the overlay's hide. Does nothing except on
/// macOS.
pub fn give_back_front() {
    imp::give_back_front();
}

/// Waits up to `timeout` for the app `pid` to be the one in front, handing
/// the front back whenever Grape has it. `false` when it never came to the
/// front, and always except on macOS.
///
/// Call it from a worker thread, never the main one.
pub fn ensure_front(pid: i32, timeout: Duration) -> bool {
    imp::ensure_front(pid, timeout)
}

/// What a picker lets the user choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    /// An app.
    App,
    /// A file or a folder.
    FileOrFolder,
}

/// What the user picked.
#[derive(Clone, PartialEq, Eq)]
pub struct Picked {
    /// Where it is.
    pub path: PathBuf,
    /// It is a folder (an app counts as a file).
    pub folder: bool,
    /// Who the app is, when an app was picked and it said.
    pub app: Option<AppIdentity>,
}

/// Prints whether it is a folder and whether an app's identity came with
/// it, never the path.
impl fmt::Debug for Picked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Picked")
            .field("folder", &self.folder)
            .field("app", &self.app.is_some())
            .finish_non_exhaustive()
    }
}

/// An app's id and the name it shows.
#[derive(Clone, PartialEq, Eq)]
pub struct AppIdentity {
    /// Its bundle id.
    pub id: String,
    /// The name it shows in Finder.
    pub name: String,
}

/// Prints neither the id nor the name.
impl fmt::Debug for AppIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppIdentity").finish_non_exhaustive()
    }
}

/// Asks the user for an app, or for a file or folder. `Ok(None)` when the
/// user cancels. Nothing on disk changes.
///
/// `owner` is the window asking, as [`pick_folder`] takes it; macOS ignores
/// it. Blocks until the user is done: call it from a worker thread, never
/// the main thread or one the main thread is waiting for.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on macOS, or an OS error if the
/// picker couldn't open.
///
/// [`pick_folder`]: crate::file_actions::pick_folder
pub fn pick_target(kind: PickKind, owner: Option<isize>) -> Result<Option<Picked>, PlatformError> {
    imp::pick_target(kind, owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Something no log line may ever hold.
    const MARKER: &str = "s3cret-marker";

    const PLACEMENTS: [Placement; 10] = [
        Placement::LeftHalf,
        Placement::RightHalf,
        Placement::TopHalf,
        Placement::BottomHalf,
        Placement::Fill,
        Placement::Center,
        Placement::TopLeft,
        Placement::TopRight,
        Placement::BottomLeft,
        Placement::BottomRight,
    ];

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// Work areas with odd and even sides, away from the origin on both
    /// sides of it.
    fn work_areas() -> Vec<Rect> {
        vec![
            rect(0, 33, 1512, 879),
            rect(-2560, -633, 2560, 1415),
            rect(1513, -207, 1919, 1055),
            rect(-7, 11, 7, 5),
            rect(3, -4, 1, 1),
            rect(40, 50, 6, 9),
        ]
    }

    fn holds(rect: Rect, x: i32, y: i32) -> bool {
        x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
    }

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.width <= outer.x + outer.width
            && inner.y + inner.height <= outer.y + outer.height
    }

    fn area(rect: Rect) -> i64 {
        i64::from(rect.width) * i64::from(rect.height)
    }

    /// The parts cover `whole` exactly: their areas add up to its own, each
    /// lies inside it, and no two overlap.
    fn tiles(parts: &[Rect], whole: Rect) -> bool {
        let overlap = |a: Rect, b: Rect| {
            a.x < b.x + b.width
                && b.x < a.x + a.width
                && a.y < b.y + b.height
                && b.y < a.y + a.height
                && area(a) > 0
                && area(b) > 0
        };
        let no_overlap = parts
            .iter()
            .enumerate()
            .all(|(i, a)| parts.iter().skip(i + 1).all(|b| !overlap(*a, *b)));
        parts.iter().all(|part| inside(*part, whole))
            && parts.iter().map(|part| area(*part)).sum::<i64>() == area(whole)
            && no_overlap
    }

    fn frames(placements: &[Placement], work_area: Rect) -> Vec<Rect> {
        placements
            .iter()
            .map(|placement| placement_frame(*placement, work_area, (640, 480)))
            .collect()
    }

    #[test]
    fn each_placement_on_an_odd_work_area_off_the_origin() {
        let area = rect(-7, 11, 1513, 879);
        let frame = |placement| placement_frame(placement, area, (601, 401));
        assert_eq!(frame(Placement::LeftHalf), rect(-7, 11, 756, 879));
        assert_eq!(frame(Placement::RightHalf), rect(749, 11, 757, 879));
        assert_eq!(frame(Placement::TopHalf), rect(-7, 11, 1513, 439));
        assert_eq!(frame(Placement::BottomHalf), rect(-7, 450, 1513, 440));
        assert_eq!(frame(Placement::Fill), area);
        assert_eq!(frame(Placement::Center), rect(449, 250, 601, 401));
        assert_eq!(frame(Placement::TopLeft), rect(-7, 11, 756, 439));
        assert_eq!(frame(Placement::TopRight), rect(749, 11, 757, 439));
        assert_eq!(frame(Placement::BottomLeft), rect(-7, 450, 756, 440));
        assert_eq!(frame(Placement::BottomRight), rect(749, 450, 757, 440));
    }

    #[test]
    fn every_placement_stays_inside_the_work_area() {
        for area in work_areas() {
            for placement in PLACEMENTS {
                for size in [(640, 480), (1, 1), (5000, 3), (3, 5000), (9999, 9999)] {
                    let frame = placement_frame(placement, area, size);
                    assert!(inside(frame, area), "{placement:?} on {area:?}");
                    assert!(frame.width >= 0 && frame.height >= 0);
                }
            }
        }
    }

    #[test]
    fn the_halves_tile_the_work_area() {
        for area in work_areas() {
            let sides = frames(&[Placement::LeftHalf, Placement::RightHalf], area);
            assert!(tiles(&sides, area), "{area:?}");
            assert_eq!(sides[0].x + sides[0].width, sides[1].x);
            let stacked = frames(&[Placement::TopHalf, Placement::BottomHalf], area);
            assert!(tiles(&stacked, area), "{area:?}");
            assert_eq!(stacked[0].y + stacked[0].height, stacked[1].y);
        }
    }

    #[test]
    fn the_quarters_tile_the_work_area() {
        let quarters = [
            Placement::TopLeft,
            Placement::TopRight,
            Placement::BottomLeft,
            Placement::BottomRight,
        ];
        for area in work_areas() {
            let parts = frames(&quarters, area);
            assert!(tiles(&parts, area), "{area:?}");
            // Each quarter is where its two halves cross.
            let halves = frames(
                &[
                    Placement::LeftHalf,
                    Placement::RightHalf,
                    Placement::TopHalf,
                    Placement::BottomHalf,
                ],
                area,
            );
            for (quarter, (side, level)) in parts.iter().zip([(0, 2), (1, 2), (0, 3), (1, 3)]) {
                assert_eq!(
                    (quarter.x, quarter.width),
                    (halves[side].x, halves[side].width)
                );
                assert_eq!(
                    (quarter.y, quarter.height),
                    (halves[level].y, halves[level].height)
                );
            }
        }
    }

    #[test]
    fn every_point_of_a_small_area_is_in_one_half_and_one_quarter() {
        let area = rect(-7, 11, 7, 5);
        let groups: [&[Placement]; 3] = [
            &[Placement::LeftHalf, Placement::RightHalf],
            &[Placement::TopHalf, Placement::BottomHalf],
            &[
                Placement::TopLeft,
                Placement::TopRight,
                Placement::BottomLeft,
                Placement::BottomRight,
            ],
        ];
        for group in groups {
            let parts = frames(group, area);
            for x in (area.x - 2)..(area.x + area.width + 2) {
                for y in (area.y - 2)..(area.y + area.height + 2) {
                    let count = parts.iter().filter(|part| holds(**part, x, y)).count();
                    assert_eq!(count, usize::from(holds(area, x, y)), "({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn center_keeps_the_size_and_sits_in_the_middle() {
        for area in work_areas() {
            let size = (area.width.min(3), area.height.min(2));
            let frame = placement_frame(Placement::Center, area, size);
            assert_eq!((frame.width, frame.height), size);
            let before = (frame.x - area.x, frame.y - area.y);
            let after = (
                area.x + area.width - (frame.x + frame.width),
                area.y + area.height - (frame.y + frame.height),
            );
            // The spare room is split evenly, the odd unit going after.
            assert!(after.0 - before.0 == 0 || after.0 - before.0 == 1);
            assert!(after.1 - before.1 == 0 || after.1 - before.1 == 1);
        }
        let area = rect(100, 200, 1000, 800);
        assert_eq!(placement_frame(Placement::Center, area, (1000, 800)), area);
    }

    #[test]
    fn center_shrinks_a_window_too_large_for_the_work_area() {
        let area = rect(-50, 20, 800, 600);
        assert_eq!(
            placement_frame(Placement::Center, area, (1200, 400)),
            rect(-50, 120, 800, 400)
        );
        assert_eq!(
            placement_frame(Placement::Center, area, (300, 900)),
            rect(200, 20, 300, 600)
        );
        assert_eq!(placement_frame(Placement::Center, area, (4000, 4000)), area);
        // A size that makes no sense is an empty window in the middle.
        assert_eq!(
            placement_frame(Placement::Center, area, (-5, -5)),
            rect(350, 320, 0, 0)
        );
    }

    #[test]
    fn only_center_reads_the_size() {
        let area = rect(-7, 11, 1513, 879);
        for placement in PLACEMENTS {
            if placement == Placement::Center {
                continue;
            }
            assert_eq!(
                placement_frame(placement, area, (1, 1)),
                placement_frame(placement, area, (4000, 3000)),
                "{placement:?}"
            );
        }
    }

    #[test]
    fn an_empty_or_inside_out_work_area_gives_empty_frames() {
        for area in [rect(5, 6, 0, 0), rect(5, 6, -100, -40)] {
            for placement in PLACEMENTS {
                let frame = placement_frame(placement, area, (640, 480));
                assert_eq!(frame, rect(5, 6, 0, 0), "{placement:?}");
            }
        }
        // Nothing overflows at the far corner either.
        let far = rect(i32::MAX - 1, i32::MAX - 1, i32::MAX, i32::MAX);
        for placement in PLACEMENTS {
            let _ = placement_frame(placement, far, (i32::MAX, i32::MAX));
        }
    }

    #[test]
    fn a_front_app_prints_its_pid_only() {
        let app = FrontApp {
            pid: 4321,
            id: Some(format!("com.example.{MARKER}")),
        };
        let printed = format!("{app:?}");
        assert_eq!(printed, "FrontApp { pid: 4321, .. }");
        // Through the types that hold one, too.
        let press = HotkeySignal::Pressed(
            HotkeyId::Open,
            PressContext {
                cursor: Some((10, 20)),
                front: Some(app),
                button_down: false,
            },
        );
        let printed = format!("{press:?} {press:#?}");
        assert!(printed.contains("4321"));
        assert!(!printed.contains(MARKER));
    }

    #[test]
    fn nothing_typed_or_picked_is_printed() {
        let path = PathBuf::from(format!("/Users/max/{MARKER}.app"));
        let printed = [
            format!(
                "{:?}",
                OpenTarget::App {
                    path: &path,
                    id: Some(MARKER),
                }
            ),
            format!("{:?}", OpenTarget::Path(&path)),
            format!("{:?}", OpenTarget::Url(MARKER)),
            format!("{:?}", ProgramRequest::AppleScript(MARKER)),
            format!("{:?}", ProgramRequest::Shell(MARKER)),
            format!("{:?}", ProgramRequest::AppleShortcut(MARKER)),
            format!("{:?}", ProgramRequest::KeyboardMaestro(MARKER)),
            format!("{:?}", ProgramRequest::Speak(MARKER)),
            format!(
                "{:#?}",
                SystemPlan::Program(Program {
                    program: path.clone(),
                    args: vec![MARKER.into(), "-e".into()],
                    stdin: Some(MARKER.to_owned()),
                })
            ),
            format!(
                "{:#?}",
                Picked {
                    path: path.clone(),
                    folder: false,
                    app: Some(AppIdentity {
                        id: MARKER.to_owned(),
                        name: MARKER.to_owned(),
                    }),
                }
            ),
            format!(
                "{:?}",
                AppIdentity {
                    id: MARKER.to_owned(),
                    name: MARKER.to_owned(),
                }
            ),
        ];
        for line in &printed {
            assert!(!line.is_empty());
            assert!(!line.contains(MARKER), "{line}");
        }
        assert_eq!(printed[0], "App");
        assert_eq!(printed[3], "AppleScript");
        assert_eq!(
            format!(
                "{:?}",
                Program {
                    program: path,
                    args: vec![MARKER.into(), "-e".into()],
                    stdin: None,
                }
            ),
            "Program { args: 2, stdin: false, .. }"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn everything_is_unsupported_off_macos() {
        use crate::keys::KeyCode;

        const UNSUPPORTED: PlatformError = PlatformError::Unsupported;
        assert!(matches!(hotkey_monitor(), Err(PlatformError::Unsupported)));
        assert!(matches!(pointer_watch(), Err(PlatformError::Unsupported)));
        assert_eq!(input_access(), InputAccess::NotNeeded);
        assert_eq!(request_input_access(false), Err(UNSUPPORTED));
        assert_eq!(request_input_access(true), Err(UNSUPPORTED));
        assert_eq!(held_modifiers(), Modifiers::NONE);
        assert_eq!(local_date_time(), None);

        let chord = Chord {
            key: KeyCode::KeyG,
            mods: Modifiers::CTRL,
        };
        assert_eq!(send_chord(chord), Err(UNSUPPORTED));
        assert_eq!(send_char_chord('c', Modifiers::META), Err(UNSUPPORTED));
        let cancel = AtomicBool::new(false);
        assert_eq!(type_text("hello", &cancel), Err(UNSUPPORTED));
        assert_eq!(clipboard_text(), Err(UNSUPPORTED));
        assert_eq!(clipboard_change_count(), Err(UNSUPPORTED));
        assert_eq!(clear_clipboard(), Err(UNSUPPORTED));

        let path = Path::new("/Applications/Notes.app");
        for target in [
            OpenTarget::App {
                path,
                id: Some("com.example.notes"),
            },
            OpenTarget::App { path, id: None },
            OpenTarget::Path(path),
            OpenTarget::Url("https://example.com/"),
        ] {
            assert_eq!(open_target(target), Err(UNSUPPORTED));
        }
        for request in [
            ProgramRequest::AppleScript("beep"),
            ProgramRequest::Shell("true"),
            ProgramRequest::AppleShortcut("Morning"),
            ProgramRequest::KeyboardMaestro("Tidy"),
            ProgramRequest::Speak("hello"),
        ] {
            assert_eq!(program_for(request), Err(UNSUPPORTED));
        }
        // No process is touched: there is nothing to end here.
        kill_group(0);
        for control in [
            SystemControl::SleepDisplay,
            SystemControl::LockScreen,
            SystemControl::Screensaver,
            SystemControl::MissionControl,
            SystemControl::ToggleMute,
            SystemControl::VolumeUp,
            SystemControl::VolumeDown,
            SystemControl::ToggleDarkMode,
            SystemControl::ScreenshotArea,
        ] {
            assert_eq!(system_plan(control), Err(UNSUPPORTED));
        }
        for placement in PLACEMENTS {
            assert_eq!(place_window(1, placement), Err(UNSUPPORTED));
        }
        give_back_front();
        assert!(!ensure_front(1, Duration::from_secs(5)));
        for kind in [PickKind::App, PickKind::FileOrFolder] {
            assert_eq!(pick_target(kind, None), Err(UNSUPPORTED));
            assert_eq!(pick_target(kind, Some(1)), Err(UNSUPPORTED));
        }
    }
}
