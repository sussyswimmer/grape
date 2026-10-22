//! File actions on Windows (`docs/PLATFORM_WINDOWS.md` → File actions).
//!
//! - Copy: `CF_HDROP` plus `Preferred DropEffect` = `DROPEFFECT_COPY` on the
//!   clipboard, so pasting in Explorer makes copies.
//! - Copy Path: `CF_UNICODETEXT`, one path per line.
//! - Terminal: `wt.exe -d <dir>`, or PowerShell when Windows Terminal isn't
//!   there.
//! - Move: `IFileOpenDialog` picks a folder, then `IFileOperation` moves the
//!   items with undo. Moving to another drive is confirmed first.
//!
//! A message-only window made for each call owns the clipboard while Grape
//! writes it: `SetClipboardData` fails when the clipboard has no owner. The
//! data stays after the window is gone, because it's real memory, not
//! delayed rendering.

use core::ffi::c_void;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use ::windows::core::{w, Error, HRESULT, HSTRING, PCWSTR};
use ::windows::Win32::Foundation::{GlobalFree, ERROR_CANCELLED, E_UNEXPECTED, HANDLE, HWND};
use ::windows::Win32::Storage::FileSystem::GetVolumePathNameW;
use ::windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use ::windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use ::windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use ::windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY};
use ::windows::Win32::UI::Shell::{
    FileOpenDialog, FileOperation, IFileOpenDialog, IFileOperation, IShellItem,
    SHCreateItemFromParsingName, COPYENGINE_E_USER_CANCELLED, DROPFILES, FOFX_ADDUNDORECORD,
    FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
    SIGDN_FILESYSPATH,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, MessageBoxW, HWND_MESSAGE, IDYES, MB_DEFBUTTON2,
    MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, WINDOW_EX_STYLE, WINDOW_STYLE,
};

use crate::file_actions::MoveOutcome;
use crate::PlatformError;

/// Tries at opening the clipboard, which another app may hold for a moment.
const OPEN_ATTEMPTS: u32 = 10;
/// Wait between those tries.
const OPEN_RETRY: Duration = Duration::from_millis(15);
/// `CREATE_NEW_CONSOLE`: PowerShell gets a window of its own instead of
/// sharing a dev build's console.
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
/// The characters PowerShell reads as a single quote: ' ‘ ’ ‚ ‛.
const SINGLE_QUOTES: [u16; 5] = [0x0027, 0x2018, 0x2019, 0x201A, 0x201B];
/// What `IFileDialog::Show` returns when the user closes the picker.
const CANCELLED: HRESULT = HRESULT::from_win32(ERROR_CANCELLED.0);
/// Room for a volume path (`C:\`, `\\server\share\`, or a mounted folder).
const VOLUME_PATH_LEN: usize = 1024;

pub(crate) fn copy_files(paths: &[PathBuf]) -> Result<(), PlatformError> {
    let files = drop_files(paths);
    // SAFETY: a static, NUL-terminated format name.
    let preferred = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
    if preferred == 0 {
        return Err(os_err("RegisterClipboardFormatW", &Error::from_thread()));
    }
    let effect = DROPEFFECT_COPY.0.to_le_bytes();
    with_clipboard(|| {
        put(u32::from(CF_HDROP.0), &files)?;
        put(preferred, &effect)
    })
}

pub(crate) fn copy_text(text: &str) -> Result<(), PlatformError> {
    let bytes = le_bytes(text.encode_utf16().chain([0]));
    with_clipboard(|| put(u32::from(CF_UNICODETEXT.0), &bytes))
}

pub(crate) fn open_terminal(dir: &Path) -> Result<(), PlatformError> {
    // wt splits its command line at `;`, so such a path goes to PowerShell.
    if !dir
        .as_os_str()
        .encode_wide()
        .any(|unit| unit == u16::from(b';'))
    {
        // Any failure (not installed, say) falls through to PowerShell.
        if Command::new("wt.exe").args(wt_args(dir)).spawn().is_ok() {
            return Ok(());
        }
    }
    match Command::new("powershell.exe")
        .args(powershell_args(dir))
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(err) => Err(PlatformError::Os {
            context: "start powershell.exe",
            code: err
                .raw_os_error()
                .map_or(E_UNEXPECTED, |code| HRESULT::from_win32(code as u32))
                .0,
        }),
    }
}

pub(crate) fn move_to_chosen_folder(paths: &[PathBuf]) -> Result<MoveOutcome, PlatformError> {
    let paths = paths.to_vec();
    // The picker needs a single-threaded COM apartment. A thread of its own
    // gets one whatever the caller's thread is.
    let mover = thread::Builder::new()
        .name("grape-move".to_owned())
        .spawn(move || with_sta(|| choose_and_move(&paths)))
        .map_err(|_| PlatformError::Os {
            context: "start the move thread",
            code: E_UNEXPECTED.0,
        })?;
    mover.join().unwrap_or(Err(PlatformError::Os {
        context: "the move thread",
        code: E_UNEXPECTED.0,
    }))
}

/// A `CF_HDROP` block: a `DROPFILES` header, then each path in UTF-16 with a
/// NUL after it, then one more NUL.
fn drop_files(paths: &[PathBuf]) -> Vec<u8> {
    let header = size_of::<DROPFILES>();
    let mut bytes = Vec::with_capacity(header);
    // pFiles: the names start right after the header.
    bytes.extend_from_slice(&(header as u32).to_le_bytes());
    // pt.x, pt.y and fNC: unused on the clipboard.
    bytes.extend_from_slice(&[0; 12]);
    // fWide: the names are UTF-16.
    bytes.extend_from_slice(&1i32.to_le_bytes());
    let names = paths
        .iter()
        .flat_map(|path| path.as_os_str().encode_wide().chain([0]))
        .chain([0]);
    bytes.extend(le_bytes(names));
    bytes
}

fn le_bytes(units: impl Iterator<Item = u16>) -> Vec<u8> {
    units.flat_map(u16::to_le_bytes).collect()
}

/// Opens the clipboard for a message-only window, empties it, runs `fill`
/// and closes it again.
fn with_clipboard(fill: impl FnOnce() -> Result<(), PlatformError>) -> Result<(), PlatformError> {
    // SAFETY: a message-only window of the system STATIC class: no window
    // procedure of ours and no creation data.
    let owner = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            PCWSTR::null(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
    .map_err(|err| os_err("CreateWindowExW", &err))?;
    let result = open_clipboard(owner).and_then(|()| {
        // SAFETY: this thread has the clipboard open.
        let filled = unsafe { EmptyClipboard() }
            .map_err(|err| os_err("EmptyClipboard", &err))
            .and_then(|()| fill());
        // SAFETY: closes the clipboard this thread opened.
        let _ = unsafe { CloseClipboard() };
        filled
    });
    // SAFETY: destroys the window this thread created above.
    let _ = unsafe { DestroyWindow(owner) };
    result
}

fn open_clipboard(owner: HWND) -> Result<(), PlatformError> {
    let mut attempt = 1;
    loop {
        // SAFETY: `owner` is a live window that this thread created.
        match unsafe { OpenClipboard(Some(owner)) } {
            Ok(()) => return Ok(()),
            Err(_) if attempt < OPEN_ATTEMPTS => {
                attempt += 1;
                thread::sleep(OPEN_RETRY);
            }
            Err(err) => return Err(os_err("OpenClipboard", &err)),
        }
    }
}

/// Copies `bytes` into movable memory and hands it to the open clipboard as
/// `format`. Once that works, the clipboard owns the memory.
fn put(format: u32, bytes: &[u8]) -> Result<(), PlatformError> {
    // SAFETY: a plain allocation.
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
        .map_err(|err| os_err("GlobalAlloc", &err))?;
    // SAFETY: `memory` is a live movable block that nobody else has.
    let target = unsafe { GlobalLock(memory) };
    if target.is_null() {
        let err = Error::from_thread();
        // SAFETY: frees the block allocated above, which is still ours.
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(os_err("GlobalLock", &err));
    }
    // SAFETY: `target` points at `bytes.len()` writable bytes that don't
    // overlap `bytes`. GlobalUnlock reports "failed" with no error once the
    // lock count reaches zero, which is the expected case.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(memory);
    }
    // SAFETY: this thread has the clipboard open and emptied it. On success
    // the system owns `memory`.
    if let Err(err) = unsafe { SetClipboardData(format, Some(HANDLE(memory.0))) } {
        // SAFETY: the clipboard refused the block, so it's still ours.
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(os_err("SetClipboardData", &err));
    }
    Ok(())
}

/// `wt -d <dir>`. The caller keeps paths with `;` away from wt.
fn wt_args(dir: &Path) -> [OsString; 2] {
    ["-d".into(), dir.as_os_str().to_owned()]
}

/// `powershell -NoExit -Command "Set-Location -LiteralPath '<dir>'"`. Inside
/// single quotes PowerShell reads ' ‘ ’ ‚ ‛ alike, and a doubled one stands
/// for itself, so each is doubled.
fn powershell_args(dir: &Path) -> [OsString; 3] {
    let mut command: Vec<u16> = "Set-Location -LiteralPath '".encode_utf16().collect();
    for unit in dir.as_os_str().encode_wide() {
        command.push(unit);
        if SINGLE_QUOTES.contains(&unit) {
            command.push(unit);
        }
    }
    command.push(u16::from(b'\''));
    [
        "-NoExit".into(),
        "-Command".into(),
        OsString::from_wide(&command),
    ]
}

/// Runs `work` with COM initialized single-threaded on this thread.
fn with_sta<T>(work: impl FnOnce() -> Result<T, PlatformError>) -> Result<T, PlatformError> {
    // SAFETY: no reserved pointer. Balanced by CoUninitialize below.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
        .ok()
        .map_err(|err| os_err("CoInitializeEx", &err))?;
    let result = work();
    // SAFETY: balances the CoInitializeEx above. `work` has dropped every
    // COM object it made.
    unsafe { CoUninitialize() };
    result
}

fn choose_and_move(sources: &[PathBuf]) -> Result<MoveOutcome, PlatformError> {
    let Some(to) = pick_folder(sources)? else {
        return Ok(MoveOutcome::Cancelled);
    };
    let sources: Vec<&Path> = sources
        .iter()
        .map(PathBuf::as_path)
        .filter(|source| !already_in(source, &to))
        .collect();
    if sources.is_empty() {
        return Ok(MoveOutcome::AlreadyThere { to });
    }
    if crosses_drives(&sources, &to) && !confirm_other_drive() {
        return Ok(MoveOutcome::Cancelled);
    }
    move_items(&sources, &to)
}

/// The folder picker, starting next to the first item. `None` when the user
/// closes it.
fn pick_folder(sources: &[PathBuf]) -> Result<Option<PathBuf>, PlatformError> {
    // SAFETY: creates the shell's file dialog on this STA thread.
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
            .map_err(|err| os_err("CoCreateInstance(FileOpenDialog)", &err))?;
    let title = HSTRING::from(picker_title(sources));
    // SAFETY: plain calls on a live dialog, with NUL-terminated strings that
    // outlive them.
    unsafe {
        let options = dialog
            .GetOptions()
            .map_err(|err| os_err("IFileDialog::GetOptions", &err))?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
            .map_err(|err| os_err("IFileDialog::SetOptions", &err))?;
        dialog
            .SetTitle(&title)
            .map_err(|err| os_err("IFileDialog::SetTitle", &err))?;
        dialog
            .SetOkButtonLabel(w!("Move here"))
            .map_err(|err| os_err("IFileDialog::SetOkButtonLabel", &err))?;
        // A start folder that can't be opened leaves the dialog's own.
        if let Some(parent) = sources.first().and_then(|source| source.parent()) {
            let parent = HSTRING::from(parent.as_os_str());
            if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(&parent, None) {
                let _ = dialog.SetFolder(&folder);
            }
        }
    }
    // SAFETY: shows the modal dialog without an owner window; it runs its
    // own message loop on this thread.
    match unsafe { dialog.Show(None) } {
        Ok(()) => {}
        Err(err) if err.code() == CANCELLED => return Ok(None),
        Err(err) => return Err(os_err("IFileDialog::Show", &err)),
    }
    // SAFETY: the dialog closed with a folder chosen.
    let item =
        unsafe { dialog.GetResult() }.map_err(|err| os_err("IFileDialog::GetResult", &err))?;
    file_system_path(&item).map(Some)
}

/// "Move photo.png to…" or "Move 3 items to…".
fn picker_title(sources: &[PathBuf]) -> String {
    match sources {
        [one] => match one.file_name() {
            Some(name) => format!("Move {} to…", name.to_string_lossy()),
            None => "Move to…".to_owned(),
        },
        many => format!("Move {} items to…", many.len()),
    }
}

fn file_system_path(item: &IShellItem) -> Result<PathBuf, PlatformError> {
    // SAFETY: asks for the item's file-system path; the shell allocates the
    // string with the COM allocator.
    let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }
        .map_err(|err| os_err("IShellItem::GetDisplayName", &err))?;
    // SAFETY: `name` is a live, NUL-terminated string from the shell. It's
    // copied out, then freed exactly once.
    let path = unsafe {
        let path = PathBuf::from(OsString::from_wide(name.as_wide()));
        CoTaskMemFree(Some(name.0.cast_const().cast::<c_void>()));
        path
    };
    Ok(path)
}

/// `true` when `source` sits directly in `folder`, so moving it there would
/// do nothing.
fn already_in(source: &Path, folder: &Path) -> bool {
    let Some(parent) = source.parent() else {
        return false;
    };
    // canonicalize gives the on-disk spelling, so case differences match.
    match (std::fs::canonicalize(parent), std::fs::canonicalize(folder)) {
        (Ok(parent), Ok(folder)) => parent == folder,
        _ => false,
    }
}

/// `true` when any item may be on another volume than `to`. A volume that
/// can't be read counts as another one, so the user is asked.
fn crosses_drives(sources: &[&Path], to: &Path) -> bool {
    let Some(target) = volume_of(to) else {
        return true;
    };
    sources
        .iter()
        .any(|source| volume_of(source).is_none_or(|volume| volume != target))
}

/// The volume path holding `path` (`C:\`, `\\server\share\`, or a mounted
/// folder), upper-cased so spellings compare equal.
fn volume_of(path: &Path) -> Option<Vec<u16>> {
    let path = HSTRING::from(path.as_os_str());
    let mut volume = vec![0u16; VOLUME_PATH_LEN];
    // SAFETY: a NUL-terminated path and a writable buffer of its full length.
    unsafe { GetVolumePathNameW(&path, &mut volume) }.ok()?;
    let len = volume.iter().position(|&unit| unit == 0)?;
    volume.truncate(len);
    Some(
        volume
            .into_iter()
            .map(|unit| match u8::try_from(unit) {
                Ok(byte) => u16::from(byte.to_ascii_uppercase()),
                Err(_) => unit,
            })
            .collect(),
    )
}

/// Asks before a move that copies to another drive and then deletes the
/// originals. No is the default.
fn confirm_other_drive() -> bool {
    // SAFETY: a modal message box with no owner and static strings.
    let answer = unsafe {
        MessageBoxW(
            None,
            w!("The folder you picked is on another drive. Moving there copies the items, then deletes the originals.\n\nMove anyway?"),
            w!("Move to another drive?"),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    answer == IDYES
}

/// Moves `sources` into `to` with the shell's file operation: its own
/// progress, its own name-clash questions, and undo.
fn move_items(sources: &[&Path], to: &Path) -> Result<MoveOutcome, PlatformError> {
    // SAFETY: creates the shell's file operation on this STA thread.
    let operation: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|err| os_err("CoCreateInstance(FileOperation)", &err))?;
    let folder = HSTRING::from(to.as_os_str());
    // SAFETY: plain calls on a live operation, with shell items made here
    // from NUL-terminated paths.
    unsafe {
        operation
            .SetOperationFlags(FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR | FOFX_ADDUNDORECORD)
            .map_err(|err| os_err("IFileOperation::SetOperationFlags", &err))?;
        let folder: IShellItem = SHCreateItemFromParsingName(&folder, None)
            .map_err(|err| os_err("SHCreateItemFromParsingName", &err))?;
        for source in sources {
            let source = HSTRING::from(source.as_os_str());
            let item: IShellItem = SHCreateItemFromParsingName(&source, None)
                .map_err(|err| os_err("SHCreateItemFromParsingName", &err))?;
            operation
                .MoveItem(&item, &folder, PCWSTR::null(), None)
                .map_err(|err| os_err("IFileOperation::MoveItem", &err))?;
        }
    }
    // SAFETY: runs the queued moves on this thread.
    let performed = unsafe { operation.PerformOperations() };
    let moved: Vec<PathBuf> = sources
        .iter()
        .filter(|source| source.symlink_metadata().is_err())
        .map(|source| source.to_path_buf())
        .collect();
    match performed {
        Err(err) if moved.is_empty() && err.code() != COPYENGINE_E_USER_CANCELLED => {
            Err(os_err("IFileOperation::PerformOperations", &err))
        }
        _ => Ok(MoveOutcome::Moved {
            to: to.to_path_buf(),
            moved,
            of: sources.len(),
        }),
    }
}

fn os_err(context: &'static str, err: &Error) -> PlatformError {
    PlatformError::Os {
        context,
        code: err.code().0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use ::windows::Win32::System::DataExchange::GetClipboardData;
    use ::windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    /// The clipboard is one per session: tests that use it take turns.
    static CLIPBOARD: Mutex<()> = Mutex::new(());

    /// A scratch folder, deleted with everything in it on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("grape-actions-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"grape").unwrap();
            path
        }

        fn folder(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// Reads clipboard `format` as raw bytes, with the clipboard opened for
    /// no window.
    fn read_bytes(format: u32) -> Vec<u8> {
        // SAFETY: test-only reads of the clipboard this test just wrote;
        // the block is locked only while it's copied.
        unsafe {
            OpenClipboard(None).unwrap();
            let handle = GetClipboardData(format).unwrap();
            let memory = ::windows::Win32::Foundation::HGLOBAL(handle.0);
            let len = ::windows::Win32::System::Memory::GlobalSize(memory);
            let ptr = GlobalLock(memory).cast::<u8>();
            let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
            let _ = GlobalUnlock(memory);
            CloseClipboard().unwrap();
            bytes
        }
    }

    fn read_drop_files() -> Vec<PathBuf> {
        // SAFETY: as in `read_bytes`; DragQueryFileW only reads the block.
        unsafe {
            OpenClipboard(None).unwrap();
            let handle = GetClipboardData(u32::from(CF_HDROP.0)).unwrap();
            let drop = HDROP(handle.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let paths = (0..count)
                .map(|index| {
                    let len = DragQueryFileW(drop, index, None) as usize;
                    let mut name = vec![0u16; len + 1];
                    DragQueryFileW(drop, index, Some(&mut name));
                    PathBuf::from(OsString::from_wide(&name[..len]))
                })
                .collect();
            CloseClipboard().unwrap();
            paths
        }
    }

    #[test]
    fn the_drop_files_header_matches_the_struct() {
        let bytes = drop_files(&[PathBuf::from(r"C:\a.png"), PathBuf::from(r"D:\b c.txt")]);
        assert_eq!(size_of::<DROPFILES>(), 20);
        assert_eq!(&bytes[0..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[4..16], &[0; 12]);
        assert_eq!(&bytes[16..20], &1i32.to_le_bytes());
        let mut names = wide(r"C:\a.png");
        names.push(0);
        names.extend(wide(r"D:\b c.txt"));
        names.extend([0, 0]);
        assert_eq!(&bytes[20..], le_bytes(names.into_iter()).as_slice());
    }

    #[test]
    fn copy_puts_the_files_on_the_clipboard_as_a_copy() {
        let _turn = CLIPBOARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let scratch = Scratch::new("copy");
        let files = [scratch.file("one.png"), scratch.file("two words.txt")];
        copy_files(&files).unwrap();
        assert_eq!(read_drop_files(), files);
        // SAFETY: a static, NUL-terminated format name.
        let preferred = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
        assert_eq!(read_bytes(preferred)[..4], DROPEFFECT_COPY.0.to_le_bytes());
        // Nothing was touched.
        assert!(files.iter().all(|file| file.exists()));
    }

    #[test]
    fn copy_path_puts_one_path_per_line_on_the_clipboard() {
        let _turn = CLIPBOARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let paths = [PathBuf::from(r"C:\a b\c.png"), PathBuf::from(r"D:\e")];
        crate::file_actions::copy_paths(&paths).unwrap();
        let bytes = read_bytes(u32::from(CF_UNICODETEXT.0));
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .take_while(|&unit| unit != 0)
            .collect();
        assert_eq!(
            String::from_utf16(&units).unwrap(),
            "C:\\a b\\c.png\r\nD:\\e"
        );
    }

    #[test]
    fn wt_gets_the_folder_as_one_argument() {
        let args = wt_args(Path::new(r"C:\My Files"));
        assert_eq!(args, [OsString::from("-d"), OsString::from(r"C:\My Files")]);
    }

    #[test]
    fn powershell_doubles_every_kind_of_single_quote() {
        let args = powershell_args(Path::new("C:\\Max's \u{2019}files"));
        assert_eq!(args[0], "-NoExit");
        assert_eq!(args[1], "-Command");
        assert_eq!(
            args[2],
            "Set-Location -LiteralPath 'C:\\Max''s \u{2019}\u{2019}files'"
        );
    }

    #[test]
    fn picker_titles_name_one_item_or_count_many() {
        assert_eq!(
            picker_title(&[PathBuf::from(r"C:\a\photo.png")]),
            "Move photo.png to…"
        );
        assert_eq!(
            picker_title(&[PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")]),
            "Move 2 items to…"
        );
    }

    #[test]
    fn an_item_already_in_the_folder_is_spotted_whatever_the_case() {
        let scratch = Scratch::new("already");
        let file = scratch.file("a.txt");
        let other = scratch.folder("other");
        let shouty = PathBuf::from(scratch.0.to_string_lossy().to_uppercase());
        assert!(already_in(&file, &scratch.0));
        assert!(already_in(&file, &shouty));
        assert!(!already_in(&file, &other));
    }

    #[test]
    fn the_same_folder_is_on_the_same_drive() {
        let scratch = Scratch::new("drive");
        let file = scratch.file("a.txt");
        let other = scratch.folder("other");
        assert!(!crosses_drives(&[file.as_path()], &other));
        assert!(volume_of(&file).is_some());
    }

    #[test]
    fn move_items_moves_with_the_shell_and_reports_what_left() {
        let scratch = Scratch::new("move");
        let one = scratch.file("one.txt");
        let two = scratch.file("two.txt");
        let into = scratch.folder("into");
        let outcome = with_sta(|| move_items(&[one.as_path(), two.as_path()], &into)).unwrap();
        assert_eq!(
            outcome,
            MoveOutcome::Moved {
                to: into.clone(),
                moved: vec![one.clone(), two.clone()],
                of: 2,
            }
        );
        assert!(!one.exists() && !two.exists());
        assert!(into.join("one.txt").exists() && into.join("two.txt").exists());
    }
}
