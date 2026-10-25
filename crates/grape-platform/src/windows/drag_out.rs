//! Dragging files out of the float shelf (`docs/PLATFORM_WINDOWS.md` →
//! Float): an OLE drag into another app, a folder or the Desktop.
//!
//! The drag only ever offers copy or link, never move (golden rule 2), so
//! no target is ever told it may take a file away. Its data object is
//! Grape's own ([`ShelfData`]). It serves two formats itself: the files as
//! `CF_HDROP` (Copy's block) and `Preferred DropEffect` = copy, each
//! `GetData` a fresh `HGLOBAL` copy, and neither can be replaced. It never
//! serves a shell ID list (`Shell IDList Array`, `Shell Object Offsets`)
//! and never stores one: asked for one it fails with `DV_E_FORMATETC`,
//! whatever anyone tried to store. So a target that acts on such lists by
//! itself, as a Recycle Bin may by deleting what it's given, has nothing to
//! act on, and the files may come from any number of folders. Every other
//! format a caller stores with `SetData` (the drag image, a target's
//! `Performed DropEffect`, a drop description) is kept in an empty shell
//! data object (`SHCreateDataObject` with no items), and only the formats
//! stored that way are served back or listed: that object can't be the
//! data object itself, or even be asked for formats nobody stored, because
//! even empty it answers for a shell ID list (and maybe file names). `SHDoDragDrop` runs the drag with the shell's own drop
//! source and its generic drag image.
//!
//! The drag is a modal loop on the calling thread, which must own the
//! shelf window: Tauri's main thread, where OLE runs. Nothing else on that
//! thread runs until it ends. A watchdog thread ends a drag that missed the
//! button's release (it can then wedge the whole app): once no button has
//! been down for 3 s, it sends Esc once, to the window holding the drag
//! thread's mouse capture or, with no capture, to the drag thread's queue.

use core::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ::windows::core::{implement, Error, Ref, BOOL, HRESULT};
use ::windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_E_CONCURRENT_DRAG_ATTEMPTED, DV_E_DVASPECT, DV_E_FORMATETC,
    DV_E_TYMED, E_ACCESSDENIED, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_UNEXPECTED, HWND, LPARAM,
    OLE_E_ADVISENOTSUPPORTED, S_OK, WPARAM,
};
use ::windows::Win32::System::Com::{
    CoTaskMemFree, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
    DATADIR_GET, DATADIR_SET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use ::windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use ::windows::Win32::System::Ole::{
    IDropSource, OleInitialize, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
};
use ::windows::Win32::System::Threading::GetCurrentThreadId;
use ::windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use ::windows::Win32::UI::Shell::{
    SHCreateDataObject, SHCreateStdEnumFmtEtc, SHDoDragDrop, CFSTR_SHELLIDLIST,
    CFSTR_SHELLIDLISTOFFSET,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    GetGUIThreadInfo, PostMessageW, PostThreadMessageW, GUITHREADINFO, WM_KEYDOWN, WM_KEYUP,
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
    /// or nothing there any more. With [`DragOutcome::ButtonUp`], empty if
    /// the button was already up before any path was looked at, else the
    /// paths left out by the time it was found up.
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

/// `Preferred DropEffect` as [`ShelfData`] serves it: copy.
const PREFERRED_COPY: [u8; 4] = DROPEFFECT_COPY.0.to_le_bytes();

/// The most formats read from the store's list, so a broken list can't
/// keep [`ShelfData`]'s own going forever.
const MAX_STORED_FORMATS: usize = 256;

/// Drags `paths` out of the shelf window `hwnd`: an OLE drag that offers
/// copy or link, never move. Blocks until the user drops or cancels; the
/// files stay where they are and on the shelf either way.
///
/// Checks first, in order: the calling thread must own `hwnd`; OLE is
/// initialized on it if needed; a mouse button must still be down (else
/// [`DragOutcome::ButtonUp`] at once, before any path is looked at); paths
/// that aren't absolute or where nothing is are skipped, and with none left
/// it's [`DragOutcome::NothingToDrag`]. Looking at the paths can take a
/// while (a network drive that doesn't answer), so the button is read once
/// more right before the drag starts: if it's up by then, it's
/// [`DragOutcome::ButtonUp`] with the paths skipped so far.
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
    // The paths may have taken a while: a drag started with the button up
    // would drop at once, wherever the pointer is now.
    if !button_down() {
        return Ok(DragOut {
            outcome: DragOutcome::ButtonUp,
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

/// The drag's data object for `files`: a [`ShelfData`] over an empty
/// shell data object.
fn data_object(files: &[PathBuf]) -> Result<IDataObject, PlatformError> {
    // SAFETY: no folder, no items and no inner object: an empty data
    // object that holds what callers store in it with SetData.
    let store: IDataObject = unsafe { SHCreateDataObject(None, None, None::<&IDataObject>) }
        .map_err(|err| os_err("SHCreateDataObject", &err))?;
    let data = ShelfData {
        files: drop_files(files),
        preferred: format_id(preferred_drop_effect()?)?,
        id_lists: id_list_formats()?,
        store,
        stored: Mutex::new(Vec::new()),
    };
    Ok(data.into())
}

/// The ids of the shell ID list formats: `Shell IDList Array` and `Shell
/// Object Offsets`.
fn id_list_formats() -> Result<[u16; 2], PlatformError> {
    let mut ids = [0; 2];
    for (id, name) in ids
        .iter_mut()
        .zip([CFSTR_SHELLIDLIST, CFSTR_SHELLIDLISTOFFSET])
    {
        // SAFETY: both names are static, NUL-terminated strings.
        let registered = unsafe { RegisterClipboardFormatW(name) };
        if registered == 0 {
            return Err(os_err("RegisterClipboardFormatW", &Error::from_thread()));
        }
        *id = format_id(registered)?;
    }
    Ok(ids)
}

/// A registered clipboard format's id, as a `FORMATETC` holds it.
fn format_id(registered: u32) -> Result<u16, PlatformError> {
    u16::try_from(registered).map_err(|_| PlatformError::Os {
        context: "RegisterClipboardFormatW",
        code: E_UNEXPECTED.0,
    })
}

/// `format` as content in movable memory, with no target device: the
/// only way [`ShelfData`] serves its own formats.
fn hglobal_format(format: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// `S_OK` if `format` asks for one of [`ShelfData`]'s own formats the way
/// it's served (content, in movable memory), else why not.
fn served_as_asked(format: &FORMATETC) -> HRESULT {
    if format.dwAspect != DVASPECT_CONTENT.0 {
        DV_E_DVASPECT
    } else if format.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
        DV_E_TYMED
    } else {
        S_OK
    }
}

/// A failure with `code` and nothing else: no error info of this thread's
/// rides along.
fn fail(code: HRESULT) -> Error {
    Error::from_hresult(code)
}

/// The shelf drag's data object. It serves the files (`CF_HDROP`) and
/// `Preferred DropEffect` = copy itself, refuses the shell ID list formats
/// outright, and keeps every other format a caller stores in `store`,
/// serving back only those (module docs).
///
/// Not agile: `store` was made on the drag's thread, so calls from other
/// apartments come through COM's proxies, on that thread.
#[implement(IDataObject, Agile = false)]
struct ShelfData {
    /// The files, as a `CF_HDROP` block.
    files: Vec<u8>,
    /// The id of the `Preferred DropEffect` format.
    preferred: u16,
    /// The ids of the shell ID list formats: never served, never stored.
    id_lists: [u16; 2],
    /// An empty shell data object that keeps what callers store.
    store: IDataObject,
    /// The formats `store` took from callers: the only ones it may serve
    /// or list.
    stored: Mutex<Vec<u16>>,
}

/// What a format is to [`ShelfData`].
enum Kind<'a> {
    /// One it serves itself, with its bytes.
    Served(&'a [u8]),
    /// A shell ID list: refused, whatever the call.
    IdList,
    /// One a caller stored: the store's.
    Stored,
    /// Anything else: nobody stored it, so it isn't there.
    Absent,
}

impl ShelfData {
    fn kind(&self, format: u16) -> Kind<'_> {
        if format == CF_HDROP.0 {
            Kind::Served(&self.files)
        } else if format == self.preferred {
            Kind::Served(&PREFERRED_COPY)
        } else if self.id_lists.contains(&format) {
            Kind::IdList
        } else if self.stored().contains(&format) {
            Kind::Stored
        } else {
            Kind::Absent
        }
    }

    /// The formats callers stored so far. The lock guards a plain list, so
    /// a panic in an earlier holder leaves nothing broken.
    fn stored(&self) -> MutexGuard<'_, Vec<u16>> {
        self.stored.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The formats the store lists. One with a target device is left out,
    /// its device freed: drag data never has one.
    fn stored_formats(&self) -> Vec<FORMATETC> {
        let mut formats = Vec::new();
        // SAFETY: a plain call on the live store.
        let Ok(listed) = (unsafe { self.store.EnumFormatEtc(DATADIR_GET.0 as u32) }) else {
            return formats;
        };
        for _ in 0..MAX_STORED_FORMATS {
            let mut next = [FORMATETC::default()];
            let mut fetched = 0_u32;
            // SAFETY: room for one FORMATETC and a writable count, both on
            // this stack frame.
            let got = unsafe { listed.Next(&mut next, Some(&raw mut fetched)) };
            if got.is_err() || fetched == 0 {
                break;
            }
            let [format] = next;
            if format.ptd.is_null() {
                formats.push(format);
            } else {
                // SAFETY: a listed FORMATETC's target device was allocated
                // with CoTaskMemAlloc for the caller, who frees it once.
                unsafe { CoTaskMemFree(Some(format.ptd.cast_const().cast::<c_void>())) };
            }
            if got != S_OK {
                break;
            }
        }
        formats
    }
}

#[allow(non_snake_case)]
impl IDataObject_Impl for ShelfData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> ::windows::core::Result<STGMEDIUM> {
        // SAFETY: the caller's FORMATETC, valid for this call, or null.
        let Some(format) = (unsafe { pformatetcin.as_ref() }) else {
            return Err(fail(E_INVALIDARG));
        };
        match self.kind(format.cfFormat) {
            Kind::Served(bytes) => {
                let asked = served_as_asked(format);
                if asked.is_err() {
                    return Err(fail(asked));
                }
                // A fresh block every time, the caller's to free: the
                // medium has no release object, so ReleaseStgMedium frees
                // the block itself.
                let memory = global_copy(bytes).map_err(|_| fail(E_OUTOFMEMORY))?;
                Ok(STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: memory },
                    pUnkForRelease: ManuallyDrop::new(None),
                })
            }
            Kind::IdList | Kind::Absent => Err(fail(DV_E_FORMATETC)),
            // SAFETY: the caller's FORMATETC, passed on unchanged within
            // this call.
            Kind::Stored => unsafe { self.store.GetData(pformatetcin) },
        }
    }

    fn GetDataHere(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *mut STGMEDIUM,
    ) -> ::windows::core::Result<()> {
        // SAFETY: as in GetData.
        let Some(format) = (unsafe { pformatetc.as_ref() }) else {
            return Err(fail(E_INVALIDARG));
        };
        match self.kind(format.cfFormat) {
            // Grape's own formats only come in blocks of its own (GetData);
            // a shell ID list never comes at all.
            Kind::Served(_) | Kind::IdList | Kind::Absent => Err(fail(DV_E_FORMATETC)),
            // SAFETY: the caller's FORMATETC and medium, passed on
            // unchanged within this call.
            Kind::Stored => unsafe { self.store.GetDataHere(pformatetc, pmedium) },
        }
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        // SAFETY: as in GetData.
        let Some(format) = (unsafe { pformatetc.as_ref() }) else {
            return E_INVALIDARG;
        };
        match self.kind(format.cfFormat) {
            Kind::Served(_) => served_as_asked(format),
            Kind::IdList | Kind::Absent => DV_E_FORMATETC,
            // SAFETY: as in GetData.
            Kind::Stored => unsafe { self.store.QueryGetData(pformatetc) },
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        // SAFETY: the caller's FORMATETC, valid for this call, or null. It's
        // copied before `out` is borrowed, in case both are the same.
        let format = unsafe { pformatectin.as_ref() }.copied();
        // SAFETY: the caller's writable FORMATETC, valid for this call, or
        // null.
        let Some(out) = (unsafe { pformatetcout.as_mut() }) else {
            return E_INVALIDARG;
        };
        if let Some(format) = format {
            *out = format;
        }
        out.ptd = ptr::null_mut();
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> ::windows::core::Result<()> {
        // SAFETY: as in GetData.
        let Some(format) = (unsafe { pformatetc.as_ref() }) else {
            return Err(fail(E_INVALIDARG));
        };
        // A refusal leaves the medium with the caller, as SetData's
        // contract says.
        match self.kind(format.cfFormat) {
            // The files and the copy are fixed.
            Kind::Served(_) => Err(fail(E_ACCESSDENIED)),
            // Never stored, so never served.
            Kind::IdList => Err(fail(DV_E_FORMATETC)),
            Kind::Stored | Kind::Absent => {
                // SAFETY: the caller's FORMATETC and medium, passed on
                // unchanged within this call: the store takes the medium as
                // `frelease` says when it accepts it, and leaves it with the
                // caller when it doesn't.
                unsafe { self.store.SetData(pformatetc, pmedium, frelease.as_bool()) }?;
                let mut stored = self.stored();
                if !stored.contains(&format.cfFormat) {
                    stored.push(format.cfFormat);
                }
                Ok(())
            }
        }
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> ::windows::core::Result<IEnumFORMATETC> {
        if dwdirection == DATADIR_SET.0 as u32 {
            return Err(fail(E_NOTIMPL));
        }
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(fail(E_INVALIDARG));
        }
        let mut formats = vec![hglobal_format(CF_HDROP.0), hglobal_format(self.preferred)];
        formats.extend(
            self.stored_formats()
                .into_iter()
                .filter(|format| matches!(self.kind(format.cfFormat), Kind::Stored)),
        );
        // SAFETY: FORMATETCs with no target device, which the enumerator
        // copies.
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<'_, IAdviseSink>,
    ) -> ::windows::core::Result<u32> {
        Err(fail(OLE_E_ADVISENOTSUPPORTED))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> ::windows::core::Result<()> {
        Err(fail(OLE_E_ADVISENOTSUPPORTED))
    }

    fn EnumDAdvise(&self) -> ::windows::core::Result<IEnumSTATDATA> {
        Err(fail(OLE_E_ADVISENOTSUPPORTED))
    }
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
/// down for [`NO_BUTTON_FOR`], it sends Esc once, to the window holding the
/// drag thread's mouse capture or, with no capture, to the drag thread's
/// queue. Stops when dropped.
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
    loop {
        thread::park_timeout(WATCH_EVERY);
        if stop.load(Ordering::Acquire) {
            break;
        }
        if primary_button_down() {
            last_down = Instant::now();
        } else if !sent && last_down.elapsed() >= NO_BUTTON_FOR {
            sent = true;
            let (to, escape) = match capture_window(drag_thread) {
                Some(capture) => ("the window holding the capture", send_escape(capture)),
                None => ("the drag thread", post_escape(drag_thread)),
            };
            match escape {
                Ok(()) => log::warn!(
                    "drag out: no button down for 3 s, so Esc went to {to} to end the drag"
                ),
                Err(err) => log::warn!(
                    "drag out: no button down for 3 s, but Esc couldn't be sent to {to} ({:#010x})",
                    err.code().0
                ),
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

/// An Esc press's `lParam`: a repeat count of 1.
const ESC_DOWN: LPARAM = LPARAM(1);

/// An Esc release's `lParam`: a repeat count of 1 and the previous-state
/// and transition bits, as the keyboard would send.
const ESC_UP: LPARAM = LPARAM(0xC000_0001);

/// Posts an Esc press and release to `window`, which OLE's drag loop reads
/// as "cancel".
fn send_escape(window: HWND) -> Result<(), Error> {
    let key = WPARAM(usize::from(VK_ESCAPE.0));
    // SAFETY: posting takes the handle by value, so a stale one only makes
    // the call fail, and the parameters are plain numbers.
    unsafe {
        PostMessageW(Some(window), WM_KEYDOWN, key, ESC_DOWN)?;
        PostMessageW(Some(window), WM_KEYUP, key, ESC_UP)
    }
}

/// Posts an Esc press and release to thread `thread`'s queue, with no
/// window, for a drag loop that has no capture window: OLE's loop reads
/// its thread's whole queue, thread messages included (as Wine's does;
/// Windows' is checked by hand).
fn post_escape(thread: u32) -> Result<(), Error> {
    let key = WPARAM(usize::from(VK_ESCAPE.0));
    // SAFETY: posting takes the thread id by value, so a thread that's gone
    // or has no queue only makes the call fail, and the parameters are
    // plain numbers.
    unsafe {
        PostThreadMessageW(thread, WM_KEYDOWN, key, ESC_DOWN)?;
        PostThreadMessageW(thread, WM_KEYUP, key, ESC_UP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    use ::windows::core::w;
    use ::windows::Win32::Foundation::{GlobalFree, HGLOBAL};
    use ::windows::Win32::System::Com::{DVASPECT_ICON, TYMED_ISTREAM};
    use ::windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use ::windows::Win32::System::Ole::{ReleaseStgMedium, DROPEFFECT_MOVE, DROPEFFECT_NONE};
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

    /// The first `len` bytes of the block `memory`, which must hold that
    /// many.
    fn read_block(memory: HGLOBAL, len: usize) -> Vec<u8> {
        // SAFETY: test-only read of a live block, locked only while it's
        // copied, and only as far as GlobalSize says it goes.
        unsafe {
            assert!(GlobalSize(memory) >= len);
            let bytes = GlobalLock(memory).cast::<u8>();
            assert!(!bytes.is_null());
            let copy = std::slice::from_raw_parts(bytes, len).to_vec();
            let _ = GlobalUnlock(memory);
            copy
        }
    }

    /// Stores `bytes` on `data` as `format_id`, in a fresh block, as any
    /// caller of SetData would, and frees the block if `data` refuses it.
    fn set_bytes(data: &IDataObject, format_id: u16, bytes: &[u8]) -> Result<(), HRESULT> {
        let memory = global_copy(bytes).unwrap();
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: memory },
            pUnkForRelease: ManuallyDrop::new(None),
        };
        // SAFETY: both structs outlive the call, and the medium holds a
        // live block. With fRelease the data object takes it on success.
        let stored = unsafe { data.SetData(&format(format_id), &medium, true) };
        if stored.is_err() {
            // SAFETY: refused, so the block is still ours; freed once.
            let _ = unsafe { GlobalFree(Some(memory)) };
        }
        stored.map_err(|err| err.code())
    }

    /// Every format `data` lists for GetData, in its order.
    fn listed(data: &IDataObject) -> Vec<u16> {
        // SAFETY: a plain call on a live data object.
        let formats = unsafe { data.EnumFormatEtc(DATADIR_GET.0 as u32) }.unwrap();
        let mut listed = Vec::new();
        loop {
            let mut next = [FORMATETC::default()];
            let mut fetched = 0_u32;
            // SAFETY: room for one FORMATETC and a writable count.
            let got = unsafe { formats.Next(&mut next, Some(&raw mut fetched)) };
            if got != S_OK || fetched != 1 {
                break;
            }
            assert!(next[0].ptd.is_null());
            listed.push(next[0].cfFormat);
            assert!(listed.len() < 1000, "the list never ends");
        }
        listed
    }

    /// The code `data` fails GetData for `format` with, if it does.
    fn get_fails_with(data: &IDataObject, format: &FORMATETC) -> Option<HRESULT> {
        // SAFETY: a valid FORMATETC that outlives the call. A medium that
        // shouldn't have come is released at once.
        unsafe {
            match data.GetData(format) {
                Ok(mut medium) => {
                    ReleaseStgMedium(&mut medium);
                    None
                }
                Err(err) => Some(err.code()),
            }
        }
    }

    thread_local! {
        /// How often [`down_then_up`] was read on this test's thread.
        static READS: Cell<u32> = const { Cell::new(0) };
    }

    /// A button that's down the first time it's read and up after.
    fn down_then_up() -> bool {
        READS.with(|reads| {
            let read = reads.get();
            reads.set(read + 1);
            read == 0
        })
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
    fn a_shell_id_list_set_on_the_data_object_is_refused_and_never_served() {
        // SAFETY: initializes OLE on the test thread; no pointers.
        unsafe { OleInitialize(None) }.unwrap();
        let scratch = Scratch::new("idlist");
        let files = [scratch.file("photo.jpg")];
        let data = data_object(&files).unwrap();
        for id in id_list_formats().unwrap() {
            assert_eq!(set_bytes(&data, id, &[0; 32]), Err(DV_E_FORMATETC));
            // SAFETY: a valid FORMATETC that outlives the call.
            assert_eq!(unsafe { data.QueryGetData(&format(id)) }, DV_E_FORMATETC);
            assert_eq!(get_fails_with(&data, &format(id)), Some(DV_E_FORMATETC));
            let mut medium = STGMEDIUM::default();
            // SAFETY: a valid FORMATETC and a writable medium, both
            // outliving the call; a refusal writes nothing.
            let here = unsafe { data.GetDataHere(&format(id), &raw mut medium) };
            assert_eq!(here.map_err(|err| err.code()), Err(DV_E_FORMATETC));
        }
        assert_eq!(read_paths(Some(&data)), files);
    }

    #[test]
    fn the_data_object_lists_its_two_formats_first_and_no_shell_id_list() {
        // SAFETY: initializes OLE on the test thread; no pointers.
        unsafe { OleInitialize(None) }.unwrap();
        let scratch = Scratch::new("listed");
        let data = data_object(&[scratch.file("photo.jpg")]).unwrap();
        let preferred = u16::try_from(preferred_drop_effect().unwrap()).unwrap();
        // What a drag image or a target would store.
        // SAFETY: a static, NUL-terminated format name.
        let private =
            u16::try_from(unsafe { RegisterClipboardFormatW(w!("Grape test listed")) }).unwrap();
        set_bytes(&data, private, &[1, 2, 3, 4]).unwrap();
        let listed = listed(&data);
        assert_eq!(listed.get(..2), Some([CF_HDROP.0, preferred].as_slice()));
        for ours in [CF_HDROP.0, preferred] {
            assert_eq!(listed.iter().filter(|&&id| id == ours).count(), 1);
        }
        // Nothing but Grape's two and what was stored: no shell ID list,
        // and none of the formats the shell object makes up (file names).
        assert!(
            listed
                .iter()
                .all(|id| [CF_HDROP.0, preferred, private].contains(id)),
            "{listed:?}"
        );
        // SAFETY: a plain call on a live data object.
        let set = unsafe { data.EnumFormatEtc(DATADIR_SET.0 as u32) };
        assert_eq!(set.err().map(|err| err.code()), Some(E_NOTIMPL));
    }

    #[test]
    fn each_get_data_hands_out_a_block_of_its_own() {
        // SAFETY: initializes OLE on the test thread; no pointers.
        unsafe { OleInitialize(None) }.unwrap();
        let scratch = Scratch::new("fresh");
        let files = [scratch.file("one/photo.jpg"), scratch.file("two/notes.txt")];
        let data = data_object(&files).unwrap();
        let preferred = u16::try_from(preferred_drop_effect().unwrap()).unwrap();
        for (id, bytes) in [
            (CF_HDROP.0, drop_files(&files)),
            (preferred, DROPEFFECT_COPY.0.to_le_bytes().to_vec()),
        ] {
            // SAFETY: valid FORMATETCs that outlive the calls. Each medium
            // is released once, the first before the second is read.
            unsafe {
                let mut first = data.GetData(&format(id)).unwrap();
                let mut second = data.GetData(&format(id)).unwrap();
                assert_eq!(first.tymed, TYMED_HGLOBAL.0 as u32);
                assert_eq!(second.tymed, TYMED_HGLOBAL.0 as u32);
                assert!(first.pUnkForRelease.is_none());
                assert!(second.pUnkForRelease.is_none());
                let (one, two) = (first.u.hGlobal, second.u.hGlobal);
                assert_ne!(one.0, two.0);
                assert_eq!(read_block(one, bytes.len()), bytes);
                ReleaseStgMedium(&mut first);
                assert_eq!(read_block(two, bytes.len()), bytes);
                ReleaseStgMedium(&mut second);
            }
        }
    }

    #[test]
    fn a_private_format_round_trips_through_the_store() {
        // SAFETY: initializes OLE on the test thread; no pointers.
        unsafe { OleInitialize(None) }.unwrap();
        let scratch = Scratch::new("private");
        let data = data_object(&[scratch.file("photo.jpg")]).unwrap();
        // SAFETY: a static, NUL-terminated format name.
        let private =
            u16::try_from(unsafe { RegisterClipboardFormatW(w!("Grape test private")) }).unwrap();
        // SAFETY: a valid FORMATETC that outlives the call.
        let before = unsafe { data.QueryGetData(&format(private)) };
        assert_eq!(before, DV_E_FORMATETC);
        // The shell object's own file-name formats aren't offered either:
        // only what a caller stored is.
        for name in [w!("FileNameW"), w!("FileName")] {
            // SAFETY: static, NUL-terminated format names.
            let id = u16::try_from(unsafe { RegisterClipboardFormatW(name) }).unwrap();
            // SAFETY: a valid FORMATETC that outlives the call.
            assert_eq!(unsafe { data.QueryGetData(&format(id)) }, DV_E_FORMATETC);
        }
        set_bytes(&data, private, &[7, 1, 2, 9]).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { data.QueryGetData(&format(private)) }, S_OK);
        assert_eq!(read_u32(&data, private), u32::from_le_bytes([7, 1, 2, 9]));
    }

    #[test]
    fn the_files_and_the_copy_are_fixed_and_come_only_as_content_in_memory() {
        // SAFETY: initializes OLE on the test thread; no pointers.
        unsafe { OleInitialize(None) }.unwrap();
        let scratch = Scratch::new("fixed");
        let files = [scratch.file("photo.jpg")];
        let other = scratch.file("other.txt");
        let data = data_object(&files).unwrap();
        let preferred = u16::try_from(preferred_drop_effect().unwrap()).unwrap();
        assert_eq!(
            set_bytes(&data, CF_HDROP.0, &drop_files(&[other])),
            Err(E_ACCESSDENIED)
        );
        assert_eq!(
            set_bytes(&data, preferred, &DROPEFFECT_MOVE.0.to_le_bytes()),
            Err(E_ACCESSDENIED)
        );
        assert_eq!(read_paths(Some(&data)), files);
        assert_eq!(read_u32(&data, preferred), DROPEFFECT_COPY.0);
        for id in [CF_HDROP.0, preferred] {
            let stream = FORMATETC {
                tymed: TYMED_ISTREAM.0 as u32,
                ..format(id)
            };
            let icon = FORMATETC {
                dwAspect: DVASPECT_ICON.0,
                ..format(id)
            };
            // SAFETY: valid FORMATETCs that outlive the calls.
            unsafe {
                assert_eq!(data.QueryGetData(&format(id)), S_OK);
                assert_eq!(data.QueryGetData(&stream), DV_E_TYMED);
                assert_eq!(data.QueryGetData(&icon), DV_E_DVASPECT);
            }
            assert_eq!(get_fails_with(&data, &stream), Some(DV_E_TYMED));
            assert_eq!(get_fails_with(&data, &icon), Some(DV_E_DVASPECT));
        }
    }

    #[test]
    fn the_data_object_takes_no_advise_sinks_and_keeps_formats_canonical() {
        // SAFETY: initializes OLE on the test thread; no pointers.
        unsafe { OleInitialize(None) }.unwrap();
        let scratch = Scratch::new("rest");
        let data = data_object(&[scratch.file("photo.jpg")]).unwrap();
        let asked = format(CF_HDROP.0);
        let mut canonical = FORMATETC::default();
        // SAFETY: valid FORMATETCs that outlive the calls, and no sink.
        let (same, advise, unadvise, advised) = unsafe {
            (
                data.GetCanonicalFormatEtc(&asked, &raw mut canonical),
                data.DAdvise(&asked, 0, None::<&IAdviseSink>).err(),
                data.DUnadvise(1).err(),
                data.EnumDAdvise().err(),
            )
        };
        assert_eq!(same, DATA_S_SAMEFORMATETC);
        assert_eq!(canonical, asked);
        for refused in [advise, unadvise, advised] {
            assert_eq!(
                refused.map(|err| err.code()),
                Some(OLE_E_ADVISENOTSUPPORTED)
            );
        }
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

    /// Looking at the paths can take long enough for the button to come
    /// up: then no drag starts, and the paths left out so far are said.
    #[test]
    fn a_button_let_go_while_the_paths_are_looked_at_starts_no_drag() {
        let window = TestWindow::new();
        let scratch = Scratch::new("late");
        let file = scratch.file("here.txt");
        let gone = scratch.0.join("gone.txt");
        assert_eq!(
            drag_with(window.raw(), &[gone.clone(), file], down_then_up),
            Ok(DragOut {
                outcome: DragOutcome::ButtonUp,
                skipped: vec![gone],
            })
        );
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
