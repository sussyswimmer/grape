//! File actions on macOS (`docs/PLATFORM_MACOS.md` → File actions), and the
//! haptic tick.
//!
//! - Copy: file URLs on the general pasteboard, so pasting in Finder makes
//!   copies.
//! - Copy Path: the paths as text, one per line.
//! - Terminal: `open -a Terminal <dir>`. Reveal: `open -R <path>`.
//! - Share: AirDrop through `NSSharingService`.
//! - Move: `NSOpenPanel` picks a folder, an `NSAlert` confirms a move to
//!   another volume, then `NSFileManager` moves each item. It never
//!   replaces anything: an item whose name is taken in the folder stays
//!   where it is. A mounted volume's root (a USB stick, a DMG, a network
//!   share) never moves either: moving it would copy what's on it, then
//!   erase it. Each item is read back from the disk right after its own
//!   move, before the next one moves, and judged by
//!   `crate::mac_rules::judge_move`.
//! - Pick a folder (Settings): the same `NSOpenPanel` as Move's, with
//!   "Choose" on its button.
//!
//! The panel, the alert and AirDrop are AppKit UI, so they run on the main
//! thread: inline when the caller is on it, otherwise through the main
//! queue, waiting for it. Grape is an accessory app that's rarely active,
//! so Move and Share activate it first, or they could open behind the
//! frontmost window. Move then hands activation back to the app that was in
//! front, whatever the user answered; Share does the same when AirDrop
//! reports that it's done or was cancelled. Settings' picker does neither:
//! the Settings window is already key when the user asks for it. The rest
//! runs on the calling thread inside an autorelease pool. `open` runs as its
//! own process with each path as its own argument, never through a shell.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use dispatch2::run_on_main;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertSecondButtonReturn, NSAlertStyle, NSApplication,
    NSApplicationActivationOptions, NSHapticFeedbackManager, NSHapticFeedbackPattern,
    NSHapticFeedbackPerformanceTime, NSHapticFeedbackPerformer, NSModalResponseOK, NSOpenPanel,
    NSPasteboard, NSPasteboardTypeString, NSPasteboardWriting, NSRunningApplication,
    NSSharingService, NSSharingServiceDelegate, NSSharingServiceNameSendViaAirDrop, NSWorkspace,
};
use objc2_foundation::{NSArray, NSError, NSFileManager, NSString, NSURL};

use super::{io_error, FEATURE_UNSUPPORTED, NO_CODE};
use crate::file_actions::{MoveOutcome, MovedItem};
use crate::mac_rules::{
    judge_move, path_lines, picker_message, reveal_args, terminal_args, ItemDevices, MoveSnapshot,
    MoveVerdict, OldPlace,
};
use crate::PlatformError;

/// The tool that opens files, folders and apps the way Finder does.
const OPEN: &str = "/usr/bin/open";

pub(crate) fn copy_files(paths: &[PathBuf]) -> Result<(), PlatformError> {
    autoreleasepool(|_| {
        let urls = file_urls(paths)?;
        let writers: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = urls
            .iter()
            .map(|url| ProtocolObject::from_ref(&**url))
            .collect();
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        if pasteboard.writeObjects(&NSArray::from_slice(&writers)) {
            Ok(())
        } else {
            Err(PlatformError::Os {
                context: "NSPasteboard writeObjects",
                code: NO_CODE,
            })
        }
    })
}

pub(crate) fn copy_paths(paths: &[PathBuf]) -> Result<(), PlatformError> {
    autoreleasepool(|_| {
        let text = NSString::from_str(&path_lines(paths));
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        // SAFETY: NSPasteboardTypeString is an immutable NSString constant
        // that AppKit defines for the life of the process.
        let string_type = unsafe { NSPasteboardTypeString };
        if pasteboard.setString_forType(&text, string_type) {
            Ok(())
        } else {
            Err(PlatformError::Os {
                context: "NSPasteboard setString",
                code: NO_CODE,
            })
        }
    })
}

pub(crate) fn open_terminal(dir: &Path) -> Result<(), PlatformError> {
    open(terminal_args(dir), "start open -a Terminal")
}

pub(crate) fn reveal(path: &Path) -> Result<(), PlatformError> {
    // `open -R` on a missing path only fails later, where Grape can't see
    // it, so check first.
    path.symlink_metadata()
        .map_err(|err| io_error("finding the item to reveal", &err))?;
    open(reveal_args(path), "start open -R")
}

pub(crate) fn share(paths: &[PathBuf]) -> Result<(), PlatformError> {
    let urls = autoreleasepool(|_| file_urls(paths))?;
    on_main("AirDrop", move |mtm| {
        let unavailable = PlatformError::Os {
            context: "AirDrop",
            code: FEATURE_UNSUPPORTED,
        };
        // SAFETY: NSSharingServiceNameSendViaAirDrop is an immutable
        // NSString constant that AppKit defines for the life of the process.
        let name = unsafe { NSSharingServiceNameSendViaAirDrop };
        let service = NSSharingService::sharingServiceNamed(name).ok_or(unavailable.clone())?;
        let items = NSArray::from_retained_slice(&urls);
        // SAFETY: an NSArray<NSURL> is an NSArray of objects; the cast only
        // forgets the element type. NSURL is a pasteboard writer, which is
        // what the sharing service expects of its items.
        let items: &NSArray<AnyObject> = unsafe { items.cast_unchecked::<AnyObject>() };
        // SAFETY: `items` holds only file URLs, as above.
        if !unsafe { service.canPerformWithItems(Some(items)) } {
            return Err(unavailable);
        }
        // The delegate gives activation back to this app once AirDrop is
        // done, as Move does after its panel.
        let done = ShareDone::new(mtm, frontmost_other_app(mtm));
        service.setDelegate(Some(ProtocolObject::from_ref(&*done)));
        // Or the AirDrop window may open behind the frontmost app's.
        activate(mtm);
        // SAFETY: as above, and the service said it can take them.
        unsafe { service.performWithItems(items) };
        // The service holds its delegate weakly, so keep both until AirDrop
        // reports back. Finished pairs go now, never from inside one of
        // their own callbacks; a share that never reports goes once
        // SHARES_KEPT newer ones have started.
        SHARING.with(|open| {
            let mut open = open.borrow_mut();
            open.retain(|(_, done)| !done.ivars().finished.get());
            if open.len() >= SHARES_KEPT {
                open.remove(0);
            }
            open.push((service, done));
        });
        Ok(())
    })
}

/// What [`ShareDone`] needs once AirDrop is done.
struct ShareIvars {
    /// The app that was in front before Grape opened AirDrop.
    front: Option<Retained<NSRunningApplication>>,
    /// AirDrop has reported back.
    finished: Cell<bool>,
}

/// How many AirDrop shares that haven't reported back Grape keeps.
const SHARES_KEPT: usize = 8;

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and ShareDone
    // doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GrapeShareDone"]
    #[ivars = ShareIvars]
    /// AirDrop's delegate: hands activation back when the share ends,
    /// whether it was sent, failed or was cancelled.
    struct ShareDone;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for ShareDone {}

    // SAFETY: both methods have the signatures NSSharingServiceDelegate
    // declares, and neither keeps `items` past the call.
    unsafe impl NSSharingServiceDelegate for ShareDone {
        #[unsafe(method(sharingService:didShareItems:))]
        fn did_share(&self, _service: &NSSharingService, _items: &NSArray) {
            self.give_back();
        }

        #[unsafe(method(sharingService:didFailToShareItems:error:))]
        fn did_fail(&self, _service: &NSSharingService, _items: &NSArray, _error: &NSError) {
            self.give_back();
        }
    }
);

impl ShareDone {
    fn new(mtm: MainThreadMarker, front: Option<Retained<NSRunningApplication>>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ShareIvars {
            front,
            finished: Cell::new(false),
        });
        // SAFETY: NSObject's designated initializer, on a fresh allocation.
        unsafe { msg_send![super(this), init] }
    }

    /// Hands activation back, unless one of Grape's windows is still key:
    /// the overlay and toast never are, so that's AirDrop's window (or
    /// Settings), and handing back would push it behind the app in front.
    /// Grape then just stays active until the user clicks elsewhere.
    fn give_back(&self) {
        self.ivars().finished.set(true);
        let mtm = self.mtm();
        if NSApplication::sharedApplication(mtm).keyWindow().is_some() {
            return;
        }
        if let Some(app) = &self.ivars().front {
            hand_back(mtm, app);
        }
    }
}

thread_local! {
    /// The AirDrop services that haven't reported back, with their
    /// delegates, on the main thread.
    static SHARING: RefCell<Vec<(Retained<NSSharingService>, Retained<ShareDone>)>> =
        const { RefCell::new(Vec::new()) };
}

pub(crate) fn move_to_chosen_folder(paths: &[PathBuf]) -> Result<MoveOutcome, PlatformError> {
    // The panel and the alert activate Grape. The app that was in front gets
    // activation back once they're answered, whatever the answer.
    let front = on_main("reading the frontmost app", |mtm| {
        Ok(frontmost_other_app(mtm))
    })?;
    let answer = ask_where(paths);
    if let Some(app) = front {
        // Focus is a courtesy: failing to hand it back doesn't stop the move.
        let _ = on_main("handing activation back", move |mtm| {
            hand_back(mtm, &app);
            Ok(())
        });
    }
    match answer? {
        Answer::Cancelled => Ok(MoveOutcome::Cancelled),
        Answer::AlreadyThere(to) => Ok(MoveOutcome::AlreadyThere { to }),
        Answer::Move { sources, to } => autoreleasepool(|_| move_items(&sources, &to)),
    }
}

/// What the user said in the Move's panel and alert.
enum Answer<'a> {
    /// They closed the panel or said no to another volume.
    Cancelled,
    /// Every item is already in the folder they picked.
    AlreadyThere(PathBuf),
    /// Move these items, the ones not already there, into `to`.
    Move { sources: Vec<&'a Path>, to: PathBuf },
}

/// Asks for the folder, then whether to move to another volume if that's
/// where it is.
fn ask_where(paths: &[PathBuf]) -> Result<Answer<'_>, PlatformError> {
    let picked = on_main("the Move folder picker", |mtm| {
        Ok(pick_move_folder(mtm, paths))
    })?;
    let Some(to) = picked else {
        return Ok(Answer::Cancelled);
    };
    let sources: Vec<&Path> = paths
        .iter()
        .map(PathBuf::as_path)
        .filter(|source| !already_in(source, &to))
        .collect();
    if sources.is_empty() {
        return Ok(Answer::AlreadyThere(to));
    }
    if crosses_volumes(&sources, &to)
        && !on_main("the Move confirmation", |mtm| Ok(confirm_other_volume(mtm)))?
    {
        return Ok(Answer::Cancelled);
    }
    Ok(Answer::Move { sources, to })
}

pub(crate) fn pick_folder(
    _owner: Option<isize>,
    title: &str,
    start: Option<&Path>,
) -> Result<Option<PathBuf>, PlatformError> {
    // No owner here: the panel is modal to the whole app.
    let start = start.filter(|dir| dir.is_dir());
    // Settings is the key window already, so the panel opens in front of
    // it: no activating, and nothing to hand back.
    on_main("the folder picker", |mtm| {
        Ok(folder_panel(mtm, "Choose", title, start))
    })
}

pub(crate) fn haptic_tick() {
    autoreleasepool(|_| {
        NSHapticFeedbackManager::defaultPerformer().performFeedbackPattern_performanceTime(
            NSHapticFeedbackPattern::Alignment,
            NSHapticFeedbackPerformanceTime::Now,
        );
    });
}

/// Runs `work` on the main thread (inline if this is it) inside an
/// autorelease pool, and turns an Objective-C exception into an error
/// instead of a crash.
///
/// It waits for the main thread, so never call it from a thread that the
/// main thread is itself waiting for.
fn on_main<R: Send>(
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

/// File URLs for `paths`, in order.
fn file_urls(paths: &[PathBuf]) -> Result<Vec<Retained<NSURL>>, PlatformError> {
    paths
        .iter()
        .map(|path| {
            NSURL::from_file_path(path).ok_or(PlatformError::Os {
                context: "making a file URL",
                code: NO_CODE,
            })
        })
        .collect()
}

/// A URL's file-system path. `to_file_path` stops at 1024 bytes, so longer
/// paths come through `path` instead.
fn url_path(url: &NSURL) -> Option<PathBuf> {
    url.to_file_path()
        .or_else(|| url.path().map(|path| PathBuf::from(path.to_string())))
}

/// Starts `open` with `args` and returns at once. A thread of its own waits
/// for it, so no zombie process is left behind.
fn open(
    args: impl IntoIterator<Item = OsString>,
    context: &'static str,
) -> Result<(), PlatformError> {
    let mut child = Command::new(OPEN)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| io_error(context, &err))?;
    // If the thread can't start, the child is only left unreaped.
    let _ = thread::Builder::new()
        .name("grape-open".to_owned())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}

/// Makes Grape the active app, so its panel, alert or AirDrop window comes
/// to the front. Grape has no Dock icon, and an inactive app's windows may
/// open behind the frontmost one.
fn activate(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    if objc2::available!(macos = 14.0) {
        app.activate();
    } else {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

/// The app in front, unless that's Grape itself. Read on the main thread,
/// where AppKit keeps it current.
fn frontmost_other_app(_mtm: MainThreadMarker) -> Option<Retained<NSRunningApplication>> {
    let front = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let grape = NSRunningApplication::currentApplication();
    (front.processIdentifier() != grape.processIdentifier()).then_some(front)
}

/// Gives activation back to `app` after Grape took it for the Move's panel
/// and alert, or for AirDrop. Only while Grape is still active: if the user went to
/// another app meanwhile, that one keeps it. From macOS 14 activation is
/// cooperative, so Grape yields to `app` first and then asks on its behalf.
/// Never `NSApp.hide`: that would hide the overlay and the toast too.
fn hand_back(mtm: MainThreadMarker, app: &NSRunningApplication) {
    let grape = NSApplication::sharedApplication(mtm);
    if !grape.isActive() {
        return;
    }
    let options = NSApplicationActivationOptions::empty();
    if objc2::available!(macos = 14.0) {
        grape.yieldActivationToApplication(app);
        app.activateFromApplication_options(&NSRunningApplication::currentApplication(), options);
    } else {
        app.activateWithOptions(options);
    }
}

/// Move's folder picker, starting next to the first item. `None` when the
/// user cancels.
fn pick_move_folder(mtm: MainThreadMarker, sources: &[PathBuf]) -> Option<PathBuf> {
    activate(mtm);
    let start = sources.first().and_then(|source| source.parent());
    folder_panel(mtm, "Move Here", &picker_message(sources), start)
}

/// An open panel that picks one folder, with `prompt` on its button and
/// `message` above the files, opening at `start` if there is one. Modal to
/// the whole app. `None` when the user cancels.
fn folder_panel(
    mtm: MainThreadMarker,
    prompt: &str,
    message: &str,
    start: Option<&Path>,
) -> Option<PathBuf> {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(true);
    panel.setPrompt(Some(&NSString::from_str(prompt)));
    panel.setMessage(Some(&NSString::from_str(message)));
    if let Some(start) = start.and_then(NSURL::from_directory_path) {
        panel.setDirectoryURL(Some(&start));
    }
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    let picked = panel.URLs().iter().next().and_then(|url| url_path(&url))?;
    // A folder URL ends in `/`; the components drop it.
    Some(picked.components().collect())
}

/// Asks before a move that copies to another volume and then deletes the
/// originals. Cancel is the first button, so Return cancels.
fn confirm_other_volume(mtm: MainThreadMarker) -> bool {
    activate(mtm);
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str("Move to another drive?"));
    alert.setInformativeText(&NSString::from_str(
        "The folder you picked is on another drive. Moving there copies the items, then deletes the originals.",
    ));
    let cancel = alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    cancel.setKeyEquivalent(&NSString::from_str("\r"));
    alert.addButtonWithTitle(&NSString::from_str("Move"));
    alert.runModal() == NSAlertSecondButtonReturn
}

/// `true` when `source` sits directly in `folder`, so moving it there would
/// do nothing.
fn already_in(source: &Path, folder: &Path) -> bool {
    let Some(parent) = source.parent() else {
        return false;
    };
    // canonicalize gives the on-disk spelling, so links and case differences
    // on a case-insensitive volume match.
    match (std::fs::canonicalize(parent), std::fs::canonicalize(folder)) {
        (Ok(parent), Ok(folder)) => parent == folder,
        _ => false,
    }
}

/// `true` when any item that would move may be on another volume than
/// `to`: the item or its folder is on another device
/// ([`ItemDevices::crosses_to`]). A volume's root never moves, so it
/// doesn't count.
fn crosses_volumes(sources: &[&Path], to: &Path) -> bool {
    let target = folder_device(to);
    sources
        .iter()
        .map(|source| item_devices(source))
        .filter(|devices| !devices.is_volume_root())
        .any(|devices| devices.crosses_to(target))
}

/// The devices of `path` and of its folder. The item's own from `lstat`,
/// because moving a link moves the link; a mount point gives the volume
/// mounted there.
fn item_devices(path: &Path) -> ItemDevices {
    ItemDevices {
        item: path.symlink_metadata().ok().map(|meta| meta.dev()),
        folder: path.parent().and_then(folder_device),
    }
}

/// A folder's device. `metadata` follows links, so a linked folder counts
/// where it really is.
fn folder_device(dir: &Path) -> Option<u64> {
    std::fs::metadata(dir).ok().map(|meta| meta.dev())
}

/// `to/<the item's name>`.
fn destination(source: &Path, to: &Path) -> Option<PathBuf> {
    source.file_name().map(|name| to.join(name))
}

/// Moves each item into `to` with `NSFileManager`, never replacing
/// anything and never moving a volume's root. Each item is judged from the
/// disk right after its own move, before the next one moves, so a later
/// item with the same name can't change what an earlier one says.
fn move_items(sources: &[&Path], to: &Path) -> Result<MoveOutcome, PlatformError> {
    let manager = NSFileManager::defaultManager();
    let mut moved = Vec::new();
    let mut unsure = Vec::new();
    let mut first_error: Option<i32> = None;
    for &path in sources {
        // No name (`/`) or a volume's root: it stays, like a taken name.
        let target = destination(path, to)
            .filter(|_| !item_devices(path).is_volume_root())
            .filter(|target| target.symlink_metadata().is_err());
        let mut failed = false;
        if let Some(target) = &target {
            // NSFileManager refuses to move onto an existing item, so a name
            // taken since the check above still isn't replaced.
            let result = match (NSURL::from_file_path(path), NSURL::from_file_path(target)) {
                (Some(from), Some(into)) => manager
                    .moveItemAtURL_toURL_error(&from, &into)
                    .map_err(|err| ns_error_code(&err)),
                _ => Err(NO_CODE),
            };
            if let Err(code) = result {
                failed = true;
                first_error.get_or_insert(code);
            }
        }
        let snapshot = after_move(path, target.as_deref(), failed);
        match judge_move(snapshot) {
            MoveVerdict::Moved => moved.push(MovedItem {
                from: path.to_path_buf(),
                now: target
                    .filter(|_| snapshot.arrived)
                    .unwrap_or_else(|| to.to_path_buf()),
            }),
            MoveVerdict::Unsure => unsure.push(path.to_path_buf()),
            MoveVerdict::Stayed => {}
        }
    }
    if moved.is_empty() && unsure.is_empty() {
        if let Some(code) = first_error {
            return Err(PlatformError::Os {
                context: "NSFileManager moveItemAtURL",
                code,
            });
        }
    }
    Ok(MoveOutcome::Moved {
        to: to.to_path_buf(),
        moved,
        unsure,
        of: sources.len(),
    })
}

/// The disk right after one item's own move. `tried` is `to/name` when
/// Grape asked for the move: the name was free just before, so anything
/// there now arrived with this item.
fn after_move(path: &Path, tried: Option<&Path>, failed: bool) -> MoveSnapshot {
    MoveSnapshot {
        tried: tried.is_some(),
        failed,
        arrived: tried.is_some_and(|target| target.symlink_metadata().is_ok()),
        old_place: old_place(path),
    }
}

/// What the item's old path says now.
fn old_place(path: &Path) -> OldPlace {
    match path.symlink_metadata() {
        Ok(_) => OldPlace::Present,
        Err(err)
            if err.kind() == ErrorKind::NotFound
                && path
                    .parent()
                    .is_some_and(|parent| parent.symlink_metadata().is_ok()) =>
        {
            OldPlace::Gone
        }
        Err(_) => OldPlace::Unreadable,
    }
}

/// An `NSError`'s code as an `i32`, or [`NO_CODE`] if it doesn't fit.
fn ns_error_code(err: &NSError) -> i32 {
    i32::try_from(err.code()).unwrap_or(NO_CODE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, PoisonError};

    use objc2::runtime::AnyClass;
    use objc2::ClassType;

    /// The general pasteboard is one per session: tests that use it take
    /// turns.
    static PASTEBOARD: Mutex<()> = Mutex::new(());

    /// A scratch folder, deleted with everything in it on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("grape-actions-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // The temp folder is behind a link (/var → /private/var).
            Self(std::fs::canonicalize(dir).unwrap())
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, name.as_bytes()).unwrap();
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

    fn pasteboard_urls() -> Vec<PathBuf> {
        autoreleasepool(|_| {
            let classes: Retained<NSArray<AnyClass>> = NSArray::from_slice(&[NSURL::class()]);
            // SAFETY: the class list holds only NSURL, which can be read
            // from a pasteboard, and there are no options.
            let objects = unsafe {
                NSPasteboard::generalPasteboard().readObjectsForClasses_options(&classes, None)
            }
            .unwrap();
            objects
                .iter()
                .map(|object| url_path(&object.downcast::<NSURL>().unwrap()).unwrap())
                .collect()
        })
    }

    fn pasteboard_text() -> String {
        autoreleasepool(|_| {
            // SAFETY: an immutable AppKit constant.
            let string_type = unsafe { NSPasteboardTypeString };
            NSPasteboard::generalPasteboard()
                .stringForType(string_type)
                .unwrap()
                .to_string()
        })
    }

    #[test]
    fn copy_puts_the_files_on_the_pasteboard() {
        let _turn = PASTEBOARD.lock().unwrap_or_else(PoisonError::into_inner);
        let scratch = Scratch::new("copy");
        let files = [
            scratch.file("one.png"),
            scratch.file("two words.txt"),
            scratch.folder("trip"),
        ];
        copy_files(&files).unwrap();
        assert_eq!(pasteboard_urls(), files);
        // Nothing was touched.
        assert!(files.iter().all(|file| file.exists()));
    }

    #[test]
    fn copy_path_puts_one_path_per_line_on_the_pasteboard() {
        let _turn = PASTEBOARD.lock().unwrap_or_else(PoisonError::into_inner);
        let paths = [
            PathBuf::from("/Users/max/a b/c.png"),
            PathBuf::from("/Volumes/USB/é"),
        ];
        copy_paths(&paths).unwrap();
        assert_eq!(pasteboard_text(), "/Users/max/a b/c.png\n/Volumes/USB/é");
    }

    #[test]
    fn an_item_already_in_the_folder_is_spotted() {
        let scratch = Scratch::new("already");
        let file = scratch.file("a.txt");
        let other = scratch.folder("other");
        assert!(already_in(&file, &scratch.0));
        assert!(already_in(&file, &other.join("..")));
        assert!(!already_in(&file, &other));
    }

    #[test]
    fn the_same_folder_is_on_the_same_volume() {
        let scratch = Scratch::new("volume");
        let file = scratch.file("a.txt");
        let other = scratch.folder("other");
        assert!(!crosses_volumes(&[file.as_path()], &other));
        // A folder that can't be read counts as another volume.
        let missing = scratch.0.join("missing").join("deeper");
        assert!(crosses_volumes(&[file.as_path()], &missing));
        // So does an item that can't be read.
        let ghost = scratch.0.join("ghost.txt");
        assert!(crosses_volumes(&[ghost.as_path()], &other));
    }

    #[test]
    fn a_volume_root_is_never_moved() {
        // devfs is mounted on /dev on every Mac: a volume's root that's
        // always there. Grape never asks to move it.
        let dev = Path::new("/dev");
        assert!(item_devices(dev).is_volume_root());
        let scratch = Scratch::new("move-root");
        let into = scratch.folder("into");
        assert!(!item_devices(&scratch.file("a.txt")).is_volume_root());
        // It stays, so it doesn't make the move cross volumes either.
        assert!(!crosses_volumes(&[dev], &into));
        let outcome = move_items(&[dev], &into).unwrap();
        assert_eq!(
            outcome,
            MoveOutcome::Moved {
                to: into.clone(),
                moved: vec![],
                unsure: vec![],
                of: 1,
            }
        );
        assert!(dev.exists());
        assert!(!into.join("dev").exists());
    }

    #[test]
    fn move_items_moves_what_it_can_and_never_replaces() {
        let scratch = Scratch::new("move");
        let one = scratch.file("one.txt");
        let two = scratch.file("two.txt");
        let clash = scratch.file("clash.txt");
        let trip = scratch.folder("trip");
        std::fs::write(trip.join("a.jpg"), b"grape").unwrap();
        let into = scratch.folder("into");
        // `into` already has a clash.txt of its own.
        std::fs::write(into.join("clash.txt"), b"theirs").unwrap();

        let sources = [
            one.as_path(),
            two.as_path(),
            clash.as_path(),
            trip.as_path(),
        ];
        let outcome = move_items(&sources, &into).unwrap();
        let MoveOutcome::Moved {
            to,
            moved,
            unsure,
            of,
        } = outcome
        else {
            panic!("expected a move, got {outcome:?}");
        };
        assert_eq!((to, unsure, of), (into.clone(), vec![], 4));
        assert_eq!(
            moved,
            vec![
                MovedItem {
                    from: one.clone(),
                    now: into.join("one.txt"),
                },
                MovedItem {
                    from: two.clone(),
                    now: into.join("two.txt"),
                },
                MovedItem {
                    from: trip.clone(),
                    now: into.join("trip"),
                },
            ]
        );
        assert!(!one.exists() && !two.exists() && !trip.exists());
        assert!(into.join("trip").join("a.jpg").exists());
        // The clash stayed, and neither file was touched.
        assert_eq!(std::fs::read(&clash).unwrap(), b"clash.txt");
        assert_eq!(std::fs::read(into.join("clash.txt")).unwrap(), b"theirs");
    }

    #[test]
    fn a_move_where_nothing_could_move_is_an_error() {
        let scratch = Scratch::new("move-fails");
        let file = scratch.file("a.txt");
        // The folder doesn't exist, so NSFileManager refuses.
        let nowhere = scratch.0.join("no-such-folder");
        let outcome = move_items(&[file.as_path()], &nowhere);
        assert!(
            matches!(outcome, Err(PlatformError::Os { .. })),
            "{outcome:?}"
        );
        assert!(file.exists());
    }

    #[test]
    fn an_item_gone_before_the_move_is_not_reported_as_moved() {
        let scratch = Scratch::new("move-ghost");
        let into = scratch.folder("into");
        let ghost = scratch.0.join("ghost.txt");
        let outcome = move_items(&[ghost.as_path()], &into).unwrap();
        assert_eq!(
            outcome,
            MoveOutcome::Moved {
                to: into,
                moved: vec![],
                unsure: vec![ghost],
                of: 1,
            }
        );
    }

    #[test]
    fn two_items_with_one_name_move_once() {
        let scratch = Scratch::new("move-twins");
        let first = scratch.folder("a").join("x.txt");
        std::fs::write(&first, b"first").unwrap();
        let second = scratch.folder("b").join("x.txt");
        std::fs::write(&second, b"second").unwrap();
        let into = scratch.folder("into");
        let outcome = move_items(&[first.as_path(), second.as_path()], &into).unwrap();
        // The first takes the name; the second finds it taken and stays.
        assert_eq!(
            outcome,
            MoveOutcome::Moved {
                to: into.clone(),
                moved: vec![MovedItem {
                    from: first.clone(),
                    now: into.join("x.txt"),
                }],
                unsure: vec![],
                of: 2,
            }
        );
        assert!(!first.exists());
        assert_eq!(std::fs::read(into.join("x.txt")).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
    }

    #[test]
    fn a_snapshot_is_unsure_of_an_item_in_both_places() {
        let scratch = Scratch::new("snapshot");
        let into = scratch.folder("into");
        // Copied over but not deleted: a folder split halfway, and a file.
        let split = scratch.folder("split");
        std::fs::create_dir_all(into.join("split")).unwrap();
        let copied = scratch.file("copied.png");
        std::fs::write(into.join("copied.png"), b"grape").unwrap();
        // The name was taken before, so the one in `into` isn't it.
        let merged = scratch.folder("merged");
        std::fs::create_dir_all(into.join("merged")).unwrap();
        // Its old folder can't be read.
        let lost = scratch.0.join("no-such-folder").join("lost.png");
        // Gone, and its move went through: moved, even if it's not at
        // `into/name` any more.
        let quiet = scratch.0.join("quiet.png");
        // Still in place, nothing at `into/name`: stayed.
        let stayed = scratch.file("stayed.png");
        // `tried` is false when the name was taken, so there was no move.
        let judged = |path: &Path, tried: bool, failed: bool| {
            let target = destination(path, &into).filter(|_| tried);
            let snapshot = after_move(path, target.as_deref(), failed);
            (judge_move(snapshot), snapshot.arrived)
        };
        assert_eq!(judged(&split, true, true), (MoveVerdict::Unsure, true));
        assert_eq!(judged(&copied, true, true), (MoveVerdict::Unsure, true));
        assert_eq!(judged(&merged, false, false), (MoveVerdict::Stayed, false));
        assert_eq!(judged(&lost, true, false), (MoveVerdict::Unsure, false));
        assert_eq!(judged(&quiet, true, false), (MoveVerdict::Moved, false));
        assert_eq!(judged(&stayed, true, true), (MoveVerdict::Stayed, false));
        assert_eq!(old_place(&lost), OldPlace::Unreadable);
        assert_eq!(old_place(&quiet), OldPlace::Gone);
        assert_eq!(old_place(&stayed), OldPlace::Present);
    }

    #[test]
    fn haptic_tick_is_safe_off_the_main_thread() {
        haptic_tick();
    }
}
