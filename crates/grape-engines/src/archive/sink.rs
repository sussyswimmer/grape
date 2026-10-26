//! Where checked entries go: a new ZIP, a new tar (plain or gzipped), or a
//! folder on disk (Extract Archive).

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tar::{EntryType, Header};
use zip::write::{FullFileOptions, SimpleFileOptions};
use zip::{CompressionMethod, System, ZipWriter};

use super::motw::Mark;
use super::names;
use super::source::Entry;
use super::stream::{Run, Side};
use super::{time, Failure};
use crate::cancel::Cancellable;

/// Deflate level for ZIP entries and gzip output: zlib's default.
pub(crate) const LEVEL: u32 = 6;

/// Entries at least this big get ZIP64 sizes: incompressible data grows a
/// little under Deflate, so this leaves room below 4 GiB.
const LARGE_ENTRY: u64 = 0xF000_0000;

/// The write buffer of each extracted file.
const FILE_BUF: usize = 64 * 1024;

/// The extended timestamp extra field ("UT"), written next to the DOS
/// time so readers that know it get the exact UTC time.
const EXTENDED_TIMESTAMP: u16 = 0x5455;

/// A place for checked entries. `data` already stops on cancel and charges
/// the limits; a sink only copies it.
pub(crate) trait Sink {
    /// Adds a folder.
    fn dir(&mut self, run: &Run<'_>, entry: &Entry) -> Result<(), Failure>;
    /// Adds a file of `size` bytes (`None`: not known ahead).
    fn file(
        &mut self,
        run: &Run<'_>,
        entry: &Entry,
        size: Option<u64>,
        data: &mut dyn Read,
    ) -> Result<(), Failure>;
    /// Adds a symbolic link to `target`, already checked.
    fn symlink(&mut self, run: &Run<'_>, entry: &Entry, target: &str) -> Result<(), Failure>;
}

/// Writes a new ZIP: Deflate level 6, UTF-8 names, Unix modes, ZIP64 for
/// big entries.
pub(crate) struct ZipSink<W: Write + Seek> {
    /// `None` once finished.
    zip: Option<ZipWriter<Quiet<W>>>,
    /// Set when the sink is dropped unfinished.
    quiet: Rc<Cell<bool>>,
}

impl<W: Write + Seek> ZipSink<W> {
    pub(crate) fn new(out: W) -> Self {
        let quiet = Rc::new(Cell::new(false));
        ZipSink {
            zip: Some(ZipWriter::new(Quiet::new(out, Rc::clone(&quiet)))),
            quiet,
        }
    }

    fn writer(&mut self) -> Result<&mut ZipWriter<Quiet<W>>, Failure> {
        self.zip
            .as_mut()
            .ok_or_else(|| Failure::Write("the zip was already finished".into()))
    }

    /// Writes the central directory and returns the writer.
    pub(crate) fn finish(mut self, run: &Run<'_>) -> Result<W, Failure> {
        let zip = self
            .zip
            .take()
            .ok_or_else(|| Failure::Write("the zip was already finished".into()))?;
        zip.finish()
            .map(Quiet::into_inner)
            .map_err(|e| run.zip_failure(e, Side::Write, "finishing the zip"))
    }
}

impl<W: Write + Seek> Drop for ZipSink<W> {
    fn drop(&mut self) {
        // Dropped unfinished (an error or a cancel): `ZipWriter`'s own drop
        // then finishes the archive into nothing, instead of writing to a
        // temp file about to be deleted and printing to stderr when that
        // fails.
        if self.zip.is_some() {
            self.quiet.set(true);
        }
    }
}

/// A ZIP writer's output that keeps track of its position and length as it
/// goes and, once `quiet` is set, writes nothing more: it only moves the
/// position and the length on, like a file would. The output starts as a
/// new, empty file, so the tracked values match the file's.
struct Quiet<W> {
    inner: W,
    quiet: Rc<Cell<bool>>,
    pos: u64,
    len: u64,
}

impl<W> Quiet<W> {
    fn new(inner: W, quiet: Rc<Cell<bool>>) -> Self {
        Quiet {
            inner,
            quiet,
            pos: 0,
            len: 0,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }

    fn moved(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n as u64);
        self.len = self.len.max(self.pos);
    }
}

impl<W: Write> Write for Quiet<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = if self.quiet.get() {
            buf.len()
        } else {
            self.inner.write(buf)?
        };
        self.moved(n);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.quiet.get() {
            Ok(())
        } else {
            self.inner.flush()
        }
    }
}

impl<W: Seek> Seek for Quiet<W> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        if !self.quiet.get() {
            let to = self.inner.seek(pos)?;
            if let SeekFrom::End(0) = pos {
                self.len = to;
            }
            self.pos = to;
            self.len = self.len.max(to);
            return Ok(to);
        }
        let to = match pos {
            SeekFrom::Start(to) => Some(to),
            SeekFrom::Current(by) => self.pos.checked_add_signed(by),
            SeekFrom::End(by) => self.len.checked_add_signed(by),
        };
        self.pos = to.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self.pos)
    }
}

/// The 5-byte extended timestamp (flags, then the mtime), for a time ZIP's
/// signed 32-bit field can hold.
fn extended_timestamp(mtime: Option<i64>) -> Option<[u8; 5]> {
    let mtime = i32::try_from(mtime?).ok().filter(|t| *t >= 0)?;
    let [a, b, c, d] = mtime.to_le_bytes();
    Some([1, a, b, c, d])
}

fn zip_options(
    run: &Run<'_>,
    entry: &Entry,
    method: CompressionMethod,
    large: bool,
) -> Result<FullFileOptions<'static, 'static>, Failure> {
    let mut options = SimpleFileOptions::DEFAULT
        .compression_method(method)
        .system(System::Unix)
        .last_modified_time(entry.mtime.map(time::to_dos).unwrap_or_default())
        .unix_permissions(entry.mode)
        .large_file(large);
    if method == CompressionMethod::DEFLATE {
        options = options.compression_level(Some(i64::from(LEVEL)));
    }
    let mut options = options.into_full_options();
    if let Some(field) = extended_timestamp(entry.mtime) {
        options
            .add_extra_field(EXTENDED_TIMESTAMP, field, false)
            .map_err(|e| run.zip_failure(e, Side::Write, "zip timestamp"))?;
    }
    Ok(options)
}

impl<W: Write + Seek> Sink for ZipSink<W> {
    fn dir(&mut self, run: &Run<'_>, entry: &Entry) -> Result<(), Failure> {
        let options = zip_options(run, entry, CompressionMethod::Stored, false)?;
        self.writer()?
            .add_directory(format!("{}/", entry.name()), options)
            .map_err(|e| run.zip_failure(e, Side::Write, "zip folder"))
    }

    fn file(
        &mut self,
        run: &Run<'_>,
        entry: &Entry,
        size: Option<u64>,
        data: &mut dyn Read,
    ) -> Result<(), Failure> {
        let large = size.is_none_or(|size| size >= LARGE_ENTRY);
        let options = zip_options(run, entry, CompressionMethod::DEFLATE, large)?;
        let zip = self.writer()?;
        zip.start_file(entry.name(), options)
            .map_err(|e| run.zip_failure(e, Side::Write, "zip entry"))?;
        io::copy(data, zip).map_err(|e| run.failure(&e, Side::Write, "zip entry"))?;
        Ok(())
    }

    fn symlink(&mut self, run: &Run<'_>, entry: &Entry, target: &str) -> Result<(), Failure> {
        let options = zip_options(run, entry, CompressionMethod::Stored, false)?;
        self.writer()?
            .add_symlink(entry.name(), target, options)
            .map_err(|e| run.zip_failure(e, Side::Write, "zip link"))
    }
}

/// Writes a new tar in GNU format (long names as GNU `L`/`K` records),
/// owner 0, the entry's mode and mtime.
pub(crate) struct TarSink<W: Write> {
    tar: tar::Builder<W>,
}

impl<W: Write> TarSink<W> {
    pub(crate) fn new(out: W) -> Self {
        TarSink {
            tar: tar::Builder::new(out),
        }
    }

    /// Writes the end-of-archive blocks and returns the writer.
    pub(crate) fn finish(self, run: &Run<'_>) -> Result<W, Failure> {
        self.tar
            .into_inner()
            .map_err(|e| run.failure(&e, Side::Write, "finishing the tar"))
    }
}

fn tar_header(entry: &Entry, kind: EntryType, size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(entry.mode);
    header.set_mtime(entry.mtime.map_or(0, |t| u64::try_from(t).unwrap_or(0)));
    header.set_uid(0);
    header.set_gid(0);
    header
}

impl<W: Write> Sink for TarSink<W> {
    fn dir(&mut self, run: &Run<'_>, entry: &Entry) -> Result<(), Failure> {
        let mut header = tar_header(entry, EntryType::Directory, 0);
        self.tar
            .append_data(&mut header, format!("{}/", entry.name()), io::empty())
            .map_err(|e| run.failure(&e, Side::Write, "tar folder"))
    }

    fn file(
        &mut self,
        run: &Run<'_>,
        entry: &Entry,
        size: Option<u64>,
        data: &mut dyn Read,
    ) -> Result<(), Failure> {
        let Some(size) = size else {
            return Err(Failure::Write("a tar entry needs its size first".into()));
        };
        let mut header = tar_header(entry, EntryType::Regular, size);
        self.tar
            .append_data(&mut header, entry.name(), data)
            .map_err(|e| run.failure(&e, Side::Write, "tar entry"))
    }

    fn symlink(&mut self, run: &Run<'_>, entry: &Entry, target: &str) -> Result<(), Failure> {
        let mut header = tar_header(entry, EntryType::Symlink, 0);
        self.tar
            .append_link(&mut header, entry.name(), target)
            .map_err(|e| run.failure(&e, Side::Write, "tar link"))
    }
}

/// Writes entries into a fresh folder (Grape's temp folder): regular files
/// with `create_new`, folders, nothing else. Never creates or follows a
/// link. Each file gets its mtime, its mode (owner read and write always
/// kept) on Unix, and the source's Mark of the Web on Windows.
pub(crate) struct FolderSink {
    root: PathBuf,
    mark: Mark,
    /// Folders named by entries, for their mtime and mode at the end.
    dirs: Vec<(PathBuf, usize, Option<i64>, u32)>,
}

impl FolderSink {
    pub(crate) fn new(root: &Path, mark: Mark) -> FolderSink {
        FolderSink {
            root: root.to_path_buf(),
            mark,
            dirs: Vec::new(),
        }
    }

    fn path_of(&self, entry: &Entry) -> PathBuf {
        let mut path = self.root.clone();
        path.extend(&entry.path);
        path
    }

    /// Sets the folders' modes and mtimes, deepest first, now that nothing
    /// more is written into them. A time the volume can't hold is skipped.
    pub(crate) fn finish(mut self, run: &Run<'_>) -> Result<(), Failure> {
        self.dirs.sort_by_key(|dir| std::cmp::Reverse(dir.1));
        for (path, _, mtime, mode) in &self.dirs {
            run.check()?;
            set_dir_mode(path, *mode)?;
            if let Some(time) = mtime.and_then(time::to_system) {
                if let Ok(handle) = open_dir(path) {
                    let _ = handle.set_modified(time);
                }
            }
        }
        Ok(())
    }
}

fn written(e: &io::Error, what: &str) -> Failure {
    Failure::Write(format!("{what}: {:?}", e.kind()))
}

/// A failure creating `entry`'s file or folder (or one above it). A name
/// the volume refuses (too long for it, most often: ext4 and APFS hold 255
/// bytes, NTFS 255 UTF-16 units) is the entry's name's fault, not the
/// destination's, so the message names the entry.
fn creating(e: &io::Error, entry: &Entry, what: &str) -> Failure {
    match e.kind() {
        io::ErrorKind::InvalidFilename => Failure::UnsafeName(names::shown(&entry.name())),
        _ => written(e, what),
    }
}

/// Opens a folder so its times can be set: on Windows that takes write
/// access and `FILE_FLAG_BACKUP_SEMANTICS`.
#[cfg(windows)]
fn open_dir(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(not(windows))]
fn open_dir(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// The folder's mode on Unix, with owner access always kept.
#[cfg(unix)]
fn set_dir_mode(path: &Path, mode: u32) -> Result<(), Failure> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))
        .map_err(|e| written(&e, "folder mode"))
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &Path, _mode: u32) -> Result<(), Failure> {
    Ok(())
}

/// The file's mode on Unix, with owner read and write always kept.
#[cfg(unix)]
fn set_file_mode(file: &File, mode: u32) -> Result<(), Failure> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode | 0o600))
        .map_err(|e| written(&e, "file mode"))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File, _mode: u32) -> Result<(), Failure> {
    Ok(())
}

impl Sink for FolderSink {
    fn dir(&mut self, run: &Run<'_>, entry: &Entry) -> Result<(), Failure> {
        run.check()?;
        let path = self.path_of(entry);
        fs::create_dir_all(&path).map_err(|e| creating(&e, entry, "creating a folder"))?;
        self.dirs
            .push((path, entry.path.len(), entry.mtime, entry.mode));
        Ok(())
    }

    fn file(
        &mut self,
        run: &Run<'_>,
        entry: &Entry,
        _size: Option<u64>,
        data: &mut dyn Read,
    ) -> Result<(), Failure> {
        let path = self.path_of(entry);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| creating(&e, entry, "creating a folder"))?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    // Two names the volume treats as one (NFC and NFD).
                    Failure::Duplicate(names::shown(&entry.name()))
                } else {
                    creating(&e, entry, "creating a file")
                }
            })?;
        let mut out = BufWriter::with_capacity(FILE_BUF, Cancellable::new(file, run.token()));
        io::copy(data, &mut out).map_err(|e| run.failure(&e, Side::Write, "extracting a file"))?;
        let file = out
            .into_inner()
            .map_err(|e| run.failure(e.error(), Side::Write, "extracting a file"))?
            .into_inner();
        // The mark first: writing a stream may update the file's last-write
        // time, which the archive's mtime must then replace.
        self.mark.apply(&path);
        if let Some(time) = entry.mtime.and_then(time::to_system) {
            // Best effort: FAT32 can't hold every time.
            let _ = file.set_modified(time);
        }
        set_file_mode(&file, entry.mode)?;
        file.sync_all().map_err(|e| written(&e, "syncing a file"))?;
        Ok(())
    }

    fn symlink(&mut self, _run: &Run<'_>, _entry: &Entry, _target: &str) -> Result<(), Failure> {
        // `Walk` leaves every link out when extracting.
        Err(Failure::Write("links are never extracted".into()))
    }
}

/// The output writer for a conversion: buffered, and failing once the job
/// is cancelled.
pub(crate) type Out = BufWriter<Cancellable<File>>;

/// Flushes and syncs the output and returns the file.
pub(crate) fn finish_out(run: &Run<'_>, out: Out) -> Result<File, Failure> {
    let file = out
        .into_inner()
        .map_err(|e| run.failure(e.error(), Side::Write, "flushing the output"))?
        .into_inner();
    file.sync_all()
        .map_err(|e| run.failure(&e, Side::Write, "syncing the output"))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_timestamps_fit_32_bits() {
        assert_eq!(extended_timestamp(Some(0x0102_0304)), Some([1, 4, 3, 2, 1]));
        assert_eq!(extended_timestamp(None), None);
        assert_eq!(extended_timestamp(Some(-1)), None);
        assert_eq!(extended_timestamp(Some(i64::from(i32::MAX) + 1)), None);
    }
}
