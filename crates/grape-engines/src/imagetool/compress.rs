//! Compress on JPEG (D26; PNG is [`super::png_compress`]): decoded as stored (the EXIF orientation not
//! applied), scaled down at Small and Balanced, encoded again with
//! image's encoder (4:4:4, baseline), its metadata carried, and kept only
//! when it's at most 90 % of the source's size.
//!
//! | Preset | Long side | Quality |
//! |---|---|---|
//! | Small | ≤ 2048 | 70 |
//! | Balanced | ≤ 4096 | 80 |
//! | High | as it is | 85 |
//!
//! - A JPEG that wouldn't be scaled and whose own quality (estimated from
//!   its luminance table, [`jpeg::estimate_quality`]) is at most the
//!   preset's + 3 ends `NoChange` before it's decoded.
//! - "Try Small" is said only when Small would do more: scale the picture,
//!   or re-encode a 4:4:4 or gray one over quality 73. A subsampled (4:2:0)
//!   JPEG Small wouldn't scale isn't sent there: its 4:4:4 output is
//!   rarely smaller.
//! - Scaling ([`scale`]): a picture over twice the long side is first
//!   averaged down by a whole factor (Grape's own loop, which a cancel
//!   stops at the next row), then Lanczos3 takes it the rest of the way, so
//!   that step holds and takes no more than for a picture of twice the long
//!   side. What the steps hold together is checked against D16's limit
//!   before decoding ([`peak`]).
//! - Carried: the EXIF (PixelXDimension and PixelYDimension follow a
//!   scale), the JFIF pixel density, the ICC profile, XMP and extended
//!   XMP, IPTC (APP13) and comments. A JPEG with MPF or data after EOI (a
//!   gain map, a motion photo) loses those, and its XMP, which describes
//!   them.
//! - CMYK and YCCK JPEGs are refused: image's encoder writes gray or RGB.

use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;

use grape_core::{CompressPreset, Format, Job};
use image::codecs::jpeg::{JpegEncoder, PixelDensity, PixelDensityUnit};
use image::{ColorType, DynamicImage, GenericImageView, ImageEncoder};

use super::exif;
use super::jpeg::{self, Header};
use super::scale;
use super::strip::{self, Error};
use crate::cancel::Cancellable;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome, Note};

/// The formats Compress runs on through this engine, by name; the content
/// decides which compressor runs ([`super::run`]).
pub(crate) fn compresses(from: Format) -> bool {
    matches!(from, Format::Jpg | Format::Png)
}

/// A JPEG preset's quality and the long side it scales down to.
fn plan(preset: CompressPreset) -> (u8, Option<u32>) {
    match preset {
        CompressPreset::Small => (70, Some(2048)),
        CompressPreset::Balanced => (80, Some(4096)),
        CompressPreset::High => (85, None),
    }
}

/// Whether `preset` would leave a JPEG of `long` px and estimated quality
/// `estimate` as it is: no scale, and its quality at most the preset's + 3.
fn leaves(preset: CompressPreset, long: u32, estimate: Option<u8>) -> bool {
    let (quality, cap) = plan(preset);
    cap.is_none_or(|cap| long <= cap) && estimate.is_some_and(|e| e <= quality + 3)
}

/// Whether Small would do more than another preset to a JPEG of `long` px,
/// estimated quality `estimate`, `subsampled` or not ([module docs](self)).
fn small_does_more(long: u32, subsampled: bool, estimate: Option<u8>) -> bool {
    let scales = plan(CompressPreset::Small).1.is_some_and(|cap| long > cap);
    scales || (!subsampled && !leaves(CompressPreset::Small, long, estimate))
}

/// What Compress holds at its peak for a `width`×`height` decoded picture
/// of `bpp` bytes a pixel at `preset` ([`scale::peak`]): to encode, the
/// picture, an RGB copy unless it's gray or RGB, and up to 2 bytes a pixel
/// of JPEG (quality 85 at 4:4:4 on noise is about 1.6), or 1 for gray
/// (about 0.7).
fn peak(width: u32, height: u32, bpp: u64, preset: CompressPreset) -> u64 {
    let encode = |w: u64, h: u64, bpp: u64| {
        let copy = if bpp == 1 || bpp == 3 { 0 } else { 3 };
        let jpeg = if bpp == 1 { 1 } else { 2 };
        w.saturating_mul(h).saturating_mul(bpp + copy + jpeg)
    };
    scale::peak(width, height, bpp, plan(preset).1, &encode)
}

/// Runs Compress at `preset` on the JPEG `src` (named as `from`) into
/// `dest`.
pub(crate) fn run(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    preset: CompressPreset,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, from);
    let dir = publish::dest_dir(dest, &names)?;
    let cancel = &ctx.cancel;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let file =
        File::open(src).map_err(|e| names.read_failed(format!("open {}: {e}", src.display())))?;
    let before = file
        .metadata()
        .map_err(|e| names.read_failed(format!("stat {}: {e}", src.display())))?
        .len();
    // Refused unread, as a conversion would: the header check reads on to
    // the end of the file.
    if before > crate::image::MAX_SOURCE {
        return Err(names.too_large(format!(
            "compress {}: the file is {} MiB, over the {} MiB limit for a JPEG source",
            src.display(),
            before >> 20,
            crate::image::MAX_SOURCE >> 20
        )));
    }
    let mut reader = BufReader::new(Cancellable::new(file, cancel));
    let header = jpeg::header(&mut reader).map_err(|e| {
        let e = match e {
            Error::Broken(why) => names.decode_failed(format!("{}: {why}", src.display())),
            Error::TooLarge(why) => names.too_large(format!("{}: {why}", src.display())),
            Error::Refused(why) => strip::refused(&names, src, &why),
            Error::Read(e) | Error::Write(e) => {
                names.read_failed(format!("read {}: {e}", src.display()))
            }
        };
        publish::or_cancelled(cancel, src, e)
    })?;
    drop(reader);
    if header.components == 4 {
        return Err(GrapeError::new(
            ErrorKind::NotYetSupported,
            format!(
                "{} is a print (CMYK) JPEG, which Compress can't keep yet.",
                names.source
            ),
            format!("4 components in {}", src.display()),
        ));
    }
    let long = u32::from(header.width.max(header.height));
    let estimate = header.luma.as_ref().map(jpeg::estimate_quality);
    let try_small =
        preset != CompressPreset::Small && small_does_more(long, header.subsampled, estimate);
    if leaves(preset, long, estimate) {
        let detail = format!(
            "{long} px at about quality {estimate:?} in {}",
            src.display()
        );
        return Err(names.not_smaller(try_small, detail));
    }
    progress(0.1);

    let tool_peak = |width: u32, height: u32, color: ColorType| {
        peak(width, height, u64::from(color.bytes_per_pixel()), preset)
    };
    let (img, icc) =
        crate::image::decode_stored(src, from, Format::Jpg, &names, cancel, &tool_peak)
            .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    progress(0.4);
    let (quality, cap) = plan(preset);
    let img = scale::scale(img, cap, cancel).ok_or_else(|| crate::cancelled(src))?;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    progress(0.6);
    let encoded = encode(&img, quality, &header, icc, cancel)
        .map_err(|e| names.encode_failed(format!("encode {}: {e}", src.display())))
        .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    progress(0.9);
    let after = encoded.len();
    if after.saturating_mul(10) > before.saturating_mul(9) {
        let detail = format!("{after} bytes from {before} in {}", src.display());
        return Err(names.not_smaller(try_small, detail));
    }

    publish::ensure_dir(job, ctx, dir, &names)?;
    let (temp, mut out) = TempFile::create(dir, from.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    encoded
        .write_to(&mut out)
        .and_then(|()| out.sync_all())
        .map_err(|e| names.write_failed(format!("write {}: {e}", temp.path().display())))?;
    drop(out);
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let mut outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    progress(1.0);
    outcome.notes.push(Note::Compressed { before, after });
    Ok(outcome)
}

/// Encodes `img` at `quality` with the source's EXIF (dimensions patched),
/// pixel density and `icc` profile, and the segments `header` carries,
/// after the encoder's own APPn segments. A cancel stops it at its next
/// write.
fn encode<'h>(
    img: &DynamicImage,
    quality: u8,
    header: &'h Header,
    icc: Option<Vec<u8>>,
    cancel: &CancelToken,
) -> Result<Spliced<'h>, String> {
    let (width, height) = img.dimensions();
    let mut out = Vec::new();
    {
        let mut encoder =
            JpegEncoder::new_with_quality(Cancellable::new(&mut out, cancel), quality);
        if let Some(density) = header.density.and_then(pixel_density) {
            encoder.set_pixel_density(density);
        }
        if let Some(mut tiff) = header.exif.clone() {
            exif::patch_dimensions(&mut tiff, width, height);
            encoder.set_exif_metadata(tiff).map_err(|e| e.to_string())?;
        }
        if let Some(icc) = icc {
            encoder.set_icc_profile(icc).map_err(|e| e.to_string())?;
        }
        match img {
            DynamicImage::ImageLuma8(gray) => {
                encoder.write_image(gray.as_raw(), width, height, image::ExtendedColorType::L8)
            }
            DynamicImage::ImageRgb8(rgb) => {
                encoder.write_image(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
            }
            other => {
                let rgb = other.to_rgb8();
                encoder.write_image(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
            }
        }
        .map_err(|e| e.to_string())?;
    }
    let carried = header
        .carried
        .iter()
        .filter(|c| !(c.xmp && header.extras))
        .map(|c| c.segment.as_slice())
        .collect();
    Ok(splice(out, carried))
}

/// A JFIF density (unit, x, y) as the encoder takes it; none for an
/// unknown unit or a zero.
fn pixel_density((unit, x, y): (u8, u16, u16)) -> Option<PixelDensity> {
    let unit = match unit {
        0 => PixelDensityUnit::PixelAspectRatio,
        1 => PixelDensityUnit::Inches,
        2 => PixelDensityUnit::Centimeters,
        _ => return None,
    };
    (x > 0 && y > 0).then_some(PixelDensity {
        density: (x, y),
        unit,
    })
}

/// A JPEG and the segments that go in it at `at`, written out in place
/// rather than copied into a second JPEG.
struct Spliced<'a> {
    jpeg: Vec<u8>,
    at: usize,
    segments: Vec<&'a [u8]>,
}

impl Spliced<'_> {
    /// The bytes [`Self::write_to`] writes.
    fn len(&self) -> u64 {
        self.segments
            .iter()
            .fold(self.jpeg.len() as u64, |n, s| n + s.len() as u64)
    }

    fn write_to(&self, w: &mut impl Write) -> std::io::Result<()> {
        w.write_all(&self.jpeg[..self.at])?;
        for s in &self.segments {
            w.write_all(s)?;
        }
        w.write_all(&self.jpeg[self.at..])
    }
}

/// `jpeg` with `segments` to go after its leading APPn segments.
fn splice(jpeg: Vec<u8>, segments: Vec<&[u8]>) -> Spliced<'_> {
    // SOI, then each APPn: FF En, a 2-byte length.
    let mut at = 2;
    while let Some(&[0xFF, 0xE0..=0xEF, hi, lo, ..]) = jpeg.get(at..) {
        at += 2 + usize::from(u16::from_be_bytes([hi, lo]));
    }
    let at = at.min(jpeg.len());
    Spliced { jpeg, at, segments }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use image::GrayImage;

    use super::*;

    #[test]
    fn a_jpeg_at_or_under_the_presets_quality_is_left_alone() {
        use CompressPreset::{Balanced, High, Small};
        // (preset, long side, estimate, left alone)
        for (preset, long, estimate, left) in [
            (Small, 2048, Some(73), true),
            (Small, 2048, Some(74), false),
            (Small, 2049, Some(50), false),
            (Balanced, 4096, Some(83), true),
            (Balanced, 4097, Some(60), false),
            (High, 9000, Some(88), true),
            (High, 9000, Some(89), false),
            (High, 100, None, false),
        ] {
            assert_eq!(
                leaves(preset, long, estimate),
                left,
                "{preset:?} {long} {estimate:?}"
            );
        }
    }

    fn bytes(spliced: &Spliced) -> Vec<u8> {
        let mut out = Vec::new();
        spliced.write_to(&mut out).unwrap();
        out
    }

    #[test]
    fn carried_segments_go_after_the_encoders_app_segments() {
        let jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xE1, 0, 2, 0xFF, 0xDB, 0, 2,
        ];
        let spliced = splice(jpeg.clone(), vec![&[0xFF, 0xFE, 0, 3, b'!']]);
        assert_eq!(spliced.len(), 21);
        assert_eq!(
            bytes(&spliced),
            [
                0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xE1, 0, 2, 0xFF, 0xFE, 0, 3, b'!', 0xFF,
                0xDB, 0, 2
            ]
        );
        assert_eq!(bytes(&splice(jpeg.clone(), Vec::new())), jpeg);
    }

    #[test]
    fn try_small_only_when_small_does_more() {
        // Under 2048 px at quality 80: a 4:4:4 JPEG yes, a 4:2:0 one no.
        assert!(small_does_more(1900, false, Some(80)));
        assert!(!small_does_more(1900, true, Some(80)));
        // Small scales it: yes either way.
        assert!(small_does_more(3000, true, Some(80)));
        // Already at Small's quality and size: no.
        assert!(!small_does_more(1900, false, Some(70)));
    }

    #[test]
    fn the_peak_stays_under_the_limit_where_the_review_measured_it_over() {
        use CompressPreset::{Balanced, High, Small};
        let limit = crate::image::WORKING_SET_LIMIT;
        for (w, h, bpp, preset) in [
            (12_000, 12_000, 3, Balanced),
            (20_000, 20_000, 1, Balanced),
            (12_000, 12_000, 3, Small),
        ] {
            let p = peak(w, h, bpp, preset);
            assert!(p <= limit, "{w}x{h}x{bpp} {preset:?}: {p}");
        }
        // Over twice the cap: averaged by 2, then Lanczos3 from 6000 px.
        assert_eq!(scale::shrink_factor(12_000, 4096), 2);
        assert_eq!(scale::shrink_factor(8192, 4096), 1);
        assert_eq!(scale::shrink_factor(8193, 4096), 2);
        // No scale: the picture, and up to 2 bytes a pixel of JPEG, or 1
        // for gray, so a gray picture up to the decode limit fits at High.
        assert_eq!(peak(100, 10, 3, High), 5000);
        assert_eq!(peak(100, 10, 1, High), 2000);
        assert!(peak(19_000, 19_000, 1, High) <= limit);
        // A 4:1 picture just over the cap: 16 bytes a pixel of 4097 by
        // its new height, besides the picture and its result.
        let (nw, nh) = scale::fit(4097, 1024, 4096);
        assert_eq!((nw, nh), (4096, 1024));
        assert_eq!(
            peak(4097, 1024, 3, Balanced),
            4097 * 1024 * 3 + 4097 * 1024 * 16 + 4096 * 1024 * 3
        );
    }

    #[test]
    fn the_jfif_density_is_carried() {
        let img = DynamicImage::ImageLuma8(GrayImage::from_pixel(8, 8, image::Luma([90])));
        let header = Header {
            density: Some((1, 300, 300)),
            ..Header::default()
        };
        let out = bytes(&encode(&img, 80, &header, None, &CancelToken::default()).unwrap());
        // APP0 JFIF: version, then units 1 and 300×300.
        let at = out.windows(5).position(|w| w == b"JFIF\0").unwrap();
        assert_eq!(&out[at + 7..at + 12], &[1, 1, 44, 1, 44]);
        assert_eq!(pixel_density((9, 1, 1)), None);
        assert_eq!(pixel_density((1, 0, 72)), None);
        // A cancelled encode stops.
        let cancel = CancelToken::default();
        cancel.cancel();
        assert!(encode(&img, 80, &Header::default(), None, &cancel).is_err());
    }
}
