//! Raster image conversions among JPG, PNG, WebP, TIFF and BMP, with the
//! `image` crate (`docs/design/m1-drag-wheel.md` §E.2).
//!
//! - **Decoding** opens the source read-only and sniffs the real format from
//!   its first bytes, so a PNG named `.jpg` still converts. EXIF orientation
//!   is applied to the pixels (the output carries no EXIF, so it can't be
//!   rotated twice), and the ICC profile is carried over when the target
//!   format can embed it and its color model matches the output pixels.
//! - **Memory limits,** checked before any pixel is decoded, so neither a
//!   huge file nor a tiny one that claims huge dimensions (a decompression
//!   bomb) can exhaust memory:
//!   - a JPEG or WebP source file must fit [`MAX_SOURCE`], checked before
//!     reading it (the JPEG decoder reads the whole file into memory);
//!   - the metadata the PNG decoder keeps is held to
//!     [`PNG_METADATA_LIMIT`] twice over: its text and EXIF chunks, counted
//!     from the chunk headers, and what its own limit holds (the inflated
//!     ICC profile);
//!   - an ICC profile over [`MAX_ICC`] is dropped as soon as it is read;
//!   - from the headers: the decoded image must fit the `image` crate's
//!     default 512 MiB allocation limit (`into_decoder` hands that limit to
//!     the codec but, unlike `ImageReader::decode`, doesn't hold the decoded
//!     buffer to it, so Grape reserves it itself), and the whole
//!     conversion's peak (what the decoder keeps, a rotated copy, the
//!     converted copy, the encoder's in-memory output and the profile
//!     carried over) must fit [`WORKING_SET_LIMIT`]. What the JPEG, WebP
//!     and PNG decoders keep depends on how the file is laid out, so their
//!     headers are read first ([`crate::headers`]).
//! - **First frame only:** an animated WebP or a multi-page TIFF converts
//!   its first frame or page.
//! - **Targets:** JPEG at quality 90 with any alpha flattened onto white;
//!   PNG keeps the color type (float becomes 16-bit); WebP is lossless
//!   (`image` has no lossy WebP encoder); BMP is 8-bit RGB or RGBA; TIFF
//!   keeps 8- and 16-bit gray/RGB/RGBA and maps the rest to RGB(A).
//! - **Writing** goes through [`crate::output`]: a temp file next to the
//!   final name, then a no-clobber finalize that re-plans the name with
//!   [`grape_core::output_name`] if it was taken meanwhile.
//! - **Cancellation** is checked between steps; the source reader and the
//!   temp file's writer fail once the job is cancelled, which stops a
//!   decoder or encoder at its next read or write; and flattening checks
//!   every [`CANCEL_CHECK_ROWS`] rows. The JPEG decoder reads the whole
//!   file before decoding, so a JPEG decode (like the `image` crate's own
//!   pixel conversions and rotations) runs to its end once started.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use ::image::codecs::bmp::BmpEncoder;
use ::image::codecs::jpeg::JpegEncoder;
use ::image::codecs::png::PngEncoder;
use ::image::codecs::tiff::TiffEncoder;
use ::image::codecs::webp::WebPEncoder;
use ::image::metadata::Orientation;
use ::image::{
    ColorType, DynamicImage, ImageDecoder, ImageEncoder, ImageError, ImageFormat, ImageReader,
    ImageResult, Limits, RgbImage,
};
use grape_core::{Format, Job};

use crate::cancel::Cancellable;
use crate::headers::{self, JpegLayout, WebpLayout};
use crate::output::{place_no_clobber, TempFile};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome};

/// The formats this engine reads and writes. Every ordered pair of two
/// different members is supported: 20 pairs.
pub(crate) const RASTER: [Format; 5] = [
    Format::Jpg,
    Format::Png,
    Format::Webp,
    Format::Tiff,
    Format::Bmp,
];

/// How many times a taken output name is re-planned before giving up.
const MAX_REPLANS: usize = 3;

/// JPEG output quality.
const JPEG_QUALITY: u8 = 90;

const MIB: u64 = 1024 * 1024;

/// The largest ICC profile carried over: what a JPEG can hold, 255 APP2
/// segments of 65,519 profile bytes each (the encoder refuses more). Real
/// profiles are far smaller; a larger one is dropped for every target, and
/// the conversion goes ahead without it.
const MAX_ICC: usize = 255 * 65_519;

/// The most memory one conversion may hold at its peak (see
/// [`working_set`]). The decoded image alone is also held to the `image`
/// crate's default 512 MiB limit.
pub(crate) const WORKING_SET_LIMIT: u64 = 1024 * MIB;

/// The largest JPEG or WebP file Grape decodes. The JPEG decoder reads the
/// whole file into memory, and the WebP decoder reads metadata chunks whole
/// with no limit of its own. (A JPEG this large whose image would still fit
/// the 512 MiB decode limit is mostly metadata or padding.) The PNG, TIFF
/// and BMP decoders read the file as they go and hold their own buffers to
/// limits, so their files can be any size: a layered TIFF or a long
/// multi-page scan converts its first image.
pub(crate) const MAX_SOURCE: u64 = 512 * MIB;

/// The limit for each of the two kinds of metadata the PNG decoder keeps
/// besides the pixels, so up to about twice this in all:
/// - the text and EXIF chunks ahead of the image data, as
///   [`headers::png_metadata`] counts them from their headers before
///   decoding (a text chunk costs up to twice its size, plus its entry, so
///   one chunk of up to about 40 MiB fits); more is refused as too large;
/// - what the decoder's own limit holds: the inflated ICC profile, the raw
///   text and its row buffers. A profile that would inflate past it is
///   dropped, and the image still converts.
const PNG_METADATA_LIMIT: u64 = 128 * MIB;

/// How many rows Grape's own pixel loops process between cancel checks
/// (at most a few milliseconds of work).
const CANCEL_CHECK_ROWS: usize = 64;

/// True for a conversion between two different [`RASTER`] formats.
pub(crate) fn supports(from: Format, to: Format) -> bool {
    from != to && RASTER.contains(&from) && RASTER.contains(&to)
}

/// Converts `src` (of format `from`, by its name) to `to` at `dest`, the
/// path the plan reserved.
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
    let dir = dest
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| {
            names.write_failed(format!("output path has no folder: {}", dest.display()))
        })?;

    let cancel = &ctx.cancel;
    // Whatever failed, a cancelled job reports that it was cancelled: the
    // failure may just be the cancelled reader or writer.
    let or_cancelled = |e: GrapeError| {
        if cancel.is_cancelled() {
            crate::cancelled(src)
        } else {
            e
        }
    };

    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let (img, icc) = decode(src, from, to, &names, cancel).map_err(or_cancelled)?;
    progress(0.5);
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }

    let img = match prepare(img, to, cancel) {
        Some(img) if !cancel.is_cancelled() => img,
        _ => return Err(crate::cancelled(src)),
    };
    let icc = icc.filter(|profile| icc_fits(profile, &img));

    // Grape's own fallback folder (Downloads\Grape) is created on first
    // use: by a job that falls back, or one whose output setting names that
    // same folder. Any other folder must still be there; one that vanished
    // since planning is a write error, not something to bring back.
    if job.used_fallback || ctx.fallback_dir.as_deref() == Some(dir) {
        fs::create_dir_all(dir)
            .map_err(|e| names.write_failed(format!("create {}: {e}", dir.display())))?;
    }
    // From here on, `temp` removes the temp file on every return path.
    let (temp, file) = TempFile::create(dir, to.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    encode(file, &img, icc, to, temp.path(), &names, cancel).map_err(or_cancelled)?;
    drop(img);

    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let outcome = finalize(&temp, job, src, dest, dir, &names)?;
    drop(temp);
    progress(1.0);
    Ok(outcome)
}

/// Why [`decode`]'s inner step failed.
enum DecodeFailure {
    /// The codec's own error.
    Image(ImageError),
    /// The header asks for more memory than Grape allows; the detail says
    /// how much.
    TooLarge(String),
}

impl From<ImageError> for DecodeFailure {
    fn from(e: ImageError) -> Self {
        DecodeFailure::Image(e)
    }
}

/// Opens `src` read-only and decodes its first frame, upright, with its ICC
/// profile if it has one. Refuses, before decoding any pixel, a file or an
/// image whose conversion to `to` wouldn't fit the memory limits.
fn decode(
    src: &Path,
    from: Format,
    to: Format,
    names: &Names,
    cancel: &CancelToken,
) -> Result<(DynamicImage, Option<Vec<u8>>), GrapeError> {
    let file =
        File::open(src).map_err(|e| names.read_failed(format!("open {}: {e}", src.display())))?;
    let len = file
        .metadata()
        .map_err(|e| names.read_failed(format!("read {}: {e}", src.display())))?
        .len();
    let mut reader = ImageReader::new(BufReader::new(Cancellable::new(file, cancel)));
    if let Some(hint) = image_format(from) {
        // Used only when the content matches no known signature.
        reader.set_format(hint);
    }
    // Sniffing reads only the first bytes.
    let mut reader = reader
        .with_guessed_format()
        .map_err(|e| names.read_failed(format!("read {}: {e}", src.display())))?;
    let format = reader.format();
    if matches!(format, Some(ImageFormat::Jpeg | ImageFormat::WebP)) && len > MAX_SOURCE {
        return Err(names.too_large(format!(
            "decode {}: the file is {} MiB, over the {} MiB limit for a JPEG or WebP source",
            src.display(),
            len / MIB,
            MAX_SOURCE / MIB
        )));
    }
    let keeps = match format {
        Some(ImageFormat::Jpeg) => DecoderKeeps::Jpeg {
            file: len,
            layout: headers::jpeg_layout(src),
        },
        Some(ImageFormat::WebP) => DecoderKeeps::WebP {
            layout: headers::webp_layout(src),
        },
        Some(ImageFormat::Png) => match headers::png_metadata(src) {
            Some(found) if found.counted <= PNG_METADATA_LIMIT => DecoderKeeps::Png {
                // An ICC profile is inflated under the decoder's own limit.
                metadata: found.counted.saturating_add(if found.icc {
                    PNG_METADATA_LIMIT
                } else {
                    0
                }),
            },
            found => {
                let why = match found {
                    Some(found) => format!("its metadata needs about {} MiB", found.counted / MIB),
                    None => "it has too many metadata chunks to count".to_owned(),
                };
                return Err(names.too_large(format!(
                    "decode {}: {why}, over the {} MiB limit for PNG metadata",
                    src.display(),
                    PNG_METADATA_LIMIT / MIB
                )));
            }
        },
        _ => DecoderKeeps::Little,
    };
    if format == Some(ImageFormat::Png) {
        // The PNG decoder holds its own allocations to the reader's limit;
        // the pixels are held to the default one below.
        let mut limits = Limits::default();
        limits.max_alloc = Some(PNG_METADATA_LIMIT);
        reader.limits(limits);
    }

    // A decoder panic on a malformed file must not take the worker down.
    let decoded = panic::catch_unwind(AssertUnwindSafe(
        move || -> Result<(DynamicImage, Option<Vec<u8>>), DecodeFailure> {
            let mut decoder = reader.into_decoder()?;
            // A malformed EXIF block is not worth failing the conversion for.
            let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
            let (width, height) = decoder.dimensions();
            let color = decoder.color_type();
            let icc = decoder
                .icc_profile()
                .ok()
                .flatten()
                .filter(|icc| icc_size_fits(icc.len()));
            let profile = icc.as_ref().map_or(0, |icc| icc.len() as u64);
            let peak = working_set(width, height, color, keeps, orientation, to, profile);
            if peak > WORKING_SET_LIMIT {
                return Err(DecodeFailure::TooLarge(format!(
                    "{width}x{height} {color:?} to {} needs about {} MiB, over the {} MiB limit",
                    to.key(),
                    peak / MIB,
                    WORKING_SET_LIMIT / MIB
                )));
            }
            // What `ImageReader::decode` does and `into_decoder` doesn't:
            // hold the decoded buffer to the default limit, and give the
            // decoder only what is left for its own buffers.
            let mut limits = Limits::default();
            limits.reserve(decoder.total_bytes())?;
            decoder.set_limits(limits)?;
            let mut img = DynamicImage::from_decoder(decoder)?;
            img.apply_orientation(orientation);
            Ok((img, icc))
        },
    ));
    match decoded {
        Ok(Ok(decoded)) => Ok(decoded),
        Ok(Err(DecodeFailure::TooLarge(detail))) => {
            Err(names.too_large(format!("decode {}: {detail}", src.display())))
        }
        Ok(Err(DecodeFailure::Image(ImageError::Limits(e)))) => {
            Err(names.too_large(format!("decode {}: {e}", src.display())))
        }
        // Decoders report a truncated or malformed stream as these `io`
        // errors: the file was read, it just isn't a valid image.
        Ok(Err(DecodeFailure::Image(ImageError::IoError(e))))
            if matches!(
                e.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ) =>
        {
            Err(names.decode_failed(format!("decode {}: {e}", src.display())))
        }
        Ok(Err(DecodeFailure::Image(ImageError::IoError(e)))) => {
            Err(names.read_failed(format!("read {}: {e}", src.display())))
        }
        Ok(Err(DecodeFailure::Image(e))) => {
            Err(names.decode_failed(format!("decode {}: {e}", src.display())))
        }
        Err(_) => {
            Err(names.decode_failed(format!("decode {}: the decoder panicked", src.display())))
        }
    }
}

/// What a decoder keeps besides the decoded image while it decodes (see
/// [`working_set`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecoderKeeps {
    /// Nothing worth counting: the TIFF and BMP decoders read the file as
    /// they go, and the TIFF decoder holds its buffers to what the 512 MiB
    /// decode limit leaves beside the image.
    Little,
    /// The PNG decoder: the `metadata` [`headers::png_metadata`] counted,
    /// plus the decoder's own limit when there is an ICC profile to
    /// inflate. The pixel data is read as it goes.
    Png { metadata: u64 },
    /// The JPEG decoder: the whole `file`, and its copies of the metadata
    /// segments it parses (ICC, EXIF, XMP), which can't exceed the file;
    /// plus 16-bit coefficients for every component of the whole image
    /// when the `layout` may need them.
    Jpeg { file: u64, layout: JpegLayout },
    /// The WebP decoder, by `layout`:
    /// - a simple file, or a still one without a lossy image with alpha: up
    ///   to an RGBA frame (lossless without alpha decodes to RGBA first;
    ///   lossy keeps 1.5 bytes per pixel of YUV planes);
    /// - a still lossy image with alpha: its YUV planes, the `ALPH` chunk
    ///   decoded as RGBA and the alpha plane, 6.5 bytes per pixel;
    /// - an animation: an RGBA frame and an RGBA canvas.
    WebP { layout: WebpLayout },
}

/// The most memory converting a `width`×`height` `color` image to `to`
/// holds at once, in bytes, when its decoder `keeps` that and it is turned
/// by `orientation`. The steps run one after another, so the peak is the
/// largest of:
/// - **decoding:** the decoded image plus what the decoder keeps;
/// - **EXIF rotation** by a quarter turn: a rotated copy;
/// - **pixel prep:** the decoded image, an RGBA copy to flatten from and the
///   converted image;
/// - **encoding:** the pixels written plus the encoder's in-memory buffers
///   (PNG compresses in memory and byte-swaps 16-bit samples into a copy;
///   WebP expands to RGBA and compresses in memory);
///
/// plus the `profile` bytes of ICC profile carried through all of them.
fn working_set(
    width: u32,
    height: u32,
    color: ColorType,
    keeps: DecoderKeeps,
    orientation: Orientation,
    to: Format,
    profile: u64,
) -> u64 {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    let size = |c: ColorType| pixels.saturating_mul(u64::from(c.bytes_per_pixel()));
    let decoded = size(color);
    let decoder_buffers = match keeps {
        DecoderKeeps::Little => 0,
        DecoderKeeps::Jpeg { file, layout } => {
            let coefficients = if layout.whole_image {
                pixels.saturating_mul(2 * u64::from(layout.components))
            } else {
                0
            };
            file.saturating_mul(2).saturating_add(coefficients)
        }
        DecoderKeeps::Png { metadata } => metadata,
        DecoderKeeps::WebP { layout } => match layout {
            WebpLayout::Simple | WebpLayout::Still { lossy_alpha: false } => size(ColorType::Rgba8),
            WebpLayout::Still { lossy_alpha: true } => pixels.saturating_mul(13) / 2,
            WebpLayout::Animated => size(ColorType::Rgba8).saturating_mul(2),
        },
    };
    let rotated = match orientation {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => decoded,
        _ => 0,
    };
    let target = target_color(color, to);
    let converted = if target == color { 0 } else { size(target) };
    let flatten_copy = if flattens(color, to) && color != ColorType::Rgba8 {
        size(ColorType::Rgba8)
    } else {
        0
    };
    let written = size(target);
    let sixteen_bit = target.bytes_per_pixel() == 2 * target.channel_count();
    let encoder_buffers = match to {
        Format::Png if sixteen_bit => written.saturating_mul(2),
        Format::Png => written,
        Format::Webp => size(ColorType::Rgba8).saturating_mul(2),
        _ => 0,
    };
    [
        decoded.saturating_add(decoder_buffers),
        decoded.saturating_add(rotated),
        decoded
            .saturating_add(flatten_copy)
            .saturating_add(converted),
        written.saturating_add(encoder_buffers),
    ]
    .into_iter()
    .max()
    .unwrap_or(u64::MAX)
    .saturating_add(profile)
}

/// The `image` crate's format for a Grape format, used as a decode hint.
fn image_format(f: Format) -> Option<ImageFormat> {
    match f {
        Format::Jpg => Some(ImageFormat::Jpeg),
        Format::Png => Some(ImageFormat::Png),
        Format::Webp => Some(ImageFormat::WebP),
        Format::Tiff => Some(ImageFormat::Tiff),
        Format::Bmp => Some(ImageFormat::Bmp),
        _ => None,
    }
}

/// The pixel layout `to` is written in, for decoded pixels of `color`:
/// JPEG takes 8-bit gray or RGB (alpha is flattened onto white); PNG keeps
/// the layout (float becomes 16-bit); WebP and BMP take 8-bit RGB(A); TIFF
/// keeps 8- and 16-bit gray/RGB/RGBA and maps the rest to RGB(A).
fn target_color(color: ColorType, to: Format) -> ColorType {
    use ColorType::{La16, Rgb16, Rgb32F, Rgb8, Rgba16, Rgba32F, Rgba8, L16, L8};
    let alpha = color.has_alpha();
    match to {
        Format::Jpg if alpha => Rgb8,
        Format::Jpg if matches!(color, L8 | L16) => L8,
        Format::Jpg => Rgb8,
        Format::Png => match color {
            Rgb32F => Rgb16,
            Rgba32F => Rgba16,
            _ => color,
        },
        Format::Webp | Format::Bmp if alpha => Rgba8,
        Format::Webp | Format::Bmp => Rgb8,
        Format::Tiff => match color {
            L8 | Rgb8 | Rgba8 | L16 | Rgb16 | Rgba16 => color,
            La16 => Rgba16,
            _ if alpha => Rgba8,
            _ => Rgb8,
        },
        _ => color,
    }
}

/// True when converting `color` pixels to `to` composites them onto white.
fn flattens(color: ColorType, to: Format) -> bool {
    to == Format::Jpg && color.has_alpha()
}

/// Converts the pixels to what the target writes ([`target_color`]).
/// `None` when the job was cancelled meanwhile.
fn prepare(img: DynamicImage, to: Format, cancel: &CancelToken) -> Option<DynamicImage> {
    let color = img.color();
    let target = target_color(color, to);
    if target == color {
        return Some(img);
    }
    if flattens(color, to) {
        return flatten_onto_white(&img, cancel).map(DynamicImage::ImageRgb8);
    }
    Some(match target {
        ColorType::L8 => DynamicImage::ImageLuma8(img.into_luma8()),
        ColorType::Rgba8 => DynamicImage::ImageRgba8(img.into_rgba8()),
        ColorType::Rgb16 => DynamicImage::ImageRgb16(img.into_rgb16()),
        ColorType::Rgba16 => DynamicImage::ImageRgba16(img.into_rgba16()),
        _ => DynamicImage::ImageRgb8(img.into_rgb8()),
    })
}

/// Composites `img` over white: JPEG has no alpha, and dropping it would
/// turn transparent areas into whatever color they happen to hold. Reads
/// 8-bit RGBA pixels in place; other layouts are converted to it first.
/// `None` when the job was cancelled meanwhile.
fn flatten_onto_white(img: &DynamicImage, cancel: &CancelToken) -> Option<RgbImage> {
    let converted;
    let rgba = match img.as_rgba8() {
        Some(rgba) => rgba,
        None => {
            converted = img.to_rgba8();
            &converted
        }
    };
    let mut flat = RgbImage::new(rgba.width(), rgba.height());
    for (y, (row, flat_row)) in rgba.rows().zip(flat.rows_mut()).enumerate() {
        if y % CANCEL_CHECK_ROWS == 0 && cancel.is_cancelled() {
            return None;
        }
        for (pixel, flat_pixel) in row.zip(flat_row) {
            let [r, g, b, a] = pixel.0;
            let a = u32::from(a);
            let over = |c: u8| -> u8 {
                let mixed = (u32::from(c) * a + 255 * (255 - a) + 127) / 255;
                u8::try_from(mixed).unwrap_or(u8::MAX)
            };
            *flat_pixel = ::image::Rgb([over(r), over(g), over(b)]);
        }
    }
    Some(flat)
}

/// True when a profile of `len` bytes is small enough to carry over
/// ([`MAX_ICC`]).
fn icc_size_fits(len: usize) -> bool {
    len <= MAX_ICC
}

/// True when an ICC profile describes the output's color model (`GRAY` for
/// gray pixels, `RGB ` otherwise). A CMYK or gray profile on RGB pixels
/// would make viewers show wrong colors, so such a profile is dropped and
/// the conversion goes ahead without it. (Its size was checked against
/// [`MAX_ICC`] when it was read.)
fn icc_fits(profile: &[u8], img: &DynamicImage) -> bool {
    use ColorType::{La16, La8, L16, L8};
    let expected: &[u8] = if matches!(img.color(), L8 | L16 | La8 | La16) {
        b"GRAY"
    } else {
        b"RGB "
    };
    profile.len() >= 128 && profile.get(16..20) == Some(expected)
}

/// Encodes `img` as `to` into the temp `file`, then flushes and syncs it.
/// Writing fails once `cancel` is set.
fn encode(
    file: File,
    img: &DynamicImage,
    icc: Option<Vec<u8>>,
    to: Format,
    temp: &Path,
    names: &Names,
    cancel: &CancelToken,
) -> Result<(), GrapeError> {
    let mut out = BufWriter::new(Cancellable::new(file, cancel));
    let written = match to {
        Format::Jpg => write(
            JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY),
            img,
            icc,
        ),
        Format::Png => write(PngEncoder::new(&mut out), img, icc),
        Format::Webp => write(WebPEncoder::new_lossless(&mut out), img, icc),
        Format::Tiff => write(TiffEncoder::new(&mut out), img, icc),
        Format::Bmp => write(BmpEncoder::new(&mut out), img, icc),
        other => {
            return Err(names.encode_failed(format!("no encoder for {}", other.key())));
        }
    };
    match written {
        Ok(()) => {}
        // Encoders report what they can't encode as these `io` errors;
        // writing the temp file didn't fail.
        Err(ImageError::IoError(e))
            if matches!(
                e.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
            ) =>
        {
            return Err(names.encode_failed(format!("encode {}: {e}", to.key())));
        }
        Err(ImageError::IoError(e)) => {
            return Err(names.write_failed(format!("write {}: {e}", temp.display())));
        }
        Err(e) => return Err(names.encode_failed(format!("encode {}: {e}", to.key()))),
    }
    let file = out
        .into_inner()
        .map_err(|e| names.write_failed(format!("write {}: {}", temp.display(), e.error())))?
        .into_inner();
    file.sync_all()
        .map_err(|e| names.write_failed(format!("sync {}: {e}", temp.display())))
}

/// Sets the ICC profile when the encoder can embed one, then encodes.
fn write(
    mut encoder: impl ImageEncoder,
    img: &DynamicImage,
    icc: Option<Vec<u8>>,
) -> ImageResult<()> {
    if let Some(icc) = icc {
        // BMP can't embed a profile; the conversion goes ahead without it.
        let _ = encoder.set_icc_profile(icc);
    }
    img.write_with_encoder(encoder)
}

/// Gives the temp file its final name without replacing anything. If the
/// planned name was taken meanwhile, re-plans it against a fresh listing of
/// the folder plus every name tried so far, up to [`MAX_REPLANS`] times.
fn finalize(
    temp: &TempFile,
    job: &Job,
    src: &Path,
    dest: &Path,
    dir: &Path,
    names: &Names,
) -> Result<JobOutcome, GrapeError> {
    let source_name = file_name(src);
    let mut target = dest.to_path_buf();
    let mut attempted: HashSet<String> = HashSet::from([file_name(dest)]);
    for replans in 0..=MAX_REPLANS {
        match place_no_clobber(temp.path(), &target) {
            Ok(()) => {
                return Ok(JobOutcome {
                    outputs: vec![target],
                    renamed: replans > 0,
                })
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if replans == MAX_REPLANS {
                    break;
                }
                let mut taken = dir_names(dir);
                taken.extend(attempted.iter().cloned());
                let name = grape_core::output_name(&source_name, &job.op, &taken);
                attempted.insert(name.clone());
                target = dir.join(name);
            }
            Err(e) => {
                return Err(names.write_failed(format!(
                    "finalize {} as {}: {e}",
                    temp.path().display(),
                    target.display()
                )))
            }
        }
    }
    Err(GrapeError::new(
        ErrorKind::NameTaken,
        format!("A file named {} keeps appearing; try again.", names.planned),
        format!(
            "every name tried in {} was taken: {}",
            dir.display(),
            sorted(&attempted).join(", ")
        ),
    ))
}

/// The names in `dir` right now; empty if it can't be listed.
fn dir_names(dir: &Path) -> HashSet<String> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn sorted(set: &HashSet<String>) -> Vec<String> {
    let mut list: Vec<String> = set.iter().cloned().collect();
    list.sort();
    list
}

/// A path's file name as text, or the whole path if it has none.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The names that go into user messages.
struct Names {
    /// The source's file name.
    source: String,
    /// The source format's display name.
    from: &'static str,
    /// The target format's display name.
    to: &'static str,
    /// The destination folder's name.
    folder: String,
    /// The planned output file name.
    planned: String,
}

impl Names {
    fn new(src: &Path, dest: &Path, from: Format, to: Format) -> Names {
        let folder = dest
            .parent()
            .map(|dir| match dir.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                None => dir.display().to_string(),
            })
            .unwrap_or_default();
        Names {
            source: file_name(src),
            from: from.display_name(),
            to: to.display_name(),
            folder,
            planned: file_name(dest),
        }
    }

    fn read_failed(&self, detail: String) -> GrapeError {
        let message = format!("Grape couldn't read {}.", self.source);
        GrapeError::new(ErrorKind::ReadFailed, message, detail)
    }

    fn decode_failed(&self, detail: String) -> GrapeError {
        let message = format!("{} isn't a readable {} image.", self.source, self.from);
        GrapeError::new(ErrorKind::DecodeFailed, message, detail)
    }

    /// Converting the image would need more memory than Grape allows.
    fn too_large(&self, detail: String) -> GrapeError {
        let message = format!("{} is too large to convert.", self.source);
        GrapeError::new(ErrorKind::DecodeFailed, message, detail)
    }

    fn encode_failed(&self, detail: String) -> GrapeError {
        let message = format!("Grape couldn't write a {} file.", self.to);
        GrapeError::new(ErrorKind::EncodeFailed, message, detail)
    }

    fn write_failed(&self, detail: String) -> GrapeError {
        let message = format!("Grape couldn't save into {}.", self.folder);
        GrapeError::new(ErrorKind::WriteFailed, message, detail)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn flattening_blends_onto_white() {
        let img = DynamicImage::ImageRgba8(::image::RgbaImage::from_fn(3, 1, |x, _| match x {
            0 => ::image::Rgba([0, 0, 0, 0]),
            1 => ::image::Rgba([0, 0, 0, 255]),
            _ => ::image::Rgba([0, 0, 0, 128]),
        }));
        let flat = flatten_onto_white(&img, &CancelToken::new()).unwrap();
        assert_eq!(flat.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(flat.get_pixel(1, 0).0, [0, 0, 0]);
        assert_eq!(flat.get_pixel(2, 0).0, [127, 127, 127]);
        // Gray with alpha is converted to RGBA first.
        let gray = DynamicImage::ImageLumaA8(::image::GrayAlphaImage::from_pixel(
            2,
            1,
            ::image::LumaA([0, 0]),
        ));
        let flat = flatten_onto_white(&gray, &CancelToken::new()).unwrap();
        assert_eq!(flat.get_pixel(1, 0).0, [255, 255, 255]);
    }

    #[test]
    fn a_cancelled_token_stops_reading_and_writing() {
        let dir = std::env::temp_dir().join(format!(
            "grape-image-unit-{}",
            crate::output::unique_suffix()
        ));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        DynamicImage::new_rgb8(8, 8).save(&src).unwrap();
        let out = dir.join("a.bmp");
        let names = Names::new(&src, &out, Format::Png, Format::Bmp);
        let cancel = CancelToken::new();
        let (img, _) = decode(&src, Format::Png, Format::Bmp, &names, &cancel).unwrap();

        cancel.cancel();

        assert!(decode(&src, Format::Png, Format::Bmp, &names, &cancel).is_err());
        let file = File::create(&out).unwrap();
        let written = encode(file, &img, None, Format::Bmp, &out, &names, &cancel);
        assert!(written.is_err());
        assert_eq!(fs::metadata(&out).unwrap().len(), 0, "nothing was written");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flattening_stops_when_cancelled() {
        let img = DynamicImage::new_rgba8(4, 300);
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(flatten_onto_white(&img, &cancel).is_none());
        assert!(prepare(img.clone(), Format::Jpg, &cancel).is_none());
        // Conversions without a pixel loop of Grape's own still finish.
        assert!(prepare(img, Format::Png, &cancel).is_some());
    }

    #[test]
    fn icc_profiles_must_match_the_color_model() {
        let mut rgb = vec![0u8; 128];
        rgb[16..20].copy_from_slice(b"RGB ");
        let mut gray = rgb.clone();
        gray[16..20].copy_from_slice(b"GRAY");
        let mut cmyk = rgb.clone();
        cmyk[16..20].copy_from_slice(b"CMYK");
        let color = DynamicImage::new_rgb8(1, 1);
        let luma = DynamicImage::new_luma8(1, 1);
        assert!(icc_fits(&rgb, &color));
        assert!(!icc_fits(&gray, &color));
        assert!(!icc_fits(&cmyk, &color));
        assert!(icc_fits(&gray, &luma));
        assert!(!icc_fits(&rgb, &luma));
        assert!(!icc_fits(&rgb[..64], &color));
    }

    #[test]
    fn the_working_set_counts_every_copy() {
        use ColorType::{La8, Rgb16, Rgb8, Rgba8, L8};
        let none = Orientation::NoTransforms;
        let little = DecoderKeeps::Little;
        // 1024×1024 is 1 MiB of pixels, so each result in MiB is the
        // number of bytes held per pixel.
        let mib = |color, keeps, orientation, to| {
            working_set(1024, 1024, color, keeps, orientation, to, 0) / MIB
        };
        // Gray to BMP: 1 decoded + 3 converted.
        assert_eq!(mib(L8, little, none, Format::Bmp), 4);
        // RGBA to JPEG is flattened in place: 4 + 3.
        assert_eq!(mib(Rgba8, little, none, Format::Jpg), 7);
        // Gray with alpha is copied to RGBA first: 2 + 4 + 3.
        assert_eq!(mib(La8, little, none, Format::Jpg), 9);
        // A quarter turn holds two copies.
        assert_eq!(mib(Rgb8, little, Orientation::Rotate90, Format::Tiff), 6);
        // 16-bit PNG: 6 written + 6 compressed + 6 byte-swapped.
        assert_eq!(mib(Rgb16, little, none, Format::Png), 18);
        // WebP: 3 written + 4 as RGBA + 4 compressed.
        assert_eq!(mib(Rgb8, little, none, Format::Webp), 11);

        // The JPEG decoder keeps the file (here 100 MiB) and its metadata
        // copies, and 2 bytes per component for the whole image only when
        // its layout may need them.
        let jpeg = |components, whole_image| DecoderKeeps::Jpeg {
            file: 100 * MIB,
            layout: JpegLayout {
                components,
                whole_image,
            },
        };
        assert_eq!(mib(Rgb8, jpeg(3, false), none, Format::Bmp), 203);
        assert_eq!(mib(Rgb8, jpeg(3, true), none, Format::Bmp), 209);
        // CMYK is decoded to RGB, but its coefficients have 4 components.
        assert_eq!(mib(Rgb8, jpeg(4, true), none, Format::Bmp), 211);
        // A 25,000×4,600 panorama in a 40 MiB file fits when it decodes row
        // by row, and doesn't with whole-image coefficients.
        let pano = |whole_image| {
            let keeps = DecoderKeeps::Jpeg {
                file: 40 * MIB,
                layout: JpegLayout {
                    components: 3,
                    whole_image,
                },
            };
            working_set(25_000, 4_600, Rgb8, keeps, none, Format::Bmp, 0)
        };
        assert!(pano(false) < WORKING_SET_LIMIT);
        assert!(pano(true) > WORKING_SET_LIMIT);

        // WebP: up to an RGBA frame; a still lossy image with alpha keeps
        // 6.5 bytes per pixel; an animation an RGBA frame and an RGBA canvas.
        let webp = |layout| DecoderKeeps::WebP { layout };
        let simple = webp(WebpLayout::Simple);
        assert_eq!(mib(Rgba8, simple, none, Format::Bmp), 8);
        let opaque = webp(WebpLayout::Still { lossy_alpha: false });
        assert_eq!(mib(Rgb8, opaque, none, Format::Bmp), 7);
        let alpha = webp(WebpLayout::Still { lossy_alpha: true });
        assert_eq!(
            working_set(1024, 1024, Rgba8, alpha, none, Format::Bmp, 0),
            21 * MIB / 2
        );
        assert_eq!(
            mib(Rgba8, webp(WebpLayout::Animated), none, Format::Bmp),
            12
        );
        assert_eq!(mib(Rgb8, webp(WebpLayout::Animated), none, Format::Bmp), 11);

        // PNG: the metadata its headers say the decoder keeps.
        let png = DecoderKeeps::Png {
            metadata: 100 * MIB,
        };
        assert_eq!(mib(Rgb8, png, none, Format::Tiff), 103);

        // The profile counts throughout: gray to BMP with 2 MiB of it.
        assert_eq!(
            working_set(1024, 1024, L8, little, none, Format::Bmp, 2 * MIB) / MIB,
            6
        );
        // Absurd dimensions saturate instead of overflowing.
        let huge = working_set(
            u32::MAX,
            u32::MAX,
            ColorType::Rgba32F,
            jpeg(4, true),
            none,
            Format::Webp,
            u64::MAX,
        );
        assert_eq!(huge, u64::MAX);
    }

    #[test]
    fn profiles_are_kept_up_to_what_a_jpeg_holds() {
        // 255 APP2 segments of 65,519 bytes.
        assert!(icc_size_fits(16_707_345));
        assert!(!icc_size_fits(16_707_346));
    }

    #[test]
    fn targets_get_the_pixel_layouts_they_can_write() {
        let rgba16 = DynamicImage::new_rgba16(1, 1);
        let la8 = DynamicImage::new_luma_a8(1, 1);
        let l16 = DynamicImage::new_luma16(1, 1);
        let rgb32f = DynamicImage::new_rgb32f(1, 1);
        let color = |img: &DynamicImage, to| {
            let prepared = prepare(img.clone(), to, &CancelToken::new()).unwrap();
            // The estimate in `working_set` relies on this.
            assert_eq!(prepared.color(), target_color(img.color(), to));
            prepared.color()
        };
        assert_eq!(color(&rgba16, Format::Jpg), ColorType::Rgb8);
        assert_eq!(color(&l16, Format::Jpg), ColorType::L8);
        assert_eq!(color(&rgba16, Format::Png), ColorType::Rgba16);
        assert_eq!(color(&rgb32f, Format::Png), ColorType::Rgb16);
        assert_eq!(color(&la8, Format::Webp), ColorType::Rgba8);
        assert_eq!(color(&l16, Format::Bmp), ColorType::Rgb8);
        assert_eq!(color(&rgba16, Format::Tiff), ColorType::Rgba16);
        assert_eq!(color(&la8, Format::Tiff), ColorType::Rgba8);
        assert_eq!(color(&l16, Format::Tiff), ColorType::L16);
        assert_eq!(color(&rgb32f, Format::Tiff), ColorType::Rgb8);
    }
}
