//! The pure rules under the Windows layer: which app Terminal starts, what
//! Share leaves out, how it spells paths for the sheet, what the sheet's
//! title says and where its anchor goes, where Move's picker's owner
//! sits, what hiding the tool panel does with the foreground, when a
//! window's frame is set again after a DPI change moved it, and what the
//! `Run` keys say about launch at login.
//!
//! No Win32 here, so these build and are tested on every OS, not only on
//! Windows. The `windows` module is the only caller outside the tests.
//! Paths are read as Windows spells them (`\` or `/` between names)
//! whatever OS the tests run on.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::file_actions::{SkipReason, TerminalApp};
use crate::Rect;

/// Why Terminal or iTerm2 got Windows Terminal, for the log. The app never
/// sends either on Windows.
const NOT_ON_WINDOWS: &str =
    "Terminal and iTerm2 aren't offered on Windows; treating the choice as Windows Terminal";

/// Why a folder with `;` in its path got PowerShell, for the log.
const SEMICOLON: &str =
    "wt splits its command line at ';' and the folder's path has one, so PowerShell opens";

/// What Terminal starts on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WinTerminal {
    /// Windows Terminal (`wt -d <dir>`), and PowerShell if wt can't start.
    WindowsTerminal,
    /// PowerShell in a console of its own.
    PowerShell,
}

/// What Terminal starts on Windows for the user's `app` in `dir`, and what
/// the log says when that isn't `app`. PowerShell is PowerShell. Windows
/// Terminal is Windows Terminal unless the path has a `;`, which wt reads
/// as the end of its command; then it's PowerShell. Terminal and iTerm2
/// aren't on Windows, so they're Windows Terminal, the default.
pub(crate) fn terminal_plan(
    app: TerminalApp,
    dir: &Path,
) -> (WinTerminal, &'static [&'static str]) {
    // `;` is ASCII, so it's one byte of the encoded form (WTF-8 on
    // Windows) and never part of another character.
    let semicolon = dir.as_os_str().as_encoded_bytes().contains(&b';');
    match (app, semicolon) {
        (TerminalApp::PowerShell, _) => (WinTerminal::PowerShell, &[]),
        (TerminalApp::WindowsTerminal, false) => (WinTerminal::WindowsTerminal, &[]),
        (TerminalApp::WindowsTerminal, true) => (WinTerminal::PowerShell, &[SEMICOLON]),
        (TerminalApp::Terminal | TerminalApp::ITerm2, false) => {
            (WinTerminal::WindowsTerminal, &[NOT_ON_WINDOWS])
        }
        (TerminalApp::Terminal | TerminalApp::ITerm2, true) => {
            (WinTerminal::PowerShell, &[NOT_ON_WINDOWS, SEMICOLON])
        }
    }
}

/// File types `StorageFile` can't represent (its docs), so the share
/// sheet can't take them: shortcuts of three kinds.
const SHORTCUT_TYPES: [&[u8]; 3] = [b"lnk", b"url", b"wsh"];

/// The share anchor's side, in logical pixels.
const ANCHOR_SIDE: f64 = 32.0;

/// Why Share leaves `path` out, given whether it's a folder and whether
/// anything is there (both following links), or `None` when it goes on to
/// be looked up. A folder is a folder even with a shortcut's name, and a
/// shortcut that's gone is missing.
pub(crate) fn share_skip(path: &Path, is_dir: bool, exists: bool) -> Option<SkipReason> {
    if is_dir {
        Some(SkipReason::Folder)
    } else if !exists {
        Some(SkipReason::Missing)
    } else if is_shortcut(path) {
        Some(SkipReason::Unshareable)
    } else {
        None
    }
}

/// `true` for a `.lnk`, `.url` or `.wsh` file, in any case.
fn is_shortcut(path: &Path) -> bool {
    let name = last_name(path.as_os_str().as_encoded_bytes());
    match name.iter().rposition(|&byte| byte == b'.') {
        Some(dot) => {
            let extension = &name[dot + 1..];
            SHORTCUT_TYPES
                .iter()
                .any(|kind| extension.eq_ignore_ascii_case(kind))
        }
        None => false,
    }
}

/// The last name in a Windows path's encoded bytes: what follows its last
/// `\` or `/`. Both are ASCII, so they're never part of another character.
fn last_name(bytes: &[u8]) -> &[u8] {
    match bytes
        .iter()
        .rposition(|&byte| byte == b'\\' || byte == b'/')
    {
        Some(slash) => &bytes[slash + 1..],
        None => bytes,
    }
}

/// `path` without a `\\?\` verbatim prefix, for the share sheet's file
/// lookup, which isn't known to take one: `\\?\C:\a` is `C:\a`, and
/// `\\?\UNC\srv\share\a` is `\\srv\share\a`. Any other path, other verbatim
/// forms (a volume GUID, say) included, comes back as it was.
pub(crate) fn plain_path(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_encoded_bytes();
    let Some(rest) = bytes.strip_prefix(br"\\?\") else {
        return path.to_path_buf();
    };
    let (cut, unc) = match rest {
        [letter, b':', b'\\', ..] if letter.is_ascii_alphabetic() => (4, false),
        [u, n, c, b'\\', _, ..] if [*u, *n, *c].eq_ignore_ascii_case(b"UNC") => (8, true),
        _ => return path.to_path_buf(),
    };
    // SAFETY: the bytes are this path's own `as_encoded_bytes`, cut right
    // after the ASCII prefix `\\?\` or `\\?\UNC\`: a split immediately
    // after a non-empty valid UTF-8 substring, which the docs allow.
    let tail = unsafe { OsStr::from_encoded_bytes_unchecked(&bytes[cut..]) };
    if unc {
        let mut plain = OsString::from(r"\\");
        plain.push(tail);
        PathBuf::from(plain)
    } else {
        PathBuf::from(tail)
    }
}

/// The share sheet's title for the files it gets: the file's name for one,
/// "{n} files" for more. Never empty, as the sheet requires a title: a
/// name that can't be read is a space.
pub(crate) fn share_title(files: &[PathBuf]) -> String {
    match files {
        [one] => {
            let name = String::from_utf8_lossy(last_name(one.as_os_str().as_encoded_bytes()));
            if name.is_empty() {
                " ".to_owned()
            } else {
                name.into_owned()
            }
        }
        [] => " ".to_owned(),
        many => format!("{} files", many.len()),
    }
}

/// Where the share sheet's anchor goes: a square of [`ANCHOR_SIDE`]
/// logical pixels centered on (`x`, `y`), in physical pixels at `scale`. A
/// scale that isn't a positive number counts as 1.
pub(crate) fn anchor_frame(x: i32, y: i32, scale: f64) -> Rect {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    // At most 32 × a sane scale, so the cast can't overflow.
    let side = (ANCHOR_SIDE * scale.min(16.0)).round().max(1.0) as i32;
    Rect {
        x: x.saturating_sub(side / 2),
        y: y.saturating_sub(side / 2),
        width: side,
        height: side,
    }
}

/// Where the invisible owner of Move's folder picker goes: the middle of
/// `work_area` (the one under the cursor), so a picker centered on its
/// owner opens on the drop's monitor.
pub(crate) fn middle(work_area: Rect) -> (i32, i32) {
    (
        work_area.x.saturating_add(work_area.width / 2),
        work_area.y.saturating_add(work_area.height / 2),
    )
}

/// What hiding the tool panel does with the foreground, before it hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandBackPlan {
    /// The panel isn't the foreground window: only hide it.
    NotActive,
    /// The panel is in front and the recorded window is a live one of
    /// another app: hand it the foreground, then hide the panel. Never the
    /// panel itself, nor another of Grape's windows (D31).
    HandTo,
    /// The panel is in front, but nothing was recorded, or that window is
    /// gone or Grape's own: only hide it.
    NoWindow,
}

/// [`HandBackPlan`] for a panel that is (`panel_in_front`) or isn't the
/// foreground window, and the window recorded when it showed: `None` when
/// none was, `Some(true)` when it's a live window of another app,
/// `Some(false)` otherwise.
pub(crate) fn hand_back_plan(
    panel_in_front: bool,
    back_to_other_app: Option<bool>,
) -> HandBackPlan {
    match (panel_in_front, back_to_other_app) {
        (false, _) => HandBackPlan::NotActive,
        (true, Some(true)) => HandBackPlan::HandTo,
        (true, Some(false) | None) => HandBackPlan::NoWindow,
    }
}

/// How many times the overlay's frame is set again after its show while
/// reading it back finds the window elsewhere. A DPI change on the way
/// (tao rescales the window in its `WM_DPICHANGED` handler) can leave it
/// smaller or larger than the monitor, and setting the frame can raise
/// another one.
pub(crate) const OVERLAY_REFRAMES: u32 = 3;

/// What to do after reading back where a window is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reframe {
    /// It's at its frame: done.
    Fits,
    /// It's elsewhere and a try is left: set the frame again, then read it
    /// back again.
    Again,
    /// It's elsewhere and no try is left, or where it is can't be read:
    /// leave it as it is.
    Stop,
}

/// [`Reframe`] for a window read back at `now` (`None` when that failed)
/// that should be at `frame`, once its frame was set again `reframed`
/// times of at most `most`.
pub(crate) fn reframe(now: Option<Rect>, frame: Rect, reframed: u32, most: u32) -> Reframe {
    match now {
        Some(now) if now == frame => Reframe::Fits,
        Some(_) if reframed < most => Reframe::Again,
        _ => Reframe::Stop,
    }
}

/// What one `Run` key (the current user's or the machine's) holds under
/// Grape's value name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunEntry {
    /// No such value, or no `Run` key.
    Absent,
    /// A value of that name, naming this executable (`names_exe`) or
    /// another program.
    Present { names_exe: bool },
    /// The key or the value couldn't be read.
    Unreadable,
}

/// What Task Manager's record for a startup entry (`StartupApproved\Run`,
/// in one hive) says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Approval {
    /// No record, or one that says on.
    NotOff,
    /// Turned off in Task Manager → Startup apps.
    Off,
    /// The record couldn't be read.
    Unreadable,
}

/// Whether Windows still starts Grape at sign-in, from the `Run` keys'
/// value named after it, read directly: never from auto-launch's
/// `is_enabled`, which is also false while Task Manager has Grape turned
/// off, and whose `enable` would turn it back on over that choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunValue {
    /// Neither `Run` key has the value, and Task Manager has no "off" for
    /// it: launch at login is gone, as after an uninstall.
    Missing,
    /// Neither has the value, but Task Manager's record for the name says
    /// off: the user turned Grape off there before the value went.
    MissingTurnedOff,
    /// One names this executable, whether Task Manager has it on or off.
    ThisExe,
    /// One names another program (another copy of Grape, say), and none
    /// names this one.
    OtherExe,
    /// A key or value couldn't be read.
    Unreadable,
}

/// [`RunValue`] from what the current user's and the machine's `Run` keys
/// hold under the name, and their Task Manager records.
pub(crate) fn run_value_of(entries: [RunEntry; 2], approvals: [Approval; 2]) -> RunValue {
    if entries.contains(&RunEntry::Present { names_exe: true }) {
        RunValue::ThisExe
    } else if entries.contains(&RunEntry::Present { names_exe: false }) {
        RunValue::OtherExe
    } else if entries.contains(&RunEntry::Unreadable) {
        RunValue::Unreadable
    } else if approvals.contains(&Approval::Off) {
        RunValue::MissingTurnedOff
    } else if approvals.contains(&Approval::Unreadable) {
        RunValue::Unreadable
    } else {
        RunValue::Missing
    }
}

/// Whether Task Manager's record for a startup entry says off: its last
/// eight bytes (when it was turned off) aren't all zero, as auto-launch
/// reads it. A record too short for that reads as on.
pub(crate) fn approval_is_off(record: &[u8]) -> bool {
    record.len() >= 8 && record[record.len() - 8..].iter().any(|byte| *byte != 0)
}

/// Whether a `Run` value's command line starts `exe`: quoted
/// (`"C:\…\Grape.exe" --at-login`) or bare, as auto-launch writes it
/// (`C:\…\Grape.exe --at-login`), ignoring case as Windows paths do.
pub(crate) fn run_value_names(command: &str, exe: &Path) -> bool {
    let exe = exe.to_string_lossy().to_lowercase();
    let command = command.trim_start().to_lowercase();
    if exe.is_empty() {
        return false;
    }
    if let Some(quoted) = command.strip_prefix('"') {
        return quoted.split('"').next() == Some(exe.as_str()) && quoted.contains('"');
    }
    command
        .strip_prefix(exe.as_str())
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: Rect = Rect {
        x: -1920,
        y: 0,
        width: 1920,
        height: 1080,
    };

    const EXE: &str = r"C:\Users\Max\AppData\Local\Grape\Grape.exe";

    #[test]
    fn the_run_value_names_this_exe_quoted_or_bare_in_any_case() {
        let exe = Path::new(EXE);
        for command in [
            r"C:\Users\Max\AppData\Local\Grape\Grape.exe --at-login",
            r"C:\Users\Max\AppData\Local\Grape\Grape.exe",
            r#""C:\Users\Max\AppData\Local\Grape\Grape.exe" --at-login"#,
            r#"  "c:\users\max\appdata\local\grape\GRAPE.EXE""#,
            "c:\\users\\max\\appdata\\local\\grape\\grape.exe\t--at-login",
        ] {
            assert!(run_value_names(command, exe), "{command}");
        }
        for command in [
            r"C:\Program Files\Grape\Grape.exe --at-login",
            r"C:\Users\Max\AppData\Local\Grape\Grape.exe.old --at-login",
            r"C:\Users\Max\AppData\Local\Grape\Grape.exe2",
            r#""C:\Users\Max\AppData\Local\Grape\Grape.exe --at-login"#,
            r#""C:\Users\Max\AppData\Local\Grape\Grape.exe2""#,
            r"%LOCALAPPDATA%\Grape\Grape.exe --at-login",
            "",
        ] {
            assert!(!run_value_names(command, exe), "{command}");
        }
        assert!(!run_value_names("", Path::new("")));
        assert!(!run_value_names(" --at-login", Path::new("")));
    }

    #[test]
    fn task_managers_off_is_a_record_with_a_time() {
        // What Task Manager writes: 2 (on) or 3 (off), then the time it was
        // turned off, zero while it's on.
        let on = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let off = [3, 0, 0, 0, 0x70, 0x3b, 0x1c, 0x5e, 0xa4, 0x3a, 0xdb, 0x01];
        assert!(!approval_is_off(&on));
        assert!(approval_is_off(&off));
        assert!(!approval_is_off(&[]));
        assert!(!approval_is_off(&[3, 0, 0, 0, 1, 1, 1]));
        assert!(approval_is_off(&[1; 8]));
    }

    #[test]
    fn a_run_value_naming_this_exe_wins_over_everything_else() {
        let this = RunEntry::Present { names_exe: true };
        for other in [
            RunEntry::Absent,
            RunEntry::Present { names_exe: false },
            RunEntry::Unreadable,
        ] {
            for approvals in [
                [Approval::NotOff, Approval::NotOff],
                [Approval::Off, Approval::Unreadable],
            ] {
                assert_eq!(run_value_of([this, other], approvals), RunValue::ThisExe);
                assert_eq!(run_value_of([other, this], approvals), RunValue::ThisExe);
            }
        }
    }

    #[test]
    fn a_run_value_naming_another_program_is_someone_elses() {
        let other = RunEntry::Present { names_exe: false };
        for second in [RunEntry::Absent, other, RunEntry::Unreadable] {
            assert_eq!(
                run_value_of([other, second], [Approval::Off, Approval::NotOff]),
                RunValue::OtherExe
            );
            assert_eq!(
                run_value_of([second, other], [Approval::NotOff, Approval::NotOff]),
                RunValue::OtherExe
            );
        }
    }

    #[test]
    fn launch_at_login_is_missing_only_when_both_keys_say_so_and_task_manager_agrees() {
        let absent = [RunEntry::Absent, RunEntry::Absent];
        assert_eq!(
            run_value_of(absent, [Approval::NotOff, Approval::NotOff]),
            RunValue::Missing
        );
        // Turned off in Task Manager before the value went: never turned
        // back on over that.
        for approvals in [
            [Approval::Off, Approval::NotOff],
            [Approval::NotOff, Approval::Off],
            [Approval::Off, Approval::Unreadable],
        ] {
            assert_eq!(
                run_value_of(absent, approvals),
                RunValue::MissingTurnedOff,
                "{approvals:?}"
            );
        }
        for (entries, approvals) in [
            (
                [RunEntry::Unreadable, RunEntry::Absent],
                [Approval::NotOff, Approval::NotOff],
            ),
            (
                [RunEntry::Absent, RunEntry::Unreadable],
                [Approval::Off, Approval::NotOff],
            ),
            (absent, [Approval::Unreadable, Approval::NotOff]),
            (absent, [Approval::NotOff, Approval::Unreadable]),
        ] {
            assert_eq!(
                run_value_of(entries, approvals),
                RunValue::Unreadable,
                "{entries:?} {approvals:?}"
            );
        }
    }

    #[test]
    fn a_window_at_its_frame_is_left_alone() {
        for reframed in [0, 1, OVERLAY_REFRAMES] {
            assert_eq!(
                reframe(Some(MONITOR), MONITOR, reframed, OVERLAY_REFRAMES),
                Reframe::Fits,
                "{reframed}"
            );
        }
    }

    #[test]
    fn a_window_elsewhere_is_framed_again_at_most_the_allowed_times() {
        // What tao's rescale makes of the monitor's frame going from 150 %
        // to 100 %.
        let rescaled = Rect {
            width: 1280,
            height: 720,
            ..MONITOR
        };
        let moved = Rect { x: 0, ..MONITOR };
        for now in [rescaled, moved] {
            for reframed in 0..OVERLAY_REFRAMES {
                assert_eq!(
                    reframe(Some(now), MONITOR, reframed, OVERLAY_REFRAMES),
                    Reframe::Again,
                    "{now:?} {reframed}"
                );
            }
            assert_eq!(
                reframe(Some(now), MONITOR, OVERLAY_REFRAMES, OVERLAY_REFRAMES),
                Reframe::Stop,
                "{now:?}"
            );
        }
        // The shelf's and the panel's one more try.
        assert_eq!(reframe(Some(rescaled), MONITOR, 0, 1), Reframe::Again);
        assert_eq!(reframe(Some(rescaled), MONITOR, 1, 1), Reframe::Stop);
    }

    #[test]
    fn a_window_that_cant_be_read_back_is_left_alone() {
        for reframed in [0, OVERLAY_REFRAMES] {
            assert_eq!(
                reframe(None, MONITOR, reframed, OVERLAY_REFRAMES),
                Reframe::Stop,
                "{reframed}"
            );
        }
    }

    #[test]
    fn the_loop_the_rule_drives_ends_after_the_allowed_tries() {
        // A window that never takes its frame: every read finds it
        // elsewhere.
        let stuck = Rect {
            width: 1280,
            ..MONITOR
        };
        let mut reframed = 0;
        while reframe(Some(stuck), MONITOR, reframed, OVERLAY_REFRAMES) == Reframe::Again {
            reframed += 1;
        }
        assert_eq!(reframed, OVERLAY_REFRAMES);
    }

    #[test]
    fn the_pickers_owner_sits_in_the_middle_of_the_work_area() {
        assert_eq!(
            middle(Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1040,
            }),
            (960, 520)
        );
        // A monitor left of and above the primary one.
        assert_eq!(
            middle(Rect {
                x: -2560,
                y: -360,
                width: 2560,
                height: 1400,
            }),
            (-1280, 340)
        );
        assert_eq!(middle(Rect::default()), (0, 0));
        assert_eq!(
            middle(Rect {
                x: i32::MAX,
                y: i32::MAX,
                width: 2,
                height: 2,
            }),
            (i32::MAX, i32::MAX)
        );
    }

    #[test]
    fn a_panel_that_isnt_in_front_only_hides() {
        for back_to in [None, Some(false), Some(true)] {
            assert_eq!(
                hand_back_plan(false, back_to),
                HandBackPlan::NotActive,
                "{back_to:?}"
            );
        }
    }

    #[test]
    fn a_panel_in_front_hands_the_foreground_only_to_another_apps_live_window() {
        assert_eq!(hand_back_plan(true, Some(true)), HandBackPlan::HandTo);
        // Nothing recorded, or it's gone or one of Grape's own windows (the
        // panel included): nothing to hand to.
        assert_eq!(hand_back_plan(true, None), HandBackPlan::NoWindow);
        assert_eq!(hand_back_plan(true, Some(false)), HandBackPlan::NoWindow);
    }

    #[test]
    fn windows_terminal_is_tried_first_by_default() {
        assert_eq!(
            terminal_plan(TerminalApp::WindowsTerminal, Path::new(r"C:\My Files")),
            (WinTerminal::WindowsTerminal, &[][..])
        );
    }

    #[test]
    fn powershell_goes_straight_to_powershell() {
        for dir in [r"C:\My Files", r"C:\a;b"] {
            assert_eq!(
                terminal_plan(TerminalApp::PowerShell, Path::new(dir)),
                (WinTerminal::PowerShell, &[][..])
            );
        }
    }

    #[test]
    fn a_path_with_a_semicolon_never_goes_to_wt() {
        assert_eq!(
            terminal_plan(TerminalApp::WindowsTerminal, Path::new(r"C:\a;b")),
            (WinTerminal::PowerShell, &[SEMICOLON][..])
        );
    }

    #[test]
    fn the_mac_apps_get_windows_terminal_and_its_rules() {
        for app in [TerminalApp::Terminal, TerminalApp::ITerm2] {
            assert_eq!(
                terminal_plan(app, Path::new(r"C:\My Files")),
                (WinTerminal::WindowsTerminal, &[NOT_ON_WINDOWS][..])
            );
            assert_eq!(
                terminal_plan(app, Path::new(r"C:\a;b")),
                (WinTerminal::PowerShell, &[NOT_ON_WINDOWS, SEMICOLON][..])
            );
        }
    }

    #[test]
    fn share_skips_folders_then_missing_items_then_shortcuts() {
        let photo = Path::new(r"C:\Pics\photo.jpg");
        assert_eq!(share_skip(photo, false, true), None);
        assert_eq!(share_skip(photo, false, false), Some(SkipReason::Missing));
        assert_eq!(
            share_skip(Path::new(r"C:\Pics"), true, true),
            Some(SkipReason::Folder)
        );
        let link = Path::new(r"C:\Desktop\Notes.LNK");
        assert_eq!(share_skip(link, false, true), Some(SkipReason::Unshareable));
        assert_eq!(share_skip(link, false, false), Some(SkipReason::Missing));
        assert_eq!(share_skip(link, true, true), Some(SkipReason::Folder));
    }

    #[test]
    fn only_the_three_shortcut_types_are_unshareable_by_name() {
        for path in [
            r"C:\a\b.lnk",
            r"C:\a\b.url",
            r"C:\a\b.WSH",
            r"\\srv\share\x.Url",
            "C:/a/b.lnk",
        ] {
            assert!(is_shortcut(Path::new(path)), "{path}");
        }
        for path in [
            r"C:\a\b.lnk.txt",
            r"C:\a.lnk\b",
            r"C:\a\lnk",
            r"C:\a\b.lnkx",
            r"C:\a\b",
            "",
        ] {
            assert!(!is_shortcut(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn verbatim_drive_and_unc_paths_lose_their_prefix() {
        assert_eq!(
            plain_path(Path::new(r"\\?\C:\My Files\a.png")),
            PathBuf::from(r"C:\My Files\a.png")
        );
        assert_eq!(
            plain_path(Path::new(r"\\?\UNC\srv\share\a.png")),
            PathBuf::from(r"\\srv\share\a.png")
        );
        assert_eq!(
            plain_path(Path::new(r"\\?\unc\srv\share")),
            PathBuf::from(r"\\srv\share")
        );
    }

    #[test]
    fn other_paths_stay_as_they_are() {
        for path in [
            r"C:\My Files\a.png",
            r"\\srv\share\a.png",
            r"\\?\Volume{0b1c2d3e-0000-0000-0000-100000000000}\a.png",
            r"\\?\C:",
            r"\\?\UNC\",
            r"\\.\C:\a.png",
            "",
        ] {
            assert_eq!(plain_path(Path::new(path)), PathBuf::from(path), "{path}");
        }
    }

    #[test]
    fn the_title_names_one_file_or_counts_many() {
        assert_eq!(
            share_title(&[PathBuf::from(r"C:\Pics\photo 1.jpg")]),
            "photo 1.jpg"
        );
        assert_eq!(
            share_title(&[PathBuf::from(r"C:\a.txt"), PathBuf::from(r"D:\b.txt")]),
            "2 files"
        );
        assert_eq!(share_title(&[PathBuf::from(r"C:\Pics\")]), " ");
        assert_eq!(share_title(&[]), " ");
    }

    #[test]
    fn the_anchor_is_centered_on_the_point_at_the_display_scale() {
        assert_eq!(
            anchor_frame(500, 300, 1.0),
            Rect {
                x: 484,
                y: 284,
                width: 32,
                height: 32
            }
        );
        assert_eq!(
            anchor_frame(500, 300, 1.5),
            Rect {
                x: 476,
                y: 276,
                width: 48,
                height: 48
            }
        );
        // On a monitor left of or above the primary one.
        assert_eq!(
            anchor_frame(-1000, -10, 2.0),
            Rect {
                x: -1032,
                y: -42,
                width: 64,
                height: 64
            }
        );
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(anchor_frame(0, 0, bad).width, 32, "{bad}");
        }
    }
}
