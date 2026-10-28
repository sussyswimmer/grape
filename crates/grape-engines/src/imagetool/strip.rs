//! Remove Metadata on images (D26, D28): the source streamed through its
//! container's stripper into a temp file, then published with a note of
//! what went. A TIFF is walked first and rewritten from the plan its walk
//! made ([`tiff::strip`]), and the output is read again before it's
//! published.
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

use super::{gif, jpeg, png, tiff, webp, Kind};
use crate::cancel::Cancellable;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome, MetaField, Note};

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
    /// The file is past one of the caps that bound what Grape holds of it.
    TooLarge(String),
    /// The file is one Grape won't change yet: the end of a sentence that
    /// starts with its name, such as "is a camera raw (DNG) file, which
    /// Grape can't change yet."
    Refused(String),
    /// Reading the source failed (or was cancelled).
    Read(io::Error),
    /// Writing the temp file failed (or was cancelled).
    Write(io::Error),
}

/// Where a stripper writes: WebP's goes back to fix its header.
pub(crate) trait Out: Write + Seek {}

impl<T: Write + Seek> Out for T {}

/// What a stripper that seeks reads from: TIFF's walks its IFDs.
pub(crate) trait In: BufRead + Seek {}

impl<T: BufRead + Seek> In for T {}

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
    matches!(
        from,
        Format::Jpg | Format::Png | Format::Webp | Format::Gif | Format::Tiff
    )
}

/// How a source is stripped: streamed through its container's stripper,
/// or, for a TIFF, rewritten from the plan its walk made.
enum Stripper {
    Jpeg,
    Png,
    Webp,
    Gif,
    Tiff(tiff::strip::Plan),
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
        return Err(names.decode_failed(format!(
            "{}: not a JPEG, PNG, WebP, GIF or TIFF",
            src.display()
        )));
    };
    // A TIFF is walked first: one Grape won't strip is refused before
    // anything is created.
    let stripper = match kind {
        Kind::Jpeg => Stripper::Jpeg,
        Kind::Png => Stripper::Png,
        Kind::Webp => Stripper::Webp,
        Kind::Gif => Stripper::Gif,
        Kind::Tiff => Stripper::Tiff(
            tiff::strip::Plan::new(&mut reader, size)
                .map_err(|e| failed(e, &names, src, dir, &ctx.cancel))?,
        ),
    };
    publish::ensure_dir(job, ctx, dir, &names)?;
    let (temp, out) = TempFile::create(dir, from.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    let mut writer = BufWriter::with_capacity(64 * 1024, Cancellable::new(out, &ctx.cancel));
    let found = match &stripper {
        Stripper::Jpeg => jpeg::strip(&mut reader, &mut writer),
        Stripper::Png => png::strip(&mut reader, &mut writer),
        Stripper::Webp => webp::strip(&mut reader, &mut writer),
        Stripper::Gif => gif::strip(&mut reader, &mut writer),
        Stripper::Tiff(plan) => plan.write(&mut reader, &mut writer, &ctx.cancel),
    }
    .and_then(|found| writer.flush().map(|()| found).map_err(Error::Write));
    let found = found.map_err(|e| failed(e, &names, src, temp.path(), &ctx.cancel))?;
    let out = writer
        .into_inner()
        .map_err(|e| names.write_failed(format!("write {}: {}", temp.path().display(), e.error())))?
        .into_inner();
    out.sync_all()
        .map_err(|e| names.write_failed(format!("sync {}: {e}", temp.path().display())))?;
    drop(out);
    drop(reader);
    if let Stripper::Tiff(plan) = &stripper {
        check(plan, temp.path(), &names, src, &ctx.cancel)?;
    }
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

/// The error for the user when a stripper stopped: `src` is the source,
/// `temp` the file it wrote into.
fn failed(e: Error, names: &Names, src: &Path, temp: &Path, cancel: &CancelToken) -> GrapeError {
    let e = match e {
        Error::Broken(why) => names.decode_failed(format!("{}: {why}", src.display())),
        Error::TooLarge(why) => names.too_large(format!("{}: {why}", src.display())),
        Error::Refused(why) => refused(names, src, &why),
        Error::Read(e) => names.read_failed(format!("read {}: {e}", src.display())),
        Error::Write(e) => names.write_failed(format!("write {}: {e}", temp.display())),
    };
    publish::or_cancelled(cancel, src, e)
}

/// Reads the TIFF Grape wrote at `path` again, against the `plan` it came
/// from (D28): anything amiss is Grape's own failure, and nothing is
/// published.
fn check(
    plan: &tiff::strip::Plan,
    path: &Path,
    names: &Names,
    src: &Path,
    cancel: &CancelToken,
) -> Result<(), GrapeError> {
    let wrong = |why: String| {
        let e = names.encode_failed(format!("check {}: {why}", path.display()));
        publish::or_cancelled(cancel, src, e)
    };
    let file = File::open(path).map_err(|e| wrong(format!("open: {e}")))?;
    let len = file
        .metadata()
        .map_err(|e| wrong(format!("stat: {e}")))?
        .len();
    let mut reader = BufReader::with_capacity(64 * 1024, Cancellable::new(file, cancel));
    plan.check(&mut reader, len).map_err(|e| match e {
        Error::Broken(why) | Error::TooLarge(why) | Error::Refused(why) => wrong(why),
        Error::Read(e) | Error::Write(e) => wrong(e.to_string()),
    })
}

/// The error for a file the tool won't change: "`source` `why`" (D28).
pub(crate) fn refused(names: &Names, src: &Path, why: &str) -> GrapeError {
    GrapeError::new(
        ErrorKind::NotYetSupported,
        format!("{} {why}", names.source),
        format!("{}: refused: {why}", src.display()),
    )
}

/// A read error, or the cancel that caused it.
fn read_error(names: &Names, src: &Path, cancel: &CancelToken, e: io::Error) -> GrapeError {
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
