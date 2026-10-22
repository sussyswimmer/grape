//! Raster image conversions among JPG, PNG, WebP, TIFF and BMP, with the
//! `image` crate (`docs/design/m1-drag-wheel.md` §E.2).
//!
//! - **Decoding** opens the source read-only and sniffs the real format from
//!   its first bytes, so a PNG named `.jpg` still converts. EXIF orientation
//!   is applied to the pixels (the output carries no EXIF, so it can't be
//!   rotated twice), and the ICC profile is carried over when the target
//!   format can embed it and its color model matches the output pixels.
//!   The `image` crate's default 512 MiB allocation limit stays on, as a
//!   guard against decompression bombs.
//! - **First frame only:** an animated WebP or a multi-page TIFF converts
//!   its first frame or page.
//! - **Targets:** JPEG at quality 90 with any alpha flattened onto white;
//!   PNG keeps the color type (float becomes 16-bit); WebP is lossless
//!   (`image` has no lossy WebP encoder); BMP is 8-bit RGB or RGBA; TIFF
//!   keeps 8- and 16-bit gray/RGB/RGBA and maps the rest to RGB(A).
//! - **Writing** goes through [`crate::output`]: a temp file next to the
//!   final name, then a no-clobber finalize that re-plans the name with
//!   [`grape_core::output_name`] if it was taken meanwhile.
//! - **Cancellation** is checked before reading, after decoding and before
//!   finalizing. Decoding and encoding are single library calls and can't
//!   be interrupted in between.

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
    ImageResult, RgbImage,
};
use grape_core::{Format, Job};

use crate::output::{place_no_clobber, TempFile};
use crate::{EngineCtx, ErrorKind, GrapeError, JobOutcome};

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

    if ctx.cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let (img, icc) = decode(src, from, &names)?;
    progress(0.5);
    if ctx.cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }

    let img = prepare(img, to);
    let icc = icc.filter(|profile| icc_fits(profile, &img));

    if job.used_fallback {
        // The fallback is Grape's own folder (Downloads\Grape); it may not
        // exist yet.
        fs::create_dir_all(dir)
            .map_err(|e| names.write_failed(format!("create {}: {e}", dir.display())))?;
    }
    // From here on, `temp` removes the temp file on every return path.
    let (temp, file) = TempFile::create(dir, to.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    encode(file, &img, icc, to, temp.path(), &names)?;
    drop(img);

    if ctx.cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let outcome = finalize(&temp, job, src, dest, dir, &names)?;
    drop(temp);
    progress(1.0);
    Ok(outcome)
}

/// Opens `src` read-only and decodes its first frame, upright, with its ICC
/// profile if it has one.
fn decode(
    src: &Path,
    from: Format,
    names: &Names,
) -> Result<(DynamicImage, Option<Vec<u8>>), GrapeError> {
    let file =
        File::open(src).map_err(|e| names.read_failed(format!("open {}: {e}", src.display())))?;
    let mut reader = ImageReader::new(BufReader::new(file));
    if let Some(hint) = image_format(from) {
        // Used only when the content matches no known signature.
        reader.set_format(hint);
    }
    let reader = reader
        .with_guessed_format()
        .map_err(|e| names.read_failed(format!("read {}: {e}", src.display())))?;

    // A decoder panic on a malformed file must not take the worker down.
    let decoded = panic::catch_unwind(AssertUnwindSafe(move || -> ImageResult<_> {
        let mut decoder = reader.into_decoder()?;
        // A malformed EXIF block is not worth failing the conversion for.
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
        let icc = decoder.icc_profile().ok().flatten();
        let mut img = DynamicImage::from_decoder(decoder)?;
        img.apply_orientation(orientation);
        Ok((img, icc))
    }));
    match decoded {
        Ok(Ok(decoded)) => Ok(decoded),
        Ok(Err(ImageError::IoError(e))) => {
            Err(names.read_failed(format!("read {}: {e}", src.display())))
        }
        Ok(Err(e)) => Err(names.decode_failed(format!("decode {}: {e}", src.display()))),
        Err(_) => {
            Err(names.decode_failed(format!("decode {}: the decoder panicked", src.display())))
        }
    }
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

/// Converts the pixels to what the target writes.
fn prepare(img: DynamicImage, to: Format) -> DynamicImage {
    use ColorType::{La16, La8, Rgb16, Rgb32F, Rgb8, Rgba16, Rgba32F, Rgba8, L16, L8};
    let color = img.color();
    match to {
        Format::Jpg if img.has_alpha() => DynamicImage::ImageRgb8(flatten_onto_white(&img)),
        Format::Jpg if matches!(color, L8 | L16) => DynamicImage::ImageLuma8(img.into_luma8()),
        Format::Jpg => DynamicImage::ImageRgb8(img.into_rgb8()),
        Format::Png => match color {
            Rgb32F => DynamicImage::ImageRgb16(img.into_rgb16()),
            Rgba32F => DynamicImage::ImageRgba16(img.into_rgba16()),
            _ => img,
        },
        Format::Webp | Format::Bmp if img.has_alpha() => DynamicImage::ImageRgba8(img.into_rgba8()),
        Format::Webp | Format::Bmp => DynamicImage::ImageRgb8(img.into_rgb8()),
        Format::Tiff => match color {
            L8 | Rgb8 | Rgba8 | L16 | Rgb16 | Rgba16 => img,
            La16 => DynamicImage::ImageRgba16(img.into_rgba16()),
            La8 => DynamicImage::ImageRgba8(img.into_rgba8()),
            _ if img.has_alpha() => DynamicImage::ImageRgba8(img.into_rgba8()),
            _ => DynamicImage::ImageRgb8(img.into_rgb8()),
        },
        _ => img,
    }
}

/// Composites `img` over white: JPEG has no alpha, and dropping it would
/// turn transparent areas into whatever color they happen to hold.
fn flatten_onto_white(img: &DynamicImage) -> RgbImage {
    let rgba = img.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let a = u32::from(a);
        let over = |c: u8| -> u8 {
            let mixed = (u32::from(c) * a + 255 * (255 - a) + 127) / 255;
            u8::try_from(mixed).unwrap_or(u8::MAX)
        };
        ::image::Rgb([over(r), over(g), over(b)])
    })
}

/// True when an ICC profile describes the output's color model (`GRAY` for
/// gray pixels, `RGB ` otherwise). A CMYK or gray profile on RGB pixels
/// would make viewers show wrong colors, so such profiles are dropped.
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
fn encode(
    file: File,
    img: &DynamicImage,
    icc: Option<Vec<u8>>,
    to: Format,
    temp: &Path,
    names: &Names,
) -> Result<(), GrapeError> {
    let mut out = BufWriter::new(file);
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
        Err(ImageError::IoError(e)) => {
            return Err(names.write_failed(format!("write {}: {e}", temp.display())));
        }
        Err(e) => return Err(names.encode_failed(format!("encode {}: {e}", to.key()))),
    }
    let file = out
        .into_inner()
        .map_err(|e| names.write_failed(format!("write {}: {}", temp.display(), e.error())))?;
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
        let flat = flatten_onto_white(&img);
        assert_eq!(flat.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(flat.get_pixel(1, 0).0, [0, 0, 0]);
        assert_eq!(flat.get_pixel(2, 0).0, [127, 127, 127]);
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
    fn targets_get_the_pixel_layouts_they_can_write() {
        let rgba16 = DynamicImage::new_rgba16(1, 1);
        let la8 = DynamicImage::new_luma_a8(1, 1);
        let l16 = DynamicImage::new_luma16(1, 1);
        let rgb32f = DynamicImage::new_rgb32f(1, 1);
        assert_eq!(
            prepare(rgba16.clone(), Format::Jpg).color(),
            ColorType::Rgb8
        );
        assert_eq!(prepare(l16.clone(), Format::Jpg).color(), ColorType::L8);
        assert_eq!(
            prepare(rgba16.clone(), Format::Png).color(),
            ColorType::Rgba16
        );
        assert_eq!(
            prepare(rgb32f.clone(), Format::Png).color(),
            ColorType::Rgb16
        );
        assert_eq!(prepare(la8.clone(), Format::Webp).color(), ColorType::Rgba8);
        assert_eq!(prepare(l16.clone(), Format::Bmp).color(), ColorType::Rgb8);
        assert_eq!(prepare(rgba16, Format::Tiff).color(), ColorType::Rgba16);
        assert_eq!(prepare(la8, Format::Tiff).color(), ColorType::Rgba8);
        assert_eq!(prepare(l16, Format::Tiff).color(), ColorType::L16);
        assert_eq!(prepare(rgb32f, Format::Tiff).color(), ColorType::Rgb8);
    }
}
