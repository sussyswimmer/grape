//! The Mac worker's AVIF encoder: rav1e through ravif (DECISIONS D41).
//!
//! `macos::heif::ImageIo` calls [`encode`] for protocol 2's AVIF encodes,
//! because ImageIO can't write AVIF on macOS 13 to 15 and nobody has shown
//! it can on every Mac 26. Only the worker process runs it (`Grape
//! --grape-heif-worker`): an AV1 frame encode is one blocking call with no
//! cancel, so Grape cancels by killing the worker, and a panic or a runaway
//! allocation ends that one job.
//!
//! The module is safe Rust and OS-neutral, so it's tested on every OS with
//! the `avif-encode` feature; the app turns that on for macOS builds only.
//!
//! - **Settings:** ravif's quality 80 unless the request names one (1 to
//!   100, alpha the same), speed 6 (ravif's own default of 4 took three
//!   times as long for files 3% smaller), 8-bit, at most 8 threads.
//!   ravif always writes 4:4:4 and a single picture, never a grid.
//! - **Color:** ravif writes BT.709 primaries with the sRGB transfer (nclx)
//!   and no ICC profile. So pixels that carry an RGB ICC profile are first
//!   converted to sRGB with moxcms, alpha untouched: a Display P3 photo
//!   comes out right in every viewer, its colors outside sRGB clipped. A
//!   profile moxcms can't read, or one that isn't RGB, is taken as sRGB.
//! - **Never written:** EXIF, XMP or any other metadata.
//!
//! Nothing logs, and no failure detail holds a path.

use std::num::NonZeroUsize;
use std::sync::Arc;

use grape_heif_proto::{FailKind, Failure, Pixels};
use moxcms::{ColorProfile, DataColorSpace, Layout, Transform8BitExecutor, TransformOptions};
use ravif::{BitDepth, Encoder, Img, RGB8, RGBA8};

/// ravif's quality when the request has none: cavif's own default, which
/// matched HEIC's 0.8 by eye and SSIM in D41's measurements.
pub const DEFAULT_QUALITY: u8 = 80;

/// rav1e's speed preset, 1 (slowest) to 10.
const SPEED: u8 = 6;

/// The most threads one encode uses. More gave little at 12 MP and would
/// only crowd the user's other work.
const MAX_THREADS: usize = 8;

/// The bytes of an AVIF file for `pixels` (3 or 4 channels, straight
/// alpha), at `quality` (1..=100; `None` is [`DEFAULT_QUALITY`]).
///
/// Every value ravif would assert on is checked here first, so a bad
/// request is a failure, never a panic.
pub fn encode(pixels: &Pixels, quality: Option<u8>) -> Result<Vec<u8>, Failure> {
    let quality = quality.unwrap_or(DEFAULT_QUALITY);
    if !(1..=100).contains(&quality) {
        return Err(fail(
            FailKind::Internal,
            format!("quality {quality} isn't in 1..=100"),
        ));
    }
    let (width, height) = size(pixels)?;
    let encoder = Encoder::new()
        .with_quality(f32::from(quality))
        .with_alpha_quality(f32::from(quality))
        .with_speed(SPEED)
        .with_bit_depth(BitDepth::Eight)
        .with_num_threads(Some(threads()));
    let encoded = match pixels.channels {
        3 => {
            let rgb = to_srgb::<3, RGB8>(pixels, |[r, g, b]| RGB8::new(r, g, b))?;
            encoder.encode_rgb(Img::new(rgb.as_slice(), width, height))
        }
        4 => {
            let rgba = to_srgb::<4, RGBA8>(pixels, |[r, g, b, a]| RGBA8::new(r, g, b, a))?;
            // ravif leaves the alpha plane out when every pixel is opaque.
            encoder.encode_rgba(Img::new(rgba.as_slice(), width, height))
        }
        channels => return Err(fail(FailKind::Internal, format!("{channels} channels"))),
    }
    .map_err(|err| fail(FailKind::Encode, format!("ravif: {err}")))?;
    Ok(encoded.avif_file)
}

/// The threads one encode may use: the machine's, at most [`MAX_THREADS`].
fn threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .clamp(1, MAX_THREADS)
}

/// The picture's width and height, once its bytes are known to match them
/// (`height` rows of `width × channels`), neither of them 0.
fn size(pixels: &Pixels) -> Result<(usize, usize), Failure> {
    let too_large = || fail(FailKind::TooLarge, "the image doesn't fit in memory");
    let width = usize::try_from(pixels.width).map_err(|_| too_large())?;
    let height = usize::try_from(pixels.height).map_err(|_| too_large())?;
    if width == 0 || height == 0 {
        return Err(fail(FailKind::Internal, "an empty picture"));
    }
    let len = width
        .checked_mul(height)
        .and_then(|area| area.checked_mul(usize::from(pixels.channels)))
        .ok_or_else(too_large)?;
    if len != pixels.data.len() {
        return Err(fail(
            FailKind::Internal,
            "the pixels don't match their size",
        ));
    }
    Ok((width, height))
}

/// The pixels as ravif's pixel type, converted to sRGB first when they
/// carry an RGB ICC profile that moxcms reads ([`srgb_transform`]); as they
/// are otherwise. One copy, made a row at a time.
fn to_srgb<const N: usize, P>(pixels: &Pixels, pixel: fn([u8; N]) -> P) -> Result<Vec<P>, Failure> {
    let layout = if N == 4 { Layout::Rgba } else { Layout::Rgb };
    let transform = pixels
        .icc
        .as_deref()
        .and_then(|icc| srgb_transform(icc, layout));
    let mut out: Vec<P> = Vec::new();
    out.try_reserve_exact(pixels.data.len() / N)
        .map_err(|_| fail(FailKind::TooLarge, "the image doesn't fit in memory"))?;
    let stride = usize::try_from(pixels.width)
        .ok()
        .and_then(|width| width.checked_mul(N))
        .filter(|&stride| stride > 0)
        .ok_or_else(|| fail(FailKind::Internal, "an empty row"))?;
    let mut converted = Vec::new();
    for row in pixels.data.chunks_exact(stride) {
        let row = match &transform {
            Some(transform) => {
                converted.clear();
                converted.resize(row.len(), 0);
                transform
                    .transform(row, &mut converted)
                    .map_err(|err| fail(FailKind::Encode, format!("moxcms: {err}")))?;
                converted.as_slice()
            }
            None => row,
        };
        out.extend(row.as_chunks::<N>().0.iter().map(|&px| pixel(px)));
    }
    Ok(out)
}

/// An 8-bit transform from `icc` to sRGB in `layout` (alpha copied as it
/// is), or `None` when the pixels should be taken as sRGB already: moxcms
/// can't read the profile, it isn't an RGB one, or moxcms can't connect it.
fn srgb_transform(icc: &[u8], layout: Layout) -> Option<Arc<Transform8BitExecutor>> {
    let profile = ColorProfile::new_from_slice(icc).ok()?;
    if profile.color_space != DataColorSpace::Rgb {
        return None;
    }
    profile
        .create_transform_8bit(
            layout,
            &ColorProfile::new_srgb(),
            layout,
            TransformOptions::default(),
        )
        .ok()
}

/// A failure of `kind` with a detail for the log.
fn fail(kind: FailKind, detail: impl Into<String>) -> Failure {
    Failure::new(kind, detail)
}

#[cfg(test)]
mod tests {
    use grape_heif_proto::{sniff, Sniff, SNIFF_BYTES};

    use super::*;

    /// A smooth gradient, opaque, or with alpha falling to the right.
    fn gradient(width: u32, height: u32, channels: u8) -> Pixels {
        let mut data = Vec::new();
        for y in 0..height {
            for x in 0..width {
                data.push((x * 255 / width.max(2).saturating_sub(1)) as u8);
                data.push((y * 255 / height.max(2).saturating_sub(1)) as u8);
                data.push(128);
                if channels == 4 {
                    data.push(255 - (x * 200 / width.max(1)) as u8);
                }
            }
        }
        Pixels {
            width,
            height,
            channels,
            data,
            icc: None,
        }
    }

    /// Every box of `kind` in the file, as the bytes after its 8-byte
    /// header, found by walking the boxes that hold other boxes.
    fn boxes<'a>(mut data: &'a [u8], kind: &[u8; 4], found: &mut Vec<&'a [u8]>) {
        // `meta` and `iinf` carry 4 bytes of version and flags (and
        // `iinf` a count) before their children; `iprp` and `ipco` don't.
        const PARENTS: [(&[u8; 4], usize); 4] =
            [(b"meta", 4), (b"iprp", 0), (b"ipco", 0), (b"iinf", 6)];
        while data.len() >= 8 {
            let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
            assert!(len >= 8 && len <= data.len(), "a box of {len} bytes");
            let (this, body) = (&data[4..8], &data[8..len]);
            if this == kind {
                found.push(body);
            }
            if let Some((_, skip)) = PARENTS.iter().find(|(parent, _)| this == *parent) {
                boxes(&body[*skip..], kind, found);
            }
            data = &data[len..];
        }
    }

    fn find<'a>(file: &'a [u8], kind: &[u8; 4]) -> Vec<&'a [u8]> {
        let mut found = Vec::new();
        boxes(file, kind, &mut found);
        found
    }

    /// The checks every encode passes: an `avif` ftyp that Grape's sniff
    /// takes for AVIF, and every picture's `ispe` at the source's size.
    fn check(file: &[u8], width: u32, height: u32) {
        assert_eq!(&file[4..8], b"ftyp");
        assert_eq!(&file[8..12], b"avif", "the major brand");
        assert_eq!(sniff(&file[..file.len().min(SNIFF_BYTES)]), Sniff::Avif);
        let sizes = find(file, b"ispe");
        assert!(!sizes.is_empty(), "an ispe");
        for ispe in sizes {
            // Version and flags, then the width and the height.
            let w = u32::from_be_bytes([ispe[4], ispe[5], ispe[6], ispe[7]]);
            let h = u32::from_be_bytes([ispe[8], ispe[9], ispe[10], ispe[11]]);
            assert_eq!((w, h), (width, height));
        }
    }

    /// True when the file has an alpha plane: an `auxC` naming alpha.
    fn has_alpha(file: &[u8]) -> bool {
        const ALPHA: &[u8] = b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";
        find(file, b"auxC")
            .iter()
            .any(|aux| aux.windows(ALPHA.len()).any(|name| name == ALPHA))
    }

    #[test]
    fn rgb_of_any_size_encodes_to_an_avif_without_alpha() {
        for (width, height) in [(1, 1), (3, 5), (64, 48)] {
            let file = encode(&gradient(width, height, 3), None).expect("an AVIF");
            check(&file, width, height);
            assert!(!has_alpha(&file), "{width}x{height}");
        }
    }

    #[test]
    fn rgba_keeps_its_alpha_plane() {
        let file = encode(&gradient(64, 48, 4), Some(70)).expect("an AVIF");
        check(&file, 64, 48);
        assert!(has_alpha(&file));
    }

    #[test]
    fn rgba_that_is_all_opaque_has_no_alpha_plane() {
        let mut pixels = gradient(16, 8, 4);
        for pixel in pixels.data.as_chunks_mut::<4>().0 {
            pixel[3] = u8::MAX;
        }
        let file = encode(&pixels, None).expect("an AVIF");
        check(&file, 16, 8);
        assert!(!has_alpha(&file));
    }

    #[test]
    fn a_quality_out_of_range_is_refused_without_a_panic() {
        let pixels = gradient(4, 4, 3);
        for quality in [0, 101, u8::MAX] {
            let failure = encode(&pixels, Some(quality)).expect_err("refused");
            assert_eq!(failure.kind, FailKind::Internal);
            assert_eq!(
                failure.detail,
                format!("quality {quality} isn't in 1..=100")
            );
        }
        for quality in [1, 100] {
            check(&encode(&pixels, Some(quality)).expect("an AVIF"), 4, 4);
        }
    }

    #[test]
    fn pixels_that_dont_match_their_size_are_refused() {
        let mut pixels = gradient(4, 4, 3);
        pixels.data.pop();
        let failure = encode(&pixels, None).expect_err("refused");
        assert_eq!(failure.kind, FailKind::Internal);
        assert_eq!(failure.detail, "the pixels don't match their size");

        let mut pixels = gradient(4, 4, 3);
        pixels.channels = 2;
        pixels.data.truncate(4 * 4 * 2);
        assert_eq!(
            encode(&pixels, None).expect_err("refused").detail,
            "2 channels"
        );

        let empty = Pixels {
            width: 0,
            height: 4,
            channels: 3,
            data: Vec::new(),
            icc: None,
        };
        assert_eq!(
            encode(&empty, None).expect_err("refused").detail,
            "an empty picture"
        );
    }

    #[test]
    fn display_p3_is_converted_to_srgb_and_gray_stays_gray() {
        let p3 = ColorProfile::new_display_p3()
            .encode()
            .expect("a Display P3 profile");
        // sRGB's red, written in Display P3, then a mid gray.
        let pixels = Pixels {
            width: 2,
            height: 1,
            channels: 3,
            data: vec![234, 51, 35, 128, 128, 128],
            icc: Some(p3.clone()),
        };
        let rgb = to_srgb::<3, [u8; 3]>(&pixels, |px| px).expect("converted");
        let near =
            |got: [u8; 3], want: [u8; 3]| got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 2);
        assert!(near(rgb[0], [255, 0, 0]), "{:?}", rgb[0]);
        assert!(near(rgb[1], [128, 128, 128]), "{:?}", rgb[1]);
        assert!(rgb[1][0].abs_diff(rgb[1][1]) <= 1 && rgb[1][1].abs_diff(rgb[1][2]) <= 1);

        // The same with alpha, which is left as it is.
        let pixels = Pixels {
            width: 2,
            height: 1,
            channels: 4,
            data: vec![234, 51, 35, 77, 128, 128, 128, 0],
            icc: Some(p3),
        };
        let rgba = to_srgb::<4, [u8; 4]>(&pixels, |px| px).expect("converted");
        assert!(
            near([rgba[0][0], rgba[0][1], rgba[0][2]], [255, 0, 0]),
            "{:?}",
            rgba[0]
        );
        assert_eq!((rgba[0][3], rgba[1][3]), (77, 0));
    }

    #[test]
    fn an_srgb_profile_changes_next_to_nothing() {
        let srgb = ColorProfile::new_srgb().encode().expect("an sRGB profile");
        let mut pixels = gradient(16, 4, 3);
        let before = pixels.data.clone();
        pixels.icc = Some(srgb);
        let rgb = to_srgb::<3, [u8; 3]>(&pixels, |px| px).expect("converted");
        for (got, want) in rgb.as_flattened().iter().zip(&before) {
            assert!(got.abs_diff(*want) <= 1, "{got} for {want}");
        }
    }

    #[test]
    fn an_unreadable_profile_is_taken_as_srgb() {
        let mut pixels = gradient(8, 4, 4);
        let before = pixels.data.clone();
        for icc in [b"not an ICC profile".to_vec(), vec![0; 132], Vec::new()] {
            pixels.icc = Some(icc);
            let rgba = to_srgb::<4, [u8; 4]>(&pixels, |px| px).expect("as they are");
            assert_eq!(rgba.as_flattened(), before.as_slice());
            check(&encode(&pixels, None).expect("an AVIF"), 8, 4);
        }
    }

    #[test]
    fn a_gray_profile_is_taken_as_srgb() {
        let gray = ColorProfile::new_gray_with_gamma(2.2)
            .encode()
            .expect("a gray profile");
        let mut pixels = gradient(8, 2, 3);
        let before = pixels.data.clone();
        pixels.icc = Some(gray);
        let rgb = to_srgb::<3, [u8; 3]>(&pixels, |px| px).expect("as they are");
        assert_eq!(rgb.as_flattened(), before.as_slice());
    }

    #[test]
    fn threads_are_between_one_and_eight() {
        assert!((1..=MAX_THREADS).contains(&threads()));
    }
}
