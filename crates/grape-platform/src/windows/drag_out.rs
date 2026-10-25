//! Dragging files out of the float shelf (`docs/PLATFORM_WINDOWS.md` →
//! Float): an OLE drag into another app, a folder or the Desktop.
//!
//! The drag only ever offers copy or link, never move (golden rule 2), so
//! no target is ever told it may take a file away. Its data object starts
//! empty (`SHCreateDataObject` with no items) and gets two formats and
//! nothing else: the files as `CF_HDROP` (Copy's block) and `Preferred
//! DropEffect` = copy. With no shell ID list on it, a target that acts on
//! such lists by itself, as a Recycle Bin may by deleting what it's given,
//! has nothing to act on, and the files may come from any number of
//! folders. `SHDoDragDrop` runs the drag with the shell's own drop source
//! and its generic drag image.
//!
//! The drag is a modal loop on the calling thread, which must own the
//! shelf window: Tauri's main thread, where OLE runs. Nothing else on that
//! thread runs until it ends. A watchdog thread ends a drag that missed the
//! button's release (it can then wedge the whole app): once no button has
//! been down for 3 s, it sends Esc to the window holding the drag thread's
//! mouse capture, once.

use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ::windows::core::Error;
use ::windows::Win32::Foundation::{
    GlobalFree, DRAGDROP_E_CONCURRENT_DRAG_ATTEMPTED, E_UNEXPECTED, HWND, LPARAM, WPARAM,
};
use ::windows::Win32::System::Com::{
    IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use ::windows::Win32::System::Ole::{
    IDropSource, OleInitialize, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
};
use ::windows::Win32::System::Threading::GetCurrentThreadId;
use ::windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use ::windows::Win32::UI::Shell::{SHCreateDataObject, SHDoDragDrop};
use ::windows::Win32::UI::WindowsAndMessaging::{
    GetGUIThreadInfo, PostMessageW, GUITHREADINFO, WM_KEYDOWN, WM_KEYUP,
};

use super::actions::{drop_files, global_copy, os_err, preferred_drop_effect};
use super::drag::primary_button_down;
use super::overlay::owned_window;
use crate::PlatformError;

/// What a drag out of the shelf did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragOut {
    /// How the drag ended.
    pub outcome: DragOutcome,
    /// The paths left out of the drag, in the order given: not absolute,
    /// or nothing there any more. Empty when the button was already up,
    /// as no path was looked at.
    pub skipped: Vec<PathBuf>,
}

/// How a drag out of the shelf ended. What a target reports is a hint:
/// some targets don't report what they did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragOutcome {
    /// Dropped, and the target says it copied the files.
    Copied,
    /// Dropped, and the target says it linked to them (made shortcuts).
    Linked,
    /// Cancelled with Esc, let go where nothing takes files, or refused by
    /// the target.
    Cancelled,
    /// No mouse button was down by the time the drag would have started,
    /// so it didn't: it would have dropped at once, wherever the pointer
    /// was.
    ButtonUp,
    /// None of the paths can be dragged (see [`DragOut::skipped`]).
    NothingToDrag,
}

/// What the drag offers: copy or link, never move (golden rule 2).
const ALLOWED: DROPEFFECT = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_LINK.0);

/// How often the watchdog reads the buttons.
const WATCH_EVERY: Duration = Duration::from_millis(100);

/// How long no button may be down before the watchdog ends the drag.
const NO_BUTTON_FOR: Duration = Duration::from_secs(3);

/// Set while a drag out runs: OLE runs one drag at a time.
static DRAGGING: AtomicBool = AtomicBool::new(false);

/// Drags `paths` out of the shelf window `hwnd`: an OLE drag that offers
/// copy or link, never move. Blocks until the user drops or cancels; the
/// files stay where they are and on the shelf either way.
///
/// Checks first, in order: the calling thread must own `hwnd`; OLE is
/// initialized on it if needed; a mouse button must still be down (else
/// [`DragOutcome::ButtonUp`] at once); paths that aren't absolute or where
/// nothing is are skipped, and with none left it's
/// [`DragOutcome::NothingToDrag`].
///
/// `hwnd` is the raw handle value (`HWND.0 as isize`). Call this on the
/// window's thread (Tauri's main thread), from a task of its own, never
/// inside a WebView2 callback such as a synchronous command: the drag loop
/// would run nested inside it.
///
/// # Errors
/// - [`PlatformError::WrongThread`] if the calling thread doesn't own the
///   window.
/// - [`PlatformError::Os`] if `hwnd` isn't a window, OLE can't start on
///   this thread, another drag out is running
///   (`DRAGDROP_E_CONCURRENT_DRAG_ATTEMPTED`), or the data object or the
///   drag fails.
pub fn drag_out(hwnd: isize, paths: &[PathBuf]) -> Result<DragOut, PlatformError> {
    drag_with(hwnd, paths, primary_button_down)
}

/// [`drag_out`] with the button read by `button_down`, so tests can check
/// everything up to the drag without ever starting one.
fn drag_with(
    hwnd: isize,
    paths: &[PathBuf],
    button_down: fn() -> bool,
) -> Result<DragOut, PlatformError> {
    owned_window(hwnd)?;
    // SAFETY: OleInitialize takes no pointers (the reserved argument must
    // be null). On a thread where OLE already runs it returns S_FALSE,
    // which is a success; it fails only if the thread is multithreaded.
    unsafe { OleInitialize(None) }.map_err(|err| os_err("OleInitialize", &err))?;
    if !button_down() {
        return Ok(DragOut {
            outcome: DragOutcome::ButtonUp,
            skipped: Vec::new(),
        });
    }
    let (files, skipped) = draggable(paths);
    if files.is_empty() {
        return Ok(DragOut {
            outcome: DragOutcome::NothingToDrag,
            skipped,
        });
    }
    let Some(_busy) = Busy::take() else {
        return Err(PlatformError::Os {
            context: "drag_out",
            code: DRAGDROP_E_CONCURRENT_DRAG_ATTEMPTED.0,
        });
    };
    let data = data_object(&files)?;
    // SAFETY: a plain query of this thread's id.
    let watchdog = Watchdog::start(unsafe { GetCurrentThreadId() });
    // SAFETY: on the window's own thread, where OLE runs (both checked
    // above), with a live data object. No window and no drop source make
    // the shell supply its generic drag image and its own drop source.
    let effect = unsafe { SHDoDragDrop(None, &data, None::<&IDropSource>, ALLOWED) };
    drop(watchdog);
    let effect = effect.map_err(|err| os_err("SHDoDragDrop", &err))?;
    Ok(DragOut {
        outcome: outcome_for(effect),
        skipped,
    })
}

/// Splits `paths` into the ones that can be dragged and the ones left
/// out: not absolute, or nothing there (or it can't be told). Folders can
/// be dragged.
fn draggable(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    paths
        .iter()
        .cloned()
        .partition(|path| path.is_absolute() && path.try_exists().unwrap_or(false))
}

/// How the drag ended, from the effect the target reported. Copy wins
/// when a target reports both.
fn outcome_for(effect: DROPEFFECT) -> DragOutcome {
    if effect.0 & DROPEFFECT_COPY.0 != 0 {
        DragOutcome::Copied
    } else if effect.0 & DROPEFFECT_LINK.0 != 0 {
        DragOutcome::Linked
    } else {
        DragOutcome::Cancelled
    }
}

/// An empty shell data object holding `files` as `CF_HDROP` and a
/// `Preferred DropEffect` of copy, and nothing else.
fn data_object(files: &[PathBuf]) -> Result<IDataObject, PlatformError> {
    // SAFETY: no folder, no items and no inner object: an empty data
    // object that takes formats through SetData.
    let data: IDataObject = unsafe { SHCreateDataObject(None, None, None::<&IDataObject>) }
        .map_err(|err| os_err("SHCreateDataObject", &err))?;
    set_global(&data, CF_HDROP.0, &drop_files(files))?;
    let preferred = u16::try_from(preferred_drop_effect()?).map_err(|_| PlatformError::Os {
        context: "RegisterClipboardFormatW",
        code: E_UNEXPECTED.0,
    })?;
    set_global(&data, preferred, &DROPEFFECT_COPY.0.to_le_bytes())?;
    Ok(data)
}

/// Copies `bytes` into movable memory and hands it to `data` as `format`.
/// Once that works, the data object owns the memory.
fn set_global(data: &IDataObject, format: u16, bytes: &[u8]) -> Result<(), PlatformError> {
    let memory = global_copy(bytes)?;
    let format = FORMATETC {
        cfFormat: format,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let medium = STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: memory },
        pUnkForRelease: ManuallyDrop::new(None),
    };
    // SAFETY: both structs outlive the call, and the medium holds a live
    // HGLOBAL. With fRelease the data object takes the memory on success.
    if let Err(err) = unsafe { data.SetData(&format, &medium, true) } {
        // SAFETY: the data object refused the block, so it's still ours.
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(os_err("IDataObject::SetData", &err));
    }
    Ok(())
}

/// Holds [`DRAGGING`] for one drag.
struct Busy;

impl Busy {
    /// `None` while another drag out runs.
    fn take() -> Option<Self> {
        (!DRAGGING.swap(true, Ordering::AcqRel)).then_some(Self)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        DRAGGING.store(false, Ordering::Release);
    }
}

/// Ends a drag that missed the button's release: once no button has been
/// down for [`NO_BUTTON_FOR`], it sends Esc to the window holding the drag
/// thread's mouse capture, once. Stops when dropped.
struct Watchdog {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Watches the drag running on thread `drag_thread`. If the watchdog's
    /// own thread can't start, the drag runs without it.
    fn start(drag_thread: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new()
            .name("grape-drag-watch".to_owned())
            .spawn({
                let stop = Arc::clone(&stop);
                move || watch(drag_thread, &stop)
            })
            .ok();
        if thread.is_none() {
            log::warn!("drag out: the watchdog couldn't start, so a missed release can't be ended");
        }
        Self { stop, thread }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

fn watch(drag_thread: u32, stop: &AtomicBool) {
    let mut last_down = Instant::now();
    let mut sent = false;
    while !stop.load(Ordering::Acquire) {
        thread::park_timeout(WATCH_EVERY);
        if primary_button_down() {
            last_down = Instant::now();
        } else if !sent && last_down.elapsed() >= NO_BUTTON_FOR {
            if let Some(capture) = capture_window(drag_thread) {
                sent = true;
                match send_escape(capture) {
                    Ok(()) => log::warn!(
                        "drag out: no button down for 3 s, so Esc went to the drag to end it"
                    ),
                    Err(err) => log::warn!(
                        "drag out: no button down for 3 s, but Esc couldn't be sent ({:#010x})",
                        err.code().0
                    ),
                }
            }
        }
    }
}

/// The window holding thread `thread`'s mouse capture, if any.
fn capture_window(thread: u32) -> Option<HWND> {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..GUITHREADINFO::default()
    };
    // SAFETY: a writable GUITHREADINFO with `cbSize` set, as the call
    // requires. A thread id that's gone only makes it fail.
    unsafe { GetGUIThreadInfo(thread, &raw mut info) }.ok()?;
    (!info.hwndCapture.is_invalid()).then_some(info.hwndCapture)
}

/// Posts an Esc press and release to `window`, which OLE's drag loop reads
/// as "cancel".
fn send_escape(window: HWND) -> Result<(), Error> {
    let key = WPARAM(usize::from(VK_ESCAPE.0));
    // SAFETY: posting takes the handle by value, so a stale one only makes
    // the call fail, and the parameters are plain numbers: a repeat count
    // of 1, and for the release also the previous-state and transition
    // bits, as the keyboard would send.
    unsafe {
        PostMessageW(Some(window), WM_KEYDOWN, key, LPARAM(1))?;
        PostMessageW(Some(window), WM_KEYUP, key, LPARAM(0xC000_0001))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ::windows::core::w;
    use ::windows::Win32::Foundation::HGLOBAL;
    use ::windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use ::windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use ::windows::Win32::System::Ole::{ReleaseStgMedium, DROPEFFECT_MOVE, DROPEFFECT_NONE};
    use ::windows::Win32::UI::Shell::CFSTR_SHELLIDLIST;
    use ::windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };

    use crate::windows::drop_target::read_paths;

    /// A scratch folder, deleted with everything in it on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("grape-drag-out-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"grape").unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A hidden top-level test window, destroyed on drop.
    struct TestWindow(HWND);

    impl TestWindow {
        fn new() -> Self {
            // SAFETY: a hidden popup of the built-in STATIC class with
            // static, NUL-terminated strings and no parent or menu.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("grape drag-out test"),
                    WS_POPUP,
                    0,
                    0,
                    32,
                    32,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap();
            Self(hwnd)
        }

        fn raw(&self) -> isize {
            self.0 .0 as isize
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: this thread's own window, destroyed once.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    fn format(format: u16) -> FORMATETC {
        FORMATETC {
            cfFormat: format,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }

    /// The first four bytes of `data`'s `format`, as a little-endian u32.
    fn read_u32(data: &IDataObject, format_id: u16) -> u32 {
        // SAFETY: test-only read of an HGLOBAL medium, locked only while
        // it's copied, then released once.
        unsafe {
            let mut medium = data.GetData(&format(format_id)).unwrap();
            assert_eq!(medium.tymed, TYMED_HGLOBAL.0 as u32);
            let memory: HGLOBAL = medium.u.hGlobal;
            let bytes = GlobalLock(memory).cast::<[u8; 4]>().read_unaligned();
            let _ = GlobalUnlock(memory);
            ReleaseStgMedium(&mut medium);
            u32::from_le_bytes(bytes)
        }
    }

    #[test]
    fn the_drag_never_offers_a_move() {
        assert_eq!(ALLOWED.0 & DROPEFFECT_MOVE.0, 0);
        assert_eq!(ALLOWED, DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_LINK.0));
    }

    #[test]
    fn the_outcome_follows_what_the_target_reports() {
        assert_eq!(outcome_for(DROPEFFECT_COPY), DragOutcome::Copied);
        assert_eq!(
            outcome_for(DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_LINK.0)),
            DragOutcome::Copied
        );
        assert_eq!(outcome_for(DROPEFFECT_LINK), DragOutcome::Linked);
        assert_eq!(outcome_for(DROPEFFECT_NONE), DragOutcome::Cancelled);
        // Never offered, so never a success either.
        assert_eq!(outcome_for(DROPEFFECT_MOVE), DragOutcome::Cancelled);
    }

    /// The shelf collects files from many drops, so they're often in
    /// several folders: the data object must carry them all, as a copy,
    /// and without a shell ID list.
    #[test]
    fn the_data_object_carries_files_from_two_folders_as_a_copy_and_nothing_else() {
        // SAFETY: initializes OLE on the test thread; no pointers.
        unsafe { OleInitialize(None) }.unwrap();
        let scratch = Scratch::new("data");
        let files = [
            scratch.file("one/photo.jpg"),
            scratch.file("two/notes and more.txt"),
        ];
        let data = data_object(&files).unwrap();
        assert_eq!(read_paths(Some(&data)), files);
        let preferred = u16::try_from(preferred_drop_effect().unwrap()).unwrap();
        assert_eq!(read_u32(&data, preferred), DROPEFFECT_COPY.0);
        // SAFETY: a static, NUL-terminated format name.
        let id_list =
            u16::try_from(unsafe { RegisterClipboardFormatW(CFSTR_SHELLIDLIST) }).unwrap();
        // SAFETY: a valid FORMATETC that outlives the call.
        assert!(unsafe { data.QueryGetData(&format(id_list)) }.is_err());
        assert!(files.iter().all(|file| file.exists()));
    }

    #[test]
    fn paths_that_are_relative_or_gone_are_left_out() {
        let scratch = Scratch::new("split");
        let file = scratch.file("here.txt");
        let folder = scratch.0.join("one");
        std::fs::create_dir_all(&folder).unwrap();
        let gone = scratch.0.join("gone.txt");
        let relative = PathBuf::from(r"relative\here.txt");
        let (files, skipped) =
            draggable(&[gone.clone(), file.clone(), relative.clone(), folder.clone()]);
        assert_eq!(files, [file, folder]);
        assert_eq!(skipped, [gone, relative]);
    }

    #[test]
    fn nothing_left_to_drag_never_starts_a_drag() {
        let window = TestWindow::new();
        let scratch = Scratch::new("nothing");
        let gone = scratch.0.join("gone.txt");
        let relative = PathBuf::from("here.txt");
        let paths = [gone.clone(), relative.clone()];
        assert_eq!(
            drag_with(window.raw(), &paths, || true),
            Ok(DragOut {
                outcome: DragOutcome::NothingToDrag,
                skipped: vec![gone, relative],
            })
        );
        assert_eq!(
            drag_with(window.raw(), &[], || true),
            Ok(DragOut {
                outcome: DragOutcome::NothingToDrag,
                skipped: Vec::new(),
            })
        );
    }

    #[test]
    fn with_the_button_up_no_drag_starts_and_no_path_is_looked_at() {
        let window = TestWindow::new();
        let scratch = Scratch::new("button");
        let file = scratch.file("here.txt");
        let button_up = Ok(DragOut {
            outcome: DragOutcome::ButtonUp,
            skipped: Vec::new(),
        });
        assert_eq!(drag_with(window.raw(), &[file], || false), button_up);
        // The real button: nobody holds one on the test runner, so this
        // can't hang. A missing path keeps even a held button from
        // starting a drag; it just fails the test.
        let gone = scratch.0.join("gone.txt");
        assert_eq!(drag_out(window.raw(), &[gone]), button_up);
    }

    #[test]
    fn a_window_of_another_thread_or_none_is_refused() {
        let window = TestWindow::new();
        let raw = window.raw();
        let result = thread::spawn(move || drag_out(raw, &[])).join().unwrap();
        assert_eq!(result, Err(PlatformError::WrongThread));
        assert!(matches!(
            drag_out(0, &[]),
            Err(PlatformError::Os {
                context: "IsWindow",
                ..
            })
        ));
    }

    #[test]
    fn only_one_drag_runs_at_a_time() {
        let first = Busy::take().unwrap();
        assert!(Busy::take().is_none());
        drop(first);
        assert!(Busy::take().is_some());
    }

    #[test]
    fn the_watchdog_finds_no_capture_and_stops_when_dropped() {
        // SAFETY: a plain query of this thread's id.
        let me = unsafe { GetCurrentThreadId() };
        assert_eq!(capture_window(me), None);
        let started = Instant::now();
        drop(Watchdog::start(me));
        assert!(started.elapsed() < NO_BUTTON_FOR);
    }
}
