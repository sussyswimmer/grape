//! Images → PDF and Create PDF, drawn by a real PDFium (feature `pdf-it`,
//! D37): every page is rendered at its point size, one pixel a point, and
//! compared with the upright decode of its image. `GRAPE_TEST_PDFIUM` must
//! name the PDFium library; without it these tests fail rather than skip.
//! PDFium is bound once, in this test binary, and the pages are drawn one
//! test at a time.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use common::{assert_no_leftovers, plan_one, planned_file, SourceState, TestDir};
use grape_core::{plan, Format, Operation, Settings, ToolKey};
use grape_engines::{build_selection, run_job, EngineCtx};
use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{
    DynamicImage, GrayImage, ImageDecoder, ImageFormat, ImageReader, Luma, Rgb, RgbImage, Rgba,
    RgbaImage,
};
use pdfium_render::prelude::{PdfBitmapFormat, PdfColor, PdfRenderConfig, Pdfium};

fn pdfium_path() -> PathBuf {
    let path = std::env::var_os("GRAPE_TEST_PDFIUM")
        .map(PathBuf::from)
        .expect("set GRAPE_TEST_PDFIUM to the PDFium library to run the pdf-it tests");
    assert!(
        path.is_file(),
        "GRAPE_TEST_PDFIUM is not a file: {}",
        path.display()
    );
    path
}

/// PDFium, bound once; the lock keeps one test drawing at a time.
fn pdfium() -> std::sync::MutexGuard<'static, Pdfium> {
    static PDFIUM: OnceLock<Mutex<Pdfium>> = OnceLock::new();
    PDFIUM
        .get_or_init(|| {
            let bindings = Pdfium::bind_to_library(pdfium_path()).unwrap();
            Mutex::new(Pdfium::new(bindings))
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A drawn page: its size in points and its pixels, RGB on white.
struct Drawn {
    width_pt: f32,
    height_pt: f32,
    pixels: RgbImage,
}

/// Draws every page of the PDF at `path` at one pixel a point.
fn draw(path: &Path) -> Vec<Drawn> {
    let bytes = fs::read(path).unwrap();
    let pdfium = pdfium();
    let document = pdfium.load_pdf_from_byte_slice(&bytes, None).unwrap();
    let mut drawn = Vec::new();
    for index in 0..document.pages().len() {
        let page = document.pages().get(index).unwrap();
        let (width_pt, height_pt) = (page.width().value, page.height().value);
        let (width, height) = (width_pt.round() as i32, height_pt.round() as i32);
        let config = PdfRenderConfig::new()
            .set_fixed_size(width, height)
            .set_format(PdfBitmapFormat::BGRA)
            .set_reverse_byte_order(true)
            .clear_before_rendering(true)
            .set_clear_color(PdfColor::WHITE);
        let bitmap = page.render_with_config(&config).unwrap();
        let raw = bitmap.as_raw_bytes();
        let (w, h) = (width as u32, height as u32);
        let stride = raw.len() / h as usize;
        let pixels = RgbImage::from_fn(w, h, |x, y| {
            let at = y as usize * stride + x as usize * 4;
            // RGBA, alpha composited onto white.
            let a = u32::from(raw[at + 3]);
            let over = |c: u8| ((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
            Rgb([over(raw[at]), over(raw[at + 1]), over(raw[at + 2])])
        });
        drawn.push(Drawn {
            width_pt,
            height_pt,
            pixels,
        });
    }
    drawn
}

/// The mean absolute difference of two images of the same size.
fn mean_error(a: &RgbImage, b: &RgbImage) -> f64 {
    assert_eq!(a.dimensions(), b.dimensions());
    let total: u64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(&x, &y)| u64::from(x.abs_diff(y)))
        .sum();
    total as f64 / a.as_raw().len() as f64
}

/// `path` decoded upright, as Grape's raster engine does.
fn upright(path: &Path) -> RgbImage {
    let mut decoder = ImageReader::open(path)
        .unwrap()
        .with_guessed_format()
        .unwrap()
        .into_decoder()
        .unwrap();
    let orientation = decoder.orientation().unwrap();
    let mut img = DynamicImage::from_decoder(decoder).unwrap();
    img.apply_orientation(orientation);
    img.to_rgb8()
}

/// Four smooth quadrants of different hues, so any turn or flip shows.
fn quadrants(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        let (right, bottom) = (x >= width / 2, y >= height / 2);
        let shade = (x * 2 + y) as u8;
        match (right, bottom) {
            (false, false) => Rgb([220, 40 + shade / 4, 40]),
            (true, false) => Rgb([40, 200, 60 + shade / 4]),
            (false, true) => Rgb([50 + shade / 4, 60, 210]),
            (true, true) => Rgb([230, 220, 40 + shade / 4]),
        }
    })
}

/// `jpeg` with an EXIF APP1 saying `orientation` right after SOI.
fn with_orientation(jpeg: &[u8], orientation: u8) -> Vec<u8> {
    let mut payload = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0".to_vec();
    payload.extend_from_slice(&[orientation, 0, 0, 0, 0, 0, 0]);
    let len = u16::try_from(payload.len() + 2).unwrap().to_be_bytes();
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1, len[0], len[1]]);
    out.extend(payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[test]
fn a_jpeg_shows_upright_in_all_eight_orientations() {
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 95)
        .encode_image(&quadrants(48, 32))
        .unwrap();
    for (exif, orientation) in [
        (1, Orientation::NoTransforms),
        (2, Orientation::FlipHorizontal),
        (3, Orientation::Rotate180),
        (4, Orientation::FlipVertical),
        (5, Orientation::Rotate90FlipH),
        (6, Orientation::Rotate90),
        (7, Orientation::Rotate270FlipH),
        (8, Orientation::Rotate270),
    ] {
        let dir = TestDir::new("create-render-orient");
        let bytes = with_orientation(&jpeg, exif);
        let (src, job) = plan_one(&dir, "photo.jpg", &bytes, Operation::convert(Format::Pdf));
        let before = SourceState::of(&src);
        run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();
        let expected = upright(&src);
        let quarter = matches!(exif, 5..=8);
        assert_eq!(
            expected.dimensions(),
            if quarter { (32, 48) } else { (48, 32) }
        );
        let pages = draw(&planned_file(&job));
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        assert_eq!(
            (page.width_pt, page.height_pt),
            (expected.width() as f32, expected.height() as f32),
            "{orientation:?}"
        );
        // PDFium's JPEG decoder isn't Grape's: allow a small difference.
        let error = mean_error(&page.pixels, &expected);
        assert!(error < 1.0, "{orientation:?}: {error}");
        before.assert_unchanged();
        assert_no_leftovers(&dir.path);
    }
}

#[test]
fn an_adobe_rgb_jpeg_draws_as_grape_decodes_it() {
    // The encoder's JPEG with its JFIF APP0 replaced by an Adobe APP14
    // saying transform 0: its samples are read as RGB, not YCbCr, by Grape's
    // decoder and, through `/DecodeParms << /ColorTransform 0 >>` and the
    // kept marker, by PDFium.
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 95)
        .encode_image(&quadrants(48, 32))
        .unwrap();
    assert_eq!(&jpeg[2..4], [0xFF, 0xE0]);
    let app0_end = 4 + usize::from(u16::from_be_bytes([jpeg[4], jpeg[5]]));
    let adobe = [
        0xFF, 0xEE, 0, 14, b'A', b'd', b'o', b'b', b'e', 0, 100, 0, 0, 0, 0, 0,
    ];
    jpeg.splice(2..app0_end, adobe);
    let dir = TestDir::new("create-render-adobe");
    let (src, job) = plan_one(&dir, "rgb.jpg", &jpeg, Operation::convert(Format::Pdf));
    let before = SourceState::of(&src);
    run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();
    let expected = upright(&src);
    let bytes = fs::read(planned_file(&job)).unwrap();
    let doc = lopdf::Document::load_mem(&bytes).unwrap();
    let image = doc
        .objects
        .values()
        .filter_map(|object| object.as_stream().ok())
        .find(|stream| stream.dict.get(b"Subtype").and_then(|s| s.as_name()).ok() == Some(b"Image"))
        .unwrap();
    let parms = image.dict.get(b"DecodeParms").unwrap().as_dict().unwrap();
    assert_eq!(parms.get(b"ColorTransform").unwrap().as_i64().unwrap(), 0);
    let pages = draw(&planned_file(&job));
    let error = mean_error(&pages[0].pixels, &expected);
    assert!(error < 1.0, "{error}");
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

fn png(img: &DynamicImage) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    img.write_to(&mut bytes, ImageFormat::Png).unwrap();
    bytes.into_inner()
}

/// `img` composited onto white.
fn on_white(img: &RgbaImage) -> RgbImage {
    RgbImage::from_fn(img.width(), img.height(), |x, y| {
        let [r, g, b, a] = img.get_pixel(x, y).0;
        let a = u32::from(a);
        let over = |c: u8| ((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
        Rgb([over(r), over(g), over(b)])
    })
}

#[test]
fn lossless_pages_draw_as_their_images() {
    let dir = TestDir::new("create-render-lossless");
    // Alpha, drawn over white.
    let alpha = RgbaImage::from_fn(40, 30, |x, y| {
        Rgba([(x * 6) as u8, (y * 8) as u8, 120, ((x + y) * 4) as u8])
    });
    // Pure black and white, written at one bit a pixel.
    let text = GrayImage::from_fn(37, 21, |x, y| {
        Luma([if (x / 2 + y / 3) % 2 == 0 { 0 } else { 255 }])
    });
    // A smooth RGB gradient as TIFF.
    let gradient = RgbImage::from_fn(33, 17, |x, y| Rgb([(x * 7) as u8, (y * 15) as u8, 99]));
    let mut tiff = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(gradient.clone())
        .write_to(&mut tiff, ImageFormat::Tiff)
        .unwrap();
    let files: Vec<(&str, Vec<u8>, RgbImage)> = vec![
        (
            "a.png",
            png(&DynamicImage::ImageRgba8(alpha.clone())),
            on_white(&alpha),
        ),
        (
            "b.png",
            png(&DynamicImage::ImageLuma8(text.clone())),
            DynamicImage::ImageLuma8(text).to_rgb8(),
        ),
        ("c.tiff", tiff.into_inner(), gradient),
    ];
    let mut paths = Vec::new();
    for (name, bytes, _) in &files {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        paths.push(path);
    }
    let settings = Settings::new(dir.join("fallback"));
    let selection = build_selection(&paths, &settings);
    let job = plan(&selection, &Operation::tool(ToolKey::CreatePdf), &settings)
        .unwrap()
        .jobs
        .remove(0);
    run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();
    let pages = draw(&dir.join("a Combined.pdf"));
    assert_eq!(pages.len(), 3);
    for (page, (name, _, expected)) in pages.iter().zip(&files) {
        assert_eq!(
            (page.width_pt, page.height_pt),
            (expected.width() as f32, expected.height() as f32),
            "{name}"
        );
        let error = mean_error(&page.pixels, expected);
        assert!(error < 1.0, "{name}: {error}");
    }
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_picture_longer_than_an_image_may_be_draws_whole() {
    // 8 × 140,000: PDFium draws nothing for an image with a side over
    // 131,071, so Grape writes it as strips. A ramp from black at the top
    // to white at the bottom shows any strip missing, misplaced or out of
    // order.
    let dir = TestDir::new("create-render-tall");
    let height = 140_000u32;
    let ramp = GrayImage::from_fn(8, height, |_, y| {
        Luma([(u64::from(y) * 255 / u64::from(height - 1)) as u8])
    });
    let (src, job) = plan_one(
        &dir,
        "tall.png",
        &png(&DynamicImage::ImageLuma8(ramp)),
        Operation::convert(Format::Pdf),
    );
    let before = SourceState::of(&src);
    run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();
    let pages = draw(&planned_file(&job));
    assert_eq!(pages.len(), 1);
    let page = &pages[0];
    // 14,400 pt tall; the width, 0.82 pt, is held at 1 pt.
    assert_eq!((page.width_pt, page.height_pt), (1.0, 14_400.0));
    let mut worst = (0.0f64, 0u32);
    for row in 0..page.pixels.height() {
        let drawn = f64::from(page.pixels.get_pixel(0, row).0[0]);
        let expected = (f64::from(row) + 0.5) / f64::from(page.pixels.height()) * 255.0;
        let error = (drawn - expected).abs();
        if error > worst.0 {
            worst = (error, row);
        }
    }
    assert!(worst.0 <= 3.0, "row {} is off by {}", worst.1, worst.0);
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_page_over_200_inches_is_scaled_down_whole() {
    let dir = TestDir::new("create-render-wide");
    let wide = RgbImage::from_fn(20_000, 10, |x, _| Rgb([(x % 256) as u8, 0, 0]));
    let (_, job) = plan_one(
        &dir,
        "wide.png",
        &png(&DynamicImage::ImageRgb8(wide)),
        Operation::convert(Format::Pdf),
    );
    run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();
    let bytes = fs::read(planned_file(&job)).unwrap();
    let pdfium = pdfium();
    let document = pdfium.load_pdf_from_byte_slice(&bytes, None).unwrap();
    let page = document.pages().get(0).unwrap();
    assert_eq!(page.width().value, 14_400.0);
    assert!(
        (page.height().value - 7.2).abs() < 1e-3,
        "{}",
        page.height().value
    );
}
