//! The rest of the Shortcuts wheel on macOS, behind [`crate::shortcuts`]
//! (`docs/design/m10-shortcuts-wheel.md` §4): opening things, the programs
//! that run scripts, ending a program's process group, moving another app's
//! window, handing the front back, and the app and file pickers. Hot keys
//! are in `hotkey`; keys, typing, the clipboard and the pointer in `input`.
//!
//! - Open: `/usr/bin/open`, waited for, so a target that can't be opened is
//!   an error. An app goes by its bundle id first and by its path second.
//! - Programs and system controls are built by `crate::mac_rules`; nothing
//!   runs here.
//! - Windows: Accessibility, declared by hand. The focused window (or else
//!   the main one) of the app is read, placed with
//!   [`placement_frame`] on the display its center is on, and set: position,
//!   size, position, because an app may clamp a size to the screen the
//!   window was on. A window already the right size is only moved, so one
//!   that can't be resized can still be centered. Each call to another app
//!   waits at most a second.
//! - The pickers are `NSOpenPanel` on the main thread, as Settings' folder
//!   picker is. They neither activate Grape nor hand anything back: the
//!   Settings window is already key when they open.
//!
//! Nothing here logs a path, a bundle id, a name or a URL.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr::{self, NonNull};
use std::thread;
use std::time::{Duration, Instant};

use dispatch2::run_on_main;
use objc2::rc::autoreleasepool;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSModalResponseOK, NSOpenPanel, NSRunningApplication, NSWorkspace};
use objc2_core_foundation::{CFGetTypeID, CFRetained, CFString, CFType, CFTypeID, CGPoint, CGSize};
use objc2_foundation::{ns_string, NSArray, NSBundle, NSFileManager, NSString, NSURL};

use super::actions::hand_back;
use super::activations::running_app;
use super::{display, hand_back_activation, io_error, NO_CODE};
use crate::mac_rules::{self, app_name, ax_frame, center, killable_group, open_programs};
use crate::shortcuts::{
    placement_frame, AppIdentity, OpenTarget, PickKind, Picked, Placement, Program, ProgramRequest,
    SystemControl, SystemPlan,
};
use crate::{PlatformError, Rect};

/// How long a program's group has to end by itself before it is killed.
const KILL_GRACE: Duration = Duration::from_millis(500);
/// How often [`ensure_front`] looks at the app in front.
const FRONT_POLL: Duration = Duration::from_millis(10);
/// How long one Accessibility call waits for the other app, in seconds.
const AX_TIMEOUT: f32 = 1.0;

/// Accessibility's `AXError`.
type AxError = i32;
/// `kAXErrorSuccess`.
const AX_SUCCESS: AxError = 0;
/// `kAXValueTypeCGPoint`.
const AX_VALUE_POINT: u32 = 1;
/// `kAXValueTypeCGSize`.
const AX_VALUE_SIZE: u32 = 2;

// An `AXUIElementRef` and an `AXValueRef` are CoreFoundation objects, so
// both are `CFType` here and released like one.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: libc::pid_t) -> Option<NonNull<CFType>>;
    fn AXUIElementGetTypeID() -> CFTypeID;
    fn AXUIElementSetMessagingTimeout(element: &CFType, seconds: f32) -> AxError;
    fn AXUIElementCopyAttributeValue(
        element: &CFType,
        attribute: &CFString,
        value: *mut *const CFType,
    ) -> AxError;
    fn AXUIElementSetAttributeValue(
        element: &CFType,
        attribute: &CFString,
        value: &CFType,
    ) -> AxError;
    fn AXValueCreate(kind: u32, value: *const c_void) -> Option<NonNull<CFType>>;
    fn AXValueGetTypeID() -> CFTypeID;
    fn AXValueGetValue(value: &CFType, kind: u32, out: *mut c_void) -> u8;
}

/// Runs `work` on the main thread (inline if this is it) inside an
/// autorelease pool, and turns an Objective-C exception into an error
/// instead of a crash, as the file actions do.
///
/// It waits for the main thread, so never call it from a thread that the
/// main thread is itself waiting for.
pub(super) fn on_main<R: Send>(
    context: &'static str,
    work: impl Send + FnOnce(MainThreadMarker) -> Result<R, PlatformError>,
) -> Result<R, PlatformError> {
    run_on_main(|mtm| {
        autoreleasepool(|_| {
            objc2::exception::catch(AssertUnwindSafe(|| work(mtm))).unwrap_or(Err(
                PlatformError::Os {
                    context,
                    code: NO_CODE,
                },
            ))
        })
    })
}

/// Runs a program that takes nothing on its standard input and waits for
/// it. An exit code other than 0 is an error with that code.
pub(super) fn run(program: &Program, context: &'static str) -> Result<(), PlatformError> {
    let status = Command::new(&program.program)
        .args(&program.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| io_error(context, &err))?;
    if status.success() {
        Ok(())
    } else {
        Err(PlatformError::Os {
            context,
            code: status.code().unwrap_or(NO_CODE),
        })
    }
}

pub(crate) fn open_target(target: OpenTarget<'_>) -> Result<(), PlatformError> {
    let mut answer = Err(PlatformError::Os {
        context: "open",
        code: NO_CODE,
    });
    // An app is tried by its id first, then by its path.
    for program in open_programs(target) {
        answer = run(&program, "open");
        if answer.is_ok() {
            break;
        }
    }
    answer
}

pub(crate) fn program_for(request: ProgramRequest<'_>) -> Result<Program, PlatformError> {
    Ok(mac_rules::program_for(request))
}

pub(crate) fn system_plan(control: SystemControl) -> Result<SystemPlan, PlatformError> {
    Ok(mac_rules::system_plan(control))
}

/// Asks the group to end, and kills what is left of it after a short
/// grace, on a thread of its own so the caller never waits.
pub(crate) fn kill_group(pid: u32) {
    // SAFETY: takes nothing and can't fail.
    let own_group = unsafe { libc::getpgrp() };
    let Some(group) = killable_group(pid, own_group) else {
        return;
    };
    // SAFETY: two plain integers. A group that is gone, or isn't Grape's
    // to signal, is an error `killpg` returns.
    if unsafe { libc::killpg(group, libc::SIGTERM) } != 0 {
        return;
    }
    // If the thread can't start, the group only got the polite signal.
    let _ = thread::Builder::new()
        .name("grape-kill".to_owned())
        .spawn(move || {
            thread::sleep(KILL_GRACE);
            // SAFETY: as above. While a process of the group is left its
            // id names that group. Once all are gone it names none until
            // the pid counter comes round to it again, which takes far
            // longer than this wait.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        });
}

pub(crate) fn place_window(pid: i32, placement: Placement) -> Result<(), PlatformError> {
    // SAFETY: takes a plain pid and returns an element the caller owns (the
    // Create rule), or null.
    let app = unsafe { AXUIElementCreateApplication(pid) };
    // SAFETY: a non-null answer is that owned element, a CF object, which
    // `CFRetained` releases.
    let app = app
        .map(|app| unsafe { CFRetained::from_raw(app) })
        .ok_or(PlatformError::Os {
            context: "AXUIElementCreateApplication",
            code: NO_CODE,
        })?;
    wait_at_most_a_second(&app);
    let window = copy_attribute(&app, "AXFocusedWindow")
        .or_else(|_| copy_attribute(&app, "AXMainWindow"))
        .map_err(|code| PlatformError::Os {
            context: "finding the app's window",
            code,
        })?;
    // SAFETY: takes nothing and returns Accessibility's type id for
    // elements.
    if CFGetTypeID(Some(&window)) != unsafe { AXUIElementGetTypeID() } {
        return Err(PlatformError::Os {
            context: "finding the app's window",
            code: NO_CODE,
        });
    }
    wait_at_most_a_second(&window);

    let at = read_value(
        &window,
        "AXPosition",
        CGPoint { x: 0.0, y: 0.0 },
        "reading the window's position",
    )?;
    let size = read_value(
        &window,
        "AXSize",
        CGSize {
            width: 0.0,
            height: 0.0,
        },
        "reading the window's size",
    )?;
    // Accessibility's points have a top-left origin, as Grape's do.
    let frame = ax_frame(at.x, at.y, size.width, size.height);
    let (x, y) = center(frame);
    let work_area = display::display_bounds_at(x, y)?.work_area;
    let target = placement_frame(placement, work_area, (frame.width, frame.height));

    set_position(&window, target)?;
    if (target.width, target.height) != (frame.width, frame.height) {
        set_size(&window, target)?;
        // Again: the app may have clamped the first move to the old size.
        set_position(&window, target)?;
    }
    Ok(())
}

/// Caps how long calls to `element`'s app wait for it.
fn wait_at_most_a_second(element: &CFType) {
    // SAFETY: `element` is a live Accessibility element. A refusal only
    // leaves the system's own, longer timeout.
    let _ = unsafe { AXUIElementSetMessagingTimeout(element, AX_TIMEOUT) };
}

/// One attribute of an Accessibility element, owned by the caller.
fn copy_attribute(
    element: &CFType,
    attribute: &'static str,
) -> Result<CFRetained<CFType>, AxError> {
    let name = CFString::from_static_str(attribute);
    let mut value: *const CFType = ptr::null();
    // SAFETY: `element` is a live Accessibility element, `name` a live
    // CFString, and `value` a valid place for the one pointer the call
    // writes.
    let status = unsafe { AXUIElementCopyAttributeValue(element, &name, &mut value) };
    // SAFETY: the Copy rule: a value written there is a CF object the
    // caller owns, so `CFRetained` must release it, whatever the status.
    let value = NonNull::new(value.cast_mut()).map(|value| unsafe { CFRetained::from_raw(value) });
    match value {
        Some(value) if status == AX_SUCCESS => Ok(value),
        _ if status == AX_SUCCESS => Err(NO_CODE),
        _ => Err(status),
    }
}

/// A struct an `AXValue` can hold.
///
/// # Safety
/// `KIND` must be the `AXValueType` whose values are exactly a `Self`.
unsafe trait AxStruct: Copy {
    /// Its `AXValueType`.
    const KIND: u32;
}

// SAFETY: a `kAXValueTypeCGPoint` value holds one `CGPoint`.
unsafe impl AxStruct for CGPoint {
    const KIND: u32 = AX_VALUE_POINT;
}

// SAFETY: a `kAXValueTypeCGSize` value holds one `CGSize`.
unsafe impl AxStruct for CGSize {
    const KIND: u32 = AX_VALUE_SIZE;
}

/// Reads a window's point or size attribute. `start` is only the value the
/// answer is written over.
fn read_value<T: AxStruct>(
    window: &CFType,
    attribute: &'static str,
    start: T,
    context: &'static str,
) -> Result<T, PlatformError> {
    let value =
        copy_attribute(window, attribute).map_err(|code| PlatformError::Os { context, code })?;
    // SAFETY: takes nothing and returns Accessibility's type id for values.
    let is_value = CFGetTypeID(Some(&value)) == unsafe { AXValueGetTypeID() };
    let mut out = start;
    // SAFETY: `value` was just checked to be an AXValue. `out` is a live
    // `T`, which is what a value of `T::KIND` writes (`AxStruct`), and the
    // call writes nothing when the value holds another kind.
    if is_value && unsafe { AXValueGetValue(&value, T::KIND, (&raw mut out).cast()) } != 0 {
        Ok(out)
    } else {
        Err(PlatformError::Os {
            context,
            code: NO_CODE,
        })
    }
}

/// Moves a window's top-left corner to `to`'s.
fn set_position(window: &CFType, to: Rect) -> Result<(), PlatformError> {
    let point = CGPoint {
        x: f64::from(to.x),
        y: f64::from(to.y),
    };
    set_value(window, "AXPosition", point, "moving the window")
}

/// Gives a window `to`'s size.
fn set_size(window: &CFType, to: Rect) -> Result<(), PlatformError> {
    let size = CGSize {
        width: f64::from(to.width),
        height: f64::from(to.height),
    };
    set_value(window, "AXSize", size, "resizing the window")
}

/// Sets a window's point or size attribute.
fn set_value<T: AxStruct>(
    window: &CFType,
    attribute: &'static str,
    from: T,
    context: &'static str,
) -> Result<(), PlatformError> {
    // SAFETY: `from` is a live `T`, which is what a value of `T::KIND`
    // copies in (`AxStruct`). The call returns a value the caller owns (the
    // Create rule), or null.
    let value = unsafe { AXValueCreate(T::KIND, (&raw const from).cast()) };
    // SAFETY: a non-null answer is that owned value, a CF object, which
    // `CFRetained` releases.
    let value = value
        .map(|value| unsafe { CFRetained::from_raw(value) })
        .ok_or(PlatformError::Os {
            context: "AXValueCreate",
            code: NO_CODE,
        })?;
    let name = CFString::from_static_str(attribute);
    // SAFETY: `window` is a live Accessibility element, and `name` and
    // `value` are live CF objects for the whole call.
    let status = unsafe { AXUIElementSetAttributeValue(window, &name, &value) };
    if status == AX_SUCCESS {
        Ok(())
    } else {
        Err(PlatformError::Os {
            context,
            code: status,
        })
    }
}

/// On the main thread, where the app queues it after the overlay's hide.
/// Anywhere else it does nothing.
pub(crate) fn give_back_front() {
    let _ = hand_back_activation();
}

pub(crate) fn ensure_front(pid: i32, timeout: Duration) -> bool {
    let started = Instant::now();
    // The main thread can't wait for itself to hand the front over.
    let may_wait = MainThreadMarker::new().is_none();
    loop {
        let (front, grape_active) = autoreleasepool(|_| {
            let front = NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .map(|app| app.processIdentifier());
            let grape = NSRunningApplication::currentApplication();
            (front, grape.isActive())
        });
        if pid > 0 && front == Some(pid) {
            return true;
        }
        if !may_wait || started.elapsed() >= timeout {
            return false;
        }
        if grape_active && pid > 0 {
            // A courtesy that may fail: the loop just looks again.
            let _ = on_main("handing the front back", move |mtm| {
                if let Some(app) = running_app(pid) {
                    hand_back(mtm, &app);
                }
                Ok(())
            });
        }
        thread::sleep(FRONT_POLL);
    }
}

/// `_owner` has no use here: the panel is modal to the whole app.
pub(crate) fn pick_target(
    kind: PickKind,
    _owner: Option<isize>,
) -> Result<Option<Picked>, PlatformError> {
    let context = match kind {
        PickKind::App => "the app picker",
        PickKind::FileOrFolder => "the file picker",
    };
    on_main(context, move |mtm| Ok(pick(mtm, kind)))
}

/// Runs the open panel for `kind`. `None` when the user cancels.
fn pick(mtm: MainThreadMarker, kind: PickKind) -> Option<Picked> {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(false);
    panel.setCanChooseFiles(true);
    panel.setPrompt(Some(ns_string!("Choose")));
    match kind {
        PickKind::App => {
            panel.setCanChooseDirectories(false);
            panel.setMessage(Some(ns_string!("Choose an app")));
            let apps = NSArray::from_slice(&[ns_string!("app")]);
            // The call that replaces this one takes UTTypes, which are in a
            // framework Grape doesn't link.
            #[allow(deprecated)]
            panel.setAllowedFileTypes(Some(&apps));
            if let Some(applications) = NSURL::from_directory_path("/Applications") {
                panel.setDirectoryURL(Some(&applications));
            }
        }
        PickKind::FileOrFolder => {
            panel.setCanChooseDirectories(true);
            panel.setMessage(Some(ns_string!("Choose a file or a folder")));
        }
    }
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    let url = panel.URLs().iter().next()?;
    // A folder URL ends in `/`; the components drop it.
    let path: PathBuf = url_path(&url)?.components().collect();
    Some(match kind {
        PickKind::App => Picked {
            app: app_identity(&url, &path),
            path,
            folder: false,
        },
        PickKind::FileOrFolder => Picked {
            folder: is_plain_folder(&path),
            path,
            app: None,
        },
    })
}

/// A URL's file-system path. `to_file_path` stops at 1024 bytes, so longer
/// paths come through `path` instead.
fn url_path(url: &NSURL) -> Option<PathBuf> {
    url.to_file_path()
        .or_else(|| url.path().map(|path| PathBuf::from(path.to_string())))
}

/// Who the app at `url` is: its bundle id and the name Finder shows for
/// it. `None` for a bundle with no id.
fn app_identity(url: &NSURL, path: &Path) -> Option<AppIdentity> {
    let id = NSBundle::bundleWithURL(url)?
        .bundleIdentifier()?
        .to_string();
    let display = path.to_str().map(|path| {
        NSFileManager::defaultManager()
            .displayNameAtPath(&NSString::from_str(path))
            .to_string()
    });
    Some(AppIdentity {
        id,
        name: app_name(display.as_deref().unwrap_or_default(), path),
    })
}

/// `true` for a folder the user sees as one: an app or another package is
/// a folder on disk, but a file in Finder.
fn is_plain_folder(path: &Path) -> bool {
    path.is_dir()
        && !path.to_str().is_some_and(|path| {
            NSWorkspace::sharedWorkspace().isFilePackageAtPath(&NSString::from_str(path))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn program(path: &str, args: &[&str]) -> Program {
        Program {
            program: PathBuf::from(path),
            args: args.iter().map(OsString::from).collect(),
            stdin: None,
        }
    }

    #[test]
    fn a_program_that_fails_is_an_error_with_its_exit_code() {
        assert_eq!(run(&program("/usr/bin/true", &[]), "true"), Ok(()));
        assert_eq!(
            run(&program("/usr/bin/false", &[]), "false"),
            Err(PlatformError::Os {
                context: "false",
                code: 1,
            })
        );
        assert_eq!(
            run(&program("/bin/sh", &["-c", "exit 7"]), "sh"),
            Err(PlatformError::Os {
                context: "sh",
                code: 7,
            })
        );
        // One that can't start: the errno.
        assert!(matches!(
            run(&program("/nonexistent/grape-test-program", &[]), "missing"),
            Err(PlatformError::Os {
                context: "missing",
                code: 2,
            })
        ));
    }

    #[test]
    fn the_rules_answer_for_programs_and_system_controls() {
        let script = program_for(ProgramRequest::AppleScript("beep")).unwrap();
        assert_eq!(
            script,
            mac_rules::program_for(ProgramRequest::AppleScript("beep"))
        );
        assert_eq!(
            system_plan(SystemControl::SleepDisplay).unwrap(),
            mac_rules::system_plan(SystemControl::SleepDisplay)
        );
    }

    #[test]
    fn no_group_that_is_not_a_programs_own_is_signalled() {
        // The caller's group, launchd's, a pid that isn't one, and Grape's
        // own: each returns before any signal.
        kill_group(0);
        kill_group(1);
        kill_group(u32::MAX);
        // SAFETY: takes nothing and can't fail.
        let own = unsafe { libc::getpgrp() };
        kill_group(u32::try_from(own).unwrap());
    }

    #[test]
    fn a_group_of_its_own_is_ended() {
        use std::os::unix::process::CommandExt;

        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        kill_group(child.id());
        // The polite signal is enough for `sleep`.
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn waiting_for_an_app_that_is_not_there_gives_up() {
        // No process has pid 0, and a test thread isn't the main one.
        let started = Instant::now();
        assert!(!ensure_front(0, Duration::from_millis(30)));
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn handing_the_front_back_off_the_main_thread_does_nothing() {
        give_back_front();
    }

    #[test]
    fn a_package_is_not_a_plain_folder() {
        assert!(is_plain_folder(Path::new("/usr")));
        assert!(!is_plain_folder(Path::new("/usr/bin/true")));
        assert!(!is_plain_folder(Path::new(
            "/System/Applications/Utilities/Terminal.app"
        )));
        assert!(!is_plain_folder(Path::new(
            "/nonexistent/grape-test-folder"
        )));
    }
}
