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
//! token is checked between steps, and inside each one through a check
//! closure: parse and write count their work with a [`Ticker`]. The temp
//! file's writer also fails once the job is cancelled.

pub(crate) mod decode;
mod entities;
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

/// How many units of work (lines read, looked at or written; cues) are
/// done between cancel checks. A unit takes well under a microsecond, so a
/// check comes every few milliseconds of work.
pub(crate) const CHECK_EVERY: usize = 64 * 1024;

/// What a step returns once the job is cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cancelled;

/// Counts units of work and asks `is_cancelled` at the first unit and
/// every [`CHECK_EVERY`] after it.
pub(crate) struct Ticker<'a> {
    is_cancelled: &'a mut dyn FnMut() -> bool,
    count: usize,
}

impl<'a> Ticker<'a> {
    pub(crate) fn new(is_cancelled: &'a mut dyn FnMut() -> bool) -> Self {
        Ticker {
            is_cancelled,
            count: 0,
        }
    }

    /// Counts one unit of work, checking first when a check is due.
    pub(crate) fn tick(&mut self) -> Result<(), Cancelled> {
        let due = self.count.is_multiple_of(CHECK_EVERY);
        self.count = self.count.wrapping_add(1);
        if due {
            self.check()
        } else {
            Ok(())
        }
    }

    /// Checks now, whatever the count.
    pub(crate) fn check(&mut self) -> Result<(), Cancelled> {
        if (self.is_cancelled)() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

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
    let mut is_cancelled = || cancel.is_cancelled();
    let bytes = read(src, &names, cancel).map_err(or_cancelled)?;
    let Some((text, encoding)) = decode::decode(&bytes, &mut is_cancelled) else {
        return Err(crate::cancelled(src));
    };
    drop(bytes);
    let doc = parse::parse(&text, &mut is_cancelled)
        .map_err(|e| parse_failed(e, src, &names, encoding, cancel))?;
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

/// The error for a text that `parse` refused. Once the job is cancelled
/// it is a cancel, whatever `parse` found: the cancel may have come after
/// its last check.
fn parse_failed(
    e: ParseError,
    src: &Path,
    names: &Names,
    encoding: Encoding,
    cancel: &CancelToken,
) -> GrapeError {
    let err = match e {
        ParseError::Cancelled => crate::cancelled(src),
        ParseError::TooLarge => names.too_large(format!(
            "parse {}: over the {} line limit for a subtitle file",
            src.display(),
            parse::MAX_LINES
        )),
        ParseError::NoCues => GrapeError::new(
            ErrorKind::DecodeFailed,
            format!("{} has no subtitles in it.", names.source),
            format!("no cue in {}, read as {}", src.display(), encoding.name()),
        ),
    };
    publish::or_cancelled(cancel, src, err)
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
    let mut is_cancelled = || cancel.is_cancelled();
    match to {
        Format::Vtt => write::write_vtt(doc, &mut out, &mut is_cancelled),
        // A BOM unless the source was plain UTF-8: Windows players otherwise
        // read a BOM-less SRT in the ANSI code page.
        Format::Srt => {
            write::write_srt(doc, &mut out, encoding != Encoding::Utf8, &mut is_cancelled)
        }
        Format::Txt => write::write_txt(doc, &mut out, &mut is_cancelled),
        other => return Err(names.encode_failed(format!("no subtitle writer for {}", other.key()))),
    }
    .map_err(write_failed)?;
    let file = out
        .into_inner()
        .map_err(|e| write_failed(io::Error::other(e.error().to_string())))?
        .into_inner();
    file.sync_all().map_err(write_failed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_text_parse_refused_is_a_cancel_once_the_job_is_cancelled() {
        let src = Path::new("shows/subs.srt");
        let names = Names::new(src, Path::new("shows/subs.vtt"), Format::Srt, Format::Vtt);
        let token = CancelToken::new();
        let failed = |e| parse_failed(e, src, &names, Encoding::Utf8, &token);
        let err = failed(ParseError::NoCues);
        assert_eq!(err.kind, ErrorKind::DecodeFailed);
        assert_eq!(err.user_message, "subs.srt has no subtitles in it.");
        let err = failed(ParseError::TooLarge);
        assert_eq!(err.kind, ErrorKind::DecodeFailed);
        assert_eq!(err.user_message, "subs.srt is too large to convert.");
        token.cancel();
        for e in [
            ParseError::Cancelled,
            ParseError::NoCues,
            ParseError::TooLarge,
        ] {
            assert_eq!(failed(e).kind, ErrorKind::Cancelled, "{e:?}");
        }
    }

    #[test]
    fn a_ticker_checks_at_the_first_unit_and_every_check_every_after() {
        let mut calls = 0;
        let mut is_cancelled = || {
            calls += 1;
            false
        };
        {
            let mut ticker = Ticker::new(&mut is_cancelled);
            for _ in 0..2 * CHECK_EVERY + 1 {
                ticker.tick().unwrap();
            }
            ticker.check().unwrap();
        }
        // At units 0, CHECK_EVERY and 2 * CHECK_EVERY, then the check.
        assert_eq!(calls, 4);
    }

    #[test]
    fn a_ticker_stops_at_the_first_check_that_finds_a_cancel() {
        let mut calls = 0;
        let mut is_cancelled = || {
            calls += 1;
            calls > 1
        };
        let mut ticker = Ticker::new(&mut is_cancelled);
        for _ in 0..CHECK_EVERY {
            ticker.tick().unwrap();
        }
        assert_eq!(ticker.tick(), Err(Cancelled));
        assert_eq!(ticker.check(), Err(Cancelled));
    }
}
