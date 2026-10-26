//! Folder output for Extract Archive (and the PDF engine's page folders,
//! through [`publish_folder`]): a hidden temp folder next to the planned
//! one, then a no-clobber publish.
//!
//! `std::fs::rename` can't publish a folder safely: on Unix, and on
//! Windows 10 1607 and later, renaming onto an existing *empty* folder
//! replaces it. So [`publish`] reserves the final name with
//! `std::fs::create_dir`, which fails if anything has the name, re-plans
//! the name the way `publish::finalize` does for files, then moves each
//! top-level item of the temp folder into the reserved one (same volume,
//! so each move is a rename). That is no-clobber but not atomic: for a
//! moment the folder exists with only some of its items.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use grape_core::Job;

use super::Failure;
use crate::output::{unique_suffix, TEMP_PREFIX};
use crate::publish::{file_name, Names};
use crate::{ErrorKind, GrapeError, JobOutcome};

/// How many times a taken folder name is re-planned before giving up (the
/// same as for files).
const MAX_REPLANS: usize = 3;

/// How many times a unique temp folder name is tried.
const UNIQUE_ATTEMPTS: usize = 8;

/// The pauses between attempts to delete a folder Grape created: a virus
/// scanner or the indexer may hold a new file for a moment on Windows.
const REMOVE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
];

/// The pauses between attempts to move an item into the published folder
/// (3.15 s in all). Windows refuses to rename a folder while a file inside
/// it is open, and a virus scanner opens each new file right after Grape
/// closes it: the last file of a one-folder archive closes just before
/// that folder is moved.
const MOVE_RETRY_DELAYS: [Duration; 6] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
];

/// A temp folder `.grape-tmp-{pid}-{nanos}-{n}` in the destination folder,
/// removed with everything in it when dropped.
#[derive(Debug)]
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a fresh, empty temp folder in `parent`.
    pub(crate) fn create(parent: &Path) -> io::Result<TempDir> {
        let mut last = None;
        for _ in 0..UNIQUE_ATTEMPTS {
            let path = parent.join(format!("{TEMP_PREFIX}{}", unique_suffix()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(TempDir { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists)))
    }

    /// The temp folder's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Empty after a publish; full after an error or a cancel. If it
        // still can't be removed after the retries, there is no one to
        // tell: a drop can't fail.
        let _ = remove_tree(&self.path);
    }
}

/// Deletes the folder `path` and everything in it, retrying while it
/// fails. A folder already gone counts as deleted. `remove_dir_all` never
/// follows links, and Grape never creates any in its folders anyway.
pub(crate) fn remove_tree(path: &Path) -> io::Result<()> {
    let mut delays = REMOVE_RETRY_DELAYS.iter();
    loop {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => match delays.next() {
                Some(delay) => thread::sleep(*delay),
                None => return Err(e),
            },
        }
    }
}

/// Runs `op` until it succeeds or fails with something other than
/// `PermissionDenied`, pausing for each of `delays` in turn; then returns
/// its last result.
fn retry_denied(delays: &[Duration], mut op: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let mut delays = delays.iter();
    loop {
        match op() {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => match delays.next() {
                Some(delay) => thread::sleep(*delay),
                None => return Err(e),
            },
            result => return result,
        }
    }
}

/// A folder [`publish`] reserved, removed (with what was moved into it)
/// unless the publish finished.
struct Reserved {
    path: PathBuf,
    done: bool,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        if !self.done {
            let _ = remove_tree(&self.path);
        }
    }
}

/// Publishes the temp folder's content as the planned folder `dest` (in
/// `dir`), never replacing anything: reserves the name with `create_dir`,
/// re-planning it with `grape_core::output_name` up to [`MAX_REPLANS`]
/// times if it was taken meanwhile, then moves each top-level item in. On
/// any failure the reserved folder is removed again; the temp folder is
/// removed by its owner.
pub(crate) fn publish(
    temp: &TempDir,
    job: &Job,
    src: &Path,
    dest: &Path,
    dir: &Path,
    names: &Names,
) -> Result<JobOutcome, Failure> {
    let source_name = file_name(src);
    publish_folder(temp.path(), dest, dir, names, |taken| {
        grape_core::output_name(&source_name, &job.op, taken)
    })
    .map_err(|failure| match failure {
        PublishFailure::Taken(error) => Failure::Grape(error),
        PublishFailure::Write(detail) => Failure::Write(detail),
    })
}

/// Why [`publish_folder`] failed.
#[derive(Debug)]
pub(crate) enum PublishFailure {
    /// Every name tried was taken (already in Grape's words).
    Taken(GrapeError),
    /// Reserving the folder or moving an item into it failed; the detail
    /// names neither paths nor items.
    Write(String),
}

/// [`publish`] for any engine: moves each top-level item of the folder
/// `from` (a temp folder, or one inside it) into a new folder at `dest` (in
/// `dir`), never replacing anything. A taken name is re-planned with
/// `replan`, which gets the names in `dir` plus every name tried so far,
/// up to [`MAX_REPLANS`] times. On any failure the reserved folder is
/// removed again; `from` is removed once empty (else by its owner).
pub(crate) fn publish_folder(
    from: &Path,
    dest: &Path,
    dir: &Path,
    names: &Names,
    mut replan: impl FnMut(&HashSet<String>) -> String,
) -> Result<JobOutcome, PublishFailure> {
    let mut target = dest.to_path_buf();
    let mut attempted: HashSet<String> = HashSet::from([file_name(dest)]);
    let mut replans = 0;
    loop {
        match fs::create_dir(&target) {
            Ok(()) => break,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if replans == MAX_REPLANS {
                    return Err(PublishFailure::Taken(GrapeError::new(
                        ErrorKind::NameTaken,
                        format!(
                            "A folder named {} keeps appearing; try again.",
                            names.planned
                        ),
                        format!("every one of {} folder names tried was taken", replans + 1),
                    )));
                }
                let mut taken = dir_names(dir);
                taken.extend(attempted.iter().cloned());
                let name = replan(&taken);
                attempted.insert(name.clone());
                target = dir.join(name);
                replans += 1;
            }
            Err(e) => {
                return Err(PublishFailure::Write(format!(
                    "reserving the folder: {:?}",
                    e.kind()
                )))
            }
        }
    }
    let mut reserved = Reserved {
        path: target,
        done: false,
    };
    let moved =
        |e: io::Error| PublishFailure::Write(format!("moving into the folder: {:?}", e.kind()));
    for item in fs::read_dir(from).map_err(moved)? {
        let item = item.map_err(moved)?;
        let to = reserved.path.join(item.file_name());
        retry_denied(&MOVE_RETRY_DELAYS, || fs::rename(item.path(), &to)).map_err(moved)?;
    }
    reserved.done = true;
    // Now empty; if this fails, the temp folder's drop tries again.
    let _ = fs::remove_dir(from);
    Ok(JobOutcome {
        outputs: vec![reserved.path.clone()],
        renamed: replans > 0,
        notes: Vec::new(),
    })
}

/// The names in `dir` right now; empty if it can't be listed.
fn dir_names(dir: &Path) -> HashSet<String> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: [Duration; 3] = [Duration::from_millis(1); 3];

    #[test]
    fn a_denied_move_is_retried_until_it_works() {
        let mut calls = 0;
        let result = retry_denied(&SHORT, || {
            calls += 1;
            if calls < 3 {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(calls, 3);
    }

    #[test]
    fn other_failures_and_lasting_denials_end_the_retries() {
        let mut calls = 0;
        let result = retry_denied(&SHORT, || {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert_eq!(result.map_err(|e| e.kind()), Err(io::ErrorKind::NotFound));
        assert_eq!(calls, 1);

        let mut calls = 0;
        let result = retry_denied(&SHORT, || {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert_eq!(
            result.map_err(|e| e.kind()),
            Err(io::ErrorKind::PermissionDenied)
        );
        assert_eq!(calls, SHORT.len() + 1);
        assert_eq!(
            MOVE_RETRY_DELAYS.iter().sum::<Duration>(),
            Duration::from_millis(3150)
        );
    }
}
