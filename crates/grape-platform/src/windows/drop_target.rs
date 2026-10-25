//! Grape's own `IDropTarget` for the overlay window (`docs/PLATFORM_WINDOWS.md`
//! → "The drop must never move the source", DECISIONS D13).
//!
//! Explorer decides what to do with the dragged files from the drop effect
//! the target reports. With Shift held the user's intent looks like "move",
//! and a target that reports `DROPEFFECT_MOVE`, or leaves the effect
//! unset, can make Explorer delete the source. wry's drop handler (0.57)
//! never sets the effect in `Drop`, so Grape registers this target in its
//! place on the overlay and every child window. It reports
//! `DROPEFFECT_COPY` for a file drag and `DROPEFFECT_NONE` for anything
//! else, in every callback, whatever keys are held. It never reports a move
//! or a link.
//!
//! Grape's other windows (Settings, the float shelf) get the same target
//! in refusing mode ([`refuse_drops`]): it answers `DROPEFFECT_NONE` to
//! everything, so OLE never calls `Drop` there and a file dropped on
//! Settings, or dragged from the shelf back onto it, stays put.
//!
//! The callbacks run on the window's thread (Tauri's main thread). They
//! read the file list and position, `try_send` a [`DropEvent`] and return.

use core::ffi::c_void;
use std::cell::Cell;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::mpsc::SyncSender;

use ::windows::core::{implement, Ref, BOOL};
use ::windows::Win32::Foundation::{E_HANDLE, HWND, LPARAM, POINTL};
use ::windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use ::windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium,
    RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use ::windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use ::windows::Win32::System::Threading::GetCurrentThreadId;
use ::windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use ::windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetWindowThreadProcessId, IsWindow,
};

use crate::{DropEvent, PlatformError};

/// How many of a window's HWNDs carry Grape's drop target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropTargetCoverage {
    /// The window plus all its descendants.
    pub windows: usize,
    /// How many of them Grape's target was registered on. Windows owned by
    /// another process (WebView2 can host some) can't be registered.
    pub registered: usize,
}

/// Registers Grape's drop target on the window `hwnd` and every descendant,
/// replacing whatever target each had (wry's or WebView2's). Events go to
/// `sink`. Calling it again re-registers, which picks up child windows
/// WebView2 created since; do that before each show.
///
/// `hwnd` is the raw handle value (`HWND.0 as isize`). Call this on the
/// thread that owns the window (Tauri's main thread). It initializes OLE on
/// that thread if needed.
///
/// # Errors
/// - [`PlatformError::WrongThread`] if the calling thread doesn't own the
///   window.
/// - [`PlatformError::Os`] if `hwnd` isn't a window, OLE can't start on
///   this thread, or the top-level window itself can't be registered.
pub fn install_drop_target(
    hwnd: isize,
    sink: SyncSender<DropEvent>,
) -> Result<DropTargetCoverage, PlatformError> {
    register(hwnd, OverlayDropTarget::new(Some(sink)))
}

/// Registers a drop target on the window `hwnd` and every descendant that
/// refuses every drag (`DROPEFFECT_NONE`), replacing wry's and tao's, which
/// don't set the effect in `Drop`. Use it on Grape's windows that take no
/// drops, such as Settings. Same threading rules and errors as
/// [`install_drop_target`].
///
/// # Errors
/// As [`install_drop_target`].
pub fn refuse_drops(hwnd: isize) -> Result<DropTargetCoverage, PlatformError> {
    register(hwnd, OverlayDropTarget::new(None))
}

fn register(hwnd: isize, target: OverlayDropTarget) -> Result<DropTargetCoverage, PlatformError> {
    let root = HWND(hwnd as *mut c_void);
    // SAFETY: IsWindow accepts any handle value and only reads state.
    if !unsafe { IsWindow(Some(root)) }.as_bool() {
        return Err(PlatformError::Os {
            context: "IsWindow",
            code: E_HANDLE.0,
        });
    }
    // SAFETY: plain queries; the process-id out-pointer is `None`.
    let (owner, current) = unsafe { (GetWindowThreadProcessId(root, None), GetCurrentThreadId()) };
    if owner != current {
        return Err(PlatformError::WrongThread);
    }
    // SAFETY: OleInitialize takes no pointers (the reserved argument must
    // be null). On a thread where OLE already runs it returns S_FALSE,
    // which is a success; it fails only if the thread is multithreaded.
    unsafe { OleInitialize(None) }.map_err(|err| PlatformError::Os {
        context: "OleInitialize",
        code: err.code().0,
    })?;

    let target: IDropTarget = target.into();
    let mut windows = vec![root];
    collect_descendants(root, &mut windows);

    let mut registered = 0;
    let mut root_error = None;
    for &window in &windows {
        // SAFETY: both calls take a window handle and, for Register, a live
        // COM object that OLE AddRefs and keeps. Revoking a window with no
        // target just returns DRAGDROP_E_NOTREGISTERED.
        let result = unsafe {
            let _ = RevokeDragDrop(window);
            RegisterDragDrop(window, &target)
        };
        match result {
            Ok(()) => registered += 1,
            Err(err) if window == root => root_error = Some(err.code().0),
            Err(_) => {}
        }
    }
    if let Some(code) = root_error {
        return Err(PlatformError::Os {
            context: "RegisterDragDrop",
            code,
        });
    }
    Ok(DropTargetCoverage {
        windows: windows.len(),
        registered,
    })
}

fn collect_descendants(root: HWND, out: &mut Vec<HWND>) {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = lparam.0 as *mut Vec<HWND>;
        // SAFETY: `lparam` is the `&mut Vec<HWND>` passed below, which
        // outlives the EnumChildWindows call that invokes this callback.
        if let Some(list) = unsafe { list.as_mut() } {
            list.push(hwnd);
        }
        true.into()
    }
    // SAFETY: the callback only pushes into the vector behind `lparam`,
    // which stays borrowed for the whole call. EnumChildWindows visits all
    // descendants, not just direct children.
    let _ = unsafe {
        EnumChildWindows(
            Some(root),
            Some(collect),
            LPARAM(ptr::from_mut(out) as isize),
        )
    };
}

/// The effect Grape reports: a copy for a file drag, otherwise nothing.
/// Never a move or a link (golden rule 2).
const fn effect_for(has_files: bool) -> DROPEFFECT {
    if has_files {
        DROPEFFECT_COPY
    } else {
        DROPEFFECT_NONE
    }
}

/// Writes `effect` to OLE's out-parameter.
///
/// # Safety
/// `out` must be null or point to a writable `DROPEFFECT`, as OLE's
/// `pdwEffect` does for the duration of a callback.
unsafe fn set_effect(out: *mut DROPEFFECT, effect: DROPEFFECT) {
    // SAFETY: the caller's contract; `as_mut` handles null.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = effect;
    }
}

#[implement(IDropTarget)]
struct OverlayDropTarget {
    /// Where events go. `None` refuses every drag and reports nothing.
    sink: Option<SyncSender<DropEvent>>,
    /// Whether the drag now over the window carries files Grape accepts.
    has_files: Cell<bool>,
}

impl OverlayDropTarget {
    fn new(sink: Option<SyncSender<DropEvent>>) -> Self {
        Self {
            sink,
            has_files: Cell::new(false),
        }
    }

    fn accepts(&self) -> bool {
        self.sink.is_some()
    }

    fn send(&self, event: DropEvent) {
        // Never block the OS drag loop: a full channel drops the event.
        if let Some(sink) = &self.sink {
            let _ = sink.try_send(event);
        }
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for OverlayDropTarget_Impl {
    fn DragEnter(
        &self,
        pDataObj: Ref<'_, IDataObject>,
        _grfKeyState: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdwEffect: *mut DROPEFFECT,
    ) -> ::windows::core::Result<()> {
        if !self.accepts() {
            // SAFETY: OLE's pdwEffect is valid for this call.
            unsafe { set_effect(pdwEffect, DROPEFFECT_NONE) };
            return Ok(());
        }
        let paths = read_paths(pDataObj.as_ref());
        let has_files = !paths.is_empty();
        self.has_files.set(has_files);
        self.send(DropEvent::Enter {
            paths,
            x: pt.x,
            y: pt.y,
        });
        // SAFETY: OLE's pdwEffect is valid for this call.
        unsafe { set_effect(pdwEffect, effect_for(has_files)) };
        Ok(())
    }

    fn DragOver(
        &self,
        _grfKeyState: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdwEffect: *mut DROPEFFECT,
    ) -> ::windows::core::Result<()> {
        let has_files = self.has_files.get();
        if has_files {
            self.send(DropEvent::Over { x: pt.x, y: pt.y });
        }
        // SAFETY: OLE's pdwEffect is valid for this call.
        unsafe { set_effect(pdwEffect, effect_for(has_files)) };
        Ok(())
    }

    fn DragLeave(&self) -> ::windows::core::Result<()> {
        self.has_files.set(false);
        if self.accepts() {
            self.send(DropEvent::Leave);
        }
        Ok(())
    }

    fn Drop(
        &self,
        pDataObj: Ref<'_, IDataObject>,
        _grfKeyState: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdwEffect: *mut DROPEFFECT,
    ) -> ::windows::core::Result<()> {
        self.has_files.set(false);
        if !self.accepts() {
            // SAFETY: OLE's pdwEffect is valid for this call.
            unsafe { set_effect(pdwEffect, DROPEFFECT_NONE) };
            return Ok(());
        }
        let paths = read_paths(pDataObj.as_ref());
        let has_files = !paths.is_empty();
        self.send(DropEvent::Drop {
            paths,
            x: pt.x,
            y: pt.y,
        });
        // SAFETY: OLE's pdwEffect is valid for this call. This is the value
        // Explorer acts on, so it must never be a move.
        unsafe { set_effect(pdwEffect, effect_for(has_files)) };
        Ok(())
    }
}

/// The file paths in a drag's data object (`CF_HDROP`), or none.
pub(crate) fn read_paths(data: Option<&IDataObject>) -> Vec<PathBuf> {
    let Some(data) = data else {
        return Vec::new();
    };
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: `format` describes CF_HDROP in an HGLOBAL. On success the
    // medium is ours to release, which happens below.
    let Ok(mut medium) = (unsafe { data.GetData(&format) }) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    if medium.tymed == TYMED_HGLOBAL.0 as u32 {
        // SAFETY: with tymed == TYMED_HGLOBAL the union holds an HGLOBAL,
        // and for CF_HDROP that HGLOBAL is a DROPFILES block, which is what
        // an HDROP is.
        let hdrop = HDROP(unsafe { medium.u.hGlobal }.0);
        // SAFETY: index 0xFFFFFFFF with no buffer asks for the file count.
        let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
        for index in 0..count {
            // SAFETY: no buffer: returns the length of name `index`,
            // without the terminator.
            let len = unsafe { DragQueryFileW(hdrop, index, None) } as usize;
            let mut name = vec![0_u16; len + 1];
            // SAFETY: `name` has room for the name and its terminator; the
            // call writes at most `name.len()` characters.
            let copied = unsafe { DragQueryFileW(hdrop, index, Some(&mut name)) } as usize;
            if let Some(chars) = name.get(..copied.min(len)) {
                paths.push(PathBuf::from(OsString::from_wide(chars)));
            }
        }
    }
    // SAFETY: releases the medium GetData handed us, exactly once.
    unsafe { ReleaseStgMedium(&mut medium) };
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    use ::windows::core::{w, HSTRING};
    use ::windows::Win32::System::Ole::{DROPEFFECT_LINK, DROPEFFECT_MOVE};
    use ::windows::Win32::System::SystemServices::{MK_CONTROL, MK_LBUTTON, MK_SHIFT};
    use ::windows::Win32::UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName};
    use ::windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };

    /// OLE's "Alt is down" key-state bit (oleidl.h), missing from the
    /// generated bindings.
    const MK_ALT: MODIFIERKEYS_FLAGS = MODIFIERKEYS_FLAGS(0x0020);

    /// Everything a source may allow: what an Explorer drag offers.
    const ANY_EFFECT: DROPEFFECT =
        DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0);

    fn ole() {
        // SAFETY: initializes OLE on the test thread; no pointers.
        unsafe { OleInitialize(None) }.unwrap();
    }

    /// A temporary file, deleted on drop.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("grape-drop-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, b"grape").unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// The shell's own data object for a file, as Explorer hands to a drop
    /// target.
    fn shell_data_object(path: &std::path::Path) -> IDataObject {
        let path = HSTRING::from(path.as_os_str());
        // SAFETY: a valid, NUL-terminated path and no bind context.
        let item: IShellItem = unsafe { SHCreateItemFromParsingName(&path, None) }.unwrap();
        // SAFETY: asks the item for its data object; no bind context.
        unsafe { item.BindToHandler(None, &BHID_DataObject) }.unwrap()
    }

    fn target() -> (IDropTarget, mpsc::Receiver<DropEvent>) {
        let (tx, rx) = mpsc::sync_channel(16);
        (OverlayDropTarget::new(Some(tx)).into(), rx)
    }

    fn keys(flags: &[MODIFIERKEYS_FLAGS]) -> MODIFIERKEYS_FLAGS {
        MODIFIERKEYS_FLAGS(flags.iter().fold(MK_LBUTTON.0, |all, flag| all | flag.0))
    }

    fn file_name(event: &DropEvent) -> Option<String> {
        let paths = match event {
            DropEvent::Enter { paths, .. } | DropEvent::Drop { paths, .. } => paths,
            _ => return None,
        };
        let [path] = paths.as_slice() else {
            return None;
        };
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }

    /// Golden rule 2: whatever modifier is held and whatever the source
    /// allows, the overlay reports a copy, so Explorer never deletes the
    /// source.
    #[test]
    fn a_file_drag_is_always_a_copy() {
        ole();
        let file = TempFile::new("golden rule 2.png");
        let data = shell_data_object(&file.0);
        for held in [
            &[MK_SHIFT][..],
            &[MK_CONTROL],
            &[MK_ALT],
            &[MK_SHIFT, MK_CONTROL],
            &[],
        ] {
            let (target, rx) = target();
            let at = POINTL { x: 120, y: -40 };

            let mut effect = ANY_EFFECT;
            // SAFETY: a live data object and a valid out-pointer.
            unsafe { target.DragEnter(&data, keys(held), at, &mut effect) }.unwrap();
            assert_eq!(effect, DROPEFFECT_COPY, "DragEnter with {held:?}");

            let mut effect = DROPEFFECT_MOVE;
            // SAFETY: a valid out-pointer.
            unsafe { target.DragOver(keys(held), at, &mut effect) }.unwrap();
            assert_eq!(effect, DROPEFFECT_COPY, "DragOver with {held:?}");

            let mut effect = ANY_EFFECT;
            // SAFETY: a live data object and a valid out-pointer.
            unsafe { target.Drop(&data, keys(held), at, &mut effect) }.unwrap();
            assert_eq!(effect, DROPEFFECT_COPY, "Drop with {held:?}");

            let events: Vec<DropEvent> = rx.try_iter().collect();
            assert_eq!(events.len(), 3, "{events:?}");
            assert_eq!(
                events.first().and_then(file_name).as_deref(),
                Some("golden rule 2.png")
            );
            assert_eq!(events.get(1), Some(&DropEvent::Over { x: 120, y: -40 }));
            assert_eq!(
                events.get(2).and_then(file_name).as_deref(),
                Some("golden rule 2.png")
            );
        }
        assert!(file.0.exists());
    }

    /// Settings and other windows that take no drops: nothing is accepted,
    /// so OLE never calls Drop and the source is never told "moved".
    #[test]
    fn a_refusing_target_refuses_file_drags_too() {
        ole();
        let file = TempFile::new("refused.png");
        let data = shell_data_object(&file.0);
        let target: IDropTarget = OverlayDropTarget::new(None).into();
        let at = POINTL { x: 1, y: 2 };
        let mut effect = ANY_EFFECT;
        // SAFETY: a live data object and a valid out-pointer.
        unsafe { target.DragEnter(&data, keys(&[MK_SHIFT]), at, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        let mut effect = ANY_EFFECT;
        // SAFETY: a valid out-pointer.
        unsafe { target.DragOver(keys(&[MK_SHIFT]), at, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        let mut effect = ANY_EFFECT;
        // SAFETY: a live data object and a valid out-pointer.
        unsafe { target.Drop(&data, keys(&[MK_SHIFT]), at, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        assert!(file.0.exists());
    }

    #[test]
    fn a_drag_without_files_is_refused() {
        ole();
        let (target, rx) = target();
        let at = POINTL { x: 5, y: 6 };
        let mut effect = ANY_EFFECT;
        // SAFETY: no data object (null) and a valid out-pointer.
        unsafe { target.DragEnter(None, keys(&[MK_SHIFT]), at, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        let mut effect = ANY_EFFECT;
        // SAFETY: a valid out-pointer.
        unsafe { target.DragOver(keys(&[MK_SHIFT]), at, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        let mut effect = ANY_EFFECT;
        // SAFETY: no data object (null) and a valid out-pointer.
        unsafe { target.Drop(None, keys(&[MK_SHIFT]), at, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        let events: Vec<DropEvent> = rx.try_iter().collect();
        assert_eq!(
            events,
            vec![
                DropEvent::Enter {
                    paths: Vec::new(),
                    x: 5,
                    y: 6
                },
                DropEvent::Drop {
                    paths: Vec::new(),
                    x: 5,
                    y: 6
                },
            ]
        );
    }

    #[test]
    fn leave_resets_the_drag() {
        ole();
        let file = TempFile::new("leave.txt");
        let data = shell_data_object(&file.0);
        let (target, rx) = target();
        let at = POINTL { x: 0, y: 0 };
        let mut effect = ANY_EFFECT;
        // SAFETY: a live data object and a valid out-pointer.
        unsafe { target.DragEnter(&data, keys(&[]), at, &mut effect) }.unwrap();
        // SAFETY: no arguments.
        unsafe { target.DragLeave() }.unwrap();
        let mut effect = ANY_EFFECT;
        // SAFETY: a valid out-pointer.
        unsafe { target.DragOver(keys(&[]), at, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        let events: Vec<DropEvent> = rx.try_iter().collect();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events.get(1), Some(&DropEvent::Leave));
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
                    w!("grape drop-target test"),
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
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: this thread's own window, revoked and destroyed once.
            unsafe {
                let _ = RevokeDragDrop(self.0);
                let _ = DestroyWindow(self.0);
            }
        }
    }

    #[test]
    fn install_registers_and_can_run_again() {
        let window = TestWindow::new();
        let (tx, _rx) = mpsc::sync_channel(4);
        let raw = window.0 .0 as isize;
        let first = install_drop_target(raw, tx.clone()).unwrap();
        assert_eq!(
            first,
            DropTargetCoverage {
                windows: 1,
                registered: 1
            }
        );
        let again = install_drop_target(raw, tx).unwrap();
        assert_eq!(again, first);
        let refused = refuse_drops(raw).unwrap();
        assert_eq!(refused, first);
    }

    #[test]
    fn install_rejects_bad_handles_and_other_threads() {
        let (tx, _rx) = mpsc::sync_channel(4);
        assert!(matches!(
            install_drop_target(0, tx.clone()),
            Err(PlatformError::Os {
                context: "IsWindow",
                ..
            })
        ));
        let window = TestWindow::new();
        let raw = window.0 .0 as isize;
        let result = std::thread::spawn(move || install_drop_target(raw, tx))
            .join()
            .unwrap();
        assert_eq!(result, Err(PlatformError::WrongThread));
    }
}
