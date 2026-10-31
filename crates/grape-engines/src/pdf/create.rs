//! Images → PDF and the Create PDF tool (DECISIONS D37), in Grape's own
//! process: no PDF worker and no PDFium, because no PDF is parsed. The
//! only untrusted input is the images, which the raster engine decodes
//! with its own limits ([`image::decode`]), Grape's image helper decodes
//! in its own process ([`heif::Decoder`]): HEIC and AVIF (D33, D41), and
//! Grape's SVG worker draws in its own process ([`svg::Renderer`], D39).
//!
//! - **One page per image,** in the job's input order: JPG, PNG, WebP,
//!   TIFF and BMP always, HEIC and AVIF with the helper, SVG with the
//!   worker ([`reads`]); Create PDF takes the same but BMP and AVIF, which
//!   FORMAT_MATRIX gives no tools ([`combines`]). The page is the
//!   upright image at one pixel a point (72 dpi), scaled down uniformly
//!   when its longer side is over [`MAX_SIDE`] (Acrobat's 200 inches); the
//!   pixels are never resampled ([`page_size`]). An SVG's page is its own
//!   size (96 CSS px = 72 pt, held to the same bound), drawn at 300 DPI or
//!   as near as the worker's caps allow ([`svg_page`]). No image object
//!   has a side over [`MAX_IMAGE_SIDE`] pixels: a longer picture (a long
//!   screenshot) is stored losslessly as strips, each with its own soft
//!   mask, drawn edge to edge on its one page ([`tiles`]).
//! - **How each image is stored** (the writer takes "a JPEG to pass
//!   through" or "upright pixels and an ICC profile" from any decoder):
//!   1. a JPEG whose content is JPEG and whose frame and color transform
//!      PDF readers take as Grape's decoder does goes in as its original
//!      data, minus its metadata ([`jpeg::passthrough`]); its EXIF
//!      orientation is applied by the page's matrix ([`placement`]);
//!   2. HEIC, AVIF, lossy WebP and a JPEG that can't pass through are
//!      re-encoded as JPEG at quality 90 from their upright pixels, alpha
//!      kept as a soft mask;
//!   3. PNG, BMP, TIFF, lossless WebP and SVG are stored losslessly: 8-bit
//!      gray or RGB, Flate with PNG predictors ([`flate`]), alpha as a soft
//!      mask, and pure black-and-white gray at 1 bit a pixel.
//!
//!   A gray or RGB ICC profile that describes the pixels written becomes an
//!   `ICCBased` color space ([`image::icc_fits`]).
//! - **Memory:** one image at a time, decoded, its streams built, written,
//!   dropped. What this module holds at its peak is the decoded image and
//!   one stream of at most about the raw size of the 8-bit pixels: rows are
//!   converted and filtered one at a time (never a whole converted copy,
//!   except for a lossy picture that isn't 8-bit already), and each
//!   stream's buffer is reserved once at its bound and never grows
//!   ([`Sink`]). A JPEG re-encode that would be larger than that bound is
//!   given up for the lossless Flate stream, which always fits. A JPEG
//!   passed through holds the file and no pixels, and the list of its kept
//!   byte ranges, at most one a segment and [`jpeg::MAX_SEGMENTS`].
//!
//!   Decoding is held to [`image::WORKING_SET_LIMIT`] by the raster engine's
//!   estimate for a stand-in target, [`FIT`] (`Format::Png`), not for
//!   `Format::Pdf`: `image.rs`'s `target_color` is the identity for PDF and
//!   its `working_set` counts no encoder buffers for it, so PDF's estimate
//!   would be the decoded image alone. PNG's is the decoded image, or the
//!   8-bit pixels written plus PNG's in-memory compressed copy of the same
//!   size: for 8-bit sources exactly what this module holds, short only of
//!   a filter byte a row and Deflate's per-block overhead; for 16-bit and
//!   float sources more than it holds. That remainder is checked exactly
//!   once the pixels are decoded ([`page_memory`]), before any stream is
//!   reserved, so nothing over the limit is ever allocated.
//! - **Writing:** a temp file in the destination folder through a
//!   cancellable writer ([`Writer`] streams the PDF one object at a time),
//!   flushed and synced, then published without replacing anything
//!   ([`publish::finalize`]). On any error or cancel the temp file is
//!   removed. The PDF carries no Info dictionary, no XMP, no dates and no
//!   `/ID`: nothing in it names the source, its folder, the user or the
//!   time.
//! - **Notes:** the references SVGs left out, summed
//!   ([`crate::Note::ImagesLeftOut`]), and the SVG pages drawn below 300
//!   DPI ([`crate::Note::PagesScaled`]).
//! - **Errors:** a file that can't be read stops the whole PDF; the message
//!   names that file the way the raster, HEIC/AVIF and SVG engines do. A write
//!   failure names the destination folder. No detail written here holds a
//!   path, a name or file contents; but details passed through from
//!   [`image::decode`] and [`publish`] name the source's path or the
//!   destination folder, as [`GrapeError::detail`] says, so a log replaces
//!   a job's paths before writing one (the app's `failure_log`).
//! - **Cancel** is checked before each input, every [`flate::CANCEL_ROWS`]
//!   rows, at every write into a stream's buffer and of the file.
//!   **Progress** for input `i` of `n`: decoded at `(i + 0.5) / n` (the
//!   image helper's and the SVG worker's own fractions within
//!   `[i / n, (i + 0.5) / n]`), written
//!   at `(i + 1) / n`, and 1.0 once the PDF is published.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::Path;

use ::image::codecs::jpeg::JpegEncoder;
use ::image::metadata::Orientation;
use ::image::{ColorType, DynamicImage, GenericImageView};
use grape_core::{Format, Job};

use super::flate::{self, FlateError, Rows};
use super::jpeg::{self, Passthrough};
use super::writer::{num, Sink, Writer, MAX_OFFSET, PAGES};
use crate::cancel::Cancellable;
use crate::heif::{self, Decoded};
use crate::image;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::svg::{self, Drawn};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome, Note, SidecarPaths};

const MIB: u64 = 1024 * 1024;

/// The longest side a page may have, in points: Acrobat's 200 inches.
pub(crate) const MAX_SIDE: f64 = 14_400.0;

/// Points per CSS pixel: 96 CSS px = 72 pt = 1 inch.
pub(crate) const POINTS_PER_CSS_PIXEL: f64 = 0.75;

/// The longest side, in pixels, of an image object Grape writes: JPEG's
/// own limit, so no reader meets a larger image from Grape than a JPEG can
/// be. PDFium (Chrome's and Edge's PDF viewer) draws nothing for an image
/// with a side over 131,071, so a longer picture is written as strips
/// drawn edge to edge on its page ([`tiles`]).
pub(crate) const MAX_IMAGE_SIDE: u32 = 65_535;

/// The quality pictures are re-encoded at: the raster engine's JPEG
/// quality.
const JPEG_QUALITY: u8 = 90;

/// The raster target whose working-set estimate the decoders are held to
/// (see the module docs).
const FIT: Format = Format::Png;

/// The buffer of the writer the PDF goes through.
const WRITE_BUFFER: usize = 1 << 20;

/// About how many bytes of a picture that isn't 8-bit are converted at a
/// time (at least one row, at most [`flate::CANCEL_ROWS`]).
const BATCH_BYTES: u64 = 4 * MIB;

/// The most RIFF chunks read to tell a lossy WebP from a lossless one.
const MAX_WEBP_CHUNKS: u32 = 1 << 16;

/// The formats Create PDF takes without a helper: the images FORMAT_MATRIX
/// gives Create PDF ("Tools per input") that this build reads. BMP
/// converts to PDF, but FORMAT_MATRIX gives it no tools, so the wheel never
/// offers Create PDF with one and the engine doesn't take one either.
const COMBINES: [Format; 4] = [Format::Jpg, Format::Png, Format::Webp, Format::Tiff];

/// True when this build puts an input of format `from` on a PDF page:
/// JPG, PNG, WebP, TIFF and BMP always, HEIC and AVIF with the helper
/// ([`heif::HEIF`]), SVG with the SVG worker (D39).
pub(crate) fn reads(from: Format, sidecars: &SidecarPaths) -> bool {
    image::RASTER.contains(&from)
        || (heif::HEIF.contains(&from) && sidecars.heif.is_some())
        || (from == Format::Svg && sidecars.svg_worker.is_some())
}

/// True when Create PDF takes an input of format `from` in this build:
/// JPG, PNG, WebP and TIFF always, HEIC with the helper and SVG with the
/// SVG worker ([`COMBINES`]). Never AVIF: FORMAT_MATRIX gives it no tools
/// in v1, so AVIF → PDF is a conversion only.
pub(crate) fn combines(from: Format, sidecars: &SidecarPaths) -> bool {
    COMBINES.contains(&from)
        || (from == Format::Heic && sidecars.heif.is_some())
        || (from == Format::Svg && sidecars.svg_worker.is_some())
}

/// The helper processes that put a HEIC, an AVIF or an SVG on a page.
#[derive(Default)]
pub(crate) struct Helpers {
    /// Grape's image helper, for HEIC and AVIF.
    pub(crate) heif: Option<heif::Decoder>,
    /// Grape's SVG worker.
    pub(crate) svg: Option<svg::Renderer>,
}

impl Helpers {
    /// The ones the app found, with Grape's limits.
    pub(crate) fn from_sidecars(sidecars: &SidecarPaths) -> Helpers {
        Helpers {
            heif: heif::Decoder::from_sidecars(sidecars),
            svg: svg::Renderer::from_sidecars(sidecars),
        }
    }
}

/// Converts `src` (of format `from`, by its name) to a one-page PDF at
/// `dest`, the path the plan reserved.
pub(crate) fn convert(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let helpers = Helpers::from_sidecars(&ctx.sidecars);
    convert_with(job, src, dest, from, ctx, progress, &helpers)
}

/// [`convert`] with these helpers.
pub(crate) fn convert_with(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    helpers: &Helpers,
) -> Result<JobOutcome, GrapeError> {
    if !image::RASTER.contains(&from) && !heif::HEIF.contains(&from) && from != Format::Svg {
        return Err(crate::not_yet_supported(job, Some(from)));
    }
    write_pdf(job, &[(src, from)], dest, ctx, progress, helpers)
}

/// Create PDF: one page per input, in the job's order, at `dest`.
pub(crate) fn combine(
    job: &Job,
    dest: &Path,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let helpers = Helpers::from_sidecars(&ctx.sidecars);
    combine_with(job, dest, ctx, progress, &helpers)
}

/// [`combine`] with these helpers.
pub(crate) fn combine_with(
    job: &Job,
    dest: &Path,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    helpers: &Helpers,
) -> Result<JobOutcome, GrapeError> {
    let mut inputs = Vec::with_capacity(job.inputs.len());
    for src in &job.inputs {
        match crate::format_of(src) {
            Some(from)
                if COMBINES.contains(&from) || matches!(from, Format::Heic | Format::Svg) =>
            {
                inputs.push((src.as_path(), from));
            }
            from => return Err(crate::not_yet_supported(job, from)),
        }
    }
    write_pdf(job, &inputs, dest, ctx, progress, helpers)
}

/// Progress passed on only forwards.
struct Forward<'a> {
    report: &'a mut dyn FnMut(f32),
    last: f32,
}

impl Forward<'_> {
    fn to(&mut self, fraction: f32) {
        if fraction > self.last {
            self.last = fraction;
            (self.report)(fraction);
        }
    }
}

/// What the toast hears about the pages written.
#[derive(Debug, Default)]
struct Tally {
    /// The references SVGs left out, summed.
    left_out: u32,
    /// SVG pages drawn below 300 DPI.
    scaled_pages: u32,
}

impl Tally {
    /// The notes, in the toast's order.
    fn notes(&self) -> Vec<Note> {
        let mut notes = Vec::new();
        if self.left_out > 0 {
            notes.push(Note::ImagesLeftOut(self.left_out));
        }
        if self.scaled_pages > 0 {
            notes.push(Note::PagesScaled(self.scaled_pages));
        }
        notes
    }
}

/// Writes one page per input into a temp file in `dest`'s folder and
/// publishes it there. `helpers` decode HEIC and AVIF inputs and draw
/// SVGs.
fn write_pdf(
    job: &Job,
    inputs: &[(&Path, Format)],
    dest: &Path,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    helpers: &Helpers,
) -> Result<JobOutcome, GrapeError> {
    let cancel = &ctx.cancel;
    let Some(&(first, first_from)) = inputs.first() else {
        return Err(crate::not_yet_supported(job, None));
    };
    let names = Names::new(first, dest, first_from, Format::Pdf);
    let dir = publish::dest_dir(dest, &names)?;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    publish::ensure_dir(job, ctx, dir, &names)?;
    // From here on, `temp` removes the temp file on every return path.
    let (temp, file) = TempFile::create(dir, Format::Pdf.output_ext())
        .map_err(|e| names.write_failed(format!("creating the temp file: {}", io_detail(&e))))?;
    let out = BufWriter::with_capacity(WRITE_BUFFER, Cancellable::new(file, cancel));
    let mut pdf = Writer::begin(out).map_err(|e| write_error(&names, cancel, &e))?;

    let mut forward = Forward {
        report: progress,
        last: 0.0,
    };
    let mut tally = Tally::default();
    let n = inputs.len() as f32;
    for (i, &(src, from)) in inputs.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let input = Names::new(src, dest, from, Format::Pdf);
        let at = i as f32;
        let span = (at / n, (at + 0.5) / n);
        let picture = {
            let mut helper_progress = |fraction: f32| forward.to(fraction);
            read(
                src,
                from,
                &input,
                cancel,
                helpers,
                span,
                &mut helper_progress,
                &mut tally,
            )?
        };
        forward.to(span.1);
        write_page(&mut pdf, picture, &input, &names, cancel)?;
        if pdf.position() > MAX_OFFSET {
            return Err(names
                .encode_failed("the PDF is too large for its cross-reference table".to_string()));
        }
        if i + 1 < inputs.len() {
            forward.to((at + 1.0) / n);
        }
    }

    let out = pdf.finish().map_err(|e| write_error(&names, cancel, &e))?;
    let file = out
        .into_inner()
        .map_err(|e| write_error(&names, cancel, e.error()))?
        .into_inner();
    file.sync_all()
        .map_err(|e| write_error(&names, cancel, &e))?;
    drop(file);
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let mut outcome = publish::finalize(&temp, job, first, dest, dir, &names)?;
    drop(temp);
    outcome.notes = tally.notes();
    forward.to(1.0);
    Ok(outcome)
}

/// How a picture's pixels are stored when they aren't passed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    /// Re-encoded as JPEG (HEIC, AVIF, lossy WebP, a JPEG that can't pass
    /// through).
    Lossy,
    /// Flate with PNG predictors (and SVG: vector art's flat colors and
    /// hard edges, which JPEG smears).
    Lossless,
}

/// One input, ready to be written as a page.
enum Picture {
    /// A JPEG passed through: the file's `bytes`, what of them is kept, and
    /// its ICC profile if it describes the data.
    Jpeg {
        bytes: Vec<u8>,
        pass: Passthrough,
        icc: Option<Vec<u8>>,
    },
    /// Upright decoded pixels, their ICC profile if it describes them, how
    /// to store them, and the page in points (`None`: [`page_size`] of the
    /// pixels; an SVG's own size otherwise).
    Pixels {
        img: DynamicImage,
        icc: Option<Vec<u8>>,
        storage: Storage,
        page: Option<(f64, f64)>,
    },
}

/// Decodes `src`: HEIC and AVIF through the helper, HEIC or AVIF by the
/// content whatever the name (one that isn't HEIF at all through the
/// raster engine, as HEIC → JPG does), SVG through the SVG worker (one
/// whose content is a raster image through the raster engine, rule 3),
/// the rest through the raster engine. A HEIC's or an AVIF's page is
/// stored lossy: they're a photo codec's output. An SVG's is lossless, on
/// a page of its own size; what it left out and whether it was drawn
/// below 300 DPI go in `tally`.
#[allow(clippy::too_many_arguments)] // The input, the helpers and the run.
fn read(
    src: &Path,
    from: Format,
    names: &Names,
    cancel: &CancelToken,
    helpers: &Helpers,
    span: (f32, f32),
    progress: &mut dyn FnMut(f32),
    tally: &mut Tally,
) -> Result<Picture, GrapeError> {
    if heif::HEIF.contains(&from) {
        let Some(helper) = &helpers.heif else {
            return Err(GrapeError::new(
                ErrorKind::NotYetSupported,
                "Grape's image helper is missing.",
                "SidecarPaths::heif is None",
            ));
        };
        match helper.decode(src, names, FIT, cancel, span, progress)? {
            Decoded::Pixels(img, icc) => return pixels(img, icc, Storage::Lossy, None, names),
            Decoded::NotHeif => {}
        }
    }
    if from == Format::Svg {
        let Some(worker) = &helpers.svg else {
            return Err(GrapeError::new(
                ErrorKind::NotYetSupported,
                "Grape's SVG helper is missing.",
                "SidecarPaths::svg_worker is None",
            ));
        };
        match worker.render(src, names, svg::Mode::Page, FIT, cancel, span, progress)? {
            Drawn::Svg(drawn) => {
                tally.left_out = tally.left_out.saturating_add(drawn.left_out);
                if drawn.scaled {
                    tally.scaled_pages = tally.scaled_pages.saturating_add(1);
                }
                let page = svg_page(drawn.css);
                return pixels(drawn.img, None, Storage::Lossless, Some(page), names);
            }
            Drawn::Raster => {}
        }
    }
    raster(src, from, names, cancel)
}

/// The raster engine's decode of `src`, and how its content says to store
/// it.
fn raster(
    src: &Path,
    from: Format,
    names: &Names,
    cancel: &CancelToken,
) -> Result<Picture, GrapeError> {
    let (img, icc) =
        image::decode(src, from, FIT, names, cancel).map_err(|e| or_cancelled(cancel, e))?;
    let content =
        sniff(src).map_err(|e| names.read_failed(format!("reading it: {}", io_detail(&e))))?;
    match content {
        Content::Jpeg => match jpeg_data(src, &img, names, cancel)? {
            Some((bytes, pass)) => {
                let icc = icc.filter(|profile| image::icc_fits(profile, &img));
                drop(img);
                Ok(Picture::Jpeg { bytes, pass, icc })
            }
            None => pixels(img, icc, Storage::Lossy, None, names),
        },
        Content::LossyWebp => pixels(img, icc, Storage::Lossy, None, names),
        Content::Other => pixels(img, icc, Storage::Lossless, None, names),
    }
}

/// `img` and its profile on `page`, if what writing them holds fits the
/// working set ([`page_memory`]).
fn pixels(
    img: DynamicImage,
    icc: Option<Vec<u8>>,
    storage: Storage,
    page: Option<(f64, f64)>,
    names: &Names,
) -> Result<Picture, GrapeError> {
    let icc = icc.filter(|profile| image::icc_fits(profile, &img));
    let profile = icc.as_ref().map_or(0, |icc| icc.len() as u64);
    let needed = page_memory(&img, storage, profile);
    if needed > image::WORKING_SET_LIMIT {
        return Err(names.too_large(format!(
            "{}x{} {:?} to PDF needs about {} MiB, over the {} MiB limit",
            img.width(),
            img.height(),
            img.color(),
            needed / MIB,
            image::WORKING_SET_LIMIT / MIB
        )));
    }
    Ok(Picture::Pixels {
        img,
        icc,
        storage,
        page,
    })
}

/// The most memory writing `img` as a page holds at once: the decoded
/// pixels; for a lossy picture that isn't 8-bit, its 8-bit copy; the
/// larger of the color stream's and the soft mask's buffers, each reserved
/// at its bound; and the profile with its compressed copy.
fn page_memory(img: &DynamicImage, storage: Storage, profile: u64) -> u64 {
    let (width, height) = img.dimensions();
    let color = img.color();
    let channels = if is_gray(color) { 1 } else { 3 };
    let pixels = u64::from(width) * u64::from(height);
    let held = pixels.saturating_mul(u64::from(color.bytes_per_pixel()));
    let stream = flate::bound(filtered(width, height, channels, 8));
    let mask = if color.has_alpha() {
        flate::bound(filtered(width, height, 1, 8))
    } else {
        0
    };
    let converted = if storage == Storage::Lossy && !is_eight_bit(color) {
        pixels.saturating_mul(u64::from(channels))
    } else {
        0
    };
    held.saturating_add(converted)
        .saturating_add(stream.max(mask))
        .saturating_add(profile.saturating_mul(2))
}

/// The bytes Flate's predictors filter for `height` rows of `width`
/// pixels: a type byte and the row.
fn filtered(width: u32, height: u32, channels: u8, bits: u8) -> u64 {
    u64::from(height).saturating_mul(flate::row_bytes(width, channels, bits) + 1)
}

/// What a source's first bytes say it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Content {
    /// JPEG (`FF D8 FF`), whatever its name.
    Jpeg,
    /// A WebP whose (first) image is a `VP8 ` (lossy) bitstream.
    LossyWebp,
    /// Anything else: stored losslessly.
    Other,
}

/// Sniffs `src`, opened read-only.
fn sniff(src: &Path) -> io::Result<Content> {
    let mut file = BufReader::new(File::open(src)?);
    let mut head = [0u8; 12];
    let mut got = 0;
    while got < head.len() {
        match file.read(&mut head[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let head = &head[..got];
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok(Content::Jpeg);
    }
    if head.len() == 12 && &head[..4] == b"RIFF" && &head[8..] == b"WEBP" {
        return Ok(if webp_is_lossy(&mut file) {
            Content::LossyWebp
        } else {
            Content::Other
        });
    }
    Ok(Content::Other)
}

/// True when the WebP whose chunks start at byte 12 of `r` holds a lossy
/// image: a `VP8 ` chunk (simple, or in a `VP8X` file) or, for an
/// animation, in its first frame, before any `VP8L`. One that can't be
/// told is stored losslessly.
fn webp_is_lossy<R: Read + Seek>(r: &mut R) -> bool {
    let le = |b: &[u8]| u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let mut header = [0u8; 8];
    let mut at = 12u64;
    let mut frame_end: Option<u64> = None;
    for _ in 0..MAX_WEBP_CHUNKS {
        if frame_end.is_some_and(|end| at >= end) {
            return false;
        }
        if r.seek(SeekFrom::Start(at)).is_err() || r.read_exact(&mut header).is_err() {
            return false;
        }
        let size = le(&header[4..]);
        match &header[..4] {
            b"VP8 " => return true,
            b"VP8L" => return false,
            // The first frame: 16 bytes of its header, then its chunks.
            b"ANMF" if frame_end.is_none() => {
                frame_end = Some(at.saturating_add(8).saturating_add(size));
                at = at.saturating_add(8 + 16);
                continue;
            }
            _ => {}
        }
        at = at.saturating_add(8).saturating_add(size + (size & 1));
    }
    false
}

/// The JPEG at `src` again, read whole, if it can pass through: its
/// structure passes [`jpeg::passthrough`] and it is the picture the
/// decoder gave (`img`: the same size once oriented, gray or RGB). `None`
/// sends it to the re-encode path.
fn jpeg_data(
    src: &Path,
    img: &DynamicImage,
    names: &Names,
    cancel: &CancelToken,
) -> Result<Option<(Vec<u8>, Passthrough)>, GrapeError> {
    let read_failed = |e: io::Error| {
        if cancel.is_cancelled() {
            cancelled()
        } else {
            names.read_failed(format!("reading it again: {}", io_detail(&e)))
        }
    };
    let file = File::open(src).map_err(read_failed)?;
    let len = file.metadata().map_err(read_failed)?.len();
    if len > image::MAX_SOURCE {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    // Reserves the file's size once, then reads it in cancellable steps.
    Cancellable::new(file, cancel)
        .read_to_end(&mut bytes)
        .map_err(read_failed)?;
    if bytes.len() as u64 != len {
        // It changed since it was decoded.
        return Ok(None);
    }
    let Some(pass) = jpeg::passthrough(&bytes) else {
        return Ok(None);
    };
    let upright = if swaps_sides(pass.orientation) {
        (pass.height, pass.width)
    } else {
        (pass.width, pass.height)
    };
    let color = match pass.components {
        1 => ColorType::L8,
        _ => ColorType::Rgb8,
    };
    if upright != img.dimensions() || img.color() != color {
        return Ok(None);
    }
    Ok(Some((bytes, pass)))
}

/// True for the orientations that turn the image a quarter.
fn swaps_sides(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// The page, in points, for an upright `width` × `height` image: one pixel
/// a point, scaled down uniformly when the longer side is over
/// [`MAX_SIDE`]; a shorter side that would then be under 1 pt is 1 pt.
pub(crate) fn page_size(width: u32, height: u32) -> (f64, f64) {
    page_points(f64::from(width), f64::from(height))
}

/// A page of `w` × `h` points, scaled down uniformly when the longer side
/// is over [`MAX_SIDE`], each side at least 1 pt.
fn page_points(w: f64, h: f64) -> (f64, f64) {
    let long = w.max(h);
    let scale = if long > MAX_SIDE {
        MAX_SIDE / long
    } else {
        1.0
    };
    ((w * scale).max(1.0), (h * scale).max(1.0))
}

/// An SVG's page: its own size, `css` CSS pixels at 0.75 pt each (a 210 mm
/// SVG makes an A4-wide page), held as [`page_size`] holds a picture's.
pub(crate) fn svg_page(css: (f64, f64)) -> (f64, f64) {
    page_points(css.0 * POINTS_PER_CSS_PIXEL, css.1 * POINTS_PER_CSS_PIXEL)
}

/// The `cm` matrix `[a b c d e f]` that maps an image's unit square onto a
/// `page` (width, height) so that an image stored with EXIF `orientation`
/// shows upright. The image's first row is at the unit square's top.
pub(crate) fn placement(orientation: Orientation, page: (f64, f64)) -> [f64; 6] {
    let (w, h) = page;
    match orientation {
        Orientation::NoTransforms => [w, 0.0, 0.0, h, 0.0, 0.0],
        Orientation::FlipHorizontal => [-w, 0.0, 0.0, h, w, 0.0],
        Orientation::Rotate180 => [-w, 0.0, 0.0, -h, w, h],
        Orientation::FlipVertical => [w, 0.0, 0.0, -h, 0.0, h],
        Orientation::Rotate90 => [0.0, -h, w, 0.0, 0.0, h],
        Orientation::Rotate270 => [0.0, h, -w, 0.0, w, 0.0],
        Orientation::Rotate90FlipH => [0.0, -h, -w, 0.0, w, h],
        Orientation::Rotate270FlipH => [0.0, h, w, 0.0, 0.0, 0.0],
    }
}

/// A part of a picture written as one image object: its columns and rows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tile {
    x: Range<u32>,
    y: Range<u32>,
}

impl Tile {
    fn width(&self) -> u32 {
        self.x.end - self.x.start
    }

    fn height(&self) -> u32 {
        self.y.end - self.y.start
    }
}

/// `side` pixels split into the fewest near-equal spans of at most
/// [`MAX_IMAGE_SIDE`].
fn spans(side: u32) -> Vec<Range<u32>> {
    let n = side.div_ceil(MAX_IMAGE_SIDE).max(1);
    let at = |k: u32| u32::try_from(u64::from(side) * u64::from(k) / u64::from(n)).unwrap_or(side);
    (0..n).map(|k| at(k)..at(k + 1)).collect()
}

/// The tiles a `width` × `height` picture is written as, from the top row
/// of tiles down: the whole picture, unless a side is over
/// [`MAX_IMAGE_SIDE`]. (Both sides can't be: that's over the working set.)
fn tiles(width: u32, height: u32) -> Vec<Tile> {
    let columns = spans(width);
    spans(height)
        .into_iter()
        .flat_map(|y| {
            columns.iter().map(move |x| Tile {
                x: x.clone(),
                y: y.clone(),
            })
        })
        .collect()
}

/// The `cm` matrix that draws `tile` of an upright picture of `size`
/// pixels whose whole covers `page`: edge to edge with its neighbors, the
/// picture's first row at the page's top.
fn tile_placement(tile: &Tile, size: (u32, u32), page: (f64, f64)) -> [f64; 6] {
    let (width, height) = (f64::from(size.0), f64::from(size.1));
    let left = page.0 * f64::from(tile.x.start) / width;
    let right = page.0 * f64::from(tile.x.end) / width;
    let top = page.1 * f64::from(size.1 - tile.y.start) / height;
    let bottom = page.1 * f64::from(size.1 - tile.y.end) / height;
    [right - left, 0.0, 0.0, top - bottom, left, bottom]
}

/// True for gray layouts (with or without alpha): the PDF gets
/// `/DeviceGray`, as [`image::icc_fits`] decides for a profile.
fn is_gray(color: ColorType) -> bool {
    matches!(
        color,
        ColorType::L8 | ColorType::La8 | ColorType::L16 | ColorType::La16
    )
}

/// True for the layouts whose samples are already 8-bit.
fn is_eight_bit(color: ColorType) -> bool {
    matches!(
        color,
        ColorType::L8 | ColorType::La8 | ColorType::Rgb8 | ColorType::Rgba8
    )
}

/// Writes `picture` as the next page.
fn write_page<W: Write>(
    pdf: &mut Writer<W>,
    picture: Picture,
    input: &Names,
    out: &Names,
    cancel: &CancelToken,
) -> Result<(), GrapeError> {
    let write = |e: io::Error| write_error(out, cancel, &e);
    match picture {
        Picture::Jpeg { bytes, pass, icc } => {
            let upright = if swaps_sides(pass.orientation) {
                (pass.height, pass.width)
            } else {
                (pass.width, pass.height)
            };
            let page = page_size(upright.0, upright.1);
            let gray = pass.components == 1;
            let image = pdf.reserve();
            let profile = icc.as_ref().map(|_| pdf.reserve());
            let mut entries = format!(
                "/Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace {} \
                 /BitsPerComponent 8 /Filter /DCTDecode",
                pass.width,
                pass.height,
                color_space(gray, profile)
            );
            if pass.rgb {
                // DCTDecode's own parameter (ISO 32000-1, table 13). The
                // Adobe marker that is kept says the same.
                entries.push_str(" /DecodeParms << /ColorTransform 0 >>");
            }
            pdf.stream_parts(image, &entries, pass.parts(&bytes))
                .map_err(write)?;
            drop(bytes);
            if let (Some(number), Some(icc)) = (profile, icc) {
                write_icc(pdf, number, &icc, gray, input, out, cancel)?;
            }
            let drawn = [(image, placement(pass.orientation, page))];
            finish_page(pdf, &drawn, page).map_err(write)
        }
        Picture::Pixels {
            img,
            icc,
            storage,
            page,
        } => {
            let size = img.dimensions();
            let gray = is_gray(img.color());
            let scan = scan(&img, storage == Storage::Lossless, cancel)?;
            let page = match page {
                Some((w, h)) => page_points(w, h),
                None => page_size(size.0, size.1),
            };
            let profile = icc.as_ref().map(|_| pdf.reserve());
            let space = color_space(gray, profile);
            let lossy = match storage {
                Storage::Lossy => jpeg_stream(&img, gray, cancel)?,
                Storage::Lossless => None,
            };
            // Each image object drawn, and its matrix.
            let mut drawn = Vec::new();
            match lossy {
                Some(data) => {
                    // One JPEG: `jpeg_stream` takes no side over
                    // MAX_IMAGE_SIDE.
                    let whole = Tile {
                        x: 0..size.0,
                        y: 0..size.1,
                    };
                    let image = pdf.reserve();
                    let mask = scan.alpha.then(|| pdf.reserve());
                    let entries = image_entries(
                        &whole,
                        &space,
                        "/BitsPerComponent 8 /Filter /DCTDecode",
                        mask,
                    );
                    pdf.stream(image, &entries, &data).map_err(write)?;
                    drop(data);
                    if let Some(mask) = mask {
                        write_mask(pdf, mask, &img, &whole, input, out, cancel)?;
                    }
                    drawn.push((image, placement(Orientation::NoTransforms, page)));
                }
                None => {
                    let bw = scan.black_and_white && storage == Storage::Lossless;
                    let (channels, bits) = match (gray, bw) {
                        (true, true) => (1u8, 1u8),
                        (true, false) => (1, 8),
                        _ => (3, 8),
                    };
                    for tile in tiles(size.0, size.1) {
                        let image = pdf.reserve();
                        let mask = scan.alpha.then(|| pdf.reserve());
                        let coding = format!(
                            "/BitsPerComponent {bits} /Filter /FlateDecode /DecodeParms \
                             << /Predictor 15 /Colors {channels} /BitsPerComponent {bits} \
                             /Columns {} >>",
                            tile.width()
                        );
                        let entries = image_entries(&tile, &space, &coding, mask);
                        let data = color_stream(&img, &tile, gray, bw, cancel)
                            .map_err(|e| flate_error(e, input, out))?;
                        pdf.stream(image, &entries, &data).map_err(write)?;
                        drop(data);
                        if let Some(mask) = mask {
                            write_mask(pdf, mask, &img, &tile, input, out, cancel)?;
                        }
                        drawn.push((image, tile_placement(&tile, size, page)));
                    }
                }
            }
            if let (Some(number), Some(icc)) = (profile, icc) {
                write_icc(pdf, number, &icc, gray, input, out, cancel)?;
            }
            drop(img);
            finish_page(pdf, &drawn, page).map_err(write)
        }
    }
}

/// An image object's dictionary entries for `tile`: its size, color space
/// `space`, the `coding` entries (bits, filter and its parameters) and its
/// soft mask, if any.
fn image_entries(tile: &Tile, space: &str, coding: &str, mask: Option<u32>) -> String {
    let mut entries = format!(
        "/Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace {space} {coding}",
        tile.width(),
        tile.height()
    );
    if let Some(mask) = mask {
        entries.push_str(&format!(" /SMask {mask} 0 R"));
    }
    entries
}

/// `tile` of `img`'s alpha as soft mask object `mask`.
fn write_mask<W: Write>(
    pdf: &mut Writer<W>,
    mask: u32,
    img: &DynamicImage,
    tile: &Tile,
    input: &Names,
    out: &Names,
    cancel: &CancelToken,
) -> Result<(), GrapeError> {
    let data = alpha_stream(img, tile, cancel).map_err(|e| flate_error(e, input, out))?;
    let coding = format!(
        "/BitsPerComponent 8 /Filter /FlateDecode /DecodeParms << /Predictor 15 /Colors 1 \
         /BitsPerComponent 8 /Columns {} >>",
        tile.width()
    );
    let entries = image_entries(tile, "/DeviceGray", &coding, None);
    pdf.stream(mask, &entries, &data)
        .map_err(|e| write_error(out, cancel, &e))
}

/// `/DeviceGray`, `/DeviceRGB`, or the ICC profile object `profile`.
fn color_space(gray: bool, profile: Option<u32>) -> String {
    match (profile, gray) {
        (Some(number), _) => format!("[/ICCBased {number} 0 R]"),
        (None, true) => "/DeviceGray".to_string(),
        (None, false) => "/DeviceRGB".to_string(),
    }
}

/// The ICC profile stream, deflated.
fn write_icc<W: Write>(
    pdf: &mut Writer<W>,
    number: u32,
    icc: &[u8],
    gray: bool,
    input: &Names,
    out: &Names,
    cancel: &CancelToken,
) -> Result<(), GrapeError> {
    let data = flate::deflate(icc, cancel).map_err(|e| flate_error(e, input, out))?;
    let entries = if gray {
        "/N 1 /Alternate /DeviceGray /Filter /FlateDecode"
    } else {
        "/N 3 /Alternate /DeviceRGB /Filter /FlateDecode"
    };
    pdf.stream(number, entries, &data)
        .map_err(|e| write_error(out, cancel, &e))
}

/// The page's content stream and the page itself, added to the tree:
/// each image object in `drawn` (`/Im0`, `/Im1`, …) drawn with its matrix.
fn finish_page<W: Write>(
    pdf: &mut Writer<W>,
    drawn: &[(u32, [f64; 6])],
    page: (f64, f64),
) -> io::Result<()> {
    let content = pdf.reserve();
    let number = pdf.reserve();
    let mut ops = Vec::with_capacity(drawn.len());
    let mut resources = String::new();
    for (k, (image, matrix)) in drawn.iter().enumerate() {
        let cm: Vec<String> = matrix.iter().map(|&v| num(v)).collect();
        ops.push(format!("q {} cm /Im{k} Do Q", cm.join(" ")));
        resources.push_str(&format!(" /Im{k} {image} 0 R"));
    }
    pdf.stream(content, "", ops.join("\n").as_bytes())?;
    let dict = format!(
        "<< /Type /Page /Parent {PAGES} 0 R /MediaBox [0 0 {} {}] \
         /Resources << /XObject <<{resources} >> >> /Contents {content} 0 R >>",
        num(page.0),
        num(page.1)
    );
    pdf.object(number, dict.as_bytes())?;
    pdf.add_page(number);
    Ok(())
}

/// What a scan of the pixels found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scan {
    /// Some pixel's 8-bit alpha is under 255: the page gets a soft mask.
    alpha: bool,
    /// Gray, and every 8-bit sample is 0 or 255 (only looked for when
    /// `find_black_and_white`).
    black_and_white: bool,
}

/// Scans `img`'s 8-bit rows for alpha under 255 and, if asked, for gray
/// that is all black and white.
fn scan(
    img: &DynamicImage,
    find_black_and_white: bool,
    cancel: &CancelToken,
) -> Result<Scan, GrapeError> {
    let color = img.color();
    let has_alpha = color.has_alpha();
    let mut found = Scan {
        alpha: false,
        black_and_white: find_black_and_white && is_gray(color),
    };
    if !has_alpha && !found.black_and_white {
        return Ok(found);
    }
    let mut rows = RowReader::new(img);
    for y in 0..img.height() {
        if y % flate::CANCEL_ROWS == 0 && cancel.is_cancelled() {
            return Err(cancelled());
        }
        let (row, step) = rows.row(y);
        for pixel in row.chunks_exact(step) {
            if found.black_and_white && pixel[0] != 0 && pixel[0] != 255 {
                found.black_and_white = false;
            }
            if has_alpha && pixel[step - 1] != 255 {
                found.alpha = true;
            }
        }
        if !found.black_and_white && (found.alpha || !has_alpha) {
            break;
        }
    }
    Ok(found)
}

/// `img` re-encoded as JPEG at [`JPEG_QUALITY`] (gray or RGB; alpha goes
/// in the soft mask), into a buffer the size of the lossless stream's
/// bound. `None` when the encoder refuses the image (a side over 65,535) or
/// its output would be larger than that bound: the caller stores it
/// losslessly instead.
fn jpeg_stream(
    img: &DynamicImage,
    gray: bool,
    cancel: &CancelToken,
) -> Result<Option<Vec<u8>>, GrapeError> {
    let (width, height) = img.dimensions();
    if width > u32::from(u16::MAX) || height > u32::from(u16::MAX) {
        return Ok(None);
    }
    let channels = if gray { 1 } else { 3 };
    let Ok(cap) = usize::try_from(flate::bound(filtered(width, height, channels, 8))) else {
        return Ok(None);
    };
    let Ok(mut sink) = Sink::with_cap(cap, cancel) else {
        return Ok(None);
    };
    let encoded = {
        let mut encoder = JpegEncoder::new_with_quality(&mut sink, JPEG_QUALITY);
        match img {
            DynamicImage::ImageLuma8(buffer) => encoder.encode_image(buffer),
            DynamicImage::ImageLumaA8(buffer) => encoder.encode_image(buffer),
            DynamicImage::ImageRgb8(buffer) => encoder.encode_image(buffer),
            DynamicImage::ImageRgba8(buffer) => encoder.encode_image(buffer),
            // Not 8-bit: one converted copy (counted in `page_memory`).
            other if gray => encoder.encode_image(&other.to_luma8()),
            other => encoder.encode_image(&other.to_rgb8()),
        }
    };
    match encoded {
        Ok(()) => Ok(Some(sink.into_inner())),
        Err(_) if cancel.is_cancelled() => Err(cancelled()),
        Err(_) => Ok(None),
    }
}

/// `tile` of `img`'s gray or RGB samples, 8-bit (or 1-bit when `bw`), as a
/// Flate stream with PNG predictors.
fn color_stream(
    img: &DynamicImage,
    tile: &Tile,
    gray: bool,
    bw: bool,
    cancel: &CancelToken,
) -> Result<Vec<u8>, FlateError> {
    let width = tile.width();
    let channels = if gray { 1 } else { 3 };
    let bits = if bw { 1 } else { 8 };
    let row_len =
        usize::try_from(flate::row_bytes(width, channels, bits)).map_err(|_| FlateError::Memory)?;
    let shape = Rows {
        height: tile.height(),
        row_len,
        bpp: if bw { 1 } else { usize::from(channels) },
    };
    let columns = tile.x.start as usize..tile.x.end as usize;
    let mut rows = RowReader::new(img);
    let mut samples = vec![0u8; if bw { width as usize } else { 0 }];
    flate::deflate_rows(shape, cancel, |y, out| {
        let y = tile.y.start + y;
        if bw {
            rows.color(y, gray, columns.clone(), &mut samples);
            flate::pack_bits(&samples, out);
        } else {
            rows.color(y, gray, columns.clone(), out);
        }
    })
}

/// `tile` of `img`'s 8-bit alpha as a Flate stream with PNG predictors.
fn alpha_stream(
    img: &DynamicImage,
    tile: &Tile,
    cancel: &CancelToken,
) -> Result<Vec<u8>, FlateError> {
    let shape = Rows {
        height: tile.height(),
        row_len: usize::try_from(tile.width()).map_err(|_| FlateError::Memory)?,
        bpp: 1,
    };
    let columns = tile.x.start as usize..tile.x.end as usize;
    let mut rows = RowReader::new(img);
    flate::deflate_rows(shape, cancel, |y, out| {
        rows.alpha(tile.y.start + y, columns.clone(), out);
    })
}

/// A decoded image's rows as 8-bit samples (gray or RGB, then alpha if it
/// has one): read in place from 8-bit images; others are converted with
/// the `image` crate's own conversions a few rows at a time.
struct RowReader<'a> {
    img: &'a DynamicImage,
    /// For an image that isn't 8-bit: the first row converted, and the
    /// rows from it.
    batch: Option<(u32, DynamicImage)>,
}

impl<'a> RowReader<'a> {
    fn new(img: &'a DynamicImage) -> RowReader<'a> {
        RowReader { img, batch: None }
    }

    /// Row `y` as 8-bit samples, and how many samples a pixel has (1 to
    /// 4: gray or RGB, then alpha if the image has one).
    fn row(&mut self, y: u32) -> (&[u8], usize) {
        let width = self.img.width() as usize;
        let color = self.img.color();
        let step = usize::from(color.channel_count());
        if is_eight_bit(color) {
            let at = y as usize * width * step;
            let bytes = self.img.as_bytes();
            return (&bytes[at..at + width * step], step);
        }
        let first = match &self.batch {
            Some((first, rows)) if (*first..*first + rows.height()).contains(&y) => *first,
            _ => {
                let per_row = u64::from(self.img.width()) * u64::from(color.bytes_per_pixel());
                let count = (BATCH_BYTES / per_row.max(1)).clamp(1, u64::from(flate::CANCEL_ROWS));
                let count = u32::try_from(count).unwrap_or(1).min(self.img.height() - y);
                let part = self.img.crop_imm(0, y, self.img.width(), count);
                let converted = match (is_gray(color), color.has_alpha()) {
                    (true, false) => DynamicImage::ImageLuma8(part.to_luma8()),
                    (true, true) => DynamicImage::ImageLumaA8(part.to_luma_alpha8()),
                    (false, false) => DynamicImage::ImageRgb8(part.to_rgb8()),
                    (false, true) => DynamicImage::ImageRgba8(part.to_rgba8()),
                };
                self.batch = Some((y, converted));
                y
            }
        };
        let rows = match &self.batch {
            Some((_, rows)) => rows.as_bytes(),
            None => &[],
        };
        let at = (y - first) as usize * width * step;
        (&rows[at..at + width * step], step)
    }

    /// The gray (`gray`) or RGB samples of row `y`'s pixels in `columns`
    /// into `out`.
    fn color(&mut self, y: u32, gray: bool, columns: Range<usize>, out: &mut [u8]) {
        let channels = if gray { 1 } else { 3 };
        let (row, step) = self.row(y);
        let row = &row[columns.start * step..columns.end * step];
        if step == channels {
            out.copy_from_slice(row);
        } else {
            for (pixel, to) in row.chunks_exact(step).zip(out.chunks_exact_mut(channels)) {
                to.copy_from_slice(&pixel[..channels]);
            }
        }
    }

    /// The alpha of row `y`'s pixels in `columns` into `out` (255 for an
    /// image without alpha).
    fn alpha(&mut self, y: u32, columns: Range<usize>, out: &mut [u8]) {
        let has_alpha = self.img.color().has_alpha();
        let (row, step) = self.row(y);
        if !has_alpha {
            out.fill(255);
            return;
        }
        let row = &row[columns.start * step..columns.end * step];
        for (pixel, to) in row.chunks_exact(step).zip(out.iter_mut()) {
            *to = pixel[step - 1];
        }
    }
}

/// A Flate stream that couldn't be built, in Grape's words.
fn flate_error(e: FlateError, input: &Names, out: &Names) -> GrapeError {
    match e {
        FlateError::Cancelled => cancelled(),
        FlateError::Memory => input.too_large("its PDF stream doesn't fit in memory".to_string()),
        FlateError::Over(detail) => {
            out.encode_failed(format!("Deflate went past its bound: {detail}"))
        }
    }
}

/// A write of the PDF failed: cancelled, or "couldn't save into" the
/// folder.
fn write_error(names: &Names, cancel: &CancelToken, e: &io::Error) -> GrapeError {
    if cancel.is_cancelled() {
        cancelled()
    } else {
        names.write_failed(format!("writing the PDF: {}", io_detail(e)))
    }
}

/// `e`, or the cancel error when the job was cancelled (the failure may be
/// the cancelled reader).
fn or_cancelled(cancel: &CancelToken, e: GrapeError) -> GrapeError {
    if cancel.is_cancelled() {
        cancelled()
    } else {
        e
    }
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
    use std::fs;
    use std::io::Read;
    use std::path::PathBuf;

    use ::image::codecs::jpeg::JpegEncoder;
    use ::image::{GrayAlphaImage, GrayImage, Luma, LumaA, Rgb, RgbImage, Rgba, RgbaImage};
    use flate2::read::ZlibDecoder;
    use grape_core::{plan, JobOutput, Operation, Settings, ToolKey};
    use lopdf::{Dictionary, Document, Object};

    use super::*;
    use crate::heif::tests::{fake, fake_heic, fake_image, nowhere};
    use crate::output::unique_suffix;
    use crate::pdf::writer::tests::xref;
    use grape_heif_proto::HeifFormat;

    fn names() -> Names {
        Names::new(
            Path::new("a.png"),
            Path::new("out/a.pdf"),
            Format::Png,
            Format::Pdf,
        )
    }

    /// `pictures`, one page each, as a whole PDF in memory.
    fn pdf_of(pictures: Vec<Picture>) -> Vec<u8> {
        let cancel = CancelToken::new();
        let mut pdf = Writer::begin(Vec::new()).unwrap();
        for picture in pictures {
            write_page(&mut pdf, picture, &names(), &names(), &cancel).unwrap();
        }
        pdf.finish().unwrap()
    }

    fn lossless(img: DynamicImage) -> Picture {
        Picture::Pixels {
            img,
            icc: None,
            storage: Storage::Lossless,
            page: None,
        }
    }

    fn lossy(img: DynamicImage) -> Picture {
        Picture::Pixels {
            img,
            icc: None,
            storage: Storage::Lossy,
            page: None,
        }
    }

    /// A page as lopdf reads it.
    struct Page {
        media_box: Vec<f32>,
        image: Dictionary,
        data: Vec<u8>,
        mask: Option<(Dictionary, Vec<u8>)>,
        content: String,
    }

    fn number(object: &Object) -> f32 {
        object.as_float().unwrap()
    }

    fn pages(bytes: &[u8]) -> (Document, Vec<Page>) {
        let doc = Document::load_mem(bytes).unwrap();
        let mut out = Vec::new();
        for (_, id) in doc.get_pages() {
            let page = doc.get_object(id).unwrap().as_dict().unwrap();
            let media_box = page
                .get(b"MediaBox")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(number)
                .collect();
            let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
            let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
            let image_id = xobjects.get(b"Im0").unwrap().as_reference().unwrap();
            let image = doc.get_object(image_id).unwrap().as_stream().unwrap();
            let mask = image.dict.get(b"SMask").ok().map(|mask| {
                let mask = doc
                    .get_object(mask.as_reference().unwrap())
                    .unwrap()
                    .as_stream()
                    .unwrap();
                (mask.dict.clone(), mask.content.clone())
            });
            let content_id = page.get(b"Contents").unwrap().as_reference().unwrap();
            let content = doc.get_object(content_id).unwrap().as_stream().unwrap();
            out.push(Page {
                media_box,
                image: image.dict.clone(),
                data: image.content.clone(),
                mask,
                content: String::from_utf8(content.content.clone()).unwrap(),
            });
        }
        (doc, out)
    }

    fn name(dict: &Dictionary, key: &[u8]) -> String {
        String::from_utf8(dict.get(key).unwrap().as_name().unwrap().to_vec()).unwrap()
    }

    fn int(dict: &Dictionary, key: &[u8]) -> i64 {
        dict.get(key).unwrap().as_i64().unwrap()
    }

    /// An image stream (`dict`, `data`) back to its rows, by lopdf: its
    /// own Flate and PNG predictor decoders, read from the stream's
    /// `/DecodeParms`, which share no code with Grape's.
    fn rows_of(dict: &Dictionary, data: &[u8]) -> Vec<u8> {
        assert_eq!(name(dict, b"Filter"), "FlateDecode");
        let rows = lopdf::Stream::new(dict.clone(), data.to_vec())
            .decompressed_content()
            .unwrap();
        let parms = dict.get(b"DecodeParms").unwrap().as_dict().unwrap();
        let columns =
            int(parms, b"Columns") * int(parms, b"Colors") * int(parms, b"BitsPerComponent");
        let row_len = usize::try_from(columns).unwrap().div_ceil(8);
        let height = usize::try_from(int(dict, b"Height")).unwrap();
        assert_eq!(rows.len(), row_len * height);
        rows
    }

    fn gradient(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            Rgb([
                (x * 7 % 256) as u8,
                (y * 11 % 256) as u8,
                ((x + y) * 5 % 256) as u8,
            ])
        })
    }

    fn jpeg_bytes(img: &DynamicImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 90)
            .encode_image(img)
            .unwrap();
        bytes
    }

    fn passed(bytes: Vec<u8>) -> Picture {
        let pass = jpeg::passthrough(&bytes).unwrap();
        Picture::Jpeg {
            bytes,
            pass,
            icc: None,
        }
    }

    #[test]
    fn one_page_parses_with_its_size_and_image() {
        let img = gradient(5, 3);
        let bytes = pdf_of(vec![lossless(DynamicImage::ImageRgb8(img.clone()))]);
        let (doc, pages) = pages(&bytes);
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        assert_eq!(page.media_box, [0.0, 0.0, 5.0, 3.0]);
        assert_eq!(name(&page.image, b"Subtype"), "Image");
        assert_eq!(
            (int(&page.image, b"Width"), int(&page.image, b"Height")),
            (5, 3)
        );
        assert_eq!(name(&page.image, b"Filter"), "FlateDecode");
        assert_eq!(name(&page.image, b"ColorSpace"), "DeviceRGB");
        assert_eq!(int(&page.image, b"BitsPerComponent"), 8);
        let parms = page.image.get(b"DecodeParms").unwrap().as_dict().unwrap();
        assert_eq!(int(parms, b"Predictor"), 15);
        assert_eq!(int(parms, b"Colors"), 3);
        assert_eq!(int(parms, b"Columns"), 5);
        assert!(page.mask.is_none());
        assert_eq!(rows_of(&page.image, &page.data), img.into_raw());
        assert_eq!(page.content, "q 5 0 0 3 0 0 cm /Im0 Do Q");
        // Every offset points at its object; the trailer names only the
        // size and the root.
        let (offsets, size) = xref(&bytes);
        for (i, &offset) in offsets.iter().enumerate() {
            let at = usize::try_from(offset).unwrap();
            let header = format!("{} 0 obj\n", i + 1);
            assert!(
                bytes[at..].starts_with(header.as_bytes()),
                "object {}",
                i + 1
            );
        }
        assert_eq!(
            doc.trailer.get(b"Size").unwrap().as_i64().unwrap(),
            i64::try_from(size).unwrap()
        );
        assert!(doc.trailer.get(b"Info").is_err());
        assert!(doc.trailer.get(b"ID").is_err());
        let catalog = doc.catalog().unwrap();
        assert!(catalog.get(b"Metadata").is_err());
    }

    #[test]
    fn three_pages_keep_their_order_layouts_and_masks() {
        let gray_alpha = GrayAlphaImage::from_fn(4, 2, |x, _| {
            LumaA([(x * 60) as u8, if x == 0 { 0 } else { 255 }])
        });
        let text = GrayImage::from_fn(9, 3, |x, y| Luma([if (x + y) % 2 == 0 { 0 } else { 255 }]));
        let photo = DynamicImage::ImageRgb8(gradient(16, 8));
        let bytes = pdf_of(vec![
            lossless(DynamicImage::ImageLumaA8(gray_alpha.clone())),
            lossless(DynamicImage::ImageLuma8(text.clone())),
            passed(jpeg_bytes(&photo)),
        ]);
        let (_, pages) = pages(&bytes);
        assert_eq!(pages.len(), 3);

        // Gray with alpha: gray samples, and the alpha as an 8-bit soft mask.
        let first = &pages[0];
        assert_eq!(first.media_box, [0.0, 0.0, 4.0, 2.0]);
        assert_eq!(name(&first.image, b"ColorSpace"), "DeviceGray");
        assert_eq!(int(&first.image, b"BitsPerComponent"), 8);
        let gray: Vec<u8> = gray_alpha.pixels().map(|p| p.0[0]).collect();
        assert_eq!(rows_of(&first.image, &first.data), gray);
        let (mask, mask_data) = first.mask.as_ref().unwrap();
        assert_eq!(name(mask, b"ColorSpace"), "DeviceGray");
        assert_eq!((int(mask, b"Width"), int(mask, b"Height")), (4, 2));
        assert_eq!(int(mask, b"BitsPerComponent"), 8);
        let alpha: Vec<u8> = gray_alpha.pixels().map(|p| p.0[1]).collect();
        assert_eq!(rows_of(mask, mask_data), alpha);

        // Pure black and white: 1 bit a pixel, rows padded to a byte.
        let second = &pages[1];
        assert_eq!(second.media_box, [0.0, 0.0, 9.0, 3.0]);
        assert_eq!(int(&second.image, b"BitsPerComponent"), 1);
        let parms = second.image.get(b"DecodeParms").unwrap().as_dict().unwrap();
        assert_eq!(int(parms, b"BitsPerComponent"), 1);
        assert_eq!(int(parms, b"Columns"), 9);
        let packed = rows_of(&second.image, &second.data);
        for (y, row) in packed.chunks(2).enumerate() {
            for x in 0..9 {
                let bit = row[x / 8] >> (7 - x % 8) & 1;
                let white = text.get_pixel(x as u32, y as u32).0[0] == 255;
                assert_eq!(bit == 1, white, "{x},{y}");
            }
        }
        assert!(second.mask.is_none());

        // A JPEG passed through.
        let third = &pages[2];
        assert_eq!(third.media_box, [0.0, 0.0, 16.0, 8.0]);
        assert_eq!(name(&third.image, b"Filter"), "DCTDecode");
        assert_eq!(name(&third.image, b"ColorSpace"), "DeviceRGB");
        assert!(third.image.get(b"ColorTransform").is_err());
        assert!(third.image.get(b"DecodeParms").is_err());
        assert!(third.data.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn an_rgb_jpeg_passes_through_with_its_transform_in_the_filter_parameters() {
        // The encoder's JPEG with its JFIF APP0 replaced by an Adobe APP14
        // saying transform 0 (RGB).
        let mut bytes = jpeg_bytes(&DynamicImage::ImageRgb8(gradient(16, 8)));
        assert_eq!(&bytes[2..4], [0xFF, 0xE0]);
        let app0_end = 4 + usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
        let adobe = [
            0xFF, 0xEE, 0, 14, b'A', b'd', b'o', b'b', b'e', 0, 100, 0, 0, 0, 0, 0,
        ];
        bytes.splice(2..app0_end, adobe);
        let pass = jpeg::passthrough(&bytes).unwrap();
        assert!(pass.rgb);
        let pdf = pdf_of(vec![passed(bytes)]);
        let (_, pages) = pages(&pdf);
        let image = &pages[0].image;
        assert_eq!(name(image, b"Filter"), "DCTDecode");
        assert!(image.get(b"ColorTransform").is_err());
        let parms = image.get(b"DecodeParms").unwrap().as_dict().unwrap();
        assert_eq!(int(parms, b"ColorTransform"), 0);
        // The Adobe marker is kept too.
        assert!(pages[0].data.windows(5).any(|w| w == b"Adobe"));
    }

    #[test]
    fn a_profile_that_describes_the_pixels_becomes_iccbased() {
        let mut profile: Vec<u8> = (0..200u8).collect();
        profile[16..20].copy_from_slice(b"RGB ");
        let bytes = pdf_of(vec![Picture::Pixels {
            img: DynamicImage::ImageRgb8(gradient(2, 2)),
            icc: Some(profile.clone()),
            storage: Storage::Lossless,
            page: None,
        }]);
        let (doc, pages) = pages(&bytes);
        let space = pages[0]
            .image
            .get(b"ColorSpace")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(space[0].as_name().unwrap(), b"ICCBased");
        let stream = doc
            .get_object(space[1].as_reference().unwrap())
            .unwrap()
            .as_stream()
            .unwrap();
        assert_eq!(int(&stream.dict, b"N"), 3);
        assert_eq!(name(&stream.dict, b"Alternate"), "DeviceRGB");
        let mut inflated = Vec::new();
        ZlibDecoder::new(&stream.content[..])
            .read_to_end(&mut inflated)
            .unwrap();
        assert_eq!(inflated, profile);
    }

    #[test]
    fn lossy_pictures_are_jpeg_with_their_alpha_as_a_mask() {
        let rgba = RgbaImage::from_fn(64, 48, |x, y| {
            Rgba([x as u8 * 3, y as u8 * 4, 90, if x < 32 { 255 } else { 128 }])
        });
        let opaque = RgbaImage::from_pixel(64, 48, Rgba([10, 20, 30, 255]));
        let bytes = pdf_of(vec![
            lossy(DynamicImage::ImageRgba8(rgba.clone())),
            lossy(DynamicImage::ImageRgba8(opaque)),
        ]);
        let (_, pages) = pages(&bytes);
        assert_eq!(name(&pages[0].image, b"Filter"), "DCTDecode");
        let decoded = ::image::load_from_memory(&pages[0].data).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (64, 48));
        let (mask, mask_data) = pages[0].mask.as_ref().unwrap();
        let alpha: Vec<u8> = rgba.pixels().map(|p| p.0[3]).collect();
        assert_eq!(rows_of(mask, mask_data), alpha);
        // Opaque: no mask.
        assert!(pages[1].mask.is_none());
    }

    #[test]
    fn a_jpeg_larger_than_its_lossless_bound_is_stored_losslessly() {
        // A tiny picture: JPEG's tables alone are larger than storing it
        // losslessly.
        let mut seed = 1u64;
        let noise = RgbImage::from_fn(8, 8, |_, _| {
            let mut next = || {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            };
            Rgb([next(), next(), next()])
        });
        let img = DynamicImage::ImageRgb8(noise.clone());
        let jpeg = jpeg_bytes(&img);
        assert!(
            jpeg.len() as u64 > flate::bound(filtered(8, 8, 3, 8)),
            "{}",
            jpeg.len()
        );
        let bytes = pdf_of(vec![lossy(img)]);
        let (_, pages) = pages(&bytes);
        assert_eq!(name(&pages[0].image, b"Filter"), "FlateDecode");
        assert_eq!(rows_of(&pages[0].image, &pages[0].data), noise.into_raw());
    }

    #[test]
    fn sixteen_bit_and_float_rows_are_reduced_to_eight_bits() {
        let rgb = gradient(70, 3);
        let wide = DynamicImage::ImageRgb8(rgb.clone()).to_rgba16();
        let bytes = pdf_of(vec![
            lossless(DynamicImage::ImageRgba16(wide.clone())),
            lossless(DynamicImage::ImageRgb32F(
                DynamicImage::ImageRgb8(rgb.clone()).to_rgb32f(),
            )),
            lossless(DynamicImage::ImageLuma16(
                DynamicImage::ImageRgb8(rgb.clone()).to_luma16(),
            )),
        ]);
        let (_, pages) = pages(&bytes);
        // Opaque 16-bit alpha: no mask.
        assert!(pages[0].mask.is_none());
        assert_eq!(
            rows_of(&pages[0].image, &pages[0].data),
            rgb.as_raw().clone()
        );
        assert_eq!(
            rows_of(&pages[1].image, &pages[1].data),
            rgb.as_raw().clone()
        );
        assert_eq!(name(&pages[2].image, b"ColorSpace"), "DeviceGray");
        let luma = DynamicImage::ImageLuma16(DynamicImage::ImageRgb8(rgb).to_luma16()).to_luma8();
        assert_eq!(rows_of(&pages[2].image, &pages[2].data), luma.into_raw());
    }

    #[test]
    fn pages_are_one_point_a_pixel_up_to_200_inches() {
        assert_eq!(page_size(612, 792), (612.0, 792.0));
        assert_eq!(page_size(14_400, 14_400), (14_400.0, 14_400.0));
        assert_eq!(page_size(28_800, 100), (14_400.0, 50.0));
        assert_eq!(page_size(100, 28_800), (50.0, 14_400.0));
        // Never resampled: the image keeps its pixels; the page shrinks.
        let (w, h) = page_size(20_000, 7);
        assert_eq!(w, 14_400.0);
        assert!((h - 5.04).abs() < 1e-9, "{h}");
        // A side that would fall under 1 pt is 1 pt.
        assert_eq!(page_size(14_401, 1), (14_400.0, 1.0));
        assert_eq!(page_size(1_000_000, 3), (14_400.0, 1.0));
        assert_eq!(page_size(1, 1), (1.0, 1.0));
    }

    #[test]
    fn a_long_side_is_split_into_the_fewest_near_equal_spans() {
        assert_eq!(spans(1), vec![0..1]);
        assert_eq!(spans(MAX_IMAGE_SIDE), vec![0..MAX_IMAGE_SIDE]);
        assert_eq!(spans(MAX_IMAGE_SIDE + 1), [0..32_768, 32_768..65_536]);
        let parts = spans(140_000);
        assert_eq!(parts, [0..46_666, 46_666..93_333, 93_333..140_000]);
        assert_eq!(tiles(5, 3), [Tile { x: 0..5, y: 0..3 }]);
        assert_eq!(
            tiles(70_000, 2),
            [
                Tile {
                    x: 0..35_000,
                    y: 0..2
                },
                Tile {
                    x: 35_000..70_000,
                    y: 0..2
                }
            ]
        );
    }

    /// Every image object a page draws, by its resource name (`Im0`, …),
    /// with its soft mask, and the page's content.
    #[allow(clippy::type_complexity)]
    fn images_of(
        bytes: &[u8],
    ) -> (
        Vec<(Dictionary, Vec<u8>, Option<(Dictionary, Vec<u8>)>)>,
        String,
    ) {
        let doc = Document::load_mem(bytes).unwrap();
        let pages = doc.get_pages();
        assert_eq!(pages.len(), 1);
        let page = doc.get_object(pages[&1]).unwrap().as_dict().unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let mut images = Vec::new();
        for k in 0..xobjects.len() {
            let id = xobjects
                .get(format!("Im{k}").as_bytes())
                .unwrap()
                .as_reference()
                .unwrap();
            let image = doc.get_object(id).unwrap().as_stream().unwrap();
            let mask = image.dict.get(b"SMask").ok().map(|mask| {
                let mask = doc
                    .get_object(mask.as_reference().unwrap())
                    .unwrap()
                    .as_stream()
                    .unwrap();
                (mask.dict.clone(), mask.content.clone())
            });
            images.push((image.dict.clone(), image.content.clone(), mask));
        }
        let content_id = page.get(b"Contents").unwrap().as_reference().unwrap();
        let content = doc.get_object(content_id).unwrap().as_stream().unwrap();
        (images, String::from_utf8(content.content.clone()).unwrap())
    }

    #[test]
    fn a_picture_longer_than_an_image_may_be_is_written_as_strips() {
        // 3 × 70,000 gray with alpha: two strips of 35,000 rows, each with
        // its soft mask, top strip first, drawn edge to edge on a page
        // scaled to 14,400 pt (the width held at 1 pt).
        let tall = GrayAlphaImage::from_fn(3, 70_000, |x, y| {
            LumaA([(y % 251) as u8, if x == 1 { 9 } else { 255 }])
        });
        let bytes = pdf_of(vec![lossless(DynamicImage::ImageLumaA8(tall.clone()))]);
        let (images, content) = images_of(&bytes);
        assert_eq!(
            content,
            "q 1 0 0 7200 0 7200 cm /Im0 Do Q\nq 1 0 0 7200 0 0 cm /Im1 Do Q"
        );
        assert_eq!(images.len(), 2);
        let mut gray = Vec::new();
        let mut alpha = Vec::new();
        for (dict, data, mask) in &images {
            assert_eq!((int(dict, b"Width"), int(dict, b"Height")), (3, 35_000));
            assert_eq!(name(dict, b"ColorSpace"), "DeviceGray");
            let parms = dict.get(b"DecodeParms").unwrap().as_dict().unwrap();
            assert_eq!(int(parms, b"Columns"), 3);
            gray.extend(rows_of(dict, data));
            let (mask, mask_data) = mask.as_ref().unwrap();
            assert_eq!((int(mask, b"Width"), int(mask, b"Height")), (3, 35_000));
            alpha.extend(rows_of(mask, mask_data));
        }
        assert_eq!(gray, tall.pixels().map(|p| p.0[0]).collect::<Vec<_>>());
        assert_eq!(alpha, tall.pixels().map(|p| p.0[1]).collect::<Vec<_>>());

        // 70,000 × 2 RGB, lossy: no JPEG takes it, so two lossless strips of
        // 35,000 columns, left one first.
        let wide = RgbImage::from_fn(70_000, 2, |x, y| {
            Rgb([(x % 256) as u8, (x / 256 % 256) as u8, y as u8])
        });
        let bytes = pdf_of(vec![lossy(DynamicImage::ImageRgb8(wide.clone()))]);
        let (images, content) = images_of(&bytes);
        assert_eq!(
            content,
            "q 7200 0 0 1 0 0 cm /Im0 Do Q\nq 7200 0 0 1 7200 0 cm /Im1 Do Q"
        );
        assert_eq!(images.len(), 2);
        for (k, (dict, data, mask)) in images.iter().enumerate() {
            assert_eq!((int(dict, b"Width"), int(dict, b"Height")), (35_000, 2));
            assert_eq!(name(dict, b"Filter"), "FlateDecode");
            assert!(mask.is_none());
            let left = u32::try_from(k).unwrap() * 35_000;
            let part = DynamicImage::ImageRgb8(wide.clone()).crop_imm(left, 0, 35_000, 2);
            assert_eq!(rows_of(dict, data), part.to_rgb8().into_raw(), "strip {k}");
        }
    }

    #[test]
    fn the_eight_orientations_land_upright() {
        use Orientation::*;
        // Every pixel of a 3×2 picture is different.
        let stored = RgbImage::from_fn(3, 2, |x, y| Rgb([x as u8, y as u8, 0]));
        for orientation in [
            NoTransforms,
            FlipHorizontal,
            Rotate180,
            FlipVertical,
            Rotate90FlipH,
            Rotate90,
            Rotate270FlipH,
            Rotate270,
        ] {
            let mut upright = DynamicImage::ImageRgb8(stored.clone());
            upright.apply_orientation(orientation);
            let upright = upright.to_rgb8();
            let page = (f64::from(upright.width()), f64::from(upright.height()));
            let [a, b, c, d, e, f] = placement(orientation, page);
            let map = |u: f64, v: f64| (a * u + c * v + e, b * u + d * v + f);
            // The unit square covers the page.
            let mut corners: Vec<(i64, i64)> = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)]
                .into_iter()
                .map(|(u, v)| {
                    let (x, y) = map(u, v);
                    (x.round() as i64, y.round() as i64)
                })
                .collect();
            corners.sort();
            let (w, h) = (page.0 as i64, page.1 as i64);
            assert_eq!(corners, [(0, 0), (0, h), (w, 0), (w, h)], "{orientation:?}");
            // Each stored pixel's center lands on the upright pixel that
            // holds it. The image's first row is the unit square's top.
            for (x, y, pixel) in stored.enumerate_pixels() {
                let u = (f64::from(x) + 0.5) / 3.0;
                let v = 1.0 - (f64::from(y) + 0.5) / 2.0;
                let (px, py) = map(u, v);
                let column = px.floor() as u32;
                let row = (page.1 - py).floor() as u32;
                assert_eq!(
                    upright.get_pixel(column, row),
                    pixel,
                    "{orientation:?} at {x},{y}"
                );
            }
            // A page scaled down keeps the same mapping, scaled.
            let half = placement(orientation, (page.0 / 2.0, page.1 / 2.0));
            for (full, half) in placement(orientation, page).iter().zip(half) {
                assert_eq!(full / 2.0, half);
            }
        }
    }

    #[test]
    fn a_jpeg_passed_through_is_turned_by_its_matrix() {
        let photo = DynamicImage::ImageRgb8(gradient(16, 8));
        let mut bytes = jpeg_bytes(&photo);
        // EXIF orientation 6 after SOI.
        let mut exif =
            b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
        let len = u16::try_from(exif.len() + 2).unwrap().to_be_bytes();
        let mut app1 = vec![0xFF, 0xE1, len[0], len[1]];
        app1.append(&mut exif);
        bytes.splice(2..2, app1);
        let pdf = pdf_of(vec![passed(bytes)]);
        let (_, pages) = pages(&pdf);
        // Stored 16×8, shown 8×16.
        assert_eq!(pages[0].media_box, [0.0, 0.0, 8.0, 16.0]);
        assert_eq!(
            (
                int(&pages[0].image, b"Width"),
                int(&pages[0].image, b"Height")
            ),
            (16, 8)
        );
        assert_eq!(pages[0].content, "q 0 -16 8 0 0 16 cm /Im0 Do Q");
        assert!(!pdf.windows(4).any(|w| w == b"Exif"));
    }

    #[test]
    fn a_picture_over_200_inches_is_drawn_onto_its_scaled_page() {
        // 14,401 × 1 lossless: the page is 14,400 × 1 pt, and so is the
        // image's matrix, not its pixel size.
        let line = DynamicImage::ImageRgb8(gradient(14_401, 1));
        let pdf = pdf_of(vec![lossless(line)]);
        let (_, page) = pages(&pdf);
        assert_eq!(page[0].media_box, [0.0, 0.0, 14_400.0, 1.0]);
        assert_eq!(page[0].content, "q 14400 0 0 1 0 0 cm /Im0 Do Q");
        assert_eq!(int(&page[0].image, b"Width"), 14_401);

        // A JPEG stored 14,401 × 8 with EXIF orientation 6, passed through:
        // upright 8 × 14,401, on a page of 7.9994 × 14,400 pt.
        let mut bytes = jpeg_bytes(&DynamicImage::ImageRgb8(gradient(14_401, 8)));
        let mut exif =
            b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
        let len = u16::try_from(exif.len() + 2).unwrap().to_be_bytes();
        let mut app1 = vec![0xFF, 0xE1, len[0], len[1]];
        app1.append(&mut exif);
        bytes.splice(2..2, app1);
        let pdf = pdf_of(vec![passed(bytes)]);
        let (_, page) = pages(&pdf);
        assert_eq!(page[0].media_box, [0.0, 0.0, 7.9994, 14_400.0]);
        assert_eq!(page[0].content, "q 0 -14400 7.9994 0 0 14400 cm /Im0 Do Q");
        assert_eq!(
            (
                int(&page[0].image, b"Width"),
                int(&page[0].image, b"Height")
            ),
            (14_401, 8)
        );
    }

    #[test]
    fn page_memory_counts_the_pixels_one_stream_and_the_profile() {
        let side = 256u32;
        let pixels = u64::from(side * side);
        let rgb = DynamicImage::new_rgb8(side, side);
        let stream = flate::bound(filtered(side, side, 3, 8));
        assert_eq!(page_memory(&rgb, Storage::Lossless, 0), 3 * pixels + stream);
        // RGBA: the color stream is the larger buffer; the profile counts
        // twice (and its compressed copy).
        let rgba = DynamicImage::new_rgba8(side, side);
        assert_eq!(
            page_memory(&rgba, Storage::Lossless, 10),
            4 * pixels + stream + 20
        );
        // A lossy 16-bit picture is converted once.
        let wide = DynamicImage::new_rgb16(side, side);
        assert_eq!(
            page_memory(&wide, Storage::Lossy, 0),
            6 * pixels + 3 * pixels + stream
        );
        assert_eq!(
            page_memory(&wide, Storage::Lossless, 0),
            6 * pixels + stream
        );
        // Gray with alpha: two one-channel streams, one at a time.
        let gray = DynamicImage::new_luma_a8(side, side);
        let one = flate::bound(filtered(side, side, 1, 8));
        assert_eq!(page_memory(&gray, Storage::Lossless, 0), 2 * pixels + one);
    }

    #[test]
    fn a_cancelled_page_stops() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut pdf = Writer::begin(Vec::new()).unwrap();
        let picture = lossless(DynamicImage::ImageRgba8(RgbaImage::new(4, 200)));
        let err = write_page(&mut pdf, picture, &names(), &names(), &cancel).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);
    }

    #[test]
    fn webp_content_tells_lossy_from_lossless() {
        let chunk = |tag: &[u8], data: &[u8]| {
            let mut bytes = tag.to_vec();
            bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
            bytes.extend_from_slice(data);
            if data.len() % 2 == 1 {
                bytes.push(0);
            }
            bytes
        };
        let riff = |chunks: &[Vec<u8>]| {
            let body: Vec<u8> = chunks.concat();
            let mut bytes = b"RIFF".to_vec();
            bytes.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
            bytes.extend_from_slice(b"WEBP");
            bytes.extend(body);
            bytes
        };
        let kind = |bytes: Vec<u8>| webp_is_lossy(&mut io::Cursor::new(bytes));
        assert!(kind(riff(&[chunk(b"VP8 ", b"lossy")])));
        assert!(!kind(riff(&[chunk(b"VP8L", b"lossless")])));
        let vp8x = chunk(b"VP8X", &[0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(kind(riff(&[
            vp8x.clone(),
            chunk(b"ICCP", b"odd"),
            chunk(b"ALPH", b"a"),
            chunk(b"VP8 ", b"x")
        ])));
        assert!(!kind(riff(&[vp8x.clone(), chunk(b"VP8L", b"x")])));
        // An animation: its first frame decides.
        let frame = |image: Vec<u8>| {
            let mut data = vec![0u8; 16];
            data.extend(image);
            chunk(b"ANMF", &data)
        };
        let anim = chunk(b"ANIM", &[0; 6]);
        assert!(kind(riff(&[
            vp8x.clone(),
            anim.clone(),
            frame(chunk(b"VP8 ", b"x")),
            frame(chunk(b"VP8L", b"y"))
        ])));
        assert!(!kind(riff(&[
            vp8x.clone(),
            anim,
            frame(chunk(b"VP8L", b"y")),
            frame(chunk(b"VP8 ", b"x"))
        ])));
        // Can't tell: lossless.
        assert!(!kind(riff(&[vp8x])));
        assert!(!kind(b"RIFF\0\0\0\0WEBP".to_vec()));
    }

    struct TestDir(PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_dir() -> TestDir {
        let path = std::env::temp_dir().join(format!("grape-create-unit-{}", unique_suffix()));
        fs::create_dir_all(path.join("work")).unwrap();
        TestDir(path)
    }

    /// Writes `files` into `dir` and plans `op` for them.
    fn planned(dir: &TestDir, files: &[(&str, Vec<u8>)], op: Operation) -> (Vec<PathBuf>, Job) {
        let mut paths = Vec::new();
        for (name, bytes) in files {
            let path = dir.0.join(name);
            fs::write(&path, bytes).unwrap();
            paths.push(path);
        }
        let settings = Settings::new(dir.0.join("fallback"));
        let selection = crate::build_selection(&paths, &settings);
        let mut jobs = plan(&selection, &op, &settings).unwrap().jobs;
        assert_eq!(jobs.len(), 1);
        (paths, jobs.remove(0))
    }

    /// Each source's bytes and modification time.
    type Snapshot = Vec<(Vec<u8>, std::time::SystemTime)>;

    fn snapshot(paths: &[PathBuf]) -> Snapshot {
        paths
            .iter()
            .map(|path| {
                let modified = fs::metadata(path).unwrap().modified().unwrap();
                (fs::read(path).unwrap(), modified)
            })
            .collect()
    }

    /// Panics if a source's bytes or modification time changed.
    fn assert_untouched(paths: &[PathBuf], before: &Snapshot) {
        assert_eq!(&snapshot(paths), before, "a source changed");
    }

    fn dest(job: &Job) -> PathBuf {
        match &job.output {
            JobOutput::File(path) => path.clone(),
            other => panic!("{other:?}"),
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

    /// Runs `job` with the fake image helper in `mode`.
    fn run_with_helper(
        dir: &TestDir,
        job: &Job,
        mode: Option<&str>,
    ) -> (Result<JobOutcome, GrapeError>, Vec<f32>) {
        let limits = heif::Limits {
            work_parent: dir.0.join("work"),
            ..heif::Limits::default()
        };
        let launch = match mode {
            Some(mode) => fake(mode),
            None => nowhere(),
        };
        let helpers = Helpers {
            heif: Some(heif::Decoder::new(launch, limits)),
            svg: None,
        };
        let inputs: Vec<(&Path, Format)> = job
            .inputs
            .iter()
            .map(|path| (path.as_path(), crate::format_of(path).unwrap()))
            .collect();
        let mut seen = Vec::new();
        let result = write_pdf(
            job,
            &inputs,
            &dest(job),
            &EngineCtx::default(),
            &mut |p| seen.push(p),
            &helpers,
        );
        assert!(
            listing(&dir.0.join("work")).is_empty(),
            "a work folder was left"
        );
        (result, seen)
    }

    fn rgb_rows(width: u32, height: u32) -> Vec<u8> {
        gradient(width, height).into_raw()
    }

    fn png_bytes(img: &DynamicImage) -> Vec<u8> {
        let mut bytes = io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, ::image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn heic_pages_come_from_the_helper_as_jpeg() {
        let dir = test_dir();
        let mut rgba = Vec::new();
        for (i, pixel) in rgb_rows(60, 40).chunks(3).enumerate() {
            rgba.extend_from_slice(pixel);
            rgba.push(if i % 2 == 0 { 255 } else { 100 });
        }
        let photo = DynamicImage::ImageRgb8(gradient(3, 5));
        let (paths, job) = planned(
            &dir,
            &[
                ("a.heic", fake_heic(60, 40, 4, &rgba, None)),
                ("b.png", png_bytes(&photo)),
            ],
            Operation::tool(ToolKey::CreatePdf),
        );
        let before = snapshot(&paths);
        let (result, progress) = run_with_helper(&dir, &job, Some("ok"));
        let outcome = result.unwrap();
        assert_eq!(outcome.outputs, vec![dir.0.join("a Combined.pdf")]);
        assert!(progress.windows(2).all(|w| w[0] <= w[1]), "{progress:?}");
        assert_eq!(progress.last(), Some(&1.0));
        assert!(
            progress.contains(&0.25) && progress.contains(&0.5) && progress.contains(&0.75),
            "{progress:?}"
        );
        let (_, pages) = pages(&fs::read(&outcome.outputs[0]).unwrap());
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].media_box, [0.0, 0.0, 60.0, 40.0]);
        assert_eq!(name(&pages[0].image, b"Filter"), "DCTDecode");
        let alpha: Vec<u8> = rgba.chunks(4).map(|p| p[3]).collect();
        let (mask, mask_data) = pages[0].mask.as_ref().unwrap();
        assert_eq!(rows_of(mask, mask_data), alpha);
        assert_eq!(pages[1].media_box, [0.0, 0.0, 3.0, 5.0]);
        assert_eq!(name(&pages[1].image, b"Filter"), "FlateDecode");
        assert_untouched(&paths, &before);
        assert_eq!(
            listing(&dir.0),
            ["a Combined.pdf", "a.heic", "b.png", "work"]
        );
    }

    #[test]
    fn heic_to_pdf_runs_one_page() {
        let dir = test_dir();
        let rows = rgb_rows(50, 20);
        let (paths, job) = planned(
            &dir,
            &[("photo.heic", fake_heic(50, 20, 3, &rows, None))],
            Operation::convert(Format::Pdf),
        );
        let before = snapshot(&paths);
        let (result, progress) = run_with_helper(&dir, &job, Some("ok"));
        let outcome = result.unwrap();
        assert_eq!(outcome.outputs, vec![dir.0.join("photo.pdf")]);
        assert!(progress.contains(&0.5), "{progress:?}");
        let (_, pages) = pages(&fs::read(dir.0.join("photo.pdf")).unwrap());
        assert_eq!(pages[0].media_box, [0.0, 0.0, 50.0, 20.0]);
        assert!(pages[0].mask.is_none());
        assert_eq!(name(&pages[0].image, b"Filter"), "DCTDecode");
        let decoded = ::image::load_from_memory(&pages[0].data).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (50, 20));
        assert_untouched(&paths, &before);
    }

    #[test]
    fn a_heic_that_isnt_heif_is_read_as_its_content() {
        let dir = test_dir();
        let photo = DynamicImage::ImageRgb8(gradient(4, 3));
        let (paths, job) = planned(
            &dir,
            &[("photo.heic", png_bytes(&photo))],
            Operation::convert(Format::Pdf),
        );
        let before = snapshot(&paths);
        // `nowhere`: a helper that would fail if started.
        let (result, _) = run_with_helper(&dir, &job, None);
        result.unwrap();
        let (_, pages) = pages(&fs::read(dir.0.join("photo.pdf")).unwrap());
        assert_eq!(name(&pages[0].image, b"Filter"), "FlateDecode");
        assert_eq!(
            rows_of(&pages[0].image, &pages[0].data),
            photo.to_rgb8().into_raw()
        );
        assert_untouched(&paths, &before);
    }

    #[test]
    fn a_helper_failure_names_the_heic_and_writes_nothing() {
        let dir = test_dir();
        let photo = png_bytes(&DynamicImage::ImageRgb8(gradient(2, 2)));
        let (paths, job) = planned(
            &dir,
            &[
                ("a.png", photo),
                ("b.heic", fake_heic(1, 1, 3, &[1, 2, 3], None)),
            ],
            Operation::tool(ToolKey::CreatePdf),
        );
        let before = snapshot(&paths);
        let (result, _) = run_with_helper(&dir, &job, Some("error-damaged"));
        let err = result.unwrap_err();
        assert_eq!(
            (err.kind, err.user_message.as_str()),
            (
                ErrorKind::DecodeFailed,
                "b.heic isn't a readable HEIC image."
            )
        );
        assert_eq!(listing(&dir.0), ["a.png", "b.heic", "work"]);
        assert_untouched(&paths, &before);
    }

    #[test]
    fn heic_without_the_helper_says_so() {
        let dir = test_dir();
        let (paths, job) = planned(
            &dir,
            &[("photo.heic", fake_heic(1, 1, 3, &[1, 2, 3], None))],
            Operation::convert(Format::Pdf),
        );
        let before = snapshot(&paths);
        let err = convert(
            &job,
            &paths[0],
            &dest(&job),
            Format::Heic,
            &EngineCtx::default(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(
            (err.kind, err.user_message.as_str()),
            (
                ErrorKind::NotYetSupported,
                "Grape's image helper is missing."
            )
        );
        assert_eq!(listing(&dir.0), ["photo.heic", "work"]);
        assert_untouched(&paths, &before);
    }

    #[test]
    fn avif_to_pdf_is_a_lossy_page_with_its_profile() {
        // D41: an AVIF goes through the helper as HEIC does, its page
        // re-encoded as JPEG, its ICC profile kept.
        let dir = test_dir();
        let rows = rgb_rows(50, 20);
        let mut profile: Vec<u8> = (0..200u8).collect();
        profile[16..20].copy_from_slice(b"RGB ");
        let (paths, job) = planned(
            &dir,
            &[(
                "photo.avif",
                fake_image(HeifFormat::Avif, 50, 20, 3, &rows, Some(&profile)),
            )],
            Operation::convert(Format::Pdf),
        );
        let before = snapshot(&paths);
        let (result, progress) = run_with_helper(&dir, &job, Some("ok"));
        let outcome = result.unwrap();
        assert_eq!(outcome.outputs, vec![dir.0.join("photo.pdf")]);
        assert!(progress.windows(2).all(|w| w[0] <= w[1]), "{progress:?}");
        assert!(progress.contains(&0.5), "{progress:?}");
        assert_eq!(progress.last(), Some(&1.0));
        let (doc, pages) = pages(&fs::read(dir.0.join("photo.pdf")).unwrap());
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].media_box, [0.0, 0.0, 50.0, 20.0]);
        assert!(pages[0].mask.is_none());
        assert_eq!(name(&pages[0].image, b"Filter"), "DCTDecode");
        let decoded = ::image::load_from_memory(&pages[0].data).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (50, 20));
        let space = pages[0]
            .image
            .get(b"ColorSpace")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(space[0].as_name().unwrap(), b"ICCBased");
        let stream = doc
            .get_object(space[1].as_reference().unwrap())
            .unwrap()
            .as_stream()
            .unwrap();
        let mut inflated = Vec::new();
        ZlibDecoder::new(&stream.content[..])
            .read_to_end(&mut inflated)
            .unwrap();
        assert_eq!(inflated, profile);
        assert_untouched(&paths, &before);
        assert_eq!(listing(&dir.0), ["photo.avif", "photo.pdf", "work"]);
    }

    #[test]
    fn the_content_decides_between_heic_and_avif() {
        // An AVIF named .heic and a HEIC named .avif both go to the helper,
        // which decodes what's there.
        let rows = rgb_rows(50, 20);
        for (file, bytes) in [
            (
                "a.heic",
                fake_image(HeifFormat::Avif, 50, 20, 3, &rows, None),
            ),
            ("b.avif", fake_heic(50, 20, 3, &rows, None)),
        ] {
            let dir = test_dir();
            let (paths, job) = planned(&dir, &[(file, bytes)], Operation::convert(Format::Pdf));
            let before = snapshot(&paths);
            let (result, _) = run_with_helper(&dir, &job, Some("ok"));
            let outcome = result.unwrap();
            let (_, pages) = pages(&fs::read(&outcome.outputs[0]).unwrap());
            assert_eq!(pages[0].media_box, [0.0, 0.0, 50.0, 20.0], "{file}");
            assert_eq!(name(&pages[0].image, b"Filter"), "DCTDecode", "{file}");
            assert_untouched(&paths, &before);
        }
    }

    #[test]
    fn an_avif_that_isnt_heif_is_read_as_its_content() {
        let dir = test_dir();
        let photo = DynamicImage::ImageRgb8(gradient(4, 3));
        let (paths, job) = planned(
            &dir,
            &[("photo.avif", png_bytes(&photo))],
            Operation::convert(Format::Pdf),
        );
        let before = snapshot(&paths);
        // `nowhere`: a helper that would fail if started.
        let (result, _) = run_with_helper(&dir, &job, None);
        result.unwrap();
        let (_, pages) = pages(&fs::read(dir.0.join("photo.pdf")).unwrap());
        assert_eq!(name(&pages[0].image, b"Filter"), "FlateDecode");
        assert_eq!(
            rows_of(&pages[0].image, &pages[0].data),
            photo.to_rgb8().into_raw()
        );
        assert_untouched(&paths, &before);
    }

    #[test]
    fn avif_failures_name_the_avif_and_write_nothing() {
        for (mode, kind, message) in [
            (
                Some("error-damaged"),
                ErrorKind::DecodeFailed,
                "photo.avif isn't a readable AVIF image.",
            ),
            (
                Some("error-unsupported"),
                ErrorKind::DecodeFailed,
                "photo.avif is a kind of AVIF image Grape can't read yet.",
            ),
        ] {
            let dir = test_dir();
            let (paths, job) = planned(
                &dir,
                &[(
                    "photo.avif",
                    fake_image(HeifFormat::Avif, 1, 1, 3, &[1, 2, 3], None),
                )],
                Operation::convert(Format::Pdf),
            );
            let before = snapshot(&paths);
            let (result, _) = run_with_helper(&dir, &job, mode);
            let err = result.unwrap_err();
            assert_eq!((err.kind, err.user_message.as_str()), (kind, message));
            assert_eq!(listing(&dir.0), ["photo.avif", "work"]);
            assert_untouched(&paths, &before);
        }
        // Without the helper.
        let dir = test_dir();
        let (paths, job) = planned(
            &dir,
            &[(
                "photo.avif",
                fake_image(HeifFormat::Avif, 1, 1, 3, &[1, 2, 3], None),
            )],
            Operation::convert(Format::Pdf),
        );
        let err = convert(
            &job,
            &paths[0],
            &dest(&job),
            Format::Avif,
            &EngineCtx::default(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(
            (err.kind, err.user_message.as_str()),
            (
                ErrorKind::NotYetSupported,
                "Grape's image helper is missing."
            )
        );
        assert_eq!(listing(&dir.0), ["photo.avif", "work"]);
    }

    #[test]
    fn create_pdf_never_takes_an_avif() {
        // FORMAT_MATRIX gives AVIF no tools in v1 (D41): AVIF → PDF is a
        // conversion only, with the helper.
        let with_heif = SidecarPaths {
            heif: Some(std::path::PathBuf::from("grape-heif")),
            svg_worker: None,
            ..SidecarPaths::default()
        };
        assert!(reads(Format::Avif, &with_heif));
        assert!(!reads(Format::Avif, &SidecarPaths::default()));
        assert!(!combines(Format::Avif, &with_heif));
        assert!(!combines(Format::Avif, &SidecarPaths::default()));
        // grape-core doesn't plan Create PDF with an AVIF either, so the job
        // is planned for two PNGs and its second input swapped.
        let dir = test_dir();
        let photo = png_bytes(&DynamicImage::ImageRgb8(gradient(2, 2)));
        let (mut paths, mut job) = planned(
            &dir,
            &[("a.png", photo.clone()), ("b.png", photo)],
            Operation::tool(ToolKey::CreatePdf),
        );
        fs::remove_file(&paths[1]).unwrap();
        paths[1] = dir.0.join("b.avif");
        fs::write(
            &paths[1],
            fake_image(HeifFormat::Avif, 2, 2, 3, &rgb_rows(2, 2), None),
        )
        .unwrap();
        job.inputs[1] = paths[1].clone();
        let before = snapshot(&paths);
        let ctx = EngineCtx {
            sidecars: with_heif,
            ..EngineCtx::default()
        };
        let err = combine(&job, &dest(&job), &ctx, &mut |_| {}).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported);
        assert_eq!(listing(&dir.0), ["a.png", "b.avif", "work"]);
        assert_untouched(&paths, &before);
    }
}
