//! HEIC ⇄ JPG, PNG, WebP, TIFF and BMP through Grape's HEIC helper, a
//! separate process (DECISIONS D33). Lane A's module, by agreement with
//! lane B, who owns this crate.
//!
//! The helper is `grape-heif.exe` (libheif) on Windows and
//! `Grape --grape-heif-worker` (ImageIO) on macOS. Both speak
//! `grape-heif-proto`, so this side has no `cfg`: it starts
//! `<SidecarPaths::heif> --grape-heif-worker` either way.
//!
//! - **HEIC → raster:** the source's `ftyp` is sniffed before anything
//!   starts. A file that isn't HEIF goes to the raster engine
//!   ([`image::convert`], FORMAT_MATRIX rule 3); HEIF with another codec is
//!   refused. For HEIC, the helper decodes the primary image, upright and
//!   8-bit, into `pixels.rgba` in a private work folder under the OS temp
//!   folder. Its `image` line comes before any pixel is decoded, so a
//!   picture whose conversion wouldn't fit the working set
//!   ([`image::pixels_fit`]) is stopped there. The pixels are read once,
//!   into the one buffer `pixels_fit` counted, and published by
//!   [`image::publish_pixels`] (no EXIF, so nothing is rotated twice).
//! - **Raster → HEIC:** the raster engine decodes the source
//!   ([`image::decode`], EXIF orientation applied), its size is checked
//!   against one HEVC picture's ([`Limits`]), and its 8-bit RGB or RGBA rows
//!   are streamed into `input.rgba`. The helper writes `out.heic`, which is
//!   checked and copied into a temp file in the destination folder, then
//!   published without replacing anything.
//! - **The helper** ([`crate::helper`]) runs with `LIBHEIF_PLUGIN_PATH` and
//!   `LIBHEIF_SECURITY_LIMITS` removed from its environment, in its work
//!   folder, contained by the app's hook (a Job Object on Windows) before
//!   it gets its request. The job polls every 100 ms: a cancel or the
//!   deadline ([`Limits`]) kills it. Its progress is passed on, never
//!   backwards.
//! - **Messages** name the file in Grape's words (heic-spec §9). No detail
//!   written here holds a path, a name or file contents; the helper's own
//!   details never do either.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ::image::{ColorType, DynamicImage, RgbImage, RgbaImage};
use grape_core::{Format, Job};
use grape_heif_proto::{
    self as proto, FailKind, Failure, ImageInfo, Message, Op, PixelHeader, PixelLimits,
    PixelWriter, Request, Sniff,
};

use crate::cancel::Cancellable;
use crate::helper::{self, Launch, Polled, Start, StartError, WorkDir};
use crate::image;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// The work folder's name starts with this (J4).
pub(crate) const WORK_PREFIX: &str = "grape-heif-";

/// How often the job checks for a cancel and the deadline.
const POLL: Duration = Duration::from_millis(100);

/// The only variables libheif reads. Removed from the helper's
/// environment: an inherited `LIBHEIF_PLUGIN_PATH` would point a build with
/// plugin loading at a folder of DLLs, and `LIBHEIF_SECURITY_LIMITS=off`
/// would switch libheif's limits off (heic-spec J38).
const LIBHEIF_ENV: [&str; 2] = ["LIBHEIF_PLUGIN_PATH", "LIBHEIF_SECURITY_LIMITS"];

/// The buffer of the writers that stream pixels and copy `out.heic`.
const WRITE_BUFFER: usize = 1 << 20;

/// A helper that ended without a result after committing at least this
/// share of [`Limits::helper_memory`] (in percent) ran out of memory.
const OVER_MEMORY_PERCENT: u64 = 90;

/// True for the 10 pairs: HEIC to or from JPG, PNG, WebP, TIFF or BMP.
pub(crate) fn supports(from: Format, to: Format) -> bool {
    (from == Format::Heic && image::RASTER.contains(&to))
        || (to == Format::Heic && image::RASTER.contains(&from))
}

/// The bounds of a HEIC job (heic-spec §8).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Limits {
    /// What the helper holds to; sent with every request.
    pub(crate) helper: proto::Limits,
    /// The time every job gets.
    pub(crate) timeout_base: Duration,
    /// Added for each started megapixel of a decoded image, once the
    /// helper reports its size.
    pub(crate) decode_per_megapixel: Duration,
    /// Added for each started megapixel of an image to encode.
    pub(crate) encode_per_megapixel: Duration,
    /// The helper's memory cap, where the app's container can set one
    /// (the Job Object on Windows).
    pub(crate) helper_memory: u64,
    /// Where the work folder is made: the OS temp folder (tests use their
    /// own).
    pub(crate) work_parent: PathBuf,
}

impl Default for Limits {
    /// The helper's [`proto::Limits::DEFAULT`]; 120 s plus 1 s per
    /// megapixel to decode and 3 s to encode; 2 GiB for the helper; the OS
    /// temp folder.
    fn default() -> Limits {
        Limits {
            helper: proto::Limits::DEFAULT,
            timeout_base: Duration::from_secs(120),
            decode_per_megapixel: Duration::from_secs(1),
            encode_per_megapixel: Duration::from_secs(3),
            helper_memory: 2 * GIB,
            work_parent: std::env::temp_dir(),
        }
    }
}

impl Launch {
    /// `<helper> --grape-heif-worker`, contained by the app's hook.
    fn heif_helper(program: &Path) -> Launch {
        Launch {
            program: program.to_path_buf(),
            args: vec![OsString::from(proto::WORKER_FLAG)],
            envs: Vec::new(),
            contain: helper::contain_hook(),
        }
    }
}

/// Converts `src` (of format `from`, by its name) to `to` at `dest`, the
/// path the plan reserved, with the helper in `ctx.sidecars.heif`.
pub(crate) fn convert(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    // `run_job` sends a job here only with a helper; this is for direct
    // calls.
    let Some(program) = ctx.sidecars.heif.as_deref() else {
        return Err(GrapeError::new(
            ErrorKind::NotYetSupported,
            "Grape's HEIC helper is missing.",
            "SidecarPaths::heif is None",
        ));
    };
    let launch = Launch::heif_helper(program);
    convert_with(
        job,
        src,
        dest,
        from,
        to,
        ctx,
        progress,
        &launch,
        &Limits::default(),
    )
}

/// [`convert`] with the helper and limits given.
#[allow(clippy::too_many_arguments)] // `convert`'s, plus the helper and limits.
pub(crate) fn convert_with(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    launch: &Launch,
    limits: &Limits,
) -> Result<JobOutcome, GrapeError> {
    let run = Run {
        launch,
        limits,
        cancel: &ctx.cancel,
    };
    if from == Format::Heic && image::RASTER.contains(&to) {
        from_heic(job, src, dest, to, ctx, progress, &run)
    } else if to == Format::Heic && image::RASTER.contains(&from) {
        to_heic(job, src, dest, from, ctx, progress, &run)
    } else {
        Err(crate::not_yet_supported(job, Some(from)))
    }
}

/// What every helper run in a job shares.
struct Run<'a> {
    launch: &'a Launch,
    limits: &'a Limits,
    cancel: &'a CancelToken,
}

/// HEIC → JPG, PNG, WebP, TIFF or BMP (heic-spec §4.1).
fn from_heic(
    job: &Job,
    src: &Path,
    dest: &Path,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    run: &Run<'_>,
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, Format::Heic, to);
    publish::dest_dir(dest, &names)?;
    let cancel = run.cancel;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }

    let (head, len) = read_head(src).map_err(|e| names.read_failed(e))?;
    match proto::sniff(&head) {
        // FORMAT_MATRIX rule 3: the content decides. The raster engine
        // keeps its own limits, and its messages name the file as HEIC.
        Sniff::NotHeif => return image::convert(job, src, dest, Format::Heic, to, ctx, progress),
        Sniff::OtherHeif => return Err(other_heif(&names, "its ftyp names another codec")),
        Sniff::Heic => {}
    }
    let max_source = run.limits.helper.max_source_bytes;
    if len > max_source {
        return Err(names.too_large(format!(
            "the file is {} MiB, over the {} MiB limit for a HEIC source",
            len / MIB,
            max_source / MIB
        )));
    }
    let source = std::path::absolute(src).map_err(|e| names.read_failed(io_detail(&e)))?;
    if source.to_str().is_none() {
        // The request is JSON: a path that isn't Unicode can't be sent.
        return Err(names.read_failed("the path isn't valid Unicode".to_string()));
    }

    let work = make_work_dir(run.limits)?;
    let mut last = 0.0f32;
    let ended = {
        let mut forward = |fraction: f32| {
            if fraction > last {
                last = fraction;
                progress(fraction);
            }
        };
        let limits = run.limits;
        let mut on_image = |info: &ImageInfo| -> Result<Duration, Ended> {
            let pixels = u64::from(info.width) * u64::from(info.height);
            // The helper drops a profile over the limit; one it reports
            // anyway is a lie.
            if pixels == 0
                || !matches!(info.channels, 3 | 4)
                || info.icc_bytes > limits.helper.max_icc_bytes
            {
                return Err(Ended::Crashed(format!(
                    "its image line says {}x{} with {} channels and {} ICC bytes",
                    info.width, info.height, info.channels, info.icc_bytes
                )));
            }
            if pixels > limits.helper.max_pixels {
                return Err(Ended::TooLarge(format!(
                    "{}x{} is over {} pixels",
                    info.width, info.height, limits.helper.max_pixels
                )));
            }
            image::pixels_fit(
                info.width,
                info.height,
                color_of(info.channels),
                to,
                u64::from(info.icc_bytes),
            )
            .map_err(Ended::TooLarge)?;
            Ok(per_megapixel(limits.decode_per_megapixel, pixels))
        };
        drive(
            run,
            &work,
            Op::Decode { source },
            run.limits.timeout_base,
            &mut on_image,
            (0.1, 0.5),
            &mut forward,
        )
    };
    let (done, info) = match ended {
        Ended::Done(done, Some(info)) => (done, info),
        Ended::Done(_, None) => return Err(helper_stopped("done without an image line")),
        other => return Err(ended_error(other, &names, Side::Decode, run.limits)),
    };
    if (done.width, done.height, done.channels, done.icc_bytes)
        != (info.width, info.height, info.channels, info.icc_bytes)
    {
        return Err(helper_stopped("its done line doesn't match its image line"));
    }
    let (img, icc) = read_decoded(&work, &info, cancel)?;
    // The pixels are in memory; the folder's copy goes at once.
    drop(work);
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    if last < 0.5 {
        progress(0.5);
    }
    image::publish_pixels(job, src, dest, Format::Heic, to, img, icc, ctx, progress)
}

/// JPG, PNG, WebP, TIFF or BMP → HEIC (heic-spec §4.2).
fn to_heic(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    run: &Run<'_>,
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, Format::Heic);
    let dir = publish::dest_dir(dest, &names)?;
    let cancel = run.cancel;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }

    let (img, icc) = image::decode(src, from, Format::Heic, &names, cancel)
        .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    progress(0.3);
    let (width, height) = (img.width(), img.height());
    let pixels = u64::from(width) * u64::from(height);
    let helper_limits = &run.limits.helper;
    if pixels > helper_limits.max_encode_pixels
        || width > helper_limits.max_encode_side
        || height > helper_limits.max_encode_side
    {
        return Err(too_large_to_save(
            &names,
            format!(
                "{width}x{height} is over one HEVC picture ({} pixels, {} a side)",
                helper_limits.max_encode_pixels, helper_limits.max_encode_side
            ),
        ));
    }
    let channels = if img.color().has_alpha() { 4 } else { 3 };
    let img = match image::prepare(img, Format::Heic, cancel) {
        Some(img) if !cancel.is_cancelled() => img,
        _ => return Err(cancelled()),
    };
    let max_icc = usize::try_from(helper_limits.max_icc_bytes).unwrap_or(usize::MAX);
    let icc = icc.filter(|profile| image::icc_fits(profile, &img) && profile.len() <= max_icc);
    let Some(rgba) = img.as_rgba8() else {
        return Err(names.encode_failed(format!(
            "the pixels for HEIC are {:?}, not RGBA8",
            img.color()
        )));
    };

    let work = make_work_dir(run.limits)?;
    write_input(&work, rgba, channels, icc.as_deref(), cancel).map_err(|e| {
        if cancel.is_cancelled() {
            cancelled()
        } else {
            temp_failed(format!("writing the pixels: {}", io_detail(&e)))
        }
    })?;
    drop(img);
    progress(0.4);

    let mut last = 0.4f32;
    let ended = {
        let mut forward = |fraction: f32| {
            if fraction > last {
                last = fraction;
                progress(fraction);
            }
        };
        let mut on_image = |_: &ImageInfo| -> Result<Duration, Ended> {
            Err(Ended::Crashed("an image line while encoding".to_string()))
        };
        let initial = run
            .limits
            .timeout_base
            .saturating_add(per_megapixel(run.limits.encode_per_megapixel, pixels));
        drive(
            run,
            &work,
            Op::Encode { quality: None },
            initial,
            &mut on_image,
            (0.4, 0.9),
            &mut forward,
        )
    };
    let done = match ended {
        Ended::Done(done, _) => done,
        other => return Err(ended_error(other, &names, Side::Encode, run.limits)),
    };
    let out = work.path().join(proto::names::OUTPUT);
    check_output(&out, done.bytes)?;

    publish::ensure_dir(job, ctx, dir, &names)?;
    // From here on, `temp` removes the temp file on every return path.
    let (temp, file) = TempFile::create(dir, Format::Heic.output_ext())
        .map_err(|e| names.write_failed(format!("creating the temp file: {}", io_detail(&e))))?;
    copy_output(&out, file, cancel).map_err(|e| {
        if cancel.is_cancelled() {
            cancelled()
        } else {
            names.write_failed(format!("writing the HEIC file: {}", io_detail(&e)))
        }
    })?;
    drop(work);
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    drop(temp);
    progress(1.0);
    Ok(outcome)
}

/// The first [`proto::SNIFF_BYTES`] of `src` and its length, opened
/// read-only; `Err` is the detail.
fn read_head(src: &Path) -> Result<(Vec<u8>, u64), String> {
    let file = File::open(src).map_err(|e| format!("opening the source: {}", io_detail(&e)))?;
    let len = file
        .metadata()
        .map_err(|e| format!("reading the source: {}", io_detail(&e)))?
        .len();
    let mut head = Vec::with_capacity(proto::SNIFF_BYTES);
    file.take(proto::SNIFF_BYTES as u64)
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

/// Streams `rgba`'s rows into `input.rgba`, as 3 channels (the alpha byte
/// dropped) or 4, then syncs it. Writing fails once `cancel` is set.
fn write_input(
    work: &WorkDir,
    rgba: &RgbaImage,
    channels: u8,
    icc: Option<&[u8]>,
    cancel: &CancelToken,
) -> io::Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(work.path().join(proto::names::INPUT))?;
    let out = BufWriter::with_capacity(WRITE_BUFFER, Cancellable::new(file, cancel));
    let icc_len = icc
        .map_or(Ok(0), |icc| u32::try_from(icc.len()))
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let header = PixelHeader {
        width: rgba.width(),
        height: rgba.height(),
        channels,
        icc_len,
    };
    let mut writer = PixelWriter::begin(out, &header, icc)?;
    let row_len = usize::try_from(rgba.width())
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut rgb_row = Vec::new();
    for row in rgba.as_raw().chunks_exact(row_len) {
        if channels == 4 {
            writer.row(row)?;
        } else {
            rgb_row.clear();
            let (pixels, _) = row.as_chunks::<4>();
            for pixel in pixels {
                rgb_row.extend_from_slice(&pixel[..3]);
            }
            writer.row(&rgb_row)?;
        }
    }
    let out = writer.finish()?;
    let file = out.into_inner().map_err(|e| e.into_error())?.into_inner();
    file.sync_all()
}

/// Reads `pixels.rgba`, which must be exactly what the `image` line
/// announced (and [`image::pixels_fit`] approved), into one buffer.
fn read_decoded(
    work: &WorkDir,
    info: &ImageInfo,
    cancel: &CancelToken,
) -> Result<(DynamicImage, Option<Vec<u8>>), GrapeError> {
    let path = work.path().join(proto::names::PIXELS);
    let file = File::open(&path)
        .map_err(|e| helper_stopped(&format!("its pixel file: {}", io_detail(&e))))?;
    let len = file
        .metadata()
        .map_err(|e| helper_stopped(&format!("its pixel file: {}", io_detail(&e))))?
        .len();
    // Nothing larger than what was approved is ever allocated.
    let limits = PixelLimits {
        max_pixels: u64::from(info.width) * u64::from(info.height),
        max_side: info.width.max(info.height),
        max_icc_bytes: info.icc_bytes,
    };
    let pixels = proto::read_pixels(Cancellable::new(file, cancel), len, &limits).map_err(|e| {
        if cancel.is_cancelled() {
            cancelled()
        } else {
            helper_stopped(&format!("its pixel file: {e}"))
        }
    })?;
    let icc_len = pixels.icc.as_ref().map_or(0, Vec::len);
    if (pixels.width, pixels.height, pixels.channels) != (info.width, info.height, info.channels)
        || u32::try_from(icc_len).ok() != Some(info.icc_bytes)
    {
        return Err(helper_stopped(
            "its pixel file doesn't match its image line",
        ));
    }
    let (width, height) = (pixels.width, pixels.height);
    // `from_raw` takes the buffer as it is: one copy, as pixels_fit counted.
    let img = match pixels.channels {
        4 => RgbaImage::from_raw(width, height, pixels.data).map(DynamicImage::ImageRgba8),
        _ => RgbImage::from_raw(width, height, pixels.data).map(DynamicImage::ImageRgb8),
    };
    let img = img.ok_or_else(|| helper_stopped("its pixels don't fill the image"))?;
    Ok((img, pixels.icc))
}

/// `out.heic` must be the `bytes` the helper reported, and HEIC.
fn check_output(out: &Path, bytes: u64) -> Result<(), GrapeError> {
    let (head, len) = read_head(out).map_err(|e| helper_stopped(&format!("its output: {e}")))?;
    if bytes == 0 || len != bytes {
        return Err(helper_stopped(&format!(
            "its output is {len} bytes, and it said {bytes}"
        )));
    }
    if proto::sniff(&head) != Sniff::Heic {
        return Err(helper_stopped("its output isn't HEIC"));
    }
    Ok(())
}

/// Copies `out.heic` into the temp `file` in the destination folder, then
/// syncs it. Reading and writing fail once `cancel` is set.
fn copy_output(out: &Path, file: File, cancel: &CancelToken) -> io::Result<()> {
    let mut reader = Cancellable::new(File::open(out)?, cancel);
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER, Cancellable::new(file, cancel));
    io::copy(&mut reader, &mut writer)?;
    let file = writer
        .into_inner()
        .map_err(|e| e.into_error())?
        .into_inner();
    file.sync_all()
}

/// The pixel layout of `channels` 8-bit samples.
fn color_of(channels: u8) -> ColorType {
    if channels == 4 {
        ColorType::Rgba8
    } else {
        ColorType::Rgb8
    }
}

/// `per` for each started megapixel of `pixels`.
fn per_megapixel(per: Duration, pixels: u64) -> Duration {
    let megapixels = pixels.div_ceil(1_000_000);
    per.saturating_mul(u32::try_from(megapixels).unwrap_or(u32::MAX))
}

/// How a run of the helper ended.
#[derive(Debug)]
enum Ended {
    /// `done`, exit code 0, and the `image` line if there was one.
    Done(proto::Done, Option<ImageInfo>),
    /// An `error` line and a failing exit code.
    Failed(Failure),
    /// The job was cancelled; the helper was killed.
    Cancelled,
    /// The deadline passed; the helper was killed.
    TimedOut,
    /// Its `image` line was over a limit; it was killed. The detail.
    TooLarge(String),
    /// It ended without a result after committing nearly all the memory
    /// its container allows.
    OverMemory,
    /// It ended without a consistent result (a crash, an abort, a lie).
    Crashed(String),
    /// It couldn't be started.
    Spawn(io::ErrorKind),
    /// It couldn't be contained, so it was killed (J6).
    Contain(String),
}

/// Runs the helper on `op` until it ends, is cancelled or runs out of time.
///
/// - The deadline is `initial` from the start; an `image` line that
///   `on_image` accepts extends it by what `on_image` returns. One it
///   refuses ends the run with what it returns.
/// - Its progress `f` is passed on as `span.0 + (span.1 - span.0) × f`, and
///   an accepted `image` line as `span.0`.
fn drive(
    run: &Run<'_>,
    work: &WorkDir,
    op: Op,
    initial: Duration,
    on_image: &mut dyn FnMut(&ImageInfo) -> Result<Duration, Ended>,
    span: (f32, f32),
    progress: &mut dyn FnMut(f32),
) -> Ended {
    let request = Request {
        protocol: proto::PROTOCOL,
        work_dir: work.path().to_path_buf(),
        op,
        limits: run.limits.helper,
    };
    let Ok(mut line) = serde_json::to_vec(&request) else {
        return Ended::Crashed("the request couldn't be written".to_string());
    };
    line.push(b'\n');
    let options = Start {
        cwd: work.path(),
        env_remove: &LIBHEIF_ENV,
        max_memory: Some(run.limits.helper_memory),
        max_line: proto::MAX_LINE_BYTES,
        name: "grape-heif",
    };
    let mut running = match helper::start::<Message>(run.launch, options, line) {
        Ok(running) => running,
        Err(StartError::Spawn(kind)) => return Ended::Spawn(kind),
        Err(StartError::Contain(why)) => return Ended::Contain(why),
        Err(StartError::Plumbing(why)) => return Ended::Crashed(why.to_string()),
    };
    let start = Instant::now();
    let mut deadline = start.checked_add(initial);
    let mut image = None;
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
                match on_image(&info) {
                    Ok(more) => {
                        let extended = initial
                            .checked_add(more)
                            .and_then(|total| start.checked_add(total));
                        if let (Some(current), Some(extended)) = (deadline, extended) {
                            deadline = Some(current.max(extended));
                        }
                    }
                    Err(refused) => return refused,
                }
                image = Some(info);
                progress(span.0);
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
    match (done, failure, status.success()) {
        (Some(done), None, true) => Ended::Done(done, image),
        (None, Some(failure), false) => Ended::Failed(failure),
        (done, failure, success) => {
            let peak = running.peak_memory();
            let limit = run.limits.helper_memory;
            let over = peak.is_some_and(|peak| {
                u128::from(peak) * 100 >= u128::from(limit) * u128::from(OVER_MEMORY_PERCENT)
            });
            if over {
                Ended::OverMemory
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

/// Which way a job converts, for the messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// HEIC → raster: the helper decodes.
    Decode,
    /// Raster → HEIC: the helper encodes.
    Encode,
}

/// A run that didn't end with `done`, in Grape's words.
fn ended_error(ended: Ended, names: &Names, side: Side, limits: &Limits) -> GrapeError {
    let source = &names.source;
    match ended {
        Ended::Failed(failure) => report(failure, names, side),
        Ended::Cancelled => cancelled(),
        Ended::TimedOut => GrapeError::new(
            ErrorKind::DecodeFailed,
            format!("{source} took too long, so Grape stopped."),
            "the HEIC helper ran past its time",
        ),
        Ended::TooLarge(detail) => names.too_large(detail),
        Ended::OverMemory => GrapeError::new(
            ErrorKind::DecodeFailed,
            format!("{source} needs too much memory, so Grape stopped."),
            format!(
                "the HEIC helper ended near its {} MiB memory cap",
                limits.helper_memory / MIB
            ),
        ),
        Ended::Crashed(detail) => helper_stopped(&detail),
        Ended::Spawn(kind) => couldnt_start(format!("starting it: {kind:?}")),
        Ended::Contain(why) => couldnt_start(format!("containing it: {why}")),
        // `drive` returns `Done` to its caller, which handles it first.
        Ended::Done(..) => helper_stopped("it ended in an unexpected way"),
    }
}

/// The helper's failure in Grape's words, naming the file.
fn report(failure: Failure, names: &Names, side: Side) -> GrapeError {
    let source = &names.source;
    let detail = format!("HEIC helper: {:?}: {}", failure.kind, failure.detail);
    let decode_failed =
        |message: String| GrapeError::new(ErrorKind::DecodeFailed, message, detail.clone());
    match (failure.kind, side) {
        (FailKind::Protocol, _) => GrapeError::new(
            ErrorKind::NotYetSupported,
            "Grape's HEIC helper doesn't match this version of Grape.",
            detail,
        ),
        (FailKind::Read, _) => names.read_failed(detail),
        (FailKind::Write, _) => temp_failed(detail),
        (FailKind::NotHeif | FailKind::Damaged, Side::Decode) => names.decode_failed(detail),
        (FailKind::Unsupported, Side::Decode) => decode_failed(format!(
            "{source} is a kind of HEIF image Grape can't read yet."
        )),
        (FailKind::NoImage, Side::Decode) => {
            decode_failed(format!("{source} has no image to convert."))
        }
        (FailKind::TooLarge, Side::Decode) => names.too_large(detail),
        (FailKind::TooLarge, Side::Encode) => too_large_to_save(names, detail),
        (FailKind::NoEncoder, Side::Encode) => GrapeError::new(
            ErrorKind::NotYetSupported,
            "This Mac can't write HEIC files.",
            detail,
        ),
        (FailKind::Encode, Side::Encode) => names.encode_failed(detail),
        // `internal`, and a kind that makes no sense for this side.
        _ => helper_stopped(&detail),
    }
}

/// "… is a kind of HEIF image Grape can't read yet."
fn other_heif(names: &Names, detail: &str) -> GrapeError {
    GrapeError::new(
        ErrorKind::DecodeFailed,
        format!(
            "{} is a kind of HEIF image Grape can't read yet.",
            names.source
        ),
        format!("HEIC: {detail}"),
    )
}

/// The image is larger than one HEVC picture Grape encodes.
fn too_large_to_save(names: &Names, detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::EncodeFailed,
        format!("{} is too large to save as HEIC.", names.source),
        detail,
    )
}

/// The work folder or a file in it couldn't be made or written.
fn temp_failed(detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::WriteFailed,
        "Grape couldn't write its temp files.",
        format!("HEIC work folder: {detail}"),
    )
}

/// The helper couldn't be started or contained.
fn couldnt_start(detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::NotYetSupported,
        "Grape's HEIC helper couldn't start.",
        format!("HEIC helper: {detail}"),
    )
}

/// The helper ended without a consistent result.
fn helper_stopped(detail: &str) -> GrapeError {
    GrapeError::new(
        ErrorKind::DecodeFailed,
        "Grape's HEIC helper stopped unexpectedly.",
        format!("HEIC helper: {detail}"),
    )
}

/// The error for a cancelled job (its detail names no file).
fn cancelled() -> GrapeError {
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

#[cfg(test)]
mod tests {
    //! The engine with the test binary itself as a fake helper: it runs
    //! only [`fake_helper`] (`--exact … --ignored`) with [`FAKE`] set to a
    //! mode, and serves the request with `grape_heif_proto::serve` and
    //! [`FakeCodec`], whose "HEIC" is an `ftyp` box followed by raw pixels.
    //! Every test checks that the source is unchanged, that nothing is left
    //! in the destination folder, and that no work folder is left.

    #![allow(clippy::unwrap_used)]

    use std::collections::hash_map::DefaultHasher;
    use std::fs;
    use std::hash::Hasher;
    use std::io::{Cursor, Write};
    use std::process::Child;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use ::image::codecs::png::PngEncoder;
    use ::image::{ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};
    use grape_core::{plan, JobOutput, Operation, Settings};
    use grape_heif_proto::{Caps, Codec, PixelSink, Pixels};

    use super::*;
    use crate::helper::{Contain, Contained};
    use crate::output::unique_suffix;
    use crate::SidecarPaths;

    /// The variable that turns [`fake_helper`] into a fake helper, and
    /// names its mode.
    const FAKE: &str = "GRAPE_HEIF_FAKE";

    /// The variable the `env` mode needs to see, to show that variables do
    /// reach the helper unless they're removed.
    const CONTROL: &str = "GRAPE_HEIF_FAKE_CONTROL";

    /// The file [`FakeContain`] writes into the work folder.
    const MARKER: &str = "contained";

    /// An `ftyp` box: major brand `heic`, compatible `mif1 heic`.
    const FTYP: [u8; 24] = *b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";

    /// 32 bytes: an `ftyp` with major `avif`, compatible `mif1 avif`, then
    /// zeros (heic-spec §5.6's `avif-brand.heic`).
    const AVIF_BRAND: [u8; 32] = *b"\0\0\0\x18ftypavif\0\0\0\0mif1avif\0\0\0\0\0\0\0\0";

    /// A fake HEIC file: [`FTYP`], `FAKE`, the width and height (u32 LE),
    /// the channels, the ICC profile's length (u32 LE), the profile, the
    /// rows.
    fn fake_heic(
        width: u32,
        height: u32,
        channels: u8,
        rows: &[u8],
        icc: Option<&[u8]>,
    ) -> Vec<u8> {
        let icc = icc.unwrap_or_default();
        let mut bytes = FTYP.to_vec();
        bytes.extend_from_slice(b"FAKE");
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.push(channels);
        bytes.extend_from_slice(&u32::try_from(icc.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(icc);
        bytes.extend_from_slice(rows);
        bytes
    }

    /// What a fake HEIC file holds.
    #[derive(Debug)]
    struct FakeImage {
        width: u32,
        height: u32,
        channels: u8,
        icc: Option<Vec<u8>>,
        rows: Vec<u8>,
    }

    fn parse_fake(bytes: &[u8]) -> Option<FakeImage> {
        let rest = bytes.strip_prefix(&FTYP)?.strip_prefix(b"FAKE")?;
        let u32_at = |at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(rest.get(at..at + 4)?.try_into().ok()?))
        };
        let (width, height) = (u32_at(0)?, u32_at(4)?);
        let channels = *rest.get(8)?;
        let icc_len = usize::try_from(u32_at(9)?).ok()?;
        let icc = rest.get(13..13 + icc_len)?;
        let rows = rest.get(13 + icc_len..)?;
        let expected =
            usize::try_from(width).ok()? * usize::try_from(height).ok()? * usize::from(channels);
        (rows.len() == expected).then(|| FakeImage {
            width,
            height,
            channels,
            icc: (!icc.is_empty()).then(|| icc.to_vec()),
            rows: rows.to_vec(),
        })
    }

    /// The fake helper's codec. Its mode picks how it misbehaves.
    struct FakeCodec {
        mode: String,
    }

    impl FakeCodec {
        /// What every op checks first: `error-<kind>` fails with that
        /// kind; `env` fails if a libheif variable arrived or the control
        /// didn't; `contained` fails unless the work folder (its current
        /// folder) was marked before the request came.
        fn check_mode(&self) -> Result<(), Failure> {
            if let Some(kind) = self.mode.strip_prefix("error-") {
                let kind: FailKind = serde_json::from_str(&format!("\"{kind}\"")).unwrap();
                return Err(Failure::new(kind, "fake failure"));
            }
            if self.mode == "env" {
                let leaked: Vec<&str> = LIBHEIF_ENV
                    .into_iter()
                    .filter(|name| std::env::var_os(name).is_some())
                    .collect();
                let control = std::env::var_os(CONTROL).is_some();
                if !leaked.is_empty() || !control {
                    return Err(Failure::new(
                        FailKind::Internal,
                        format!("fake: leaked {leaked:?}, control {control}"),
                    ));
                }
            }
            if self.mode == "contained" {
                let marked = std::env::current_dir().is_ok_and(|dir| dir.join(MARKER).is_file());
                if !marked {
                    return Err(Failure::new(
                        FailKind::Internal,
                        "fake: not contained first",
                    ));
                }
            }
            Ok(())
        }
    }

    impl Codec for FakeCodec {
        fn caps(&self) -> Caps {
            Caps {
                decode: true,
                encode: true,
                codec: "fake".to_string(),
            }
        }

        fn decode(
            &self,
            source: &Path,
            _limits: &proto::Limits,
            out: &mut dyn PixelSink,
        ) -> Result<(), Failure> {
            self.check_mode()?;
            let bytes = fs::read(source).map_err(|_| Failure::new(FailKind::Read, "fake"))?;
            let image =
                parse_fake(&bytes).ok_or_else(|| Failure::new(FailKind::Damaged, "fake"))?;
            let info = ImageInfo {
                width: image.width,
                height: image.height,
                channels: image.channels,
                icc_bytes: u32::try_from(image.icc.as_ref().map_or(0, Vec::len)).unwrap(),
            };
            out.begin(&info, image.icc.as_deref())?;
            if self.mode == "slow" {
                thread::sleep(Duration::from_secs(60));
            }
            let row_len = usize::try_from(image.width).unwrap() * usize::from(image.channels);
            for row in image.rows.chunks_exact(row_len) {
                out.row(row)?;
            }
            Ok(())
        }

        fn encode(
            &self,
            pixels: &Pixels,
            _quality: Option<u8>,
            _limits: &proto::Limits,
        ) -> Result<Vec<u8>, Failure> {
            self.check_mode()?;
            match self.mode.as_str() {
                "slow" => thread::sleep(Duration::from_secs(60)),
                "noencoder" => return Err(Failure::new(FailKind::NoEncoder, "fake")),
                _ => {}
            }
            Ok(fake_heic(
                pixels.width,
                pixels.height,
                pixels.channels,
                &pixels.data,
                pixels.icc.as_deref(),
            ))
        }
    }

    /// Not a test: the helper the tests start. The test binary runs only
    /// this "test" (`--exact … --ignored`) with [`FAKE`] set to a mode.
    #[test]
    #[ignore = "a fake HEIC helper, run by the tests below"]
    fn fake_helper() {
        let Ok(mode) = std::env::var(FAKE) else {
            return;
        };
        let say = |line: &str| {
            let mut out = io::stdout().lock();
            out.write_all(line.as_bytes()).unwrap();
            out.write_all(b"\n").unwrap();
            out.flush().unwrap();
        };
        let wait = || thread::sleep(Duration::from_secs(60));
        match mode.as_str() {
            "crash" => {
                say(r#"{"progress":0.5}"#);
                std::process::exit(3);
            }
            "protocol" => {
                say(r#"{"error":{"kind":"protocol","detail":"fake: protocol 2"}}"#);
                std::process::exit(1);
            }
            // 512 MiB of RGBA: within the helper's pixel limit, but too
            // much for most conversions' working set.
            "huge-image" => {
                say(r#"{"image":{"width":16384,"height":8192,"channels":4,"iccBytes":0}}"#);
                wait();
            }
            "over-pixels" => {
                say(r#"{"image":{"width":20000,"height":20000,"channels":3,"iccBytes":0}}"#);
                wait();
            }
            "two-channels" => {
                say(r#"{"image":{"width":2,"height":2,"channels":2,"iccBytes":0}}"#);
                wait();
            }
            "huge-icc" => {
                say(r#"{"image":{"width":2,"height":2,"channels":3,"iccBytes":16707346}}"#);
                wait();
            }
            "lie" => {
                say(r#"{"done":{"bytes":5}}"#);
                std::process::exit(0);
            }
            "noisy" => {
                say("not a message");
                say(&"x".repeat(proto::MAX_LINE_BYTES + 10));
                say(r#"{"progress":0.9}"#);
                say(r#"{"progress":0.2}"#);
                say(r#"{"progress":"NaN"}"#);
                say(r#"{"unknown":1}"#);
            }
            _ => {}
        }
        let codec = FakeCodec { mode: mode.clone() };
        let done = proto::serve(&codec, io::stdin().lock(), io::stdout());
        // After the last line: spoil what was written, for the parent's
        // checks.
        let here = std::env::current_dir().unwrap();
        let append = |name: &str| {
            let mut file = OpenOptions::new()
                .append(true)
                .open(here.join(name))
                .unwrap();
            file.write_all(b"!").unwrap();
        };
        match mode.as_str() {
            "bad-pixels" => append(proto::names::PIXELS),
            "bad-output" => append(proto::names::OUTPUT),
            "wrong-size" => {
                let header = PixelHeader {
                    width: 1,
                    height: 1,
                    channels: 3,
                    icc_len: 0,
                };
                let mut writer = PixelWriter::begin(Vec::new(), &header, None).unwrap();
                writer.row(&[1, 2, 3]).unwrap();
                fs::write(here.join(proto::names::PIXELS), writer.finish().unwrap()).unwrap();
            }
            "done-then-fail" => std::process::exit(1),
            _ => {}
        }
        std::process::exit(if done { 0 } else { 1 });
    }

    /// The test binary as a fake helper in `mode`.
    fn fake(mode: &str) -> Launch {
        Launch {
            program: std::env::current_exe().unwrap(),
            args: [
                "--exact",
                "heif::tests::fake_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ]
            .iter()
            .map(OsString::from)
            .collect(),
            envs: vec![(OsString::from(FAKE), OsString::from(mode))],
            contain: None,
        }
    }

    /// A helper that doesn't exist: a job that starts it fails with
    /// "couldn't start", so success proves nothing was started.
    fn nowhere() -> Launch {
        Launch {
            program: std::env::temp_dir().join(format!("grape-no-heif-helper-{}", unique_suffix())),
            args: Vec::new(),
            envs: Vec::new(),
            contain: None,
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            let path = std::env::temp_dir().join(format!("grape-heif-unit-{}", unique_suffix()));
            fs::create_dir_all(path.join("work")).unwrap();
            TestDir(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn digest(path: &Path) -> (usize, u64) {
        let bytes = fs::read(path).unwrap();
        let mut hasher = DefaultHasher::new();
        hasher.write(&bytes);
        (bytes.len(), hasher.finish())
    }

    /// When a run cancels its job.
    #[derive(Clone, Copy)]
    enum CancelAt {
        Never,
        /// Once progress reaches this.
        Progress(f32),
        /// This long after the job starts.
        After(Duration),
    }

    /// What a run gave: its result, its progress, and when it was
    /// cancelled.
    struct Ran {
        result: Result<JobOutcome, GrapeError>,
        progress: Vec<f32>,
        cancelled_at: Option<Instant>,
    }

    /// A source in its own folder, planned the way the app plans it.
    struct Case {
        dir: TestDir,
        src: PathBuf,
        before: (usize, u64),
        job: Job,
        from: Format,
        to: Format,
    }

    fn case(name: &str, bytes: &[u8], to: Format) -> Case {
        let dir = TestDir::new();
        let src = dir.0.join(name);
        fs::write(&src, bytes).unwrap();
        let settings = Settings::new(dir.0.join("fallback"));
        let selection = crate::build_selection(std::slice::from_ref(&src), &settings);
        let mut jobs = plan(&selection, &Operation::convert(to), &settings)
            .unwrap()
            .jobs;
        assert_eq!(jobs.len(), 1);
        Case {
            before: digest(&src),
            from: Format::from_file_name(&src.file_name().unwrap().to_string_lossy()).unwrap(),
            src,
            job: jobs.remove(0),
            dir,
            to,
        }
    }

    impl Case {
        fn work(&self) -> PathBuf {
            self.dir.0.join("work")
        }

        /// The default limits, with the work folder in this case's folder.
        fn limits(&self) -> Limits {
            Limits {
                work_parent: self.work(),
                ..Limits::default()
            }
        }

        fn dest(&self) -> PathBuf {
            match &self.job.output {
                JobOutput::File(path) => path.clone(),
                other => panic!("{other:?}"),
            }
        }

        fn run(&self, launch: &Launch) -> Ran {
            self.run_with(launch, &self.limits(), CancelAt::Never)
        }

        fn run_with(&self, launch: &Launch, limits: &Limits, cancel: CancelAt) -> Ran {
            let ctx = EngineCtx::default();
            let token = ctx.cancel.clone();
            let cancelled_at = Arc::new(Mutex::new(None));
            if let CancelAt::After(delay) = cancel {
                let (token, at) = (token.clone(), Arc::clone(&cancelled_at));
                thread::spawn(move || {
                    thread::sleep(delay);
                    *at.lock().unwrap() = Some(Instant::now());
                    token.cancel();
                });
            }
            let mut seen = Vec::new();
            let result = {
                let mut progress = |fraction: f32| {
                    seen.push(fraction);
                    if let CancelAt::Progress(at) = cancel {
                        if fraction >= at && !token.is_cancelled() {
                            *cancelled_at.lock().unwrap() = Some(Instant::now());
                            token.cancel();
                        }
                    }
                };
                convert_with(
                    &self.job,
                    &self.src,
                    &self.dest(),
                    self.from,
                    self.to,
                    &ctx,
                    &mut progress,
                    launch,
                    limits,
                )
            };
            let cancelled_at = *cancelled_at.lock().unwrap();
            Ran {
                result,
                progress: seen,
                cancelled_at,
            }
        }

        /// The source is unchanged, and no temp file or work folder is
        /// left.
        fn check(&self) {
            assert_eq!(digest(&self.src), self.before, "the source changed");
            let left: Vec<String> = listing(&self.dir.0)
                .into_iter()
                .filter(|name| name.starts_with(".grape-"))
                .collect();
            assert!(left.is_empty(), "left behind: {left:?}");
            let work = listing(&self.work());
            assert!(work.is_empty(), "work folders left: {work:?}");
        }

        /// Fails with exactly `kind` and `message`, writes nothing, and
        /// leaves nothing.
        fn refused(&self, ran: Ran, kind: ErrorKind, message: &str) -> GrapeError {
            let err = ran.result.unwrap_err();
            assert_eq!(
                (err.kind, err.user_message.as_str()),
                (kind, message),
                "{err:?}"
            );
            assert!(!self.dest().exists(), "an output was written");
            self.check();
            err
        }
    }

    fn assert_monotone(progress: &[f32]) {
        assert!(progress.windows(2).all(|w| w[0] <= w[1]), "{progress:?}");
        assert!(
            progress.iter().all(|p| (0.0..=1.0).contains(p)),
            "{progress:?}"
        );
    }

    /// A smooth `width`×`height` gradient, as rows of RGB or RGBA (alpha
    /// alternating 255 and 128).
    fn gradient(width: u32, height: u32, channels: u8) -> Vec<u8> {
        let mut rows = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let at = |n: u32| u8::try_from(n.min(255)).unwrap();
                rows.extend_from_slice(&[at(x * 30), at(y * 40), at((x + y) * 15)]);
                if channels == 4 {
                    rows.push(if x % 2 == 0 { 255 } else { 128 });
                }
            }
        }
        rows
    }

    /// An ICC profile that `image::icc_fits` takes for RGB pixels.
    fn rgb_profile() -> Vec<u8> {
        let mut profile: Vec<u8> = (0..200u8).collect();
        profile[16..20].copy_from_slice(b"RGB ");
        profile
    }

    /// `rows` (RGB) as a file of `format`.
    fn raster(width: u32, height: u32, rows: &[u8], format: ImageFormat) -> Vec<u8> {
        let img =
            DynamicImage::ImageRgb8(RgbImage::from_raw(width, height, rows.to_vec()).unwrap());
        let mut bytes = Cursor::new(Vec::new());
        img.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    /// `rows` of `color` as a PNG, with `icc` if given.
    fn png(
        width: u32,
        height: u32,
        rows: &[u8],
        color: ExtendedColorType,
        icc: Option<Vec<u8>>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = PngEncoder::new(&mut bytes);
        if let Some(icc) = icc {
            encoder.set_icc_profile(icc).unwrap();
        }
        encoder.write_image(rows, width, height, color).unwrap();
        bytes
    }

    /// The mean absolute difference of two equally long sample lists.
    fn mean_error(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len());
        let total: u64 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| u64::from(x.abs_diff(y)))
            .sum();
        total as f64 / a.len() as f64
    }

    fn image_format(format: Format) -> ImageFormat {
        match format {
            Format::Jpg => ImageFormat::Jpeg,
            Format::Png => ImageFormat::Png,
            Format::Webp => ImageFormat::WebP,
            Format::Tiff => ImageFormat::Tiff,
            Format::Bmp => ImageFormat::Bmp,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn all_ten_pairs_convert_both_ways() {
        let rows = gradient(6, 4, 3);
        let heic = fake_heic(6, 4, 3, &rows, None);
        for to in image::RASTER {
            let case = case("photo.heic", &heic, to);
            let ran = case.run(&fake("ok"));
            let outcome = ran.result.unwrap();
            assert_eq!(outcome.outputs, vec![case.dest()], "{to:?}");
            assert!(!outcome.renamed);
            let out = ::image::open(case.dest()).unwrap().to_rgb8();
            assert_eq!(out.dimensions(), (6, 4));
            let error = mean_error(out.as_raw(), &rows);
            assert!(
                error <= if to == Format::Jpg { 8.0 } else { 0.0 },
                "{to:?}: {error}"
            );
            assert_monotone(&ran.progress);
            assert!(ran.progress.contains(&0.1), "{:?}", ran.progress);
            assert_eq!(ran.progress.last(), Some(&1.0));
            case.check();
        }
        for from in image::RASTER {
            let bytes = raster(6, 4, &rows, image_format(from));
            let case = case(&format!("photo.{}", from.key()), &bytes, Format::Heic);
            let ran = case.run(&fake("ok"));
            let outcome = ran.result.unwrap();
            assert_eq!(outcome.outputs, vec![case.dest()], "{from:?}");
            assert!(case.dest().to_string_lossy().ends_with("photo.heic"));
            let out = parse_fake(&fs::read(case.dest()).unwrap()).unwrap();
            assert_eq!((out.width, out.height, out.channels), (6, 4, 3), "{from:?}");
            let error = mean_error(&out.rows, &rows);
            assert!(
                error <= if from == Format::Jpg { 8.0 } else { 0.0 },
                "{from:?}: {error}"
            );
            assert_monotone(&ran.progress);
            assert_eq!(ran.progress.last(), Some(&1.0));
            case.check();
        }
    }

    #[test]
    fn this_engine_takes_exactly_its_ten_pairs() {
        let pairs: Vec<(Format, Format)> = Format::ALL
            .into_iter()
            .flat_map(|from| Format::ALL.into_iter().map(move |to| (from, to)))
            .filter(|&(from, to)| supports(from, to))
            .collect();
        assert_eq!(pairs.len(), 10);
        for raster in image::RASTER {
            assert!(pairs.contains(&(Format::Heic, raster)));
            assert!(pairs.contains(&(raster, Format::Heic)));
        }
    }

    #[test]
    fn the_dispatch_needs_the_helper() {
        let with_heif = SidecarPaths {
            heif: Some(PathBuf::from("grape-heif")),
            ..SidecarPaths::default()
        };
        let convert = Operation::convert;
        for raster in image::RASTER {
            assert!(crate::supports_with(
                &convert(raster),
                &[Some(Format::Heic)],
                &with_heif
            ));
            assert!(crate::supports_with(
                &convert(Format::Heic),
                &[Some(raster)],
                &with_heif
            ));
            let none = SidecarPaths::default();
            assert!(!crate::supports_with(
                &convert(raster),
                &[Some(Format::Heic)],
                &none
            ));
            assert!(!crate::supports_with(
                &convert(Format::Heic),
                &[Some(raster)],
                &none
            ));
            assert!(!crate::supports(&convert(raster), Some(Format::Heic)));
            assert!(!crate::supports(&convert(Format::Heic), Some(raster)));
        }
        // Over every pair, the helper adds exactly the ten.
        let count = |sidecars: &SidecarPaths| {
            Format::ALL
                .into_iter()
                .flat_map(|from| Format::ALL.into_iter().map(move |to| (from, to)))
                .filter(|&(from, to)| crate::supports_with(&convert(to), &[Some(from)], sidecars))
                .count()
        };
        assert_eq!(count(&with_heif), count(&SidecarPaths::default()) + 10);

        // Without the helper, run_job refuses before reading anything.
        let case = case(
            "photo.heic",
            &fake_heic(1, 1, 3, &[1, 2, 3], None),
            Format::Png,
        );
        let err = crate::run_job(&case.job, &EngineCtx::default(), &mut |_| {}).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported);
        assert_eq!(err.user_message, "HEIC to PNG isn't in this build yet.");
        assert!(!case.dest().exists());
        case.check();
    }

    #[test]
    fn a_missing_helper_says_so() {
        let case = case(
            "photo.heic",
            &fake_heic(1, 1, 3, &[1, 2, 3], None),
            Format::Png,
        );
        let err = convert(
            &case.job,
            &case.src,
            &case.dest(),
            Format::Heic,
            Format::Png,
            &EngineCtx::default(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported);
        assert_eq!(err.user_message, "Grape's HEIC helper is missing.");
        case.check();
    }

    #[test]
    fn alpha_is_kept_both_ways() {
        let rows = gradient(5, 3, 4);
        // PNG with alpha → HEIC: 4 channels in input.rgba, so in the fake.
        let case_in = case(
            "photo.png",
            &png(5, 3, &rows, ExtendedColorType::Rgba8, None),
            Format::Heic,
        );
        case_in.run(&fake("ok")).result.unwrap();
        let out = parse_fake(&fs::read(case_in.dest()).unwrap()).unwrap();
        assert_eq!(out.channels, 4);
        assert_eq!(out.rows, rows);
        case_in.check();

        // HEIC with alpha → PNG keeps it; → JPG flattens it.
        let heic = fake_heic(5, 3, 4, &rows, None);
        let case_out = case("photo.heic", &heic, Format::Png);
        case_out.run(&fake("ok")).result.unwrap();
        let out = ::image::open(case_out.dest()).unwrap();
        assert!(out.color().has_alpha());
        assert_eq!(out.to_rgba8().as_raw(), &rows);
        case_out.check();
        let case_jpg = case("photo.heic", &heic, Format::Jpg);
        case_jpg.run(&fake("ok")).result.unwrap();
        assert!(!::image::open(case_jpg.dest()).unwrap().color().has_alpha());
        case_jpg.check();
    }

    #[test]
    fn an_opaque_source_sends_three_channels() {
        let rows = gradient(4, 4, 3);
        let case = case(
            "photo.png",
            &png(4, 4, &rows, ExtendedColorType::Rgb8, None),
            Format::Heic,
        );
        case.run(&fake("ok")).result.unwrap();
        let out = parse_fake(&fs::read(case.dest()).unwrap()).unwrap();
        assert_eq!(out.channels, 3);
        assert_eq!(out.rows, rows);
        case.check();
    }

    #[test]
    fn the_icc_profile_is_kept_both_ways() {
        let rows = gradient(4, 2, 3);
        let profile = rgb_profile();
        let heic = fake_heic(4, 2, 3, &rows, Some(&profile));
        let case_out = case("photo.heic", &heic, Format::Png);
        case_out.run(&fake("ok")).result.unwrap();
        let mut decoder = ImageReader::open(case_out.dest())
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(decoder.icc_profile().unwrap(), Some(profile.clone()));
        case_out.check();

        let source = png(4, 2, &rows, ExtendedColorType::Rgb8, Some(profile.clone()));
        let case_in = case("photo.png", &source, Format::Heic);
        case_in.run(&fake("ok")).result.unwrap();
        let out = parse_fake(&fs::read(case_in.dest()).unwrap()).unwrap();
        assert_eq!(out.icc, Some(profile));
        case_in.check();

        // A gray profile doesn't describe RGBA pixels, so it's dropped.
        let mut gray = rgb_profile();
        gray[16..20].copy_from_slice(b"GRAY");
        let source = png(4, 2, &[7; 8], ExtendedColorType::L8, Some(gray));
        let case_gray = case("gray.png", &source, Format::Heic);
        case_gray.run(&fake("ok")).result.unwrap();
        let out = parse_fake(&fs::read(case_gray.dest()).unwrap()).unwrap();
        assert_eq!((out.channels, out.icc), (3, None));
        assert_eq!(out.rows, [7; 24]);
        case_gray.check();
    }

    #[test]
    fn a_heic_name_that_isnt_heif_goes_to_the_raster_engine() {
        let rows = gradient(3, 2, 3);
        let case = case(
            "photo.heic",
            &png(3, 2, &rows, ExtendedColorType::Rgb8, None),
            Format::Bmp,
        );
        let ran = case.run(&nowhere());
        ran.result.unwrap();
        assert_eq!(
            ::image::open(case.dest()).unwrap().to_rgb8().as_raw(),
            &rows
        );
        assert_monotone(&ran.progress);
        case.check();

        // Its size isn't held to the HEIC source cap (J41).
        let limits = Limits {
            helper: proto::Limits {
                max_source_bytes: 10,
                ..proto::Limits::DEFAULT
            },
            ..case.limits()
        };
        let case = self::case(
            "photo.heic",
            &png(3, 2, &rows, ExtendedColorType::Rgb8, None),
            Format::Png,
        );
        case.run_with(
            &nowhere(),
            &Limits {
                work_parent: case.work(),
                ..limits
            },
            CancelAt::Never,
        )
        .result
        .unwrap();
        case.check();

        // Not readable as anything: named as the HEIC it claims to be.
        let case = self::case("photo.heic", b"not an image at all", Format::Png);
        let ran = case.run(&nowhere());
        case.refused(
            ran,
            ErrorKind::DecodeFailed,
            "photo.heic isn't a readable HEIC image.",
        );
    }

    #[test]
    fn other_heif_is_refused_before_anything_starts() {
        let case = case("photo.heic", &AVIF_BRAND, Format::Jpg);
        let ran = case.run(&nowhere());
        case.refused(
            ran,
            ErrorKind::DecodeFailed,
            "photo.heic is a kind of HEIF image Grape can't read yet.",
        );
    }

    #[test]
    fn a_heic_over_the_source_cap_is_too_large() {
        let heic = fake_heic(2, 2, 3, &[9; 12], None);
        let case = case("photo.heic", &heic, Format::Png);
        let limits = Limits {
            helper: proto::Limits {
                max_source_bytes: u64::try_from(heic.len() - 1).unwrap(),
                ..proto::Limits::DEFAULT
            },
            ..case.limits()
        };
        let ran = case.run_with(&nowhere(), &limits, CancelAt::Never);
        case.refused(
            ran,
            ErrorKind::DecodeFailed,
            "photo.heic is too large to convert.",
        );
    }

    #[test]
    fn an_image_over_one_hevc_picture_is_refused_before_anything_starts() {
        let source = png(6, 4, &gradient(6, 4, 3), ExtendedColorType::Rgb8, None);
        for helper in [
            proto::Limits {
                max_encode_pixels: 23,
                ..proto::Limits::DEFAULT
            },
            proto::Limits {
                max_encode_side: 5,
                ..proto::Limits::DEFAULT
            },
        ] {
            let case = case("photo.png", &source, Format::Heic);
            let limits = Limits {
                helper,
                ..case.limits()
            };
            let ran = case.run_with(&nowhere(), &limits, CancelAt::Never);
            case.refused(
                ran,
                ErrorKind::EncodeFailed,
                "photo.png is too large to save as HEIC.",
            );
        }
        // Exactly at the caps is fine.
        let case = case("photo.png", &source, Format::Heic);
        let limits = Limits {
            helper: proto::Limits {
                max_encode_pixels: 24,
                max_encode_side: 6,
                ..proto::Limits::DEFAULT
            },
            ..case.limits()
        };
        case.run_with(&fake("ok"), &limits, CancelAt::Never)
            .result
            .unwrap();
        case.check();
    }

    #[test]
    fn an_image_announced_over_a_limit_stops_the_helper_at_once() {
        // Over the helper's pixel limit; and within it, but too much for
        // a WebP's working set (pixels_fit).
        for (mode, to) in [("over-pixels", Format::Png), ("huge-image", Format::Webp)] {
            let case = case("photo.heic", &fake_heic(1, 1, 3, &[0; 3], None), to);
            let started = Instant::now();
            let ran = case.run(&fake(mode));
            assert!(started.elapsed() < Duration::from_secs(20), "{mode}");
            case.refused(
                ran,
                ErrorKind::DecodeFailed,
                "photo.heic is too large to convert.",
            );
        }
    }

    #[test]
    fn cancel_ends_the_helper_quickly() {
        let heic = fake_heic(2, 2, 3, &[5; 12], None);
        // Decode: once the helper has said how large the image is.
        let case_out = case("photo.heic", &heic, Format::Png);
        let ran = case_out.run_with(&fake("slow"), &case_out.limits(), CancelAt::Progress(0.1));
        let at = ran.cancelled_at.unwrap();
        assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
        assert_monotone(&ran.progress);
        case_out.refused(ran, ErrorKind::Cancelled, "Cancelled. Nothing was saved.");

        // Encode: while the helper encodes.
        let source = png(2, 2, &[5; 12], ExtendedColorType::Rgb8, None);
        let case_in = case("photo.png", &source, Format::Heic);
        let ran = case_in.run_with(
            &fake("slow"),
            &case_in.limits(),
            CancelAt::After(Duration::from_millis(700)),
        );
        let at = ran.cancelled_at.unwrap();
        assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
        case_in.refused(ran, ErrorKind::Cancelled, "Cancelled. Nothing was saved.");
    }

    #[test]
    fn a_helper_past_its_time_is_stopped() {
        let quick = |case: &Case| Limits {
            timeout_base: Duration::from_millis(400),
            decode_per_megapixel: Duration::ZERO,
            encode_per_megapixel: Duration::ZERO,
            ..case.limits()
        };
        let case_out = case(
            "photo.heic",
            &fake_heic(2, 2, 3, &[5; 12], None),
            Format::Png,
        );
        let ran = case_out.run_with(&fake("slow"), &quick(&case_out), CancelAt::Never);
        case_out.refused(
            ran,
            ErrorKind::DecodeFailed,
            "photo.heic took too long, so Grape stopped.",
        );

        let source = png(2, 2, &[5; 12], ExtendedColorType::Rgb8, None);
        let case_in = case("photo.png", &source, Format::Heic);
        let ran = case_in.run_with(&fake("slow"), &quick(&case_in), CancelAt::Never);
        case_in.refused(
            ran,
            ErrorKind::DecodeFailed,
            "photo.png took too long, so Grape stopped.",
        );
    }

    #[test]
    fn every_failure_has_its_message() {
        let stopped = (
            ErrorKind::DecodeFailed,
            "Grape's HEIC helper stopped unexpectedly.",
        );
        let temp = (
            ErrorKind::WriteFailed,
            "Grape couldn't write its temp files.",
        );
        let protocol = (
            ErrorKind::NotYetSupported,
            "Grape's HEIC helper doesn't match this version of Grape.",
        );
        let unreadable = (
            ErrorKind::DecodeFailed,
            "photo.heic isn't a readable HEIC image.",
        );
        let heic = fake_heic(2, 2, 3, &[5; 12], None);
        for (kind, (error, message)) in [
            ("notHeif", unreadable),
            ("damaged", unreadable),
            (
                "unsupported",
                (
                    ErrorKind::DecodeFailed,
                    "photo.heic is a kind of HEIF image Grape can't read yet.",
                ),
            ),
            (
                "noImage",
                (
                    ErrorKind::DecodeFailed,
                    "photo.heic has no image to convert.",
                ),
            ),
            (
                "tooLarge",
                (
                    ErrorKind::DecodeFailed,
                    "photo.heic is too large to convert.",
                ),
            ),
            ("noEncoder", stopped),
            ("encode", stopped),
            (
                "read",
                (ErrorKind::ReadFailed, "Grape couldn't read photo.heic."),
            ),
            ("write", temp),
            ("protocol", protocol),
            ("internal", stopped),
        ] {
            let case = case("photo.heic", &heic, Format::Png);
            let ran = case.run(&fake(&format!("error-{kind}")));
            let err = case.refused(ran, error, message);
            assert!(err.detail.contains("fake failure"), "{kind}: {err:?}");
        }

        let source = png(2, 2, &[5; 12], ExtendedColorType::Rgb8, None);
        let no_encoder = (
            ErrorKind::NotYetSupported,
            "This Mac can't write HEIC files.",
        );
        for (mode, (error, message)) in [
            (
                "error-tooLarge",
                (
                    ErrorKind::EncodeFailed,
                    "photo.png is too large to save as HEIC.",
                ),
            ),
            (
                "error-encode",
                (ErrorKind::EncodeFailed, "Grape couldn't write a HEIC file."),
            ),
            ("error-noEncoder", no_encoder),
            ("noencoder", no_encoder),
            (
                "error-read",
                (ErrorKind::ReadFailed, "Grape couldn't read photo.png."),
            ),
            ("error-write", temp),
            ("error-protocol", protocol),
            ("error-notHeif", stopped),
            ("error-noImage", stopped),
            ("error-internal", stopped),
        ] {
            let case = case("photo.png", &source, Format::Heic);
            let ran = case.run(&fake(mode));
            case.refused(ran, error, message);
        }
    }

    #[test]
    fn crashes_lies_and_bad_files_say_the_helper_stopped() {
        let stopped = "Grape's HEIC helper stopped unexpectedly.";
        let heic = fake_heic(3, 2, 3, &[5; 18], None);
        for mode in [
            "crash",
            "lie",
            "bad-pixels",
            "wrong-size",
            "done-then-fail",
            "two-channels",
            "huge-icc",
        ] {
            let case = case("photo.heic", &heic, Format::Png);
            let ran = case.run(&fake(mode));
            case.refused(ran, ErrorKind::DecodeFailed, stopped);
        }
        let source = png(3, 2, &[5; 18], ExtendedColorType::Rgb8, None);
        for mode in ["crash", "lie", "bad-output", "done-then-fail"] {
            let case = case("photo.png", &source, Format::Heic);
            let ran = case.run(&fake(mode));
            case.refused(ran, ErrorKind::DecodeFailed, stopped);
        }
        // A helper that isn't Grape's at all: this test binary, run plainly,
        // prints test results and no message.
        let mut plain = fake("ok");
        plain.args = vec![OsString::from("--list")];
        let case = case("photo.heic", &heic, Format::Png);
        let ran = case.run(&plain);
        case.refused(ran, ErrorKind::DecodeFailed, stopped);
    }

    #[test]
    fn a_protocol_mismatch_says_so() {
        let case = case(
            "photo.heic",
            &fake_heic(1, 1, 3, &[0; 3], None),
            Format::Png,
        );
        let ran = case.run(&fake("protocol"));
        case.refused(
            ran,
            ErrorKind::NotYetSupported,
            "Grape's HEIC helper doesn't match this version of Grape.",
        );
    }

    #[test]
    fn noise_on_stdout_is_skipped_and_progress_never_goes_back() {
        let rows = gradient(4, 40, 3);
        let case = case("photo.heic", &fake_heic(4, 40, 3, &rows, None), Format::Png);
        let ran = case.run(&fake("noisy"));
        ran.result.unwrap();
        assert_monotone(&ran.progress);
        assert_eq!(ran.progress.last(), Some(&1.0));
        assert_eq!(
            ::image::open(case.dest()).unwrap().to_rgb8().as_raw(),
            &rows
        );
        case.check();
    }

    #[test]
    fn libheif_variables_never_reach_the_helper() {
        let heic = fake_heic(1, 1, 3, &[0; 3], None);
        let source = png(1, 1, &[0; 3], ExtendedColorType::Rgb8, None);
        let with_env = |control: bool| {
            let mut launch = fake("env");
            launch.envs.extend(
                [
                    ("LIBHEIF_PLUGIN_PATH", "/plugins"),
                    ("LIBHEIF_SECURITY_LIMITS", "off"),
                ]
                .into_iter()
                .chain(control.then_some((CONTROL, "1")))
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
            );
            launch
        };
        for (name, bytes, to) in [
            ("photo.heic", &heic, Format::Png),
            ("photo.png", &source, Format::Heic),
        ] {
            let case = case(name, bytes, to);
            case.run(&with_env(true)).result.unwrap();
            case.check();
            // The fake does see variables that aren't removed.
            let case = self::case(name, bytes, to);
            let ran = case.run(&with_env(false));
            let err = case.refused(
                ran,
                ErrorKind::DecodeFailed,
                "Grape's HEIC helper stopped unexpectedly.",
            );
            assert!(err.detail.contains("control false"), "{err:?}");
            assert!(err.detail.contains("leaked []"), "{err:?}");
        }
    }

    /// A container that marks the work folder, so the `contained` fake can
    /// tell it came before the request.
    #[derive(Debug)]
    struct FakeContain {
        work_parent: PathBuf,
        fail: bool,
        peak: Option<u64>,
        calls: AtomicUsize,
        max_memory: Mutex<Option<Option<u64>>>,
    }

    impl FakeContain {
        fn new(case: &Case, fail: bool, peak: Option<u64>) -> Arc<FakeContain> {
            Arc::new(FakeContain {
                work_parent: case.work(),
                fail,
                peak,
                calls: AtomicUsize::new(0),
                max_memory: Mutex::new(None),
            })
        }
    }

    struct FakeContained(Option<u64>);

    impl Contained for FakeContained {
        fn peak_memory(&self) -> Option<u64> {
            self.0
        }
    }

    impl Contain for FakeContain {
        fn contain(
            &self,
            _child: &Child,
            max_memory: Option<u64>,
        ) -> Result<Box<dyn Contained>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.max_memory.lock().unwrap() = Some(max_memory);
            if self.fail {
                return Err("fake: no container".to_string());
            }
            for entry in fs::read_dir(&self.work_parent).unwrap() {
                let entry = entry.unwrap();
                if entry.file_name().to_string_lossy().starts_with(WORK_PREFIX) {
                    fs::write(entry.path().join(MARKER), b"").unwrap();
                }
            }
            Ok(Box::new(FakeContained(self.peak)))
        }
    }

    #[test]
    fn the_helper_is_contained_before_it_gets_its_request() {
        let heic = fake_heic(2, 1, 3, &[1, 2, 3, 4, 5, 6], None);
        let case = case("photo.heic", &heic, Format::Png);
        let contain = FakeContain::new(&case, false, None);
        let mut launch = fake("contained");
        launch.contain = Some(contain.clone());
        case.run(&launch).result.unwrap();
        assert_eq!(contain.calls.load(Ordering::SeqCst), 1);
        assert_eq!(*contain.max_memory.lock().unwrap(), Some(Some(2 * GIB)));
        case.check();

        // Uncontained, the fake notices.
        let case = self::case("photo.heic", &heic, Format::Png);
        let ran = case.run(&fake("contained"));
        case.refused(
            ran,
            ErrorKind::DecodeFailed,
            "Grape's HEIC helper stopped unexpectedly.",
        );
    }

    #[test]
    fn a_helper_that_cant_be_contained_is_stopped() {
        let case = case(
            "photo.heic",
            &fake_heic(1, 1, 3, &[0; 3], None),
            Format::Png,
        );
        let contain = FakeContain::new(&case, true, None);
        let mut launch = fake("slow");
        launch.contain = Some(contain.clone());
        let started = Instant::now();
        let ran = case.run(&launch);
        assert!(started.elapsed() < Duration::from_secs(20));
        let err = case.refused(
            ran,
            ErrorKind::NotYetSupported,
            "Grape's HEIC helper couldn't start.",
        );
        assert!(err.detail.contains("fake: no container"), "{err:?}");
        assert_eq!(contain.calls.load(Ordering::SeqCst), 1);

        // A helper that can't be found says the same.
        let case = self::case(
            "photo.heic",
            &fake_heic(1, 1, 3, &[0; 3], None),
            Format::Png,
        );
        let ran = case.run(&nowhere());
        case.refused(
            ran,
            ErrorKind::NotYetSupported,
            "Grape's HEIC helper couldn't start.",
        );
    }

    #[test]
    fn a_crash_at_the_memory_cap_needs_too_much_memory() {
        let heic = fake_heic(1, 1, 3, &[0; 3], None);
        for (peak, message) in [
            (
                Some(2 * GIB),
                "photo.heic needs too much memory, so Grape stopped.",
            ),
            // 90% of the cap, rounded up; just under it isn't.
            (
                Some((2 * GIB * 9).div_ceil(10)),
                "photo.heic needs too much memory, so Grape stopped.",
            ),
            (
                Some((2 * GIB * 9).div_ceil(10) - 1),
                "Grape's HEIC helper stopped unexpectedly.",
            ),
            (Some(GIB), "Grape's HEIC helper stopped unexpectedly."),
            (None, "Grape's HEIC helper stopped unexpectedly."),
        ] {
            let case = case("photo.heic", &heic, Format::Png);
            let mut launch = fake("crash");
            launch.contain = Some(FakeContain::new(&case, false, peak));
            let ran = case.run(&launch);
            case.refused(ran, ErrorKind::DecodeFailed, message);
        }
    }

    #[test]
    fn a_work_folder_that_cant_be_made_writes_nothing() {
        let heic = fake_heic(1, 1, 3, &[0; 3], None);
        let case = case("photo.heic", &heic, Format::Png);
        let limits = Limits {
            work_parent: case.dir.0.join("no-such-folder"),
            ..case.limits()
        };
        let ran = case.run_with(&nowhere(), &limits, CancelAt::Never);
        case.refused(
            ran,
            ErrorKind::WriteFailed,
            "Grape couldn't write its temp files.",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_isnt_unicode_cant_be_sent() {
        use std::os::unix::ffi::OsStrExt;
        let dir = TestDir::new();
        let src = dir.0.join(std::ffi::OsStr::from_bytes(b"photo\xff.heic"));
        let heic = fake_heic(1, 1, 3, &[0; 3], None);
        fs::write(&src, &heic).unwrap();
        let settings = Settings::new(dir.0.join("fallback"));
        let selection = crate::build_selection(std::slice::from_ref(&src), &settings);
        let job = plan(&selection, &Operation::convert(Format::Png), &settings)
            .unwrap()
            .jobs
            .remove(0);
        let JobOutput::File(dest) = &job.output else {
            panic!("{:?}", job.output);
        };
        let limits = Limits {
            work_parent: dir.0.join("work"),
            ..Limits::default()
        };
        let err = convert_with(
            &job,
            &src,
            dest,
            Format::Heic,
            Format::Png,
            &EngineCtx::default(),
            &mut |_| {},
            &nowhere(),
            &limits,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReadFailed, "{err:?}");
        assert!(listing(&dir.0.join("work")).is_empty());
        assert_eq!(fs::read(&src).unwrap(), heic);
    }

    #[test]
    fn limits_and_timeouts() {
        let helper = proto::Limits::DEFAULT;
        assert_eq!(helper.max_source_bytes, image::MAX_SOURCE);
        assert_eq!(helper.max_decoder_memory, image::WORKING_SET_LIMIT);
        // 512 MiB of RGBA8, the image crate's default decode limit.
        assert_eq!(helper.max_pixels * 4, 512 * MIB);
        let limits = Limits::default();
        assert_eq!(limits.helper, helper);
        assert_eq!(limits.helper_memory, 2 * GIB);
        assert_eq!(limits.work_parent, std::env::temp_dir());
        // At the caps: 120 s + 135 s to decode, 120 s + 3 × 36 s to encode.
        let decode = per_megapixel(limits.decode_per_megapixel, helper.max_pixels);
        let encode = per_megapixel(limits.encode_per_megapixel, helper.max_encode_pixels);
        assert_eq!(limits.timeout_base + decode, Duration::from_secs(255));
        assert_eq!(limits.timeout_base + encode, Duration::from_secs(228));
        assert_eq!(
            per_megapixel(Duration::from_secs(1), 1),
            Duration::from_secs(1)
        );
        assert_eq!(per_megapixel(Duration::from_secs(1), 0), Duration::ZERO);
        assert_eq!(
            per_megapixel(Duration::from_secs(1), u64::MAX),
            Duration::from_secs(u64::from(u32::MAX))
        );
    }

    #[test]
    fn a_cancelled_job_starts_nothing() {
        let case = case(
            "photo.heic",
            &fake_heic(1, 1, 3, &[0; 3], None),
            Format::Png,
        );
        let ctx = EngineCtx::default();
        ctx.cancel.cancel();
        let err = convert_with(
            &case.job,
            &case.src,
            &case.dest(),
            Format::Heic,
            Format::Png,
            &ctx,
            &mut |_| {},
            &nowhere(),
            &case.limits(),
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);
        case.check();
    }
}
