//! PDF → JPG and PNG: pages drawn by PDFium, encoded with the `image` crate.
//!
//! Every page is drawn at [`Bounds::dpi`] (300) on white. A page over
//! [`Bounds::max_page_pixels`] (or wider or taller than
//! [`Bounds::max_side`]) is drawn at the highest resolution that fits, never
//! a higher one, and reported in `Done::scaled_pages`. The bitmap is
//! PDFium's BGRA with its byte order reversed (so RGBA in memory); it is
//! copied once, flattened onto white as RGB in place, and encoded: JPEG at
//! quality 90 with the resolution in its header, PNG with the default
//! compression. Memory per page is about 8 bytes per pixel at the peak.

use std::io::{self, Write};

use image::codecs::jpeg::{JpegEncoder, PixelDensity};
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use pdfium_render::prelude::{PdfBitmapFormat, PdfColor, PdfPage, PdfRenderConfig, PdfiumError};

use super::protocol::{Bounds, ImageFormat};

/// JPEG quality, the same as Grape's image engine.
const JPEG_QUALITY: u8 = 90;

/// How big one page is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PageSize {
    /// Width in pixels.
    pub(crate) width: u32,
    /// Height in pixels.
    pub(crate) height: u32,
    /// The resolution it works out to.
    pub(crate) dpi: f64,
    /// True when the page was over the bounds and drawn below the
    /// requested resolution.
    pub(crate) scaled: bool,
}

/// The pixel size of a page of `width_pt` × `height_pt` points under
/// `bounds`. `None` for a page with no usable size (not finite, or not
/// above zero).
pub(crate) fn page_size(width_pt: f32, height_pt: f32, bounds: &Bounds) -> Option<PageSize> {
    let (w_pt, h_pt) = (f64::from(width_pt), f64::from(height_pt));
    let dpi = f64::from(bounds.dpi);
    if !(w_pt.is_finite() && h_pt.is_finite() && dpi.is_finite())
        || w_pt <= 0.0
        || h_pt <= 0.0
        || dpi <= 0.0
    {
        return None;
    }
    let (w, h) = (w_pt / 72.0 * dpi, h_pt / 72.0 * dpi);
    let max_pixels = (bounds.max_page_pixels.max(1)) as f64;
    let max_side = f64::from(bounds.max_side.max(1));
    let mut scale: f64 = 1.0;
    if w * h > max_pixels {
        scale = (max_pixels / (w * h)).sqrt();
    }
    scale = scale.min(max_side / w).min(max_side / h);
    let scaled = scale < 1.0;
    // Rounded at the bounds, never up past them.
    let side = |v: f64| -> u32 {
        let px = if scaled {
            (v * scale).floor()
        } else {
            v.round()
        };
        px.clamp(1.0, max_side) as u32
    };
    let (width, height) = (side(w), side(h));
    Some(PageSize {
        width,
        height,
        dpi: dpi * scale.min(1.0),
        scaled,
    })
}

/// Draws `page` at `size` and returns its pixels as RGB, flattened onto
/// white.
pub(crate) fn draw(page: &PdfPage, size: PageSize) -> Result<Vec<u8>, DrawError> {
    let width = i32::try_from(size.width).map_err(|_| DrawError::Size)?;
    let height = i32::try_from(size.height).map_err(|_| DrawError::Size)?;
    let config = PdfRenderConfig::new()
        .set_fixed_size(width, height)
        .set_format(PdfBitmapFormat::BGRA)
        .set_reverse_byte_order(true)
        .clear_before_rendering(true)
        .set_clear_color(PdfColor::WHITE)
        .render_form_data(true)
        .render_annotations(true);
    let mut pixels = {
        let bitmap = page
            .render_with_config(&config)
            .map_err(DrawError::Pdfium)?;
        bitmap.as_raw_bytes()
    };
    let (w, h) = (size.width as usize, size.height as usize);
    let stride = pixels.len().checked_div(h).unwrap_or(0);
    if !rgba_to_rgb_on_white(&mut pixels, w, h, stride) {
        return Err(DrawError::Size);
    }
    Ok(pixels)
}

/// Why a page couldn't be drawn.
#[derive(Debug)]
pub(crate) enum DrawError {
    /// PDFium failed (often: the bitmap couldn't be allocated).
    Pdfium(PdfiumError),
    /// The bitmap PDFium returned doesn't have the size asked for.
    Size,
}

/// Turns `buf`, `height` rows of `stride` bytes holding `width` RGBA
/// pixels each, into tightly packed RGB in place, compositing onto white.
/// False (and `buf` unchanged) when the buffer is too small for the
/// sizes.
pub(crate) fn rgba_to_rgb_on_white(
    buf: &mut Vec<u8>,
    width: usize,
    height: usize,
    stride: usize,
) -> bool {
    let Some(row) = width.checked_mul(4) else {
        return false;
    };
    let fits = stride >= row
        && stride
            .checked_mul(height)
            .is_some_and(|needed| needed <= buf.len());
    if !fits {
        return false;
    }
    let mut out = 0;
    for y in 0..height {
        let start = y * stride;
        for x in 0..width {
            let i = start + x * 4;
            let alpha = u32::from(buf[i + 3]);
            for c in 0..3 {
                let value = u32::from(buf[i + c]);
                // value·a + 255·(255 − a), over 255, rounded.
                let mixed = (value * alpha + 255 * (255 - alpha) + 127) / 255;
                // `out` never passes `i + c`: 3 bytes out per 4 read, and
                // rows only get longer by the stride's padding.
                buf[out] = mixed as u8;
                out += 1;
            }
        }
    }
    buf.truncate(out);
    true
}

/// Encodes `rgb` (`size.width` × `size.height`) as `format` into `out`.
pub(crate) fn encode(
    rgb: &[u8],
    size: PageSize,
    format: ImageFormat,
    out: &mut dyn Write,
) -> Result<(), image::ImageError> {
    match format {
        ImageFormat::Jpg => {
            let mut encoder = JpegEncoder::new_with_quality(out, JPEG_QUALITY);
            let dpi = size.dpi.round().clamp(1.0, f64::from(u16::MAX)) as u16;
            encoder.set_pixel_density(PixelDensity::dpi(dpi));
            encoder.encode(rgb, size.width, size.height, ExtendedColorType::Rgb8)
        }
        ImageFormat::Png => {
            PngEncoder::new(out).write_image(rgb, size.width, size.height, ExtendedColorType::Rgb8)
        }
    }
}

/// A writer that fails once more than `left` bytes went through it, so a
/// job stops at its output bound instead of filling the disk.
pub(crate) struct Budget<W> {
    inner: W,
    left: u64,
    /// True once the bound was hit.
    pub(crate) over: bool,
}

impl<W: Write> Budget<W> {
    /// `inner`, allowed `left` more bytes.
    pub(crate) fn new(inner: W, left: u64) -> Budget<W> {
        Budget {
            inner,
            left,
            over: false,
        }
    }

    /// The bytes still allowed.
    pub(crate) fn left(&self) -> u64 {
        self.left
    }

    /// The wrapped writer.
    pub(crate) fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for Budget<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() as u64 > self.left {
            self.over = true;
            return Err(io::Error::other("over the output bound"));
        }
        let n = self.inner.write(buf)?;
        self.left -= n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn bounds() -> Bounds {
        Bounds {
            dpi: 300.0,
            max_pages: 2000,
            max_page_pixels: 100_000_000,
            max_side: 65_535,
            max_output_bytes: 8 << 30,
            max_text_bytes: 256 << 20,
            max_decompressed: 512 << 20,
            max_split_pages: 100_000,
            max_heap_bytes: 4 << 30,
        }
    }

    #[test]
    fn letter_and_a4_pages_are_drawn_at_300_dpi() {
        let letter = page_size(612.0, 792.0, &bounds()).unwrap();
        assert_eq!((letter.width, letter.height), (2550, 3300));
        assert!(!letter.scaled);
        assert_eq!(letter.dpi, 300.0);
        let a4 = page_size(595.276, 841.89, &bounds()).unwrap();
        assert_eq!((a4.width, a4.height), (2480, 3508));
    }

    #[test]
    fn huge_pages_are_drawn_smaller_never_larger() {
        // 200 × 200 inches: 60,000² pixels at 300 DPI.
        let huge = page_size(14_400.0, 14_400.0, &bounds()).unwrap();
        assert!(huge.scaled);
        assert_eq!((huge.width, huge.height), (10_000, 10_000));
        assert!(u64::from(huge.width) * u64::from(huge.height) <= 100_000_000);
        assert!((huge.dpi - 50.0).abs() < 1e-9);
        // A long strip is held by the side bound.
        let strip = page_size(72.0, 72.0 * 1000.0, &bounds()).unwrap();
        assert!(strip.scaled);
        assert!(strip.height <= 65_535);
        assert!(u64::from(strip.width) * u64::from(strip.height) <= 100_000_000);
        // A tiny page keeps 300 DPI and at least one pixel.
        let tiny = page_size(0.01, 0.01, &bounds()).unwrap();
        assert_eq!((tiny.width, tiny.height), (1, 1));
        assert!(!tiny.scaled);
        // A lower bound scales a normal page.
        let small = Bounds {
            max_page_pixels: 1_000_000,
            ..bounds()
        };
        let letter = page_size(612.0, 792.0, &small).unwrap();
        assert!(letter.scaled && letter.dpi < 300.0);
        assert!(u64::from(letter.width) * u64::from(letter.height) <= 1_000_000);
    }

    #[test]
    fn pages_without_a_usable_size_are_refused() {
        for (w, h) in [
            (0.0, 792.0),
            (-1.0, 792.0),
            (f32::NAN, 1.0),
            (1.0, f32::INFINITY),
        ] {
            assert!(page_size(w, h, &bounds()).is_none());
        }
        let no_dpi = Bounds {
            dpi: 0.0,
            ..bounds()
        };
        assert!(page_size(612.0, 792.0, &no_dpi).is_none());
    }

    #[test]
    fn rgba_flattens_onto_white_in_place() {
        // 2 × 2 pixels in rows of 12 bytes (4 bytes of padding).
        let mut buf = vec![
            255, 0, 0, 255, /**/ 0, 0, 0, 0, /**/ 9, 9, 9, 9, //
            0, 0, 255, 255, /**/ 0, 0, 0, 128, /**/ 9, 9, 9, 9,
        ];
        assert!(rgba_to_rgb_on_white(&mut buf, 2, 2, 12));
        assert_eq!(buf, [255, 0, 0, 255, 255, 255, 0, 0, 255, 127, 127, 127]);
        let mut short = vec![0u8; 15];
        assert!(!rgba_to_rgb_on_white(&mut short, 2, 2, 8));
        assert_eq!(short.len(), 15);
        assert!(!rgba_to_rgb_on_white(&mut vec![0u8; 64], 4, 2, 8));
    }

    #[test]
    fn encoders_write_readable_images() {
        let size = PageSize {
            width: 3,
            height: 2,
            dpi: 300.0,
            scaled: false,
        };
        let rgb = vec![200u8; 3 * 2 * 3];
        for format in [ImageFormat::Jpg, ImageFormat::Png] {
            let mut out = Vec::new();
            encode(&rgb, size, format, &mut out).unwrap();
            let image = image::load_from_memory(&out).unwrap();
            assert_eq!((image.width(), image.height()), (3, 2));
            assert!(!image.color().has_alpha());
        }
        // The JPEG says 300 DPI (JFIF density units 1, 300 × 300).
        let mut out = Vec::new();
        encode(&rgb, size, ImageFormat::Jpg, &mut out).unwrap();
        assert_eq!(&out[6..11], b"JFIF\0");
        assert_eq!(&out[13..18], &[1, 1, 44, 1, 44]);
    }

    #[test]
    fn the_budget_stops_writes_past_the_bound() {
        let mut budget = Budget::new(Vec::new(), 5);
        budget.write_all(b"abc").unwrap();
        assert_eq!(budget.left(), 2);
        assert!(budget.write_all(b"def").is_err());
        assert!(budget.over);
        assert_eq!(budget.into_inner(), b"abc");
    }
}
