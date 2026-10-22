//! Building a `grape_core::Selection` from dropped paths: the one place
//! that reads the disk on the planner's behalf (`grape-core` stays pure).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use grape_core::{DirSnapshot, OutputLocation, Selection, SelectionItem, Settings};

use crate::output::create_unique;

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
///   with `create_new`, then remove it; any error means `false`. A missing
///   or unreadable folder gets no names and `writable = false`.
/// - **The fallback folder** is Grape's own (`Downloads\Grape`), so it is
///   created with `create_dir_all` if it doesn't exist yet, before it is
///   probed. No other folder is ever created.
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
    if !fallback.as_os_str().is_empty() {
        // Grape's own folder: make it so it can be probed (and used). A
        // failure here shows up as `writable = false` below.
        let _ = fs::create_dir_all(fallback);
        dirs.insert(fallback.clone());
    }

    let mut selection = Selection::new(items);
    for dir in dirs {
        let snapshot = snapshot(&dir);
        selection = selection.with_dir(dir, snapshot);
    }
    selection
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

/// True when a new file can be created in `dir` and removed again.
fn probe_writable(dir: &Path) -> bool {
    match create_unique(dir, PROBE_PREFIX, "") {
        Ok((probe, file)) => {
            drop(file);
            fs::remove_file(probe).is_ok()
        }
        Err(_) => false,
    }
}
