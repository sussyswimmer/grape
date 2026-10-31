//! Compress on WebP (D28): image-webp decodes the picture as stored (an
//! extended file's VP8X flags read as its chunks bear them out, as libwebp
//! reads them: [`webp::reading`]), Grape scales a lossy one down, libwebp
//! 1.6.0 encodes it again through `grape-webp`, and Grape puts the
//! source's metadata back in the file libwebp wrote ([`webp::assemble`]).
//!
//! | Source | Small | Balanced | High |
//! |---|---|---|---|
//! | Lossy (`VP8 `) | long side ≤ 2048, q70 | ≤ 4096, q80 | as it is, q85 |
//! | Lossless (`VP8L`) | lossless | lossless | lossless |
//!
//! - **Lossy:** libwebp's method 6, or 5 when the picture it encodes has
//!   transparency ([`LOSSY_METHOD`]; one its scale left opaque is encoded
//!   as opaque): libwebp encodes the alpha plane at the picture's method,
//!   losslessly, and at 6, even a simple one, that took 8 to 80 times as
//!   long as at 5, with no progress report, for files that on the
//!   WebP-decoded pixels Compress encodes came out 4.0 % larger to 22.7 %
//!   smaller at the same quality.
//!   Straight to YUV 4:2:0 (a lossy source is 4:2:0 already), alpha
//!   exact, the color under fully transparent pixels free to change; a
//!   picture with transparency is scaled by area near its transparency,
//!   its color weighed by its alpha, its alpha in steps of half what a
//!   source pixel covers of a new one (so a straight line keeps its
//!   weight), its fully transparent pixels colored as the visible ones
//!   nearest them, and by Lanczos3 elsewhere ([`scale`]). The qualities
//!   are JPEG's, so a preset means the same across formats. A picture
//!   with transparency its cap scales down is also encoded as it is, at
//!   any ratio, first, wherever High would encode it ([`fits_as_it_is`]),
//!   and the smaller file kept ([`lossy_file`]).
//!   Kept when it's at most 90 % of the source's size. "Try Small" only
//!   when Small's own file is: when Balanced or High ends `NoChange`, Small
//!   is run as a trial, its file measured and never written
//!   ([`small_would_do`]).
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
//! - **Refused:** an animation; and as for a conversion (`crate::image`
//!   walks every WebP it decodes with [`webp::reading`]), a file whose
//!   alpha libwebp and image-webp read differently (VP8X's alpha flag over
//!   a `VP8L` whose own bit says none, once its pixels prove transparent,
//!   or the `ALPH` chunks), whose transparency Compress can't keep for both, and what [`webp::scan`] finds broken. Nothing
//!   past the RIFF's end is read, by the scan or the decoder
//!   (`crate::image` ends the file there, as libwebp does).
//! - **Memory:** what the steps hold is checked against D16's limit
//!   before decoding ([`peak`]), with libwebp's share measured
//!   ([`LOSSY_BYTES`], [`LOSSY_ALPHA_BYTES`], [`LOSSLESS_BYTES`]).
//! - **Cancel:** the token is checked between steps and at each of
//!   libwebp's progress reports, which end the encode
//!   ([`grape_webp::encode`] says when libwebp reports). At the largest
//!   pictures [`peak`] admits, reports came at most this far apart (release
//!   builds, 4 vCPUs): lossy at 6500², 1 s for a real photo and 6.0 s for
//!   one under heavy grain (±32), which libwebp encodes 4 times over to fit
//!   VP8's first partition (5 times and 14.5 s before `grape-webp` set its
//!   partition limit); lossy with alpha at 3734², the most High admits,
//!   14.4 s while libwebp encoded the alpha plane of a radial gradient
//!   with Gaussian grain (σ 24; 11.6 to 14.4 s on two machines, a cancel
//!   2.5 s in returning after 12.1 to 13.8 s), 11.6 s for a photo's
//!   luminance as alpha, 8.8 s for uniform noise and 0.1 s for a
//!   sticker's; lossless at 4096², 4.2 s for a picture of many sharp shapes
//!   (3.0 s for a real photo); lossy with alpha encoded as it is before
//!   its scaled picture ([`lossy_file`]) at 13.9 MP, 19.0 and 19.7 s at
//!   Small at 12,232 × 1140 and 16,383 × 851 and 19.7 and 20.6 s at
//!   Balanced there while libwebp encoded the grained plane (a cancel 5 s
//!   in returning after 14.6 to 15.5 s). The decode before them runs to its end once
//!   started, with no report: a 9850² grained source took 6.3 s at Small,
//!   and a cancel 1 s in returned after 5.3 s.

use std::cell::Cell;
use std::fs::File;
use std::io::{BufReader, Cursor, Write};
use std::path::Path;

use grape_core::{CompressPreset, Format, Job};
use grape_webp::{Mode, Options, Pixels};
use image::{ColorType, DynamicImage, GenericImageView};

use super::exif;
use super::scale;
use super::strip::{self, Error};
use super::webp::{self, Carried, ALPHA_FLAG};
use crate::cancel::Cancellable;
use crate::image::WORKING_SET_LIMIT;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome, Note};

/// libwebp's method for a lossy picture, by whether the picture encoded
/// has transparency ([module docs](self)). At 6, libwebp encodes an alpha
/// plane with quality 100's lossless search for each of its filters, on a
/// thread that reports no progress: for a 3734² photo (the most High admits
/// with alpha; release build, 4 vCPUs), 19 to 25 s with rounded corners,
/// one pixel at 254 or a hard-edged cutout, 31 s with a sticker's soft edge
/// and 290 to 700 s with noise, grain or a photo's luminance, against 1 to
/// 14 s at 5, for files 0.5 to 5.0 % smaller there (q85, round 4): past
/// every wait D28 names, so 5 it is. What 5 costs in size depends on the
/// pixels, and Compress always encodes a WebP's decoded ones (round 6,
/// libwebp 1.6.0 through Pillow 12.3, three 3000 × 1688 photos saved at
/// q75 to q95 and decoded): encoded as they are, 5's file was −0.3 to
/// +22.7 % of 6's at q70, −2.5 to +16.1 % at q80 and −4.0 to +1.8 % at
/// q85, its PSNR 0.58 dB lower to 1.39 dB higher; scaled to 2048 px first,
/// as Small does, +2.9 to +6.7 %, within 0.15 dB. Through Compress, the
/// lake photo from Pillow's q90 with one pixel at 254 came out of Balanced
/// 12.3 % larger than opaque, and of High 1.3 % smaller.
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

/// Whether Compress at a long side of `cap` also encodes a `width`×`height`
/// picture as it is, if it has `alpha` ([`lossy_file`]): a picture with
/// transparency its cap scales down.
fn tries_unscaled(width: u32, height: u32, alpha: bool, cap: Option<u32>) -> bool {
    alpha && cap.is_some_and(|cap| width.max(height) > cap)
}

/// Whether Compress also encodes a `width`×`height` picture with alpha as
/// it is, with `metadata` bytes held beside it and an ICC profile of
/// `profile` bytes read with it ([`lossy_file`]): wherever High encodes it,
/// as High counts it ([`peak`] with no cap; the profile `crate::image`
/// read, not the largest one).
fn fits_as_it_is(width: u32, height: u32, metadata: u64, profile: u64) -> bool {
    peak(width, height, 4, false, None)
        .saturating_add(metadata)
        .saturating_add(profile)
        <= WORKING_SET_LIMIT
}

/// A lossy preset's quality and the long side it scales down to.
fn plan(preset: CompressPreset) -> (f32, Option<u32>) {
    match preset {
        CompressPreset::Small => (70.0, Some(2048)),
        CompressPreset::Balanced => (80.0, Some(4096)),
        CompressPreset::High => (85.0, None),
    }
}

/// The fewest bytes a lossy file Compress writes with `carried` in it can
/// take: `RIFF` and `WEBP`, a `VP8X` when anything is carried, each chunk
/// carried, and a `VP8 ` chunk of at least a key frame's 10 bytes.
fn least_lossy_file(carried: &Carried) -> u64 {
    let chunk = |len: u64| 8 + len + (len & 1);
    let parts: u64 = [&carried.iccp, &carried.exif, &carried.xmp]
        .into_iter()
        .flatten()
        .map(|c| chunk(c.len() as u64))
        .sum();
    let vp8x = if parts > 0 { chunk(10) } else { 0 };
    12 + vp8x + parts + chunk(10)
}

/// Whether Small would write at most `most` bytes of the lossy WebP `src`
/// (D26's "Try Small"): Small's own decode, scale, encode and file, with
/// the metadata it keeps, `carried`, run as a trial whose file is only
/// measured, never written. Skipped when no file can be that small
/// ([`least_lossy_file`]). It holds what Small would, under the same peak
/// check; a file Small couldn't write (too large, a failed encode) is no
/// reason to try it. Its progress goes to 0.9–0.98, and a cancel stops it
/// as it stops Small, its last report too.
#[allow(clippy::too_many_arguments)] // `run`'s, plus the trial's own.
fn small_would_do(
    src: &Path,
    from: Format,
    names: &Names,
    carried: &mut Carried,
    most: u64,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(f32),
) -> Result<bool, GrapeError> {
    if least_lossy_file(carried) > most {
        return Ok(false);
    }
    progress(0.9);
    let decoded = decode(
        src,
        from,
        names,
        cancel,
        false,
        CompressPreset::Small,
        carried,
    );
    let (img, unscaled) = match decoded {
        Ok(decoded) => decoded,
        Err(_) if cancel.is_cancelled() => return Err(crate::cancelled(src)),
        Err(_) => return Ok(false),
    };
    progress(0.92);
    match lossy_file(
        (img, unscaled),
        CompressPreset::Small,
        carried,
        (src, names, cancel),
        progress,
        (0.93, 0.98),
    ) {
        // A cancel during the last report is a cancel, as for Small.
        _ if cancel.is_cancelled() => Err(crate::cancelled(src)),
        Ok((file, _, _)) => Ok(file.len() as u64 <= most),
        Err(_) => Ok(false),
    }
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
    progress(0.1);

    let lossless = scan.lossless;
    let mut carried = scan.carried;
    let (img, unscaled) = decode(src, from, &names, cancel, lossless, preset, &carried)?;
    progress(0.4);
    let (out, img, width, height) = if lossless {
        progress(0.5);
        let (width, height) = img.dimensions();
        let opts = Options {
            mode: Mode::Lossless {
                level: LOSSLESS_LEVEL,
            },
            // A second thread gains nothing at this level but a second
            // encoder's buffers.
            threads: false,
        };
        let encoded = encode(&img, &opts, cancel, progress, (0.5, 0.9))
            .map_err(|e| encode_error(e, src, &names, cancel))?;
        progress(0.9);
        let out = assembled(encoded, width, height, &mut carried, src, &names)?;
        (out, Some(img), width, height)
    } else {
        let ctx = (src, &names, cancel);
        let (out, width, height) = lossy_file(
            (img, unscaled),
            preset,
            &mut carried,
            ctx,
            progress,
            (0.5, 0.9),
        )?;
        (out, None, width, height)
    };
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
        // Only its size counts: what Small would write is measured with
        // nothing else held.
        drop((out, img));
        let try_small = !lossless
            && preset != CompressPreset::Small
            && small_would_do(src, from, &names, &mut carried, most, cancel, progress)?;
        let detail = format!("{after} bytes from {before} in {}", src.display());
        return Err(names.not_smaller(try_small, detail));
    }
    drop(carried);
    match &img {
        Some(img) => check_lossless(&out, img),
        None => check_lossy(&out, width, height),
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

/// The picture `src` holds, decoded as stored (8-bit RGB or RGBA) once
/// what Compress at `preset` holds for it, its metadata `carried` among
/// it, is checked against D16's limit ([`peak`]). Read as libwebp reads
/// it, or refused as conversions refuse it (`webp::reading`). With it,
/// whether a lossy one with alpha its cap scales down fits D16's limit
/// encoded as it is too, counted with the rest and the profile it was read
/// with ([`fits_as_it_is`]), which [`lossy_file`] then holds to: one that
/// doesn't is only scaled, as before, and never refused for it.
fn decode(
    src: &Path,
    from: Format,
    names: &Names,
    cancel: &CancelToken,
    lossless: bool,
    preset: CompressPreset,
    carried: &Carried,
) -> Result<(DynamicImage, bool), GrapeError> {
    let (_, cap) = plan(preset);
    // The metadata is held throughout, and again in the file written.
    let metadata = carried.len().saturating_mul(2);
    let tries = Cell::new(None);
    let tool_peak = |width: u32, height: u32, color: ColorType| {
        let bpp = u64::from(color.bytes_per_pixel());
        if !lossless && bpp == 4 && tries_unscaled(width, height, color.has_alpha(), cap) {
            tries.set(Some((width, height)));
        }
        peak(width, height, bpp, lossless, cap).saturating_add(metadata)
    };
    let (img, icc) =
        crate::image::decode_stored(src, from, Format::Webp, names, cancel, &tool_peak)
            .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    // The profile `crate::image` counted with High's peak, as it counts it.
    let profile = icc.map_or(0, |icc| icc.len() as u64);
    let as_it_is = tries
        .get()
        .is_some_and(|(w, h)| fits_as_it_is(w, h, metadata, profile));
    Ok((rgb_or_rgba(img), as_it_is))
}

/// What the lossy `preset` writes of the decoded picture `img`: scaled to
/// its cap ([`scale`]), encoded at its quality by the method the scaled
/// picture calls for ([`LOSSY_METHOD`]), libwebp's progress going to
/// `span` of `progress`, and put in a file with `carried` ([`assembled`]).
/// A picture with transparency the cap scales down is encoded as it is
/// first, when `as_it_is` says that fits D16's limit ([`decode`],
/// [`fits_as_it_is`]), and the smaller of the two files kept, the scaled
/// one on a tie: the two encodes share `span`, the one as it is the first
/// half. The scaled encode is made beside that file when that fits
/// ([`fits_scaled_beside`]), or else that file goes. Gives the file and
/// the picture's size.
fn lossy_file(
    (img, as_it_is): (DynamicImage, bool),
    preset: CompressPreset,
    carried: &mut Carried,
    ctx: (&Path, &Names, &CancelToken),
    progress: &mut dyn FnMut(f32),
    span: (f32, f32),
) -> Result<(Vec<u8>, u32, u32), GrapeError> {
    let (src, names, _) = ctx;
    let (quality, cap) = plan(preset);
    let (from_w, from_h) = img.dimensions();
    let transparent = matches!(&img, DynamicImage::ImageRgba8(rgba) if scale::transparent(rgba));
    let (encoded, width, height) = if as_it_is && tries_unscaled(from_w, from_h, transparent, cap) {
        let half = (span.0 + span.1) / 2.0;
        progress(span.0);
        let first = encode_lossy(&img, quality, ctx, progress, (span.0, half))?;
        progress(half);
        let metadata = carried.len().saturating_mul(2);
        if fits_scaled_beside(from_w, from_h, cap, metadata, first.len() as u64) {
            let scaled = scaled_file(img, quality, cap, ctx, progress, (half, span.1))?;
            if first.len() < scaled.0.len() {
                (first, from_w, from_h)
            } else {
                scaled
            }
        } else {
            drop(first);
            scaled_file(img, quality, cap, ctx, progress, (half, span.1))?
        }
    } else {
        scaled_file(img, quality, cap, ctx, progress, span)?
    };
    let out = assembled(encoded, width, height, carried, src, names)?;
    Ok((out, width, height))
}

/// `img` scaled to fit `cap` ([`scale`]) and encoded at `quality`
/// ([`encode_lossy`]), libwebp's progress going to `span` of `progress`:
/// libwebp's file and the scaled picture's size.
fn scaled_file(
    img: DynamicImage,
    quality: f32,
    cap: Option<u32>,
    (src, names, cancel): (&Path, &Names, &CancelToken),
    progress: &mut dyn FnMut(f32),
    span: (f32, f32),
) -> Result<(Vec<u8>, u32, u32), GrapeError> {
    let scaled = scale::scale(img, cap, cancel).ok_or_else(|| crate::cancelled(src))?;
    let scaled = opaque_as_rgb(scaled);
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    progress(span.0);
    let (width, height) = scaled.dimensions();
    let encoded = encode_lossy(&scaled, quality, (src, names, cancel), progress, span)?;
    progress(span.1);
    Ok((encoded, width, height))
}

/// Whether the scale of a `width`×`height` picture with alpha to `cap` and
/// that encode ([`peak`]) fit D16's limit beside `metadata` bytes and its
/// file as it is, `first` bytes ([`lossy_file`]).
fn fits_scaled_beside(
    width: u32,
    height: u32,
    cap: Option<u32>,
    metadata: u64,
    first: u64,
) -> bool {
    peak(width, height, 4, false, cap)
        .saturating_add(metadata)
        .saturating_add(first)
        <= WORKING_SET_LIMIT
}

/// `img` (8-bit RGB or RGBA) encoded lossily at `quality`, by the method
/// the picture calls for ([`LOSSY_METHOD`]: one with no alpha plane to
/// encode, an opaque one, at 6), libwebp's progress going to `span` of
/// `progress`.
fn encode_lossy(
    img: &DynamicImage,
    quality: f32,
    (src, names, cancel): (&Path, &Names, &CancelToken),
    progress: &mut dyn FnMut(f32),
    span: (f32, f32),
) -> Result<Vec<u8>, GrapeError> {
    let method = if img.color().has_alpha() {
        LOSSY_METHOD.1
    } else {
        LOSSY_METHOD.0
    };
    let opts = Options {
        mode: Mode::Lossy { quality, method },
        // A thread more for the alpha plane.
        threads: true,
    };
    encode(img, &opts, cancel, progress, span).map_err(|e| encode_error(e, src, names, cancel))
}

/// libwebp's `encoded` file of a `width`×`height` picture with `carried`
/// put in it ([`webp::assemble`]), the EXIF's PixelXDimension and
/// PixelYDimension set to that size first.
fn assembled(
    encoded: Vec<u8>,
    width: u32,
    height: u32,
    carried: &mut Carried,
    src: &Path,
    names: &Names,
) -> Result<Vec<u8>, GrapeError> {
    if let Some(exif) = carried.exif.as_mut() {
        let at = if exif.starts_with(b"Exif\0\0") { 6 } else { 0 };
        exif::patch_dimensions(&mut exif[at..], width, height);
    }
    webp::assemble(encoded, width, height, carried).map_err(|e| {
        let why = match e {
            Error::Broken(why) | Error::TooLarge(why) | Error::Refused(why) => why,
            Error::Read(e) | Error::Write(e) => e.to_string(),
        };
        names.encode_failed(format!("assemble {}: {why}", src.display()))
    })
}

/// What a failed encode of `src` tells the user.
fn encode_error(
    e: grape_webp::Error,
    src: &Path,
    names: &Names,
    cancel: &CancelToken,
) -> GrapeError {
    let detail = format!("encode {}: {e}", src.display());
    let e = match e {
        grape_webp::Error::Cancelled => crate::cancelled(src),
        grape_webp::Error::OutOfMemory | grape_webp::Error::TooLarge => names.too_large(detail),
        _ => names.encode_failed(detail),
    };
    publish::or_cancelled(cancel, src, e)
}

/// `img` as 8-bit RGB or RGBA, what image-webp decodes to.
fn rgb_or_rgba(img: DynamicImage) -> DynamicImage {
    match img {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) => img,
        other if other.color().has_alpha() => DynamicImage::ImageRgba8(other.to_rgba8()),
        other => DynamicImage::ImageRgb8(other.to_rgb8()),
    }
}

/// `img` (8-bit RGB or RGBA) as RGB when every pixel is opaque: libwebp
/// then writes no alpha plane, and Compress encodes it at the opaque
/// method ([`LOSSY_METHOD`]).
fn opaque_as_rgb(img: DynamicImage) -> DynamicImage {
    match img {
        DynamicImage::ImageRgba8(rgba) if rgba.pixels().all(|p| p.0[3] == u8::MAX) => {
            DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(rgba).to_rgb8())
        }
        other => other,
    }
}

/// `img` (8-bit RGB or RGBA) encoded with `opts`; libwebp's progress goes
/// to `span` of `progress`, never back (a lossless encode's percent can go
/// down), and a cancel ends the encode at its next report.
fn encode(
    img: &DynamicImage,
    opts: &Options,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(f32),
    (from, to): (f32, f32),
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
        progress(from + (to - from) * f32::from(most) / 100.0);
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
    use crate::image::{MAX_ICC, WORKING_SET_LIMIT};
    use crate::imagetool::webp::Carried;

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
        encode(img, opts, &CancelToken::default(), &mut |_| {}, (0.5, 0.9)).unwrap()
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
    fn a_picture_with_alpha_over_the_cap_is_also_tried_as_it_is() {
        // Over the cap, at any ratio, with alpha: tried as it is too.
        for (w, h, cap) in [
            (2049, 10, 2048),
            (10, 4095, 2048),
            (4096, 10, 2048),
            (12_000, 1_140, 2048),
            (8191, 1675, 4096),
            (16_383, 836, 4096),
        ] {
            assert!(tries_unscaled(w, h, true, Some(cap)), "{w}x{h} {cap}");
        }
        // Not over the cap, no cap, or opaque.
        for (w, h, alpha, cap) in [
            (2048, 2048, true, Some(2048)),
            (3000, 10, true, None),
            (3000, 10, false, Some(2048)),
        ] {
            assert!(!tries_unscaled(w, h, alpha, cap), "{w}x{h} {alpha} {cap:?}");
        }
    }

    #[test]
    fn a_picture_is_encoded_as_it_is_too_wherever_high_encodes_it() {
        // High's own check: the picture and libwebp's share, the metadata
        // and the profile `crate::image` read ([`decode`]).
        let high = |w: u32, h: u32, metadata: u64, profile: u64| {
            peak(w, h, 4, false, None) + metadata + profile <= WORKING_SET_LIMIT
        };
        // The widest picture High admits at each height, and the next.
        let widest = |h: u32, metadata: u64, profile: u64| {
            let (mut lo, mut hi) = (1u32, 1 << 20);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if high(mid, h, metadata, profile) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            lo
        };
        // No profile: up to 2³⁰ ÷ 77 = 13,944,699 px (4125 × 3380), where
        // the largest profile's room stopped it at 13,727,720 (4061 ×
        // 3380) whatever the picture had; a 3 KB profile, the largest one
        // and metadata take theirs.
        for (h, metadata, profile, want) in [
            (3380, 0, 0, Some(4125)),
            (3225, 0, 0, Some(4323)),
            (2775, 0, 0, Some(5025)),
            (1690, 0, 0, Some(8251)),
            (1140, 0, 0, Some(12_232)),
            (836, 0, 0, Some(16_680)),
            (3390, 0, 0, Some(4113)),
            (1690, 0, 3144, Some(8251)),
            (3380, 0, MAX_ICC as u64, Some(4061)),
            (2775, 2 * 65_536, 3144, None),
            (1140, 2 * 1_000_000, MAX_ICC as u64, None),
        ] {
            let w = widest(h, metadata, profile);
            if let Some(want) = want {
                assert_eq!(w, want, "{h} {profile}");
            }
            assert!(fits_as_it_is(w, h, metadata, profile), "{w}x{h}");
            assert!(!fits_as_it_is(w + 1, h, metadata, profile), "{w}x{h}");
        }
        // Then the scaled picture's encode beside that file: at Balanced
        // a hair over its cap (4113 × 3390, 1.004 to 1), with up to about
        // 9 MB of it.
        let (w, h) = (4113, 3390);
        let room = WORKING_SET_LIMIT - peak(w, h, 4, false, Some(4096));
        assert_eq!(room, 8_978_432);
        assert!(fits_scaled_beside(w, h, Some(4096), 0, room));
        assert!(!fits_scaled_beside(w, h, Some(4096), 0, room + 1));
        assert!(!fits_scaled_beside(w, h, Some(4096), 1, room));
        // Small's scale of it, far less.
        assert!(fits_scaled_beside(w, h, Some(2048), 0, 500_000_000));
    }

    #[test]
    fn small_is_tried_unless_no_file_could_be_small_enough() {
        // RIFF, WEBP and a 10-byte VP8 frame: 30 bytes.
        assert_eq!(least_lossy_file(&Carried::default()), 30);
        // With metadata, a VP8X, each chunk padded.
        let carried = Carried {
            flags: 0x0C,
            iccp: None,
            exif: Some(vec![0; 101]),
            xmp: Some(vec![0; 4]),
        };
        assert_eq!(least_lossy_file(&carried), 12 + 18 + 110 + 12 + 18);
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
        let span = (0.93, 0.98);
        encode(
            &img,
            &lossy(80.0),
            &CancelToken::default(),
            &mut |p| seen.push(p),
            span,
        )
        .unwrap();
        assert!(seen.len() > 3, "{seen:?}");
        assert!(seen.iter().all(|p| (0.93..=0.98).contains(p)), "{seen:?}");
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
            (0.5, 0.9),
        )
        .unwrap();
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
        let cancel = CancelToken::default();
        cancel.cancel();
        for opts in [lossy(80.0), LOSSLESS] {
            assert_eq!(
                encode(&img, &opts, &cancel, &mut |_| {}, (0.5, 0.9)),
                Err(grape_webp::Error::Cancelled)
            );
        }
        let gray = DynamicImage::ImageLuma8(image::GrayImage::new(4, 4));
        assert_eq!(
            encode(
                &gray,
                &LOSSLESS,
                &CancelToken::default(),
                &mut |_| {},
                (0.5, 0.9)
            ),
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
        // Opaque RGBA goes as RGB; anything see-through stays RGBA.
        let opaque = RgbaImage::from_pixel(2, 2, Rgba([9, 9, 9, 255]));
        let rgb = opaque_as_rgb(DynamicImage::ImageRgba8(opaque));
        assert_eq!(rgb.as_rgb8().unwrap().as_raw(), &[9; 12]);
        assert!(opaque_as_rgb(DynamicImage::ImageRgba8(rgba))
            .as_rgba8()
            .is_some());
    }
}
