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
/// With [`parse::MAX_LINES`] and [`parse::MAX_LINE_BYTES`] this bounds the
/// memory and time a conversion takes.
pub(crate) const MAX_SOURCE: u64 = 16 * 1024 * 1024;

/// How many units of work are done between cancel checks. A line read,
/// looked at, turned into spans or written is a unit, and so is each
/// [`BYTES_PER_UNIT`] of it; so are a cue and a span written. A unit takes
/// under a microsecond in a release build: measured, checks come 4 to 7 ms
/// of work apart as a rule and about 13 ms at most, however long or dense
/// the lines. The one longer gap is sorting the cue keys, which can't
/// check inside (`sort_by_start` in [`parse`]): 19 to 27 ms for 500,000
/// cues, the most a text can hold.
pub(crate) const CHECK_EVERY: usize = 16 * 1024;

/// How many bytes of a line count as one more unit of work. Turning dense
/// markup into spans, the slowest work per byte, takes about this many
/// bytes to cost what a short line does.
pub(crate) const BYTES_PER_UNIT: usize = 8;

/// What a step returns once the job is cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cancelled;

/// Counts units of work and asks `is_cancelled` before the first piece
/// of work and before each piece that comes [`CHECK_EVERY`] or more units
/// after the last check. One piece may be several units (a long line), so
/// a check is never more than one piece late.
pub(crate) struct Ticker<'a> {
    is_cancelled: &'a mut dyn FnMut() -> bool,
    /// The units counted since the last check; [`CHECK_EVERY`] at first,
    /// so the first piece of work is checked.
    since_check: usize,
}

impl<'a> Ticker<'a> {
    pub(crate) fn new(is_cancelled: &'a mut dyn FnMut() -> bool) -> Self {
        Ticker {
            is_cancelled,
            since_check: CHECK_EVERY,
        }
    }

    /// Counts a piece of work of `units`, checking first when a check is
    /// due.
    pub(crate) fn work(&mut self, units: usize) -> Result<(), Cancelled> {
        if self.since_check >= CHECK_EVERY {
            self.since_check = 0;
            self.check()?;
        }
        self.since_check = self.since_check.saturating_add(units);
        Ok(())
    }

    /// Counts one unit of work.
    pub(crate) fn tick(&mut self) -> Result<(), Cancelled> {
        self.work(1)
    }

    /// Counts the work on one line of text: a unit, and one more for each
    /// [`BYTES_PER_UNIT`] of it.
    pub(crate) fn line(&mut self, line: &str) -> Result<(), Cancelled> {
        self.work(1 + line.len() / BYTES_PER_UNIT)
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
    let (text, encoding) = decoded(&bytes, src, &mut is_cancelled)?;
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

/// The source's bytes as text, and the encoding they were read in.
/// [`decode::decode`] gives nothing only once the job is cancelled, so
/// that is a cancel.
fn decoded(
    bytes: &[u8],
    src: &Path,
    is_cancelled: &mut dyn FnMut() -> bool,
) -> Result<(String, Encoding), GrapeError> {
    decode::decode(bytes, is_cancelled).ok_or_else(|| crate::cancelled(src))
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
            "parse {}: over the limit of {} lines of at most {} KiB for a subtitle file",
            src.display(),
            parse::MAX_LINES,
            parse::MAX_LINE_BYTES / 1024
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

    use std::fs;
    use std::path::PathBuf;

    use super::*;

    /// A fresh folder under the system temp dir, removed when dropped.
    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Dir {
            let path = std::env::temp_dir().join(format!(
                "grape-subtitle-unit-{}",
                crate::output::unique_suffix()
            ));
            fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_source_over_the_limit_is_refused_without_reading_it() {
        let dir = Dir::new();
        let src = dir.0.join("huge.srt");
        // Sparse where the file system allows it.
        File::create(&src).unwrap().set_len(MAX_SOURCE + 1).unwrap();
        let names = Names::new(&src, &dir.0.join("huge.vtt"), Format::Srt, Format::Vtt);
        // Cancelled, so reading any of it would fail as a read error.
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = read(&src, &names, &cancel).unwrap_err();
        assert_eq!(err.kind, ErrorKind::DecodeFailed);
        assert_eq!(err.user_message, "huge.srt is too large to convert.");
        assert!(
            err.detail.contains("over the 16 MiB limit"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn reading_the_source_stops_once_the_job_is_cancelled() {
        let dir = Dir::new();
        let src = dir.0.join("subs.srt");
        let srt = b"1\n00:00:01,000 --> 00:00:02,000\nHi\n";
        fs::write(&src, srt).unwrap();
        let names = Names::new(&src, &dir.0.join("subs.vtt"), Format::Srt, Format::Vtt);
        let cancel = CancelToken::new();
        assert_eq!(read(&src, &names, &cancel).unwrap(), srt);
        cancel.cancel();
        let err = read(&src, &names, &cancel).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReadFailed);
        assert_eq!(err.user_message, "Grape couldn't read subs.srt.");
        // convert reports it as the cancel it is.
        let err = publish::or_cancelled(&cancel, &src, err);
        assert_eq!(err.kind, ErrorKind::Cancelled);
    }

    #[test]
    fn writing_fails_once_the_job_is_cancelled_and_the_temp_file_goes() {
        let dir = Dir::new();
        let src = dir.0.join("subs.srt");
        let names = Names::new(&src, &dir.0.join("subs.vtt"), Format::Srt, Format::Vtt);
        let doc = parse::parse("1\n00:00:01,000 --> 00:00:02,000\nHi\n", &mut || false).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        for to in [Format::Vtt, Format::Srt, Format::Txt] {
            let (temp, file) = TempFile::create(&dir.0, to.output_ext()).unwrap();
            let err = write_output(file, &doc, to, Encoding::Utf8, temp.path(), &names, &cancel)
                .unwrap_err();
            assert_eq!(err.kind, ErrorKind::WriteFailed, "{to:?}");
            let err = publish::or_cancelled(&cancel, &src, err);
            assert_eq!(err.kind, ErrorKind::Cancelled, "{to:?}");
            drop(temp);
            assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0, "{to:?}");
        }
    }

    #[test]
    fn a_job_cancelled_while_decoding_is_cancelled() {
        let src = Path::new("shows/subs.srt");
        let srt = b"1\n00:00:01,000 --> 00:00:02,000\nCaf\xE9\n";
        let (text, encoding) = decoded(srt, src, &mut || false).unwrap();
        assert!(text.ends_with("Café\n"));
        assert_eq!(encoding, Encoding::Windows1252);
        let err = decoded(srt, src, &mut || true).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);
        assert_eq!(err.user_message, crate::cancelled(src).user_message);
    }

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
    fn a_long_piece_of_work_brings_the_next_check_forward() {
        let mut calls = 0;
        let mut is_cancelled = || {
            calls += 1;
            false
        };
        {
            let mut ticker = Ticker::new(&mut is_cancelled);
            // Checked before it, and before the piece after it.
            ticker.work(3 * CHECK_EVERY).unwrap();
            ticker.tick().unwrap();
            // Then not again for CHECK_EVERY units.
            for _ in 0..CHECK_EVERY - 1 {
                ticker.tick().unwrap();
            }
        }
        assert_eq!(calls, 2);
    }

    #[test]
    fn a_line_is_a_unit_and_one_more_per_eight_bytes() {
        // Fixed sizes, so a change to CHECK_EVERY or BYTES_PER_UNIT shows:
        // 1 + 131,064 / 8 is 16,384 units, a check's worth; one less isn't.
        let checks_after = |len: usize| {
            let mut calls = 0;
            let mut is_cancelled = || {
                calls += 1;
                false
            };
            {
                let mut ticker = Ticker::new(&mut is_cancelled);
                ticker.line(&"x".repeat(len)).unwrap();
                ticker.tick().unwrap();
            }
            calls - 1
        };
        assert_eq!(checks_after(131_064), 1);
        assert_eq!(checks_after(131_063), 0);
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
