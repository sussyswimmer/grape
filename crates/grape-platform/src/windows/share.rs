//! Share on Windows (`docs/PLATFORM_WINDOWS.md` → File actions): the share
//! sheet, through `IDataTransferManagerInterop`.
//!
//! [`share`] runs on the caller's thread and blocks, as on macOS:
//! 1. Folders and missing items are left out, and so are shortcuts, which
//!    `StorageFile` can't represent (`crate::win_rules::share_skip`).
//! 2. The rest are looked up as `StorageFile`s (`GetFileFromPathAsync`,
//!    all at once, 10 s for the lot) in the multithreaded apartment, each
//!    kept as an agile reference so it can cross to the share thread. A
//!    file that fails or doesn't come back in time is left out too.
//! 3. With nothing left, the sheet doesn't open.
//! 4. Otherwise the share thread gets the request, and the caller waits up
//!    to 30 s (Chromium's limit) for the sheet to ask for the files.
//!
//! The share thread ("grape-share") is Grape's own: a single-threaded
//! apartment with a message loop, started on first use and kept for the
//! app's life. It owns the sheet's anchor: a 32 × 32 logical-pixel popup
//! centered on the drop point, a tool window (no taskbar button, no
//! Alt-Tab entry), layered at alpha 1 of 255 and click-through, so it can't
//! be seen or hit. It isn't topmost, and it's shown without being
//! activated: Grape never takes the focus to open Share. Every working
//! share-sheet owner on record is a live, visible, activatable window of
//! the app's own; the toast never activates, and tao holds back every
//! event while a main-thread task runs, so the anchor is a window of its
//! own on a thread of its own. Whether Windows accepts it is checked by
//! hand, so every HRESULT on the way goes to the log.
//!
//! The `DataRequested` handler follows Chromium's rules. It runs on the
//! share thread. It's removed right after `ShowShareUIForWindow` when it
//! already ran inside that call or the call failed, otherwise at the start
//! of the handler, and never inside a synchronous invocation. It fills the
//! package (a title, which is required, `Copy`, and the files read-only),
//! calls `FailWithDisplayText` if any of that fails, and always returns
//! `Ok`.
//!
//! Once a share was served, the sheet closing may activate the anchor. It
//! then hides and hands the foreground back to the window that had it when
//! the share began, unless that one is gone or Grape's. Otherwise the
//! anchor stays where it is, invisible, until the next share moves it; it's
//! never destroyed, as a sheet may still be open on it.

use core::ffi::c_void;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ::windows::core::{factory, w, AgileReference, IUnknown, Interface, Ref, HRESULT, HSTRING};
use ::windows::ApplicationModel::DataTransfer::{
    DataPackageOperation, DataRequestedEventArgs, DataTransferManager,
};
use ::windows::Foundation::TypedEventHandler;
use ::windows::Storage::{IStorageItem, StorageFile};
use ::windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, ERROR_TIMEOUT, E_UNEXPECTED, HWND, LPARAM,
    LRESULT, POINT, RPC_E_CHANGED_MODE, WPARAM,
};
use ::windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    COINIT_MULTITHREADED,
};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::System::Threading::GetCurrentProcessId;
use ::windows::Win32::UI::Shell::IDataTransferManagerInterop;
use ::windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, IsWindow, PostMessageW,
    RegisterClassExW, SetForegroundWindow, SetLayeredWindowAttributes, SetWindowPos, ShowWindow,
    TranslateMessage, LWA_ALPHA, MSG, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WA_INACTIVE,
    WM_ACTIVATE, WM_APP, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows_collections::IIterable;

use super::actions::os_err;
use crate::file_actions::{ShareOutcome, SkipReason, Skipped};
use crate::win_rules::{anchor_frame, plain_path, share_skip, share_title};
use crate::{PlatformError, Rect};

/// How long all the file lookups may take together.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the sheet may take to ask for the files (Chromium's limit).
/// The sheet's own process can crash, and then it never asks.
const SHEET_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the share thread may take to start and make its anchor.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// The anchor's alpha, of 255: as good as invisible, but not hidden.
const ANCHOR_ALPHA: u8 = 1;

/// The anchor's window class.
const ANCHOR_CLASS: ::windows::core::PCWSTR = w!("GrapeShareAnchor");

/// What the sheet says when the package can't be filled.
const FAILED: &str = "Grape couldn't share these files.";

/// To the anchor: a request waits in [`REQUEST`].
const WM_SHARE: u32 = WM_APP + 1;

/// To the anchor: the caller of the share whose id is in wParam gave up.
const WM_EXPIRE: u32 = WM_APP + 2;

/// To the anchor: it was activated after a share, so hand the foreground
/// back (outside `WM_ACTIVATE` itself).
const WM_GIVE_BACK: u32 = WM_APP + 3;

/// The request on its way to the share thread.
static REQUEST: Mutex<Option<Request>> = Mutex::new(None);

/// The anchor's raw handle, once the share thread runs.
static ANCHOR: Mutex<Option<isize>> = Mutex::new(None);

/// One share at a time: a second one waits for the first to be served or
/// to give up.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Tells a request's expiry from a later request's.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// One share, as the share thread gets it.
struct Request {
    id: u64,
    /// Where the anchor goes, in global physical pixels.
    frame: Rect,
    payload: Payload,
}

/// What the handler needs to fill the package and answer the caller.
struct Payload {
    /// The files, in the order given.
    items: Vec<AgileReference<IStorageItem>>,
    title: String,
    /// The caller waits on the other end. Only its first answer counts.
    reply: Sender<Result<(), PlatformError>>,
}

/// A `DataRequested` registration whose handler hasn't run yet, on the
/// share thread.
struct Pending {
    id: u64,
    manager: DataTransferManager,
    token: i64,
    /// The handler's payload, taken by whichever comes first: the handler,
    /// or the expiry.
    slot: Arc<Mutex<Option<Payload>>>,
}

thread_local! {
    /// The share thread's registration still waiting for its handler.
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
    /// The window in front when the last share began (a raw handle).
    static FRONT: Cell<Option<isize>> = const { Cell::new(None) };
    /// The last share was served since the anchor was shown, so the anchor
    /// being activated is the sheet handing it the focus.
    static SERVED: Cell<bool> = const { Cell::new(false) };
}

/// Steps 1 and 2: the files going to the sheet, and the items left out.
struct Prepared {
    /// The files as storage items, in the order given.
    items: Vec<AgileReference<IStorageItem>>,
    /// Their paths, as given.
    shared: Vec<PathBuf>,
    /// The items left out, in the order given.
    skipped: Vec<Skipped>,
}

pub(crate) fn share(
    paths: &[PathBuf],
    near: Option<(i32, i32)>,
) -> Result<ShareOutcome, PlatformError> {
    // The apartment stays until the sheet has the files, so the items
    // made in it live at least that long.
    with_mta(|| {
        let Prepared {
            items,
            shared,
            skipped,
        } = prepare(paths, LOOKUP_TIMEOUT);
        if items.is_empty() {
            return Ok(ShareOutcome {
                opened: false,
                skipped,
            });
        }
        let point = near.or_else(cursor).unwrap_or((0, 0));
        let scale = crate::display_bounds_at(point.0, point.1).map_or(1.0, |display| display.scale);
        open_sheet(
            anchor_frame(point.0, point.1, scale),
            items,
            share_title(&shared),
        )?;
        Ok(ShareOutcome {
            opened: true,
            skipped,
        })
    })
}

/// Runs `work` with COM initialized multithreaded on this thread, balanced
/// afterwards. A thread already in a single-threaded apartment stays in
/// it: the lookups' callbacks don't need this thread to pump.
fn with_mta<T>(work: impl FnOnce() -> Result<T, PlatformError>) -> Result<T, PlatformError> {
    // SAFETY: no reserved pointer. Balanced by CoUninitialize below when it
    // succeeds (S_OK or S_FALSE).
    let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if joined.is_err() && joined != RPC_E_CHANGED_MODE {
        return Err(PlatformError::Os {
            context: "CoInitializeEx",
            code: joined.0,
        });
    }
    let result = work();
    if joined.is_ok() {
        // SAFETY: balances the CoInitializeEx above. The COM objects `work`
        // made are gone; only agile references to them crossed threads.
        unsafe { CoUninitialize() };
    }
    result
}

/// The cursor's position, if it can be read.
fn cursor() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    // SAFETY: a writable POINT on this stack frame.
    unsafe { GetCursorPos(&raw mut point) }.ok()?;
    Some((point.x, point.y))
}

/// Steps 1 and 2: sorts `paths` and looks up the files, waiting at most
/// `timeout` for them. Call it with COM initialized on this thread.
fn prepare(paths: &[PathBuf], timeout: Duration) -> Prepared {
    let mut why: Vec<Option<SkipReason>> = paths
        .iter()
        .map(|path| {
            let (is_dir, exists) = match std::fs::metadata(path) {
                Ok(meta) => (meta.is_dir(), true),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => (false, false),
                // Can't tell (access denied, say): the lookup decides.
                Err(_) => (false, true),
            };
            share_skip(path, is_dir, exists)
        })
        .collect();
    let wanted: Vec<usize> = (0..paths.len()).filter(|&at| why[at].is_none()).collect();
    let plain: Vec<PathBuf> = wanted.iter().map(|&at| plain_path(&paths[at])).collect();
    let mut items = Vec::new();
    let mut shared = Vec::new();
    for (&at, found) in wanted.iter().zip(look_up(&plain, timeout)) {
        match found {
            Some(item) => {
                items.push(item);
                shared.push(paths[at].clone());
            }
            None => why[at] = Some(SkipReason::Unshareable),
        }
    }
    let skipped = paths
        .iter()
        .zip(why)
        .filter_map(|(path, why)| {
            why.map(|why| Skipped {
                path: path.clone(),
                why,
            })
        })
        .collect();
    Prepared {
        items,
        shared,
        skipped,
    }
}

/// Looks every file up as a storage item at once, and waits at most
/// `timeout` for the lot. `None` for a file that failed or didn't come
/// back in time.
fn look_up(files: &[PathBuf], timeout: Duration) -> Vec<Option<AgileReference<IStorageItem>>> {
    let (tx, rx) = mpsc::channel();
    let mut found: Vec<Option<AgileReference<IStorageItem>>> = files.iter().map(|_| None).collect();
    // Kept until the wait is over, so nothing depends on the system
    // keeping an operation alive that nobody holds.
    let mut running = Vec::with_capacity(files.len());
    for (at, file) in files.iter().enumerate() {
        let tx = tx.clone();
        let started = StorageFile::GetFileFromPathAsync(&HSTRING::from(file.as_os_str())).and_then(
            |operation| {
                // Runs on a pool thread when the lookup ends, or right here
                // if it already has. StorageFile can't cross threads; an
                // agile reference to it can.
                operation.when(move |result| {
                    let item = result
                        .and_then(|file| file.cast::<IStorageItem>())
                        .and_then(|item| AgileReference::new(&item));
                    let _ = tx.send((at, item));
                })?;
                Ok(operation)
            },
        );
        match started {
            Ok(operation) => running.push(operation),
            Err(err) => log_hresult("GetFileFromPathAsync", err.code()),
        }
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let mut waiting = running.len();
    while waiting > 0 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((at, Ok(item))) => {
                waiting -= 1;
                if let Some(slot) = found.get_mut(at) {
                    *slot = Some(item);
                }
            }
            Ok((_, Err(err))) => {
                waiting -= 1;
                log_hresult("GetFileFromPathAsync (result)", err.code());
            }
            Err(RecvTimeoutError::Timeout) => {
                log::info!("share: {waiting} file lookups didn't finish in time");
                break;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    found
}

/// Step 4: hands the files to the share thread and waits for the sheet to
/// ask for them.
fn open_sheet(
    frame: Rect,
    items: Vec<AgileReference<IStorageItem>>,
    title: String,
) -> Result<(), PlatformError> {
    let _turn = lock(&ONE_AT_A_TIME);
    let anchor = share_thread()?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (reply, answer) = mpsc::channel();
    *lock(&REQUEST) = Some(Request {
        id,
        frame,
        payload: Payload {
            items,
            title,
            reply,
        },
    });
    // SAFETY: posting takes the handle by value, so a stale one only makes
    // the call fail; the parameters are plain numbers.
    if let Err(err) = unsafe { PostMessageW(Some(anchor), WM_SHARE, WPARAM(0), LPARAM(0)) } {
        lock(&REQUEST).take();
        return Err(os_err("PostMessageW", &err));
    }
    match answer.recv_timeout(SHEET_TIMEOUT) {
        Ok(answer) => answer,
        Err(RecvTimeoutError::Timeout) => {
            // Not taken yet: the share thread is stuck. Taken: the sheet
            // never asked. Either way this share is over.
            lock(&REQUEST).take_if(|request| request.id == id);
            // SAFETY: as above.
            let _ =
                unsafe { PostMessageW(Some(anchor), WM_EXPIRE, WPARAM(id as usize), LPARAM(0)) };
            log::warn!("share: the share sheet didn't ask for the files within 30 s");
            Err(PlatformError::Os {
                context: "DataRequested",
                code: HRESULT::from_win32(ERROR_TIMEOUT.0).0,
            })
        }
        // The share thread dropped the request without a word.
        Err(RecvTimeoutError::Disconnected) => Err(PlatformError::Os {
            context: "the share thread",
            code: E_UNEXPECTED.0,
        }),
    }
}

/// Locks `mutex` even if a panic poisoned it: each value it guards is
/// written in one go, so it's never half-made.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The anchor, starting the share thread first if it isn't running.
fn share_thread() -> Result<HWND, PlatformError> {
    let mut anchor = lock(&ANCHOR);
    if let Some(raw) = *anchor {
        let hwnd = HWND(raw as *mut c_void);
        // SAFETY: IsWindow accepts any handle value and only reads state.
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Ok(hwnd);
        }
    }
    let (started, start) = mpsc::channel();
    thread::Builder::new()
        .name("grape-share".to_owned())
        .spawn(move || run_share_thread(&started))
        .map_err(|_| PlatformError::Os {
            context: "start the share thread",
            code: E_UNEXPECTED.0,
        })?;
    let raw = start
        .recv_timeout(START_TIMEOUT)
        .unwrap_or(Err(PlatformError::Os {
            context: "start the share thread",
            code: E_UNEXPECTED.0,
        }))?;
    *anchor = Some(raw);
    Ok(HWND(raw as *mut c_void))
}

/// The share thread: an STA that makes the anchor, says so (with its raw
/// handle) and pumps messages for the app's life.
fn run_share_thread(started: &Sender<Result<isize, PlatformError>>) {
    // SAFETY: no reserved pointer. The thread stays in the apartment for
    // its whole life; CoUninitialize below balances it.
    let joined = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    if let Err(err) = joined.ok() {
        let _ = started.send(Err(os_err("CoInitializeEx", &err)));
        return;
    }
    match create_anchor() {
        Ok(anchor) => {
            let _ = started.send(Ok(anchor.0 as isize));
            pump();
        }
        Err(err) => {
            let _ = started.send(Err(err));
        }
    }
    // SAFETY: balances the CoInitializeEx above, on the same thread.
    unsafe { CoUninitialize() };
}

/// This thread's message loop, until `WM_QUIT` or an error.
fn pump() {
    let mut msg = MSG::default();
    // SAFETY: a writable MSG on this stack frame; GetMessageW returns 0
    // for WM_QUIT and -1 on error, and both end the loop. Translate and
    // dispatch only read the message GetMessageW filled.
    unsafe {
        while GetMessageW(&raw mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

/// Makes the hidden anchor on this thread: a click-through tool-window
/// popup at alpha 1, neither topmost nor `WS_EX_NOACTIVATE`.
fn create_anchor() -> Result<HWND, PlatformError> {
    // SAFETY: the module handle of this executable; no name to read.
    let instance =
        unsafe { GetModuleHandleW(None) }.map_err(|err| os_err("GetModuleHandleW", &err))?;
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(anchor_proc),
        hInstance: instance.into(),
        lpszClassName: ANCHOR_CLASS,
        ..WNDCLASSEXW::default()
    };
    // SAFETY: a filled WNDCLASSEXW whose proc is a `'static` function and
    // whose name is a static, NUL-terminated string. A class registered
    // before (a test made an anchor) is fine.
    if unsafe { RegisterClassExW(&raw const class) } == 0 {
        // SAFETY: reads this thread's last error.
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            return Err(PlatformError::Os {
                context: "RegisterClassExW",
                code: HRESULT::from_win32(error.0).0,
            });
        }
    }
    // SAFETY: a hidden popup of the class above, with static strings and no
    // parent, menu or creation data.
    let anchor = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            ANCHOR_CLASS,
            w!("Grape Share"),
            WS_POPUP,
            0,
            0,
            32,
            32,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .map_err(|err| os_err("CreateWindowExW", &err))?;
    // SAFETY: a live layered window this thread just made; no color key.
    let layered =
        unsafe { SetLayeredWindowAttributes(anchor, COLORREF(0), ANCHOR_ALPHA, LWA_ALPHA) };
    if let Err(err) = layered {
        // SAFETY: destroys the window made above, on its own thread.
        let _ = unsafe { DestroyWindow(anchor) };
        return Err(os_err("SetLayeredWindowAttributes", &err));
    }
    Ok(anchor)
}

/// The anchor's window proc, on the share thread.
unsafe extern "system" fn anchor_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SHARE => {
            serve(hwnd);
            LRESULT(0)
        }
        WM_EXPIRE => {
            expire(hwnd, wparam.0 as u64);
            LRESULT(0)
        }
        WM_GIVE_BACK => {
            give_back(hwnd);
            LRESULT(0)
        }
        _ => {
            if msg == WM_ACTIVATE && (wparam.0 & 0xFFFF) as u32 != WA_INACTIVE && SERVED.get() {
                // SAFETY: posting to this thread's own live window, with
                // plain numbers.
                let _ = unsafe { PostMessageW(Some(hwnd), WM_GIVE_BACK, WPARAM(0), LPARAM(0)) };
            }
            // SAFETY: every other message goes to the default proc
            // unchanged, on the window's own thread.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// Opens the sheet for the request waiting in [`REQUEST`], if any.
fn serve(anchor: HWND) {
    let Some(Request { id, frame, payload }) = lock(&REQUEST).take() else {
        return;
    };
    // An earlier registration still waiting: its caller has given up.
    drop_pending(None);
    SERVED.set(false);
    place(anchor, frame);
    // SAFETY: a plain query.
    let front = unsafe { GetForegroundWindow() };
    FRONT.set((!front.is_invalid()).then_some(front.0 as isize));
    let reply = payload.reply.clone();
    if let Err(err) = register_and_show(anchor, id, payload) {
        let _ = reply.send(Err(err));
        // SAFETY: hides this thread's own live window; the return value is
        // only whether it was visible.
        let _ = unsafe { ShowWindow(anchor, SW_HIDE) };
    }
}

/// Moves the anchor to `frame`, on top of the windows that aren't topmost,
/// and shows it without activating it.
fn place(anchor: HWND, frame: Rect) {
    // SAFETY: this thread's own live window. No insert-after window is
    // HWND_TOP; NOACTIVATE leaves the focus where it is.
    let placed = unsafe {
        SetWindowPos(
            anchor,
            None,
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            SWP_NOACTIVATE,
        )
    };
    if let Err(err) = placed {
        log_hresult("SetWindowPos (share anchor)", err.code());
    }
    // SAFETY: as above; the return value is only whether it was visible.
    let _ = unsafe { ShowWindow(anchor, SW_SHOWNOACTIVATE) };
}

/// Registers the handler and opens the sheet. An error before the handler
/// ran is the caller's answer.
fn register_and_show(anchor: HWND, id: u64, payload: Payload) -> Result<(), PlatformError> {
    let interop = interop().map_err(|err| os_err("RoGetActivationFactory", &err))?;
    // SAFETY: the anchor is a live window of this thread, the one the sheet
    // belongs to.
    let manager: DataTransferManager =
        logged("GetForWindow", unsafe { interop.GetForWindow(anchor) })
            .map_err(|err| os_err("GetForWindow", &err))?;
    let slot = Arc::new(Mutex::new(Some(payload)));
    let in_show = Arc::new(AtomicBool::new(true));
    let token = Arc::new(AtomicI64::new(0));
    let handler = TypedEventHandler::<DataTransferManager, DataRequestedEventArgs>::new({
        let (slot, in_show, token) = (Arc::clone(&slot), Arc::clone(&in_show), Arc::clone(&token));
        move |sender, args| {
            on_data_requested(id, &slot, &in_show, &token, &sender, &args);
            // Never an error: Chromium says Windows ends the process for
            // one.
            Ok(())
        }
    });
    let registered = logged("add_DataRequested", manager.DataRequested(&handler))
        .map_err(|err| os_err("add_DataRequested", &err))?;
    token.store(registered, Ordering::SeqCst);
    // SAFETY: as for GetForWindow.
    let shown = logged("ShowShareUIForWindow", unsafe {
        interop.ShowShareUIForWindow(anchor)
    });
    in_show.store(false, Ordering::SeqCst);
    let served = lock(&slot).is_none();
    if served || shown.is_err() {
        // Ran inside the call, or never will: remove it now, outside it.
        let _ = logged(
            "remove_DataRequested",
            manager.RemoveDataRequested(registered),
        );
        if !served {
            lock(&slot).take();
        }
    } else {
        PENDING.with(|pending| {
            *pending.borrow_mut() = Some(Pending {
                id,
                manager,
                token: registered,
                slot,
            });
        });
    }
    match shown {
        // Served inside the call: the handler answered already.
        Err(_) if served => Ok(()),
        Err(err) => Err(os_err("ShowShareUIForWindow", &err)),
        Ok(()) => Ok(()),
    }
}

/// The interop factory, asked for directly as Firefox does, or through
/// `IUnknown` as Chromium does.
fn interop() -> ::windows::core::Result<IDataTransferManagerInterop> {
    logged(
        "RoGetActivationFactory(IDataTransferManagerInterop)",
        factory::<DataTransferManager, IDataTransferManagerInterop>(),
    )
    .or_else(|_| {
        logged(
            "RoGetActivationFactory(IUnknown)",
            factory::<DataTransferManager, IUnknown>(),
        )
        .and_then(|unknown| {
            logged(
                "QueryInterface(IDataTransferManagerInterop)",
                unknown.cast(),
            )
        })
    })
}

/// The `DataRequested` handler's work, on the share thread.
fn on_data_requested(
    id: u64,
    slot: &Mutex<Option<Payload>>,
    in_show: &AtomicBool,
    token: &AtomicI64,
    sender: &Ref<'_, DataTransferManager>,
    args: &Ref<'_, DataRequestedEventArgs>,
) {
    let taken = lock(slot).take();
    let Some(payload) = taken else {
        // The share expired or was served already: give the sheet nothing.
        if let Ok(request) = args.ok().and_then(DataRequestedEventArgs::Request) {
            let _ = request.FailWithDisplayText(&HSTRING::from(FAILED));
        }
        return;
    };
    if !in_show.load(Ordering::SeqCst) {
        // After ShowShareUIForWindow returned: remove the handler first.
        if let Ok(manager) = sender.ok() {
            let _ = logged(
                "remove_DataRequested",
                manager.RemoveDataRequested(token.load(Ordering::SeqCst)),
            );
        }
        let pending = PENDING.with(|pending| pending.borrow_mut().take_if(|it| it.id == id));
        drop(pending);
    }
    let filled = args.ok().and_then(|args| fill(args, &payload));
    let filled = logged("DataRequested (fill)", filled);
    if filled.is_ok() {
        SERVED.set(true);
    }
    let _ = payload
        .reply
        .send(filled.map_err(|err| os_err("DataRequested", &err)));
}

/// Fills the sheet's package: the title, `Copy`, and the files read-only.
/// On any failure the sheet says so.
fn fill(args: &DataRequestedEventArgs, payload: &Payload) -> ::windows::core::Result<()> {
    let request = args.Request()?;
    let filled = (|| {
        let package = request.Data()?;
        let title = if payload.title.is_empty() {
            " "
        } else {
            payload.title.as_str()
        };
        package.Properties()?.SetTitle(&HSTRING::from(title))?;
        package.SetRequestedOperation(DataPackageOperation::Copy)?;
        let items = payload
            .items
            .iter()
            .map(|item| item.resolve().map(Some))
            .collect::<::windows::core::Result<Vec<Option<IStorageItem>>>>()?;
        package.SetStorageItems(&IIterable::<IStorageItem>::from(items), true)
    })();
    if filled.is_err() {
        let _ = request.FailWithDisplayText(&HSTRING::from(FAILED));
    }
    filled
}

/// The caller of share `id` gave up: remove its handler if it's still
/// waiting, and hide the anchor, as no sheet asked for it.
fn expire(anchor: HWND, id: u64) {
    if drop_pending(Some(id)) {
        // SAFETY: hides this thread's own live window.
        let _ = unsafe { ShowWindow(anchor, SW_HIDE) };
    }
}

/// Removes the waiting registration (only share `id`'s, when given) and
/// drops its payload. `true` if there was one.
fn drop_pending(id: Option<u64>) -> bool {
    let pending = PENDING.with(|pending| {
        pending
            .borrow_mut()
            .take_if(|it| id.is_none_or(|id| it.id == id))
    });
    let Some(pending) = pending else {
        return false;
    };
    let _ = logged(
        "remove_DataRequested",
        pending.manager.RemoveDataRequested(pending.token),
    );
    lock(&pending.slot).take();
    true
}

/// The anchor was activated after a share was served: the sheet closed
/// and handed it the focus. Hide it and hand the foreground back.
fn give_back(anchor: HWND) {
    if !SERVED.replace(false) {
        return;
    }
    // SAFETY: hides this thread's own live window.
    let _ = unsafe { ShowWindow(anchor, SW_HIDE) };
    let Some(front) = FRONT.take().map(|raw| HWND(raw as *mut c_void)) else {
        log::info!("share: the sheet closed; no window was in front before it");
        return;
    };
    if !is_window_of_another_app(front) {
        log::info!("share: the sheet closed; the window in front before it is gone or Grape's");
        return;
    }
    // SAFETY: takes the handle by value; Grape was just activated, so it
    // may set the foreground.
    let handed = unsafe { SetForegroundWindow(front) }.as_bool();
    log::info!("share: the sheet closed; foreground handed back: {handed}");
}

/// `true` if `hwnd` is a live window of another process.
fn is_window_of_another_app(hwnd: HWND) -> bool {
    // SAFETY: IsWindow accepts any handle value and only reads state.
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return false;
    }
    let mut process = 0_u32;
    // SAFETY: a plain query with a writable u32 for the process id.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process)) };
    // SAFETY: a plain query.
    process != 0 && process != unsafe { GetCurrentProcessId() }
}

/// Logs a share step's HRESULT (never a path or a name) and passes the
/// result on.
fn logged<T>(step: &str, result: ::windows::core::Result<T>) -> ::windows::core::Result<T> {
    let code = result
        .as_ref()
        .map_or_else(|err| err.code(), |_| HRESULT(0));
    log_hresult(step, code);
    result
}

fn log_hresult(step: &str, code: HRESULT) {
    if code.is_ok() {
        log::info!("share: {step}: {:#010x}", code.0);
    } else {
        log::warn!("share: {step}: {:#010x}", code.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ::windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, IsWindowVisible, SendMessageW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOPMOST,
    };

    /// A scratch folder, deleted with everything in it on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("grape-share-{name}-{}", std::process::id()));
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

    fn skipped(path: &std::path::Path, why: SkipReason) -> Skipped {
        Skipped {
            path: path.to_path_buf(),
            why,
        }
    }

    #[test]
    fn a_real_file_becomes_a_storage_item() {
        let scratch = Scratch::new("file");
        let file = scratch.file("photo 1.jpg");
        let prepared =
            with_mta(|| Ok(prepare(std::slice::from_ref(&file), LOOKUP_TIMEOUT))).unwrap();
        assert_eq!(prepared.skipped, []);
        assert_eq!(prepared.shared, std::slice::from_ref(&file));
        let [item] = prepared.items.as_slice() else {
            panic!("expected one item, got {}", prepared.items.len());
        };
        let item = item.resolve().unwrap();
        assert_eq!(item.Name().unwrap(), "photo 1.jpg");
        assert!(file.exists());
    }

    #[test]
    fn folders_and_missing_items_are_skipped_before_any_lookup() {
        let scratch = Scratch::new("skip");
        let folder = scratch.folder("Trip");
        let gone = scratch.0.join("gone.png");
        let prepared =
            with_mta(|| Ok(prepare(&[folder.clone(), gone.clone()], LOOKUP_TIMEOUT))).unwrap();
        assert!(prepared.items.is_empty());
        // A lookup would have made them Unshareable.
        assert_eq!(
            prepared.skipped,
            [
                skipped(&folder, SkipReason::Folder),
                skipped(&gone, SkipReason::Missing)
            ]
        );
    }

    #[test]
    fn a_shortcut_cannot_be_shared() {
        let scratch = Scratch::new("shortcut");
        let link = scratch.file("Notes.lnk");
        let prepared =
            with_mta(|| Ok(prepare(std::slice::from_ref(&link), LOOKUP_TIMEOUT))).unwrap();
        assert!(prepared.items.is_empty());
        assert_eq!(prepared.skipped, [skipped(&link, SkipReason::Unshareable)]);
    }

    /// Nothing left to share: no sheet, and no share thread either.
    #[test]
    fn with_nothing_to_share_no_sheet_opens() {
        let scratch = Scratch::new("nothing");
        let folder = scratch.folder("Trip");
        let link = scratch.file("Notes.url");
        let gone = scratch.0.join("gone.png");
        let outcome = share(
            &[folder.clone(), link.clone(), gone.clone()],
            Some((10, 10)),
        )
        .unwrap();
        assert_eq!(
            outcome,
            ShareOutcome {
                opened: false,
                skipped: vec![
                    skipped(&folder, SkipReason::Folder),
                    skipped(&link, SkipReason::Unshareable),
                    skipped(&gone, SkipReason::Missing),
                ],
            }
        );
    }

    #[test]
    fn the_mta_is_joined_and_left_or_an_sta_kept() {
        assert_eq!(with_mta(|| Ok(1)), Ok(1));
        // SAFETY: the test thread joins an STA, balanced below.
        let sta = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        assert!(sta.is_ok(), "{sta:?}");
        assert_eq!(with_mta(|| Ok(2)), Ok(2));
        // SAFETY: balances the CoInitializeEx above.
        unsafe { CoUninitialize() };
    }

    #[test]
    fn the_anchor_is_an_invisible_click_through_tool_window_that_can_activate() {
        let anchor = create_anchor().unwrap();
        // SAFETY: plain queries of this thread's own live window.
        let (ex, visible) = unsafe {
            (
                GetWindowLongW(anchor, GWL_EXSTYLE) as u32,
                IsWindowVisible(anchor).as_bool(),
            )
        };
        let wanted = WS_EX_TOOLWINDOW.0 | WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0;
        assert_eq!(ex & wanted, wanted, "{ex:#x}");
        assert_eq!(ex & (WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0), 0, "{ex:#x}");
        assert!(!visible);
        // A second anchor reuses the registered class.
        let again = create_anchor().unwrap();
        // With no request waiting, a share message does nothing, and an
        // expiry or a give-back with nothing served neither.
        // SAFETY: same-thread messages to our own live window, with plain
        // numbers.
        unsafe {
            SendMessageW(anchor, WM_SHARE, None, None);
            SendMessageW(anchor, WM_EXPIRE, Some(WPARAM(0)), None);
            SendMessageW(anchor, WM_GIVE_BACK, None, None);
        }
        // SAFETY: as above.
        assert!(!unsafe { IsWindowVisible(anchor) }.as_bool());
        // SAFETY: destroys the windows made above, on their own thread.
        unsafe {
            DestroyWindow(anchor).unwrap();
            DestroyWindow(again).unwrap();
        }
    }
}
