//! File actions (`docs/TOOLS_AND_ACTIONS.md` → File actions): Copy, Copy
//! Path, Terminal and Move. Share comes later; Float lives in the app
//! because it's a window.
//!
//! Only Move touches the originals, and only after the user picks a folder
//! (golden rule 2). The others read paths and nothing else.
//!
//! Every function returns [`PlatformError::Unsupported`] except on Windows.

use std::path::{Path, PathBuf};

use crate::{imp, PlatformError};

/// What a Move did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    /// The user closed the folder picker or said no to moving to another
    /// drive. Nothing moved.
    Cancelled,
    /// Every item is already in the folder the user picked. Nothing moved.
    AlreadyThere {
        /// The folder the user picked.
        to: PathBuf,
    },
    /// The move ran.
    Moved {
        /// The folder the user picked.
        to: PathBuf,
        /// The items that left their original place, in selection order.
        /// Fewer than `of` when the user skipped some (a name clash, say).
        moved: Vec<PathBuf>,
        /// How many items were asked to move.
        of: usize,
    },
}

/// Puts the files on the clipboard so pasting them in a folder makes
/// copies (Windows: `CF_HDROP` plus `Preferred DropEffect` = copy).
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows, or an OS error (for
/// example when another app holds the clipboard).
pub fn copy_files(paths: &[PathBuf]) -> Result<(), PlatformError> {
    imp::copy_files(paths)
}

/// Puts the absolute paths on the clipboard as text, one per line,
/// unquoted ([`path_lines`]).
///
/// # Errors
/// As [`copy_files`].
pub fn copy_paths(paths: &[PathBuf]) -> Result<(), PlatformError> {
    imp::copy_text(&path_lines(paths))
}

/// Opens a terminal in `dir`: Windows Terminal if it's installed, else
/// PowerShell.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows, or an OS error if no
/// terminal could start.
pub fn open_terminal(dir: &Path) -> Result<(), PlatformError> {
    imp::open_terminal(dir)
}

/// Asks the user for a folder and moves the items there with the OS's own
/// file operation, so name clashes get the usual dialog and the move can be
/// undone. Asks again before moving to another drive.
///
/// Blocks until the user is done: call it from a thread of its own.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows, or an OS error if the
/// picker or the move failed before anything moved.
pub fn move_to_chosen_folder(paths: &[PathBuf]) -> Result<MoveOutcome, PlatformError> {
    imp::move_to_chosen_folder(paths)
}

/// The line break between copied paths on this OS.
const LINE_BREAK: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// Absolute paths, one per line, unquoted: `\r\n` between them on Windows
/// and `\n` elsewhere, with no break after the last one.
pub fn path_lines(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join(LINE_BREAK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_path_has_no_line_break() {
        assert_eq!(path_lines(&[PathBuf::from("/a/b.png")]), "/a/b.png");
        assert_eq!(path_lines(&[]), "");
    }

    #[test]
    fn paths_are_one_per_line_and_unquoted() {
        let lines = path_lines(&[PathBuf::from("/a/b c.png"), PathBuf::from("/d")]);
        assert_eq!(lines, format!("/a/b c.png{LINE_BREAK}/d"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn actions_are_unsupported_off_windows() {
        let paths = [PathBuf::from("/a/b.png")];
        assert_eq!(copy_files(&paths), Err(PlatformError::Unsupported));
        assert_eq!(copy_paths(&paths), Err(PlatformError::Unsupported));
        assert_eq!(
            open_terminal(Path::new("/a")),
            Err(PlatformError::Unsupported)
        );
        assert_eq!(
            move_to_chosen_folder(&paths),
            Err(PlatformError::Unsupported)
        );
    }
}
