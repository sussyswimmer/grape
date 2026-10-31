//! HEIC and AVIF ⇄ JPG, PNG, WebP, TIFF and BMP, HEIC ⇄ AVIF (and their
//! pixels for `pdf::create`) through Grape's image helper, a separate
//! process (DECISIONS D33, D41). Lane A's module, by agreement with lane B,
//! who owns this crate.
//!
//! The helper is `grape-heif.exe` (libheif: libde265 and x265 for HEIC,
//! dav1d and libaom for AVIF) on Windows and `Grape --grape-heif-worker`
//! (ImageIO, and rav1e for AVIF output) on macOS. Both speak
//! `grape-heif-proto` (protocol 2, D41), so this side has no `cfg`: it
//! starts `<SidecarPaths::heif> --grape-heif-worker` either way.
//!
//! - **HEIC or AVIF → raster:** the source's `ftyp` is sniffed before
//!   anything starts, and the content decides, whatever the name: a `.avif`
//!   that's HEIC decodes as HEIC, and a `.heic` that's AVIF as AVIF. A file
//!   that isn't HEIF goes to the raster engine ([`image::convert`],
//!   FORMAT_MATRIX rule 3); HEIF with another codec (AVC, JPEG, VVC…) is
//!   refused. Otherwise the helper decodes the primary image, upright and
//!   8-bit, into `pixels.rgba` in a private work folder under the OS temp
//!   folder. Its `image` line comes before any pixel is written (libheif
//!   sends it before decoding, ImageIO after its own decode, held to the
//!   decoder budget), so a picture whose conversion wouldn't fit the
//!   working set ([`image::pixels_fit`]) is stopped there. The pixels are
//!   read once, into the one buffer `pixels_fit` counted, and published by
//!   [`image::publish_pixels`] (no EXIF, so nothing is rotated twice).
//!   That decode half ([`decode_pixels`]) also serves HEIC and AVIF → PDF
//!   (`pdf::create`, D37), through [`Decoder`], and HEIC Compress (D28),
//!   through [`Decoder::decode_within`], which holds the `image` line to
//!   what the tool itself holds instead.
//! - **Raster → HEIC or AVIF:** the raster engine decodes the source
//!   ([`image::decode`], EXIF orientation applied), its size is checked
//!   against one picture of the format ([`Limits::encode_limits`]: one HEVC
//!   or AV1 picture), and its 8-bit RGB or RGBA rows are streamed into
//!   `input.rgba` ([`encode_pixels`]). The helper writes `out.heic` or
//!   `out.avif`, which must sniff as the format asked for; it's copied into
//!   a temp file in the destination folder, then published without
//!   replacing anything. HEIC Compress (D28) encodes through the same steps
//!   with a quality ([`Encoder`]), and gets the bytes back instead; its
//!   [`Encoder::check_encode`] probes each helper once.
//! - **HEIC ⇄ AVIF** is two helper runs: a decode, then an encode, each in
//!   its own work folder and with its own deadline. A source that isn't
//!   HEIF is decoded by the raster engine instead (rule 3).
//! - **The helper** ([`crate::helper`]) runs with `LIBHEIF_PLUGIN_PATH` and
//!   `LIBHEIF_SECURITY_LIMITS` removed from its environment, in its work
//!   folder, contained by the app's hook (a Job Object on Windows) before
//!   it gets its request. The job polls every 100 ms: a cancel or the
//!   deadline ([`Limits`]) kills it. Its progress is passed on, never
//!   backwards. One that ends near its container's memory cap, or with
//!   the heap bound's exit code ([`OVER_HEAP_EXIT`], the Mac worker's),
//!   needed too much memory.
//! - **Messages** name the file in Grape's words (heic-spec §9), and the
//!   format where one is named ("… is too large to save as AVIF."). No
//!   detail written here holds a path, a name or file contents; the
//!   helper's own details never do either.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ::image::{ColorType, DynamicImage};
use grape_core::{Format, Job};
use grape_heif_proto::{
    self as proto, Caps, FailKind, Failure, HeifFormat, ImageInfo, Message, Op, PixelHeader,
    PixelWriter, Request, Sniff,
};

use crate::cancel::Cancellable;
use crate::helper::{self, Launch, Polled, Start, StartError, WorkDir};
use crate::image;
use crate::output::TempFile;
use crate::pdf::heap::OVER_HEAP_EXIT;
use crate::publish::{self, Names};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome};

const MIB: u64 = 1024 * 1024;

/// The work folder's name starts with this (J4).
pub(crate) const WORK_PREFIX: &str = "grape-heif-";

/// How often the job checks for a cancel and the deadline.
const POLL: Duration = Duration::from_millis(100);

/// The only variables libheif reads. Removed from the helper's
/// environment: an inherited `LIBHEIF_PLUGIN_PATH` would point a build with
/// plugin loading at a folder of DLLs, and `LIBHEIF_SECURITY_LIMITS=off`
/// would switch libheif's limits off (heic-spec J38).
const LIBHEIF_ENV: [&str; 2] = ["LIBHEIF_PLUGIN_PATH", "LIBHEIF_SECURITY_LIMITS"];

/// The buffer of the writers that stream pixels and copy the output.
const WRITE_BUFFER: usize = 1 << 20;

/// The formats the helper reads and writes (D41).
pub(crate) const HEIF: [Format; 2] = [Format::Heic, Format::Avif];

/// True for the 22 pairs: HEIC or AVIF to or from JPG, PNG, WebP, TIFF or
/// BMP (20), and HEIC to AVIF and back (2).
pub(crate) fn supports(from: Format, to: Format) -> bool {
    (HEIF.contains(&from) && image::RASTER.contains(&to))
        || (HEIF.contains(&to) && image::RASTER.contains(&from))
        || (HEIF.contains(&from) && HEIF.contains(&to) && from != to)
}

/// The helper's name for `format`, one of [`HEIF`].
fn heif_format(format: Format) -> Option<HeifFormat> {
    match format {
        Format::Heic => Some(HeifFormat::Heic),
        Format::Avif => Some(HeifFormat::Avif),
        _ => None,
    }
}

/// Grape's name for the helper's `format`.
fn grape_format(format: HeifFormat) -> Format {
    match format {
        HeifFormat::Heic => Format::Heic,
        HeifFormat::Avif => Format::Avif,
    }
}

/// The bounds of a HEIC or AVIF job (heic-spec §8, D41).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Limits {
    /// What the helper holds to; sent with every decode, and with an
    /// encode's caps held to one picture of its format
    /// ([`Limits::encode_limits`]).
    pub(crate) helper: proto::Limits,
    /// The time every job gets.
    pub(crate) timeout_base: Duration,
    /// Added for each started megapixel of a decoded image, once the
    /// helper reports its size.
    pub(crate) decode_per_megapixel: Duration,
    /// Added for each started megapixel of an image to encode.
    pub(crate) encode_per_megapixel: Duration,
    /// The helper's memory cap, where the app's container can set one
    /// (the Job Object on Windows): [`proto::WORKER_HEAP_BYTES`], the
    /// Mac worker's own heap bound too.
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
            helper_memory: proto::WORKER_HEAP_BYTES,
            work_parent: std::env::temp_dir(),
        }
    }
}

impl Limits {
    /// What an encode into `format` holds to and sends: [`Limits::helper`],
    /// its encode caps no larger than one picture of that format
    /// ([`proto::Limits::for_encode`]: 16,888 a side for HEIC, 16,384 for
    /// AVIF, 35,651,584 pixels for both).
    pub(crate) fn encode_limits(&self, format: HeifFormat) -> proto::Limits {
        let picture = proto::Limits::for_encode(format);
        proto::Limits {
            max_encode_pixels: self.helper.max_encode_pixels.min(picture.max_encode_pixels),
            max_encode_side: self.helper.max_encode_side.min(picture.max_encode_side),
            ..self.helper
        }
    }
}

impl Launch {
    /// `<helper> --grape-heif-worker`, contained by the app's hook.
    pub(crate) fn heif_helper(program: &Path) -> Launch {
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
            "Grape's image helper is missing.",
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
    match (heif_format(from), heif_format(to)) {
        (Some(_), None) if image::RASTER.contains(&to) => {
            from_heif(job, src, dest, from, to, ctx, progress, &run)
        }
        (None, Some(format)) if image::RASTER.contains(&from) => {
            to_heif(job, src, dest, from, format, ctx, progress, &run)
        }
        (Some(_), Some(format)) if from != to => {
            heif_to_heif(job, src, dest, from, format, ctx, progress, &run)
        }
        _ => Err(crate::not_yet_supported(job, Some(from))),
    }
}

/// What every helper run in a job shares.
pub(crate) struct Run<'a> {
    /// How to start the helper.
    pub(crate) launch: &'a Launch,
    /// The bounds of the job.
    pub(crate) limits: &'a Limits,
    /// The job's cancel flag.
    pub(crate) cancel: &'a CancelToken,
}

/// HEIC or AVIF → JPG, PNG, WebP, TIFF or BMP (heic-spec §4.1, D41).
#[allow(clippy::too_many_arguments)] // `convert`'s, plus the run.
fn from_heif(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    run: &Run<'_>,
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, to);
    publish::dest_dir(dest, &names)?;
    if run.cancel.is_cancelled() {
        return Err(cancelled());
    }

    let mut last = 0.0f32;
    let decoded = {
        let mut forward = |fraction: f32| {
            if fraction > last {
                last = fraction;
                progress(fraction);
            }
        };
        decode_pixels(src, &names, Fit::Convert(to), run, (0.1, 0.5), &mut forward)?
    };
    let (img, icc) = match decoded {
        // FORMAT_MATRIX rule 3: the content decides. The raster engine
        // keeps its own limits, and its messages name the file by its name.
        Decoded::NotHeif => return image::convert(job, src, dest, from, to, ctx, progress),
        Decoded::Pixels(img, icc) => (img, icc),
    };
    if last < 0.5 {
        progress(0.5);
    }
    image::publish_pixels(job, src, dest, from, to, img, icc, ctx, progress)
}

/// HEIC → AVIF or AVIF → HEIC (D41): two helper runs, a decode in
/// (0.05, 0.45) and an encode in (0.5, 0.9), each in its own work folder.
/// A source that isn't HEIF at all is decoded by the raster engine
/// instead (rule 3), then encoded the same way.
#[allow(clippy::too_many_arguments)] // `convert`'s, plus the run.
fn heif_to_heif(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    format: HeifFormat,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    run: &Run<'_>,
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, grape_format(format));
    publish::dest_dir(dest, &names)?;
    let cancel = run.cancel;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }

    let mut last = 0.0f32;
    let mut forward = |fraction: f32| {
        if fraction > last {
            last = fraction;
            progress(fraction);
        }
    };
    // Held to what encoding the pixels takes: `Format::Heic` stands in for
    // both (see `encode_pixels`).
    let fit = Fit::Convert(Format::Heic);
    let (img, icc) = match decode_pixels(src, &names, fit, run, (0.05, 0.45), &mut forward)? {
        Decoded::Pixels(img, icc) => (img, icc),
        Decoded::NotHeif => image::decode(src, from, Format::Heic, &names, cancel)
            .map_err(|e| publish::or_cancelled(cancel, src, e))?,
    };
    forward(0.45);
    encode_pixels(
        job,
        src,
        dest,
        from,
        format,
        img,
        icc,
        ctx,
        &mut forward,
        (0.5, 0.9),
        run,
    )
}

/// What [`decode_pixels`] found in a HEIC or AVIF file.
#[derive(Debug)]
pub(crate) enum Decoded {
    /// The primary image, upright and 8-bit RGB or RGBA, as the helper
    /// decoded it, with its ICC profile.
    Pixels(DynamicImage, Option<Vec<u8>>),
    /// The file isn't HEIF at all: the raster engine reads it
    /// (FORMAT_MATRIX rule 3). Nothing was started.
    NotHeif,
}

/// Grape's image helper and the bounds a job holds it to, for an engine
/// that needs a HEIC's or an AVIF's pixels rather than a raster file
/// (`pdf::create`).
pub(crate) struct Decoder {
    launch: Launch,
    limits: Limits,
}

impl Decoder {
    /// The helper in the app's sidecars, if it found one, with Grape's
    /// limits.
    pub(crate) fn from_sidecars(sidecars: &crate::SidecarPaths) -> Option<Decoder> {
        let program = sidecars.heif.as_deref()?;
        Some(Decoder {
            launch: Launch::heif_helper(program),
            limits: Limits::default(),
        })
    }

    /// This helper and these limits (the tests' fake helper).
    #[cfg(test)]
    pub(crate) fn new(launch: Launch, limits: Limits) -> Decoder {
        Decoder { launch, limits }
    }

    /// Decodes `src`, HEIC or AVIF by its content, as [`decode_pixels`]
    /// does, its pixels held to what converting them to `fit` may use
    /// ([`image::pixels_fit`]). The helper's progress goes to `progress`
    /// within `span`.
    pub(crate) fn decode(
        &self,
        src: &Path,
        names: &Names,
        fit: Format,
        cancel: &CancelToken,
        span: (f32, f32),
        progress: &mut dyn FnMut(f32),
    ) -> Result<Decoded, GrapeError> {
        let run = Run {
            launch: &self.launch,
            limits: &self.limits,
            cancel,
        };
        decode_pixels(src, names, Fit::Convert(fit), &run, span, progress)
    }

    /// [`Decoder::decode`] for a tool that holds the pixels itself (HEIC
    /// Compress, D28): the helper's `image` line is held to `peak(width,
    /// height, color)` plus its ICC bytes, at most
    /// [`image::WORKING_SET_LIMIT`], instead of [`image::pixels_fit`]. Over
    /// it, the helper is killed before any pixel is written ("… is too large
    /// to convert."). Everything else, the errors included, is
    /// [`Decoder::decode`]'s.
    #[cfg_attr(not(test), allow(dead_code))] // Lane B's HEIC Compress (D28) calls it.
    pub(crate) fn decode_within(
        &self,
        src: &Path,
        names: &Names,
        peak: image::ToolPeak<'_>,
        cancel: &CancelToken,
        span: (f32, f32),
        progress: &mut dyn FnMut(f32),
    ) -> Result<Decoded, GrapeError> {
        let run = Run {
            launch: &self.launch,
            limits: &self.limits,
            cancel,
        };
        decode_pixels(src, names, Fit::Peak(peak), &run, span, progress)
    }
}

/// What a decoded image's `image` line must fit before any pixel is
/// written.
#[derive(Clone, Copy)]
enum Fit<'a> {
    /// The working set of converting the pixels to this format
    /// ([`image::pixels_fit`]).
    Convert(Format),
    /// What a tool holds at its peak for the image, plus the ICC profile,
    /// within [`image::WORKING_SET_LIMIT`] ([`Decoder::decode_within`]).
    Peak(image::ToolPeak<'a>),
}

impl Fit<'_> {
    /// `Err` with the detail when a `width`×`height` image of `color` with
    /// `icc` profile bytes doesn't fit.
    fn check(self, width: u32, height: u32, color: ColorType, icc: u64) -> Result<(), String> {
        match self {
            Fit::Convert(to) => image::pixels_fit(width, height, color, to, icc),
            Fit::Peak(peak) => {
                let needed = peak(width, height, color).saturating_add(icc);
                if needed > image::WORKING_SET_LIMIT {
                    return Err(format!(
                        "{width}x{height} {color:?} needs about {} MiB, over the {} MiB limit",
                        needed / MIB,
                        image::WORKING_SET_LIMIT / MIB
                    ));
                }
                Ok(())
            }
        }
    }
}

/// Grape's image helper as the encoder of one format, with the bounds a
/// job holds it to: for an engine that has pixels and wants a HEIC's (or
/// an AVIF's) bytes back rather than a published file (HEIC Compress,
/// D28). It runs the conversions' encode ([`encode_pixels`]): the same
/// checks, `input.rgba`, request, deadline and output checks.
#[cfg_attr(not(test), allow(dead_code))] // Lane B's HEIC Compress (D28) builds it.
pub(crate) struct Encoder {
    launch: Launch,
    limits: Limits,
    format: HeifFormat,
}

#[cfg_attr(not(test), allow(dead_code))] // Lane B's HEIC Compress (D28) calls these.
impl Encoder {
    /// The helper in the app's sidecars, if it found one, writing `format`
    /// with Grape's limits.
    pub(crate) fn from_sidecars(
        sidecars: &crate::SidecarPaths,
        format: HeifFormat,
    ) -> Option<Encoder> {
        let program = sidecars.heif.as_deref()?;
        Some(Encoder {
            launch: Launch::heif_helper(program),
            limits: Limits::default(),
            format,
        })
    }

    /// This helper and these limits (the tests' fake helper).
    #[cfg(test)]
    pub(crate) fn new(launch: Launch, limits: Limits, format: HeifFormat) -> Encoder {
        Encoder {
            launch,
            limits,
            format,
        }
    }

    /// The largest picture it encodes, as it sends it: (`max_encode_pixels`,
    /// `max_encode_side`) of [`Limits::encode_limits`] for its format
    /// (35,651,584 pixels and 16,888 a side for HEIC, 16,384 for AVIF).
    pub(crate) fn max_picture(&self) -> (u64, u32) {
        let limits = self.limits.encode_limits(self.format);
        (limits.max_encode_pixels, limits.max_encode_side)
    }

    /// `Ok` when its helper says it can write the format. The helper is
    /// asked (`probe`) once per process for each helper path; a probe that
    /// fails isn't kept, so the next job asks again. A helper that can't
    /// write it is `noEncoder`'s message ("This Mac can't write HEIC
    /// files." on a Mac, where the machine decides; on Windows, a
    /// `grape-heif.exe` that isn't this Grape's). Its other failures read as
    /// an encode's would.
    pub(crate) fn check_encode(
        &self,
        names: &Names,
        cancel: &CancelToken,
    ) -> Result<(), GrapeError> {
        let caps = probed_caps(&self.run(cancel), names, self.format)?;
        if caps.of(self.format).encode {
            return Ok(());
        }
        Err(GrapeError::new(
            ErrorKind::NotYetSupported,
            no_encoder(self.format, cfg!(target_os = "macos")),
            format!(
                "HEIF helper: its probe says it can't write {}",
                self.format.name()
            ),
        ))
    }

    /// The bytes of a file of its format for `img`, which must be upright
    /// 8-bit RGB or RGBA ([`DynamicImage::ImageRgb8`] or
    /// [`DynamicImage::ImageRgba8`]; anything else is "Grape couldn't write
    /// a HEIC file.") and fit [`Encoder::max_picture`]. Its rows are
    /// streamed into `input.rgba` as they are, with `icc` if it's an RGB
    /// profile ([`image::icc_fits`]) no larger than the helper keeps (one
    /// that isn't is dropped, as conversions drop it), and the helper
    /// encodes them at `quality` (1..=100; `None`: its default for the
    /// format). Its output must be the size it reported, sniff as the
    /// format, and be no larger than the source cap
    /// ([`proto::Limits::max_source_bytes`]) to be read back. The helper's
    /// progress goes to `progress` within `span`, only forwards, ending at
    /// `span.1`. Errors are a conversion's into the format.
    #[allow(clippy::too_many_arguments)] // The pixels, the quality, the names and the job's.
    pub(crate) fn encode(
        &self,
        img: &DynamicImage,
        icc: Option<&[u8]>,
        quality: Option<u8>,
        names: &Names,
        cancel: &CancelToken,
        span: (f32, f32),
        progress: &mut dyn FnMut(f32),
    ) -> Result<Vec<u8>, GrapeError> {
        let run = self.run(cancel);
        let format = self.format;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        if let Some(quality) = quality.filter(|q| !(1..=100).contains(q)) {
            return Err(names.encode_failed(format!(
                "quality {quality} for {} isn't in 1..=100",
                format.name()
            )));
        }
        let staged = stage(&run, format, img, icc, names)?;
        let mut last = span.0;
        progress(span.0);
        let mut forward = |fraction: f32| {
            if fraction > last {
                last = fraction;
                progress(fraction);
            }
        };
        let (out, bytes) = run_encode(&run, format, &staged, quality, names, span, &mut forward)?;
        let cap = self.limits.helper.max_source_bytes;
        if bytes > cap {
            return Err(too_large_to_save(
                names,
                format,
                format!("its output is {bytes} bytes, over the {cap}-byte cap for reading it back"),
            ));
        }
        let data = read_output(&out, bytes, cancel).map_err(|e| {
            if cancel.is_cancelled() {
                cancelled()
            } else {
                helper_stopped(&format!("its output: {}", io_detail(&e)))
            }
        })?;
        // The bytes are in memory; the folder's copy goes at once.
        drop(staged);
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        forward(span.1);
        Ok(data)
    }

    /// One helper run's share of this encoder.
    fn run<'a>(&'a self, cancel: &'a CancelToken) -> Run<'a> {
        Run {
            launch: &self.launch,
            limits: &self.limits,
            cancel,
        }
    }
}

/// How a helper is started, as [`PROBED`] tells helpers apart: in the app
/// its path (the arguments are always [`proto::WORKER_FLAG`]); in the tests
/// the fake's mode too.
type ProbeKey = (PathBuf, Vec<OsString>, Vec<(OsString, OsString)>);

/// What each helper answered to its probe, asked once per process
/// ([`Encoder::check_encode`]).
static PROBED: OnceLock<Mutex<HashMap<ProbeKey, Caps>>> = OnceLock::new();

/// The caps of `run`'s helper: [`PROBED`]'s, or a probe's, which is kept.
/// Two jobs that start together may both probe; the first answer is kept.
/// A probe never blocks on another, so each still answers its own cancel.
fn probed_caps(run: &Run<'_>, names: &Names, format: HeifFormat) -> Result<Caps, GrapeError> {
    let launch = run.launch;
    let key: ProbeKey = (
        launch.program.clone(),
        launch.args.clone(),
        launch.envs.clone(),
    );
    let probed = PROBED.get_or_init(Mutex::default);
    let known = probed
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .cloned();
    if let Some(caps) = known {
        return Ok(caps);
    }
    let caps = probe(run, names, format)?;
    let mut probed = probed.lock().unwrap_or_else(PoisonError::into_inner);
    Ok(probed.entry(key).or_insert(caps).clone())
}

/// Asks the helper what it can do (`probe`): one run with its own work
/// folder and the base deadline. Its failures read as an encode into
/// `format`'s.
fn probe(run: &Run<'_>, names: &Names, format: HeifFormat) -> Result<Caps, GrapeError> {
    if run.cancel.is_cancelled() {
        return Err(cancelled());
    }
    let work = make_work_dir(run.limits)?;
    let mut on_image = |_: &ImageInfo| -> Result<Duration, Ended> {
        Err(Ended::Crashed("an image line while probing".to_string()))
    };
    let ended = drive(
        run,
        &work,
        Op::Probe {},
        run.limits.timeout_base,
        &mut on_image,
        (0.0, 0.0),
        &mut |_| {},
    );
    match ended {
        Ended::Done(done, _) => done
            .caps
            .ok_or_else(|| helper_stopped("its probe's done line has no caps")),
        other => Err(ended_error(other, names, Side::Encode(format))),
    }
}

/// The decode half of HEIC or AVIF → anything: `src`'s `ftyp` is sniffed,
/// and the content decides whatever the name (not HEIF at all →
/// [`Decoded::NotHeif`]; HEIC or AVIF → the helper; HEIF with another
/// codec is refused), its size checked, and the helper run. Its `image`
/// line must fit the helper's limits and `fit` (the working set of
/// converting the pixels, [`image::pixels_fit`], or a tool's peak); the
/// pixels are then read once, into the one buffer that check counted. The
/// helper's progress `f` goes to `progress` as `span.0 + (span.1 - span.0)
/// × f` (it may go backwards; callers pass it on only forwards). `names`
/// name the file in the messages.
fn decode_pixels(
    src: &Path,
    names: &Names,
    fit: Fit<'_>,
    run: &Run<'_>,
    span: (f32, f32),
    progress: &mut dyn FnMut(f32),
) -> Result<Decoded, GrapeError> {
    let cancel = run.cancel;
    let (head, len) = read_head(src).map_err(|e| names.read_failed(e))?;
    let format = match proto::sniff(&head) {
        Sniff::NotHeif => return Ok(Decoded::NotHeif),
        Sniff::OtherHeif => return Err(other_heif(names, "its ftyp names another codec")),
        Sniff::Heic => HeifFormat::Heic,
        Sniff::Avif => HeifFormat::Avif,
    };
    let max_source = run.limits.helper.max_source_bytes;
    if len > max_source {
        return Err(names.too_large(format!(
            "the file is {} MiB, over the {} MiB limit for a {} source",
            len / MIB,
            max_source / MIB,
            format.name()
        )));
    }
    let source = std::path::absolute(src).map_err(|e| names.read_failed(io_detail(&e)))?;
    if source.to_str().is_none() {
        // The request is JSON: a path that isn't Unicode can't be sent.
        return Err(names.read_failed("the path isn't valid Unicode".to_string()));
    }

    let work = make_work_dir(run.limits)?;
    let ended = {
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
            fit.check(
                info.width,
                info.height,
                color_of(info.channels),
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
            span,
            progress,
        )
    };
    let (done, info) = match ended {
        Ended::Done(done, Some(info)) => (done, info),
        Ended::Done(_, None) => return Err(helper_stopped("done without an image line")),
        other => return Err(ended_error(other, names, Side::Decode(format))),
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
    Ok(Decoded::Pixels(img, icc))
}

/// JPG, PNG, WebP, TIFF or BMP → HEIC or AVIF (heic-spec §4.2, D41).
#[allow(clippy::too_many_arguments)] // `convert`'s, plus the run.
fn to_heif(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    format: HeifFormat,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    run: &Run<'_>,
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, grape_format(format));
    publish::dest_dir(dest, &names)?;
    let cancel = run.cancel;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    // `Format::Heic` stands in for both formats (see `encode_pixels`).
    let (img, icc) = image::decode(src, from, Format::Heic, &names, cancel)
        .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    progress(0.3);
    encode_pixels(
        job,
        src,
        dest,
        from,
        format,
        img,
        icc,
        ctx,
        progress,
        (0.4, 0.9),
        run,
    )
}

/// The encode half of anything → HEIC or AVIF: `img` (upright, with its
/// ICC profile) from `src`, a file of format `from`, as `format` at `dest`
/// through the helper, at its default quality. Its size must fit one
/// picture of the format ([`Limits::encode_limits`]); it's made 8-bit RGB,
/// or RGBA when it has alpha, its rows are streamed into `input.rgba` with
/// the profile if it describes them ([`stage`]; then `progress(span.0)`),
/// the helper's progress goes on within `span`, and its output must sniff
/// as `format` ([`run_encode`], which [`Encoder`] runs too). It's copied
/// into a temp file in the destination folder and published without
/// replacing anything (then `progress(1.0)`). Raster → HEIC or AVIF, HEIC
/// ⇄ AVIF, and SVG → either (D39) end here.
#[allow(clippy::too_many_arguments)] // The pixels, the target and the run.
pub(crate) fn encode_pixels(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    format: HeifFormat,
    img: DynamicImage,
    icc: Option<Vec<u8>>,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    span: (f32, f32),
    run: &Run<'_>,
) -> Result<JobOutcome, GrapeError> {
    let to = grape_format(format);
    let names = Names::new(src, dest, from, to);
    let dir = publish::dest_dir(dest, &names)?;
    let cancel = run.cancel;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    // Before converting anything.
    fits_one_picture(run.limits, format, img.width(), img.height(), &names)?;
    let img = rgb_or_rgba(img);
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let staged = stage(run, format, &img, icc.as_deref(), &names)?;
    drop(img);
    progress(span.0);

    let mut last = span.0;
    let mut forward = |fraction: f32| {
        if fraction > last {
            last = fraction;
            progress(fraction);
        }
    };
    let (out, _) = run_encode(run, format, &staged, None, &names, span, &mut forward)?;

    publish::ensure_dir(job, ctx, dir, &names)?;
    // From here on, `temp` removes the temp file on every return path.
    let (temp, file) = TempFile::create(dir, to.output_ext())
        .map_err(|e| names.write_failed(format!("creating the temp file: {}", io_detail(&e))))?;
    copy_output(&out, file, cancel).map_err(|e| {
        if cancel.is_cancelled() {
            cancelled()
        } else {
            names.write_failed(format!(
                "writing the {} file: {}",
                format.name(),
                io_detail(&e)
            ))
        }
    })?;
    drop(staged);
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    drop(temp);
    progress(1.0);
    Ok(outcome)
}

/// `img` as 8-bit RGB, or RGBA when it has alpha: what `input.rgba` is
/// made from. 8-bit RGB and RGBA are kept as they are; anything else is
/// converted once (`image::decode` counted an RGBA copy for it, with
/// `Format::Heic` as the target, which stands in for both formats).
fn rgb_or_rgba(img: DynamicImage) -> DynamicImage {
    match img {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) => img,
        other if other.color().has_alpha() => DynamicImage::ImageRgba8(other.into_rgba8()),
        other => DynamicImage::ImageRgb8(other.into_rgb8()),
    }
}

/// `Err` ("… is too large to save as HEIC.") unless a `width`×`height`
/// picture fits one picture of `format` as an encode sends it
/// ([`Limits::encode_limits`]).
fn fits_one_picture(
    limits: &Limits,
    format: HeifFormat,
    width: u32,
    height: u32,
    names: &Names,
) -> Result<(), GrapeError> {
    let pixels = u64::from(width) * u64::from(height);
    let helper_limits = limits.encode_limits(format);
    if pixels > helper_limits.max_encode_pixels
        || width > helper_limits.max_encode_side
        || height > helper_limits.max_encode_side
    {
        return Err(too_large_to_save(
            names,
            format,
            format!(
                "{width}x{height} is over one {} picture ({} pixels, {} a side)",
                match format {
                    HeifFormat::Heic => "HEVC",
                    HeifFormat::Avif => "AV1",
                },
                helper_limits.max_encode_pixels,
                helper_limits.max_encode_side
            ),
        ));
    }
    Ok(())
}

/// Pixels to encode, in a work folder of their own as `input.rgba`.
struct Staged {
    /// Removed, with `input.rgba` and the output, when this is dropped.
    work: WorkDir,
    /// Width × height, for the deadline.
    pixels: u64,
}

/// The first step of every encode: `img`, 8-bit RGB or RGBA, must fit one
/// picture of `format` ([`fits_one_picture`]), then its rows are streamed
/// into `input.rgba` in a new work folder as they are (3 or 4 channels, no
/// copy), with `icc` if it's an RGB profile ([`image::icc_fits`]) no larger
/// than the helper keeps; a profile that isn't is dropped.
fn stage(
    run: &Run<'_>,
    format: HeifFormat,
    img: &DynamicImage,
    icc: Option<&[u8]>,
    names: &Names,
) -> Result<Staged, GrapeError> {
    let cancel = run.cancel;
    let (width, height) = (img.width(), img.height());
    if !matches!(
        img,
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_)
    ) || width == 0
        || height == 0
    {
        return Err(names.encode_failed(format!(
            "the pixels for {} are {width}x{height} {:?}, not RGB8 or RGBA8",
            format.name(),
            img.color()
        )));
    }
    fits_one_picture(run.limits, format, width, height, names)?;
    let max_icc =
        usize::try_from(run.limits.encode_limits(format).max_icc_bytes).unwrap_or(usize::MAX);
    let icc = icc.filter(|profile| image::icc_fits(profile, img) && profile.len() <= max_icc);
    let work = make_work_dir(run.limits)?;
    write_input(&work, img, icc, cancel).map_err(|e| {
        if cancel.is_cancelled() {
            cancelled()
        } else {
            temp_failed(format!("writing the pixels: {}", io_detail(&e)))
        }
    })?;
    Ok(Staged {
        work,
        pixels: u64::from(width) * u64::from(height),
    })
}

/// The helper's run on `staged`: `encode {format, quality}` with its
/// format's caps and a deadline for its pixels, then its output checked
/// ([`check_output`]). Its progress `f` goes to `progress` as `span.0 +
/// (span.1 - span.0) × f`. The output's path (in `staged`'s work folder)
/// and size.
fn run_encode(
    run: &Run<'_>,
    format: HeifFormat,
    staged: &Staged,
    quality: Option<u8>,
    names: &Names,
    span: (f32, f32),
    progress: &mut dyn FnMut(f32),
) -> Result<(PathBuf, u64), GrapeError> {
    let mut on_image = |_: &ImageInfo| -> Result<Duration, Ended> {
        Err(Ended::Crashed("an image line while encoding".to_string()))
    };
    let initial = run.limits.timeout_base.saturating_add(per_megapixel(
        run.limits.encode_per_megapixel,
        staged.pixels,
    ));
    let ended = drive(
        run,
        &staged.work,
        Op::Encode { format, quality },
        initial,
        &mut on_image,
        span,
        progress,
    );
    let done = match ended {
        Ended::Done(done, _) => done,
        other => return Err(ended_error(other, names, Side::Encode(format))),
    };
    let out = staged.work.path().join(proto::names::output(format));
    check_output(&out, done.bytes, format)?;
    Ok((out, done.bytes))
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

/// Streams `img`'s rows into `input.rgba` as they are: 3 channels for
/// 8-bit RGB, 4 for RGBA (anything else is `InvalidInput`), then syncs it.
/// Writing fails once `cancel` is set.
fn write_input(
    work: &WorkDir,
    img: &DynamicImage,
    icc: Option<&[u8]>,
    cancel: &CancelToken,
) -> io::Result<()> {
    let (channels, raw): (u8, &[u8]) = match img {
        DynamicImage::ImageRgb8(rgb) => (3, rgb.as_raw()),
        DynamicImage::ImageRgba8(rgba) => (4, rgba.as_raw()),
        _ => return Err(io::Error::from(io::ErrorKind::InvalidInput)),
    };
    let row_len = usize::try_from(img.width())
        .ok()
        .and_then(|w| w.checked_mul(usize::from(channels)))
        .filter(|&len| len > 0)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(work.path().join(proto::names::INPUT))?;
    let out = BufWriter::with_capacity(WRITE_BUFFER, Cancellable::new(file, cancel));
    let icc_len = icc
        .map_or(Ok(0), |icc| u32::try_from(icc.len()))
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let header = PixelHeader {
        width: img.width(),
        height: img.height(),
        channels,
        icc_len,
    };
    let mut writer = PixelWriter::begin(out, &header, icc)?;
    for row in raw.chunks_exact(row_len) {
        writer.row(row)?;
    }
    let out = writer.finish()?;
    let file = out.into_inner().map_err(|e| e.into_error())?.into_inner();
    file.sync_all()
}

/// Reads `pixels.rgba`, which must be exactly what the `image` line
/// announced (and [`image::pixels_fit`] approved), into one buffer
/// ([`helper::read_pixel_file`]).
fn read_decoded(
    work: &WorkDir,
    info: &ImageInfo,
    cancel: &CancelToken,
) -> Result<(DynamicImage, Option<Vec<u8>>), GrapeError> {
    let announced = helper::Announced {
        width: info.width,
        height: info.height,
        channels: info.channels,
        icc_bytes: info.icc_bytes,
    };
    helper::read_pixel_file(&work.path().join(proto::names::PIXELS), &announced, cancel).map_err(
        |e| match e {
            helper::PixelFileError::Cancelled => cancelled(),
            helper::PixelFileError::Bad(detail) => helper_stopped(&detail),
        },
    )
}

/// The helper's output (`out.heic` or `out.avif`) must be the `bytes` it
/// reported, and sniff as `format`.
fn check_output(out: &Path, bytes: u64, format: HeifFormat) -> Result<(), GrapeError> {
    let (head, len) = read_head(out).map_err(|e| helper_stopped(&format!("its output: {e}")))?;
    if bytes == 0 || len != bytes {
        return Err(helper_stopped(&format!(
            "its output is {len} bytes, and it said {bytes}"
        )));
    }
    if proto::sniff(&head).format() != Some(format) {
        return Err(helper_stopped(&format!(
            "its output isn't {}",
            format.name()
        )));
    }
    Ok(())
}

/// Reads the helper's output, `bytes` long (as [`check_output`] found it),
/// into memory. Reading fails once `cancel` is set, and a file that isn't
/// `bytes` long now is `UnexpectedEof` or `InvalidData`.
fn read_output(out: &Path, bytes: u64, cancel: &CancelToken) -> io::Result<Vec<u8>> {
    let len = usize::try_from(bytes).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    let mut data = Vec::new();
    data.try_reserve_exact(len)
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    // One byte past what it said, to tell a file that grew.
    Cancellable::new(File::open(out)?, cancel)
        .take(bytes.saturating_add(1))
        .read_to_end(&mut data)?;
    match data.len().cmp(&len) {
        std::cmp::Ordering::Equal => Ok(data),
        std::cmp::Ordering::Less => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        std::cmp::Ordering::Greater => Err(io::Error::from(io::ErrorKind::InvalidData)),
    }
}

/// Copies the helper's output into the temp `file` in the destination
/// folder, then syncs it. Reading and writing fail once `cancel` is set.
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
    /// its container allows, or past its own heap bound
    /// ([`OVER_HEAP_EXIT`]). The detail.
    OverMemory(String),
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
/// - An encode sends its format's caps ([`Limits::encode_limits`]), any
///   other op [`Limits::helper`].
fn drive(
    run: &Run<'_>,
    work: &WorkDir,
    op: Op,
    initial: Duration,
    on_image: &mut dyn FnMut(&ImageInfo) -> Result<Duration, Ended>,
    span: (f32, f32),
    progress: &mut dyn FnMut(f32),
) -> Ended {
    let limits = match op {
        Op::Encode { format, .. } => run.limits.encode_limits(format),
        Op::Probe {} | Op::Decode { .. } => run.limits.helper,
    };
    let request = Request {
        protocol: proto::PROTOCOL,
        work_dir: work.path().to_path_buf(),
        op,
        limits,
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
    // The Mac worker's heap bound (D41); grape-heif.exe never exits so.
    if status.code() == Some(OVER_HEAP_EXIT) {
        return Ended::OverMemory(format!(
            "the HEIF helper went past its {} MiB heap bound",
            run.limits.helper_memory / MIB
        ));
    }
    match (done, failure, status.success()) {
        (Some(done), None, true) => Ended::Done(done, image),
        (None, Some(failure), false) => Ended::Failed(failure),
        (done, failure, success) => {
            let limit = run.limits.helper_memory;
            if helper::near_memory_cap(running.peak_memory(), limit) {
                Ended::OverMemory(format!(
                    "the HEIF helper ended near its {} MiB memory cap",
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

/// Which way a job converts, and the format the helper reads or writes,
/// for the messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The helper decodes the format (the source's content, by its sniff).
    Decode(HeifFormat),
    /// The helper encodes the format (the target).
    Encode(HeifFormat),
}

/// A run that didn't end with `done`, in Grape's words.
fn ended_error(ended: Ended, names: &Names, side: Side) -> GrapeError {
    let source = &names.source;
    match ended {
        Ended::Failed(failure) => report(failure, names, side),
        Ended::Cancelled => cancelled(),
        Ended::TimedOut => GrapeError::new(
            ErrorKind::DecodeFailed,
            format!("{source} took too long, so Grape stopped."),
            "the HEIF helper ran past its time",
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
        // `drive` returns `Done` to its caller, which handles it first.
        Ended::Done(..) => helper_stopped("it ended in an unexpected way"),
    }
}

/// The helper's failure in Grape's words, naming the file.
fn report(failure: Failure, names: &Names, side: Side) -> GrapeError {
    let source = &names.source;
    let detail = format!("HEIF helper: {:?}: {}", failure.kind, failure.detail);
    let decode_failed =
        |message: String| GrapeError::new(ErrorKind::DecodeFailed, message, detail.clone());
    match (failure.kind, side) {
        (FailKind::Protocol, _) => GrapeError::new(
            ErrorKind::NotYetSupported,
            "Grape's image helper doesn't match this version of Grape.",
            detail,
        ),
        (FailKind::Read, _) => names.read_failed(detail),
        (FailKind::Write, _) => temp_failed(detail),
        (FailKind::NotHeif | FailKind::Damaged, Side::Decode(_)) => names.decode_failed(detail),
        // HEIC's other profiles are "a kind of HEIF image", as other codecs;
        // AVIF's (or what ImageIO typed as something else) "of AVIF image".
        (FailKind::Unsupported, Side::Decode(format)) => decode_failed(format!(
            "{source} is a kind of {} image Grape can't read yet.",
            match format {
                HeifFormat::Heic => "HEIF",
                HeifFormat::Avif => "AVIF",
            }
        )),
        (FailKind::NoImage, Side::Decode(_)) => {
            decode_failed(format!("{source} has no image to convert."))
        }
        (FailKind::TooLarge, Side::Decode(_)) => names.too_large(detail),
        (FailKind::TooLarge, Side::Encode(format)) => too_large_to_save(names, format, detail),
        (FailKind::NoEncoder, Side::Encode(format)) => GrapeError::new(
            ErrorKind::NotYetSupported,
            no_encoder(format, cfg!(target_os = "macos")),
            detail,
        ),
        (FailKind::Encode, Side::Encode(_)) => names.encode_failed(detail),
        // `internal`, and a kind that makes no sense for this side.
        _ => helper_stopped(&detail),
    }
}

/// "… is a kind of HEIF image Grape can't read yet.": HEIF with a codec no
/// helper reads, for an engine that tells it apart itself too (HEIC Remove
/// Metadata and Compress, D28). The detail is `HEIF: {detail}`.
pub(crate) fn other_heif(names: &Names, detail: &str) -> GrapeError {
    GrapeError::new(
        ErrorKind::DecodeFailed,
        format!(
            "{} is a kind of HEIF image Grape can't read yet.",
            names.source
        ),
        format!("HEIF: {detail}"),
    )
}

/// What a helper that can't write `format` (`noEncoder`) means, in
/// Grape's words. On a Mac (`mac`) the helper is Grape itself and the
/// machine decides: ImageIO writes HEIC only with a HEVC encoder (CI's VMs
/// have none), and a build without `avif-encode` writes no AVIF. On Windows
/// `grape-heif.exe` always has both encoders, so one without isn't the
/// helper this Grape came with.
fn no_encoder(format: HeifFormat, mac: bool) -> String {
    if mac {
        format!("This Mac can't write {} files.", format.name())
    } else {
        format!(
            "Grape's image helper can't write {} files. Reinstall Grape.",
            format.name()
        )
    }
}

/// The image is larger than the one picture Grape encodes in `format`.
fn too_large_to_save(names: &Names, format: HeifFormat, detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::EncodeFailed,
        format!(
            "{} is too large to save as {}.",
            names.source,
            format.name()
        ),
        detail,
    )
}

/// The work folder or a file in it couldn't be made or written.
fn temp_failed(detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::WriteFailed,
        "Grape couldn't write its temp files.",
        format!("HEIF work folder: {detail}"),
    )
}

/// The helper couldn't be started or contained.
fn couldnt_start(detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::NotYetSupported,
        "Grape's image helper couldn't start.",
        format!("HEIF helper: {detail}"),
    )
}

/// The helper ended without a consistent result.
fn helper_stopped(detail: &str) -> GrapeError {
    GrapeError::new(
        ErrorKind::DecodeFailed,
        "Grape's image helper stopped unexpectedly.",
        format!("HEIF helper: {detail}"),
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
pub(crate) mod tests {
    //! The engine with the test binary itself as a fake helper: it runs
    //! only [`fake_helper`] (`--exact … --ignored`) with [`FAKE`] set to a
    //! mode, and serves the request with `grape_heif_proto::serve` and
    //! [`FakeCodec`], whose "HEIC" (and "AVIF") is real HEIF around raw
    //! pixels ([`fake_file`]): its primary item's data, found through `pitm`
    //! and `iloc` ([`parse_fake`]), so a file another engine rewrote around
    //! it still decodes. An encode records the quality it was asked for
    //! ([`fake_quality`]).
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
    use ::image::{
        ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, RgbImage,
        RgbaImage,
    };
    use grape_core::{plan, JobOutput, Operation, Settings};
    use grape_heif_proto::{Codec, FormatCaps, PixelSink, Pixels};

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

    const GIB: u64 = 1024 * MIB;

    /// An `ftyp` box: major brand `heic`, compatible `mif1 heic`.
    const FTYP: [u8; 24] = *b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";

    /// The same for AVIF: major brand `avif`, compatible `mif1 avif`.
    const FTYP_AVIF: [u8; 24] = *b"\0\0\0\x18ftypavif\0\0\0\0mif1avif";

    /// 32 bytes: an `ftyp` with major `avci`, compatible `mif1 avci`, then
    /// zeros (grape-heif-fixtures' `avci-brand.heic`, D41): AVC, a HEIF
    /// codec no helper reads.
    const AVCI_BRAND: [u8; 32] = *b"\0\0\0\x18ftypavci\0\0\0\0mif1avci\0\0\0\0\0\0\0\0";

    /// The start of a fake image's coded data, the primary item's payload.
    const PAYLOAD_MAGIC: &[u8; 4] = b"FAKE";

    /// An 8-bit 4:2:0 Main profile `hvcC` record (ISO/IEC 14496-15
    /// 8.3.3.1), 23 bytes, with no parameter sets: version 1, Main, level
    /// 3.0, `lengthSizeMinusOne` 3.
    const HVCC: [u8; 23] = [
        1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 90, 0xF0, 0x00, 0xFC, 0xFD, 0xF8, 0xF8, 0, 0,
        0x0F, 0,
    ];

    /// An 8-bit 4:2:0 Main profile `av1C` record (AV1-ISOBMFF 2.3.3), with
    /// no OBUs.
    const AV1C: [u8; 4] = [0x81, 0x00, 0x0C, 0x00];

    /// A box: its 32-bit size, its type, its body.
    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        let mut bytes = size.to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes
    }

    /// A full box: [`mp4_box`] with a version and 24 bits of flags first.
    fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut full = (u32::from(version) << 24 | flags).to_be_bytes().to_vec();
        full.extend_from_slice(body);
        mp4_box(kind, &full)
    }

    /// The `meta` box of a fake file whose one image item, the primary
    /// one, is `item` (`hvc1` or `av01`) with `config` (`hvcC` or `av1C`),
    /// `width`×`height`, coded in `length` bytes at file offset `offset`.
    fn fake_meta(
        item: &[u8; 4],
        config: (&[u8; 4], &[u8]),
        width: u32,
        height: u32,
        offset: u32,
        length: u32,
    ) -> Vec<u8> {
        let mut hdlr = vec![0; 4];
        hdlr.extend_from_slice(b"pict");
        hdlr.extend_from_slice(&[0; 13]); // reserved, and an empty name
        let pitm = 1u16.to_be_bytes();
        let mut infe = 1u16.to_be_bytes().to_vec(); // item_ID
        infe.extend_from_slice(&0u16.to_be_bytes()); // item_protection_index
        infe.extend_from_slice(item);
        infe.push(0); // an empty item_name
        let mut iinf = 1u16.to_be_bytes().to_vec();
        iinf.extend_from_slice(&full_box(b"infe", 2, 0, &infe));
        // Version 1: 4-byte offsets and lengths, no base offset or index.
        let mut iloc = vec![0x44, 0x00];
        iloc.extend_from_slice(&1u16.to_be_bytes()); // item_count
        iloc.extend_from_slice(&1u16.to_be_bytes()); // item_ID
        iloc.extend_from_slice(&0u16.to_be_bytes()); // construction_method 0
        iloc.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
        iloc.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        iloc.extend_from_slice(&offset.to_be_bytes());
        iloc.extend_from_slice(&length.to_be_bytes());
        let mut ispe = width.to_be_bytes().to_vec();
        ispe.extend_from_slice(&height.to_be_bytes());
        let mut ipco = mp4_box(config.0, config.1);
        ipco.extend_from_slice(&full_box(b"ispe", 0, 0, &ispe));
        // Item 1: property 1 (the codec's, essential), then 2 (`ispe`).
        let mut ipma = 1u32.to_be_bytes().to_vec();
        ipma.extend_from_slice(&1u16.to_be_bytes());
        ipma.extend_from_slice(&[2, 0x81, 0x02]);
        let mut iprp = mp4_box(b"ipco", &ipco);
        iprp.extend_from_slice(&full_box(b"ipma", 0, 0, &ipma));
        let mut meta = full_box(b"hdlr", 0, 0, &hdlr);
        meta.extend_from_slice(&full_box(b"pitm", 0, 0, &pitm));
        meta.extend_from_slice(&full_box(b"iinf", 0, 0, &iinf));
        meta.extend_from_slice(&full_box(b"iloc", 1, 0, &iloc));
        meta.extend_from_slice(&mp4_box(b"iprp", &iprp));
        full_box(b"meta", 0, 0, &meta)
    }

    /// A fake HEIC file: [`fake_image`]'s.
    pub(crate) fn fake_heic(
        width: u32,
        height: u32,
        channels: u8,
        rows: &[u8],
        icc: Option<&[u8]>,
    ) -> Vec<u8> {
        fake_image(HeifFormat::Heic, width, height, channels, rows, icc)
    }

    /// A fake HEIC or AVIF file, as the fake helper writes one: real HEIF
    /// around raw pixels ([`fake_file`]), with no quality recorded.
    pub(crate) fn fake_image(
        format: HeifFormat,
        width: u32,
        height: u32,
        channels: u8,
        rows: &[u8],
        icc: Option<&[u8]>,
    ) -> Vec<u8> {
        fake_file(format, width, height, channels, rows, icc, None)
    }

    /// A fake HEIC or AVIF file, real HEIF that sniffs as `format`: `ftyp`
    /// ([`FTYP`] or [`FTYP_AVIF`]), then `meta` (`hdlr` `pict`, `pitm` 1,
    /// `iinf` with one `infe` v2, item 1 of type `hvc1` or `av01`, `iloc`
    /// v1 with one extent, `iprp` with `ipco` [`HVCC`] or [`AV1C`] and
    /// `ispe`, and `ipma` 1 → [1, 2]), then `mdat`, whose data is item 1's:
    /// [`PAYLOAD_MAGIC`], the quality it was encoded at (0: the default),
    /// the width and height (u32 LE), the channels, the ICC profile's
    /// length (u32 LE), the profile and the rows.
    fn fake_file(
        format: HeifFormat,
        width: u32,
        height: u32,
        channels: u8,
        rows: &[u8],
        icc: Option<&[u8]>,
        quality: Option<u8>,
    ) -> Vec<u8> {
        let icc = icc.unwrap_or_default();
        let mut payload = PAYLOAD_MAGIC.to_vec();
        payload.push(quality.unwrap_or(0));
        payload.extend_from_slice(&width.to_le_bytes());
        payload.extend_from_slice(&height.to_le_bytes());
        payload.push(channels);
        payload.extend_from_slice(&u32::try_from(icc.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(icc);
        payload.extend_from_slice(rows);
        let (ftyp, item, config) = match format {
            HeifFormat::Heic => (FTYP, b"hvc1", (b"hvcC", &HVCC[..])),
            HeifFormat::Avif => (FTYP_AVIF, b"av01", (b"av1C", &AV1C[..])),
        };
        let length = u32::try_from(payload.len()).unwrap();
        // The offset doesn't change the meta box's size (4 bytes either
        // way), so it's measured with 0 first.
        let meta_len = fake_meta(item, config, width, height, 0, length).len();
        let offset = u32::try_from(ftyp.len() + meta_len + 8).unwrap();
        let mut bytes = ftyp.to_vec();
        bytes.extend_from_slice(&fake_meta(item, config, width, height, offset, length));
        bytes.extend_from_slice(&mp4_box(b"mdat", &payload));
        bytes
    }

    /// The quality a file the fake helper encoded was encoded at: `None`
    /// for its default (no quality in the request), or for a file that
    /// isn't one of the fake's ([`parse_fake`]).
    pub(crate) fn fake_quality(heic: &[u8]) -> Option<u8> {
        parse_fake(heic)?.quality
    }

    /// What a fake HEIC or AVIF file holds.
    #[derive(Debug)]
    pub(crate) struct FakeImage {
        /// By the primary item's type: `hvc1` is HEIC, `av01` AVIF.
        pub(crate) format: HeifFormat,
        pub(crate) width: u32,
        pub(crate) height: u32,
        pub(crate) channels: u8,
        pub(crate) icc: Option<Vec<u8>>,
        pub(crate) rows: Vec<u8>,
        /// The quality it was encoded at; `None` for the default.
        pub(crate) quality: Option<u8>,
    }

    /// The boxes in `data`, in order: each one's type and body. `None` when
    /// one runs past the end. A size of 0 runs to the end, 1 is 64-bit.
    fn mp4_boxes(data: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
        let mut boxes = Vec::new();
        let mut rest = data;
        while !rest.is_empty() {
            let size = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
            let kind: [u8; 4] = rest.get(4..8)?.try_into().ok()?;
            let (len, header) = match size {
                0 => (rest.len(), 8),
                1 => {
                    let large = u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?);
                    (usize::try_from(large).ok()?, 16)
                }
                n => (usize::try_from(n).ok()?, 8),
            };
            boxes.push((kind, rest.get(header..len)?));
            rest = &rest[len..];
        }
        Some(boxes)
    }

    /// The body of the first `kind` box among `boxes`.
    fn find<'a>(boxes: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> Option<&'a [u8]> {
        boxes.iter().find(|(k, _)| k == kind).map(|&(_, body)| body)
    }

    /// Reads big-endian unsigned fields in turn.
    struct Fields<'a>(&'a [u8]);

    impl Fields<'_> {
        /// The next `n` bytes (0 to 8) as a number.
        fn uint(&mut self, n: usize) -> Option<u64> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head.iter().fold(0, |value, &b| value << 8 | u64::from(b)))
        }

        fn take(&mut self, n: usize) -> Option<&[u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
    }

    /// The primary item's type and data in a HEIF file: through `pitm`,
    /// `iinf` and `iloc` (versions 0 to 2, any field sizes, every extent in
    /// order), from the file (construction method 0) or `idat` (1). So a
    /// file another tool rewrote, with more items and boxes, still reads.
    fn primary_item(bytes: &[u8]) -> Option<([u8; 4], Vec<u8>)> {
        let top = mp4_boxes(bytes)?;
        let meta = mp4_boxes(find(&top, b"meta")?.get(4..)?)?;
        let pitm = find(&meta, b"pitm")?;
        let primary = Fields(pitm.get(4..)?).uint(if pitm[0] == 0 { 2 } else { 4 })?;

        let iinf = find(&meta, b"iinf")?;
        let count_len = if iinf[0] == 0 { 2 } else { 4 };
        let mut item_type = None;
        for (kind, infe) in mp4_boxes(iinf.get(4 + count_len..)?)? {
            if &kind != b"infe" || infe[0] < 2 {
                continue;
            }
            let mut fields = Fields(infe.get(4..)?);
            let id = fields.uint(if infe[0] == 2 { 2 } else { 4 })?;
            fields.uint(2)?; // item_protection_index
            if id == primary {
                item_type = Some(fields.take(4)?.try_into().ok()?);
            }
        }

        let iloc = find(&meta, b"iloc")?;
        let version = iloc[0];
        let mut fields = Fields(iloc.get(4..)?);
        let sizes = fields.uint(1)?;
        let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 15) as usize);
        let sizes = fields.uint(1)?;
        let base_size = (sizes >> 4) as usize;
        let index_size = if version >= 1 {
            (sizes & 15) as usize
        } else {
            0
        };
        let id_len = if version < 2 { 2 } else { 4 };
        let count = fields.uint(id_len)?;
        for _ in 0..count {
            let id = fields.uint(id_len)?;
            let method = if version >= 1 {
                fields.uint(2)? & 15
            } else {
                0
            };
            fields.uint(2)?; // data_reference_index
            let base = fields.uint(base_size)?;
            let extents = fields.uint(2)?;
            let mut data = Vec::new();
            for _ in 0..extents {
                fields.uint(index_size)?;
                let offset = usize::try_from(base + fields.uint(offset_size)?).ok()?;
                let length = usize::try_from(fields.uint(length_size)?).ok()?;
                let source = match method {
                    0 => bytes,
                    1 => find(&meta, b"idat")?,
                    _ => return None,
                };
                // A length of 0 runs to the end.
                let end = if length == 0 {
                    source.len()
                } else {
                    offset.checked_add(length)?
                };
                data.extend_from_slice(source.get(offset..end)?);
            }
            if id == primary {
                return Some((item_type?, data));
            }
        }
        None
    }

    /// What a fake file holds ([`fake_file`]): `None` for anything else.
    pub(crate) fn parse_fake(bytes: &[u8]) -> Option<FakeImage> {
        let (item_type, payload) = primary_item(bytes)?;
        let format = match &item_type {
            b"hvc1" => HeifFormat::Heic,
            b"av01" => HeifFormat::Avif,
            _ => return None,
        };
        let rest = payload.strip_prefix(PAYLOAD_MAGIC)?;
        let quality = (*rest.first()? != 0).then_some(rest[0]);
        let rest = &rest[1..];
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
            format,
            width,
            height,
            channels,
            icc: (!icc.is_empty()).then(|| icc.to_vec()),
            rows: rows.to_vec(),
            quality,
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
        /// Both formats both ways; `no-heic-encoder` can't write HEIC (a Mac
        /// without a HEVC encoder).
        fn caps(&self) -> Caps {
            let both = FormatCaps {
                decode: true,
                encode: true,
            };
            let heic = FormatCaps {
                encode: self.mode != "no-heic-encoder",
                ..both
            };
            Caps {
                heic,
                avif: both,
                codec: "fake".to_string(),
            }
        }

        fn decode(
            &self,
            source: &Path,
            format: HeifFormat,
            _limits: &proto::Limits,
            out: &mut dyn PixelSink,
        ) -> Result<(), Failure> {
            self.check_mode()?;
            let bytes = fs::read(source).map_err(|_| Failure::new(FailKind::Read, "fake"))?;
            let image =
                parse_fake(&bytes).ok_or_else(|| Failure::new(FailKind::Damaged, "fake"))?;
            // `serve` hands over what it sniffed: the content's format.
            if image.format != format {
                return Err(Failure::new(
                    FailKind::Internal,
                    format!("fake: told {format:?} for {:?}", image.format),
                ));
            }
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
            format: HeifFormat,
            quality: Option<u8>,
            _limits: &proto::Limits,
        ) -> Result<Vec<u8>, Failure> {
            self.check_mode()?;
            match self.mode.as_str() {
                "slow" | "slow-encode" => thread::sleep(Duration::from_secs(60)),
                "noencoder" => return Err(Failure::new(FailKind::NoEncoder, "fake")),
                _ => {}
            }
            // The format and the quality asked for, so the tests can see
            // them ([`fake_quality`]).
            Ok(fake_file(
                format,
                pixels.width,
                pixels.height,
                pixels.channels,
                &pixels.data,
                pixels.icc.as_deref(),
                quality,
            ))
        }
    }

    /// Not a test: the helper the tests start. The test binary runs only
    /// this "test" (`--exact … --ignored`) with [`FAKE`] set to a mode.
    ///
    /// The request is read first. A probe is answered as [`FakeCodec`]
    /// answers it (so `no-heic-encoder` still can't write HEIC) with none of
    /// the mode's misbehavior, before or after `serve`: a job that probes
    /// first ([`Encoder::check_encode`]) still reaches the decode or encode
    /// its mode is for. `probe-<mode>` is the other way round: `<mode>` on
    /// the probe only, and every other request served as `ok`.
    #[test]
    #[ignore = "a fake image helper, run by the tests below"]
    fn fake_helper() {
        let Ok(mode) = std::env::var(FAKE) else {
            return;
        };
        let mut request = Vec::new();
        io::stdin()
            .lock()
            .take(proto::MAX_REQUEST_BYTES + 1)
            .read_to_end(&mut request)
            .unwrap();
        let probe = serde_json::from_slice::<serde_json::Value>(&request)
            .is_ok_and(|request| request["op"]["kind"] == "probe");
        let (mode, misbehave) = match mode.strip_prefix("probe-") {
            Some(on_probe) if probe => (on_probe.to_string(), true),
            Some(_) => ("ok".to_string(), false),
            None => (mode, !probe),
        };
        let say = |line: &str| {
            let mut out = io::stdout().lock();
            out.write_all(line.as_bytes()).unwrap();
            out.write_all(b"\n").unwrap();
            out.flush().unwrap();
        };
        let wait = || thread::sleep(Duration::from_secs(60));
        match mode.as_str() {
            _ if !misbehave => {}
            "crash" => {
                say(r#"{"progress":0.5}"#);
                std::process::exit(3);
            }
            "protocol" => {
                say(r#"{"error":{"kind":"protocol","detail":"fake: protocol 3"}}"#);
                std::process::exit(1);
            }
            // The Mac worker past its heap bound (D41).
            "over-heap" => {
                say(r#"{"progress":0.1}"#);
                std::process::exit(OVER_HEAP_EXIT);
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
        // What's past the request's cap too, so `serve` refuses a long one.
        let input = Cursor::new(request).chain(io::stdin().lock());
        let done = proto::serve(&codec, input, io::stdout());
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
        // The output it wrote, whichever format was asked for.
        let output = [proto::names::OUTPUT_HEIC, proto::names::OUTPUT_AVIF]
            .into_iter()
            .find(|name| here.join(name).is_file());
        match mode.as_str() {
            _ if !misbehave => {}
            "bad-pixels" => append(proto::names::PIXELS),
            "bad-output" => append(output.unwrap()),
            // The same length, but the other format's ftyp.
            "other-output" => {
                let path = here.join(output.unwrap());
                let mut bytes = fs::read(&path).unwrap();
                let other = if bytes.starts_with(&FTYP) {
                    FTYP_AVIF
                } else {
                    FTYP
                };
                bytes[..other.len()].copy_from_slice(&other);
                fs::write(&path, bytes).unwrap();
            }
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
    pub(crate) fn fake(mode: &str) -> Launch {
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
    pub(crate) fn nowhere() -> Launch {
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

    /// `format`'s fake ftyp.
    fn ftyp(format: HeifFormat) -> [u8; 24] {
        match format {
            HeifFormat::Heic => FTYP,
            HeifFormat::Avif => FTYP_AVIF,
        }
    }

    #[test]
    fn every_raster_pair_converts_both_ways_for_heic_and_avif() {
        let rows = gradient(6, 4, 3);
        for format in HeifFormat::ALL {
            let heif = grape_format(format);
            let ext = heif.key();
            let bytes = fake_image(format, 6, 4, 3, &rows, None);
            for to in image::RASTER {
                let case = case(&format!("photo.{ext}"), &bytes, to);
                let ran = case.run(&fake("ok"));
                let outcome = ran.result.unwrap();
                assert_eq!(outcome.outputs, vec![case.dest()], "{format:?} → {to:?}");
                assert!(!outcome.renamed);
                let out = ::image::open(case.dest()).unwrap().to_rgb8();
                assert_eq!(out.dimensions(), (6, 4));
                let error = mean_error(out.as_raw(), &rows);
                assert!(
                    error <= if to == Format::Jpg { 8.0 } else { 0.0 },
                    "{format:?} → {to:?}: {error}"
                );
                assert_monotone(&ran.progress);
                assert!(ran.progress.contains(&0.1), "{:?}", ran.progress);
                assert_eq!(ran.progress.last(), Some(&1.0));
                case.check();
            }
            for from in image::RASTER {
                let bytes = raster(6, 4, &rows, image_format(from));
                let case = case(&format!("photo.{}", from.key()), &bytes, heif);
                let ran = case.run(&fake("ok"));
                let outcome = ran.result.unwrap();
                assert_eq!(outcome.outputs, vec![case.dest()], "{from:?} → {format:?}");
                assert!(case
                    .dest()
                    .to_string_lossy()
                    .ends_with(&format!("photo.{ext}")));
                let written = fs::read(case.dest()).unwrap();
                // The fake writes the format it's asked for.
                assert!(written.starts_with(&ftyp(format)), "{from:?} → {format:?}");
                let out = parse_fake(&written).unwrap();
                assert_eq!(out.format, format);
                assert_eq!((out.width, out.height, out.channels), (6, 4, 3), "{from:?}");
                let error = mean_error(&out.rows, &rows);
                assert!(
                    error <= if from == Format::Jpg { 8.0 } else { 0.0 },
                    "{from:?} → {format:?}: {error}"
                );
                assert_monotone(&ran.progress);
                assert!(ran.progress.contains(&0.4), "{:?}", ran.progress);
                assert_eq!(ran.progress.last(), Some(&1.0));
                case.check();
            }
        }
    }

    #[test]
    fn heic_and_avif_convert_into_each_other_in_two_runs() {
        let rows = gradient(5, 3, 4);
        let profile = rgb_profile();
        for (from, to) in [
            (HeifFormat::Heic, HeifFormat::Avif),
            (HeifFormat::Avif, HeifFormat::Heic),
        ] {
            let bytes = fake_image(from, 5, 3, 4, &rows, Some(&profile));
            let name = format!("photo.{}", grape_format(from).key());
            let case = case(&name, &bytes, grape_format(to));
            let contain = FakeContain::new(&case, false, None);
            let mut launch = fake("ok");
            launch.contain = Some(contain.clone());
            let ran = case.run(&launch);
            let outcome = ran.result.unwrap();
            assert_eq!(outcome.outputs, vec![case.dest()], "{from:?} → {to:?}");
            // A decode, then an encode: two helpers.
            assert_eq!(contain.calls.load(Ordering::SeqCst), 2);
            let out = parse_fake(&fs::read(case.dest()).unwrap()).unwrap();
            assert_eq!(out.format, to);
            assert_eq!((out.width, out.height, out.channels), (5, 3, 4));
            assert_eq!(out.rows, rows, "the alpha is kept");
            assert_eq!(out.icc, Some(profile.clone()), "the profile is kept");
            assert_monotone(&ran.progress);
            for at in [0.05, 0.45, 0.5, 1.0] {
                assert!(ran.progress.contains(&at), "{at}: {:?}", ran.progress);
            }
            case.check();
        }

        // Rule 3: a .heic that's a PNG is read by the raster engine, and only
        // the encode starts a helper.
        let png_rows = gradient(4, 4, 3);
        let source = png(4, 4, &png_rows, ExtendedColorType::Rgb8, None);
        let case = case("photo.heic", &source, Format::Avif);
        let contain = FakeContain::new(&case, false, None);
        let mut launch = fake("ok");
        launch.contain = Some(contain.clone());
        case.run(&launch).result.unwrap();
        assert_eq!(contain.calls.load(Ordering::SeqCst), 1);
        let out = parse_fake(&fs::read(case.dest()).unwrap()).unwrap();
        assert_eq!((out.format, out.channels), (HeifFormat::Avif, 3));
        assert_eq!(out.rows, png_rows);
        case.check();
    }

    #[test]
    fn heic_to_avif_fails_cleanly_in_either_run() {
        let heic = fake_heic(2, 2, 3, &[5; 12], None);
        // The decode fails: nothing is encoded.
        let case_decode = case("photo.heic", &heic, Format::Avif);
        let ran = case_decode.run(&fake("error-damaged"));
        case_decode.refused(
            ran,
            ErrorKind::DecodeFailed,
            "photo.heic isn't a readable HEIC image.",
        );
        // The encode fails (a Mac whose worker has no AVIF encoder).
        let case_encode = case("photo.heic", &heic, Format::Avif);
        let ran = case_encode.run(&fake("noencoder"));
        case_encode.refused(
            ran,
            ErrorKind::NotYetSupported,
            &no_encoder(HeifFormat::Avif, cfg!(target_os = "macos")),
        );
        // An AVIF to a Mac that can't write HEIC (CI's VMs).
        let avif = fake_image(HeifFormat::Avif, 2, 2, 3, &[5; 12], None);
        let case_heic = case("photo.avif", &avif, Format::Heic);
        let ran = case_heic.run(&fake("noencoder"));
        case_heic.refused(
            ran,
            ErrorKind::NotYetSupported,
            &no_encoder(HeifFormat::Heic, cfg!(target_os = "macos")),
        );
        // Cancelled while the second run encodes.
        let case_cancel = case("photo.avif", &avif, Format::Heic);
        let ran = case_cancel.run_with(
            &fake("slow-encode"),
            &case_cancel.limits(),
            CancelAt::Progress(0.5),
        );
        let at = ran.cancelled_at.unwrap();
        assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
        assert_monotone(&ran.progress);
        case_cancel.refused(ran, ErrorKind::Cancelled, "Cancelled. Nothing was saved.");
    }

    #[test]
    fn this_engine_takes_exactly_its_22_pairs() {
        let pairs: Vec<(Format, Format)> = Format::ALL
            .into_iter()
            .flat_map(|from| Format::ALL.into_iter().map(move |to| (from, to)))
            .filter(|&(from, to)| supports(from, to))
            .collect();
        assert_eq!(pairs.len(), 22);
        for heif in HEIF {
            for raster in image::RASTER {
                assert!(pairs.contains(&(heif, raster)));
                assert!(pairs.contains(&(raster, heif)));
            }
            assert!(!pairs.contains(&(heif, heif)));
            assert!(!pairs.contains(&(heif, Format::Pdf)), "pdf::create's");
        }
        assert!(pairs.contains(&(Format::Heic, Format::Avif)));
        assert!(pairs.contains(&(Format::Avif, Format::Heic)));
    }

    /// `noEncoder` names the machine on a Mac and the helper elsewhere: on
    /// Windows it's a `grape-heif.exe` that isn't this Grape's (D41).
    #[test]
    fn no_encoder_says_this_mac_only_on_a_mac() {
        for (format, name) in [(HeifFormat::Heic, "HEIC"), (HeifFormat::Avif, "AVIF")] {
            assert_eq!(
                no_encoder(format, true),
                format!("This Mac can't write {name} files.")
            );
            assert_eq!(
                no_encoder(format, false),
                format!("Grape's image helper can't write {name} files. Reinstall Grape.")
            );
        }
    }

    #[test]
    fn the_dispatch_needs_the_helper() {
        let with_heif = SidecarPaths {
            heif: Some(PathBuf::from("grape-heif")),
            svg_worker: None,
            ..SidecarPaths::default()
        };
        let none = SidecarPaths::default();
        let convert = Operation::convert;
        let runs = |from: Format, to: Format, sidecars: &SidecarPaths| {
            crate::supports_with(&convert(to), &[Some(from)], sidecars)
        };
        for heif in HEIF {
            for raster in image::RASTER {
                for (from, to) in [(heif, raster), (raster, heif)] {
                    assert!(runs(from, to, &with_heif), "{from:?} → {to:?}");
                    assert!(!runs(from, to, &none), "{from:?} → {to:?}");
                    assert!(!crate::supports(&convert(to), Some(from)));
                }
            }
            // pdf::create, D37 and D41.
            assert!(runs(heif, Format::Pdf, &with_heif), "{heif:?}");
            assert!(!runs(heif, Format::Pdf, &none), "{heif:?}");
        }
        // SVG → HEIC or AVIF needs the SVG worker too (D39).
        for to in HEIF {
            assert!(!runs(Format::Svg, to, &with_heif), "{to:?}");
        }
        for (from, to) in [(Format::Heic, Format::Avif), (Format::Avif, Format::Heic)] {
            assert!(runs(from, to, &with_heif));
            assert!(!runs(from, to, &none));
        }
        // Over every pair, the helper adds exactly the 22, HEIC → PDF and
        // AVIF → PDF.
        let count = |sidecars: &SidecarPaths| {
            Format::ALL
                .into_iter()
                .flat_map(|from| Format::ALL.into_iter().map(move |to| (from, to)))
                .filter(|&(from, to)| runs(from, to, sidecars))
                .count()
        };
        assert_eq!(count(&with_heif), count(&none) + 24);
        // AVIF has no tools in v1 (FORMAT_MATRIX): not even Create PDF.
        for tool in grape_core::ToolKey::ALL {
            assert!(
                !crate::supports_with(&Operation::tool(tool), &[Some(Format::Avif)], &with_heif),
                "{tool:?}"
            );
        }

        // Without the helper, run_job refuses before reading anything.
        for (name, bytes, to, message) in [
            (
                "photo.heic",
                fake_heic(1, 1, 3, &[1, 2, 3], None),
                Format::Png,
                "HEIC to PNG isn't in this build yet.",
            ),
            (
                "photo.avif",
                fake_image(HeifFormat::Avif, 1, 1, 3, &[1, 2, 3], None),
                Format::Png,
                "AVIF to PNG isn't in this build yet.",
            ),
            (
                "photo.avif",
                fake_image(HeifFormat::Avif, 1, 1, 3, &[1, 2, 3], None),
                Format::Heic,
                "AVIF to HEIC isn't in this build yet.",
            ),
        ] {
            let case = case(name, &bytes, to);
            let err = crate::run_job(&case.job, &EngineCtx::default(), &mut |_| {}).unwrap_err();
            assert_eq!(err.kind, ErrorKind::NotYetSupported);
            assert_eq!(err.user_message, message);
            assert!(!case.dest().exists());
            case.check();
        }
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
        assert_eq!(err.user_message, "Grape's image helper is missing.");
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
    fn an_avif_name_that_isnt_heif_goes_to_the_raster_engine() {
        let rows = gradient(3, 2, 3);
        let source = png(3, 2, &rows, ExtendedColorType::Rgb8, None);
        for to in [Format::Bmp, Format::Jpg] {
            let case = case("photo.avif", &source, to);
            let ran = case.run(&nowhere());
            ran.result.unwrap();
            let out = ::image::open(case.dest()).unwrap();
            assert_eq!((out.width(), out.height()), (3, 2));
            assert_monotone(&ran.progress);
            case.check();
        }
        let case = case("photo.avif", b"not an image at all", Format::Png);
        let ran = case.run(&nowhere());
        case.refused(
            ran,
            ErrorKind::DecodeFailed,
            "photo.avif isn't a readable AVIF image.",
        );
    }

    #[test]
    fn other_heif_is_refused_before_anything_starts() {
        for (name, to) in [
            ("photo.heic", Format::Jpg),
            ("photo.avif", Format::Jpg),
            ("photo.avif", Format::Heic),
            ("photo.heic", Format::Pdf),
        ] {
            let case = case(name, &AVCI_BRAND, to);
            let err = if to == Format::Pdf {
                // pdf::create's decode half.
                let decoder = Decoder::new(nowhere(), case.limits());
                let names = Names::new(&case.src, &case.dest(), case.from, to);
                decoder
                    .decode(
                        &case.src,
                        &names,
                        Format::Png,
                        &CancelToken::new(),
                        (0.0, 0.5),
                        &mut |_| {},
                    )
                    .unwrap_err()
            } else {
                let ran = case.run(&nowhere());
                case.refused(
                    ran,
                    ErrorKind::DecodeFailed,
                    &format!("{name} is a kind of HEIF image Grape can't read yet."),
                )
            };
            assert_eq!(
                err.user_message,
                format!("{name} is a kind of HEIF image Grape can't read yet.")
            );
            assert_eq!(err.detail, "HEIF: its ftyp names another codec");
            case.check();
        }
    }

    #[test]
    fn the_content_decides_whatever_the_name() {
        // D41: an AVIF named .heic decodes as AVIF, a HEIC named .avif as
        // HEIC (the fake checks it's told the content's format).
        let rows = gradient(3, 2, 3);
        for (name, format) in [
            ("photo.heic", HeifFormat::Avif),
            ("photo.avif", HeifFormat::Heic),
        ] {
            let case = case(name, &fake_image(format, 3, 2, 3, &rows, None), Format::Png);
            case.run(&fake("ok")).result.unwrap();
            assert_eq!(
                ::image::open(case.dest()).unwrap().to_rgb8().as_raw(),
                &rows
            );
            case.check();
        }
        // An unsupported AVIF profile is "a kind of AVIF image", by the
        // content, whatever the name; HEIC's is "a kind of HEIF image".
        for (name, format, kind) in [
            ("photo.heic", HeifFormat::Avif, "AVIF"),
            ("photo.avif", HeifFormat::Avif, "AVIF"),
            ("photo.avif", HeifFormat::Heic, "HEIF"),
        ] {
            let case = case(
                name,
                &fake_image(format, 1, 1, 3, &[1, 2, 3], None),
                Format::Png,
            );
            let ran = case.run(&fake("error-unsupported"));
            case.refused(
                ran,
                ErrorKind::DecodeFailed,
                &format!("{name} is a kind of {kind} image Grape can't read yet."),
            );
        }
    }

    #[test]
    fn a_helper_past_its_heap_bound_needs_too_much_memory() {
        // The Mac worker's exit code past its heap bound, without a
        // container (macOS has none), both ways.
        let heic = fake_heic(1, 1, 3, &[0; 3], None);
        let case_out = case("photo.heic", &heic, Format::Png);
        let ran = case_out.run(&fake("over-heap"));
        let err = case_out.refused(
            ran,
            ErrorKind::DecodeFailed,
            "photo.heic needs too much memory, so Grape stopped.",
        );
        assert_eq!(
            err.detail,
            "the HEIF helper went past its 2048 MiB heap bound"
        );

        let source = png(1, 1, &[0; 3], ExtendedColorType::Rgb8, None);
        for to in [Format::Heic, Format::Avif] {
            let case_in = case("photo.png", &source, to);
            let ran = case_in.run(&fake("over-heap"));
            case_in.refused(
                ran,
                ErrorKind::DecodeFailed,
                "photo.png needs too much memory, so Grape stopped.",
            );
        }
        // An AVIF, decoded alone and before an encode.
        let avif = fake_image(HeifFormat::Avif, 1, 1, 3, &[0; 3], None);
        for to in [Format::Png, Format::Heic] {
            let case = case("photo.avif", &avif, to);
            let ran = case.run(&fake("over-heap"));
            case.refused(
                ran,
                ErrorKind::DecodeFailed,
                "photo.avif needs too much memory, so Grape stopped.",
            );
        }
    }

    #[test]
    fn an_output_of_the_other_format_is_refused() {
        // The helper's done line is right, but out.heic is AVIF, or out.avif
        // is HEIC.
        let source = png(2, 2, &[5; 12], ExtendedColorType::Rgb8, None);
        for (to, detail) in [
            (Format::Heic, "HEIF helper: its output isn't HEIC"),
            (Format::Avif, "HEIF helper: its output isn't AVIF"),
        ] {
            let case = case("photo.png", &source, to);
            let ran = case.run(&fake("other-output"));
            let err = case.refused(
                ran,
                ErrorKind::DecodeFailed,
                "Grape's image helper stopped unexpectedly.",
            );
            assert_eq!(err.detail, detail);
        }
    }

    #[test]
    fn each_format_has_its_own_side_cap() {
        // AV1 level 6's 16,384 a side for AVIF, HEVC's 16,888 for HEIC
        // (D41): over it is refused before anything starts, at it runs.
        for (to, cap, name) in [
            (Format::Avif, 16_384, "AVIF"),
            (Format::Heic, 16_888, "HEIC"),
        ] {
            for (width, height) in [(cap + 1, 1), (1, cap + 1)] {
                let source = png(
                    width,
                    height,
                    &vec![9; (width * height * 3) as usize],
                    ExtendedColorType::Rgb8,
                    None,
                );
                let case = case("photo.png", &source, to);
                let ran = case.run(&nowhere());
                let err = case.refused(
                    ran,
                    ErrorKind::EncodeFailed,
                    &format!("photo.png is too large to save as {name}."),
                );
                assert!(err.detail.contains(&format!("{cap} a side")), "{err:?}");
            }
            let source = png(
                cap,
                1,
                &vec![9; (cap * 3) as usize],
                ExtendedColorType::Rgb8,
                None,
            );
            let case = case("photo.png", &source, to);
            case.run(&fake("ok")).result.unwrap();
            let out = parse_fake(&fs::read(case.dest()).unwrap()).unwrap();
            assert_eq!((out.width, out.height), (cap, 1));
            case.check();
        }
        // HEIC → AVIF: a picture HEIC holds but AV1 doesn't is decoded, then
        // refused before the encode starts.
        let wide = fake_heic(16_385, 1, 3, &[9; 16_385 * 3], None);
        let case = case("photo.heic", &wide, Format::Avif);
        let contain = FakeContain::new(&case, false, None);
        let mut launch = fake("ok");
        launch.contain = Some(contain.clone());
        let ran = case.run(&launch);
        case.refused(
            ran,
            ErrorKind::EncodeFailed,
            "photo.heic is too large to save as AVIF.",
        );
        assert_eq!(
            contain.calls.load(Ordering::SeqCst),
            1,
            "only the decode ran"
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
            "Grape's image helper stopped unexpectedly.",
        );
        let temp = (
            ErrorKind::WriteFailed,
            "Grape couldn't write its temp files.",
        );
        let protocol = (
            ErrorKind::NotYetSupported,
            "Grape's image helper doesn't match this version of Grape.",
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
        let heic_no_encoder = no_encoder(HeifFormat::Heic, cfg!(target_os = "macos"));
        let no_encoder = (ErrorKind::NotYetSupported, heic_no_encoder.as_str());
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

        // AVIF (D41): its name where a message names the format.
        let avif = fake_image(HeifFormat::Avif, 2, 2, 3, &[5; 12], None);
        let unreadable = (
            ErrorKind::DecodeFailed,
            "photo.avif isn't a readable AVIF image.",
        );
        for (kind, (error, message)) in [
            ("notHeif", unreadable),
            ("damaged", unreadable),
            (
                "unsupported",
                (
                    ErrorKind::DecodeFailed,
                    "photo.avif is a kind of AVIF image Grape can't read yet.",
                ),
            ),
            (
                "noImage",
                (
                    ErrorKind::DecodeFailed,
                    "photo.avif has no image to convert.",
                ),
            ),
            (
                "tooLarge",
                (
                    ErrorKind::DecodeFailed,
                    "photo.avif is too large to convert.",
                ),
            ),
            ("noEncoder", stopped),
            (
                "read",
                (ErrorKind::ReadFailed, "Grape couldn't read photo.avif."),
            ),
            ("write", temp),
            ("protocol", protocol),
            ("internal", stopped),
        ] {
            let case = case("photo.avif", &avif, Format::Png);
            let ran = case.run(&fake(&format!("error-{kind}")));
            let err = case.refused(ran, error, message);
            assert!(err.detail.contains("fake failure"), "{kind}: {err:?}");
        }
        let avif_no_encoder = super::no_encoder(HeifFormat::Avif, cfg!(target_os = "macos"));
        let no_encoder = (ErrorKind::NotYetSupported, avif_no_encoder.as_str());
        for (mode, (error, message)) in [
            (
                "error-tooLarge",
                (
                    ErrorKind::EncodeFailed,
                    "photo.png is too large to save as AVIF.",
                ),
            ),
            (
                "error-encode",
                (ErrorKind::EncodeFailed, "Grape couldn't write a AVIF file."),
            ),
            ("error-noEncoder", no_encoder),
            ("noencoder", no_encoder),
            ("error-write", temp),
            ("error-protocol", protocol),
            ("error-unsupported", stopped),
        ] {
            let case = case("photo.png", &source, Format::Avif);
            let ran = case.run(&fake(mode));
            case.refused(ran, error, message);
        }
    }

    #[test]
    fn crashes_lies_and_bad_files_say_the_helper_stopped() {
        let stopped = "Grape's image helper stopped unexpectedly.";
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
            "Grape's image helper doesn't match this version of Grape.",
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
                "Grape's image helper stopped unexpectedly.",
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
            FakeContain::at(case.work(), fail, peak)
        }

        /// One that marks the work folders made in `work_parent`.
        fn at(work_parent: PathBuf, fail: bool, peak: Option<u64>) -> Arc<FakeContain> {
            Arc::new(FakeContain {
                work_parent,
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
            "Grape's image helper stopped unexpectedly.",
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
            "Grape's image helper couldn't start.",
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
            "Grape's image helper couldn't start.",
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
                "Grape's image helper stopped unexpectedly.",
            ),
            (Some(GIB), "Grape's image helper stopped unexpectedly."),
            (None, "Grape's image helper stopped unexpectedly."),
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
        if let Err(err) = fs::write(&src, &heic) {
            // APFS refuses names that aren't UTF-8 ("Illegal byte
            // sequence"), so no such source can reach Grape on a Mac.
            if cfg!(target_os = "macos") {
                return;
            }
            panic!("{err}");
        }
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
        assert_eq!(limits.helper, proto::Limits::for_encode(HeifFormat::Heic));
        // An encode sends its format's picture caps (D41).
        for format in HeifFormat::ALL {
            assert_eq!(
                limits.encode_limits(format),
                proto::Limits::for_encode(format)
            );
        }
        assert_eq!(
            limits.encode_limits(HeifFormat::Avif).max_encode_side,
            16_384
        );
        assert_eq!(
            limits.encode_limits(HeifFormat::Heic).max_encode_side,
            16_888
        );
        // A job's own limits only ever tighten them.
        let tight = Limits {
            helper: proto::Limits {
                max_encode_side: 100,
                max_encode_pixels: 23,
                ..proto::Limits::DEFAULT
            },
            ..Limits::default()
        };
        for format in HeifFormat::ALL {
            let sent = tight.encode_limits(format);
            assert_eq!((sent.max_encode_side, sent.max_encode_pixels), (100, 23));
        }
        assert_eq!(limits.helper_memory, 2 * GIB);
        assert_eq!(limits.helper_memory, proto::WORKER_HEAP_BYTES);
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

    /// A folder for an [`Encoder`]'s or a [`Decoder`]'s work folders, the
    /// default limits with it, and the names of a `photo.heic` there.
    struct Tool {
        dir: TestDir,
    }

    impl Tool {
        fn new() -> Tool {
            Tool {
                dir: TestDir::new(),
            }
        }

        fn limits(&self) -> Limits {
            Limits {
                work_parent: self.dir.0.join("work"),
                ..Limits::default()
            }
        }

        fn names(&self) -> Names {
            let src = self.dir.0.join("photo.heic");
            Names::new(&src, &src, Format::Heic, Format::Heic)
        }

        /// No work folder is left.
        fn check(&self) {
            let work = listing(&self.dir.0.join("work"));
            assert!(work.is_empty(), "work folders left: {work:?}");
        }
    }

    /// What an [`Encoder::encode`] gave, with its progress.
    fn encode_with(
        encoder: &Encoder,
        img: &DynamicImage,
        icc: Option<&[u8]>,
        quality: Option<u8>,
        tool: &Tool,
        cancel: &CancelToken,
    ) -> (Result<Vec<u8>, GrapeError>, Vec<f32>) {
        let mut seen = Vec::new();
        let result = encoder.encode(
            img,
            icc,
            quality,
            &tool.names(),
            cancel,
            (0.4, 0.8),
            &mut |fraction| seen.push(fraction),
        );
        (result, seen)
    }

    fn rgb_image(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(
            RgbImage::from_raw(width, height, gradient(width, height, 3)).unwrap(),
        )
    }

    fn rgba_image(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgba8(
            RgbaImage::from_raw(width, height, gradient(width, height, 4)).unwrap(),
        )
    }

    /// The fake's mode with a tag of its own, so [`PROBED`] has never seen
    /// this helper.
    fn fresh(mode: &str) -> Launch {
        let mut launch = fake(mode);
        launch.envs.push((
            OsString::from("GRAPE_HEIF_FAKE_TAG"),
            OsString::from(unique_suffix()),
        ));
        launch
    }

    #[test]
    fn an_encoder_needs_the_helper() {
        assert!(Encoder::from_sidecars(&SidecarPaths::default(), HeifFormat::Heic).is_none());
        let sidecars = SidecarPaths {
            heif: Some(PathBuf::from("grape-heif")),
            ..SidecarPaths::default()
        };
        for format in HeifFormat::ALL {
            let encoder = Encoder::from_sidecars(&sidecars, format).unwrap();
            assert_eq!(encoder.format, format);
            assert_eq!(encoder.launch.program, PathBuf::from("grape-heif"));
            assert_eq!(encoder.launch.args, [OsString::from(proto::WORKER_FLAG)]);
            assert_eq!(encoder.limits, Limits::default());
        }
    }

    #[test]
    fn an_encoder_writes_rgb_and_rgba_as_they_are() {
        let tool = Tool::new();
        for format in HeifFormat::ALL {
            let encoder = Encoder::new(fake("ok"), tool.limits(), format);
            for (img, channels) in [(rgb_image(4, 3), 3), (rgba_image(5, 3), 4)] {
                let (result, progress) =
                    encode_with(&encoder, &img, None, None, &tool, &CancelToken::new());
                let bytes = result.unwrap();
                assert_eq!(proto::sniff(&bytes).format(), Some(format));
                let out = parse_fake(&bytes).unwrap();
                assert_eq!(out.format, format);
                assert_eq!(
                    (out.width, out.height, out.channels),
                    (img.width(), img.height(), channels)
                );
                assert_eq!(out.rows, img.as_bytes(), "{format:?}, {channels} channels");
                assert_eq!(out.icc, None);
                assert_monotone(&progress);
                assert_eq!(progress.first(), Some(&0.4), "{progress:?}");
                assert_eq!(progress.last(), Some(&0.8), "{progress:?}");
                tool.check();
            }
        }
    }

    #[test]
    fn an_encoder_refuses_other_pixels_before_starting() {
        let tool = Tool::new();
        // `nowhere`: a helper that started would say "couldn't start".
        let encoder = Encoder::new(nowhere(), tool.limits(), HeifFormat::Heic);
        let gray = DynamicImage::ImageLuma8(::image::GrayImage::new(2, 2));
        let gray_alpha = DynamicImage::ImageLumaA8(::image::GrayAlphaImage::new(2, 2));
        let deep = DynamicImage::ImageRgb16(::image::ImageBuffer::new(2, 2));
        let float = DynamicImage::ImageRgba32F(::image::ImageBuffer::new(2, 2));
        let empty = DynamicImage::ImageRgb8(RgbImage::new(0, 0));
        for img in [gray, gray_alpha, deep, float, empty] {
            let (result, progress) =
                encode_with(&encoder, &img, None, None, &tool, &CancelToken::new());
            let err = result.unwrap_err();
            assert_eq!(
                (err.kind, err.user_message.as_str()),
                (ErrorKind::EncodeFailed, "Grape couldn't write a HEIC file."),
                "{err:?}"
            );
            assert!(err.detail.contains("not RGB8 or RGBA8"), "{err:?}");
            assert!(progress.is_empty());
        }
        // A quality outside 1..=100.
        for quality in [0, 101, 255] {
            let (result, _) = encode_with(
                &encoder,
                &rgb_image(2, 2),
                None,
                Some(quality),
                &tool,
                &CancelToken::new(),
            );
            let err = result.unwrap_err();
            assert_eq!(err.kind, ErrorKind::EncodeFailed, "{err:?}");
            assert!(err.detail.contains("isn't in 1..=100"), "{err:?}");
        }
        // Over one picture: refused as a conversion would refuse it.
        let tight = Limits {
            helper: proto::Limits {
                max_encode_side: 3,
                ..proto::Limits::DEFAULT
            },
            ..tool.limits()
        };
        let encoder = Encoder::new(nowhere(), tight, HeifFormat::Heic);
        let (result, _) = encode_with(
            &encoder,
            &rgb_image(4, 2),
            None,
            None,
            &tool,
            &CancelToken::new(),
        );
        let err = result.unwrap_err();
        assert_eq!(
            (err.kind, err.user_message.as_str()),
            (
                ErrorKind::EncodeFailed,
                "photo.heic is too large to save as HEIC."
            )
        );
        tool.check();
    }

    #[test]
    fn an_encoder_says_how_large_a_picture_it_takes() {
        let tool = Tool::new();
        let heic = Encoder::new(nowhere(), tool.limits(), HeifFormat::Heic);
        let avif = Encoder::new(nowhere(), tool.limits(), HeifFormat::Avif);
        assert_eq!(heic.max_picture(), (35_651_584, 16_888));
        assert_eq!(avif.max_picture(), (35_651_584, 16_384));
        // A job's own limits only tighten it.
        let tight = Limits {
            helper: proto::Limits {
                max_encode_pixels: 99,
                max_encode_side: 20_000,
                ..proto::Limits::DEFAULT
            },
            ..tool.limits()
        };
        let heic = Encoder::new(nowhere(), tight, HeifFormat::Heic);
        assert_eq!(heic.max_picture(), (99, 16_888));
        // At the cap it runs.
        let at = Limits {
            helper: proto::Limits {
                max_encode_pixels: 12,
                max_encode_side: 4,
                ..proto::Limits::DEFAULT
            },
            ..tool.limits()
        };
        let encoder = Encoder::new(fake("ok"), at, HeifFormat::Heic);
        let (result, _) = encode_with(
            &encoder,
            &rgb_image(4, 3),
            None,
            None,
            &tool,
            &CancelToken::new(),
        );
        result.unwrap();
        tool.check();
    }

    #[test]
    fn an_encoder_keeps_an_rgb_profile_and_drops_one_past_the_cap() {
        let tool = Tool::new();
        let profile = rgb_profile();
        let encode = |limits: Limits, img: &DynamicImage, icc: &[u8]| {
            let encoder = Encoder::new(fake("ok"), limits, HeifFormat::Heic);
            let (result, _) =
                encode_with(&encoder, img, Some(icc), None, &tool, &CancelToken::new());
            parse_fake(&result.unwrap()).unwrap().icc
        };
        for img in [rgb_image(3, 2), rgba_image(3, 2)] {
            assert_eq!(encode(tool.limits(), &img, &profile), Some(profile.clone()));
            // Exactly at the helper's cap it's kept; one byte over, dropped.
            let cap = |bytes: usize| Limits {
                helper: proto::Limits {
                    max_icc_bytes: u32::try_from(bytes).unwrap(),
                    ..proto::Limits::DEFAULT
                },
                ..tool.limits()
            };
            assert_eq!(
                encode(cap(profile.len()), &img, &profile),
                Some(profile.clone())
            );
            assert_eq!(encode(cap(profile.len() - 1), &img, &profile), None);
            // A profile that doesn't describe RGB pixels is dropped, as a
            // conversion drops it.
            let mut gray = rgb_profile();
            gray[16..20].copy_from_slice(b"GRAY");
            assert_eq!(encode(tool.limits(), &img, &gray), None);
        }
        tool.check();
    }

    #[test]
    fn an_encoders_failures_read_as_a_conversions() {
        let tool = Tool::new();
        let no_encoder = no_encoder(HeifFormat::Heic, cfg!(target_os = "macos"));
        for (mode, kind, message) in [
            (
                "error-encode",
                ErrorKind::EncodeFailed,
                "Grape couldn't write a HEIC file.",
            ),
            (
                "error-tooLarge",
                ErrorKind::EncodeFailed,
                "photo.heic is too large to save as HEIC.",
            ),
            ("noencoder", ErrorKind::NotYetSupported, no_encoder.as_str()),
            (
                "error-protocol",
                ErrorKind::NotYetSupported,
                "Grape's image helper doesn't match this version of Grape.",
            ),
            (
                "bad-output",
                ErrorKind::DecodeFailed,
                "Grape's image helper stopped unexpectedly.",
            ),
            (
                "other-output",
                ErrorKind::DecodeFailed,
                "Grape's image helper stopped unexpectedly.",
            ),
            (
                "crash",
                ErrorKind::DecodeFailed,
                "Grape's image helper stopped unexpectedly.",
            ),
            (
                "over-heap",
                ErrorKind::DecodeFailed,
                "photo.heic needs too much memory, so Grape stopped.",
            ),
        ] {
            let encoder = Encoder::new(fake(mode), tool.limits(), HeifFormat::Heic);
            let (result, progress) = encode_with(
                &encoder,
                &rgb_image(3, 2),
                None,
                Some(50),
                &tool,
                &CancelToken::new(),
            );
            let err = result.unwrap_err();
            assert_eq!(
                (err.kind, err.user_message.as_str()),
                (kind, message),
                "{mode}"
            );
            assert_monotone(&progress);
            tool.check();
        }
    }

    #[test]
    fn the_bytes_read_back_are_capped_at_the_source_cap() {
        let tool = Tool::new();
        let img = rgb_image(4, 3);
        let encoder = Encoder::new(fake("ok"), tool.limits(), HeifFormat::Heic);
        let (result, _) = encode_with(&encoder, &img, None, None, &tool, &CancelToken::new());
        let len = u64::try_from(result.unwrap().len()).unwrap();
        let capped = |cap: u64| Limits {
            helper: proto::Limits {
                max_source_bytes: cap,
                ..proto::Limits::DEFAULT
            },
            ..tool.limits()
        };
        let encoder = Encoder::new(fake("ok"), capped(len), HeifFormat::Heic);
        let (result, _) = encode_with(&encoder, &img, None, None, &tool, &CancelToken::new());
        assert_eq!(u64::try_from(result.unwrap().len()).unwrap(), len);
        let encoder = Encoder::new(fake("ok"), capped(len - 1), HeifFormat::Heic);
        let (result, _) = encode_with(&encoder, &img, None, None, &tool, &CancelToken::new());
        let err = result.unwrap_err();
        assert_eq!(
            (err.kind, err.user_message.as_str()),
            (
                ErrorKind::EncodeFailed,
                "photo.heic is too large to save as HEIC."
            )
        );
        assert!(err.detail.contains("cap for reading it back"), "{err:?}");
        tool.check();
    }

    #[test]
    fn cancel_ends_an_encoder_quickly() {
        let tool = Tool::new();
        let encoder = Encoder::new(fake("slow"), tool.limits(), HeifFormat::Heic);
        let cancel = CancelToken::new();
        let at = Arc::new(Mutex::new(None));
        {
            let (cancel, at) = (cancel.clone(), Arc::clone(&at));
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(700));
                *at.lock().unwrap() = Some(Instant::now());
                cancel.cancel();
            });
        }
        let (result, progress) =
            encode_with(&encoder, &rgb_image(3, 2), None, None, &tool, &cancel);
        let at = at.lock().unwrap().unwrap();
        assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
        let err = result.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
        assert_monotone(&progress);
        tool.check();
        // A cancelled job starts nothing.
        let (result, progress) =
            encode_with(&encoder, &rgb_image(3, 2), None, None, &tool, &cancel);
        assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
        assert!(progress.is_empty());
    }

    #[test]
    fn check_encode_probes_each_helper_once() {
        let tool = Tool::new();
        let names = tool.names();
        let contained = |mode: &str| {
            let contain = FakeContain::at(tool.dir.0.join("work"), false, None);
            let mut launch = fresh(mode);
            launch.contain = Some(contain.clone());
            (launch, contain)
        };
        let check = |launch: &Launch, format: HeifFormat| {
            Encoder::new(launch.clone(), tool.limits(), format)
                .check_encode(&names, &CancelToken::new())
        };

        // Asked once, whichever format and however many encoders.
        let (launch, contain) = contained("ok");
        for format in [HeifFormat::Heic, HeifFormat::Heic, HeifFormat::Avif] {
            check(&launch, format).unwrap();
        }
        assert_eq!(contain.calls.load(Ordering::SeqCst), 1);
        // Another helper is asked too.
        let (other, other_contain) = contained("ok");
        check(&other, HeifFormat::Heic).unwrap();
        assert_eq!(other_contain.calls.load(Ordering::SeqCst), 1);

        // One that can't write HEIC: noEncoder's words, from the kept caps.
        let (launch, contain) = contained("no-heic-encoder");
        for _ in 0..2 {
            let err = check(&launch, HeifFormat::Heic).unwrap_err();
            assert_eq!(
                (err.kind, err.user_message),
                (
                    ErrorKind::NotYetSupported,
                    no_encoder(HeifFormat::Heic, cfg!(target_os = "macos"))
                )
            );
            assert_eq!(
                err.detail,
                "HEIF helper: its probe says it can't write HEIC"
            );
        }
        check(&launch, HeifFormat::Avif).unwrap();
        assert_eq!(contain.calls.load(Ordering::SeqCst), 1);

        // A probe that fails isn't kept: the next job asks again.
        let (launch, contain) = contained("probe-crash");
        for calls in 1..=2 {
            let err = check(&launch, HeifFormat::Heic).unwrap_err();
            assert_eq!(
                err.user_message,
                "Grape's image helper stopped unexpectedly."
            );
            assert_eq!(contain.calls.load(Ordering::SeqCst), calls);
        }
        // Nor is a probe of a helper that isn't there.
        let err = check(&nowhere(), HeifFormat::Heic).unwrap_err();
        assert_eq!(err.user_message, "Grape's image helper couldn't start.");
        // A cancelled job doesn't ask.
        let (launch, contain) = contained("ok");
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = Encoder::new(launch, tool.limits(), HeifFormat::Heic)
            .check_encode(&names, &cancel)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);
        assert_eq!(contain.calls.load(Ordering::SeqCst), 0);
        tool.check();
    }

    #[test]
    fn a_failed_probe_reads_as_an_encodes_failure() {
        let tool = Tool::new();
        for (mode, kind, message) in [
            (
                "probe-crash",
                ErrorKind::DecodeFailed,
                "Grape's image helper stopped unexpectedly.",
            ),
            // A done line with no caps.
            (
                "probe-lie",
                ErrorKind::DecodeFailed,
                "Grape's image helper stopped unexpectedly.",
            ),
            (
                "probe-protocol",
                ErrorKind::NotYetSupported,
                "Grape's image helper doesn't match this version of Grape.",
            ),
            (
                "probe-over-heap",
                ErrorKind::DecodeFailed,
                "photo.heic needs too much memory, so Grape stopped.",
            ),
        ] {
            let encoder = Encoder::new(fresh(mode), tool.limits(), HeifFormat::Heic);
            let err = encoder
                .check_encode(&tool.names(), &CancelToken::new())
                .unwrap_err();
            assert_eq!(
                (err.kind, err.user_message.as_str()),
                (kind, message),
                "{mode}"
            );
            // Anything but the probe is served as `ok`.
            let (result, _) = encode_with(
                &encoder,
                &rgb_image(3, 2),
                None,
                Some(50),
                &tool,
                &CancelToken::new(),
            );
            assert_eq!(fake_quality(&result.unwrap()), Some(50), "{mode}");
            tool.check();
        }
    }

    /// A job that probes first reaches what its fake's mode is for: the
    /// modes leave the probe alone.
    #[test]
    fn the_fakes_modes_leave_the_probe_alone() {
        let tool = Tool::new();
        let names = tool.names();
        for mode in [
            "ok",
            "slow",
            "slow-encode",
            "noencoder",
            "error-encode",
            "error-tooLarge",
            "crash",
            "protocol",
            "over-heap",
            "huge-image",
            "over-pixels",
            "two-channels",
            "huge-icc",
            "lie",
            "noisy",
            "bad-pixels",
            "bad-output",
            "other-output",
            "wrong-size",
            "done-then-fail",
        ] {
            let encoder = Encoder::new(fresh(mode), tool.limits(), HeifFormat::Heic);
            encoder
                .check_encode(&names, &CancelToken::new())
                .unwrap_or_else(|err| panic!("{mode}: {err:?}"));
            tool.check();
        }

        // The encode's own check, not the probe, finds the other format's
        // file.
        let launch = fresh("other-output");
        let encoder = Encoder::new(launch, tool.limits(), HeifFormat::Heic);
        encoder.check_encode(&names, &CancelToken::new()).unwrap();
        let (result, _) = encode_with(
            &encoder,
            &rgb_image(3, 2),
            None,
            Some(50),
            &tool,
            &CancelToken::new(),
        );
        let err = result.unwrap_err();
        assert_eq!(
            err.user_message,
            "Grape's image helper stopped unexpectedly."
        );
        assert_eq!(err.detail, "HEIF helper: its output isn't HEIC");
        tool.check();

        // The decode's own peak, not the probe, refuses the huge image.
        let rows = gradient(3, 2, 3);
        let src = tool.dir.0.join("photo.heic");
        fs::write(&src, fake_heic(3, 2, 3, &rows, None)).unwrap();
        let launch = fresh("huge-image");
        Encoder::new(launch.clone(), tool.limits(), HeifFormat::Heic)
            .check_encode(&names, &CancelToken::new())
            .unwrap();
        // 12 bytes a pixel: 1.5 GiB for its 512 MiB of RGBA.
        let peak =
            |width: u32, height: u32, _: ColorType| u64::from(width) * u64::from(height) * 12;
        let err = Decoder::new(launch, tool.limits())
            .decode_within(
                &src,
                &names,
                &peak,
                &CancelToken::new(),
                (0.0, 0.5),
                &mut |_| {},
            )
            .unwrap_err();
        assert_eq!(
            (err.kind, err.user_message.as_str()),
            (
                ErrorKind::DecodeFailed,
                "photo.heic is too large to convert."
            )
        );
        assert!(
            err.detail.contains("16384x8192 Rgba8 needs about 1536 MiB"),
            "{err:?}"
        );
        tool.check();
    }

    #[test]
    fn decode_within_holds_the_image_line_to_the_tools_peak() {
        let tool = Tool::new();
        let rows = gradient(3, 2, 3);
        let profile = rgb_profile();
        let icc = u64::try_from(profile.len()).unwrap();
        let src = tool.dir.0.join("photo.heic");
        fs::write(&src, fake_heic(3, 2, 3, &rows, Some(&profile))).unwrap();
        let names = tool.names();
        let decode = |mode: &str, peak: u64| {
            let seen = Mutex::new(Vec::new());
            let tool_peak = |width: u32, height: u32, color: ColorType| {
                seen.lock().unwrap().push((width, height, color));
                peak
            };
            let decoder = Decoder::new(fake(mode), tool.limits());
            let result = decoder.decode_within(
                &src,
                &names,
                &tool_peak,
                &CancelToken::new(),
                (0.0, 0.5),
                &mut |_| {},
            );
            assert_eq!(*seen.lock().unwrap(), [(3, 2, ColorType::Rgb8)]);
            result
        };
        // Just under: the peak and the profile are exactly the limit.
        match decode("ok", image::WORKING_SET_LIMIT - icc).unwrap() {
            Decoded::Pixels(img, got) => {
                assert_eq!(img.as_bytes(), rows.as_slice());
                assert_eq!(got, Some(profile.clone()));
            }
            Decoded::NotHeif => panic!("not HEIF"),
        }
        tool.check();
        // Just over: the helper (which would sleep a minute) is killed at
        // once, though converting the pixels would fit.
        let started = Instant::now();
        let err = decode("slow", image::WORKING_SET_LIMIT - icc + 1).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(
            (err.kind, err.user_message.as_str()),
            (
                ErrorKind::DecodeFailed,
                "photo.heic is too large to convert."
            )
        );
        assert!(
            err.detail.contains("3x2 Rgb8 needs about 1024 MiB"),
            "{err:?}"
        );
        tool.check();

        // The rest is `decode`'s: a file that isn't HEIF is the raster
        // engine's, and another codec's HEIF is refused.
        let peak = |_: u32, _: u32, _: ColorType| 0;
        let decoder = Decoder::new(nowhere(), tool.limits());
        let png_src = tool.dir.0.join("png.heic");
        fs::write(
            &png_src,
            png(1, 1, &[1, 2, 3], ExtendedColorType::Rgb8, None),
        )
        .unwrap();
        let other_src = tool.dir.0.join("avc.heic");
        fs::write(&other_src, AVCI_BRAND).unwrap();
        let within = |src: &Path| {
            decoder.decode_within(
                src,
                &names,
                &peak,
                &CancelToken::new(),
                (0.0, 0.5),
                &mut |_| {},
            )
        };
        assert!(matches!(within(&png_src), Ok(Decoded::NotHeif)));
        let err = within(&other_src).unwrap_err();
        assert_eq!(
            err.user_message,
            "photo.heic is a kind of HEIF image Grape can't read yet."
        );
        assert_eq!(err.detail, "HEIF: its ftyp names another codec");
        tool.check();
    }

    #[test]
    fn an_encoder_sends_the_quality_and_a_conversion_none() {
        let tool = Tool::new();
        for format in HeifFormat::ALL {
            let encoder = Encoder::new(fake("ok"), tool.limits(), format);
            for quality in [Some(1), Some(40), Some(60), Some(100), None] {
                let (result, _) = encode_with(
                    &encoder,
                    &rgb_image(3, 2),
                    None,
                    quality,
                    &tool,
                    &CancelToken::new(),
                );
                let bytes = result.unwrap();
                assert_eq!(fake_quality(&bytes), quality, "{format:?}");
                assert_eq!(parse_fake(&bytes).unwrap().rows, gradient(3, 2, 3));
            }
        }
        tool.check();
        // A conversion leaves the quality to the helper.
        let source = png(2, 2, &[5; 12], ExtendedColorType::Rgb8, None);
        let case = case("photo.png", &source, Format::Heic);
        case.run(&fake("ok")).result.unwrap();
        let written = fs::read(case.dest()).unwrap();
        assert!(parse_fake(&written).is_some());
        assert_eq!(fake_quality(&written), None);
        case.check();
        // Not the fake's.
        assert_eq!(fake_quality(&AVCI_BRAND), None);
        assert_eq!(fake_quality(b""), None);
    }

    /// `body`'s boxes, by a walk of its own: each box's type and body,
    /// with 32-bit sizes only (what the fake writes).
    fn walk(mut body: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut boxes = Vec::new();
        while !body.is_empty() {
            let size = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
            assert!(size >= 8 && size <= body.len(), "a box of {size} bytes");
            let kind = String::from_utf8(body[4..8].to_vec()).unwrap();
            boxes.push((kind, body[8..size].to_vec()));
            body = &body[size..];
        }
        boxes
    }

    fn kinds(boxes: &[(String, Vec<u8>)]) -> Vec<&str> {
        boxes.iter().map(|(kind, _)| kind.as_str()).collect()
    }

    fn be32(bytes: &[u8]) -> u32 {
        u32::from_be_bytes(bytes[..4].try_into().unwrap())
    }

    fn be16(bytes: &[u8]) -> u16 {
        u16::from_be_bytes(bytes[..2].try_into().unwrap())
    }

    #[test]
    fn the_fakes_files_are_real_heif() {
        let rows = gradient(3, 2, 3);
        let profile = rgb_profile();
        for (format, brand, item, config, config_len) in [
            (HeifFormat::Heic, "heic", "hvc1", "hvcC", 23),
            (HeifFormat::Avif, "avif", "av01", "av1C", 4),
        ] {
            let bytes = fake_image(format, 3, 2, 3, &rows, Some(&profile));
            // It sniffs as its format under protocol 2's brand rules.
            let head = &bytes[..bytes.len().min(proto::SNIFF_BYTES)];
            assert_eq!(proto::sniff(head).format(), Some(format));

            let top = walk(&bytes);
            assert_eq!(kinds(&top), ["ftyp", "meta", "mdat"], "{format:?}");
            let ftyp = &top[0].1;
            assert_eq!(&ftyp[..4], brand.as_bytes());
            assert_eq!(&ftyp[8..], [b"mif1", brand.as_bytes()].concat());

            let meta = &top[1].1;
            assert_eq!(be32(meta), 0, "meta is a version 0 full box");
            let meta = walk(&meta[4..]);
            assert_eq!(kinds(&meta), ["hdlr", "pitm", "iinf", "iloc", "iprp"]);
            let hdlr = &meta[0].1;
            assert_eq!(&hdlr[8..12], b"pict");
            let pitm = &meta[1].1;
            assert_eq!((pitm[0], be16(&pitm[4..])), (0, 1));

            let iinf = &meta[2].1;
            assert_eq!((iinf[0], be16(&iinf[4..])), (0, 1));
            let infe = walk(&iinf[6..]);
            assert_eq!(kinds(&infe), ["infe"]);
            let infe = &infe[0].1;
            assert_eq!(infe[0], 2, "infe version 2");
            assert_eq!((be16(&infe[4..]), be16(&infe[6..])), (1, 0));
            assert_eq!(&infe[8..12], item.as_bytes());

            // iloc v1, 4-byte offsets and lengths: item 1's one extent is
            // the whole of mdat's body.
            let iloc = &meta[3].1;
            assert_eq!((iloc[0], iloc[4], iloc[5]), (1, 0x44, 0x00));
            assert_eq!(be16(&iloc[6..]), 1, "one item");
            assert_eq!(be16(&iloc[8..]), 1, "item 1");
            assert_eq!(be16(&iloc[10..]) & 15, 0, "in the file");
            assert_eq!(be16(&iloc[14..]), 1, "one extent");
            let (offset, length) = (be32(&iloc[16..]) as usize, be32(&iloc[20..]) as usize);
            let mdat = &top[2].1;
            assert_eq!(offset, bytes.len() - mdat.len());
            assert_eq!(length, mdat.len());
            assert!(mdat.starts_with(PAYLOAD_MAGIC));

            let iprp = walk(&meta[4].1);
            assert_eq!(kinds(&iprp), ["ipco", "ipma"]);
            let ipco = walk(&iprp[0].1);
            assert_eq!(kinds(&ipco), [config, "ispe"]);
            assert_eq!(ipco[0].1.len(), config_len);
            if format == HeifFormat::Heic {
                let hvcc = &ipco[0].1;
                assert_eq!(hvcc[0], 1, "configurationVersion");
                assert_eq!(hvcc[16] & 3, 1, "4:2:0");
                assert_eq!((hvcc[17] & 7, hvcc[18] & 7), (0, 0), "8-bit");
                assert_eq!(hvcc[22], 0, "no arrays");
            }
            let ispe = &ipco[1].1;
            assert_eq!((be32(&ispe[4..]), be32(&ispe[8..])), (3, 2));
            let ipma = &iprp[1].1;
            assert_eq!(be32(&ipma[4..]), 1, "one entry");
            assert_eq!(be16(&ipma[8..]), 1, "item 1");
            assert_eq!(&ipma[10..], [2, 0x81, 0x02], "→ [1 (essential), 2]");

            let parsed = parse_fake(&bytes).unwrap();
            assert_eq!(parsed.format, format);
            assert_eq!((parsed.width, parsed.height, parsed.channels), (3, 2, 3));
            assert_eq!(
                (parsed.icc, parsed.rows, parsed.quality),
                (Some(profile.clone()), rows.clone(), None)
            );
        }
    }

    /// A file the fake didn't write, as another tool might rewrite one: a
    /// `free` box first, `pitm` and `iinf` version 1, an `Exif` item stored
    /// in `idat` (construction method 1) listed before the primary item,
    /// whose data is in two extents of `mdat` from a base offset, in
    /// `iloc` version 2 with 8-byte offsets.
    fn rewritten(original: &[u8]) -> Vec<u8> {
        let top = walk(original);
        let payload = &top[2].1;
        let half = payload.len() / 2;
        let infe_exif = full_box(b"infe", 2, 0, &[0, 3, 0, 0, b'E', b'x', b'i', b'f', 0]);
        let infe_image = full_box(
            b"infe",
            3,
            0,
            &[0, 0, 0, 7, 0, 0, b'h', b'v', b'c', b'1', 0],
        );
        let mut iinf = 2u32.to_be_bytes().to_vec();
        iinf.extend_from_slice(&infe_exif);
        iinf.extend_from_slice(&infe_image);
        let idat = b"\0\0\0\0Exif\0\0".to_vec();
        let iloc_for = |base: u64| {
            // offset 8 bytes, length 4, base offset 4, index 0.
            let mut iloc = vec![0x84, 0x40];
            iloc.extend_from_slice(&2u32.to_be_bytes());
            // Item 3: in idat, from 4, 6 bytes.
            iloc.extend_from_slice(&3u32.to_be_bytes());
            iloc.extend_from_slice(&1u16.to_be_bytes());
            iloc.extend_from_slice(&0u16.to_be_bytes());
            iloc.extend_from_slice(&0u32.to_be_bytes());
            iloc.extend_from_slice(&1u16.to_be_bytes());
            iloc.extend_from_slice(&4u64.to_be_bytes());
            iloc.extend_from_slice(&6u32.to_be_bytes());
            // Item 7: in the file, two extents from `base`.
            iloc.extend_from_slice(&7u32.to_be_bytes());
            iloc.extend_from_slice(&0u16.to_be_bytes());
            iloc.extend_from_slice(&0u16.to_be_bytes());
            iloc.extend_from_slice(&u32::try_from(base).unwrap().to_be_bytes());
            iloc.extend_from_slice(&2u16.to_be_bytes());
            iloc.extend_from_slice(&0u64.to_be_bytes());
            iloc.extend_from_slice(&u32::try_from(half).unwrap().to_be_bytes());
            iloc.extend_from_slice(&u64::try_from(half).unwrap().to_be_bytes());
            iloc.extend_from_slice(&u32::try_from(payload.len() - half).unwrap().to_be_bytes());
            full_box(b"iloc", 2, 0, &iloc)
        };
        let old_meta = walk(&top[1].1[4..]);
        let meta_with = |base: u64| {
            let mut meta = mp4_box(b"hdlr", &old_meta[0].1);
            meta.extend_from_slice(&full_box(b"pitm", 1, 0, &7u32.to_be_bytes()));
            meta.extend_from_slice(&full_box(b"iinf", 1, 0, &iinf));
            meta.extend_from_slice(&mp4_box(b"iprp", &old_meta[4].1));
            meta.extend_from_slice(&mp4_box(b"idat", &idat));
            meta.extend_from_slice(&iloc_for(base));
            full_box(b"meta", 0, 0, &meta)
        };
        let free = mp4_box(b"free", &[0; 16]);
        let ftyp = mp4_box(b"ftyp", &top[0].1);
        let base = (ftyp.len() + free.len() + meta_with(0).len() + 8) as u64;
        let mut bytes = ftyp;
        bytes.extend_from_slice(&free);
        bytes.extend_from_slice(&meta_with(base));
        bytes.extend_from_slice(&mp4_box(b"mdat", payload));
        bytes
    }

    #[test]
    fn a_rewritten_heic_still_decodes_through_the_fake() {
        let rows = gradient(4, 3, 4);
        let original = fake_heic(4, 3, 4, &rows, None);
        let bytes = rewritten(&original);
        assert_ne!(bytes, original);
        assert_eq!(proto::sniff(&bytes), Sniff::Heic);
        let parsed = parse_fake(&bytes).unwrap();
        assert_eq!(
            (parsed.format, parsed.channels, parsed.rows),
            (HeifFormat::Heic, 4, rows.clone())
        );
        // Through the helper, as a conversion reads it.
        let case = case("photo.heic", &bytes, Format::Png);
        case.run(&fake("ok")).result.unwrap();
        assert_eq!(
            ::image::open(case.dest()).unwrap().to_rgba8().as_raw(),
            &rows
        );
        case.check();
        // Item 1 in the wrong place isn't read as anything.
        let mut cut = fake_heic(4, 3, 4, &rows, None);
        cut.truncate(cut.len() - 1);
        assert!(parse_fake(&cut).is_none());
    }
}
