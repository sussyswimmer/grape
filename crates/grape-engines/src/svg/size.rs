//! The size rule (D39): how many pixels an SVG is drawn at. Pure.
//!
//! usvg gives the SVG's size `(w, h)` in CSS pixels at 96 per inch, its
//! viewBox already applied ([`resvg::usvg::Tree::size`]).
//!
//! - **Raster** (JPG, PNG, WebP, TIFF, BMP, HEIC, AVIF): twice that when
//!   the longer side is under 256 px (the computed size, not the viewBox:
//!   viewBox units aren't pixels), each side rounded up and at least 1 px.
//! - **Page** (PDF): 300 DPI, `(w, h) × 300 / 96`, rounded up, at least
//!   1 px.
//!
//! Either is then fitted into the request's caps, a side and a pixel
//! count, at the largest uniform factor; a canvas made smaller says so
//! ([`Canvas::scaled`]).

use super::protocol::Mode;

/// Below this many CSS pixels on its longer side, a raster SVG is drawn at
/// twice its size.
pub(crate) const SMALL_SIDE: f64 = 256.0;

/// PDF pages are drawn at this many pixels per inch.
pub(crate) const PAGE_DPI: f64 = 300.0;

/// CSS pixels per inch.
pub(crate) const CSS_DPI: f64 = 96.0;

/// The pixels an SVG is drawn into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Canvas {
    /// Width in pixels, at least 1.
    pub(crate) width: u32,
    /// Height in pixels, at least 1.
    pub(crate) height: u32,
    /// True when the caps made it smaller than the rule's size.
    pub(crate) scaled: bool,
}

/// The canvas for an SVG of `w` × `h` CSS pixels in `mode`, within
/// `max_side` a side and `max_pixels` in all (both at least 1).
pub(crate) fn canvas(mode: Mode, w: f64, h: f64, max_side: u32, max_pixels: u64) -> Canvas {
    let scale = match mode {
        Mode::Raster if w.max(h) < SMALL_SIDE => 2.0,
        Mode::Raster => 1.0,
        Mode::Page => PAGE_DPI / CSS_DPI,
    };
    fit(up(w * scale), up(h * scale), max_side, max_pixels)
}

/// `value` rounded up, at least 1. Not a number is 1; anything too large
/// for a `u64` is `u64::MAX` (`as` saturates).
fn up(value: f64) -> u64 {
    (value.ceil() as u64).max(1)
}

/// `width` × `height` fitted into `max_side` and `max_pixels` at the
/// largest uniform factor `f = min(max_side / W, max_side / H,
/// sqrt(max_pixels / (W × H)))`. When `f < 1`, each side is `floor(side ×
/// f)`, at least 1, and then held to the caps exactly: the factor is a
/// float, so a product that is a whole number may land a hair under it
/// (it's nudged up by a part in 10^12 before the floor) or a side one over.
fn fit(width: u64, height: u64, max_side: u32, max_pixels: u64) -> Canvas {
    let max_side = max_side.max(1);
    let max_pixels = max_pixels.max(1);
    let side = u64::from(max_side);
    let (w, h) = (width as f64, height as f64);
    let f = (side as f64 / w)
        .min(side as f64 / h)
        .min((max_pixels as f64 / (w * h)).sqrt());
    let within = width <= side
        && height <= side
        && width.checked_mul(height).is_some_and(|p| p <= max_pixels);
    if f >= 1.0 && within {
        return Canvas {
            width: width as u32,
            height: height as u32,
            scaled: false,
        };
    }
    let floor = |v: f64| (((v + v * 1e-12).floor() as u64).max(1)).min(side);
    let (mut fw, mut fh) = (floor(w * f), floor(h * f));
    // A float factor can leave the product just over the cap: take rows
    // (or columns) off the longer side until it fits.
    while fw.saturating_mul(fh) > max_pixels {
        if fw >= fh && fw > 1 {
            fw -= 1;
        } else if fh > 1 {
            fh -= 1;
        } else {
            break;
        }
    }
    Canvas {
        // Both are at most `max_side`, a `u32`.
        width: u32::try_from(fw).unwrap_or(max_side),
        height: u32::try_from(fh).unwrap_or(max_side),
        scaled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDE: u32 = 16_384;
    const PIXELS: u64 = 67_108_864;
    const HEIF_PIXELS: u64 = 35_651_584;

    fn raster(w: f64, h: f64) -> Canvas {
        canvas(Mode::Raster, w, h, SIDE, PIXELS)
    }

    fn page(w: f64, h: f64) -> Canvas {
        canvas(Mode::Page, w, h, SIDE, PIXELS)
    }

    fn exact(width: u32, height: u32) -> Canvas {
        Canvas {
            width,
            height,
            scaled: false,
        }
    }

    #[test]
    fn the_spec_table() {
        // viewBox="0 0 24 24" only: 24 × 24, so 48 × 48.
        assert_eq!(raster(24.0, 24.0), exact(48, 48));
        // width="200" viewBox="0 0 100 50": 200 × 100, under 256: twice.
        assert_eq!(raster(200.0, 100.0), exact(400, 200));
        // 210 mm × 297 mm: 793.7 × 1122.5, rounded up.
        assert_eq!(raster(793.700_7, 1_122.519_7), exact(794, 1123));
        // 10 cm × 2 in: 377.95 × 192.
        assert_eq!(raster(377.952_76, 192.0), exact(378, 192));
        // No size and no viewBox, empty: usvg's 100 × 100.
        assert_eq!(raster(100.0, 100.0), exact(200, 200));
        // width="1000" viewBox="0 0 24 24": 1000 px, not 24.
        assert_eq!(raster(1000.0, 1000.0), exact(1000, 1000));
        // viewBox="0 0 1e-30 1e-30": at least 1 px.
        assert_eq!(raster(1e-30, 1e-30), exact(1, 1));
    }

    #[test]
    fn twice_below_256_and_once_from_it() {
        assert_eq!(raster(255.9, 10.0), exact(512, 20));
        assert_eq!(raster(256.0, 10.0), exact(256, 10));
        assert_eq!(raster(10.0, 255.9), exact(20, 512));
        assert_eq!(raster(10.0, 256.0), exact(10, 256));
    }

    #[test]
    fn sides_round_up_and_are_at_least_one() {
        assert_eq!(raster(300.1, 300.0), exact(301, 300));
        assert_eq!(raster(0.2, 0.2), exact(1, 1));
        assert_eq!(raster(1000.0, 0.0001), exact(1000, 1));
        assert_eq!(raster(f64::NAN, 300.0), exact(1, 300));
    }

    #[test]
    fn huge_sizes_are_fitted() {
        let fitted = raster(1e6, 1e6);
        assert!(fitted.scaled);
        assert_eq!((fitted.width, fitted.height), (8192, 8192));
        // One long side: held to 16,384, the other at least 1.
        let long = raster(1e9, 10.0);
        assert!(long.scaled);
        assert_eq!(long.width, SIDE);
        assert_eq!(long.height, 1);
        // Past what f64 or u64 can say.
        let inf = raster(f64::MAX, f64::MAX);
        assert!(inf.scaled);
        assert!(inf.width <= SIDE && inf.height <= SIDE);
        assert!(u64::from(inf.width) * u64::from(inf.height) <= PIXELS);
    }

    #[test]
    fn each_targets_caps_hold() {
        // WebP's format limit is 16,383 a side.
        let webp = canvas(Mode::Raster, 20_000.0, 300.0, 16_383, PIXELS);
        assert!(webp.scaled);
        assert_eq!(webp.width, 16_383);
        // HEIC and AVIF: 35,651,584 pixels.
        let heif = canvas(Mode::Raster, 8000.0, 8000.0, SIDE, HEIF_PIXELS);
        assert!(heif.scaled);
        assert!(u64::from(heif.width) * u64::from(heif.height) <= HEIF_PIXELS);
        assert_eq!((heif.width, heif.height), (5970, 5970));
        // Exactly at the caps is not scaled.
        assert_eq!(raster(8192.0, 8192.0), exact(8192, 8192));
        assert_eq!(raster(16_384.0, 4096.0), exact(16_384, 4096));
        let over = raster(16_384.0, 4097.0);
        assert!(over.scaled);
    }

    #[test]
    fn floor_after_fitting_never_passes_a_cap() {
        for (cap_side, cap_pixels) in [(SIDE, PIXELS), (16_383, PIXELS), (SIDE, HEIF_PIXELS)] {
            for w in [
                257.0, 999.7, 4097.3, 8191.9, 9000.0, 12_345.6, 16_385.0, 33_333.3, 1e5, 7.7e7,
            ] {
                for h in [
                    1.0, 3.3, 256.0, 4097.0, 8191.0, 10_000.5, 16_384.0, 99_999.0,
                ] {
                    let c = canvas(Mode::Raster, w, h, cap_side, cap_pixels);
                    assert!(c.width >= 1 && c.height >= 1);
                    assert!(
                        c.width <= cap_side && c.height <= cap_side,
                        "{w}x{h}: {c:?}"
                    );
                    assert!(
                        u64::from(c.width) * u64::from(c.height) <= cap_pixels,
                        "{w}x{h}: {c:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn pages_are_drawn_at_300_dpi() {
        // 96 CSS px is an inch: 300 px.
        assert_eq!(page(96.0, 48.0), exact(300, 150));
        // A4 at 300 DPI, rounded up.
        assert_eq!(page(793.700_7, 1_122.519_7), exact(2481, 3508));
        // A small SVG is not doubled on a page.
        assert_eq!(page(24.0, 24.0), exact(75, 75));
        assert_eq!(page(0.01, 0.01), exact(1, 1));
        // An SVG too large for 300 DPI is drawn below it.
        let poster = page(10_000.0, 10_000.0);
        assert!(poster.scaled);
        assert_eq!((poster.width, poster.height), (8192, 8192));
    }
}
