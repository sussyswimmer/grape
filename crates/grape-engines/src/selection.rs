//! Building a `grape_core::Selection` from dropped paths: the one place
//! that reads the disk on the planner's behalf (`grape-core` stays pure).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use grape_core::{DirSnapshot, OutputLocation, Selection, SelectionItem, Settings};

#[cfg(windows)]
use crate::output::create_unique_with;
#[cfg(not(windows))]
use crate::output::{create_unique, remove_created};

/// Prefix of the writability probe files.
const PROBE_PREFIX: &str = ".grape-probe-";

/// Builds the [`Selection`] that `grape_core::plan` needs for `paths`.
///
/// - **Items**, in order: a folder when `fs::metadata` says so, otherwise a
///   file whose format comes from its name (`SelectionItem::file`; `None`
///   for an unknown extension). A path that can't be read is kept as a
///   file, so the job reports it ("Grape couldn't read …") instead of it
///   silently disappearing.
/// - **Directory snapshots** for every folder a plan may write to: each
///   item's parent, the `OutputLocation::Folder` path if set, and
///   `settings.fallback_dir`. `names` are the folder's current entries
///   (lossy UTF-8; unreadable entries skipped). `writable` comes from a
///   probe: create a uniquely named `.grape-probe-{pid}-{nanos}-{n}` file
///   with `create_new`, then delete it (on Windows, the OS deletes it as
///   its handle closes); any error means `false`. A missing or unreadable
///   folder gets no names and `writable = false`.
/// - **The fallback folder** is Grape's own (`Downloads\Grape`) and may not
///   exist yet. It is never created here: a missing fallback gets no names
///   and counts as writable when its nearest existing ancestor is a folder
///   that passes the probe. The engine creates it only when a job writes
///   into it (by falling back, or because `OutputLocation::Folder` names
///   it). No folder is ever created here.
/// - **The fallback is read only when a plan may write there**: a folder
///   the planner tries first (the `OutputLocation::Folder` path, otherwise
///   each item's parent) is the fallback or isn't writable. Otherwise it
///   gets `DirSnapshot::default()`, as an empty fallback path always does,
///   without touching the disk: the planner never looks at it then, so the
///   plan is the same. On macOS, Grape's first read in `~/Downloads` asks
///   the user for the Downloads folder (TCC's Files and Folders; a drop
///   covers only the dropped items), and a drop that can't fall back must
///   not ask.
///
/// Never panics; problems show up as `writable = false` and the planner
/// then falls back or reports `NoWritableDestination`.
pub fn build_selection(paths: &[PathBuf], settings: &Settings) -> Selection {
    let items: Vec<SelectionItem> = paths.iter().map(|path| item(path)).collect();

    let mut dirs: BTreeSet<PathBuf> = items
        .iter()
        .filter_map(|item| item.path.parent())
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    if let OutputLocation::Folder(dir) = &settings.output {
        dirs.insert(dir.clone());
    }
    let fallback = &settings.fallback_dir;

    let mut selection = Selection::new(items);
    for dir in dirs.iter().filter(|dir| *dir != fallback) {
        selection = selection.with_dir(dir.clone(), snapshot(dir));
    }
    // The folders the planner tries first (with outputs next to the
    // source, `dirs` holds just the parents). It looks at the fallback's
    // snapshot (writable, then names) only when one of them is the
    // fallback or isn't writable.
    let preferred: Vec<&PathBuf> = match &settings.output {
        OutputLocation::Folder(dir) => vec![dir],
        OutputLocation::NextToSource => dirs.iter().collect(),
    };
    let may_fall_back = preferred.iter().any(|dir| {
        *dir == fallback || !selection.dirs.get(*dir).is_some_and(|known| known.writable)
    });
    // An empty fallback path gets a snapshot nobody can write to, so the
    // planner reports `NoWritableDestination`, not a missing snapshot. So
    // does a fallback no plan reaches, unread.
    let fallback_snapshot = if fallback.as_os_str().is_empty() || !may_fall_back {
        DirSnapshot::default()
    } else {
        fallback_snapshot(fallback)
    };
    selection.with_dir(fallback.clone(), fallback_snapshot)
}

/// The fallback folder's snapshot: a normal one when it exists, otherwise
/// no names and `writable` when the folder could be created, i.e. its
/// nearest existing ancestor is a folder that passes the probe. A path
/// blocked by a file, or by a link to nowhere (which `exists` would call
/// missing, though nothing can be created in its place), is not writable.
fn fallback_snapshot(fallback: &Path) -> DirSnapshot {
    let present = |path: &Path| fs::symlink_metadata(path).is_ok();
    if present(fallback) {
        return snapshot(fallback);
    }
    let ancestor = fallback.ancestors().skip(1).find(|dir| present(dir));
    DirSnapshot {
        names: HashSet::new(),
        writable: ancestor.is_some_and(|dir| dir.is_dir() && probe_writable(dir)),
    }
}

/// One dropped path as a folder or a file.
fn item(path: &Path) -> SelectionItem {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => SelectionItem::folder(path),
        _ => SelectionItem::file(path),
    }
}

/// What the planner needs to know about `dir`.
fn snapshot(dir: &Path) -> DirSnapshot {
    let Ok(entries) = fs::read_dir(dir) else {
        return DirSnapshot::default();
    };
    let names: HashSet<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    DirSnapshot {
        names,
        writable: probe_writable(dir),
    }
}

/// True when a new file can be created in `dir` and deleted again.
///
/// On Windows the probe is opened with `FILE_FLAG_DELETE_ON_CLOSE` (and the
/// `DELETE` access that needs), so the OS removes it when its handle
/// closes, even if a scanner opened it meanwhile: no probe is ever left
/// behind. A folder that lets Grape create files but not delete them
/// refuses that open, so nothing is created and the folder counts as not
/// writable (Grape couldn't remove its temp file there either).
#[cfg(windows)]
fn probe_writable(dir: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;

    // Win32 values (winnt.h / winbase.h).
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const DELETE: u32 = 0x0001_0000;
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;

    let mut options = fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_WRITE | DELETE)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
    // Dropping the handle deletes the probe.
    create_unique_with(&options, dir, PROBE_PREFIX, "").is_ok()
}

/// True when a new file can be created in `dir` and deleted again. The
/// delete is retried like a temp file's; only a folder that allows creating
/// but never deleting (an append-only folder) keeps the probe.
#[cfg(not(windows))]
fn probe_writable(dir: &Path) -> bool {
    match create_unique(dir, PROBE_PREFIX, "") {
        Ok((probe, file)) => {
            drop(file);
            remove_created(&probe).is_ok()
        }
        Err(_) => false,
    }
}
