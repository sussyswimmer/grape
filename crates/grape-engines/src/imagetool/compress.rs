//! Compress on JPEG (D26): decoded as stored (the EXIF orientation not
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
//! - Carried: the EXIF (PixelXDimension and PixelYDimension follow a
//!   scale), the ICC profile, XMP and extended XMP, IPTC (APP13) and
//!   comments. A JPEG with MPF or data after EOI (a gain map, a motion
//!   photo) loses those, and its XMP, which describes them.
//! - CMYK and YCCK JPEGs are refused: image's encoder writes gray or RGB.

use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;

use grape_core::{CompressPreset, Format, Job};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageEncoder};

use super::exif;
use super::jpeg::{self, Header};
use super::strip::Error;
use crate::cancel::Cancellable;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{EngineCtx, ErrorKind, GrapeError, JobOutcome, Note};

/// The formats Compress runs on through this engine.
pub(crate) fn compresses(from: Format) -> bool {
    matches!(from, Format::Jpg)
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
        preset != CompressPreset::Small && !leaves(CompressPreset::Small, long, estimate);
    if leaves(preset, long, estimate) {
        let detail = format!(
            "{long} px at about quality {estimate:?} in {}",
            src.display()
        );
        return Err(names.not_smaller(try_small, detail));
    }
    progress(0.1);

    let (img, icc) = crate::image::decode_stored(src, from, Format::Jpg, &names, cancel)?;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    progress(0.5);
    let (quality, cap) = plan(preset);
    let img = match cap {
        Some(cap) if img.width().max(img.height()) > cap => {
            img.resize(cap, cap, FilterType::Lanczos3)
        }
        _ => img,
    };
    let encoded = encode(&img, quality, &header, icc)
        .map_err(|e| names.encode_failed(format!("encode {}: {e}", src.display())))?;
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
/// `icc` profile, and the segments `header` carries, after the encoder's
/// own APPn segments.
fn encode(
    img: &DynamicImage,
    quality: u8,
    header: &Header,
    icc: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let (width, height) = img.dimensions();
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
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
        other => {
            let rgb = other.to_rgb8();
            encoder.write_image(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
        }
    }
    .map_err(|e| e.to_string())?;
    let carried: Vec<&[u8]> = header
        .carried
        .iter()
        .filter(|c| !(c.xmp && header.extras))
        .map(|c| c.segment.as_slice())
        .collect();
    Ok(splice(out, &carried))
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
}
