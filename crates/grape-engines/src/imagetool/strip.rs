//! Remove Metadata on images (D26): the source streamed through its
//! container's stripper into a temp file, then published with a note of
//! what went.
//!
//! The container comes from the file's first bytes, not its name: a PNG
//! named `.jpg` is stripped as a PNG (and keeps its name). Reads and
//! writes go through [`Cancellable`], so a cancel stops the copy at the
//! next read or write; the temp file removes itself on every error.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::path::Path;

use grape_core::{Format, Job};

use super::{gif, jpeg, png, webp, Kind};
use crate::cancel::Cancellable;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{EngineCtx, GrapeError, JobOutcome, MetaField, Note};

/// What a stripper found in what it dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) fields: BTreeSet<MetaField>,
}

/// Why a stripper stopped. Nothing it wrote is published.
#[derive(Debug)]
pub(crate) enum Error {
    /// The file isn't what its first bytes say: truncated, or broken.
    Broken(String),
    /// Reading the source failed (or was cancelled).
    Read(io::Error),
    /// Writing the temp file failed (or was cancelled).
    Write(io::Error),
}

/// Where a stripper writes: WebP's goes back to fix its header.
pub(crate) trait Out: Write + Seek {}

impl<T: Write + Seek> Out for T {}

/// Reads exactly `buf.len()` bytes; the end of the file is [`Error::Broken`].
pub(crate) fn read_exact(r: &mut dyn BufRead, buf: &mut [u8]) -> Result<(), Error> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => Error::Broken("the file ends early".into()),
        _ => Error::Read(e),
    })
}

/// Reads one byte; the end of the file is [`Error::Broken`].
pub(crate) fn read_u8(r: &mut dyn BufRead) -> Result<u8, Error> {
    let mut b = [0];
    read_exact(r, &mut b)?;
    Ok(b[0])
}

/// Whether anything but `padding` bytes follows the container's end:
/// reads on, as far as the end of the file, up to the first byte that
/// isn't padding.
pub(crate) fn more_after(r: &mut dyn BufRead, padding: &[u8]) -> Result<bool, Error> {
    loop {
        let buf = r.fill_buf().map_err(Error::Read)?;
        if buf.is_empty() {
            return Ok(false);
        }
        if buf.iter().any(|b| !padding.contains(b)) {
            return Ok(true);
        }
        let n = buf.len();
        r.consume(n);
    }
}

/// The formats Remove Metadata runs on through this engine, by name; the
/// content decides the container.
pub(crate) fn strips(from: Format) -> bool {
    matches!(from, Format::Jpg | Format::Png | Format::Webp | Format::Gif)
}

/// Runs Remove Metadata on the image `src` (named as `from`) into `dest`.
pub(crate) fn run(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, from);
    let dir = publish::dest_dir(dest, &names)?;
    if ctx.cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let file =
        File::open(src).map_err(|e| names.read_failed(format!("open {}: {e}", src.display())))?;
    let size = file
        .metadata()
        .map_err(|e| names.read_failed(format!("stat {}: {e}", src.display())))?
        .len();
    let mut reader = BufReader::with_capacity(
        64 * 1024,
        Counting {
            inner: Cancellable::new(file, &ctx.cancel),
            read: 0,
            size,
            progress,
        },
    );
    let head = reader
        .fill_buf()
        .map_err(|e| read_error(&names, src, &ctx.cancel, e))?;
    let Some(kind) = Kind::of(head) else {
        return Err(names.decode_failed(format!("{}: not a JPEG, PNG, WebP or GIF", src.display())));
    };
    publish::ensure_dir(job, ctx, dir, &names)?;
    let (temp, out) = TempFile::create(dir, from.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    let mut writer = BufWriter::with_capacity(64 * 1024, Cancellable::new(out, &ctx.cancel));
    let found = match kind {
        Kind::Jpeg => jpeg::strip(&mut reader, &mut writer),
        Kind::Png => png::strip(&mut reader, &mut writer),
        Kind::Webp => webp::strip(&mut reader, &mut writer),
        Kind::Gif => gif::strip(&mut reader, &mut writer),
    }
    .and_then(|found| writer.flush().map(|()| found).map_err(Error::Write));
    let found = found.map_err(|e| {
        let e = match e {
            Error::Broken(why) => names.decode_failed(format!("{}: {why}", src.display())),
            Error::Read(e) => names.read_failed(format!("read {}: {e}", src.display())),
            Error::Write(e) => names.write_failed(format!("write {}: {e}", temp.path().display())),
        };
        publish::or_cancelled(&ctx.cancel, src, e)
    })?;
    let out = writer
        .into_inner()
        .map_err(|e| names.write_failed(format!("write {}: {}", temp.path().display(), e.error())))?
        .into_inner();
    out.sync_all()
        .map_err(|e| names.write_failed(format!("sync {}: {e}", temp.path().display())))?;
    drop(out);
    drop(reader);
    if ctx.cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let mut outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    progress(1.0);
    outcome
        .notes
        .push(Note::MetadataRemoved(found.fields.into_iter().collect()));
    Ok(outcome)
}

/// A read error, or the cancel that caused it.
fn read_error(names: &Names, src: &Path, cancel: &crate::CancelToken, e: io::Error) -> GrapeError {
    let e = names.read_failed(format!("read {}: {e}", src.display()));
    publish::or_cancelled(cancel, src, e)
}

/// A reader that reports how much of `size` bytes it has read, up to 95 %.
struct Counting<'a, R> {
    inner: R,
    read: u64,
    size: u64,
    progress: &'a mut dyn FnMut(f32),
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if self.size > 0 {
            let done = (self.read as f64 / self.size as f64).min(1.0) * 0.95;
            (self.progress)(done as f32);
        }
        Ok(n)
    }
}

/// Seeking is passed through; what was read still counts.
impl<R: Seek> Seek for Counting<'_, R> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}
