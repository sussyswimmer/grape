//! Compress on WebP (D28): image-webp decodes the picture as stored (an
//! extended file's VP8X flags read as its chunks bear them out, as libwebp
//! reads them: [`decode_flags`]), Grape scales a lossy one down, libwebp
//! 1.6.0 encodes it again through `grape-webp`, and Grape puts the
//! source's metadata back in the file libwebp wrote ([`webp::assemble`]).
//!
//! | Source | Small | Balanced | High |
//! |---|---|---|---|
//! | Lossy (`VP8 `) | long side ≤ 2048, q70 | ≤ 4096, q80 | as it is, q85 |
//! | Lossless (`VP8L`) | lossless | lossless | lossless |
//!
//! - **Lossy:** libwebp's method 6, or 5 when the picture has
//!   transparency ([`LOSSY_METHOD`]): libwebp encodes the alpha plane at
//!   the picture's method, losslessly, and at 6 that took 9 to 18 times as
//!   long as at 5, for 0.05 % (a sticker's alpha) to 10 % (soft alpha)
//!   less. Straight to YUV 4:2:0 (a lossy source is 4:2:0 already), alpha
//!   exact, the color under fully transparent pixels free to change; a
//!   picture with transparency is scaled by area, its color weighed by its
//!   alpha, its alpha kept to the values the source's has but its thin
//!   strokes whole, and its fully transparent pixels colored as the visible
//!   ones nearest them ([`scale`]). The qualities are JPEG's, so a preset
//!   means the same across formats.
//!   Kept when it's at most 90 % of the source's size; "Try Small" only
//!   when Small would do more ([`small_does_more`]).
//! - **Lossless:** at every preset, never scaled, libwebp's lossless
//!   preset [`LOSSLESS_LEVEL`] with every pixel's color kept (`exact`),
//!   even under full transparency. The output is decoded again and must
//!   match the picture pixel for pixel, or it's `EncodeFailed`. Kept when
//!   it's at most 95 % of the source's size; never "Try Small".
//! - **Carried:** the first `ICCP`, `EXIF` (PixelXDimension and
//!   PixelYDimension follow a scale, after an optional `Exif\0\0`) and
//!   `XMP ` of an extended file, with VP8X's flags for them as they were.
//!   Unknown chunks and anything after the RIFF are dropped, as PNG
//!   Compress drops them. The pixels stay as stored and the EXIF says how
//!   to turn them, as for JPEG.
//! - **Refused:** an animation; a file whose alpha libwebp and image-webp
//!   read differently (VP8X's flag against `VP8L`'s alpha bit or the
//!   `ALPH` chunks, past the RIFF's end too), whose transparency Compress
//!   can't keep for both; and what [`webp::scan`] finds broken.
//! - **Memory:** what the steps hold is checked against D16's limit
//!   before decoding ([`peak`]), with libwebp's share measured
//!   ([`LOSSY_BYTES`], [`LOSSY_ALPHA_BYTES`], [`LOSSLESS_BYTES`]).
//! - **Cancel:** the token is checked between steps and at each of
//!   libwebp's progress reports, which end the encode
//!   ([`grape_webp::encode`] says when libwebp reports). At the largest
//!   pictures [`peak`] admits, reports came at most this far apart (release
//!   build, 4 vCPUs): lossy at 6500², 1 s for a real photo and 6.0 s for
//!   one under heavy grain (±32), which libwebp encodes 4 times over to fit
//!   VP8's first partition (5 times and 14.5 s before `grape-webp` set its
//!   partition limit); lossy with alpha at 3700², 9.3 s while libwebp
//!   finished a soft alpha plane with noise (0.1 s for a sticker's);
//!   lossless at 4096², 4.2 s for a picture of many sharp shapes (3.0 s for
//!   a real photo).

use std::fs::File;
use std::io::{BufReader, Cursor, Write};
use std::path::Path;

use grape_core::{CompressPreset, Format, Job};
use grape_webp::{Mode, Options, Pixels};
use image::{ColorType, DynamicImage, GenericImageView};

use super::exif;
use super::scale;
use super::strip::{self, Error};
use super::webp::{self, Scan, ALPHA_FLAG, EXIF_FLAG, VP8X_FLAGS_AT, XMP_FLAG};
use crate::cancel::Cancellable;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome, Note};

/// libwebp's method for a lossy picture, by whether it has transparency
/// ([module docs](self)).
const LOSSY_METHOD: (u8, u8) = (6, 5);

/// libwebp's lossless preset (cwebp's `-z`): 8 is method 5 at quality 90.
/// 9 (method 6, quality 100) took 26 to 31 s for a 12 MP photo with a
/// second thread, holding 87 bytes a pixel (50 to 55 s without), for at
/// most 0.2 % less than 8's 15 to 18 s and 37 bytes.
const LOSSLESS_LEVEL: u8 = 8;

/// What libwebp holds besides the picture, a pixel, to encode an RGB
/// picture lossily: its YUV planes, and at method 6 every token of the
/// picture before it writes them, which grow with the detail. Measured in
/// a release build at 2.3 for photo-like pictures and up to 17.5 for
/// noise, plus a quarter.
const LOSSY_BYTES: u64 = 22;

/// The same with alpha: the YUV and alpha planes, and beside them the
/// lossless encode of the alpha plane (a picture of its own, 4 bytes a
/// pixel, and that encoder's buffers). Measured at 17 to 18 for a
/// sticker's alpha, 27 to 29 for soft edges and up to 57.7 for noise, plus
/// a quarter.
const LOSSY_ALPHA_BYTES: u64 = 73;

/// What libwebp holds besides the picture, a pixel, for a lossless encode
/// at [`LOSSLESS_LEVEL`]: the ARGB copy, its transforms, hash chain and
/// backward references. Measured at 36 to 38 for 12 and 16 MP photo-like
/// pictures, 45.7 for 65 MP and up to 46.1 for noise, plus a quarter. The
/// output (at most about 4 bytes a pixel) and the RGBA it decodes to for
/// the check (4) come once libwebp's buffers are freed.
const LOSSLESS_BYTES: u64 = 58;

/// A lossy preset's quality and the long side it scales down to.
fn plan(preset: CompressPreset) -> (f32, Option<u32>) {
    match preset {
        CompressPreset::Small => (70.0, Some(2048)),
        CompressPreset::Balanced => (80.0, Some(4096)),
        CompressPreset::High => (85.0, None),
    }
}

/// Whether Small would do more than another preset to the lossy WebP
/// `scan` found, a file of `before` bytes (D26): it scales the picture (a
/// side over 2048), or the source's quality ([`webp::quality`]) is finer
/// than Small's 70 and its `VP8 ` data holds detail a coarser quality
/// drops (2 bytes a macroblock or more: a flat or fully transparent
/// picture takes about half a byte, whatever its quality); and either way
/// its image data (`ALPH` and `VP8 `) is a fifth of the file or more, so
/// that what Small drops can bring the file under 90 %. Small's method 6
/// at q70 made Pillow's q70 to q75 photos (method 4) 11 to 23 % smaller,
/// and some of its q60 to q68 ones too; a q55 one it couldn't.
fn small_does_more(scan: &Scan, before: u64) -> bool {
    let (quality, cap) = plan(CompressPreset::Small);
    let (w, h) = scan.size;
    let scales = cap.is_some_and(|cap| w.max(h) > cap);
    let macroblocks = u64::from(w.div_ceil(16)) * u64::from(h.div_ceil(16));
    let detail = scan.image_bytes >= macroblocks.saturating_mul(2);
    let finer = detail && scan.quality.is_some_and(|q| f32::from(q) > quality);
    let share = scan
        .image_bytes
        .saturating_add(scan.alpha_bytes)
        .saturating_mul(5)
        >= before;
    share && (scales || finer)
}

/// What Compress holds at its peak for a `width`×`height` decoded picture
/// of `bpp` bytes a pixel ([`scale::peak`]): to encode, the picture and
/// libwebp's buffers; a lossy one scaled to fit `cap`, a lossless one as
/// it is.
fn peak(width: u32, height: u32, bpp: u64, lossless: bool, cap: Option<u32>) -> u64 {
    let libwebp = match (lossless, bpp) {
        (true, _) => LOSSLESS_BYTES,
        (false, 4) => LOSSY_ALPHA_BYTES,
        (false, _) => LOSSY_BYTES,
    };
    let encode = |w: u64, h: u64, bpp: u64| w.saturating_mul(h).saturating_mul(bpp + libwebp);
    let cap = if lossless { None } else { cap };
    scale::peak(width, height, bpp, cap, &encode)
}

/// Runs Compress at `preset` on the WebP `src` (named as `from`) into
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
    // Refused unread, as the decoder would refuse it.
    if before > crate::image::MAX_SOURCE {
        return Err(names.too_large(format!(
            "compress {}: the file is {} MiB, over the {} MiB limit for a WebP source",
            src.display(),
            before >> 20,
            crate::image::MAX_SOURCE >> 20
        )));
    }
    let scan = webp::scan(&mut BufReader::new(Cancellable::new(file, cancel))).map_err(|e| {
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
    if scan.animated {
        return Err(GrapeError::new(
            ErrorKind::NotYetSupported,
            format!(
                "{} is animated. Compress works on still WebP images for now.",
                names.source
            ),
            format!("an animated WebP: {}", src.display()),
        ));
    }
    if let Some(why) = alpha_disagrees(&scan) {
        return Err(names.decode_failed(format!("{}: {why}", src.display())));
    }
    progress(0.1);

    let (quality, cap) = plan(preset);
    let lossless = scan.lossless;
    let try_small = !lossless && preset != CompressPreset::Small && small_does_more(&scan, before);
    // The metadata is held throughout, and again in the file written.
    let metadata = scan.carried.len().saturating_mul(2);
    let tool_peak = |width: u32, height: u32, color: ColorType| {
        let bpp = u64::from(color.bytes_per_pixel());
        peak(width, height, bpp, lossless, cap).saturating_add(metadata)
    };
    let decoded = match decode_flags(&scan) {
        Some(flags) => crate::image::decode_stored_patched(
            src,
            from,
            Format::Webp,
            &names,
            cancel,
            &tool_peak,
            (VP8X_FLAGS_AT, flags),
        ),
        None => crate::image::decode_stored(src, from, Format::Webp, &names, cancel, &tool_peak),
    };
    let (img, _icc) = decoded.map_err(|e| publish::or_cancelled(cancel, src, e))?;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let img = rgb_or_rgba(img);
    let transparent = transparent(&img);
    if lossless && transparent && !scan.alpha {
        // VP8X says alpha and VP8L's bit says none: libwebp shows it
        // opaque, image-webp didn't.
        let detail = format!(
            "{}: transparent in image-webp, opaque in libwebp",
            src.display()
        );
        return Err(names.decode_failed(detail));
    }
    progress(0.4);
    let img = if lossless {
        img
    } else {
        scale::scale(img, cap, cancel).ok_or_else(|| crate::cancelled(src))?
    };
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    progress(0.5);

    let (width, height) = img.dimensions();
    let opts = Options {
        mode: if lossless {
            Mode::Lossless {
                level: LOSSLESS_LEVEL,
            }
        } else {
            let method = if transparent {
                LOSSY_METHOD.1
            } else {
                LOSSY_METHOD.0
            };
            Mode::Lossy { quality, method }
        },
        // A thread more for a lossy picture's alpha plane. A lossless one
        // gains nothing at its level but a second encoder's buffers.
        threads: !lossless,
    };
    let encoded = encode(&img, &opts, cancel, progress).map_err(|e| {
        let detail = format!("encode {}: {e}", src.display());
        let e = match e {
            grape_webp::Error::Cancelled => crate::cancelled(src),
            grape_webp::Error::OutOfMemory | grape_webp::Error::TooLarge => names.too_large(detail),
            _ => names.encode_failed(detail),
        };
        publish::or_cancelled(cancel, src, e)
    })?;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    progress(0.9);

    let mut carried = scan.carried;
    if let Some(exif) = carried.exif.as_mut() {
        let at = if exif.starts_with(b"Exif\0\0") { 6 } else { 0 };
        exif::patch_dimensions(&mut exif[at..], width, height);
    }
    let out = webp::assemble(encoded, width, height, &carried).map_err(|e| {
        let why = match e {
            Error::Broken(why) | Error::TooLarge(why) | Error::Refused(why) => why,
            Error::Read(e) | Error::Write(e) => e.to_string(),
        };
        names.encode_failed(format!("assemble {}: {why}", src.display()))
    })?;
    drop(carried);
    // A cancel during the last report is a cancel, not a verdict on size.
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let after = out.len() as u64;
    // 90 % lossy, 95 % exact (D26).
    let most = if lossless {
        before.saturating_mul(19) / 20
    } else {
        before.saturating_mul(9) / 10
    };
    if after > most {
        let detail = format!("{after} bytes from {before} in {}", src.display());
        return Err(names.not_smaller(try_small, detail));
    }
    if lossless {
        check_lossless(&out, &img)
    } else {
        check_lossy(&out, width, height)
    }
    .map_err(|why| names.encode_failed(format!("check {}: {why}", src.display())))?;
    drop(img);
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    progress(0.98);

    publish::ensure_dir(job, ctx, dir, &names)?;
    let (temp, mut file) = TempFile::create(dir, from.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    file.write_all(&out)
        .and_then(|()| file.sync_all())
        .map_err(|e| names.write_failed(format!("write {}: {e}", temp.path().display())))?;
    drop(file);
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let mut outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    progress(1.0);
    outcome.notes.push(Note::Compressed { before, after });
    Ok(outcome)
}

/// Why `scan`'s alpha reads differently in libwebp (by the chunks) and
/// image-webp (by VP8X's flag, then the first `ALPH` anywhere it looks,
/// past the RIFF's end too), if it does: Compress would make one of them
/// see a change.
fn alpha_disagrees(scan: &Scan) -> Option<&'static str> {
    let flagged = scan.vp8x?.flags & ALPHA_FLAG != 0;
    if !flagged && scan.alpha {
        Some("VP8X says no alpha, and its image has some libwebp reads")
    } else if flagged && !scan.lossless && !scan.alpha && scan.late_alpha {
        Some("its only ALPH chunk comes after its VP8, where libwebp never reads it")
    } else {
        None
    }
}

/// VP8X's flags as image-webp should read `scan`'s file to see what
/// libwebp does, when they aren't the file's own: alpha off for a lossy
/// image with no `ALPH` chunk before it (one only after it, with the flag,
/// is refused before this), and the EXIF and XMP flags off, which change
/// no pixel. image-webp refuses a flag whose chunk is missing ("An expected
/// chunk was missing"), and libwebp, Pillow and ImageMagick read the file.
fn decode_flags(scan: &Scan) -> Option<u8> {
    let flags = scan.vp8x?.flags;
    let mut view = flags & !(EXIF_FLAG | XMP_FLAG);
    if !scan.lossless && !scan.alpha {
        view &= !ALPHA_FLAG;
    }
    (view != flags).then_some(view)
}

/// `img` as 8-bit RGB or RGBA, what image-webp decodes to.
fn rgb_or_rgba(img: DynamicImage) -> DynamicImage {
    match img {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) => img,
        other if other.color().has_alpha() => DynamicImage::ImageRgba8(other.to_rgba8()),
        other => DynamicImage::ImageRgb8(other.to_rgb8()),
    }
}

/// Whether any pixel of `img` isn't opaque.
fn transparent(img: &DynamicImage) -> bool {
    img.as_rgba8().is_some_and(|rgba| {
        rgba.as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| p[3] != u8::MAX)
    })
}

/// `img` (8-bit RGB or RGBA) encoded with `opts`; libwebp's progress goes
/// to 0.5–0.9 of `progress`, never back (a lossless encode's percent can
/// go down), and a cancel ends the encode at its next report.
fn encode(
    img: &DynamicImage,
    opts: &Options,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<u8>, grape_webp::Error> {
    let (width, height) = img.dimensions();
    let px = match img {
        DynamicImage::ImageRgba8(rgba) => Pixels::Rgba {
            data: rgba.as_raw(),
            width,
            height,
        },
        DynamicImage::ImageRgb8(rgb) => Pixels::Rgb {
            data: rgb.as_raw(),
            width,
            height,
        },
        _ => return Err(grape_webp::Error::BadInput),
    };
    let mut most = 0;
    grape_webp::encode(px, opts, &mut |percent| {
        most = most.max(percent);
        progress(0.5 + 0.4 * f32::from(most) / 100.0);
        !cancel.is_cancelled()
    })
}

/// A lossless `file` decoded again by image-webp must be `img`, pixel for
/// pixel: RGBA against RGB when every pixel is opaque.
fn check_lossless(file: &[u8], img: &DynamicImage) -> Result<(), String> {
    let decoder =
        image::codecs::webp::WebPDecoder::new(Cursor::new(file)).map_err(|e| e.to_string())?;
    let back = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if back.dimensions() != img.dimensions() {
        return Err(format!(
            "{:?} decoded, {:?} encoded",
            back.dimensions(),
            img.dimensions()
        ));
    }
    let same = match (img, &back) {
        (DynamicImage::ImageRgb8(a), DynamicImage::ImageRgb8(b)) => a.as_raw() == b.as_raw(),
        (DynamicImage::ImageRgba8(a), DynamicImage::ImageRgba8(b)) => a.as_raw() == b.as_raw(),
        (DynamicImage::ImageRgba8(a), DynamicImage::ImageRgb8(b))
        | (DynamicImage::ImageRgb8(b), DynamicImage::ImageRgba8(a)) => a
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .zip(b.as_raw().as_chunks::<3>().0)
            .all(|(a, b)| a[3] == u8::MAX && a[..3] == *b),
        _ => false,
    };
    if same {
        Ok(())
    } else {
        Err("the pixels decoded aren't the ones encoded".into())
    }
}

/// A lossy `file` walked again ([`webp::scan`]): its RIFF the whole file,
/// one `VP8 ` image of `width`×`height`, its canvas the same, and VP8X's
/// alpha flag set exactly when an `ALPH` chunk comes before the image.
fn check_lossy(file: &[u8], width: u32, height: u32) -> Result<(), String> {
    let scan = webp::scan(&mut Cursor::new(file)).map_err(|e| match e {
        Error::Broken(why) | Error::TooLarge(why) | Error::Refused(why) => why,
        Error::Read(e) | Error::Write(e) => e.to_string(),
    })?;
    let riff = file
        .get(4..8)
        .map(|b| u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])) + 8);
    if riff != Some(file.len() as u64) {
        return Err(format!(
            "a RIFF of {riff:?} bytes in a file of {}",
            file.len()
        ));
    }
    if scan.lossless || scan.size != (width, height) || scan.late_alpha {
        return Err(format!("{scan:?} for {width}x{height}"));
    }
    match scan.vp8x {
        Some(vp8x)
            if vp8x.canvas != (width, height) || (vp8x.flags & ALPHA_FLAG != 0) != scan.alpha =>
        {
            Err(format!("{vp8x:?} with alpha {}", scan.alpha))
        }
        None if scan.alpha => Err("alpha in a simple file".into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    use super::*;
    use crate::image::WORKING_SET_LIMIT;
    use crate::imagetool::webp::{Carried, Vp8x};

    /// A photo-like picture: smooth shapes with a fine, faint texture.
    fn photo(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            let (x, y) = (x as f32, y as f32);
            let grain = 4.0 * (x / 2.3).sin() * (y / 3.1).cos();
            let v = |base: f32| (base + grain).clamp(0.0, 255.0) as u8;
            Rgb([
                v(128.0 + 80.0 * (x / 41.0).sin() * (y / 53.0).cos()),
                v(128.0 + 70.0 * ((x + y) / 67.0).sin()),
                v(100.0 + 50.0 * (x / 29.0 - y / 31.0).cos()),
            ])
        })
    }

    /// `rgb` with alpha: opaque, a soft edge, and a transparent hole.
    fn with_alpha(rgb: &RgbImage) -> RgbaImage {
        let (w, h) = rgb.dimensions();
        RgbaImage::from_fn(w, h, |x, y| {
            let [r, g, b] = rgb.get_pixel(x, y).0;
            let a = if x < w / 4 {
                0
            } else {
                (y * 255 / h) as u8 | 0x80
            };
            Rgba([r, g, b, a])
        })
    }

    fn lossy(quality: f32) -> Options {
        Options {
            mode: Mode::Lossy { quality, method: 6 },
            threads: true,
        }
    }

    const LOSSLESS: Options = Options {
        mode: Mode::Lossless {
            level: LOSSLESS_LEVEL,
        },
        threads: false,
    };

    fn encoded(img: &DynamicImage, opts: &Options) -> Vec<u8> {
        encode(img, opts, &CancelToken::default(), &mut |_| {}).unwrap()
    }

    #[test]
    fn the_presets_are_jpegs_and_a_lossless_picture_is_never_scaled() {
        assert_eq!(plan(CompressPreset::Small), (70.0, Some(2048)));
        assert_eq!(plan(CompressPreset::Balanced), (80.0, Some(4096)));
        assert_eq!(plan(CompressPreset::High), (85.0, None));
        // Lossless: the picture and libwebp's share, whatever the cap.
        assert_eq!(
            peak(9000, 100, 3, true, Some(2048)),
            9000 * 100 * (3 + LOSSLESS_BYTES)
        );
        // Lossy, scaled: the decoded picture and its steps, then the
        // scaled picture and libwebp's share.
        let (w, h) = scale::fit(2100, 100, 2048);
        assert_eq!(
            peak(2100, 100, 3, false, Some(2048)),
            (2100 * 100 * 3 + 2100 * h * 16 + w * h * 3).max(w * h * (3 + LOSSY_BYTES))
        );
        assert_eq!(
            peak(100, 100, 4, false, None),
            100 * 100 * (4 + LOSSY_ALPHA_BYTES)
        );
    }

    #[test]
    fn the_measured_shares_keep_the_peak_under_the_limit() {
        let fits = |w, h, bpp, lossless, cap| peak(w, h, bpp, lossless, cap) <= WORKING_SET_LIMIT;
        // Lossless: a 12 MP photo, with alpha or not; not 20 MP.
        assert!(fits(4000, 3000, 4, true, None));
        assert!(!fits(5000, 4000, 3, true, None));
        // Lossy RGB at High: 24 MP, not 48. With alpha: 12 MP at High,
        // and a 67 MP one scaled down at Small.
        assert!(fits(6000, 4000, 3, false, None));
        assert!(!fits(8000, 6000, 3, false, None));
        assert!(fits(4000, 3000, 4, false, None));
        assert!(!fits(4096, 4096, 4, false, Some(4096)));
        assert!(fits(8192, 8192, 4, false, Some(2048)));
    }

    #[test]
    fn the_quality_is_read_back_from_libwebps_files() {
        // A photo-like picture and flat colors, at methods 4 (Pillow's and
        // cwebp's) and 6: within 2 points of the quality libwebp was given.
        let flat = RgbImage::from_fn(320, 240, |x, y| {
            if (x / 40 + y / 40) % 2 == 0 {
                Rgb([200, 40, 40])
            } else {
                Rgb([30, 30, 160])
            }
        });
        for src in [photo(320, 240), flat] {
            for quality in [30.0, 50.0, 70.0, 75.0, 80.0, 90.0] {
                for method in [4, 6] {
                    let opts = Options {
                        mode: Mode::Lossy { quality, method },
                        threads: false,
                    };
                    let file = encoded(&DynamicImage::ImageRgb8(src.clone()), &opts);
                    let scan = webp::scan(&mut Cursor::new(&file)).unwrap();
                    let estimate = f32::from(scan.quality.unwrap());
                    assert!((estimate - quality).abs() <= 2.0, "{quality}: {estimate}");
                    assert_eq!(scan.image_bytes as usize, file.len() - 20);
                }
            }
        }
    }

    #[test]
    fn try_small_only_when_small_does_more() {
        // 1600 × 900, 5,700 macroblocks: a q90 photo of 200 KB.
        let photo = Scan {
            size: (1600, 900),
            quality: Some(90),
            image_bytes: 200_000,
            ..Scan::default()
        };
        assert!(small_does_more(&photo, 200_100));
        assert!(small_does_more(
            &Scan {
                quality: Some(71),
                ..photo.clone()
            },
            200_100
        ));
        // Its quality Small's 70 or coarser (q50, q30), or unread: no.
        for quality in [Some(70), Some(50), Some(30), None] {
            let coarse = Scan {
                quality,
                ..photo.clone()
            };
            assert!(!small_does_more(&coarse, 200_100), "{quality:?}");
        }
        // Flat (a fully transparent picture's VP8 takes about half a byte
        // a macroblock): no, at any quality.
        let flat = Scan {
            image_bytes: 2_636,
            alpha_bytes: 75,
            ..photo.clone()
        };
        assert!(!small_does_more(&flat, 2_758));
        // From 2 bytes a macroblock, yes.
        assert!(!small_does_more(
            &Scan {
                image_bytes: 11_399,
                ..flat.clone()
            },
            11_500
        ));
        assert!(small_does_more(
            &Scan {
                image_bytes: 11_400,
                ..flat.clone()
            },
            11_500
        ));
        // Metadata over four times the image: no.
        assert!(!small_does_more(&photo, 1_000_001));
        assert!(small_does_more(&photo, 1_000_000));
        // A side over 2048, whatever its quality and detail, but not when
        // outweighed.
        let wide = Scan {
            size: (2049, 100),
            quality: Some(30),
            image_bytes: 300,
            alpha_bytes: 100,
            ..Scan::default()
        };
        assert!(small_does_more(&wide, 2_000));
        assert!(!small_does_more(&wide, 2_001));
        assert!(!small_does_more(
            &Scan {
                size: (2048, 100),
                ..wide
            },
            400
        ));
    }

    #[test]
    fn alpha_libwebp_and_image_webp_read_differently_is_refused() {
        let scan = |flags: u8, lossless: bool, alpha: bool, late_alpha: bool| Scan {
            vp8x: Some(Vp8x {
                flags,
                canvas: (9, 9),
            }),
            lossless,
            size: (9, 9),
            alpha,
            late_alpha,
            ..Scan::default()
        };
        // Agreed: none, or flagged and there.
        assert_eq!(alpha_disagrees(&scan(0, false, false, false)), None);
        assert_eq!(alpha_disagrees(&scan(0x10, false, true, false)), None);
        assert_eq!(alpha_disagrees(&scan(0x10, true, true, false)), None);
        // A late ALPH after one image-webp and libwebp both read.
        assert_eq!(alpha_disagrees(&scan(0x10, false, true, true)), None);
        // Unflagged alpha libwebp shows and image-webp drops.
        assert!(alpha_disagrees(&scan(0, false, true, false)).is_some());
        assert!(alpha_disagrees(&scan(0, true, true, false)).is_some());
        // Only a late ALPH: image-webp reads it, libwebp doesn't.
        assert!(alpha_disagrees(&scan(0x10, false, false, true)).is_some());
        // A simple file has no flag to disagree with.
        let simple = Scan {
            vp8x: None,
            ..scan(0, true, true, false)
        };
        assert_eq!(alpha_disagrees(&simple), None);
    }

    #[test]
    fn image_webp_reads_the_flags_the_chunks_bear_out() {
        let scan = |flags: u8, lossless: bool, alpha: bool| Scan {
            vp8x: Some(Vp8x {
                flags,
                canvas: (9, 9),
            }),
            lossless,
            size: (9, 9),
            alpha,
            ..Scan::default()
        };
        // As they are.
        assert_eq!(decode_flags(&scan(0x30, false, true)), None);
        assert_eq!(decode_flags(&scan(0x20, false, false)), None);
        assert_eq!(decode_flags(&scan(0x10, true, false)), None);
        assert_eq!(decode_flags(&Scan::default()), None);
        // Alpha with no ALPH chunk before a lossy image; EXIF and XMP.
        assert_eq!(decode_flags(&scan(0x30, false, false)), Some(0x20));
        assert_eq!(decode_flags(&scan(0x3C, false, true)), Some(0x30));
        assert_eq!(decode_flags(&scan(0x1C, true, true)), Some(0x10));
    }

    #[test]
    fn a_lossless_output_must_decode_to_the_same_pixels() {
        let rgba = DynamicImage::ImageRgba8(with_alpha(&photo(33, 17)));
        let file = encoded(&rgba, &LOSSLESS);
        assert_eq!(check_lossless(&file, &rgba), Ok(()));
        let mut other = rgba.to_rgba8();
        other.get_pixel_mut(5, 5).0[3] ^= 1;
        assert!(check_lossless(&file, &DynamicImage::ImageRgba8(other)).is_err());
        // Opaque RGBA comes back as RGB: the same pixels.
        let opaque = DynamicImage::ImageRgba8(DynamicImage::ImageRgb8(photo(8, 8)).to_rgba8());
        let file = encoded(&opaque, &LOSSLESS);
        assert_eq!(check_lossless(&file, &opaque), Ok(()));
        let mut seen = opaque.to_rgba8();
        seen.get_pixel_mut(0, 0).0[3] = 254;
        assert!(check_lossless(&file, &DynamicImage::ImageRgba8(seen)).is_err());
        assert!(check_lossless(&file[..file.len() / 2], &opaque).is_err());
        let small = DynamicImage::ImageRgba8(opaque.to_rgba8().view(0, 0, 4, 4).to_image());
        assert!(check_lossless(&file, &small).is_err());
    }

    #[test]
    fn a_lossy_output_is_walked_again() {
        for img in [
            DynamicImage::ImageRgb8(photo(31, 9)),
            DynamicImage::ImageRgba8(with_alpha(&photo(31, 9))),
        ] {
            let file = encoded(&img, &lossy(80.0));
            assert_eq!(check_lossy(&file, 31, 9), Ok(()));
            assert!(check_lossy(&file, 31, 10).is_err());
            let mut longer = file.clone();
            longer.push(0);
            assert!(check_lossy(&longer, 31, 9).is_err());
            // Assembled with metadata, still whole.
            let carried = Carried {
                flags: 0x08,
                exif: Some(b"MM\0*\0\0\0\x08\0\0".to_vec()),
                ..Carried::default()
            };
            let out = webp::assemble(file, 31, 9, &carried).unwrap();
            assert_eq!(check_lossy(&out, 31, 9), Ok(()));
            // VP8X's alpha flag turned over.
            let mut flipped = out.clone();
            flipped[20] ^= ALPHA_FLAG;
            assert!(check_lossy(&flipped, 31, 9).is_err());
        }
        let lossless = encoded(&DynamicImage::ImageRgb8(photo(4, 4)), &LOSSLESS);
        assert!(check_lossy(&lossless, 4, 4).is_err());
    }

    #[test]
    fn encode_reports_progress_and_stops_at_a_cancel() {
        let img = DynamicImage::ImageRgb8(photo(256, 256));
        let mut seen = Vec::new();
        encode(&img, &lossy(80.0), &CancelToken::default(), &mut |p| {
            seen.push(p)
        })
        .unwrap();
        assert!(seen.len() > 3, "{seen:?}");
        assert!(seen.iter().all(|p| (0.5..=0.9).contains(p)), "{seen:?}");
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
        // libwebp's lossless percent goes down now and then (1, 2, 34, 18,
        // 22, … for a 16,383 × 1 gradient with noise); what `progress`
        // sees doesn't.
        let mut n = 7u32;
        let line = RgbImage::from_fn(16_383, 1, |x, _| {
            n = n.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let v = (x / 64) as u8;
            Rgb([v, v.wrapping_add((n >> 16) as u8 & 15), v / 2])
        });
        let mut raw = Vec::new();
        grape_webp::encode(
            Pixels::Rgb {
                data: line.as_raw(),
                width: 16_383,
                height: 1,
            },
            &LOSSLESS,
            &mut |p| {
                raw.push(p);
                true
            },
        )
        .unwrap();
        assert!(raw.windows(2).any(|w| w[0] > w[1]), "{raw:?}");
        let mut seen = Vec::new();
        encode(
            &DynamicImage::ImageRgb8(line),
            &LOSSLESS,
            &CancelToken::default(),
            &mut |p| seen.push(p),
        )
        .unwrap();
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
        let cancel = CancelToken::default();
        cancel.cancel();
        for opts in [lossy(80.0), LOSSLESS] {
            assert_eq!(
                encode(&img, &opts, &cancel, &mut |_| {}),
                Err(grape_webp::Error::Cancelled)
            );
        }
        let gray = DynamicImage::ImageLuma8(image::GrayImage::new(4, 4));
        assert_eq!(
            encode(&gray, &LOSSLESS, &CancelToken::default(), &mut |_| {}),
            Err(grape_webp::Error::BadInput)
        );
        assert!(matches!(rgb_or_rgba(gray), DynamicImage::ImageRgb8(_)));
    }

    #[test]
    fn q80_keeps_a_photo_over_38_db() {
        let src = photo(512, 384);
        let file = encoded(&DynamicImage::ImageRgb8(src.clone()), &lossy(80.0));
        let back = image::load_from_memory(&file).unwrap().to_rgb8();
        let se: f64 = src
            .as_raw()
            .iter()
            .zip(back.as_raw())
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum();
        let mse = se / src.as_raw().len() as f64;
        let psnr = 10.0 * (255.0 * 255.0 / mse).log10();
        assert!(psnr > 38.0, "{psnr:.1} dB");
        // The alpha plane comes back exactly.
        let rgba = with_alpha(&src);
        let file = encoded(&DynamicImage::ImageRgba8(rgba.clone()), &lossy(80.0));
        let back = image::load_from_memory(&file).unwrap().to_rgba8();
        assert!(back
            .pixels()
            .zip(rgba.pixels())
            .all(|(a, b)| a.0[3] == b.0[3]));
        assert!(transparent(&DynamicImage::ImageRgba8(rgba)));
        assert!(!transparent(&DynamicImage::ImageRgb8(src)));
    }
}
