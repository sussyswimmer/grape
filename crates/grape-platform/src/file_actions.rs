//! File actions (`docs/TOOLS_AND_ACTIONS.md` → File actions): Copy, Copy
//! Path, Terminal, Move and Share, plus Reveal for the toasts and Move's
//! folder picker on its own for Settings. Float lives in the app because
//! it's a window.
//!
//! Only Move touches the originals, and only after the user picks a folder
//! (golden rule 2). The others read paths and nothing else.
//!
//! Every function returns [`PlatformError::Unsupported`] except on Windows
//! and macOS. Share is the share sheet on Windows and AirDrop on macOS.

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
/// copies (Windows: `CF_HDROP` plus `Preferred DropEffect` = copy; macOS:
/// file URLs on the general pasteboard).
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows and macOS, or an OS
/// error (for example when another app holds the clipboard).
pub fn copy_files(paths: &[PathBuf]) -> Result<(), PlatformError> {
    imp::copy_files(paths)
}

/// Puts the absolute paths on the clipboard as text, one per line,
/// unquoted. On Windows the lines are joined with `\r\n` and every path
/// goes on exactly as the file system spells it, even a name that isn't
/// valid Unicode. On macOS they're joined with `\n`.
///
/// # Errors
/// As [`copy_files`].
pub fn copy_paths(paths: &[PathBuf]) -> Result<(), PlatformError> {
    imp::copy_paths(paths)
}

/// Which app Terminal opens (Settings → Terminal). Each OS offers two; one
/// it doesn't offer gets that OS's default, and the log says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalApp {
    /// Windows Terminal, or PowerShell when it can't start. Windows'
    /// default.
    WindowsTerminal,
    /// PowerShell in a console of its own (Windows).
    PowerShell,
    /// Terminal. macOS' default.
    Terminal,
    /// iTerm2 (macOS), or Terminal when it isn't installed.
    ITerm2,
}

/// Opens a terminal in `dir` with `app`. On Windows: Windows Terminal if it
/// starts, else PowerShell; or PowerShell straight away. On macOS: Terminal,
/// or iTerm2 if it's installed (else Terminal). An app this OS doesn't
/// offer gets its default (Windows Terminal, Terminal), and the log says
/// why whenever what opens isn't `app`.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows and macOS, or an OS
/// error if no terminal could start.
pub fn open_terminal(dir: &Path, app: TerminalApp) -> Result<(), PlatformError> {
    imp::open_terminal(dir, app)
}

/// Asks the user for a folder and moves the items there. Asks again before
/// moving to another drive.
///
/// On Windows the shell's own file operation moves them, so name clashes
/// get the usual dialog and the move can be undone. On macOS
/// `NSFileManager` moves them and never replaces anything: an item whose
/// name is taken in the folder stays where it was, and so does a mounted
/// volume's root. The picker and the question run on the main thread
/// there, so the caller waits for it, and the app that was in front gets
/// activation back afterwards.
///
/// Blocks until the user is done: call it from a thread of its own, and
/// never from one the main thread is waiting for.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows and macOS, or an OS
/// error if the picker or the move failed before anything moved.
pub fn move_to_chosen_folder(paths: &[PathBuf]) -> Result<MoveOutcome, PlatformError> {
    imp::move_to_chosen_folder(paths)
}

/// Asks the user for a folder, for Settings: Move's folder picker, titled
/// `title` (on macOS that's the panel's message, and its button says
/// "Choose"), opening at `start` when that's an existing folder. Returns
/// the folder's absolute path, or `Ok(None)` when the user cancels.
/// Nothing on disk changes.
///
/// `owner` is the raw handle (`HWND.0 as isize`) of the window asking, on
/// Windows: the picker is modal to it, so that window stays disabled
/// behind it until the user is done. Other OSes ignore it.
///
/// Blocks until the user is done: call it from a worker thread, never the
/// main thread or one the main thread is waiting for. On Windows the
/// picker runs its own modal loop on a thread of its own, as Move's does,
/// and the caller waits for that thread; the owner's thread (the main
/// one) must keep pumping messages meanwhile, as the picker talks to its
/// window. On macOS the panel runs on the main thread, so the caller waits
/// for the main thread; it's modal to the whole app, and Grape isn't
/// activated for it because the window asking is already in front.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows and macOS, or an OS
/// error if the picker couldn't open.
pub fn pick_folder(
    owner: Option<isize>,
    title: &str,
    start: Option<&Path>,
) -> Result<Option<PathBuf>, PlatformError> {
    imp::pick_folder(owner, title, start)
}

/// What a Share did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareOutcome {
    /// The share sheet (Windows) or AirDrop's window (macOS) opened with
    /// every item that wasn't skipped. `false` when nothing was left to
    /// share, so nothing opened.
    pub opened: bool,
    /// The items left out, in the order they were given, and why. Always
    /// empty on macOS.
    pub skipped: Vec<Skipped>,
}

/// One item a Share left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The item, as it was given.
    pub path: PathBuf,
    /// Why it was left out.
    pub why: SkipReason,
}

/// Why a Share left an item out (Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// A folder. Share sends files only: the sheet's targets don't
    /// reliably take folders.
    Folder,
    /// Nothing is there any more.
    Missing,
    /// Windows can't hand it to the sheet: a shortcut (`.lnk`, `.url`,
    /// `.wsh`), or a file the system couldn't look up for sharing in time.
    Unshareable,
}

/// Shares the files.
///
/// On Windows that's the share sheet, near `near` (the drop point in
/// global physical pixels; `None` is the cursor). Folders, missing items
/// and files Windows can't share are left out and listed in
/// [`ShareOutcome::skipped`]; when nothing is left, the sheet doesn't open.
/// The sheet belongs to a small invisible window on a thread of Grape's
/// own, and Grape doesn't take the focus to open it. This waits up to 10 s
/// for Windows to look the files up, then up to 30 s for the sheet to ask
/// for them.
///
/// On macOS it's AirDrop with every item: its window opens and the user
/// picks a device there. That runs on the main thread, so the caller waits
/// for it. `near` is ignored.
///
/// Blocks: call it from a thread of its own, and never from one the main
/// thread is waiting for.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows and macOS, or an OS
/// error when AirDrop is off or can't take these items, or the share sheet
/// couldn't open or never asked for the files.
pub fn share(paths: &[PathBuf], near: Option<(i32, i32)>) -> Result<ShareOutcome, PlatformError> {
    imp::share(paths, near)
}

/// Opens the folder holding `path` in Explorer or Finder, with `path`
/// selected. Starts the file manager and returns; nothing else runs.
///
/// # Errors
/// [`PlatformError::Unsupported`] except on Windows and macOS, or an OS
/// error if the file manager couldn't start (or, on macOS, the item is
/// gone).
pub fn reveal(path: &Path) -> Result<(), PlatformError> {
    imp::reveal(path)
}

#[cfg(test)]
mod tests {
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn actions_are_unsupported_off_windows() {
        use super::*;

        let paths = [PathBuf::from("/a/b.png")];
        assert_eq!(copy_files(&paths), Err(PlatformError::Unsupported));
        assert_eq!(copy_paths(&paths), Err(PlatformError::Unsupported));
        for app in [
            TerminalApp::WindowsTerminal,
            TerminalApp::PowerShell,
            TerminalApp::Terminal,
            TerminalApp::ITerm2,
        ] {
            assert_eq!(
                open_terminal(Path::new("/a"), app),
                Err(PlatformError::Unsupported)
            );
        }
        assert_eq!(
            move_to_chosen_folder(&paths),
            Err(PlatformError::Unsupported)
        );
        assert_eq!(share(&paths, None), Err(PlatformError::Unsupported));
        assert_eq!(share(&paths, Some((1, 2))), Err(PlatformError::Unsupported));
        assert_eq!(
            reveal(Path::new("/a/b.png")),
            Err(PlatformError::Unsupported)
        );
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn the_folder_picker_is_unsupported_off_windows_and_macos() {
        use super::*;

        assert_eq!(
            pick_folder(None, "Choose a folder", Some(Path::new("/a"))),
            Err(PlatformError::Unsupported)
        );
        assert_eq!(
            pick_folder(Some(1), "Choose a folder", None),
            Err(PlatformError::Unsupported)
        );
    }
}
