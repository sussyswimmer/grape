//! Scaling a picture down for Compress, and what that holds at its peak.
//!
//! A picture over twice the long side is first averaged down by a whole
//! factor (Grape's own loop, which a cancel stops at the next row), then
//! Lanczos3 takes it the rest of the way, so that step holds and takes no
//! more than for a picture of twice the long side. [`peak`] says what the
//! steps hold together, so a tool can check it against D16's limit before
//! decoding. A picture with transparency is scaled premultiplied, as both
//! steps assume, in place, so the color under its transparent pixels
//! doesn't bleed into their neighbors.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, GrayImage, RgbImage, RgbaImage};

use crate::image::CANCEL_CHECK_ROWS;
use crate::CancelToken;

/// The whole factor a picture with a `long` side is averaged down by
/// before Lanczos3 takes it to `cap`: none (1) unless it's over twice
/// `cap`, and never below `cap`.
pub(super) fn shrink_factor(long: u64, cap: u64) -> u64 {
    if long > 2 * cap {
        long.div_ceil(2 * cap)
    } else {
        1
    }
}

/// `w`×`h` fitted inside `cap`×`cap`, about as `resize` rounds it.
pub(super) fn fit(w: u64, h: u64, cap: u64) -> (u64, u64) {
    let long = w.max(h).max(1);
    (
        (w * cap).div_ceil(long).max(1),
        (h * cap).div_ceil(long).max(1),
    )
}

/// What a tool holds at its peak for a `width`×`height` decoded picture of
/// `bpp` bytes a pixel scaled to fit `cap` (none: not scaled), its steps
/// one after another:
/// - the decoded picture and its averaged copy;
/// - the picture Lanczos3 starts from, its intermediate (`image` keeps one
///   of 16 bytes a pixel, the source's width by the result's height) and
///   the result;
/// - `encode(w, h, bpp)`: what encoding the `w`×`h` result holds, itself
///   included.
pub(super) fn peak(
    width: u32,
    height: u32,
    bpp: u64,
    cap: Option<u32>,
    encode: &dyn Fn(u64, u64, u64) -> u64,
) -> u64 {
    let (w, h) = (u64::from(width), u64::from(height));
    let size = |w: u64, h: u64| w.saturating_mul(h).saturating_mul(bpp);
    let long = w.max(h);
    let Some(cap) = cap.map(u64::from).filter(|&cap| long > cap) else {
        return encode(w, h, bpp);
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
        encode(nw, nh, bpp),
    ]
    .into_iter()
    .max()
    .unwrap_or(u64::MAX)
}

/// `img` fitted inside `cap`×`cap` ([module docs](self)), or as it is when
/// it already fits or `cap` is `None`; `None` if cancelled.
pub(super) fn scale(
    img: DynamicImage,
    cap: Option<u32>,
    cancel: &CancelToken,
) -> Option<DynamicImage> {
    let long = img.width().max(img.height());
    let Some(cap) = cap.filter(|&cap| long > cap) else {
        return Some(img);
    };
    let (img, straight) = match img {
        DynamicImage::ImageRgba8(mut rgba) if transparent(&rgba) => {
            alpha(&mut rgba, premultiply, cancel)?;
            (DynamicImage::ImageRgba8(rgba), true)
        }
        other => (other, false),
    };
    let k = u32::try_from(shrink_factor(u64::from(long), u64::from(cap))).unwrap_or(u32::MAX);
    let img = if k > 1 { shrink(img, k, cancel)? } else { img };
    if cancel.is_cancelled() {
        return None;
    }
    match img.resize(cap, cap, FilterType::Lanczos3) {
        DynamicImage::ImageRgba8(mut rgba) if straight => {
            alpha(&mut rgba, unpremultiply, cancel)?;
            Some(DynamicImage::ImageRgba8(rgba))
        }
        other => Some(other),
    }
}

/// Whether any pixel of `img` isn't opaque.
fn transparent(img: &RgbaImage) -> bool {
    img.as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[3] != u8::MAX)
}

/// Runs `f` on every pixel of `img`, in place; `None` if cancelled, which
/// is checked every [`CANCEL_CHECK_ROWS`] rows.
fn alpha(img: &mut RgbaImage, f: fn(&mut [u8; 4]), cancel: &CancelToken) -> Option<()> {
    let row = img.width() as usize * 4;
    for (y, line) in img.chunks_mut(row.max(4)).enumerate() {
        if y % CANCEL_CHECK_ROWS == 0 && cancel.is_cancelled() {
            return None;
        }
        line.as_chunks_mut::<4>().0.iter_mut().for_each(f);
    }
    Some(())
}

/// A straight-alpha pixel premultiplied, rounded.
fn premultiply(p: &mut [u8; 4]) {
    let a = u32::from(p[3]);
    for c in &mut p[..3] {
        *c = ((u32::from(*c) * a + 127) / 255) as u8;
    }
}

/// A premultiplied pixel back to straight alpha, rounded: black where
/// it's fully transparent, and a color Lanczos3 rang past its alpha held
/// at 255.
fn unpremultiply(p: &mut [u8; 4]) {
    let a = u32::from(p[3]);
    for c in &mut p[..3] {
        *c = (u32::from(*c) * 255 + a / 2)
            .checked_div(a)
            .map_or(0, |v| v.min(255) as u8);
    }
}

/// `src`, `w`×`h` pixels of `ch` 8-bit channels, with each `k`×`k` block
/// averaged into one pixel (the last ones from what's left); `None` if
/// cancelled, which is checked every output row.
pub(super) fn average(
    src: &[u8],
    w: u32,
    h: u32,
    ch: usize,
    k: u32,
    cancel: &CancelToken,
) -> Option<Vec<u8>> {
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
pub(super) fn shrink(img: DynamicImage, k: u32, cancel: &CancelToken) -> Option<DynamicImage> {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_peak_counts_each_step_and_the_encode_stage() {
        let encode = |w: u64, h: u64, bpp: u64| w * h * (bpp + 1);
        // No cap, or under it: the encode stage alone.
        assert_eq!(peak(100, 10, 3, None, &encode), 4000);
        assert_eq!(peak(100, 10, 3, Some(100), &encode), 4000);
        // Just over the cap: the picture, Lanczos3's 16 bytes a pixel of
        // its width by the new height, and the result.
        assert_eq!(
            peak(4097, 1024, 3, Some(4096), &encode),
            4097 * 1024 * 3 + 4097 * 1024 * 16 + 4096 * 1024 * 3
        );
        // Over twice the cap: the picture and its averaged copy.
        let (w, h) = (20_000u64, 20_000u64);
        assert_eq!(shrink_factor(w, 4096), 3);
        let averaged = w.div_ceil(3) * h.div_ceil(3) * 3;
        assert_eq!(
            peak(20_000, 20_000, 3, Some(4096), &|_, _, _| 0),
            w * h * 3 + averaged
        );
        // A huge encode stage wins.
        assert_eq!(peak(10, 10, 1, Some(5), &|_, _, _| u64::MAX), u64::MAX);
    }

    #[test]
    fn scaling_fits_the_cap_and_stops_once_cancelled() {
        let cancel = CancelToken::default();
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(300, 100, image::Rgb([9, 8, 7])));
        let same = scale(img.clone(), None, &cancel).unwrap();
        assert_eq!(same.dimensions(), (300, 100));
        assert_eq!(scale(img.clone(), Some(300), &cancel).unwrap(), same);
        // Averaged by 3, then Lanczos3 to the cap.
        let out = scale(img.clone(), Some(50), &cancel).unwrap();
        assert_eq!(out.dimensions(), (50, 17));
        assert_eq!(out.as_rgb8().unwrap().get_pixel(25, 8).0, [9, 8, 7]);
        cancel.cancel();
        assert!(scale(img, Some(50), &cancel).is_none());
    }

    #[test]
    fn transparency_is_scaled_premultiplied() {
        let cancel = CancelToken::default();
        // White, opaque, beside black under full transparency: scaled
        // straight, the edge would come out gray.
        let edge = RgbaImage::from_fn(300, 100, |x, _| {
            if x < 150 {
                image::Rgba([255, 255, 255, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        // By 3 and Lanczos3, and by Lanczos3 alone.
        for cap in [50, 200] {
            let out = scale(DynamicImage::ImageRgba8(edge.clone()), Some(cap), &cancel).unwrap();
            for p in out.as_rgba8().unwrap().pixels() {
                if p.0[3] > 8 {
                    assert!(p.0[..3].iter().all(|&c| c >= 250), "{cap}: {p:?}");
                }
            }
        }
        // Opaque RGBA is scaled as it is.
        let opaque = RgbaImage::from_pixel(4, 2, image::Rgba([10, 200, 30, 255]));
        let out = scale(DynamicImage::ImageRgba8(opaque), Some(2), &cancel).unwrap();
        assert_eq!(
            out.as_rgba8().unwrap().get_pixel(0, 0).0,
            [10, 200, 30, 255]
        );
        // The round trip of one pixel, and a cancel.
        let mut p = [200, 100, 3, 128];
        premultiply(&mut p);
        assert_eq!(p, [100, 50, 2, 128]);
        unpremultiply(&mut p);
        assert_eq!(p, [199, 100, 4, 128]);
        let mut gone = [9, 9, 9, 0];
        unpremultiply(&mut gone);
        assert_eq!(gone, [0, 0, 0, 0]);
        let mut rung = [90, 0, 0, 80];
        unpremultiply(&mut rung);
        assert_eq!(rung, [255, 0, 0, 80]);
        cancel.cancel();
        assert!(scale(DynamicImage::ImageRgba8(edge), Some(50), &cancel).is_none());
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
}
