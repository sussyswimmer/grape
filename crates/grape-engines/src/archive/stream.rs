//! One job's shared state ([`Run`]) and the readers that report to it.
//!
//! The source is read as `File` → [`Counting`] (progress, read errors) →
//! `Cancellable` → `BufReader`; a tar or gzip stream then goes through
//! [`Metered`] (the decompressed bytes, against [`Limits`]); every entry's
//! data goes through [`Pump`] (cancel, the bytes written, progress), with
//! [`Exact`] holding it to the size its header gives. Everything runs on
//! the job's thread, so the state is in `Cell`s.

use std::cell::{Cell, RefCell};
use std::io::{self, Read, Seek, SeekFrom, Write};

use super::{Failure, Limits, Over};
use crate::CancelToken;

/// The most header and metadata bytes (GNU long names, PAX records) the
/// tar reader may take in front of one entry. The tar crate reads them
/// whole into memory; real ones are at most a few KiB.
pub(crate) const MAX_HEADER_BYTES: u64 = 16 * 1024 * 1024;

/// Progress is reported in steps of at least this much.
const PROGRESS_STEP: f32 = 0.005;

/// Progress stays below this until the job has published its output.
const PROGRESS_CAP: f32 = 0.99;

/// Which side of a copy failed, when nothing more specific is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    /// Reading or decoding the source.
    Read,
    /// Writing the output.
    Write,
}

/// One job's shared state: cancel, limits, progress and what went wrong.
pub(crate) struct Run<'a> {
    cancel: &'a CancelToken,
    /// The bounds this job enforces.
    pub(crate) limits: Limits,
    /// The source's size in bytes.
    src_len: u64,
    progress: RefCell<&'a mut dyn FnMut(f32)>,
    /// What progress counts up to: the source's size, twice when it is
    /// read twice.
    progress_total: Cell<u64>,
    /// Source bytes read so far.
    consumed: Cell<u64>,
    /// The last fraction reported.
    reported: Cell<f32>,
    /// Entries seen, written or not.
    entries: Cell<u64>,
    /// Decompressed tar or gzip stream bytes.
    stream_bytes: Cell<u64>,
    /// Entry data bytes.
    content_bytes: Cell<u64>,
    /// Stream bytes read since the current tar entry began, outside its data.
    header_bytes: Cell<u64>,
    /// Bytes of the last tar entry's data still to come before the next
    /// header, which the tar reader skips itself; they count as data.
    header_credit: Cell<u64>,
    /// True while a [`Pump`] reads, so [`Metered`] can tell data from headers.
    in_content: Cell<bool>,
    /// The bound the job went over.
    over: Cell<Option<Over>>,
    /// Reading the source file itself failed.
    source_failed: Cell<bool>,
    /// Writing the output failed inside a reader ([`Tee`]).
    write_failed: Cell<bool>,
    /// The first failure a [`Pump`] saw on the read side.
    read_failure: RefCell<Option<Failure>>,
}

impl<'a> Run<'a> {
    /// The state for one job over a source of `src_len` bytes.
    pub(crate) fn new(
        cancel: &'a CancelToken,
        limits: Limits,
        src_len: u64,
        progress: &'a mut dyn FnMut(f32),
    ) -> Run<'a> {
        Run {
            cancel,
            limits,
            src_len,
            progress: RefCell::new(progress),
            progress_total: Cell::new(src_len.max(1)),
            consumed: Cell::new(0),
            reported: Cell::new(0.0),
            entries: Cell::new(0),
            stream_bytes: Cell::new(0),
            content_bytes: Cell::new(0),
            header_bytes: Cell::new(0),
            header_credit: Cell::new(0),
            in_content: Cell::new(false),
            over: Cell::new(None),
            source_failed: Cell::new(false),
            write_failed: Cell::new(false),
            read_failure: RefCell::new(None),
        }
    }

    /// The job's cancel token.
    pub(crate) fn token(&self) -> &CancelToken {
        self.cancel
    }

    /// `Err(Cancelled)` once the job is cancelled.
    pub(crate) fn check(&self) -> Result<(), Failure> {
        if self.cancel.is_cancelled() {
            Err(Failure::Cancelled)
        } else {
            Ok(())
        }
    }

    /// The source will be read `passes` times in all.
    pub(crate) fn set_passes(&self, passes: u64) {
        self.progress_total
            .set(self.src_len.saturating_mul(passes).max(1));
    }

    /// Forgets the source bytes read so far (the ZIP directory, read
    /// first, from the end).
    pub(crate) fn reset_consumed(&self) {
        self.consumed.set(0);
    }

    fn add_consumed(&self, n: usize) {
        self.consumed
            .set(self.consumed.get().saturating_add(n as u64));
        self.tick();
    }

    /// Reports progress if it moved enough: source bytes read over the
    /// total, never backwards, below [`PROGRESS_CAP`].
    fn tick(&self) {
        let fraction = (self.consumed.get() as f64 / self.progress_total.get() as f64) as f32;
        let fraction = fraction.clamp(0.0, PROGRESS_CAP);
        if fraction >= self.reported.get() + PROGRESS_STEP {
            self.reported.set(fraction);
            if let Ok(mut progress) = self.progress.try_borrow_mut() {
                (*progress)(fraction);
            }
        }
    }

    /// Counts one more entry against [`Limits::max_entries`].
    pub(crate) fn count_entry(&self) -> Result<(), Failure> {
        let n = self.entries.get() + 1;
        self.entries.set(n);
        if n > self.limits.max_entries {
            self.over.set(Some(Over::Entries));
            return Err(Failure::TooBig(Over::Entries));
        }
        Ok(())
    }

    /// The bound `total` bytes written would break, if any.
    fn bound(&self, total: u64) -> Option<Over> {
        if total > self.limits.max_total {
            Some(Over::Total)
        } else if total > self.limits.ratio_floor
            && total > self.limits.max_ratio.saturating_mul(self.src_len)
        {
            Some(Over::Ratio)
        } else {
            None
        }
    }

    /// Refuses an archive whose declared sizes already break a bound.
    pub(crate) fn precheck(&self, declared: u64) -> Result<(), Failure> {
        match self.bound(declared) {
            Some(over) => {
                self.over.set(Some(over));
                Err(Failure::TooBig(over))
            }
            None => Ok(()),
        }
    }

    fn charge(&self, counter: &Cell<u64>, n: usize) -> io::Result<()> {
        let total = counter.get().saturating_add(n as u64);
        counter.set(total);
        match self.bound(total) {
            Some(over) => Err(self.went_over(over)),
            None => Ok(()),
        }
    }

    fn went_over(&self, over: Over) -> io::Error {
        self.over.set(Some(over));
        io::Error::other("over an archive bound")
    }

    /// Starts counting a new tar entry's header bytes.
    pub(crate) fn start_entry(&self) {
        self.start_entry_after(0);
    }

    /// Starts counting a new tar entry's header bytes after the `skipped`
    /// bytes of the last entry's data that the tar reader will skip first
    /// (when Grape didn't read that data itself).
    pub(crate) fn start_entry_after(&self, skipped: u64) {
        self.header_bytes.set(0);
        self.header_credit.set(skipped);
    }

    /// Source bytes read so far, for tests.
    #[cfg(test)]
    pub(crate) fn consumed(&self) -> u64 {
        self.consumed.get()
    }

    /// Forgets the bytes written so far, for a second pass over the same
    /// data (a plain .gz is measured first, then written).
    pub(crate) fn reset_budget(&self) {
        self.stream_bytes.set(0);
        self.content_bytes.set(0);
        self.header_bytes.set(0);
        self.header_credit.set(0);
    }

    /// Records the first read-side failure a [`Pump`] saw.
    fn note_read(&self, e: &io::Error) {
        if self.cancel.is_cancelled() || self.over.get().is_some() {
            return;
        }
        let mut slot = self.read_failure.borrow_mut();
        if slot.is_none() {
            *slot = Some(if self.source_failed.get() {
                Failure::Read(format!("reading the source: {:?}", e.kind()))
            } else {
                Failure::Corrupt(format!("entry data: {:?}", e.kind()))
            });
        }
    }

    /// What an `io` error from an archive operation means: a cancel, a
    /// bound, the output, the source, or else `side`'s default (`what` says
    /// which step, for the log).
    pub(crate) fn failure(&self, e: &io::Error, side: Side, what: &str) -> Failure {
        if self.cancel.is_cancelled() {
            return Failure::Cancelled;
        }
        if let Some(over) = self.over.get() {
            return Failure::TooBig(over);
        }
        if self.write_failed.get() {
            return Failure::Write(format!("{what}: writing the output: {:?}", e.kind()));
        }
        if let Some(failure) = self.read_failure.borrow_mut().take() {
            return failure;
        }
        if self.source_failed.get() {
            return Failure::Read(format!("{what}: reading the source: {:?}", e.kind()));
        }
        match side {
            Side::Read => Failure::Corrupt(format!("{what}: {:?}", e.kind())),
            Side::Write => Failure::Write(format!("{what}: {:?}", e.kind())),
        }
    }

    /// [`Run::failure`] for an error from the zip crate.
    pub(crate) fn zip_failure(&self, e: zip::result::ZipError, side: Side, what: &str) -> Failure {
        use zip::result::ZipError;
        if self.cancel.is_cancelled() {
            return Failure::Cancelled;
        }
        match e {
            ZipError::Io(io) => self.failure(&io, side, what),
            ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED) => Failure::Encrypted,
            ZipError::UnsupportedArchive(why) => Failure::Corrupt(format!("{what}: {why}")),
            ZipError::InvalidArchive(why) => match why {
                std::borrow::Cow::Borrowed(why) => Failure::Corrupt(format!("{what}: {why}")),
                std::borrow::Cow::Owned(_) => Failure::Corrupt(format!("{what}: invalid archive")),
            },
            other => {
                let shown = match &other {
                    ZipError::FileNotFound => "entry not found",
                    ZipError::InvalidPassword => "invalid password",
                    ZipError::CompressionMethodNotSupported(_) => "compression method",
                    _ => "zip error",
                };
                match side {
                    Side::Read => Failure::Corrupt(format!("{what}: {shown}")),
                    Side::Write => Failure::Write(format!("{what}: {shown}")),
                }
            }
        }
    }
}

/// The source file, counting the bytes read for progress and noting when
/// the file itself fails.
pub(crate) struct Counting<'r, 'a, R> {
    run: &'r Run<'a>,
    inner: R,
}

impl<'r, 'a, R> Counting<'r, 'a, R> {
    pub(crate) fn new(run: &'r Run<'a>, inner: R) -> Self {
        Counting { run, inner }
    }
}

impl<R: Read> Read for Counting<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buf) {
            Ok(n) => {
                self.run.add_consumed(n);
                Ok(n)
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::Interrupted {
                    self.run.source_failed.set(true);
                }
                Err(e)
            }
        }
    }
}

impl<R: Seek> Seek for Counting<'_, '_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// A decompressed tar or gzip stream, charged against the limits as it is
/// read. Bytes read outside an entry's data (headers, long names, PAX
/// records) are also held to [`MAX_HEADER_BYTES`] per entry.
pub(crate) struct Metered<'r, 'a, R> {
    run: &'r Run<'a>,
    inner: R,
}

impl<'r, 'a, R> Metered<'r, 'a, R> {
    pub(crate) fn new(run: &'r Run<'a>, inner: R) -> Self {
        Metered { run, inner }
    }
}

impl<R: Read> Read for Metered<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let run = self.run;
        run.charge(&run.stream_bytes, n)?;
        if !run.in_content.get() {
            // The data the tar reader skips comes first.
            let credit = run.header_credit.get();
            let skipped = credit.min(n as u64);
            run.header_credit.set(credit - skipped);
            let header = run.header_bytes.get().saturating_add(n as u64 - skipped);
            run.header_bytes.set(header);
            if header > MAX_HEADER_BYTES {
                return Err(run.went_over(Over::Header));
            }
        }
        Ok(n)
    }
}

/// An entry's data: stops once cancelled, charges what it reads as written
/// bytes, and notes read-side failures so the writer that called it can
/// tell them from its own.
pub(crate) struct Pump<'r, 'a, R> {
    run: &'r Run<'a>,
    inner: R,
}

impl<'r, 'a, R> Pump<'r, 'a, R> {
    pub(crate) fn new(run: &'r Run<'a>, inner: R) -> Self {
        Pump { run, inner }
    }
}

impl<R: Read> Read for Pump<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let run = self.run;
        if run.cancel.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        run.in_content.set(true);
        let result = self.inner.read(buf);
        run.in_content.set(false);
        match result {
            Ok(n) => {
                run.charge(&run.content_bytes, n)?;
                Ok(n)
            }
            Err(e) => {
                run.note_read(&e);
                Err(e)
            }
        }
    }
}

/// `inner`, held to exactly `left` bytes: shorter data and longer data are
/// both errors. The read after the last byte goes to `inner`, so a ZIP
/// entry's or gzip member's checksum is verified.
pub(crate) struct Exact<R> {
    inner: R,
    left: u64,
}

impl<R> Exact<R> {
    pub(crate) fn new(inner: R, size: u64) -> Self {
        Exact { inner, left: size }
    }
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "entry data is longer than its size",
                )),
            };
        }
        let max = usize::try_from(self.left)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "entry data is shorter than its size",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// `inner`, with every byte read also written to `out`: a tar converted to
/// tar.gz (or back) keeps its bytes exactly, while the tar reader on top
/// still checks every entry.
pub(crate) struct Tee<'r, 'a, R, W> {
    run: &'r Run<'a>,
    inner: R,
    out: W,
}

impl<'r, 'a, R, W> Tee<'r, 'a, R, W> {
    pub(crate) fn new(run: &'r Run<'a>, inner: R, out: W) -> Self {
        Tee { run, inner, out }
    }

    /// The output writer.
    pub(crate) fn into_out(self) -> W {
        self.out
    }
}

impl<R: Read, W: Write> Read for Tee<'_, '_, R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Err(e) = self.out.write_all(&buf[..n]) {
            self.run.write_failed.set(true);
            return Err(e);
        }
        Ok(n)
    }
}

/// Reads up to `max` bytes, fewer only at the end of the stream.
pub(crate) fn read_prefix<R: Read>(reader: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut prefix = Vec::with_capacity(max);
    reader.by_ref().take(max as u64).read_to_end(&mut prefix)?;
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use super::*;

    fn limits() -> Limits {
        Limits {
            max_entries: 3,
            max_total: 100,
            ratio_floor: 10,
            max_ratio: 4,
        }
    }

    #[test]
    fn exact_refuses_short_and_long_data() {
        let mut ok = Vec::new();
        Exact::new(&b"abc"[..], 3).read_to_end(&mut ok).unwrap();
        assert_eq!(ok, b"abc");
        let short = Exact::new(&b"ab"[..], 3).read_to_end(&mut Vec::new());
        assert_eq!(short.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let long = Exact::new(&b"abcd"[..], 3).read_to_end(&mut Vec::new());
        assert_eq!(long.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn bounds_count_entries_bytes_and_ratio() {
        let cancel = CancelToken::new();
        let mut progress = |_: f32| {};
        // A 5-byte source: 4× is 20 bytes, over the 10-byte floor.
        let run = Run::new(&cancel, limits(), 5, &mut progress);
        for _ in 0..3 {
            run.count_entry().unwrap();
        }
        assert!(matches!(
            run.count_entry(),
            Err(Failure::TooBig(Over::Entries))
        ));

        let run = Run::new(&cancel, limits(), 5, &mut progress);
        let mut pump = Pump::new(&run, Cursor::new(vec![0u8; 21]));
        let err = pump.read_to_end(&mut Vec::new()).unwrap_err();
        assert!(matches!(
            run.failure(&err, Side::Read, "test"),
            Failure::TooBig(Over::Ratio)
        ));

        // A big source: only the total applies.
        let run = Run::new(&cancel, limits(), 1_000, &mut progress);
        let mut pump = Pump::new(&run, Cursor::new(vec![0u8; 101]));
        let err = pump.read_to_end(&mut Vec::new()).unwrap_err();
        assert!(matches!(
            run.failure(&err, Side::Write, "test"),
            Failure::TooBig(Over::Total)
        ));
        assert!(matches!(
            run.precheck(101),
            Err(Failure::TooBig(Over::Total))
        ));
    }

    #[test]
    fn headers_are_held_to_their_own_bound() {
        let cancel = CancelToken::new();
        let mut progress = |_: f32| {};
        let big = Limits {
            max_entries: 10,
            max_total: u64::MAX,
            ratio_floor: u64::MAX,
            max_ratio: 1,
        };
        let run = Run::new(&cancel, big, 1, &mut progress);
        let data = vec![0u8; (MAX_HEADER_BYTES + 1) as usize];
        // Read as entry data, it is fine.
        let mut pump = Pump::new(&run, Metered::new(&run, Cursor::new(data.clone())));
        io::copy(&mut pump, &mut io::sink()).unwrap();
        // Read as headers, it is over.
        let mut metered = Metered::new(&run, Cursor::new(data));
        let err = io::copy(&mut metered, &mut io::sink()).unwrap_err();
        assert!(matches!(
            run.failure(&err, Side::Read, "test"),
            Failure::TooBig(Over::Header)
        ));
    }

    #[test]
    fn data_the_tar_reader_skips_is_not_a_header() {
        let cancel = CancelToken::new();
        let mut progress = |_: f32| {};
        let big = Limits {
            max_entries: 10,
            max_total: u64::MAX,
            ratio_floor: u64::MAX,
            max_ratio: 1,
        };
        let run = Run::new(&cancel, big, 1, &mut progress);
        let skipped = MAX_HEADER_BYTES * 2;
        let len = usize::try_from(skipped + MAX_HEADER_BYTES).unwrap();
        // The skipped data, then exactly the most header bytes allowed.
        run.start_entry_after(skipped);
        let mut metered = Metered::new(&run, Cursor::new(vec![0u8; len]));
        io::copy(&mut metered, &mut io::sink()).unwrap();
        // One more is over; the credit doesn't carry over to it.
        let mut metered = Metered::new(&run, Cursor::new(vec![0u8; 1]));
        let err = io::copy(&mut metered, &mut io::sink()).unwrap_err();
        assert!(matches!(
            run.failure(&err, Side::Read, "test"),
            Failure::TooBig(Over::Header)
        ));
    }

    #[test]
    fn progress_is_monotone_and_capped() {
        let cancel = CancelToken::new();
        let mut seen = Vec::new();
        {
            let mut progress = |f: f32| seen.push(f);
            let run = Run::new(&cancel, limits(), 1_000, &mut progress);
            let mut reader = Counting::new(&run, Cursor::new(vec![1u8; 1_000]));
            let mut buf = [0u8; 7];
            while reader.read(&mut buf).unwrap() > 0 {}
            run.reset_consumed();
            let mut reader = Counting::new(&run, Cursor::new(vec![1u8; 300]));
            io::copy(&mut reader, &mut io::sink()).unwrap();
        }
        assert!(!seen.is_empty());
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
        assert!(seen.iter().all(|f| (0.0..=PROGRESS_CAP).contains(f)));
    }

    #[test]
    fn cancel_stops_a_pump_and_wins_over_other_failures() {
        let cancel = CancelToken::new();
        let mut progress = |_: f32| {};
        let run = Run::new(&cancel, limits(), 1_000, &mut progress);
        let mut pump = Pump::new(&run, Cursor::new(vec![0u8; 10]));
        cancel.cancel();
        let err = pump.read(&mut [0u8; 4]).unwrap_err();
        assert!(matches!(
            run.failure(&err, Side::Write, "test"),
            Failure::Cancelled
        ));
        assert!(matches!(run.check(), Err(Failure::Cancelled)));
    }

    #[test]
    fn tee_copies_what_it_reads() {
        let cancel = CancelToken::new();
        let mut progress = |_: f32| {};
        let run = Run::new(&cancel, limits(), 1_000, &mut progress);
        let mut tee = Tee::new(&run, Cursor::new(b"hello".to_vec()), Vec::new());
        let mut first = [0u8; 2];
        tee.read_exact(&mut first).unwrap();
        io::copy(&mut tee, &mut io::sink()).unwrap();
        assert_eq!(tee.into_out(), b"hello");
        assert_eq!(read_prefix(&mut &b"abcdef"[..], 4).unwrap(), b"abcd");
        assert_eq!(read_prefix(&mut &b"ab"[..], 4).unwrap(), b"ab");
    }
}
