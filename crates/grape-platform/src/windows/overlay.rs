//! Keeps the overlay window out of the taskbar and Alt-Tab, and stops it
//! from taking focus when it is shown or moved (`docs/PLATFORM_WINDOWS.md`
//! → "Overlay window").
//!
//! The Tauri config already asks for most of this: `focusable: false`
//! becomes `WS_EX_NOACTIVATE` and `alwaysOnTop` becomes `WS_EX_TOPMOST`.
//! `WS_EX_TOOLWINDOW` has no config key, and setting it once from outside
//! doesn't stick: tao recomputes the whole extended style and writes it with
//! `SetWindowLongW(GWL_EXSTYLE, …)` on every window-flag change, including
//! each show and hide. So [`harden_overlay`] installs a subclass proc that
//! edits every `WM_STYLECHANGING` for `GWL_EXSTYLE`: it forces
//! `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` on and `WS_EX_APPWINDOW` off, after
//! the rest of the window-proc chain has had its say.
//!
//! `WS_EX_NOACTIVATE` only stops clicks from activating the window. It
//! doesn't stop a programmatic show: once the window exists, tao shows it
//! with `ShowWindow(SW_SHOW)`, which activates it whenever Grape owns the
//! foreground (for example just after its tray menu closes). `ShowWindow`
//! does its work through a window-position change, so the subclass also
//! adds `SWP_NOACTIVATE` to every `WM_WINDOWPOSCHANGING`. That covers show,
//! move, resize and z-order changes. A direct `SetForegroundWindow` or
//! `SetActiveWindow` (Tauri's `set_focus`) still activates the window, so
//! never call those on the overlay. Whether Windows honors this has to be
//! checked by hand (`docs/TESTING.md` → "App shell").
//!
//! The float shelf ([`harden_shelf`]) gets the same subclass with other
//! bits: `WS_EX_TOOLWINDOW` on, `WS_EX_APPWINDOW` and `WS_EX_NOACTIVATE`
//! off, and nothing added to position changes. A click must activate the
//! shelf: only the foreground window can capture the mouse, and a drag out
//! of the shelf needs the capture. Grape still never activates it itself:
//! [`show_shelf`] shows it with `SWP_NOACTIVATE`, and [`hide_shelf`] hides
//! it; never use Tauri's `show`, `hide` or `set_focus` on it.

use core::ffi::c_void;

use ::windows::core::HRESULT;
use ::windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_INVALID_WINDOW_HANDLE, ERROR_SUCCESS, E_FAIL, HWND, LPARAM,
    LRESULT, WIN32_ERROR, WPARAM,
};
use ::windows::Win32::System::Threading::GetCurrentThreadId;
use ::windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use ::windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsWindow, SetWindowLongW,
    SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOPMOST, SET_WINDOW_POS_FLAGS, STYLESTRUCT,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, WINDOWPOS,
    WINDOW_EX_STYLE, WM_NCDESTROY, WM_STYLECHANGING, WM_WINDOWPOSCHANGING, WS_EX_ACCEPTFILES,
    WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_EX_WINDOWEDGE,
};

use crate::{PlatformError, Rect};

/// Identifies Grape's subclass on the window ("GRAP").
const SUBCLASS_ID: usize = 0x4752_4150;

/// Bits the overlay must always have: no taskbar or Alt-Tab entry, not
/// activated by clicks.
const FORCED_ON: u32 = WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0;

/// Bits the overlay must never have: `WS_EX_APPWINDOW` puts a window back on
/// the taskbar.
const FORCED_OFF: u32 = WS_EX_APPWINDOW.0;

/// Bits the shelf must always have: no taskbar or Alt-Tab entry.
const SHELF_ON: u32 = WS_EX_TOOLWINDOW.0;

/// Bits the shelf must never have: back on the taskbar, or not activated
/// by a click (a drag out of it needs the mouse capture, which only the
/// foreground window gets).
const SHELF_OFF: u32 = WS_EX_APPWINDOW.0 | WS_EX_NOACTIVATE.0;

/// Which window the subclass keeps in shape. It rides in the subclass's
/// reference data, so one proc serves both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The overlay (and the toast): never activated.
    Overlay,
    /// The float shelf: activated by a click, never by Grape.
    Shelf,
}

impl Role {
    /// The subclass's reference data. The overlay's is 0, as it always was.
    const fn ref_data(self) -> usize {
        match self {
            Self::Overlay => 0,
            Self::Shelf => 1,
        }
    }

    const fn from_ref_data(data: usize) -> Self {
        if data == Self::Shelf.ref_data() {
            Self::Shelf
        } else {
            Self::Overlay
        }
    }

    const fn forced_on(self) -> u32 {
        match self {
            Self::Overlay => FORCED_ON,
            Self::Shelf => SHELF_ON,
        }
    }

    const fn forced_off(self) -> u32 {
        match self {
            Self::Overlay => FORCED_OFF,
            Self::Shelf => SHELF_OFF,
        }
    }

    /// Whether every position change gets `SWP_NOACTIVATE`.
    const fn never_activated(self) -> bool {
        matches!(self, Self::Overlay)
    }
}

/// Extended-style flags named in log lines, in display order. Anything else
/// is printed as leftover hex.
const NAMED_FLAGS: [(WINDOW_EX_STYLE, &str); 9] = [
    (WS_EX_TOOLWINDOW, "TOOLWINDOW"),
    (WS_EX_NOACTIVATE, "NOACTIVATE"),
    (WS_EX_TOPMOST, "TOPMOST"),
    (WS_EX_APPWINDOW, "APPWINDOW"),
    (WS_EX_LAYERED, "LAYERED"),
    (WS_EX_TRANSPARENT, "TRANSPARENT"),
    (WS_EX_NOREDIRECTIONBITMAP, "NOREDIRECTIONBITMAP"),
    (WS_EX_ACCEPTFILES, "ACCEPTFILES"),
    (WS_EX_WINDOWEDGE, "WINDOWEDGE"),
];

/// Makes `hwnd` behave as Grape's overlay: tool window (no taskbar button,
/// no Alt-Tab entry), topmost, and not activated when it is shown, moved or
/// clicked. `SetForegroundWindow`/`SetActiveWindow` can still activate it;
/// don't call them (or Tauri's `set_focus`) on the overlay.
///
/// It installs a subclass that keeps the style bits through tao's style
/// rewrites and adds `SWP_NOACTIVATE` to every window-position change,
/// applies the bits once with `SetWindowLongW`, and refreshes the frame and
/// z-order with `SetWindowPos(HWND_TOPMOST, NOMOVE | NOSIZE | NOACTIVATE |
/// FRAMECHANGED)`. The subclass removes itself when the window is destroyed.
///
/// `hwnd` is the raw handle value (`HWND.0 as isize`). Call this on the
/// thread that created the window (Tauri's main thread, e.g. in `setup`),
/// before the window is first shown. Calling it again is harmless.
///
/// # Errors
/// - [`PlatformError::WrongThread`] if the calling thread doesn't own the
///   window.
/// - [`PlatformError::Os`] if `hwnd` isn't a window or a Win32 call fails.
pub fn harden_overlay(hwnd: isize) -> Result<(), PlatformError> {
    harden(hwnd, Role::Overlay)
}

/// Makes `hwnd` behave as Grape's float shelf: tool window (no taskbar
/// button, no Alt-Tab entry) and topmost, but activated by a click, so a
/// drag out of it can capture the mouse. Grape itself never activates it:
/// show and move it with [`show_shelf`], hide it with [`hide_shelf`], and
/// never call Tauri's `show`, `set_focus` or `hide` on it.
///
/// The same subclass as [`harden_overlay`]'s keeps `WS_EX_TOOLWINDOW` on
/// and `WS_EX_APPWINDOW` and `WS_EX_NOACTIVATE` off through tao's style
/// rewrites. It leaves window-position changes alone. The bits are applied
/// once, then the frame and z-order are refreshed with
/// `SetWindowPos(HWND_TOPMOST, NOMOVE | NOSIZE | NOACTIVATE |
/// FRAMECHANGED)`.
///
/// `hwnd` is the raw handle value (`HWND.0 as isize`). Call this on the
/// thread that created the window (Tauri's main thread), before the window
/// is first shown. Calling it again is harmless.
///
/// # Errors
/// As [`harden_overlay`].
pub fn harden_shelf(hwnd: isize) -> Result<(), PlatformError> {
    harden(hwnd, Role::Shelf)
}

/// Shows the shelf at `frame` (global physical pixels) on top of every
/// window that isn't topmost, without activating it:
/// `SetWindowPos(HWND_TOPMOST, …, SWP_NOACTIVATE | SWP_SHOWWINDOW)`. Also
/// moves or resizes a shelf that's already shown. When moving to a monitor
/// with another scale makes tao resize the window on the way, the frame is
/// set once more.
///
/// `hwnd` is the raw handle value. Call this on the window's thread
/// (Tauri's main thread).
///
/// # Errors
/// - [`PlatformError::WrongThread`] if the calling thread doesn't own the
///   window.
/// - [`PlatformError::Os`] if `hwnd` isn't a window or `SetWindowPos`
///   fails.
pub fn show_shelf(hwnd: isize, frame: Rect) -> Result<(), PlatformError> {
    let hwnd = owned_window(hwnd)?;
    place(hwnd, frame)?;
    if window_rect(hwnd).is_some_and(|now| now != frame) {
        place(hwnd, frame)?;
    }
    Ok(())
}

/// Hides the shelf (`ShowWindow(SW_HIDE)`). Hiding a hidden shelf does
/// nothing.
///
/// `hwnd` is the raw handle value. Call this on the window's thread
/// (Tauri's main thread).
///
/// # Errors
/// - [`PlatformError::WrongThread`] if the calling thread doesn't own the
///   window.
/// - [`PlatformError::Os`] if `hwnd` isn't a window.
pub fn hide_shelf(hwnd: isize) -> Result<(), PlatformError> {
    let hwnd = owned_window(hwnd)?;
    // SAFETY: `hwnd` is a live window owned by this thread. The return
    // value is only whether it was visible before.
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    Ok(())
}

/// Installs the subclass for `role` and applies its bits once.
fn harden(hwnd: isize, role: Role) -> Result<(), PlatformError> {
    let hwnd = owned_window(hwnd)?;

    // SAFETY: `hwnd` is a live window owned by the calling thread (checked
    // above), which is what SetWindowSubclass requires. The proc is a
    // `'static` function with the SUBCLASSPROC signature, and its reference
    // data is a plain number (the role), not a pointer, so nothing it uses
    // can dangle. It removes itself on WM_NCDESTROY. Installing the same
    // (proc, id) pair twice only updates the reference data, so repeat
    // calls are harmless. SetLastError/GetLastError only touch this
    // thread's error slot. SetWindowSubclass isn't documented to set a last
    // error, so it's cleared first: a stale code from an earlier call is
    // never reported.
    let (installed, error) = unsafe {
        SetLastError(ERROR_SUCCESS);
        let installed = SetWindowSubclass(
            hwnd,
            Some(overlay_subclass_proc),
            SUBCLASS_ID,
            role.ref_data(),
        );
        (installed, GetLastError())
    };
    if !installed.as_bool() {
        return Err(win32_error_or_fail("SetWindowSubclass", error));
    }

    let before = read_ex_style(hwnd)?;
    let wanted = hardened(role, before);
    if wanted != before {
        set_ex_style(hwnd, wanted)?;
    }

    // SAFETY: `hwnd` is a live window owned by this thread. NOMOVE | NOSIZE
    // keep its geometry, NOACTIVATE leaves focus where it is, FRAMECHANGED
    // applies the new extended style, and HWND_TOPMOST is a documented
    // sentinel value, not a real window.
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
    }
    .map_err(|err| PlatformError::Os {
        context: "SetWindowPos",
        code: err.code().0,
    })
}

/// Reads the window's extended style, for logging. See
/// [`describe_ex_style`].
///
/// # Errors
/// [`PlatformError::Os`] if `hwnd` isn't a window.
pub fn ex_style(hwnd: isize) -> Result<u32, PlatformError> {
    read_ex_style(to_hwnd(hwnd))
}

/// Formats an extended style for a log line, e.g.
/// `0x08000088 TOOLWINDOW|NOACTIVATE|TOPMOST`. Bits without a name are
/// appended as hex.
pub fn describe_ex_style(ex_style: u32) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut rest = ex_style;
    for (flag, name) in NAMED_FLAGS {
        if ex_style & flag.0 == flag.0 {
            names.push(name.to_owned());
            rest &= !flag.0;
        }
    }
    if rest != 0 {
        names.push(format!("{rest:#x}"));
    }
    if names.is_empty() {
        names.push("none".to_owned());
    }
    format!("{ex_style:#010x} {}", names.join("|"))
}

/// The extended style `role` keeps, given what the rest of the system
/// wants.
const fn hardened(role: Role, ex_style: u32) -> u32 {
    (ex_style | role.forced_on()) & !role.forced_off()
}

fn to_hwnd(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// `raw` as a window this thread owns: `Os` (`IsWindow`) if it isn't a
/// window, `WrongThread` if another thread owns it.
pub(crate) fn owned_window(raw: isize) -> Result<HWND, PlatformError> {
    let hwnd = to_hwnd(raw);
    // SAFETY: IsWindow accepts any handle value, including null or stale
    // ones, and only reads window-manager state.
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err(win32_error("IsWindow", ERROR_INVALID_WINDOW_HANDLE));
    }
    // SAFETY: both are plain queries. The process-id out-pointer is `None`,
    // so no memory is written.
    let (owner, current) = unsafe { (GetWindowThreadProcessId(hwnd, None), GetCurrentThreadId()) };
    if owner != current {
        return Err(PlatformError::WrongThread);
    }
    Ok(hwnd)
}

/// Shows `hwnd` topmost at `frame`, without activating it.
fn place(hwnd: HWND, frame: Rect) -> Result<(), PlatformError> {
    const SHOW: SET_WINDOW_POS_FLAGS = SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0 | SWP_SHOWWINDOW.0);
    // SAFETY: the caller checked that `hwnd` is a live window owned by this
    // thread. HWND_TOPMOST is a documented sentinel value, not a real
    // window, and NOACTIVATE leaves the focus where it is.
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            SHOW,
        )
    }
    .map_err(|err| PlatformError::Os {
        context: "SetWindowPos",
        code: err.code().0,
    })
}

/// Where `hwnd` is now, or `None` if that can't be read.
fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut rect = ::windows::Win32::Foundation::RECT::default();
    // SAFETY: a live window and a writable RECT on this stack frame.
    unsafe { GetWindowRect(hwnd, &raw mut rect) }.ok()?;
    Some(Rect {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

fn read_ex_style(hwnd: HWND) -> Result<u32, PlatformError> {
    // SAFETY: SetLastError and GetLastError only touch this thread's error
    // slot. GetWindowLongW only reads, and fails cleanly for a handle that
    // isn't a window. Clearing the error first tells a real 0 style apart
    // from a failure.
    let (style, error) = unsafe {
        SetLastError(ERROR_SUCCESS);
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        (style, GetLastError())
    };
    if style == 0 && error != ERROR_SUCCESS {
        return Err(win32_error("GetWindowLongW", error));
    }
    Ok(style as u32)
}

fn set_ex_style(hwnd: HWND, ex_style: u32) -> Result<(), PlatformError> {
    // SAFETY: the caller checked that `hwnd` is a live window owned by this
    // thread. GWL_EXSTYLE takes a plain flag value, no pointers. The call
    // sends WM_STYLECHANGING to our subclass, which enforces the same bits.
    let (previous, error) = unsafe {
        SetLastError(ERROR_SUCCESS);
        let previous = SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style as i32);
        (previous, GetLastError())
    };
    if previous == 0 && error != ERROR_SUCCESS {
        return Err(win32_error("SetWindowLongW", error));
    }
    Ok(())
}

/// The subclass proc, for the overlay and the shelf alike (`ref_data` says
/// which, see [`Role`]). It runs on the window's thread for every message,
/// so it stays tiny: let the chain handle the message, then adjust the
/// proposed extended style or window-position change, if that's what the
/// message carries.
unsafe extern "system" fn overlay_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    ref_data: usize,
) -> LRESULT {
    let role = Role::from_ref_data(ref_data);
    if msg == WM_NCDESTROY {
        // SAFETY: we're on the window's own thread inside its window proc,
        // and removing our own subclass during WM_NCDESTROY is the documented
        // way to clean up. The return value only says whether it was found.
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(overlay_subclass_proc), SUBCLASS_ID) };
    }

    // SAFETY: forwarding the message unchanged to the next proc in the
    // chain, which every subclass proc must do for messages it doesn't eat.
    let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };

    match msg {
        // WM_STYLECHANGING's wParam says which style is changing; it carries
        // the index (-20 for GWL_EXSTYLE) sign-extended to pointer width.
        WM_STYLECHANGING if wparam.0 as isize == GWL_EXSTYLE.0 as isize => {
            let styles = lparam.0 as *mut STYLESTRUCT;
            // SAFETY: for WM_STYLECHANGING the system passes, in lParam, a
            // pointer to a writable STYLESTRUCT that lives for the duration
            // of this call; the docs invite the window proc to edit
            // `styleNew`. `as_mut` also guards against a null pointer.
            if let Some(styles) = unsafe { styles.as_mut() } {
                styles.styleNew = hardened(role, styles.styleNew);
            }
        }
        WM_WINDOWPOSCHANGING if role.never_activated() => {
            let pos = lparam.0 as *mut WINDOWPOS;
            // SAFETY: for WM_WINDOWPOSCHANGING the system passes, in lParam,
            // a pointer to a writable WINDOWPOS that lives for the duration
            // of this call; the docs let the window proc set `flags` bits to
            // veto parts of the change. `as_mut` also guards against null.
            if let Some(pos) = unsafe { pos.as_mut() } {
                pos.flags |= SWP_NOACTIVATE;
            }
        }
        _ => {}
    }
    result
}

fn win32_error(context: &'static str, error: WIN32_ERROR) -> PlatformError {
    PlatformError::Os {
        context,
        code: HRESULT::from_win32(error.0).0,
    }
}

/// [`win32_error`] for a call that failed without setting a last error:
/// reports `E_FAIL` rather than a code that reads as success (0).
fn win32_error_or_fail(context: &'static str, error: WIN32_ERROR) -> PlatformError {
    if error == ERROR_SUCCESS {
        PlatformError::Os {
            context,
            code: E_FAIL.0,
        }
    } else {
        win32_error(context, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::windows::core::w;
    use ::windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, IsWindowVisible, SendMessageW, WS_POPUP,
    };

    /// A hidden top-level test window, destroyed on drop.
    struct TestWindow(HWND);

    impl TestWindow {
        fn new(ex_style: WINDOW_EX_STYLE) -> Self {
            // SAFETY: creates a hidden popup of the built-in STATIC class with
            // static, NUL-terminated strings and no parent, menu or create
            // parameters.
            let hwnd = unsafe {
                CreateWindowExW(
                    ex_style,
                    w!("STATIC"),
                    w!("grape-platform test"),
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
            // SAFETY: the window was created by this thread and is destroyed
            // once; WM_NCDESTROY removes the subclass.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn hardened_sets_and_clears_the_right_bits() {
        let out = hardened(Role::Overlay, WS_EX_APPWINDOW.0 | WS_EX_LAYERED.0);
        assert_eq!(out & WS_EX_TOOLWINDOW.0, WS_EX_TOOLWINDOW.0);
        assert_eq!(out & WS_EX_NOACTIVATE.0, WS_EX_NOACTIVATE.0);
        assert_eq!(out & WS_EX_APPWINDOW.0, 0);
        assert_eq!(out & WS_EX_LAYERED.0, WS_EX_LAYERED.0);
        assert_eq!(hardened(Role::Overlay, out), out);
    }

    #[test]
    fn the_shelf_is_a_tool_window_that_a_click_activates() {
        let out = hardened(
            Role::Shelf,
            WS_EX_APPWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_LAYERED.0,
        );
        assert_eq!(out & WS_EX_TOOLWINDOW.0, WS_EX_TOOLWINDOW.0);
        assert_eq!(out & WS_EX_NOACTIVATE.0, 0);
        assert_eq!(out & WS_EX_APPWINDOW.0, 0);
        assert_eq!(out & WS_EX_LAYERED.0, WS_EX_LAYERED.0);
        assert_eq!(hardened(Role::Shelf, out), out);
    }

    #[test]
    fn the_overlay_keeps_its_reference_data_and_unknown_data_is_the_overlay() {
        assert_eq!(Role::Overlay.ref_data(), 0);
        for role in [Role::Overlay, Role::Shelf] {
            assert_eq!(Role::from_ref_data(role.ref_data()), role);
        }
        assert_eq!(Role::from_ref_data(7), Role::Overlay);
        assert!(Role::Overlay.never_activated());
        assert!(!Role::Shelf.never_activated());
    }

    #[test]
    fn describe_names_flags_and_leftovers() {
        assert_eq!(
            describe_ex_style(WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOPMOST.0),
            "0x08000088 TOOLWINDOW|NOACTIVATE|TOPMOST"
        );
        assert_eq!(describe_ex_style(0), "0x00000000 none");
        assert_eq!(describe_ex_style(0x200), "0x00000200 0x200");
    }

    #[test]
    fn harden_applies_the_styles() {
        let window = TestWindow::new(WS_EX_APPWINDOW);
        harden_overlay(window.raw()).unwrap();
        let ex = ex_style(window.raw()).unwrap();
        assert_eq!(ex & FORCED_ON, FORCED_ON, "{}", describe_ex_style(ex));
        assert_eq!(ex & FORCED_OFF, 0, "{}", describe_ex_style(ex));
        assert_eq!(
            ex & WS_EX_TOPMOST.0,
            WS_EX_TOPMOST.0,
            "{}",
            describe_ex_style(ex)
        );
    }

    #[test]
    fn subclass_survives_a_style_rewrite() {
        let window = TestWindow::new(WINDOW_EX_STYLE(0));
        harden_overlay(window.raw()).unwrap();
        // What tao does on show/hide: write a freshly computed style that
        // knows nothing about TOOLWINDOW.
        set_ex_style(window.0, WS_EX_APPWINDOW.0 | WS_EX_LAYERED.0).unwrap();
        let ex = ex_style(window.raw()).unwrap();
        assert_eq!(ex & FORCED_ON, FORCED_ON, "{}", describe_ex_style(ex));
        assert_eq!(ex & FORCED_OFF, 0, "{}", describe_ex_style(ex));
        assert_eq!(
            ex & WS_EX_LAYERED.0,
            WS_EX_LAYERED.0,
            "{}",
            describe_ex_style(ex)
        );
    }

    #[test]
    fn subclass_keeps_a_show_from_activating() {
        let window = TestWindow::new(WINDOW_EX_STYLE(0));
        harden_overlay(window.raw()).unwrap();
        // The position change ShowWindow(SW_SHOW) asks for: show, and
        // activate (no SWP_NOACTIVATE).
        let mut pos = WINDOWPOS {
            hwnd: window.0,
            flags: SWP_SHOWWINDOW | SWP_NOMOVE | SWP_NOSIZE,
            ..WINDOWPOS::default()
        };
        // SAFETY: a same-thread message to our own live window. lParam
        // points to a WINDOWPOS that outlives the call, which is what
        // WM_WINDOWPOSCHANGING carries.
        unsafe {
            SendMessageW(
                window.0,
                WM_WINDOWPOSCHANGING,
                Some(WPARAM(0)),
                Some(LPARAM(&raw mut pos as isize)),
            )
        };
        assert!(pos.flags.contains(SWP_NOACTIVATE), "{:#x}", pos.flags.0);
        assert!(pos.flags.contains(SWP_SHOWWINDOW), "{:#x}", pos.flags.0);
    }

    #[test]
    fn harden_twice_is_harmless() {
        let window = TestWindow::new(WINDOW_EX_STYLE(0));
        harden_overlay(window.raw()).unwrap();
        harden_overlay(window.raw()).unwrap();
        let ex = ex_style(window.raw()).unwrap();
        assert_eq!(ex & FORCED_ON, FORCED_ON);
    }

    #[test]
    fn rejects_a_window_from_another_thread() {
        let window = TestWindow::new(WINDOW_EX_STYLE(0));
        let raw = window.raw();
        let result = std::thread::spawn(move || harden_overlay(raw))
            .join()
            .unwrap();
        assert_eq!(result, Err(PlatformError::WrongThread));
    }

    #[test]
    fn a_failure_without_a_last_error_reads_as_a_failure() {
        assert_eq!(
            win32_error_or_fail("SetWindowSubclass", ERROR_SUCCESS),
            PlatformError::Os {
                context: "SetWindowSubclass",
                code: E_FAIL.0,
            }
        );
        assert_eq!(
            win32_error_or_fail("SetWindowSubclass", ERROR_INVALID_WINDOW_HANDLE),
            win32_error("SetWindowSubclass", ERROR_INVALID_WINDOW_HANDLE)
        );
        assert!(E_FAIL.is_err());
    }

    #[test]
    fn rejects_a_handle_that_is_not_a_window() {
        assert!(matches!(
            harden_overlay(0),
            Err(PlatformError::Os {
                context: "IsWindow",
                ..
            })
        ));
        assert!(ex_style(0).is_err());
        for result in [
            harden_shelf(0),
            show_shelf(0, Rect::default()),
            hide_shelf(0),
        ] {
            assert!(
                matches!(
                    result,
                    Err(PlatformError::Os {
                        context: "IsWindow",
                        ..
                    })
                ),
                "{result:?}"
            );
        }
    }

    #[test]
    fn harden_shelf_applies_the_shelf_styles() {
        let window = TestWindow::new(WS_EX_APPWINDOW | WS_EX_NOACTIVATE);
        harden_shelf(window.raw()).unwrap();
        let ex = ex_style(window.raw()).unwrap();
        assert_eq!(ex & SHELF_ON, SHELF_ON, "{}", describe_ex_style(ex));
        assert_eq!(ex & SHELF_OFF, 0, "{}", describe_ex_style(ex));
        assert_eq!(
            ex & WS_EX_TOPMOST.0,
            WS_EX_TOPMOST.0,
            "{}",
            describe_ex_style(ex)
        );
    }

    #[test]
    fn the_shelf_subclass_survives_a_style_rewrite() {
        let window = TestWindow::new(WINDOW_EX_STYLE(0));
        harden_shelf(window.raw()).unwrap();
        // tao writes NOACTIVATE for a window that isn't focusable, and
        // APPWINDOW for one on the taskbar.
        set_ex_style(
            window.0,
            WS_EX_APPWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_LAYERED.0,
        )
        .unwrap();
        let ex = ex_style(window.raw()).unwrap();
        assert_eq!(ex & SHELF_ON, SHELF_ON, "{}", describe_ex_style(ex));
        assert_eq!(ex & SHELF_OFF, 0, "{}", describe_ex_style(ex));
        assert_eq!(
            ex & WS_EX_LAYERED.0,
            WS_EX_LAYERED.0,
            "{}",
            describe_ex_style(ex)
        );
    }

    #[test]
    fn the_shelf_subclass_leaves_activation_alone() {
        let window = TestWindow::new(WINDOW_EX_STYLE(0));
        harden_shelf(window.raw()).unwrap();
        let mut pos = WINDOWPOS {
            hwnd: window.0,
            flags: SWP_SHOWWINDOW | SWP_NOMOVE | SWP_NOSIZE,
            ..WINDOWPOS::default()
        };
        // SAFETY: as in `subclass_keeps_a_show_from_activating`.
        unsafe {
            SendMessageW(
                window.0,
                WM_WINDOWPOSCHANGING,
                Some(WPARAM(0)),
                Some(LPARAM(&raw mut pos as isize)),
            )
        };
        assert!(!pos.flags.contains(SWP_NOACTIVATE), "{:#x}", pos.flags.0);
    }

    #[test]
    fn show_shelf_places_and_shows_and_hide_shelf_hides() {
        let window = TestWindow::new(WINDOW_EX_STYLE(0));
        harden_shelf(window.raw()).unwrap();
        let frame = Rect {
            x: 40,
            y: 60,
            width: 280,
            height: 136,
        };
        show_shelf(window.raw(), frame).unwrap();
        // SAFETY: a plain query of this thread's own live window.
        assert!(unsafe { IsWindowVisible(window.0) }.as_bool());
        assert_eq!(window_rect(window.0), Some(frame));
        // Already shown: only the frame changes.
        let taller = Rect {
            height: 176,
            ..frame
        };
        show_shelf(window.raw(), taller).unwrap();
        assert_eq!(window_rect(window.0), Some(taller));
        hide_shelf(window.raw()).unwrap();
        // SAFETY: as above.
        assert!(!unsafe { IsWindowVisible(window.0) }.as_bool());
        hide_shelf(window.raw()).unwrap();
    }

    #[test]
    fn the_shelf_calls_reject_a_window_from_another_thread() {
        let window = TestWindow::new(WINDOW_EX_STYLE(0));
        let raw = window.raw();
        let results = std::thread::spawn(move || {
            [
                harden_shelf(raw),
                show_shelf(raw, Rect::default()),
                hide_shelf(raw),
            ]
        })
        .join()
        .unwrap();
        for result in results {
            assert_eq!(result, Err(PlatformError::WrongThread));
        }
    }
}
