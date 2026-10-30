//! Scaling a picture down for Compress, and what that holds at its peak.
//!
//! A picture over twice the long side is first averaged down by a whole
//! factor (Grape's own loop, which a cancel stops at the next row), then
//! Lanczos3 takes it the rest of the way, so that step holds and takes no
//! more than for a picture of twice the long side. [`peak`] says what the
//! steps hold together, so a tool can check it against D16's limit before
//! decoding.
//!
//! A picture with transparency is averaged over each new pixel's area in
//! one step instead ([`area`]), its color weighed by its alpha, so that
//! the color under its transparent pixels doesn't bleed into their
//! neighbors: Lanczos3 rings, and its alpha would come out with values on
//! either side of an edge that the source never had, which libwebp stores
//! losslessly (a two-disc picture's alpha plane took 20 KB at 2048 × 1024
//! instead of 11 KB, and Small's file was larger than its source). Its
//! alpha then takes the nearest value the source's alpha has ([`Levels`]):
//! a cutout with hard edges (only 0 and 255) keeps them hard, as its
//! source has them, and its alpha plane as compact (3.6 KB). But a new
//! pixel a quarter covered or more, or by more than 2 to 1 a little under
//! half what a source pixel covers ([`keep`]), takes the lowest visible
//! level instead of 0, so a thin stroke stays whole: with only 0 and 255,
//! a 1 px stroke at 1.3 to 1 lost the pixels it covered less than half of
//! and came out dotted, and at 3 to 1 vanished. Such a stroke comes out a
//! new pixel or two wide, so bolder than it was. Its color is the visible
//! pixels' own, so no edge darkens or lightens, and its fully transparent
//! pixels take the color of the visible ones nearest them
//! ([`fill_transparent`]): black there made libwebp's stroke edges steps
//! of color, and its files of text and line art 2 to 4 times as large.

use std::collections::VecDeque;

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, GrayImage, RgbImage, RgbaImage};

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
///
/// A picture with transparency holds less: the picture, a row of 16 bytes
/// a pixel and the result ([`area`]), which is colored in place
/// ([`fill_transparent`]: at most 13 bytes for each 8 × 8 block of it).
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
    let img = match img {
        DynamicImage::ImageRgba8(rgba) if transparent(&rgba) => {
            let levels = Levels::of(&rgba);
            let (nw, nh) = scaled_size(rgba.width(), rgba.height(), cap);
            let keep = keep(rgba.dimensions(), (nw, nh));
            let mut out = area(&rgba, nw, nh, cancel, |s| levels.finish(s, keep))?;
            drop(rgba);
            fill_transparent(&mut out, cancel)?;
            return Some(DynamicImage::ImageRgba8(out));
        }
        other => other,
    };
    let k = u32::try_from(shrink_factor(u64::from(long), u64::from(cap))).unwrap_or(u32::MAX);
    let img = if k > 1 { shrink(img, k, cancel)? } else { img };
    if cancel.is_cancelled() {
        return None;
    }
    Some(img.resize(cap, cap, FilterType::Lanczos3))
}

/// The size [`scale`] makes a `width`×`height` picture for `cap`, the
/// same with transparency or not: averaged down by [`shrink_factor`], then
/// fitted as `image`'s `resize` fits it, its sides rounded.
fn scaled_size(width: u32, height: u32, cap: u32) -> (u32, u32) {
    let k = shrink_factor(u64::from(width.max(height)), u64::from(cap));
    let k = u32::try_from(k).unwrap_or(u32::MAX);
    let (w, h) = (f64::from(width.div_ceil(k)), f64::from(height.div_ceil(k)));
    let ratio = (f64::from(cap) / w).min(f64::from(cap) / h);
    let side = |s: f64| ((s * ratio).round() as u32).max(1);
    (side(w), side(h))
}

/// Whether any pixel of `img` isn't opaque.
fn transparent(img: &RgbaImage) -> bool {
    img.as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[3] != u8::MAX)
}

/// The alpha values a picture has: for each of the 256 the nearest of
/// them (the lower on a tie), and the lowest above 0.
struct Levels {
    nearest: [u8; 256],
    /// 0 when there's none.
    lowest: u8,
}

impl Levels {
    fn of(img: &RgbaImage) -> Levels {
        let mut has = [false; 256];
        for p in img.as_raw().as_chunks::<4>().0 {
            has[usize::from(p[3])] = true;
        }
        let mut nearest = [0; 256];
        for (v, slot) in nearest.iter_mut().enumerate() {
            let distance = |l: usize| l.abs_diff(v);
            *slot = (0..=255u8)
                .filter(|&l| has[usize::from(l)])
                .min_by_key(|&l| distance(usize::from(l)))
                .unwrap_or(u8::MAX);
        }
        let lowest = (1..=255u8).find(|&l| has[usize::from(l)]).unwrap_or(0);
        Levels { nearest, lowest }
    }

    /// A new pixel from its [`Sums`]: its alpha the nearest level, but the
    /// lowest above 0 where that's 0 and its alpha is `keep` or more; its
    /// color the visible pixels' mean, or 0 where it's fully transparent
    /// ([`fill_transparent`] colors it).
    fn finish(&self, s: &Sums, keep: f32) -> [u8; 4] {
        let byte = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        let [r, g, b, a] = *s;
        let mut alpha = self.nearest[usize::from(byte(a))];
        if alpha == 0 && a >= keep {
            alpha = self.lowest;
        }
        if alpha == 0 || a <= 0.0 {
            [0, 0, 0, alpha]
        } else {
            [byte(r / a), byte(g / a), byte(b / a), alpha]
        }
    }
}

/// The alpha (0 to 255) from which a new pixel of a `from` picture scaled
/// to `to` is kept visible, whatever level is nearest: a quarter, or when
/// a source pixel covers less than half a new one, a little under half of
/// what it covers (0.45 ÷ the ratio). A 1 px stroke across two new pixels
/// leaves at least half of what it covers in one, so none breaks up.
fn keep(from: (u32, u32), to: (u32, u32)) -> f32 {
    let ratio = |from: u32, to: u32| f64::from(from) / f64::from(to.max(1));
    let ratio = ratio(from.0, to.0).max(ratio(from.1, to.1));
    (255.0 * (0.45 / ratio).min(0.25)) as f32
}

/// The side of the blocks [`fill_transparent`] colors by: libwebp's own
/// for the luma under transparency (`WebPCleanupTransparentArea`), whose
/// chroma blocks are 4 × 4.
const FILL_BLOCK: usize = 8;

/// Colors `img`'s fully transparent pixels as the visible ones nearest
/// them, by [`FILL_BLOCK`]² blocks: in a block with visible pixels, their
/// mean, weighed by alpha; in a block with none, the nearest such block's
/// (by steps across and down, the first found on a tie); with none
/// anywhere, black. libwebp (`exact` off) evens out the luma under the
/// transparency of a block it can see through only in part, to the mean of
/// its visible pixels, but not the chroma, and fills each run of fully
/// transparent blocks with its first one's color: black there put a color
/// step at the edge of every stroke, and a dark one past it, and one color
/// for every fully transparent block tinted the strokes of another. Holds
/// 5 bytes a block, and a queue of up to 8 more; `None` if cancelled,
/// which is checked every block row.
fn fill_transparent(img: &mut RgbaImage, cancel: &CancelToken) -> Option<()> {
    let width = img.width() as usize;
    let row = width * 4;
    if row == 0 {
        return Some(());
    }
    let across = width.div_ceil(FILL_BLOCK);
    let down = (img.height() as usize).div_ceil(FILL_BLOCK);
    // Each block's visible pixels' mean, if it has any.
    let mut fills: Vec<Option<[u8; 4]>> = Vec::with_capacity(across * down);
    let mut sums = vec![[0f64; 4]; across];
    for band in img.chunks(row * FILL_BLOCK) {
        if cancel.is_cancelled() {
            return None;
        }
        sums.fill([0.0; 4]);
        for line in band.chunks(row) {
            for (x, p) in line.as_chunks::<4>().0.iter().enumerate() {
                let sum = &mut sums[x / FILL_BLOCK];
                let a = f64::from(p[3]);
                for (s, &c) in sum.iter_mut().zip(&p[..3]) {
                    *s += a * f64::from(c);
                }
                sum[3] += a;
            }
        }
        fills.extend(sums.iter().map(|s| {
            let c = |v: f64| (v / s[3]).round().clamp(0.0, 255.0) as u8;
            (s[3] > 0.0).then(|| [c(s[0]), c(s[1]), c(s[2]), 0])
        }));
    }
    // Spread outward from the blocks with visible pixels, a step at a time.
    let mut queue: VecDeque<usize> = (0..fills.len()).filter(|&i| fills[i].is_some()).collect();
    while let Some(i) = queue.pop_front() {
        let (x, y) = (i % across, i / across);
        let steps = [
            (x > 0).then(|| i - 1),
            (x + 1 < across).then(|| i + 1),
            (y > 0).then(|| i - across),
            (y + 1 < down).then(|| i + across),
        ];
        for j in steps.into_iter().flatten() {
            if fills[j].is_none() {
                fills[j] = fills[i];
                queue.push_back(j);
            }
        }
    }
    for (band, fills) in img.chunks_mut(row * FILL_BLOCK).zip(fills.chunks(across)) {
        if cancel.is_cancelled() {
            return None;
        }
        for line in band.chunks_mut(row) {
            for (x, p) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                if p[3] == 0 {
                    *p = fills[x / FILL_BLOCK].unwrap_or([0; 4]);
                }
            }
        }
    }
    Some(())
}

/// How much of each source pixel along a side of `from` pixels each of `to`
/// new ones covers, as a share of the new pixel: (the first source pixel,
/// the shares).
fn coverage(from: u32, to: u32) -> Vec<(usize, Vec<f32>)> {
    let ratio = f64::from(from) / f64::from(to.max(1));
    (0..to)
        .map(|i| {
            let (start, end) = (f64::from(i) * ratio, f64::from(i + 1) * ratio);
            let first = start.floor() as usize;
            let shares = (first..(end.ceil() as usize).min(from as usize))
                .map(|j| ((end.min(j as f64 + 1.0) - start.max(j as f64)) / ratio) as f32)
                .collect();
            (first, shares)
        })
        .collect()
}

/// What [`area`] sums for a new pixel, each source pixel weighed by how
/// much of it the new one covers: its color premultiplied (0 to 255²) and
/// its alpha.
type Sums = [f32; 4];

/// `src` scaled to `width`×`height`, each new pixel `finish`ed from the
/// [`Sums`] of the source pixels it covers (a box filter): no weight is
/// negative, so no mean comes out past the values it's made of, and the
/// color under a fully transparent pixel weighs nothing. Holds a row of 16
/// bytes a source pixel besides the two pictures; `None` if cancelled,
/// which is checked every new row.
pub(super) fn area(
    src: &RgbaImage,
    width: u32,
    height: u32,
    cancel: &CancelToken,
    finish: impl Fn(&Sums) -> [u8; 4],
) -> Option<RgbaImage> {
    let across = coverage(src.width(), width);
    let down = coverage(src.height(), height);
    let row = src.width() as usize * 4;
    let mut sum = vec![[0f32; 4]; src.width() as usize];
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    for (first, shares) in &down {
        if cancel.is_cancelled() {
            return None;
        }
        sum.fill([0.0; 4]);
        for (y, &share) in (*first..).zip(shares) {
            let line = src.as_raw().get(y * row..(y + 1) * row)?;
            for (s, p) in sum.iter_mut().zip(line.as_chunks::<4>().0) {
                let a = share * f32::from(p[3]);
                for (s, &c) in s.iter_mut().zip(&p[..3]) {
                    *s += a * f32::from(c);
                }
                s[3] += a;
            }
        }
        for (first, shares) in &across {
            let mut px = [0f32; 4];
            for (x, &share) in (*first..).zip(shares) {
                for (c, &v) in px.iter_mut().zip(sum.get(x)?) {
                    *c += share * v;
                }
            }
            out.extend(finish(&px));
        }
    }
    RgbaImage::from_raw(width, height, out)
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
        // A half transparent color keeps its color, as premultiplied and
        // back in 8 bits it wouldn't (199, 100, 4), beside full
        // transparency too, which weighs nothing: its alpha, 64, the
        // nearer level (0 on a tie), or kept at the lowest above 0.
        let half = RgbaImage::from_fn(2, 1, |x, _| {
            image::Rgba(if x == 0 {
                [200, 100, 3, 128]
            } else {
                [9, 9, 9, 0]
            })
        });
        let levels = Levels::of(&half);
        let view = half.view(0, 0, 1, 1).to_image();
        let out = area(&view, 1, 1, &cancel, |s| levels.finish(s, f32::MAX)).unwrap();
        assert_eq!(out.get_pixel(0, 0).0, [200, 100, 3, 128]);
        let out = area(&half, 1, 1, &cancel, |s| levels.finish(s, 64.0)).unwrap();
        assert_eq!(out.get_pixel(0, 0).0, [200, 100, 3, 128]);
        let out = area(&half, 1, 1, &cancel, |s| levels.finish(s, 64.5)).unwrap();
        assert_eq!(out.get_pixel(0, 0).0, [0, 0, 0, 0]);
        cancel.cancel();
        assert!(scale(DynamicImage::ImageRgba8(edge), Some(50), &cancel).is_none());
    }

    #[test]
    fn transparency_keeps_its_alpha_levels_and_never_rings() {
        let cancel = CancelToken::default();
        // Hard edges (only 0 and 255): a disc stays hard-edged, scaled by
        // 3 and by less, its color white wherever it's seen, and under
        // its transparency too.
        let disc = RgbaImage::from_fn(300, 200, |x, y| {
            let (dx, dy) = (x as f32 - 150.0, y as f32 - 100.0);
            if dx * dx + dy * dy < 80.0 * 80.0 {
                image::Rgba([255, 255, 255, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        for cap in [100, 250] {
            let out = scale(DynamicImage::ImageRgba8(disc.clone()), Some(cap), &cancel).unwrap();
            let out = out.as_rgba8().unwrap();
            assert!(out.pixels().any(|p| p.0 == [255; 4]), "{cap}");
            for p in out.pixels() {
                assert!(p.0 == [255; 4] || p.0 == [255, 255, 255, 0], "{cap}: {p:?}");
            }
        }
        // Three levels stay three.
        let steps = RgbaImage::from_fn(300, 10, |x, _| {
            image::Rgba([90, 90, 90, [0, 128, 255][x as usize / 100]])
        });
        let out = scale(DynamicImage::ImageRgba8(steps), Some(250), &cancel).unwrap();
        assert!(out
            .as_rgba8()
            .unwrap()
            .pixels()
            .all(|p| [0, 128, 255].contains(&p.0[3])));
        // A soft edge keeps every level, and rises steadily, with no value
        // past its neighbours' (Lanczos3 rang 2 to 5 levels either side).
        let ramp = RgbaImage::from_fn(300, 4, |x, _| {
            let a = (x.saturating_sub(100) * 255 / 100).min(255) as u8;
            image::Rgba([40, 200, 40, a])
        });
        for cap in [100, 250] {
            let out = scale(DynamicImage::ImageRgba8(ramp.clone()), Some(cap), &cancel).unwrap();
            let row: Vec<u8> = (0..out.width())
                .map(|x| out.as_rgba8().unwrap().get_pixel(x, 0).0[3])
                .collect();
            assert!(row.windows(2).all(|w| w[0] <= w[1]), "{cap}: {row:?}");
            assert_eq!((row[0], row[row.len() - 1]), (0, 255));
            assert!(row.iter().filter(|&&a| a > 0 && a < 255).count() > 10);
        }
        // The same size as without transparency.
        for (w, h, cap) in [(300, 199, 50), (300, 199, 200), (199, 300, 64)] {
            let rgb = RgbImage::new(w, h);
            let rgba = RgbaImage::new(w, h);
            let plain = scale(DynamicImage::ImageRgb8(rgb), Some(cap), &cancel).unwrap();
            let clear = scale(DynamicImage::ImageRgba8(rgba), Some(cap), &cancel).unwrap();
            assert_eq!(plain.dimensions(), clear.dimensions(), "{w}x{h} {cap}");
        }
        cancel.cancel();
        assert!(area(&disc, 10, 10, &cancel, |_| [0; 4]).is_none());
        assert!(fill_transparent(&mut disc.clone(), &cancel).is_none());
    }

    /// Hard-edged 1 px lines (alpha 0 or 255 only), one every 13 px, at
    /// every phase against the new pixels: across in the top half, down in
    /// the bottom one.
    fn lines(side: u32) -> RgbaImage {
        RgbaImage::from_fn(side, side, |x, y| {
            let half = side / 2;
            let on = if y < half {
                y % 13 == 6 && (4..side - 4).contains(&x)
            } else {
                x % 13 == 6 && (half + 4..side - 4).contains(&y)
            };
            if on {
                image::Rgba([20, 20, 160, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        })
    }

    #[test]
    fn thin_strokes_stay_whole_at_every_ratio() {
        let cancel = CancelToken::default();
        let side = 910;
        let src = lines(side);
        // About 1.3, 2.9 and 4.4 source pixels to a new one.
        for cap in [700, 310, 207] {
            let out = scale(DynamicImage::ImageRgba8(src.clone()), Some(cap), &cancel).unwrap();
            let out = out.as_rgba8().unwrap();
            assert_eq!(out.dimensions(), (cap, cap));
            let r = f64::from(side) / f64::from(cap);
            // The new rows (or columns) a source one at `at` falls in.
            let band = |at: u32| {
                let first = (f64::from(at) / r).floor() as u32;
                let last = ((f64::from(at + 1) / r).ceil() as u32 - 1).min(cap - 1);
                first..=last
            };
            let seen = |x: u32, y: u32| out.get_pixel(x, y).0[3] > 0;
            let inner = |from: u32, to: u32| {
                ((f64::from(from) / r).ceil() as u32 + 1)..((f64::from(to) / r).floor() as u32 - 1)
            };
            for line in (0..side / 2).filter(|y| y % 13 == 6) {
                for x in inner(4, side - 4) {
                    assert!(band(line).any(|y| seen(x, y)), "{cap}: row {line} at {x}");
                }
            }
            for line in (0..side).filter(|x| x % 13 == 6) {
                for y in inner(side / 2 + 4, side - 4) {
                    assert!(
                        band(line).any(|x| seen(x, y)),
                        "{cap}: column {line} at {y}"
                    );
                }
            }
            // Still hard-edged, and the lines' own color.
            for p in out.pixels() {
                assert!(p.0[3] == 0 || p.0 == [20, 20, 160, 255], "{cap}: {p:?}");
            }
        }
    }

    #[test]
    fn transparent_pixels_take_the_color_of_visible_ones_near_them() {
        let cancel = CancelToken::default();
        // Blue strokes over black under full transparency: the color
        // under the transparency comes out the strokes', everywhere.
        let strokes = RgbaImage::from_fn(600, 300, |x, y| {
            if y % 20 < 3 && x < 400 {
                image::Rgba([20, 20, 160, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        let out = scale(DynamicImage::ImageRgba8(strokes), Some(400), &cancel).unwrap();
        assert!(out.as_rgba8().unwrap().pixels().any(|p| p.0[3] == 0));
        for p in out.as_rgba8().unwrap().pixels() {
            assert_eq!(p.0[..3], [20, 20, 160], "{p:?}");
        }
        // In an 8 × 8 block of the new picture with visible pixels, their
        // mean; in one with none, the nearest such block's (the first
        // found on a tie). Red in the first block's top two rows, green in
        // the third block's, by 2.
        let two = RgbaImage::from_fn(128, 32, |x, y| match (x, y) {
            (0..16, 0..4) => image::Rgba([200, 0, 0, 255]),
            (32..48, 0..4) => image::Rgba([0, 100, 0, 255]),
            _ => image::Rgba([7, 7, 7, 0]),
        });
        let out = scale(DynamicImage::ImageRgba8(two), Some(64), &cancel).unwrap();
        let out = out.as_rgba8().unwrap();
        assert_eq!(out.dimensions(), (64, 16));
        assert_eq!(out.get_pixel(0, 0).0, [200, 0, 0, 255]);
        assert_eq!(out.get_pixel(16, 1).0, [0, 100, 0, 255]);
        let (red, green) = ([200, 0, 0, 0], [0, 100, 0, 0]);
        for (x, y, color) in [
            (7, 7, red),
            (20, 5, green),
            // Between the two, as near to each: the first.
            (8, 0, red),
            (15, 7, red),
            (0, 8, red),
            (31, 0, green),
            (63, 15, green),
        ] {
            assert_eq!(out.get_pixel(x, y).0, color, "({x}, {y})");
        }
        // Nothing visible: black.
        let clear = scale(
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(64, 64, image::Rgba([9, 9, 9, 0]))),
            Some(32),
            &cancel,
        )
        .unwrap();
        assert!(clear.as_rgba8().unwrap().pixels().all(|p| p.0 == [0; 4]));
    }

    #[test]
    fn an_area_average_weighs_each_pixel_by_how_much_it_covers() {
        let cancel = CancelToken::default();
        // Opaque: each channel's mean, and the alpha's.
        let mean = |s: &Sums| {
            let c = |v: f32| (v / s[3]).round() as u8;
            [c(s[0]), c(s[1]), c(s[2]), s[3].round() as u8]
        };
        let px = |v: [u8; 3]| {
            RgbaImage::from_fn(3, 1, |x, _| {
                let v = v[x as usize];
                image::Rgba([v, v, v, 255])
            })
        };
        // 3 to 2: (a + b/2) / 1.5 and (b/2 + c) / 1.5.
        let out = area(&px([30, 60, 90]), 2, 1, &cancel, mean).unwrap();
        assert_eq!(out.get_pixel(0, 0).0, [40, 40, 40, 255]);
        assert_eq!(out.get_pixel(1, 0).0, [80, 80, 80, 255]);
        // 3 to 1, and as it is.
        assert_eq!(
            area(&px([30, 60, 90]), 1, 1, &cancel, mean)
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [60, 60, 60, 255]
        );
        assert_eq!(
            area(&px([30, 60, 90]), 3, 1, &cancel, mean).unwrap(),
            px([30, 60, 90])
        );
        // Down too: 1 × 3 to 1 × 2.
        let tall = RgbaImage::from_fn(1, 3, |_, y| {
            let v = [30, 60, 90][y as usize];
            image::Rgba([v, v, v, 255])
        });
        let out = area(&tall, 1, 2, &cancel, mean).unwrap();
        assert_eq!(
            (out.get_pixel(0, 0).0[0], out.get_pixel(0, 1).0[0]),
            (40, 80)
        );
        // Alpha weighs the color, and is averaged itself: 255 and 0 to 1.
        let clear = RgbaImage::from_fn(2, 1, |x, _| {
            image::Rgba(if x == 0 {
                [90, 0, 0, 255]
            } else {
                [0, 200, 0, 0]
            })
        });
        let out = area(&clear, 1, 1, &cancel, mean).unwrap();
        assert_eq!(out.get_pixel(0, 0).0, [90, 0, 0, 128]);
        // 4 to 3: the middle one covers the second half of the second
        // pixel and the first half of the third.
        let (first, shares) = &coverage(4, 3)[1];
        assert_eq!(*first, 1);
        assert_eq!(shares.len(), 2);
        assert!(shares.iter().all(|s| (s - 0.5).abs() < 1e-6), "{shares:?}");
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
