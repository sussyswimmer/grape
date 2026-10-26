//! The worker side: [`serve`] runs one request against a [`Codec`].
//!
//! `grape-heif.exe` (libheif), `Grape --grape-heif-worker` (ImageIO) and
//! grape-engines' test fakes all call it, so they check the request, the
//! limits and the codec's output the same way. A codec only decodes into a
//! [`PixelSink`] and encodes [`Pixels`]; `serve` does the files and the
//! lines.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, Once, PoisonError};
use std::thread;

use serde::Deserialize;

use crate::message::{Caps, Done, FailKind, Failure, ImageInfo, Message};
use crate::pixels::{read_pixels, PixelError, PixelHeader, PixelLimits, PixelWriter, Pixels};
use crate::request::{Limits, Op, Request};
use crate::sniff::{sniff, Sniff};
use crate::{names, MAX_REQUEST_BYTES, PROTOCOL, SNIFF_BYTES};

/// The job thread's name; its panics are silent ([`silence_job_panics`]).
const JOB_THREAD: &str = "grape-heif-job";

/// The job thread's stack, as the PDF worker's (D29): codecs recurse, and
/// the main thread's stack is 1 MiB on Windows.
const STACK_BYTES: usize = 64 * 1024 * 1024;

/// How often the Unix watchdog looks for a new parent.
#[cfg(unix)]
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// Decode progress is sent once per this fraction of the rows (5%).
const PROGRESS_STEPS: u64 = 20;

/// The `pixels.rgba` writer's buffer.
const WRITE_BUFFER: usize = 1 << 20;

/// Where a codec's decoded rows go. [`serve`] gives the codec one that
/// checks them and writes `pixels.rgba`.
pub trait PixelSink {
    /// Called once, before any row: the size, the channels (3 or 4) and the
    /// ICC profile (`None`: sRGB). `info.icc_bytes` is ignored: the
    /// profile's own length counts, and one over the limit is dropped. An
    /// error (over the limits, say) must end the decode: return it from
    /// [`Codec::decode`].
    fn begin(&mut self, info: &ImageInfo, icc: Option<&[u8]>) -> Result<(), Failure>;
    /// One row, top first: exactly width × channels bytes, straight alpha.
    /// An error must end the decode, as for [`begin`](Self::begin).
    fn row(&mut self, row: &[u8]) -> Result<(), Failure>;
}

/// A HEIC codec: libheif in `grape-heif.exe`, ImageIO on the Mac, or a
/// test fake.
pub trait Codec {
    /// What it can do on this machine.
    fn caps(&self) -> Caps;
    /// Decodes the primary image of `source` (a file that sniffed as
    /// [`Sniff::Heic`]), upright, 8-bit, straight alpha, under `limits`:
    /// [`PixelSink::begin`] once, before any pixel is decoded, then every
    /// row in order.
    fn decode(
        &self,
        source: &Path,
        limits: &Limits,
        out: &mut dyn PixelSink,
    ) -> Result<(), Failure>;
    /// The HEIC file's bytes for `pixels`. `quality`: `None` is this codec's
    /// default, otherwise 1..=100.
    fn encode(
        &self,
        pixels: &Pixels,
        quality: Option<u8>,
        limits: &Limits,
    ) -> Result<Vec<u8>, Failure>;
}

/// The helper's whole run: one request on stdin, the lines on stdout, and
/// the exit code (0 after `done`, 1 after `error`). Every panic is silent:
/// its message could name what the job was working on.
pub fn worker_main(codec: &(dyn Codec + Sync)) -> ExitCode {
    panic::set_hook(Box::new(|_| {}));
    exit_code(serve(codec, io::stdin().lock(), io::stdout()))
}

fn exit_code(done: bool) -> ExitCode {
    if done {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// [`worker_main`] over any streams: reads one request from `input`, runs
/// it and writes the lines to `output`. True when it ended with `done`.
///
/// 1. Panics on the job thread are made silent.
/// 2. The request is read (at most [`MAX_REQUEST_BYTES`]) and parsed.
/// 3. Its `protocol` must be [`PROTOCOL`] (`protocol` otherwise).
/// 4. `workDir` must be an absolute path to a folder, `decode`'s source an
///    absolute path to a file, `encode`'s quality in 1..=100, and no limit
///    0 (`internal` otherwise).
/// 5. On Unix, a watchdog ends the process with exit code 1 once its
///    parent is gone (Windows has the Job Object for that).
/// 6. The op runs on a thread with a 64 MiB stack; a panic is `internal`.
/// 7. The last line is `done` or `error`.
pub fn serve(codec: &(dyn Codec + Sync), input: impl Read, output: impl Write + Send) -> bool {
    silence_job_panics();
    let lines = Lines {
        out: Mutex::new(output),
    };
    let result = read_request(input).and_then(|request| {
        watch_parent();
        run(codec, &request, &lines)
    });
    lines.finish(result)
}

/// Makes panics on [`JOB_THREAD`] silent, once per process, and leaves the
/// rest to the hook that was there (so a test binary still reports its own
/// failures). [`worker_main`] silences every panic first.
fn silence_job_panics() {
    static SILENCE: Once = Once::new();
    SILENCE.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if thread::current().name() != Some(JOB_THREAD) {
                previous(info);
            }
        }));
    });
}

/// Ends the process once its parent changes (heic-spec J3): a Mac worker
/// whose Grape died is reparented. Started once per process.
#[cfg(unix)]
fn watch_parent() {
    static WATCH: Once = Once::new();
    WATCH.call_once(|| {
        let parent = std::os::unix::process::parent_id();
        // Without the thread the helper still ends with its job or when
        // the parent kills it.
        let _ = thread::Builder::new()
            .name("grape-heif-watchdog".to_string())
            .spawn(move || loop {
                thread::sleep(WATCH_EVERY);
                if std::os::unix::process::parent_id() != parent {
                    std::process::exit(1);
                }
            });
    });
}

#[cfg(not(unix))]
fn watch_parent() {}

/// The output, one JSON line per message, shared by the job thread and the
/// last line.
struct Lines<W> {
    out: Mutex<W>,
}

impl<W: Write> Lines<W> {
    fn send(&self, message: &Message) -> io::Result<()> {
        let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        out.write_all(&line)?;
        out.flush()
    }

    /// Writes the last line; true when it is `done` and got out.
    fn finish(self, result: Result<Done, Failure>) -> bool {
        match result {
            Ok(done) => self.send(&Message::Done(done)).is_ok(),
            Err(failure) => {
                // The parent may be gone; then there's no one to tell.
                let _ = self.send(&Message::Error(failure));
                false
            }
        }
    }
}

/// Reads, parses and checks the request.
fn read_request(input: impl Read) -> Result<Request, Failure> {
    let mut bytes = Vec::new();
    input
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| internal(format!("reading the request: {:?}", e.kind())))?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(internal("the request is too long"));
    }
    // The version first, so another version's request is refused as that,
    // not as unknown fields.
    #[derive(Deserialize)]
    struct Version {
        protocol: u32,
    }
    if let Ok(Version { protocol }) = serde_json::from_slice::<Version>(&bytes) {
        if protocol != PROTOCOL {
            return Err(Failure::new(
                FailKind::Protocol,
                format!("protocol {protocol}, but this helper speaks {PROTOCOL}"),
            ));
        }
    }
    // serde_json's messages can quote the input: report the position only.
    let request: Request = serde_json::from_slice(&bytes).map_err(|e| {
        internal(format!(
            "bad request ({:?} at {}:{})",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;
    check(&request)?;
    Ok(request)
}

/// The checks on the paths, the quality and the limits.
fn check(request: &Request) -> Result<(), Failure> {
    let work_dir = &request.work_dir;
    if !work_dir.is_absolute() || !work_dir.is_dir() {
        return Err(internal(
            "the work folder isn't an absolute path to a folder",
        ));
    }
    match &request.op {
        Op::Decode { source } if !source.is_absolute() || !source.is_file() => {
            return Err(internal("the source isn't an absolute path to a file"));
        }
        Op::Encode {
            quality: Some(quality),
        } if !(1..=100).contains(quality) => {
            return Err(internal(format!("quality {quality} isn't in 1..=100")));
        }
        _ => {}
    }
    let limits = &request.limits;
    let all = [
        limits.max_source_bytes,
        limits.max_pixels,
        limits.max_encode_pixels,
        u64::from(limits.max_encode_side),
        u64::from(limits.max_icc_bytes),
        limits.max_decoder_memory,
    ];
    if all.contains(&0) {
        return Err(internal("a limit is 0"));
    }
    Ok(())
}

/// Runs the op on its own thread, catching a panic.
fn run<W: Write + Send>(
    codec: &(dyn Codec + Sync),
    request: &Request,
    lines: &Lines<W>,
) -> Result<Done, Failure> {
    let panicked = || internal("the job panicked");
    thread::scope(|scope| {
        let job = thread::Builder::new()
            .name(JOB_THREAD.to_string())
            .stack_size(STACK_BYTES)
            .spawn_scoped(scope, || {
                panic::catch_unwind(AssertUnwindSafe(|| execute(codec, request, lines)))
                    .unwrap_or_else(|_| Err(panicked()))
            });
        match job {
            Ok(handle) => handle.join().unwrap_or_else(|_| Err(panicked())),
            Err(e) => Err(internal(format!("starting the job thread: {:?}", e.kind()))),
        }
    })
}

fn execute<W: Write>(
    codec: &(dyn Codec + Sync),
    request: &Request,
    lines: &Lines<W>,
) -> Result<Done, Failure> {
    let work_dir = request.work_dir.as_path();
    let limits = &request.limits;
    match &request.op {
        Op::Probe {} => Ok(Done {
            caps: Some(codec.caps()),
            ..Done::default()
        }),
        Op::Decode { source } => {
            check_source(source, limits)?;
            let mut sink = FileSink::new(work_dir.join(names::PIXELS), limits, lines);
            let decoded = codec.decode(source, limits, &mut sink);
            sink.finish(decoded)
        }
        Op::Encode { quality } => encode(codec, *quality, work_dir, limits),
    }
}

/// The source's size and its `ftyp` box, before the codec sees it.
fn check_source(source: &Path, limits: &Limits) -> Result<(), Failure> {
    let file = File::open(source).map_err(|e| read_failed("opening the source", &e))?;
    let len = file
        .metadata()
        .map_err(|e| read_failed("reading the source's size", &e))?
        .len();
    if len > limits.max_source_bytes {
        return Err(Failure::new(
            FailKind::TooLarge,
            format!("the file is {len} bytes, over {}", limits.max_source_bytes),
        ));
    }
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    file.take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(|e| read_failed("reading the source", &e))?;
    match sniff(&head) {
        Sniff::Heic => Ok(()),
        Sniff::OtherHeif => Err(Failure::new(
            FailKind::Unsupported,
            "a HEIF image that isn't HEVC",
        )),
        Sniff::NotHeif => Err(Failure::new(FailKind::NotHeif, "no HEIF ftyp box")),
    }
}

/// The sink [`serve`] gives a decoding codec: it checks the size and every
/// row, sends the `image` line and writes `pixels.rgba`. Its first failure
/// sticks, and wins over what the codec returns.
struct FileSink<'a, W: Write> {
    path: PathBuf,
    limits: &'a Limits,
    lines: &'a Lines<W>,
    writer: Option<PixelWriter<BufWriter<File>>>,
    info: Option<ImageInfo>,
    rows: u32,
    step: u64,
    failed: Option<Failure>,
}

impl<'a, W: Write> FileSink<'a, W> {
    fn new(path: PathBuf, limits: &'a Limits, lines: &'a Lines<W>) -> Self {
        FileSink {
            path,
            limits,
            lines,
            writer: None,
            info: None,
            rows: 0,
            step: 0,
            failed: None,
        }
    }

    /// Keeps the first failure.
    fn note(&mut self, result: Result<(), Failure>) -> Result<(), Failure> {
        if let (Err(failure), None) = (&result, &self.failed) {
            self.failed = Some(failure.clone());
        }
        result
    }

    fn start(&mut self, info: &ImageInfo, icc: Option<&[u8]>) -> Result<(), Failure> {
        if self.info.is_some() {
            return Err(internal("the codec began the image twice"));
        }
        let (width, height) = (info.width, info.height);
        if width == 0 || height == 0 {
            return Err(internal("the codec gave an empty image"));
        }
        if u64::from(width) * u64::from(height) > self.limits.max_pixels {
            return Err(Failure::new(
                FailKind::TooLarge,
                format!("{width}x{height} is over {} pixels", self.limits.max_pixels),
            ));
        }
        if !matches!(info.channels, 3 | 4) {
            return Err(internal(format!(
                "the codec gave {} channels",
                info.channels
            )));
        }
        // A profile over the limit is dropped, not refused.
        let max_icc = self.limits.max_icc_bytes;
        let icc =
            icc.filter(|p| !p.is_empty() && u32::try_from(p.len()).is_ok_and(|n| n <= max_icc));
        let icc_bytes = icc.map_or(0, |p| u32::try_from(p.len()).unwrap_or(0));
        let info = ImageInfo { icc_bytes, ..*info };
        self.lines
            .send(&Message::Image(info))
            .map_err(|e| internal(format!("writing to the parent: {:?}", e.kind())))?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .map_err(|e| write_failed("creating pixels.rgba", &e))?;
        let header = PixelHeader {
            width,
            height,
            channels: info.channels,
            icc_len: icc_bytes,
        };
        let writer = PixelWriter::begin(BufWriter::with_capacity(WRITE_BUFFER, file), &header, icc)
            .map_err(|e| write_failed("writing pixels.rgba", &e))?;
        self.info = Some(info);
        self.writer = Some(writer);
        Ok(())
    }

    fn next_row(&mut self, row: &[u8]) -> Result<(), Failure> {
        let (Some(info), Some(writer)) = (self.info, self.writer.as_mut()) else {
            return Err(internal("the codec gave a row before the image"));
        };
        if self.rows == info.height {
            return Err(internal("the codec gave more rows than the height"));
        }
        let expected = u64::from(info.width) * u64::from(info.channels);
        if row.len() as u64 != expected {
            return Err(internal(format!(
                "the codec gave a row of {} bytes, not {expected}",
                row.len()
            )));
        }
        writer
            .row(row)
            .map_err(|e| write_failed("writing pixels.rgba", &e))?;
        self.rows += 1;
        let step = u64::from(self.rows) * PROGRESS_STEPS / u64::from(info.height);
        if step > self.step {
            self.step = step;
            // A parent that's gone shows up at the last line.
            let _ = self
                .lines
                .send(&Message::Progress(self.rows as f32 / info.height as f32));
        }
        Ok(())
    }

    /// After the codec returned: every row there, the file synced.
    fn finish(self, decoded: Result<(), Failure>) -> Result<Done, Failure> {
        if let Some(failure) = self.failed {
            return Err(failure);
        }
        decoded?;
        let (Some(info), Some(writer)) = (self.info, self.writer) else {
            return Err(internal("the codec gave no image"));
        };
        if self.rows != info.height {
            return Err(internal(format!(
                "the codec gave {} of {} rows",
                self.rows, info.height
            )));
        }
        writer
            .finish()
            .and_then(|out| out.into_inner().map_err(|e| e.into_error()))
            .and_then(|file| file.sync_all())
            .map_err(|e| write_failed("writing pixels.rgba", &e))?;
        Ok(Done {
            width: info.width,
            height: info.height,
            channels: info.channels,
            icc_bytes: info.icc_bytes,
            ..Done::default()
        })
    }
}

impl<W: Write> PixelSink for FileSink<'_, W> {
    fn begin(&mut self, info: &ImageInfo, icc: Option<&[u8]>) -> Result<(), Failure> {
        if let Some(failure) = &self.failed {
            return Err(failure.clone());
        }
        let result = self.start(info, icc);
        self.note(result)
    }

    fn row(&mut self, row: &[u8]) -> Result<(), Failure> {
        if let Some(failure) = &self.failed {
            return Err(failure.clone());
        }
        let result = self.next_row(row);
        self.note(result)
    }
}

/// `input.rgba` → the codec → `out.heic`.
fn encode(
    codec: &(dyn Codec + Sync),
    quality: Option<u8>,
    work_dir: &Path,
    limits: &Limits,
) -> Result<Done, Failure> {
    let file = File::open(work_dir.join(names::INPUT))
        .map_err(|e| read_failed("opening input.rgba", &e))?;
    let len = file
        .metadata()
        .map_err(|e| read_failed("reading input.rgba's size", &e))?
        .len();
    let pixel_limits = PixelLimits {
        max_pixels: limits.max_encode_pixels,
        max_side: limits.max_encode_side,
        max_icc_bytes: limits.max_icc_bytes,
    };
    let pixels = read_pixels(BufReader::new(file), len, &pixel_limits).map_err(|e| match e {
        PixelError::Io(e) => read_failed("reading input.rgba", &e),
        PixelError::TooLarge(why) => Failure::new(FailKind::TooLarge, why),
        PixelError::Malformed(why) => internal(format!("input.rgba: {why}")),
    })?;
    let heic = codec.encode(&pixels, quality, limits)?;
    drop(pixels);
    if sniff(&heic[..heic.len().min(SNIFF_BYTES)]) != Sniff::Heic {
        return Err(internal("the encoder's output isn't HEIC"));
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(work_dir.join(names::OUTPUT))
        .map_err(|e| write_failed("creating out.heic", &e))?;
    out.write_all(&heic)
        .and_then(|()| out.sync_all())
        .map_err(|e| write_failed("writing out.heic", &e))?;
    Ok(Done {
        bytes: heic.len() as u64,
        ..Done::default()
    })
}

fn internal(detail: impl Into<String>) -> Failure {
    Failure::new(FailKind::Internal, detail)
}

fn read_failed(what: &str, e: &io::Error) -> Failure {
    Failure::new(FailKind::Read, format!("{what}: {:?}", e.kind()))
}

fn write_failed(what: &str, e: &io::Error) -> Failure {
    Failure::new(FailKind::Write, format!("{what}: {:?}", e.kind()))
}

#[cfg(test)]
mod tests;
