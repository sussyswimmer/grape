//! Grape's own subtitle engine: SRT ⇄ WebVTT, and either to plain text
//! (`docs/ENGINES.md`). Everything here is a pure function over bytes or
//! text, so every rule is unit-tested in its file; reading the source and
//! publishing the output are the convert step's job.
//!
//! 1. [`decode`] turns the file's bytes into text: a BOM picks UTF-8 or
//!    UTF-16; without one, valid UTF-8 is UTF-8 and anything else is
//!    Windows-1252.
//! 2. [`parse`] reads the cues leniently, as SRT or WebVTT by content (not
//!    by file name), and normalizes them: sorted by start, never
//!    zero-length, no blank lines.
//! 3. [`write`] writes WebVTT, SRT or plain text with the source's line
//!    endings, cues renumbered from 1, and italic, bold and underline tags
//!    balanced per cue.
//!
//! [`convert`] runs the three on one file: it reads the source read-only
//! (refusing one over [`MAX_SOURCE`] before reading it), writes a temp file
//! next to the planned name and publishes it with [`crate::publish`]. The
//! token is checked between steps and inside each one, and the temp file's
//! writer fails once the job is cancelled.

pub(crate) mod decode;
pub(crate) mod parse;
pub(crate) mod write;

use std::fs::File;
use std::io::{self, BufWriter, Read};
use std::path::Path;

use grape_core::{Format, Job};

use crate::cancel::Cancellable;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome};

use decode::Encoding;
use parse::{Document, ParseError};

/// The largest subtitle file Grape reads. Real files are well under 1 MiB.
/// With [`parse::MAX_LINES`] this bounds the memory a conversion takes,
/// whatever the file holds.
pub(crate) const MAX_SOURCE: u64 = 16 * 1024 * 1024;

/// True for SRT → VTT or TXT, and WebVTT → SRT or TXT.
pub(crate) fn supports(from: Format, to: Format) -> bool {
    matches!(
        (from, to),
        (Format::Srt, Format::Vtt | Format::Txt) | (Format::Vtt, Format::Srt | Format::Txt)
    )
}

/// Converts the subtitle file `src` (of format `from`, by its name) to `to`
/// at `dest`, the path the plan reserved. The content decides whether it is
/// read as WebVTT or SRT.
pub(crate) fn convert(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, to);
    let dir = publish::dest_dir(dest, &names)?;
    let cancel = &ctx.cancel;
    let or_cancelled = |e| publish::or_cancelled(cancel, src, e);

    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let bytes = read(src, &names, cancel).map_err(or_cancelled)?;
    let Some((text, encoding)) = decode::decode(&bytes, cancel) else {
        return Err(crate::cancelled(src));
    };
    drop(bytes);
    let doc = match parse::parse(&text, cancel) {
        Ok(doc) => doc,
        Err(ParseError::Cancelled) => return Err(crate::cancelled(src)),
        Err(ParseError::TooLarge) => {
            return Err(names.too_large(format!(
                "parse {}: over the {} line limit for a subtitle file",
                src.display(),
                parse::MAX_LINES
            )))
        }
        Err(ParseError::NoCues) => {
            return Err(GrapeError::new(
                ErrorKind::DecodeFailed,
                format!("{} has no subtitles in it.", names.source),
                format!("no cue in {}, read as {}", src.display(), encoding.name()),
            ))
        }
    };
    drop(text);
    progress(0.5);

    publish::ensure_dir(job, ctx, dir, &names)?;
    // From here on, `temp` removes the temp file on every return path.
    let (temp, file) = TempFile::create(dir, to.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    write_output(file, &doc, to, encoding, temp.path(), &names, cancel).map_err(or_cancelled)?;
    drop(doc);

    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    drop(temp);
    progress(1.0);
    Ok(outcome)
}

/// Reads `src` read-only, refusing a file over [`MAX_SOURCE`] before reading
/// it, and one that grew past it while being read.
fn read(src: &Path, names: &Names, cancel: &CancelToken) -> Result<Vec<u8>, GrapeError> {
    let file =
        File::open(src).map_err(|e| names.read_failed(format!("open {}: {e}", src.display())))?;
    let len = file
        .metadata()
        .map_err(|e| names.read_failed(format!("read {}: {e}", src.display())))?
        .len();
    let too_large = || {
        names.too_large(format!(
            "read {}: over the {} MiB limit for a subtitle file",
            src.display(),
            MAX_SOURCE / (1024 * 1024)
        ))
    };
    if len > MAX_SOURCE {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    // `len` is at most MAX_SOURCE, so it fits a usize.
    bytes.reserve_exact(usize::try_from(len).unwrap_or(0).saturating_add(1));
    Cancellable::new(file, cancel)
        .take(MAX_SOURCE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| names.read_failed(format!("read {}: {e}", src.display())))?;
    if bytes.len() as u64 > MAX_SOURCE {
        return Err(too_large());
    }
    Ok(bytes)
}

/// Writes `doc` as `to` into the temp file and syncs it to disk.
fn write_output(
    file: File,
    doc: &Document,
    to: Format,
    encoding: Encoding,
    temp: &Path,
    names: &Names,
    cancel: &CancelToken,
) -> Result<(), GrapeError> {
    let write_failed = |e: io::Error| names.write_failed(format!("write {}: {e}", temp.display()));
    let mut out = BufWriter::new(Cancellable::new(file, cancel));
    match to {
        Format::Vtt => write::write_vtt(doc, &mut out, cancel),
        // A BOM unless the source was plain UTF-8: Windows players otherwise
        // read a BOM-less SRT in the ANSI code page.
        Format::Srt => write::write_srt(doc, &mut out, encoding != Encoding::Utf8, cancel),
        Format::Txt => write::write_txt(doc, &mut out, cancel),
        other => return Err(names.encode_failed(format!("no subtitle writer for {}", other.key()))),
    }
    .map_err(write_failed)?;
    let file = out
        .into_inner()
        .map_err(|e| write_failed(io::Error::other(e.error().to_string())))?
        .into_inner();
    file.sync_all().map_err(write_failed)
}
