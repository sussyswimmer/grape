//! The pure rules under the Windows layer: which app Terminal starts.
//!
//! No Win32 here, so these build and are tested on every OS, not only on
//! Windows. The `windows` module is the only caller outside the tests.

use std::path::Path;

use crate::file_actions::TerminalApp;

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
}
