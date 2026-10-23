//! Writing outputs without ever touching an existing file.
//!
//! Every engine writes into a [`TempFile`] in the destination folder and then
//! gives it its final name with [`place_no_clobber`]:
//!
//! 1. `std::fs::hard_link(tmp, final)` (`link(2)` on Unix, `CreateHardLinkW`
//!    on Windows). It fails with `AlreadyExists` when `final` exists, and the
//!    name appears atomically with the full content. The temp name is then
//!    removed.
//! 2. When hard links aren't available (FAT32, exFAT, some network shares:
//!    any error other than `AlreadyExists`), the fallback creates `final`
//!    exclusively (`create_new`), copies the temp into it and syncs it. That
//!    is still no-clobber but not atomic: for a moment `final` exists with
//!    partial content. If the copy fails, the partial `final` (which this
//!    call created) is removed.
//!
//! `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` would be atomic on
//! every Windows filesystem, but it needs `unsafe`, which this crate forbids.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Prefix of every temp file name. Hidden on Unix; a dot-name on Windows.
pub(crate) const TEMP_PREFIX: &str = ".grape-tmp-";

/// How many times a unique name is retried when one is (improbably) taken.
const UNIQUE_ATTEMPTS: usize = 8;

/// The pauses between attempts to delete a file Grape created (775 ms in
/// all): on Windows a virus scanner or the indexer may hold a brand-new
/// file for a moment.
const REMOVE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
];

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// `{pid}-{nanos}-{counter}`: unique within this process, and across
/// processes as long as pids differ.
pub(crate) fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{n}", std::process::id())
}

/// Deletes `path`, a file Grape created, retrying with growing pauses
/// ([`REMOVE_RETRY_DELAYS`]) while it fails. A missing file counts as
/// deleted. Returns the last error if every attempt failed.
pub(crate) fn remove_created(path: &Path) -> io::Result<()> {
    let mut delays = REMOVE_RETRY_DELAYS.iter();
    loop {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => match delays.next() {
                Some(delay) => thread::sleep(*delay),
                None => return Err(e),
            },
        }
    }
}

/// Creates a new file named `{prefix}{unique_suffix}{suffix}` in `dir`,
/// exclusively (`create_new`), retrying with a fresh name if one exists.
pub(crate) fn create_unique(dir: &Path, prefix: &str, suffix: &str) -> io::Result<(PathBuf, File)> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    create_unique_with(&options, dir, prefix, suffix)
}

/// [`create_unique`] with the caller's `options`, which must include
/// `create_new(true)` so an existing file is never opened.
pub(crate) fn create_unique_with(
    options: &OpenOptions,
    dir: &Path,
    prefix: &str,
    suffix: &str,
) -> io::Result<(PathBuf, File)> {
    let mut last = None;
    for _ in 0..UNIQUE_ATTEMPTS {
        let path = dir.join(format!("{prefix}{}{suffix}", unique_suffix()));
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists)))
}

/// A temp file in the destination folder, removed when dropped: on success
/// (after [`place_no_clobber`] gave the content its final name), on every
/// error path and on cancel.
#[derive(Debug)]
pub(crate) struct TempFile {
    path: PathBuf,
}

impl TempFile {
    /// Creates `.grape-tmp-{pid}-{nanos}-{counter}.{ext}` in `dir`.
    pub(crate) fn create(dir: &Path, ext: &str) -> io::Result<(TempFile, File)> {
        let (path, file) = create_unique(dir, TEMP_PREFIX, &format!(".{ext}"))?;
        Ok((TempFile { path }, file))
    }

    /// The temp file's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // After a hard link this only removes the temp name. If it still
        // can't be removed after the retries, there is no one to tell: a
        // drop can't fail, and the output (if any) is complete either way.
        let _ = remove_created(&self.path);
    }
}

/// Gives the content of `tmp` the name `dest` without ever replacing an
/// existing file. Returns an `AlreadyExists` error when `dest` exists (the
/// caller re-plans the name); `tmp` is left in place either way.
pub(crate) fn place_no_clobber(tmp: &Path, dest: &Path) -> io::Result<()> {
    match fs::hard_link(tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(link_err) => copy_new(tmp, dest).map_err(|copy_err| {
            if copy_err.kind() == io::ErrorKind::AlreadyExists {
                copy_err
            } else {
                io::Error::new(
                    copy_err.kind(),
                    format!("hard link failed ({link_err}); copy failed ({copy_err})"),
                )
            }
        }),
    }
}

/// The fallback: create `dest` exclusively, copy `tmp` into it, sync it.
/// On failure the partial `dest` (created here, so ours to delete) is
/// closed and removed.
pub(crate) fn copy_new(tmp: &Path, dest: &Path) -> io::Result<()> {
    let out = OpenOptions::new().write(true).create_new(true).open(dest)?;
    let result = fill(tmp, out);
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

/// Copies `tmp` into `out`, syncs it and closes it.
fn fill(tmp: &Path, mut out: File) -> io::Result<()> {
    let mut src = File::open(tmp)?;
    io::copy(&mut src, &mut out)?;
    out.sync_all()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Dir {
            let path = std::env::temp_dir().join(format!("grape-output-unit-{}", unique_suffix()));
            fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn temp_file_is_removed_on_drop() {
        let dir = Dir::new();
        let (temp, file) = TempFile::create(&dir.0, "png").unwrap();
        drop(file);
        let name = temp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with(TEMP_PREFIX) && name.ends_with(".png"),
            "{name}"
        );
        assert!(temp.path().exists());
        drop(temp);
        assert!(names(&dir.0).is_empty());
    }

    #[test]
    fn removing_a_created_file_tolerates_one_already_gone() {
        let dir = Dir::new();
        let path = dir.0.join("x");
        fs::write(&path, b"x").unwrap();
        remove_created(&path).unwrap();
        assert!(names(&dir.0).is_empty());
        remove_created(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn removing_retries_and_then_reports_the_error() {
        let dir = Dir::new();
        // A non-empty folder can't be removed as a file, however often we try.
        let stuck = dir.0.join("stuck");
        fs::create_dir(&stuck).unwrap();
        fs::write(stuck.join("inner"), b"x").unwrap();
        let started = std::time::Instant::now();
        assert!(remove_created(&stuck).is_err());
        let waited: Duration = REMOVE_RETRY_DELAYS.iter().sum();
        assert!(started.elapsed() >= waited);
        assert!(stuck.exists());
    }

    #[test]
    fn place_links_and_refuses_an_existing_name() {
        let dir = Dir::new();
        let (temp, file) = TempFile::create(&dir.0, "jpg").unwrap();
        drop(file);
        fs::write(temp.path(), b"new").unwrap();
        let dest = dir.0.join("a.jpg");
        fs::write(&dest, b"old").unwrap();
        let err = place_no_clobber(temp.path(), &dest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&dest).unwrap(), b"old");

        let free = dir.0.join("a 2.jpg");
        place_no_clobber(temp.path(), &free).unwrap();
        drop(temp);
        assert_eq!(fs::read(&free).unwrap(), b"new");
        assert_eq!(names(&dir.0), ["a 2.jpg", "a.jpg"]);
    }

    #[test]
    fn copy_fallback_is_no_clobber() {
        let dir = Dir::new();
        let tmp = dir.0.join(".grape-tmp-x.png");
        fs::write(&tmp, b"content").unwrap();
        let dest = dir.0.join("b.png");
        fs::write(&dest, b"keep me").unwrap();
        let err = copy_new(&tmp, &dest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&dest).unwrap(), b"keep me");

        let free = dir.0.join("b 2.png");
        copy_new(&tmp, &free).unwrap();
        assert_eq!(fs::read(&free).unwrap(), b"content");
    }

    #[test]
    fn copy_fallback_removes_its_partial_output() {
        let dir = Dir::new();
        // The source is missing, so the copy fails after `dest` was created.
        let dest = dir.0.join("c.png");
        let err = copy_new(&dir.0.join("missing"), &dest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(names(&dir.0).is_empty());
    }
}
