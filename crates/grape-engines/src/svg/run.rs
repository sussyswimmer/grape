//! The parent's side of the SVG worker (D39), on [`crate::helper`]'s
//! `Launch`, `start`, `Running` and `WorkDir`: what `svg::convert` and
//! `pdf::create` call to get an SVG's pixels ([`Renderer::render`]).
//!
//! - **Rule 3 first:** the source's first bytes are sniffed before
//!   anything starts. PNG, JPEG, WebP, TIFF or BMP content named `.svg` is
//!   the raster engine's ([`Drawn::Raster`]); gzip and text go to the
//!   worker. A file over the worker's source limit is refused unread.
//! - **The worker** runs in a private work folder under the OS temp folder
//!   (`grape-svg-{pid}-{nanos}-{n}`, 0700 on Unix), detached, with stdin
//!   and stdout piped and stderr discarded, contained by the app's hook (a
//!   Job Object on Windows, [`Limits::worker_memory`]) before it gets its
//!   request. The job polls every 100 ms: a cancel or the deadline kills
//!   it. The deadline is [`Limits::timeout_base`] from the start, extended
//!   once the `image` line arrives by [`Limits::per_megapixel`] for each
//!   started megapixel ([`MEGAPIXEL`]) of the canvas.
//! - **The `image` line** must be within the caps the request sent, and
//!   converting that many RGBA pixels to the target must fit the working
//!   set ([`image::pixels_fit`]); otherwise the worker is killed before it
//!   allocates anything. The pixels are then read once, into the one
//!   buffer that check counted ([`helper::read_pixel_file`]).
//! - **How it ended:** exit code [`OVER_HEAP_EXIT`] (the heap bound), or no
//!   result after committing 90% of the container's cap, "needs too much
//!   memory"; a crash, an abort, an `internal` or `protocol` error or
//!   lines that disagree, "Grape's SVG helper stopped unexpectedly." No
//!   detail written here holds a path, a name or file contents.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ::image::{ColorType, DynamicImage};
use grape_core::Format;
use grape_heif_proto::HeifFormat;

use super::protocol::{
    self, Done, FailKind, Failure, ImageInfo, Message, Mode, Request, MAX_LINE_BYTES, PIXELS,
    PROTOCOL,
};
use super::WORKER_FLAG;
use crate::helper::{self, Launch, Polled, Start, StartError, WorkDir};
use crate::image;
use crate::pdf::heap::OVER_HEAP_EXIT;
use crate::publish::Names;
use crate::{CancelToken, ErrorKind, GrapeError, SidecarPaths};

const MIB: u64 = 1024 * 1024;

/// The work folder's name starts with this.
pub(crate) const WORK_PREFIX: &str = "grape-svg-";

/// How often the job checks for a cancel and the deadline.
const POLL: Duration = Duration::from_millis(100);

/// How many of the source's first bytes the rule-3 sniff reads.
const SNIFF_BYTES: u64 = 16;

/// A canvas's longest side and its pixels, for any target.
const MAX_SIDE: u32 = 16_384;
const MAX_PIXELS: u64 = 64 * MIB;

/// A megapixel for the deadline: 2^20 pixels, as [`MAX_PIXELS`] is 64 of
/// them, so the largest canvas adds 64 s to the 60 (D39's 124 s).
const MEGAPIXEL: u64 = MIB;

/// WebP's format limit, a side.
const WEBP_MAX_SIDE: u32 = 16_383;

/// The bounds of an SVG job (D39's limits table).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Limits {
    /// What the worker holds to. Its canvas caps are lowered per target
    /// ([`Limits::request_limits`]).
    pub(crate) worker: protocol::Limits,
    /// The time every job gets.
    pub(crate) timeout_base: Duration,
    /// Added for each started megapixel ([`MEGAPIXEL`]) of the canvas,
    /// once the worker reports it.
    pub(crate) per_megapixel: Duration,
    /// The worker's memory cap, where the app's container can set one (the
    /// Job Object on Windows): it covers the stacks and anything outside
    /// the heap bound.
    pub(crate) worker_memory: u64,
    /// Where the work folder is made: the OS temp folder (tests use their
    /// own).
    pub(crate) work_parent: PathBuf,
}

impl Default for Limits {
    /// [`protocol::Limits::DEFAULT`]; 60 s plus 1 s per megapixel (124 s
    /// at most); 2.5 GiB for the container; the OS temp folder.
    fn default() -> Limits {
        Limits {
            worker: protocol::Limits::DEFAULT,
            timeout_base: Duration::from_secs(60),
            per_megapixel: Duration::from_secs(1),
            worker_memory: 2560 * MIB,
            work_parent: std::env::temp_dir(),
        }
    }
}

impl Limits {
    /// What the request sends for drawing in `mode` for `fit`: the
    /// worker's limits, its canvas no larger than [`caps`].
    pub(crate) fn request_limits(&self, mode: Mode, fit: Format) -> protocol::Limits {
        let (side, pixels) = caps(mode, fit);
        protocol::Limits {
            max_side: self.worker.max_side.min(side),
            max_pixels: self.worker.max_pixels.min(pixels),
            ..self.worker
        }
    }
}

/// The canvas caps for drawing in `mode` for `fit` (D39): a PDF page and
/// JPG, PNG, TIFF and BMP 16,384 a side and 64 MP; WebP 16,383 a side;
/// HEIC and AVIF (`Format::Heic` stands for both) one picture of either
/// format, 16,384 a side and 35,651,584 pixels (D33, D41).
pub(crate) fn caps(mode: Mode, fit: Format) -> (u32, u64) {
    match (mode, fit) {
        (Mode::Page, _) => (MAX_SIDE, MAX_PIXELS),
        (Mode::Raster, Format::Webp) => (WEBP_MAX_SIDE, MAX_PIXELS),
        (Mode::Raster, Format::Heic | Format::Avif) => {
            let (heic, avif) = (
                grape_heif_proto::Limits::for_encode(HeifFormat::Heic),
                grape_heif_proto::Limits::for_encode(HeifFormat::Avif),
            );
            (
                MAX_SIDE.min(heic.max_encode_side).min(avif.max_encode_side),
                MAX_PIXELS
                    .min(heic.max_encode_pixels)
                    .min(avif.max_encode_pixels),
            )
        }
        (Mode::Raster, _) => (MAX_SIDE, MAX_PIXELS),
    }
}

impl Launch {
    /// `<worker> --grape-svg-worker`, contained by the app's hook.
    pub(crate) fn svg_worker(program: &Path) -> Launch {
        Launch {
            program: program.to_path_buf(),
            args: vec![OsString::from(WORKER_FLAG)],
            envs: Vec::new(),
            contain: helper::contain_hook(),
        }
    }
}

/// What [`Renderer::render`] found.
#[derive(Debug)]
pub(crate) enum Drawn {
    /// The SVG, drawn.
    Svg(Rendered),
    /// The file is a raster image (PNG, JPEG, WebP, TIFF or BMP) named
    /// `.svg`: the raster engine reads it (FORMAT_MATRIX rule 3). Nothing
    /// was started.
    Raster,
}

/// An SVG drawn by the worker.
#[derive(Debug)]
pub(crate) struct Rendered {
    /// The canvas: 8-bit RGB when every pixel is opaque, otherwise RGBA
    /// with straight alpha. sRGB, no profile.
    pub(crate) img: DynamicImage,
    /// The SVG's own size in CSS pixels (96 per inch), each finite and
    /// over 0.
    pub(crate) css: (f64, f64),
    /// True when the canvas was made smaller to fit its caps.
    pub(crate) scaled: bool,
    /// How many distinct references were left out.
    pub(crate) left_out: u32,
}

/// Grape's SVG worker and the bounds a job holds it to.
#[derive(Debug)]
pub(crate) struct Renderer {
    launch: Launch,
    limits: Limits,
}

impl Renderer {
    /// The worker in the app's sidecars (Grape's own executable), if it
    /// set one, with Grape's limits.
    pub(crate) fn from_sidecars(sidecars: &SidecarPaths) -> Option<Renderer> {
        let program = sidecars.svg_worker.as_deref()?;
        Some(Renderer::new(
            Launch::svg_worker(program),
            Limits::default(),
        ))
    }

    /// This worker and these limits.
    pub(crate) fn new(launch: Launch, limits: Limits) -> Renderer {
        Renderer { launch, limits }
    }

    /// Draws `src` in `mode`, the canvas held to [`caps`] for `fit` and to
    /// what converting RGBA pixels of its size to `fit` may use
    /// ([`image::pixels_fit`]). The worker's progress `f` goes to
    /// `progress` as `span.0 + (span.1 - span.0) × f` (it may go
    /// backwards; callers pass it on only forwards). `names` name the file
    /// in the messages.
    #[allow(clippy::too_many_arguments)] // The source, the target and the run.
    pub(crate) fn render(
        &self,
        src: &Path,
        names: &Names,
        mode: Mode,
        fit: Format,
        cancel: &CancelToken,
        span: (f32, f32),
        progress: &mut dyn FnMut(f32),
    ) -> Result<Drawn, GrapeError> {
        let (head, len) = read_head(src).map_err(|e| names.read_failed(e))?;
        if is_raster(&head) {
            return Ok(Drawn::Raster);
        }
        let max_source = self.limits.worker.max_source_bytes;
        if len > max_source {
            return Err(names.too_large(format!(
                "the file is {} MiB, over the {} MiB limit for an SVG",
                len / MIB,
                max_source / MIB
            )));
        }
        let source = std::path::absolute(src).map_err(|e| names.read_failed(io_detail(&e)))?;
        if source.to_str().is_none() {
            // The request is JSON: a path that isn't Unicode can't be sent.
            return Err(names.read_failed("the path isn't valid Unicode".to_string()));
        }
        if cancel.is_cancelled() {
            return Err(cancelled());
        }

        let work = make_work_dir(&self.limits)?;
        let request = Request {
            protocol: PROTOCOL,
            source,
            work_dir: work.path().to_path_buf(),
            mode,
            limits: self.limits.request_limits(mode, fit),
        };
        let run = Run {
            launch: &self.launch,
            limits: &self.limits,
            cancel,
        };
        let (done, info) = match drive(&run, &work, &request, fit, span, progress) {
            Ended::Done(done, info) => (done, info),
            other => return Err(ended_error(other, names)),
        };
        if (done.width, done.height, done.scaled) != (info.width, info.height, info.scaled)
            || !matches!(done.channels, 3 | 4)
        {
            return Err(helper_stopped("its done line doesn't match its image line"));
        }
        let announced = helper::Announced {
            width: done.width,
            height: done.height,
            channels: done.channels,
            icc_bytes: 0,
        };
        let (img, _) = helper::read_pixel_file(&work.path().join(PIXELS), &announced, cancel)
            .map_err(|e| match e {
                helper::PixelFileError::Cancelled => cancelled(),
                helper::PixelFileError::Bad(detail) => helper_stopped(&detail),
            })?;
        // The pixels are in memory; the folder's copy goes at once.
        drop(work);
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        Ok(Drawn::Svg(Rendered {
            img,
            css: (f64::from(info.css_width), f64::from(info.css_height)),
            scaled: info.scaled,
            left_out: done.left_out,
        }))
    }
}

/// What every worker run shares.
struct Run<'a> {
    launch: &'a Launch,
    limits: &'a Limits,
    cancel: &'a CancelToken,
}

/// True for PNG, JPEG, WebP, TIFF and BMP content, by the signatures the
/// raster engine's decoder sniffs.
pub(crate) fn is_raster(head: &[u8]) -> bool {
    head.starts_with(b"\x89PNG\r\n\x1a\n")
        || head.starts_with(&[0xff, 0xd8, 0xff])
        || (head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP")
        || head.starts_with(b"MM\0*")
        || head.starts_with(b"II*\0")
        || head.starts_with(b"BM")
}

/// The first [`SNIFF_BYTES`] of `src` and its length, opened read-only;
/// `Err` is the detail.
fn read_head(src: &Path) -> Result<(Vec<u8>, u64), String> {
    let file = File::open(src).map_err(|e| format!("opening the source: {}", io_detail(&e)))?;
    let len = file
        .metadata()
        .map_err(|e| format!("reading the source: {}", io_detail(&e)))?
        .len();
    let mut head = Vec::new();
    file.take(SNIFF_BYTES)
        .read_to_end(&mut head)
        .map_err(|e| format!("reading the source: {}", io_detail(&e)))?;
    Ok((head, len))
}

/// The private work folder, under [`Limits::work_parent`].
fn make_work_dir(limits: &Limits) -> Result<WorkDir, GrapeError> {
    let parent = std::path::absolute(&limits.work_parent)
        .map_err(|e| temp_failed(format!("the temp folder: {}", io_detail(&e))))?;
    if parent.to_str().is_none() {
        // The request is JSON: a path that isn't Unicode can't be sent.
        return Err(temp_failed(
            "the temp folder's path isn't valid Unicode".to_string(),
        ));
    }
    WorkDir::create(&parent, WORK_PREFIX)
        .map_err(|e| temp_failed(format!("making the work folder: {}", io_detail(&e))))
}

/// `per` for each started megapixel ([`MEGAPIXEL`]) of `pixels`.
pub(super) fn per_megapixel(per: Duration, pixels: u64) -> Duration {
    let megapixels = pixels.div_ceil(MEGAPIXEL);
    per.saturating_mul(u32::try_from(megapixels).unwrap_or(u32::MAX))
}

/// How a run of the worker ended.
#[derive(Debug)]
pub(super) enum Ended {
    /// `done`, exit code 0, after an `image` line.
    Done(Done, ImageInfo),
    /// An `error` line and a failing exit code.
    Failed(Failure),
    /// The job was cancelled; the worker was killed.
    Cancelled,
    /// The deadline passed; the worker was killed.
    TimedOut,
    /// Its canvas wouldn't fit the working set; it was killed. The detail.
    TooLarge(String),
    /// It went past its heap bound ([`OVER_HEAP_EXIT`]), or ended without a
    /// result after committing nearly all the memory its container allows.
    /// The detail.
    OverMemory(String),
    /// It ended without a consistent result (a crash, an abort, a lie).
    Crashed(String),
    /// It couldn't be started.
    Spawn(io::ErrorKind),
    /// It couldn't be contained, so it was killed.
    Contain(String),
}

/// Runs the worker on `request` until it ends, is cancelled or runs out of
/// time (the module docs).
fn drive(
    run: &Run<'_>,
    work: &WorkDir,
    request: &Request,
    fit: Format,
    span: (f32, f32),
    progress: &mut dyn FnMut(f32),
) -> Ended {
    let Ok(mut line) = serde_json::to_vec(request) else {
        return Ended::Crashed("the request couldn't be written".to_string());
    };
    line.push(b'\n');
    let options = Start {
        cwd: work.path(),
        env_remove: &[],
        max_memory: Some(run.limits.worker_memory),
        max_line: MAX_LINE_BYTES,
        name: "grape-svg",
    };
    let mut running = match helper::start::<Message>(run.launch, options, line) {
        Ok(running) => running,
        Err(StartError::Spawn(kind)) => return Ended::Spawn(kind),
        Err(StartError::Contain(why)) => return Ended::Contain(why),
        Err(StartError::Plumbing(why)) => return Ended::Crashed(why.to_string()),
    };
    let initial = run.limits.timeout_base;
    let start = Instant::now();
    let mut deadline = start.checked_add(initial);
    let mut image: Option<ImageInfo> = None;
    let mut done = None;
    let mut failure = None;
    let status = loop {
        if run.cancel.is_cancelled() {
            return Ended::Cancelled;
        }
        let now = Instant::now();
        let wait = match deadline {
            Some(deadline) if now >= deadline => return Ended::TimedOut,
            Some(deadline) => (deadline - now).min(POLL),
            None => POLL,
        };
        let message = match running.poll(wait) {
            Ok(Polled::Message(message)) => message,
            Ok(Polled::Idle) => continue,
            Ok(Polled::Exited(status)) => break status,
            Err(e) => return Ended::Crashed(format!("waiting for it: {}", io_detail(&e))),
        };
        let ended = done.is_some() || failure.is_some();
        match message {
            Message::Image(info) => {
                if image.is_some() || ended {
                    return Ended::Crashed("an image line out of place".to_string());
                }
                if let Err(refused) = check_image(&info, &request.limits, fit) {
                    return refused;
                }
                let pixels = u64::from(info.width) * u64::from(info.height);
                let extended = initial
                    .checked_add(per_megapixel(run.limits.per_megapixel, pixels))
                    .and_then(|total| start.checked_add(total));
                if let (Some(current), Some(extended)) = (deadline, extended) {
                    deadline = Some(current.max(extended));
                }
                image = Some(info);
            }
            Message::Progress(fraction) => {
                if fraction.is_finite() && !ended {
                    progress(span.0 + (span.1 - span.0) * fraction.clamp(0.0, 1.0));
                }
            }
            Message::Done(result) if !ended => done = Some(result),
            Message::Error(result) if !ended => failure = Some(result),
            Message::Done(_) | Message::Error(_) => {
                return Ended::Crashed("a second last line".to_string())
            }
        }
    };
    if status.code() == Some(OVER_HEAP_EXIT) {
        return Ended::OverMemory(format!(
            "the SVG worker went past its {} MiB heap bound",
            request.limits.max_heap_bytes / MIB
        ));
    }
    match (done, failure, status.success(), image) {
        (Some(done), None, true, Some(image)) => Ended::Done(done, image),
        (Some(_), None, true, None) => Ended::Crashed("done without an image line".to_string()),
        (None, Some(failure), false, _) => Ended::Failed(failure),
        (done, failure, success, _) => {
            let limit = run.limits.worker_memory;
            if helper::near_memory_cap(running.peak_memory(), limit) {
                Ended::OverMemory(format!(
                    "the SVG worker ended near its {} MiB memory cap",
                    limit / MIB
                ))
            } else {
                Ended::Crashed(format!(
                    "exit {:?} (success {success}) with done {} and error {}",
                    status.code(),
                    done.is_some(),
                    failure.is_some()
                ))
            }
        }
    }
}

/// The parent's check of an `image` line: a canvas within the caps it sent
/// (otherwise the worker broke its word), whose RGBA pixels converting to
/// `fit` may hold ([`image::pixels_fit`]), and a real CSS size.
pub(super) fn check_image(
    info: &ImageInfo,
    sent: &protocol::Limits,
    fit: Format,
) -> Result<(), Ended> {
    let pixels = u64::from(info.width) * u64::from(info.height);
    let css_ok = |v: f32| v.is_finite() && v > 0.0;
    if pixels == 0
        || info.width > sent.max_side
        || info.height > sent.max_side
        || pixels > sent.max_pixels
        || !css_ok(info.css_width)
        || !css_ok(info.css_height)
    {
        return Err(Ended::Crashed(format!(
            "its image line says {}x{} for {}x{} CSS px, past what it was sent",
            info.width, info.height, info.css_width, info.css_height
        )));
    }
    image::pixels_fit(info.width, info.height, ColorType::Rgba8, fit, 0).map_err(Ended::TooLarge)
}

/// A run that didn't end with `done`, in Grape's words.
fn ended_error(ended: Ended, names: &Names) -> GrapeError {
    let source = &names.source;
    match ended {
        Ended::Failed(failure) => report(failure, names),
        Ended::Cancelled => cancelled(),
        Ended::TimedOut => GrapeError::new(
            ErrorKind::DecodeFailed,
            format!("{source} took too long, so Grape stopped."),
            "the SVG worker ran past its time",
        ),
        Ended::TooLarge(detail) => names.too_large(detail),
        Ended::OverMemory(detail) => GrapeError::new(
            ErrorKind::DecodeFailed,
            format!("{source} needs too much memory, so Grape stopped."),
            detail,
        ),
        Ended::Crashed(detail) => helper_stopped(&detail),
        Ended::Spawn(kind) => couldnt_start(format!("starting it: {kind:?}")),
        Ended::Contain(why) => couldnt_start(format!("containing it: {why}")),
        // `drive`'s caller handles `Done` first.
        Ended::Done(..) => helper_stopped("it ended in an unexpected way"),
    }
}

/// The worker's failure in Grape's words, naming the file.
fn report(failure: Failure, names: &Names) -> GrapeError {
    let detail = format!("SVG worker: {:?}: {}", failure.kind, failure.detail);
    match failure.kind {
        FailKind::Read => names.read_failed(detail),
        FailKind::NotSvg => names.decode_failed(detail),
        FailKind::TooLarge => names.too_large(detail),
        FailKind::Write => temp_failed(detail),
        FailKind::Protocol | FailKind::Internal => helper_stopped(&detail),
    }
}

/// The work folder or a file in it couldn't be made or written.
fn temp_failed(detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::WriteFailed,
        "Grape couldn't write its temp files.",
        format!("SVG work folder: {detail}"),
    )
}

/// The worker couldn't be started or contained.
fn couldnt_start(detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::NotYetSupported,
        "Grape's SVG helper couldn't start.",
        format!("SVG worker: {detail}"),
    )
}

/// The worker ended without a consistent result.
fn helper_stopped(detail: &str) -> GrapeError {
    GrapeError::new(
        ErrorKind::DecodeFailed,
        "Grape's SVG helper stopped unexpectedly.",
        format!("SVG worker: {detail}"),
    )
}

/// No worker in the sidecars (only a direct call gets here: `run_job`
/// sends an SVG job to this engine only with one).
pub(crate) fn missing() -> GrapeError {
    GrapeError::new(
        ErrorKind::NotYetSupported,
        "Grape's SVG helper is missing.",
        "SidecarPaths::svg_worker is None",
    )
}

/// The error for a cancelled job (its detail names no file).
pub(crate) fn cancelled() -> GrapeError {
    GrapeError::new(
        ErrorKind::Cancelled,
        "Cancelled. Nothing was saved.",
        "cancelled",
    )
}

/// An `io` error without its message, which can hold a path.
fn io_detail(e: &io::Error) -> String {
    format!("{:?}", e.kind())
}
