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
//!   where it is. What moved is read back from the disk afterwards.
//!
//! The panel, the alert and AirDrop are AppKit UI, so they run on the main
//! thread: inline when the caller is on it, otherwise through the main
//! queue, waiting for it. The rest runs on the calling thread inside an
//! autorelease pool. `open` runs as its own process with each path as its
//! own argument, never through a shell.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use dispatch2::run_on_main;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAlert, NSAlertSecondButtonReturn, NSAlertStyle, NSApplication, NSHapticFeedbackManager,
    NSHapticFeedbackPattern, NSHapticFeedbackPerformanceTime, NSHapticFeedbackPerformer,
    NSModalResponseOK, NSOpenPanel, NSPasteboard, NSPasteboardTypeString, NSPasteboardWriting,
    NSSharingService, NSSharingServiceNameSendViaAirDrop,
};
use objc2_foundation::{NSArray, NSError, NSFileManager, NSString, NSURL};

use super::{io_error, FEATURE_UNSUPPORTED, NO_CODE};
use crate::file_actions::{MoveOutcome, MovedItem};
use crate::mac_rules::{path_lines, picker_message, reveal_args, terminal_args};
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
    on_main("AirDrop", move |_mtm| {
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
        // SAFETY: as above, and the service said it can take them.
        unsafe { service.performWithItems(items) };
        Ok(())
    })
}

pub(crate) fn move_to_chosen_folder(paths: &[PathBuf]) -> Result<MoveOutcome, PlatformError> {
    let Some(to) = on_main("the Move folder picker", |mtm| Ok(pick_folder(mtm, paths)))? else {
        return Ok(MoveOutcome::Cancelled);
    };
    let sources: Vec<&Path> = paths
        .iter()
        .map(PathBuf::as_path)
        .filter(|source| !already_in(source, &to))
        .collect();
    if sources.is_empty() {
        return Ok(MoveOutcome::AlreadyThere { to });
    }
    if crosses_volumes(&sources, &to)
        && !on_main("the Move confirmation", |mtm| Ok(confirm_other_volume(mtm)))?
    {
        return Ok(MoveOutcome::Cancelled);
    }
    autoreleasepool(|_| move_items(&sources, &to))
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

/// Makes Grape the active app, so its panel or alert comes to the front.
/// Grape has no Dock icon, and an inactive app's modal UI may open behind
/// the frontmost window.
fn activate(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    if objc2::available!(macos = 14.0) {
        app.activate();
    } else {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

/// The folder picker, starting next to the first item. `None` when the user
/// cancels.
fn pick_folder(mtm: MainThreadMarker, sources: &[PathBuf]) -> Option<PathBuf> {
    activate(mtm);
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(true);
    panel.setPrompt(Some(&NSString::from_str("Move Here")));
    panel.setMessage(Some(&NSString::from_str(&picker_message(sources))));
    if let Some(start) = sources
        .first()
        .and_then(|source| source.parent())
        .and_then(NSURL::from_directory_path)
    {
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

/// `true` when any item may be on another volume than `to`: its folder's
/// device differs from `to`'s. The item's folder, not the item, because
/// moving a link moves the link. A device that can't be read counts as
/// another volume, so the user is asked.
fn crosses_volumes(sources: &[&Path], to: &Path) -> bool {
    // `metadata` follows links, so a linked folder counts where it really is.
    let device = |dir: &Path| std::fs::metadata(dir).ok().map(|meta| meta.dev());
    let target = device(to);
    sources
        .iter()
        .any(|source| match (source.parent().and_then(device), target) {
            (Some(device), Some(target)) => device != target,
            _ => true,
        })
}

/// `to/<the item's name>`.
fn destination(source: &Path, to: &Path) -> Option<PathBuf> {
    source.file_name().map(|name| to.join(name))
}

/// One item asked to move.
struct Source<'a> {
    path: &'a Path,
    /// `to` already had the item's name before the move, so the item was
    /// left where it was, and `to/name` is something else.
    taken: bool,
    /// `NSFileManager` reported an error for this item.
    failed: bool,
}

/// Moves each item into `to` with `NSFileManager`, never replacing
/// anything, then reports what the disk says moved.
fn move_items(sources: &[&Path], to: &Path) -> Result<MoveOutcome, PlatformError> {
    let manager = NSFileManager::defaultManager();
    let mut items = Vec::with_capacity(sources.len());
    let mut first_error: Option<i32> = None;
    for &path in sources {
        // No name (a volume root) counts as taken: there's nowhere to put it.
        let target = destination(path, to).filter(|target| target.symlink_metadata().is_err());
        let taken = target.is_none();
        let mut failed = false;
        if let Some(target) = target {
            // NSFileManager refuses to move onto an existing item, so a name
            // taken since the check above still isn't replaced.
            let result = match (NSURL::from_file_path(path), NSURL::from_file_path(&target)) {
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
        items.push(Source {
            path,
            taken,
            failed,
        });
    }
    let (moved, unsure) = settle(&items, to);
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

/// Sorts the items after a move, by what the disk says now.
///
/// An item moved if its old path is gone while its old folder can still be
/// read, unless its move failed and it isn't at `to/name` either: then
/// Grape can't tell where it went. An item still in its old place while
/// `to/name` now exists (and didn't before) is in both places, partly or
/// wholly: a move to another volume copies first, and a folder can fail
/// halfway. An old place that can't be read can't be judged. Those three
/// are `unsure`. Everything else stayed.
fn settle(items: &[Source<'_>], to: &Path) -> (Vec<MovedItem>, Vec<PathBuf>) {
    let mut moved = Vec::new();
    let mut unsure = Vec::new();
    for item in items {
        let target = destination(item.path, to).filter(|_| !item.taken);
        let arrived = target
            .as_ref()
            .is_some_and(|target| target.symlink_metadata().is_ok());
        match item.path.symlink_metadata() {
            Err(err)
                if err.kind() == ErrorKind::NotFound
                    && item
                        .path
                        .parent()
                        .is_some_and(|parent| parent.symlink_metadata().is_ok()) =>
            {
                if arrived || !item.failed {
                    moved.push(MovedItem {
                        from: item.path.to_path_buf(),
                        now: target
                            .filter(|_| arrived)
                            .unwrap_or_else(|| to.to_path_buf()),
                    });
                } else {
                    unsure.push(item.path.to_path_buf());
                }
            }
            Err(_) => unsure.push(item.path.to_path_buf()),
            Ok(_) if arrived => unsure.push(item.path.to_path_buf()),
            Ok(_) => {}
        }
    }
    (moved, unsure)
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
    fn settle_is_unsure_of_an_item_in_both_places() {
        let scratch = Scratch::new("settle");
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
        let source = |path, taken, failed| Source {
            path,
            taken,
            failed,
        };
        let items = [
            source(&split, false, true),
            source(&copied, false, true),
            source(&merged, true, false),
            source(&lost, false, false),
            source(&quiet, false, false),
            source(&stayed, false, true),
        ];
        let (moved, unsure) = settle(&items, &into);
        assert_eq!(
            moved,
            vec![MovedItem {
                from: quiet,
                now: into.clone(),
            }]
        );
        assert_eq!(unsure, vec![split, copied, lost]);
    }

    #[test]
    fn haptic_tick_is_safe_off_the_main_thread() {
        haptic_tick();
    }
}
