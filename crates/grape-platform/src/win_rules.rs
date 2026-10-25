//! The pure rules under the Windows layer: which app Terminal starts, and
//! what Share leaves out, how it spells paths for the sheet, what the
//! sheet's title says and where its anchor goes.
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

#[cfg(test)]
mod tests {
    use super::*;

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
