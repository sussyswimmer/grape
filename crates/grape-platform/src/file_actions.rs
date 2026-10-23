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
    /// Nothing moved: the user closed the folder picker, said no to moving
    /// to another drive, or cancelled the move before anything left.
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
        moved: Vec<MovedItem>,
        /// Items that may now be partly or wholly in both places: a folder
        /// still in its old place after a merge, a skip inside it or a
        /// cancel, or an item whose old place can't be read any more. Grape
        /// can't tell, so it says so instead of guessing.
        unsure: Vec<PathBuf>,
        /// How many items were asked to move. The ones in neither list
        /// stayed where they were (skipped in a name clash, say).
        of: usize,
    },
}

/// One item a Move took out of its original place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedItem {
    /// Where it was.
    pub from: PathBuf,
    /// Where it is now: its new path, or the folder it went into when the
    /// new path isn't known (the shell renamed it in a name clash, say).
    pub now: PathBuf,
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
/// unquoted. On Windows the lines are joined with `\r\n` and every path
/// goes on exactly as the file system spells it, even a name that isn't
/// valid Unicode.
///
/// # Errors
/// As [`copy_files`].
pub fn copy_paths(paths: &[PathBuf]) -> Result<(), PlatformError> {
    imp::copy_paths(paths)
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

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn actions_are_unsupported_off_windows() {
        use super::*;

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
