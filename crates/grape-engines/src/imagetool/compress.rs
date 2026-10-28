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
//! - Scaling: a picture over twice the long side is first averaged down by
//!   a whole factor (Grape's own loop, which a cancel stops at the next
//!   row), then Lanczos3 takes it the rest of the way, so that step holds
//!   and takes no more than for a picture of twice the long side. What the
//!   steps hold together is checked against D16's limit before decoding
//!   ([`peak`]).
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
use image::imageops::FilterType;
use image::{
    ColorType, DynamicImage, GenericImageView, GrayImage, ImageEncoder, RgbImage, RgbaImage,
};

use super::exif;
use super::jpeg::{self, Header};
use super::strip::Error;
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

/// The whole factor a picture with a `long` side is averaged down by
/// before Lanczos3 takes it to `cap`: none (1) unless it's over twice
/// `cap`, and never below `cap`.
fn shrink_factor(long: u64, cap: u64) -> u64 {
    if long > 2 * cap {
        long.div_ceil(2 * cap)
    } else {
        1
    }
}

/// `w`×`h` fitted inside `cap`×`cap`, about as `resize` rounds it.
fn fit(w: u64, h: u64, cap: u64) -> (u64, u64) {
    let long = w.max(h).max(1);
    (
        (w * cap).div_ceil(long).max(1),
        (h * cap).div_ceil(long).max(1),
    )
}

/// What Compress holds at its peak for a `width`×`height` decoded picture
/// of `bpp` bytes a pixel at `preset`, its steps one after another:
/// - the decoded picture and its averaged copy;
/// - the picture Lanczos3 starts from, its intermediate (`image` keeps one
///   of 16 bytes a pixel, the source's width by the result's height) and
///   the result;
/// - the picture encoded, an RGB copy unless it's gray or RGB, and about a
///   byte a pixel of JPEG.
fn peak(width: u32, height: u32, bpp: u64, preset: CompressPreset) -> u64 {
    let (w, h) = (u64::from(width), u64::from(height));
    let size = |w: u64, h: u64| w.saturating_mul(h).saturating_mul(bpp);
    let copy = if bpp == 1 || bpp == 3 { 0 } else { 3 };
    let encode = |w: u64, h: u64| w.saturating_mul(h).saturating_mul(bpp + copy + 1);
    let long = w.max(h);
    let Some(cap) = plan(preset).1.map(u64::from).filter(|&cap| long > cap) else {
        return encode(w, h);
    };
    let k = shrink_factor(long, cap);
    let (sw, sh) = (w.div_ceil(k), h.div_ceil(k));
    let averaged = if k > 1 { size(sw, sh) } else { 0 };
    let from = if k > 1 { averaged } else { size(w, h) };
    let (nw, nh) = fit(sw, sh, cap);
    let lanczos = sw.saturating_mul(nh).saturating_mul(16);
    [
        size(w, h).saturating_add(averaged),
        from.saturating_add(lanczos).saturating_add(size(nw, nh)),
        encode(nw, nh),
    ]
    .into_iter()
    .max()
    .unwrap_or(u64::MAX)
}

/// `src`, `w`×`h` pixels of `ch` 8-bit channels, with each `k`×`k` block
/// averaged into one pixel (the last ones from what's left); `None` if
/// cancelled, which is checked every output row.
fn average(src: &[u8], w: u32, h: u32, ch: usize, k: u32, cancel: &CancelToken) -> Option<Vec<u8>> {
    let (w, h, k) = (w as usize, h as usize, k as usize);
    let (nw, nh) = (w.div_ceil(k), h.div_ceil(k));
    let mut out = Vec::with_capacity(nw * nh * ch);
    let mut sums = vec![0u32; nw * ch];
    for oy in 0..nh {
        if cancel.is_cancelled() {
            return None;
        }
        sums.fill(0);
        let rows = oy * k..((oy + 1) * k).min(h);
        let tall = rows.len();
        for y in rows {
            let row = src.get(y * w * ch..(y + 1) * w * ch)?;
            for (x, px) in row.chunks_exact(ch).enumerate() {
                let at = x / k * ch;
                for (c, &v) in px.iter().enumerate() {
                    sums[at + c] += u32::from(v);
                }
            }
        }
        for ox in 0..nw {
            let wide = ((ox + 1) * k).min(w) - ox * k;
            let n = (wide * tall) as u32;
            for c in 0..ch {
                out.push(((sums[ox * ch + c] + n / 2) / n) as u8);
            }
        }
    }
    Some(out)
}

/// `img` averaged down by `k` ([`average`]); `None` if cancelled.
fn shrink(img: DynamicImage, k: u32, cancel: &CancelToken) -> Option<DynamicImage> {
    let (w, h) = img.dimensions();
    let (nw, nh) = (w.div_ceil(k), h.div_ceil(k));
    // `from_raw` is given exactly the buffer it wants.
    match img {
        DynamicImage::ImageLuma8(b) => {
            GrayImage::from_raw(nw, nh, average(&b, w, h, 1, k, cancel)?)
                .map(DynamicImage::ImageLuma8)
        }
        DynamicImage::ImageRgb8(b) => RgbImage::from_raw(nw, nh, average(&b, w, h, 3, k, cancel)?)
            .map(DynamicImage::ImageRgb8),
        DynamicImage::ImageRgba8(b) => {
            RgbaImage::from_raw(nw, nh, average(&b, w, h, 4, k, cancel)?)
                .map(DynamicImage::ImageRgba8)
        }
        // Not what a JPEG decodes to: an RGB copy first.
        other => {
            let rgb = other.to_rgb8();
            drop(other);
            RgbImage::from_raw(nw, nh, average(&rgb, w, h, 3, k, cancel)?)
                .map(DynamicImage::ImageRgb8)
        }
    }
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
    let img = match cap {
        Some(cap) if img.width().max(img.height()) > cap => {
            let long = u64::from(img.width().max(img.height()));
            let k = u32::try_from(shrink_factor(long, u64::from(cap))).unwrap_or(u32::MAX);
            let img = if k > 1 {
                shrink(img, k, cancel).ok_or_else(|| crate::cancelled(src))?
            } else {
                img
            };
            if cancel.is_cancelled() {
                return Err(crate::cancelled(src));
            }
            img.resize(cap, cap, FilterType::Lanczos3)
        }
        _ => img,
    };
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
    let after = encoded.len() as u64;
    if after.saturating_mul(10) > before.saturating_mul(9) {
        let detail = format!("{after} bytes from {before} in {}", src.display());
        return Err(names.not_smaller(try_small, detail));
    }

    publish::ensure_dir(job, ctx, dir, &names)?;
    let (temp, mut out) = TempFile::create(dir, from.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    out.write_all(&encoded)
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
fn encode(
    img: &DynamicImage,
    quality: u8,
    header: &Header,
    icc: Option<Vec<u8>>,
    cancel: &CancelToken,
) -> Result<Vec<u8>, String> {
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
    let carried: Vec<&[u8]> = header
        .carried
        .iter()
        .filter(|c| !(c.xmp && header.extras))
        .map(|c| c.segment.as_slice())
        .collect();
    Ok(splice(out, &carried))
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

/// `jpeg` with `segments` inserted after its leading APPn segments.
fn splice(jpeg: Vec<u8>, segments: &[&[u8]]) -> Vec<u8> {
    if segments.is_empty() {
        return jpeg;
    }
    // SOI, then each APPn: FF En, a 2-byte length.
    let mut at = 2;
    while let Some(&[0xFF, 0xE0..=0xEF, hi, lo, ..]) = jpeg.get(at..) {
        at += 2 + usize::from(u16::from_be_bytes([hi, lo]));
    }
    let at = at.min(jpeg.len());
    let mut out = jpeg[..at].to_vec();
    for s in segments {
        out.extend_from_slice(s);
    }
    out.extend_from_slice(&jpeg[at..]);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

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

    #[test]
    fn carried_segments_go_after_the_encoders_app_segments() {
        let jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xE1, 0, 2, 0xFF, 0xDB, 0, 2,
        ];
        let out = splice(jpeg.clone(), &[&[0xFF, 0xFE, 0, 3, b'!']]);
        assert_eq!(
            out,
            [
                0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xE1, 0, 2, 0xFF, 0xFE, 0, 3, b'!', 0xFF,
                0xDB, 0, 2
            ]
        );
        assert_eq!(splice(jpeg.clone(), &[]), jpeg);
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
        assert_eq!(shrink_factor(12_000, 4096), 2);
        assert_eq!(shrink_factor(8192, 4096), 1);
        assert_eq!(shrink_factor(8193, 4096), 2);
        // No scale: the picture, and about a byte a pixel of JPEG.
        assert_eq!(peak(100, 10, 3, High), 4000);
        // A 4:1 picture just over the cap: 16 bytes a pixel of 4097 by
        // its new height, besides the picture and its result.
        let (nw, nh) = fit(4097, 1024, 4096);
        assert_eq!((nw, nh), (4096, 1024));
        assert_eq!(
            peak(4097, 1024, 3, Balanced),
            4097 * 1024 * 3 + 4097 * 1024 * 16 + 4096 * 1024 * 3
        );
    }

    #[test]
    fn averaging_takes_each_block_including_the_last_partial_ones() {
        let cancel = CancelToken::default();
        // 5×3 gray, by 2: blocks of 2×2, 1×2, 2×1 and 1×1.
        let src: Vec<u8> = (0..15).map(|v| v * 10).collect();
        let out = average(&src, 5, 3, 1, 2, &cancel).unwrap();
        // (0+10+50+60)/4, (20+30+70+80)/4, (40+90)/2; (100+110)/2, ..., 140.
        assert_eq!(out, [30, 50, 65, 105, 125, 140]);
        // RGB keeps its channels apart.
        let rgb = [10, 20, 30, 30, 40, 50];
        assert_eq!(average(&rgb, 2, 1, 3, 2, &cancel).unwrap(), [20, 30, 40]);
        cancel.cancel();
        assert!(average(&src, 5, 3, 1, 2, &cancel).is_none());
    }

    #[test]
    fn shrinking_keeps_gray_rgb_and_rgba_as_they_are() {
        let cancel = CancelToken::default();
        let rgb = RgbImage::from_raw(2, 1, vec![10, 20, 30, 30, 40, 50]).unwrap();
        let out = shrink(DynamicImage::ImageRgb8(rgb.clone()), 2, &cancel).unwrap();
        assert_eq!(out.as_rgb8().unwrap().as_raw(), &[20, 30, 40]);
        let gray = GrayImage::from_raw(2, 1, vec![10, 30]).unwrap();
        let out = shrink(DynamicImage::ImageLuma8(gray), 2, &cancel).unwrap();
        assert_eq!(out.as_luma8().unwrap().as_raw(), &[20]);
        let rgba = RgbaImage::from_raw(2, 1, vec![10, 20, 30, 40, 30, 40, 50, 60]).unwrap();
        let out = shrink(DynamicImage::ImageRgba8(rgba), 2, &cancel).unwrap();
        assert_eq!(out.as_rgba8().unwrap().as_raw(), &[20, 30, 40, 50]);
        // Anything else is averaged as RGB.
        let wide = DynamicImage::ImageRgb8(rgb).to_rgb16();
        let out = shrink(DynamicImage::ImageRgb16(wide), 2, &cancel).unwrap();
        assert_eq!(out.as_rgb8().unwrap().as_raw(), &[20, 30, 40]);
    }

    #[test]
    fn the_jfif_density_is_carried() {
        let img = DynamicImage::ImageLuma8(GrayImage::from_pixel(8, 8, image::Luma([90])));
        let header = Header {
            density: Some((1, 300, 300)),
            ..Header::default()
        };
        let out = encode(&img, 80, &header, None, &CancelToken::default()).unwrap();
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
