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
//! losslessly. A new pixel that covers only one alpha value keeps it
//! exactly; one that covers several (an edge) takes the nearest step from
//! the least of them toward the most, each step half of what a source
//! pixel covers of a new one, 1 ÷ 2r of the way at a ratio r ([`Steps`]).
//! So a straight line keeps its weight at every phase: a 1 px line covers
//! two steps of the new pixels it falls across, and their two shares round
//! to steps that add up to two (with n + 1 even steps, n the ratio rounded
//! up, a row or column of lines came out at 0.55 to 1.96 of their weight,
//! line by line, as each keeps one phase along its length); a 2 px line,
//! which can cover a new pixel whole, to within a step of its 4. A
//! region's coverage (a dither's, a stroke's) is kept to within half a
//! step a pixel and about exactly on the whole; a 1 px stroke keeps at
//! least a step wherever it passes, at its own opacity whatever other
//! alpha values the picture holds; and a hard edge's alpha plane holds
//! about 2r + 1 values instead of every coverage (by area alone, a
//! signature's alpha plane took 3.4 times as many bytes at 1.03 to 1 as
//! its source's). A 1 px dot covers less of a new pixel than a stroke, so
//! a pattern of them fades below 2 to 1 (one in nine kept 0.87 of its
//! coverage at 1.03 to 1, 0.84 at 1.47) and is held at a step past it
//! (1.55 at 2.93). Its color is the visible pixels' own,
//! so no edge darkens or lightens, and its fully transparent pixels take
//! the color of the visible ones nearest them ([`fill_transparent`]):
//! black there made libwebp's stroke edges steps of color, and its files
//! of text and line art 2 to 4 times as large.
//!
//! Where Lanczos3 would read only opaque pixels, more than
//! [`LANCZOS_REACH`] new pixels from every one the area found something
//! see-through under ([`reach`]), the color is Lanczos3's after all, as
//! without transparency: by area alone, a photo with rounded corners or
//! one pixel at 254 came out of Small softer (38.2 and 38.1 dB against a
//! Lanczos3-scaled reference, opaque 38.7), as did a cutout's inside.

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
/// A picture with transparency (`bpp` 4) holds, through the first two
/// steps, its area-averaged result and a byte a new pixel for where it's
/// near transparency, and 2 more to work out where Lanczos3 reaches it
/// ([`area`], [`reach`]): 7 bytes a new pixel; the result is then colored
/// in place ([`fill_transparent`]: at most 13 bytes for each 8 × 8 block
/// of it).
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
    let area = if bpp == 4 {
        nw.saturating_mul(nh).saturating_mul(7)
    } else {
        0
    };
    [
        size(w, h).saturating_add(averaged).saturating_add(area),
        from.saturating_add(lanczos)
            .saturating_add(size(nw, nh))
            .saturating_add(area),
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
    match img {
        DynamicImage::ImageRgba8(rgba) if transparent(&rgba) => {
            let (nw, nh) = scaled_size(rgba.width(), rgba.height(), cap);
            let steps = Steps::of(rgba.dimensions(), (nw, nh));
            let (mut out, near) = area(&rgba, nw, nh, cancel, |s, lo, hi| steps.finish(s, lo, hi))?;
            let opaque = reach(near, nw as usize, LANCZOS_REACH);
            if opaque.iter().any(|&o| o) {
                // Lanczos3 where it sees only opaque pixels, as without
                // transparency.
                let sharp = lanczos(DynamicImage::ImageRgba8(rgba), cap, cancel)?.into_rgba8();
                if sharp.dimensions() == out.dimensions() {
                    let pixels = out.as_chunks_mut::<4>().0.iter_mut();
                    let sharp = sharp.as_raw().as_chunks::<4>().0;
                    for ((p, s), &o) in pixels.zip(sharp).zip(&opaque) {
                        if o {
                            p[..3].copy_from_slice(&s[..3]);
                        }
                    }
                }
            }
            fill_transparent(&mut out, cancel)?;
            Some(DynamicImage::ImageRgba8(out))
        }
        other => lanczos(other, cap, cancel),
    }
}

/// How many new pixels on each side of one Lanczos3 reads ([`lanczos`]):
/// its kernel's 3, and one more for the whole-factor average before it.
const LANCZOS_REACH: u8 = 4;

/// `img`, over `cap` on its long side, averaged down by [`shrink_factor`]
/// and then fitted inside `cap`×`cap` with Lanczos3; `None` if cancelled.
fn lanczos(img: DynamicImage, cap: u32, cancel: &CancelToken) -> Option<DynamicImage> {
    let long = img.width().max(img.height());
    let k = u32::try_from(shrink_factor(u64::from(long), u64::from(cap))).unwrap_or(u32::MAX);
    let img = if k > 1 { shrink(img, k, cancel)? } else { img };
    if cancel.is_cancelled() {
        return None;
    }
    Some(img.resize(cap, cap, FilterType::Lanczos3))
}

/// Which new pixels of a picture `width` wide are more than `by` pixels
/// (across, down or both) from every one `near` marks. Holds 2 bytes a
/// pixel besides `near`.
fn reach(near: Vec<bool>, width: usize, by: u8) -> Vec<bool> {
    if width == 0 {
        return near;
    }
    // How far each is from a marked one along its row, up to past `by`,
    // then whether that's past `by` in every row within `by`.
    let past = by.saturating_add(1);
    let mut across = vec![past; near.len()];
    for (row, out) in near.chunks(width).zip(across.chunks_mut(width)) {
        let mut to = past;
        for (&n, d) in row.iter().zip(out.iter_mut()) {
            to = if n { 0 } else { to.saturating_add(1).min(past) };
            *d = to;
        }
        let mut to = past;
        for (&n, d) in row.iter().zip(out.iter_mut()).rev() {
            to = if n { 0 } else { to.saturating_add(1).min(past) };
            *d = (*d).min(to);
        }
    }
    drop(near);
    let height = across.len() / width;
    let by = usize::from(by);
    let mut far = vec![false; across.len()];
    for (y, row) in far.chunks_mut(width).enumerate() {
        let rows = y.saturating_sub(by)..(y + by + 1).min(height);
        for (x, f) in row.iter_mut().enumerate() {
            *f = rows.clone().all(|r| across[r * width + x] == past);
        }
    }
    far
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

/// How [`area`]'s new pixels take their alpha ([module docs](self)): one
/// that covers a single alpha value keeps it, and one that covers several
/// a step from the least of them toward the most, each step half of what
/// a source pixel covers of a new pixel (1 ÷ 2r of the way, r the ratio),
/// up to the most. A straight line of source pixels covers a whole number
/// of steps of the new pixels it falls across, two for a 1 px line, split
/// between them as its phase has it: the two shares round to steps that
/// add up to its own, ties to the even step (one of the two is odd, the
/// other even), so it keeps its weight at every phase.
struct Steps {
    /// A step, as a share of the way from the least alpha to the most.
    step: f64,
    /// How far from half a step, in steps, a share is taken as a tie: the
    /// ratios across and down differ a little, by the rounding of the new
    /// size, so a line's shares add up to a little more than its steps.
    tie: f64,
}

impl Steps {
    /// For a `from` picture scaled to `to`.
    fn of(from: (u32, u32), to: (u32, u32)) -> Steps {
        let ratio = |from: u32, to: u32| (f64::from(from) / f64::from(to.max(1))).max(1.0);
        let (across, down) = (ratio(from.0, to.0), ratio(from.1, to.1));
        let ratio = across.max(down);
        Steps {
            step: 1.0 / (2.0 * ratio),
            // A 2 px line's shares add up to 4 steps, so 4 times the
            // difference, and a margin for the sums' rounding.
            tie: 4.0 * (ratio / across.min(down) - 1.0) + 1e-4,
        }
    }

    /// The step nearest `x` (0 to 1, of the way from the least alpha to
    /// the most), ties to the even one, or the most when that's nearer
    /// than the last whole step.
    fn nearest(&self, x: f64) -> f64 {
        let steps = x / self.step;
        let whole = (1.0 / self.step + 1e-9).floor();
        if steps > whole {
            let last = whole * self.step;
            return if x - last < 1.0 - x { last } else { 1.0 };
        }
        let below = steps.floor();
        let k = if (steps - below - 0.5).abs() <= self.tie {
            below + below.rem_euclid(2.0)
        } else {
            steps.round()
        };
        (k * self.step).min(1.0)
    }

    /// A new pixel from its [`Sums`] and the least and most alpha it
    /// covers: its alpha a step, its color the visible pixels' mean, or 0
    /// where it's fully transparent ([`fill_transparent`] colors it).
    fn finish(&self, s: &Sums, lo: u8, hi: u8) -> [u8; 4] {
        let byte = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        let [r, g, b, a] = *s;
        let alpha = if lo == hi {
            lo
        } else {
            let (lo, hi) = (f64::from(lo), f64::from(hi));
            let x = ((f64::from(a) - lo) / (hi - lo)).clamp(0.0, 1.0);
            byte((lo + self.nearest(x) * (hi - lo)) as f32)
        };
        if alpha == 0 || a <= 0.0 {
            [0, 0, 0, alpha]
        } else {
            [byte(r / a), byte(g / a), byte(b / a), alpha]
        }
    }
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
/// [`Sums`] of the source pixels it covers (a box filter) and the least and
/// most alpha among them: no weight is negative, so no mean comes out past
/// the values it's made of, and the color under a fully transparent pixel
/// weighs nothing. With it, which new pixels cover a source pixel that
/// isn't opaque. Holds a row of 18 bytes a source pixel besides the two
/// pictures and a byte a new pixel; `None` if cancelled, which is checked
/// every new row.
pub(super) fn area(
    src: &RgbaImage,
    width: u32,
    height: u32,
    cancel: &CancelToken,
    finish: impl Fn(&Sums, u8, u8) -> [u8; 4],
) -> Option<(RgbaImage, Vec<bool>)> {
    let across = coverage(src.width(), width);
    let down = coverage(src.height(), height);
    let row = src.width() as usize * 4;
    let mut sum = vec![[0f32; 4]; src.width() as usize];
    // The least and most alpha down each source column.
    let mut range = vec![(u8::MAX, 0u8); src.width() as usize];
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    let mut near = Vec::with_capacity(width as usize * height as usize);
    for (first, shares) in &down {
        if cancel.is_cancelled() {
            return None;
        }
        sum.fill([0.0; 4]);
        range.fill((u8::MAX, 0));
        for (y, &share) in (*first..).zip(shares) {
            let line = src.as_raw().get(y * row..(y + 1) * row)?;
            for ((s, p), range) in sum.iter_mut().zip(line.as_chunks::<4>().0).zip(&mut range) {
                let a = share * f32::from(p[3]);
                for (s, &c) in s.iter_mut().zip(&p[..3]) {
                    *s += a * f32::from(c);
                }
                s[3] += a;
                *range = (range.0.min(p[3]), range.1.max(p[3]));
            }
        }
        for (first, shares) in &across {
            let mut px = [0f32; 4];
            let (mut lo, mut hi) = (u8::MAX, 0);
            for (x, &share) in (*first..).zip(shares) {
                for (c, &v) in px.iter_mut().zip(sum.get(x)?) {
                    *c += share * v;
                }
                let &(l, h) = range.get(x)?;
                (lo, hi) = (lo.min(l), hi.max(h));
            }
            out.extend(finish(&px, lo.min(hi), hi));
            near.push(lo != u8::MAX);
        }
    }
    Some((RgbaImage::from_raw(width, height, out)?, near))
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
        // With transparency, the area's result and the map of where it's
        // near transparency, 7 bytes a new pixel, beside both steps.
        assert_eq!(
            peak(4097, 1024, 4, Some(4096), &|_, _, _| 0),
            4097 * 1024 * 4 + 4097 * 1024 * 16 + 4096 * 1024 * 4 + 4096 * 1024 * 7
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
        // transparency too, which weighs nothing: its alpha 64, a step
        // between the 0 and 128 it covers.
        let half = RgbaImage::from_fn(2, 1, |x, _| {
            image::Rgba(if x == 0 {
                [200, 100, 3, 128]
            } else {
                [9, 9, 9, 0]
            })
        });
        let steps = Steps::of((2, 1), (1, 1));
        let view = half.view(0, 0, 1, 1).to_image();
        let (out, _) = area(&view, 1, 1, &cancel, |s, lo, hi| steps.finish(s, lo, hi)).unwrap();
        assert_eq!(out.get_pixel(0, 0).0, [200, 100, 3, 128]);
        let (out, near) = area(&half, 1, 1, &cancel, |s, lo, hi| steps.finish(s, lo, hi)).unwrap();
        assert_eq!(near, [true]);
        assert_eq!(out.get_pixel(0, 0).0, [200, 100, 3, 64]);
        cancel.cancel();
        assert!(scale(DynamicImage::ImageRgba8(edge), Some(50), &cancel).is_none());
    }

    /// Whether `alpha` (from 0 to 255) is a whole number of `step`s or 255,
    /// to within its rounding to a byte.
    fn on_a_step(alpha: u8, step: f64) -> bool {
        let steps = f64::from(alpha) / 255.0 / step;
        alpha == 255 || (steps - steps.round()).abs() <= 0.5 / 255.0 / step + 1e-9
    }

    /// The mean alpha of `img`, 0 to 255.
    fn mean_alpha(img: &RgbaImage) -> f64 {
        let sum: u64 = img.pixels().map(|p| u64::from(p.0[3])).sum();
        sum as f64 / img.pixels().len() as f64
    }

    #[test]
    fn transparency_takes_steps_of_what_it_covers_and_never_rings() {
        let cancel = CancelToken::default();
        // Hard edges (only 0 and 255): a disc's edge takes steps of 1 ÷ 2r
        // (a sixth at 3 to 1; at 1.2, 0.417 and 0.833, and then 1), its
        // color white wherever it's seen, and under its transparency too,
        // and the disc's coverage is kept.
        let disc = RgbaImage::from_fn(300, 200, |x, y| {
            let (dx, dy) = (x as f32 - 150.0, y as f32 - 100.0);
            if dx * dx + dy * dy < 80.0 * 80.0 {
                image::Rgba([255, 255, 255, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        for (cap, step) in [(100, 1.0 / 6.0), (250, 1.0 / 2.4)] {
            let out = scale(DynamicImage::ImageRgba8(disc.clone()), Some(cap), &cancel).unwrap();
            let out = out.as_rgba8().unwrap();
            assert!(out.pixels().any(|p| p.0 == [255; 4]), "{cap}");
            for p in out.pixels() {
                assert!(on_a_step(p.0[3], step), "{cap}: {p:?}");
                assert_eq!(p.0[..3], [255; 3], "{cap}: {p:?}");
            }
            let kept = mean_alpha(out) / mean_alpha(&disc);
            assert!((kept - 1.0).abs() < 0.01, "{cap}: {kept}");
        }
        // Each pixel covering one value keeps it; one covering several takes
        // a step from the least to the most of them.
        let three = RgbaImage::from_fn(300, 10, |x, _| {
            image::Rgba([90, 90, 90, [0, 40, 255][x as usize / 100]])
        });
        let out = scale(DynamicImage::ImageRgba8(three), Some(250), &cancel).unwrap();
        let row: Vec<u8> = (0..250)
            .map(|x| out.as_rgba8().unwrap().get_pixel(x, 0).0[3])
            .collect();
        assert!(row[..83].iter().all(|&a| a == 0), "{row:?}");
        assert!(row[84..166].iter().all(|&a| a == 40), "{row:?}");
        assert!(row[167..].iter().all(|&a| a == 255), "{row:?}");
        // 300 × 10 to 250 × 8: 1.25 to 1, so steps of 0.4 of the way.
        assert!([0, 16, 32, 40].contains(&row[83]), "{}", row[83]);
        assert!([40, 126, 212, 255].contains(&row[166]), "{}", row[166]);
        // A soft edge rises steadily, with no value past its neighbours'
        // (Lanczos3 rang 2 to 5 levels either side), and keeps its levels
        // to within half a step of what each pixel covers.
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
            let kept = mean_alpha(out.as_rgba8().unwrap()) / mean_alpha(&ramp);
            assert!((kept - 1.0).abs() < 0.01, "{cap}: {kept}");
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
        assert!(area(&disc, 10, 10, &cancel, |_, _, _| [0; 4]).is_none());
        assert!(fill_transparent(&mut disc.clone(), &cancel).is_none());
    }

    #[test]
    fn dithered_transparency_keeps_its_coverage() {
        let cancel = CancelToken::default();
        // On or off pixel by pixel (a checkerboard, 2 px stripes), at about
        // 1.03, 1.5 and 2.9 to 1: kept to a few percent, where the nearest
        // source level made a checkerboard 93 % opaque at 1.03, the stripes
        // 63 %. At 1.53, steps of 1 ÷ 2r are 0.33 apart, so a
        // checkerboard's pixels, each a little either side of half covered,
        // come out at 0.33 or 0.65: 1.05 of its coverage (1.00 with steps
        // of 1 ÷ ⌈r⌉, which had a step at half). One dot in nine at 1.03
        // (18 % before): within a sixth; a dot covers less than a stroke.
        let side = 930;
        for (name, on) in [
            (
                "checker",
                &(|x: u32, y: u32| (x + y).is_multiple_of(2)) as &dyn Fn(u32, u32) -> bool,
            ),
            ("stripes", &|_, y| (y / 2).is_multiple_of(2)),
            ("dots", &|x, y| x % 3 == 1 && y % 3 == 1),
        ] {
            let src = RgbaImage::from_fn(side, side, |x, y| {
                image::Rgba([60, 60, 60, if on(x, y) { 255 } else { 0 }])
            });
            let caps: &[u32] = if name == "dots" {
                &[903]
            } else {
                &[903, 607, 317]
            };
            for &cap in caps {
                let out = scale(DynamicImage::ImageRgba8(src.clone()), Some(cap), &cancel).unwrap();
                let kept = mean_alpha(out.as_rgba8().unwrap()) / mean_alpha(&src);
                let most = match name {
                    "dots" => 0.17,
                    "checker" => 0.06,
                    _ => 0.03,
                };
                assert!((kept - 1.0).abs() < most, "{name} {cap}: {kept}");
            }
        }
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
        // About 1.3, 3.0 and 4.5 source pixels to a new one. (At exactly
        // 1.3, lines every 13 px, 10 new pixels apart, all fall across the
        // same two new pixels the same way: with steps of 1 ÷ ⌈r⌉, each
        // came out half a step heavier or lighter, 30 % here.)
        for cap in [690, 307, 203] {
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
            // The lines' own color, at a step of their 255, and as bold as
            // they are: their coverage kept but for their ends.
            for p in out.pixels() {
                assert!(on_a_step(p.0[3], 1.0 / (2.0 * r)), "{cap}: {p:?}");
                assert!(p.0[3] == 0 || p.0[..3] == [20, 20, 160], "{cap}: {p:?}");
            }
            let kept = mean_alpha(out) / mean_alpha(&src);
            assert!((kept - 1.0).abs() < 0.02, "{cap}: {kept}");
        }
    }

    /// Each straight line's weight in `out`, scaled from a `w`×`h` grid of
    /// lines `width` px wide every 17 px from 8, across and down, as a share
    /// of its true weight (`width` ÷ the ratio): a line down measured over
    /// the new rows clear of every line across, and the other way round.
    fn line_weights(out: &RgbaImage, (w, h): (u32, u32), width: u32) -> Vec<f64> {
        let (rx, ry) = (
            f64::from(w) / f64::from(out.width()),
            f64::from(h) / f64::from(out.height()),
        );
        let lines = |side: u32| (8..side).step_by(17).collect::<Vec<u32>>();
        let clear = |n: u32, r: f64, across: &[u32]| -> Vec<u32> {
            (0..n)
                .filter(|&i| {
                    let (from, to) = (f64::from(i) * r, f64::from(i + 1) * r);
                    across
                        .iter()
                        .all(|&l| to <= f64::from(l) - 1.0 || from >= f64::from(l + width) + 1.0)
                })
                .collect()
        };
        let (rows, cols) = (
            clear(out.height(), ry, &lines(h)),
            clear(out.width(), rx, &lines(w)),
        );
        let alpha = |x: u32, y: u32| f64::from(out.get_pixel(x, y).0[3]) / 255.0;
        let band = |at: u32, r: f64, n: u32| {
            let first = (f64::from(at) / r).floor() as u32;
            let last = (f64::from(at + width) / r).ceil() as u32;
            (first.saturating_sub(1)..(last + 1).min(n)).collect::<Vec<u32>>()
        };
        let mut weights = Vec::new();
        for x0 in lines(w).into_iter().filter(|&x| x + width + 17 < w) {
            let band = band(x0, rx, out.width());
            let sum: f64 = rows
                .iter()
                .map(|&y| band.iter().map(|&x| alpha(x, y)).sum::<f64>())
                .sum();
            weights.push(sum / rows.len() as f64 / (f64::from(width) / rx));
        }
        for y0 in lines(h).into_iter().filter(|&y| y + width + 17 < h) {
            let band = band(y0, ry, out.height());
            let sum: f64 = cols
                .iter()
                .map(|&x| band.iter().map(|&y| alpha(x, y)).sum::<f64>())
                .sum();
            weights.push(sum / cols.len() as f64 / (f64::from(width) / ry));
        }
        weights
    }

    #[test]
    fn straight_lines_keep_their_weight_at_every_ratio_and_phase() {
        let cancel = CancelToken::default();
        // Grids of hard 1 and 2 px lines (alpha 0 or 255) every 17 px, at
        // the ratios of 2250, 2600, 3000 and 4000 px to Small's 2048, and
        // 2.93 to 1. A line keeps one phase against the new pixels along
        // its length, so steps of 1 ÷ ⌈r⌉ (round 4) left whole lines at
        // 0.55 to 1.96 of their weight, side by side.
        for (w, cap) in [
            (1125, 1024),
            (1300, 1024),
            (1500, 1024),
            (2000, 1024),
            (1500, 512),
        ] {
            for width in [1, 2] {
                let h = 340;
                let grid = RgbaImage::from_fn(w, h, |x, y| {
                    let on = |v: u32| v >= 8 && (v - 8) % 17 < width;
                    let a = if on(x) || on(y) { 255 } else { 0 };
                    image::Rgba([90, 90, 90, a])
                });
                let out = scale(DynamicImage::ImageRgba8(grid), Some(cap), &cancel).unwrap();
                let weights = line_weights(out.as_rgba8().unwrap(), (w, h), width);
                assert!(weights.len() > 20, "{w} {cap}: {weights:?}");
                // A 1 px line exactly. A 2 px one can cover a new pixel
                // whole, which keeps its alpha, so only its two partly
                // covered pixels take steps, apart: within a step of its 4.
                let (least, most) = if width == 1 {
                    (0.98, 1.02)
                } else {
                    (0.75, 1.25)
                };
                for (i, &weight) in weights.iter().enumerate() {
                    assert!(
                        (least..=most).contains(&weight),
                        "{w} {cap} {width} px, line {i}: {weights:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn faint_strokes_stay_visible_at_their_own_opacity_whatever_else_there_is() {
        let cancel = CancelToken::default();
        // 1 px lines at alpha 128 and 40, one every 13 px across the top
        // half, beside a pixel at 1 and a panel at 40 in the bottom half:
        // the nearest of the picture's levels left a line at 40 nowhere
        // past 2 to 1, and the pixel at 1 made every faint coverage 1.
        let side = 910;
        for alpha in [128u8, 40] {
            let src = RgbaImage::from_fn(side, side, |x, y| {
                let a = if (x, y) == (5, 700) {
                    1
                } else if y >= side / 2 + 50 && (600..690).contains(&x) {
                    40
                } else if y < side / 2 && y % 13 == 6 && (4..side - 4).contains(&x) {
                    alpha
                } else {
                    0
                };
                image::Rgba([20, 20, 160, a])
            });
            for cap in [690, 307, 203] {
                let out = scale(DynamicImage::ImageRgba8(src.clone()), Some(cap), &cancel).unwrap();
                let out = out.as_rgba8().unwrap();
                let r = f64::from(side) / f64::from(cap);
                let mut sum = 0u64;
                for line in (0..side / 2).filter(|y| y % 13 == 6) {
                    let rows = (f64::from(line) / r).floor() as u32
                        ..=((f64::from(line + 1) / r).ceil() as u32 - 1).min(cap - 1);
                    let first = (4.0 / r).ceil() as u32 + 1;
                    for x in first..((f64::from(side - 4) / r).floor() as u32 - 1) {
                        let seen: Vec<u8> =
                            rows.clone().map(|y| out.get_pixel(x, y).0[3]).collect();
                        assert!(seen.iter().any(|&a| a > 0), "{alpha} {cap}: {line} at {x}");
                        assert!(seen.iter().all(|&a| a <= alpha), "{alpha} {cap}: {seen:?}");
                        sum += seen.iter().map(|&a| u64::from(a)).sum::<u64>();
                    }
                }
                // Each line's opacity over the new pixels it crosses, as a
                // share of its own: 1 ÷ the ratio, give or take a quarter.
                let lines = (0..side / 2).filter(|y| y % 13 == 6).count() as f64;
                let columns = ((f64::from(side - 4) / r).floor() - (4.0 / r).ceil() - 2.0).max(1.0);
                let opacity = sum as f64 / (lines * columns * f64::from(alpha)) * r;
                assert!((opacity - 1.0).abs() < 0.25, "{alpha} {cap}: {opacity}");
                // Nothing faint spreads past the lines, the panel and the
                // pixel at 1 (which keeps itself, at 1).
                for (x, y, p) in out.enumerate_pixels() {
                    let (sx, sy) = (f64::from(x) * r, f64::from(y) * r);
                    let panel = sy + r > f64::from(side / 2 + 50) && sx + r > 600.0 && sx < 690.0;
                    let stray = sx < 6.0 && sx + r > 5.0 && sy < 701.0 && sy + r > 700.0;
                    if sy > f64::from(side / 2) && !panel {
                        assert!(p.0[3] <= u8::from(stray), "{alpha} {cap}: ({x}, {y}) {p:?}");
                    }
                }
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
    fn a_picture_with_a_little_transparency_stays_as_sharp_as_without() {
        let cancel = CancelToken::default();
        // Detail Lanczos3 keeps sharper than an area average, with one
        // pixel at 254 in the middle, or transparent corners.
        let rgb = RgbImage::from_fn(300, 150, |x, y| {
            let v = |a: u32, b: u32| ((a * 37 + b * 11) % 256) as u8;
            image::Rgb([v(x, y), v(y, x), v(x + y, x * y % 7)])
        });
        let with = |alpha: &dyn Fn(u32, u32) -> u8| {
            RgbaImage::from_fn(300, 150, |x, y| {
                let [r, g, b] = rgb.get_pixel(x, y).0;
                image::Rgba([r, g, b, alpha(x, y)])
            })
        };
        let dot = with(&|x, y| if (x, y) == (150, 75) { 254 } else { 255 });
        let corners = with(&|x, y| {
            let (dx, dy) = (x.min(299 - x), y.min(149 - y));
            if dx + dy < 12 {
                0
            } else {
                255
            }
        });
        // By 1.5, and by 3 (averaged by 2 first).
        for cap in [200, 100] {
            let plain = scale(DynamicImage::ImageRgb8(rgb.clone()), Some(cap), &cancel).unwrap();
            let plain = plain.as_rgb8().unwrap();
            let r = 300.0 / f64::from(cap);
            let near_dot = |x: u32, y: u32| x.abs_diff(cap / 2) <= 6 && y.abs_diff(cap / 4) <= 6;
            let near_corner = |x: u32, y: u32| {
                let (w, h) = (cap, cap / 2);
                let d = x.min(w - 1 - x) + y.min(h - 1 - y);
                f64::from(d) <= 12.0 / r + 10.0
            };
            type Near<'a> = &'a dyn Fn(u32, u32) -> bool;
            let cases: [(&RgbaImage, Near); 2] = [(&dot, &near_dot), (&corners, &near_corner)];
            for (img, near) in cases {
                let out = scale(DynamicImage::ImageRgba8(img.clone()), Some(cap), &cancel).unwrap();
                let out = out.as_rgba8().unwrap();
                assert_eq!(out.dimensions(), plain.dimensions());
                let (mut same, mut far) = (0, 0);
                for (x, y, p) in out.enumerate_pixels() {
                    if !near(x, y) {
                        far += 1;
                        assert_eq!(p.0[..3], plain.get_pixel(x, y).0, "{cap}: ({x}, {y})");
                        assert_eq!(p.0[3], 255);
                    }
                    same += usize::from(p.0[..3] == plain.get_pixel(x, y).0);
                }
                assert!(far > 0 && same < out.pixels().len(), "{cap}");
            }
        }
    }

    #[test]
    fn reach_marks_what_is_far_from_every_marked_pixel() {
        // 7 × 3, one marked at (3, 1): within 1 of it, all but the outer
        // columns; within 0, all but it; nothing marked, all far.
        let mut near = vec![false; 21];
        near[7 + 3] = true;
        let far = reach(near.clone(), 7, 1);
        let rows: Vec<Vec<bool>> = far.chunks(7).map(<[bool]>::to_vec).collect();
        let row = [true, true, false, false, false, true, true];
        assert_eq!(rows, [row; 3]);
        let far = reach(near, 7, 0);
        assert_eq!(far.iter().filter(|&&f| !f).count(), 1);
        assert!(!far[10]);
        assert!(reach(vec![false; 21], 7, 4).iter().all(|&f| f));
        assert!(reach(Vec::new(), 0, 4).is_empty());
    }

    #[test]
    fn an_area_average_weighs_each_pixel_by_how_much_it_covers() {
        let cancel = CancelToken::default();
        // Opaque: each channel's mean, and the alpha's.
        let mean = |s: &Sums, _: u8, _: u8| {
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
        let (out, near) = area(&px([30, 60, 90]), 2, 1, &cancel, mean).unwrap();
        assert_eq!(near, [false, false]);
        assert_eq!(out.get_pixel(0, 0).0, [40, 40, 40, 255]);
        assert_eq!(out.get_pixel(1, 0).0, [80, 80, 80, 255]);
        // 3 to 1, and as it is.
        assert_eq!(
            area(&px([30, 60, 90]), 1, 1, &cancel, mean)
                .unwrap()
                .0
                .get_pixel(0, 0)
                .0,
            [60, 60, 60, 255]
        );
        assert_eq!(
            area(&px([30, 60, 90]), 3, 1, &cancel, mean).unwrap().0,
            px([30, 60, 90])
        );
        // Down too: 1 × 3 to 1 × 2.
        let tall = RgbaImage::from_fn(1, 3, |_, y| {
            let v = [30, 60, 90][y as usize];
            image::Rgba([v, v, v, 255])
        });
        let (out, _) = area(&tall, 1, 2, &cancel, mean).unwrap();
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
        let (out, near) = area(&clear, 1, 1, &cancel, mean).unwrap();
        assert_eq!(near, [true]);
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
