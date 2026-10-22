//! The low-level mouse hook (`WH_MOUSE_LL`) on its own message-pumping
//! thread (`docs/PLATFORM_WINDOWS.md` → "Detecting a file drag").
//!
//! Windows runs a low-level hook on the thread that installed it, by sending
//! that thread a message, so the thread must pump messages (`GetMessageW`)
//! or the whole system's mouse stalls. The callback stays tiny: it forwards
//! primary-button presses, releases and moves-while-pressed as [`RawMouse`]
//! values with `try_send`, and returns. A callback slower than
//! `LowLevelHooksTimeout` gets the hook removed without notice, so anything
//! slower (the allowlist check, key polling) happens on the drag worker
//! thread instead.

use std::cell::RefCell;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use ::windows::Win32::Foundation::{E_FAIL, LPARAM, LRESULT, WPARAM};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::System::Threading::GetCurrentThreadId;
use ::windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, MSG, MSLLHOOKSTRUCT,
    PM_NOREMOVE, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT,
};

use crate::PlatformError;

/// One raw mouse event from the hook. Global, physical pixels: low-level
/// hooks report per-monitor-aware screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawMouse {
    /// The primary button went down.
    Down { x: i32, y: i32 },
    /// The pointer moved while the primary button was down.
    Move { x: i32, y: i32 },
    /// The primary button went up.
    Up,
}

/// Where the hook callback sends events. Lives in a thread-local on the
/// hook thread, the only thread that runs the callback.
struct Sink {
    tx: SyncSender<RawMouse>,
    pressed: bool,
}

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// A running hook thread. Dropping it unhooks and joins the thread.
#[derive(Debug)]
pub(crate) struct MouseHook {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl MouseHook {
    /// Starts the hook thread and waits until the hook is installed.
    pub(crate) fn start(tx: SyncSender<RawMouse>) -> Result<Self, PlatformError> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("grape-mouse-hook".to_owned())
            .spawn(move || run(tx, &ready_tx))
            .map_err(|_| PlatformError::Os {
                context: "spawning the mouse-hook thread",
                code: E_FAIL.0,
            })?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err(PlatformError::Os {
                    context: "starting the mouse-hook thread",
                    code: E_FAIL.0,
                })
            }
        }
    }

    /// Unhooks and joins the thread. Safe to call more than once.
    pub(crate) fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // SAFETY: posts WM_QUIT (no pointers) to the hook thread, whose
            // queue exists: it was created before the thread reported ready.
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            let _ = thread.join();
        }
    }
}

impl Drop for MouseHook {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The hook thread: install the hook, report ready, pump messages until
/// WM_QUIT, unhook.
fn run(tx: SyncSender<RawMouse>, ready: &SyncSender<Result<u32, PlatformError>>) {
    SINK.with(|sink| {
        *sink.borrow_mut() = Some(Sink { tx, pressed: false });
    });

    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid out-pointer for the call. Peeking creates this
    // thread's message queue, so PostThreadMessageW(WM_QUIT) can't arrive
    // before the queue exists. Nothing is removed.
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    // SAFETY: a plain query of the calling thread's id.
    let thread_id = unsafe { GetCurrentThreadId() };

    // SAFETY: `None` asks for this executable's own module handle; nothing
    // is written through pointers.
    let module = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => module,
        Err(err) => {
            let _ = ready.send(Err(PlatformError::Os {
                context: "GetModuleHandleW",
                code: err.code().0,
            }));
            return;
        }
    };
    // SAFETY: `hook_proc` is a `'static` function with the HOOKPROC
    // signature, and this thread pumps messages below for as long as the
    // hook is installed, which is what WH_MOUSE_LL requires.
    let hook =
        match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), Some(module.into()), 0) } {
            Ok(hook) => hook,
            Err(err) => {
                let _ = ready.send(Err(PlatformError::Os {
                    context: "SetWindowsHookExW(WH_MOUSE_LL)",
                    code: err.code().0,
                }));
                return;
            }
        };
    let _ = ready.send(Ok(thread_id));

    loop {
        // SAFETY: `msg` is a valid out-pointer; `None` takes messages for
        // any window of this thread plus thread messages.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // 0 is WM_QUIT, -1 an error: stop either way.
        if got.0 <= 0 {
            break;
        }
        // SAFETY: `msg` was just filled in by GetMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // SAFETY: `hook` came from SetWindowsHookExW above and is removed once.
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    // Dropping the sender tells the drag worker the hook is gone.
    SINK.with(|sink| {
        sink.borrow_mut().take();
    });
}

/// The `WH_MOUSE_LL` callback. Runs on the hook thread for every mouse
/// event in the system, so it only copies a point and returns.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = lparam.0 as *const MSLLHOOKSTRUCT;
        // SAFETY: for HC_ACTION, lParam points to an MSLLHOOKSTRUCT that is
        // valid for the duration of this call. `as_ref` guards against null.
        if let Some(info) = unsafe { info.as_ref() } {
            forward(wparam.0 as u32, info.pt.x, info.pt.y);
        }
    }
    // SAFETY: hands the event on to the next hook unchanged, as every hook
    // must. The hook handle argument is ignored by modern Windows.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn forward(message: u32, x: i32, y: i32) {
    SINK.with(|sink| {
        let Ok(mut sink) = sink.try_borrow_mut() else {
            return;
        };
        let Some(sink) = sink.as_mut() else {
            return;
        };
        let raw = match message {
            WM_LBUTTONDOWN => {
                sink.pressed = true;
                RawMouse::Down { x, y }
            }
            WM_MOUSEMOVE if sink.pressed => RawMouse::Move { x, y },
            WM_LBUTTONUP => {
                sink.pressed = false;
                RawMouse::Up
            }
            _ => return,
        };
        // A full channel drops the event rather than block the user's
        // input. The worker's button poll recovers a lost release.
        let _ = sink.tx.try_send(raw);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_installs_and_stops() {
        let (tx, rx) = mpsc::sync_channel(16);
        let mut hook = MouseHook::start(tx).unwrap();
        hook.stop();
        hook.stop();
        // The thread dropped its sender, so the channel is closed.
        assert!(rx.recv().is_err());
    }

    #[test]
    fn forward_tracks_the_button() {
        let (tx, rx) = mpsc::sync_channel(16);
        SINK.with(|sink| {
            *sink.borrow_mut() = Some(Sink { tx, pressed: false });
        });
        forward(WM_MOUSEMOVE, 1, 1);
        forward(WM_LBUTTONDOWN, 2, 3);
        forward(WM_MOUSEMOVE, 4, 5);
        forward(WM_LBUTTONUP, 4, 5);
        forward(WM_MOUSEMOVE, 6, 7);
        SINK.with(|sink| {
            sink.borrow_mut().take();
        });
        let got: Vec<RawMouse> = rx.iter().collect();
        assert_eq!(
            got,
            vec![
                RawMouse::Down { x: 2, y: 3 },
                RawMouse::Move { x: 4, y: 5 },
                RawMouse::Up,
            ]
        );
    }
}
