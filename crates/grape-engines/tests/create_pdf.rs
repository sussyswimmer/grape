//! Images → PDF and Create PDF end to end through `run_job` (D37): in
//! Grape's own process, so no sidecar is needed. Every image is generated
//! here; every test that runs a job hashes its sources before and after
//! and checks that no temp file is left. The PDFs are read back with
//! lopdf.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use common::{assert_no_leftovers, plan_one, planned_file, SourceState, TestDir};
use grape_core::{plan, Format, Job, Operation, Settings, ToolKey};
use grape_engines::{
    build_selection, run_job, supports, supports_with, EngineCtx, ErrorKind, GrapeError,
    JobOutcome, SidecarPaths,
};
use image::codecs::jpeg::JpegEncoder;
use image::{
    DynamicImage, GrayImage, ImageEncoder, ImageFormat, Luma, Rgb, RgbImage, Rgba, RgbaImage,
};
use lopdf::{Dictionary, Document};

fn gradient(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
        Rgb([
            (x * 5 % 256) as u8,
            (y * 7 % 256) as u8,
            ((x + y) % 256) as u8,
        ])
    }))
}

fn encode(img: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    img.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}

fn image_format(format: Format) -> ImageFormat {
    match format {
        Format::Jpg => ImageFormat::Jpeg,
        Format::Png => ImageFormat::Png,
        Format::Webp => ImageFormat::WebP,
        Format::Tiff => ImageFormat::Tiff,
        Format::Bmp => ImageFormat::Bmp,
        other => panic!("{other:?}"),
    }
}

fn run(job: &Job) -> (Result<JobOutcome, GrapeError>, Vec<f32>) {
    let mut seen = Vec::new();
    let result = run_job(job, &EngineCtx::default(), &mut |p| seen.push(p));
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
    assert!(seen.iter().all(|p| (0.0..=1.0).contains(p)), "{seen:?}");
    (result, seen)
}

/// A page as lopdf reads it.
struct Page {
    media_box: Vec<f32>,
    image: Dictionary,
    data: Vec<u8>,
    mask: Option<Dictionary>,
}

/// Parses `path`: no Info, no `/ID`, no XMP; its pages.
fn read_pdf(path: &Path) -> (Vec<u8>, Vec<Page>) {
    let bytes = fs::read(path).unwrap();
    assert!(bytes.starts_with(b"%PDF-1.4\n"));
    let doc = Document::load_mem(&bytes).unwrap();
    assert!(doc.trailer.get(b"Info").is_err());
    assert!(doc.trailer.get(b"ID").is_err());
    assert!(doc.catalog().unwrap().get(b"Metadata").is_err());
    let mut pages = Vec::new();
    for (_, id) in doc.get_pages() {
        let page = doc.get_object(id).unwrap().as_dict().unwrap();
        let media_box = page
            .get(b"MediaBox")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_float().unwrap())
            .collect();
        let xobjects = page
            .get(b"Resources")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"XObject")
            .unwrap()
            .as_dict()
            .unwrap();
        let image = doc
            .get_object(xobjects.get(b"Im0").unwrap().as_reference().unwrap())
            .unwrap()
            .as_stream()
            .unwrap();
        let mask = image.dict.get(b"SMask").ok().map(|mask| {
            doc.get_object(mask.as_reference().unwrap())
                .unwrap()
                .as_stream()
                .unwrap()
                .dict
                .clone()
        });
        pages.push(Page {
            media_box,
            image: image.dict.clone(),
            data: image.content.clone(),
            mask,
        });
    }
    (bytes, pages)
}

fn name(dict: &Dictionary, key: &[u8]) -> String {
    String::from_utf8(dict.get(key).unwrap().as_name().unwrap().to_vec()).unwrap()
}

fn int(dict: &Dictionary, key: &[u8]) -> i64 {
    dict.get(key).unwrap().as_i64().unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn every_raster_format_converts_to_a_one_page_pdf() {
    for from in [
        Format::Jpg,
        Format::Png,
        Format::Webp,
        Format::Tiff,
        Format::Bmp,
    ] {
        let dir = TestDir::new("create-each");
        let bytes = encode(&gradient(64, 40), image_format(from));
        let name_in = format!("photo.{}", from.key());
        let (src, job) = plan_one(&dir, &name_in, &bytes, Operation::convert(Format::Pdf));
        let before = SourceState::of(&src);
        assert!(supports_with(
            &job.op,
            &[Some(from)],
            &SidecarPaths::default()
        ));

        let (result, progress) = run(&job);

        let outcome = result.unwrap();
        let dest = dir.join("photo.pdf");
        assert_eq!(outcome.outputs, std::slice::from_ref(&dest), "{from:?}");
        assert!(!outcome.renamed);
        assert!(outcome.notes.is_empty());
        assert_eq!(progress, [0.5, 1.0], "{from:?}");
        let (_, pages) = read_pdf(&dest);
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        assert_eq!(page.media_box, [0.0, 0.0, 64.0, 40.0], "{from:?}");
        assert_eq!(
            (int(&page.image, b"Width"), int(&page.image, b"Height")),
            (64, 40)
        );
        let filter = if from == Format::Jpg {
            "DCTDecode"
        } else {
            "FlateDecode"
        };
        assert_eq!(name(&page.image, b"Filter"), filter, "{from:?}");
        assert_eq!(name(&page.image, b"ColorSpace"), "DeviceRGB");
        if from == Format::Jpg {
            // The original data, byte for byte: the encoder writes nothing
            // the passthrough drops (its JFIF header has no thumbnail).
            assert_eq!(page.data, bytes);
        }
        assert!(page.mask.is_none());
        before.assert_unchanged();
        assert_no_leftovers(&dir.path);
        let mut expected = vec![name_in.clone(), "photo.pdf".to_string()];
        expected.sort();
        assert_eq!(dir.names(), expected);
    }
}

#[test]
fn the_content_decides_how_an_image_is_stored() {
    // A PNG named .jpg is stored losslessly; a JPEG named .png passes
    // through.
    let dir = TestDir::new("create-misnamed");
    let img = gradient(16, 9);
    for (name_in, bytes, filter) in [
        ("png.jpg", encode(&img, ImageFormat::Png), "FlateDecode"),
        ("jpeg.png", encode(&img, ImageFormat::Jpeg), "DCTDecode"),
    ] {
        let (src, job) = plan_one(&dir, name_in, &bytes, Operation::convert(Format::Pdf));
        let before = SourceState::of(&src);
        run(&job).0.unwrap();
        let (_, pages) = read_pdf(&planned_file(&job));
        assert_eq!(name(&pages[0].image, b"Filter"), filter, "{name_in}");
        assert_eq!(pages[0].media_box, [0.0, 0.0, 16.0, 9.0]);
        before.assert_unchanged();
    }
    assert_no_leftovers(&dir.path);
}

/// EXIF: orientation 6 in IFD0 and a GPS IFD with a latitude. Returns the
/// block and where the GPS IFD's bytes are in it.
fn exif_with_gps() -> (Vec<u8>, std::ops::Range<usize>) {
    let mut exif = b"MM\0\x2a".to_vec();
    exif.extend_from_slice(&8u32.to_be_bytes());
    // IFD0 at 8: two entries, then the next-IFD offset.
    exif.extend_from_slice(&2u16.to_be_bytes());
    exif.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0]);
    let gps_at = 8 + 2 + 2 * 12 + 4;
    exif.extend_from_slice(&[0x88, 0x25, 0, 4, 0, 0, 0, 1]);
    exif.extend_from_slice(&u32::try_from(gps_at).unwrap().to_be_bytes());
    exif.extend_from_slice(&0u32.to_be_bytes());
    // The GPS IFD: latitude ref "N" and the latitude, three rationals.
    assert_eq!(exif.len(), gps_at);
    let data_at = gps_at + 2 + 2 * 12 + 4;
    exif.extend_from_slice(&2u16.to_be_bytes());
    exif.extend_from_slice(&[0, 1, 0, 2, 0, 0, 0, 2, b'N', 0, 0, 0]);
    exif.extend_from_slice(&[0, 2, 0, 5, 0, 0, 0, 3]);
    exif.extend_from_slice(&u32::try_from(data_at).unwrap().to_be_bytes());
    exif.extend_from_slice(&0u32.to_be_bytes());
    for (num, den) in [(59u32, 1u32), (19, 1), (424_242, 10_000)] {
        exif.extend_from_slice(&num.to_be_bytes());
        exif.extend_from_slice(&den.to_be_bytes());
    }
    let end = exif.len();
    (exif, gps_at..end)
}

#[test]
fn a_rotated_phone_photo_keeps_its_pixels_and_loses_its_exif() {
    let dir = TestDir::new("create-exif");
    let (exif, gps) = exif_with_gps();
    let img = gradient(40, 20);
    let mut jpeg = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, 90);
    encoder.set_exif_metadata(exif.clone()).unwrap();
    encoder
        .write_image(img.as_bytes(), 40, 20, image::ExtendedColorType::Rgb8)
        .unwrap();
    assert!(contains(&jpeg, b"Exif") && contains(&jpeg, &exif[gps.clone()]));
    let (src, job) = plan_one(&dir, "phone.jpg", &jpeg, Operation::convert(Format::Pdf));
    let before = SourceState::of(&src);

    let (result, _) = run(&job);

    result.unwrap();
    let (pdf, pages) = read_pdf(&dir.join("phone.pdf"));
    // Stored 40×20, shown upright at 20×40.
    assert_eq!(pages[0].media_box, [0.0, 0.0, 20.0, 40.0]);
    assert_eq!(
        (
            int(&pages[0].image, b"Width"),
            int(&pages[0].image, b"Height")
        ),
        (40, 20)
    );
    assert!(!contains(&pdf, b"Exif"));
    assert!(!contains(&pdf, &exif[gps]));
    // The entropy-coded data is the source's, from SOS to the end.
    let sos = jpeg.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
    assert!(pages[0].data.ends_with(&jpeg[sos..]));
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_jpeg_passes_through_in_all_eight_orientations() {
    // Stored 40 × 20. Each EXIF orientation keeps the source's scan data
    // and stored size, and the page's matrix turns it upright.
    let plain = encode(&gradient(40, 20), ImageFormat::Jpeg);
    let sos = plain.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
    for (exif, page, cm) in [
        (1u8, [40.0, 20.0], "40 0 0 20 0 0"),
        (2, [40.0, 20.0], "-40 0 0 20 40 0"),
        (3, [40.0, 20.0], "-40 0 0 -20 40 20"),
        (4, [40.0, 20.0], "40 0 0 -20 0 20"),
        (5, [20.0, 40.0], "0 -40 -20 0 20 40"),
        (6, [20.0, 40.0], "0 -40 20 0 0 40"),
        (7, [20.0, 40.0], "0 40 20 0 0 0"),
        (8, [20.0, 40.0], "0 40 -20 0 20 0"),
    ] {
        let dir = TestDir::new("create-orient");
        let mut payload = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0".to_vec();
        payload.extend_from_slice(&[exif, 0, 0, 0, 0, 0, 0]);
        let len = u16::try_from(payload.len() + 2).unwrap().to_be_bytes();
        let mut jpeg = plain[..2].to_vec();
        jpeg.extend_from_slice(&[0xFF, 0xE1, len[0], len[1]]);
        jpeg.extend(payload);
        jpeg.extend_from_slice(&plain[2..]);
        let (src, job) = plan_one(&dir, "photo.jpg", &jpeg, Operation::convert(Format::Pdf));
        let before = SourceState::of(&src);

        run(&job).0.unwrap();

        let path = planned_file(&job);
        let (_, pages) = read_pdf(&path);
        let image = &pages[0].image;
        assert_eq!(pages[0].media_box, [0.0, 0.0, page[0], page[1]], "{exif}");
        assert_eq!(name(image, b"Filter"), "DCTDecode", "{exif}");
        assert_eq!(
            (int(image, b"Width"), int(image, b"Height")),
            (40, 20),
            "{exif}"
        );
        assert!(
            pages[0].data.ends_with(&plain[sos..]),
            "{exif}: not passed through"
        );
        let doc = Document::load_mem(&fs::read(&path).unwrap()).unwrap();
        let content = doc.get_page_content(doc.get_pages()[&1]);
        assert_eq!(
            String::from_utf8(content).unwrap().trim_end(),
            format!("q {cm} cm /Im0 Do Q"),
            "{exif}"
        );
        before.assert_unchanged();
        assert_no_leftovers(&dir.path);
    }
}

#[test]
fn a_jpeg_of_thousands_of_segments_is_re_encoded() {
    // Grape's decoder takes 2,000 empty DHT segments before the frame; the
    // passthrough reads at most 1,024 segments, so this one is re-encoded.
    let dir = TestDir::new("create-segments");
    let plain = encode(&gradient(24, 16), ImageFormat::Jpeg);
    let mut jpeg = plain[..2].to_vec();
    for _ in 0..2_000 {
        jpeg.extend_from_slice(&[0xFF, 0xC4, 0, 2]);
    }
    jpeg.extend_from_slice(&plain[2..]);
    let (src, job) = plan_one(&dir, "padded.jpg", &jpeg, Operation::convert(Format::Pdf));
    let before = SourceState::of(&src);

    run(&job).0.unwrap();

    let (_, pages) = read_pdf(&planned_file(&job));
    assert_eq!(pages[0].media_box, [0.0, 0.0, 24.0, 16.0]);
    assert_eq!(name(&pages[0].image, b"Filter"), "DCTDecode");
    let sos = jpeg.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
    assert!(!pages[0].data.ends_with(&jpeg[sos..]));
    assert!(!contains(&pages[0].data, &[0xFF, 0xC4, 0, 2]));
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_motion_jpeg_frame_is_re_encoded_with_its_tables() {
    // A webcam frame: an AVI1 APP0 and no DHT, relying on the standard
    // Huffman tables (the encoder's own), which Grape's decoder supplies
    // for it. In the PDF it must carry its tables.
    let dir = TestDir::new("create-mjpeg");
    let plain = encode(&gradient(24, 16), ImageFormat::Jpeg);
    let mut mjpeg = plain[..2].to_vec();
    mjpeg.extend_from_slice(&[0xFF, 0xE0, 0, 14]);
    mjpeg.extend_from_slice(b"AVI1\0\0\0\0\0\0\0\0");
    let mut at = 2;
    while plain[at + 1] != 0xDA {
        let end = at + 2 + usize::from(u16::from_be_bytes([plain[at + 2], plain[at + 3]]));
        if plain[at + 1] != 0xC4 {
            mjpeg.extend_from_slice(&plain[at..end]);
        }
        at = end;
    }
    mjpeg.extend_from_slice(&plain[at..]);
    assert!(!contains(&mjpeg, &[0xFF, 0xC4]));
    assert_eq!(
        image::load_from_memory(&mjpeg).unwrap().to_rgb8(),
        image::load_from_memory(&plain).unwrap().to_rgb8()
    );
    let (src, job) = plan_one(&dir, "frame.jpg", &mjpeg, Operation::convert(Format::Pdf));
    let before = SourceState::of(&src);

    run(&job).0.unwrap();

    let (_, pages) = read_pdf(&planned_file(&job));
    assert_eq!(name(&pages[0].image, b"Filter"), "DCTDecode");
    let data = &pages[0].data;
    assert!(contains(data, &[0xFF, 0xC4]));
    assert!(!contains(data, b"AVI1"));
    // Its own tables: it decodes without the Motion-JPEG defaults.
    let decoded = image::load_from_memory(data).unwrap();
    assert_eq!(decoded.width(), 24);
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn transparency_becomes_a_soft_mask_and_opaque_alpha_does_not() {
    let dir = TestDir::new("create-alpha");
    let clear = RgbaImage::from_fn(10, 6, |x, _| {
        Rgba([200, 30, 30, if x < 5 { 0 } else { 255 }])
    });
    let opaque = RgbaImage::from_pixel(10, 6, Rgba([1, 2, 3, 255]));
    for (stem, img, masked) in [("clear", clear, true), ("opaque", opaque, false)] {
        let bytes = encode(&DynamicImage::ImageRgba8(img), ImageFormat::Png);
        let (src, job) = plan_one(
            &dir,
            &format!("{stem}.png"),
            &bytes,
            Operation::convert(Format::Pdf),
        );
        let before = SourceState::of(&src);
        run(&job).0.unwrap();
        let (_, pages) = read_pdf(&planned_file(&job));
        assert_eq!(pages[0].mask.is_some(), masked, "{stem}");
        if let Some(mask) = &pages[0].mask {
            assert_eq!(name(mask, b"ColorSpace"), "DeviceGray");
            assert_eq!(int(mask, b"BitsPerComponent"), 8);
        }
        before.assert_unchanged();
    }
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_black_and_white_scan_is_one_bit_a_pixel() {
    let dir = TestDir::new("create-bw");
    let text = GrayImage::from_fn(101, 33, |x, y| {
        Luma([if (x / 3 + y / 5) % 2 == 0 { 0 } else { 255 }])
    });
    let gray = GrayImage::from_fn(101, 33, |x, _| Luma([(x * 2) as u8]));
    for (stem, img, bits) in [("text", text, 1), ("gray", gray, 8)] {
        let bytes = encode(&DynamicImage::ImageLuma8(img), ImageFormat::Png);
        let (src, job) = plan_one(
            &dir,
            &format!("{stem}.png"),
            &bytes,
            Operation::convert(Format::Pdf),
        );
        let before = SourceState::of(&src);
        run(&job).0.unwrap();
        let (_, pages) = read_pdf(&planned_file(&job));
        assert_eq!(name(&pages[0].image, b"ColorSpace"), "DeviceGray");
        assert_eq!(int(&pages[0].image, b"BitsPerComponent"), bits, "{stem}");
        before.assert_unchanged();
    }
    assert_no_leftovers(&dir.path);
}

/// Writes `files` into `dir` and plans Create PDF for them, in this order.
fn plan_create(dir: &TestDir, files: &[(&str, Vec<u8>)]) -> (Vec<PathBuf>, Job) {
    let mut paths = Vec::new();
    for (name, bytes) in files {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        paths.push(path);
    }
    let settings = Settings::new(dir.join("fallback"));
    let selection = build_selection(&paths, &settings);
    let mut jobs = plan(&selection, &Operation::tool(ToolKey::CreatePdf), &settings)
        .unwrap()
        .jobs;
    assert_eq!(jobs.len(), 1);
    (paths, jobs.remove(0))
}

/// A JPG, a PNG and a TIFF of different sizes. (BMP has no tools in
/// FORMAT_MATRIX, so Create PDF is never planned with one.)
fn three_images() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("a.jpg", encode(&gradient(30, 20), ImageFormat::Jpeg)),
        ("b.png", encode(&gradient(12, 34), ImageFormat::Png)),
        ("c.tiff", encode(&gradient(50, 5), ImageFormat::Tiff)),
    ]
}

#[test]
fn create_pdf_makes_one_page_per_image_in_order() {
    let dir = TestDir::new("create-three");
    let (paths, job) = plan_create(&dir, &three_images());
    let sources: Vec<SourceState> = paths.iter().map(|p| SourceState::of(p)).collect();
    let dest = dir.join("a Combined.pdf");
    assert_eq!(planned_file(&job), dest);
    let formats: Vec<Option<Format>> =
        vec![Some(Format::Jpg), Some(Format::Png), Some(Format::Tiff)];
    assert!(supports_with(&job.op, &formats, &SidecarPaths::default()));

    let (result, progress) = run(&job);

    let outcome = result.unwrap();
    assert_eq!(outcome.outputs, std::slice::from_ref(&dest));
    assert!(!outcome.renamed);
    let expected = [1.0 / 6.0, 1.0 / 3.0, 0.5, 2.0 / 3.0, 5.0 / 6.0, 1.0];
    assert_eq!(progress.len(), expected.len(), "{progress:?}");
    for (seen, want) in progress.iter().zip(expected) {
        assert!((seen - want).abs() < 1e-6, "{progress:?}");
    }
    let (_, pages) = read_pdf(&dest);
    let sizes: Vec<Vec<f32>> = pages.iter().map(|p| p.media_box.clone()).collect();
    assert_eq!(
        sizes,
        [
            vec![0.0, 0.0, 30.0, 20.0],
            vec![0.0, 0.0, 12.0, 34.0],
            vec![0.0, 0.0, 50.0, 5.0]
        ]
    );
    assert_eq!(name(&pages[0].image, b"Filter"), "DCTDecode");
    assert_eq!(name(&pages[1].image, b"Filter"), "FlateDecode");
    assert_eq!(name(&pages[2].image, b"Filter"), "FlateDecode");
    for source in &sources {
        source.assert_unchanged();
    }
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_name_taken_after_planning_gets_the_next_number() {
    let dir = TestDir::new("create-taken");
    let (paths, job) = plan_create(&dir, &three_images());
    let sources: Vec<SourceState> = paths.iter().map(|p| SourceState::of(p)).collect();
    let taken = dir.join("a Combined.pdf");
    fs::write(&taken, b"someone else's").unwrap();

    let outcome = run(&job).0.unwrap();

    let renamed = dir.join("a Combined 2.pdf");
    assert_eq!(outcome.outputs, std::slice::from_ref(&renamed));
    assert!(outcome.renamed);
    assert_eq!(fs::read(&taken).unwrap(), b"someone else's");
    assert_eq!(read_pdf(&renamed).1.len(), 3);
    for source in &sources {
        source.assert_unchanged();
    }
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_damaged_image_in_the_middle_stops_the_whole_pdf() {
    let dir = TestDir::new("create-damaged");
    let mut files = three_images();
    files[1].1 = b"\x89PNG\r\n\x1a\nnot really".to_vec();
    let (paths, job) = plan_create(&dir, &files);
    let sources: Vec<SourceState> = paths.iter().map(|p| SourceState::of(p)).collect();
    let names_before = dir.names();

    let err = run(&job).0.unwrap_err();

    assert_eq!(err.kind, ErrorKind::DecodeFailed);
    assert_eq!(err.user_message, "b.png isn't a readable PNG image.");
    assert_eq!(dir.names(), names_before);
    for source in &sources {
        source.assert_unchanged();
    }
}

#[test]
fn a_missing_image_is_named() {
    let dir = TestDir::new("create-missing");
    let (paths, job) = plan_create(&dir, &three_images());
    fs::remove_file(&paths[2]).unwrap();
    let sources: Vec<SourceState> = paths[..2].iter().map(|p| SourceState::of(p)).collect();
    let err = run(&job).0.unwrap_err();
    assert_eq!(err.kind, ErrorKind::ReadFailed);
    assert_eq!(err.user_message, "Grape couldn't read c.tiff.");
    assert_eq!(dir.names(), ["a.jpg", "b.png"]);
    for source in &sources {
        source.assert_unchanged();
    }
}

#[test]
fn a_cancelled_job_writes_nothing() {
    let dir = TestDir::new("create-cancel");
    let (paths, job) = plan_create(&dir, &three_images());
    let sources: Vec<SourceState> = paths.iter().map(|p| SourceState::of(p)).collect();
    let names_before = dir.names();
    let ctx = EngineCtx::default();
    ctx.cancel.cancel();
    let err = run_job(&job, &ctx, &mut |_| {}).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(err.user_message, "Cancelled. Nothing was saved.");
    assert_eq!(dir.names(), names_before);

    // Cancelled once the first page is written: nothing is left either.
    let ctx = EngineCtx::default();
    let token = ctx.cancel.clone();
    let err = run_job(&job, &ctx, &mut |p| {
        if p >= 1.0 / 3.0 - 1e-6 {
            token.cancel();
        }
    })
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(dir.names(), names_before);
    for source in &sources {
        source.assert_unchanged();
    }
}

#[test]
fn heic_needs_the_helper_even_in_a_mix() {
    let dir = TestDir::new("create-heic");
    let mut files = three_images();
    files.push(("d.heic", b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec()));
    let (paths, job) = plan_create(&dir, &files);
    let sources: Vec<SourceState> = paths.iter().map(|p| SourceState::of(p)).collect();
    let formats: Vec<Option<Format>> = [Format::Jpg, Format::Png, Format::Tiff, Format::Heic]
        .into_iter()
        .map(Some)
        .collect();
    assert!(!supports_with(&job.op, &formats, &SidecarPaths::default()));
    let with_heif = SidecarPaths {
        heif: Some(PathBuf::from("grape-heif")),
        ..SidecarPaths::default()
    };
    assert!(supports_with(&job.op, &formats, &with_heif));
    let names_before = dir.names();
    let err = run(&job).0.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported);
    assert_eq!(dir.names(), names_before);

    let (src, job) = plan_one(
        &dir,
        "e.heic",
        b"\0\0\0\x18ftypheic\0\0\0\0mif1heic",
        Operation::convert(Format::Pdf),
    );
    let before = SourceState::of(&src);
    let err = run(&job).0.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported);
    assert_eq!(err.user_message, "HEIC to PDF isn't in this build yet.");
    before.assert_unchanged();
    for source in &sources {
        source.assert_unchanged();
    }
    assert_no_leftovers(&dir.path);
}

#[test]
fn which_images_go_to_pdf_and_into_create_pdf() {
    let with_heif = SidecarPaths {
        heif: Some(PathBuf::from("grape-heif")),
        ..SidecarPaths::default()
    };
    let to_pdf = Operation::convert(Format::Pdf);
    let create = Operation::tool(ToolKey::CreatePdf);
    // Over every format: images → PDF and Create PDF need no sidecar for
    // these, and HEIC and AVIF only Grape's image helper. BMP and AVIF
    // convert to PDF, but FORMAT_MATRIX gives them no tools, so Create PDF
    // takes neither (D41: AVIF → PDF is a conversion only).
    let to_pdf_from = |sidecars: &SidecarPaths| -> Vec<Format> {
        Format::ALL
            .into_iter()
            .filter(|&from| supports_with(&to_pdf, &[Some(from)], sidecars))
            .collect()
    };
    let combines = |sidecars: &SidecarPaths| -> Vec<Format> {
        Format::ALL
            .into_iter()
            .filter(|&from| supports_with(&create, &[Some(from)], sidecars))
            .collect()
    };
    let raster = [
        Format::Jpg,
        Format::Png,
        Format::Webp,
        Format::Tiff,
        Format::Bmp,
    ];
    let mut expected = raster.to_vec();
    assert_eq!(
        sorted(to_pdf_from(&SidecarPaths::default())),
        sorted(expected.clone())
    );
    expected.extend([Format::Heic, Format::Avif]);
    assert_eq!(sorted(to_pdf_from(&with_heif)), sorted(expected));
    let mut expected = vec![Format::Jpg, Format::Png, Format::Webp, Format::Tiff];
    assert_eq!(
        sorted(combines(&SidecarPaths::default())),
        sorted(expected.clone())
    );
    expected.push(Format::Heic);
    assert_eq!(sorted(combines(&with_heif)), sorted(expected));
    for from in raster {
        assert!(supports(&to_pdf, Some(from)), "{from:?}");
    }
    assert!(supports(&create, Some(Format::Png)));
    assert!(!supports(&create, Some(Format::Bmp)));
    assert!(!supports(&create, Some(Format::Heic)));
    assert!(!supports(&to_pdf, Some(Format::Avif)));
    assert!(!supports_with(
        &create,
        &[Some(Format::Jpg), Some(Format::Bmp)],
        &with_heif
    ));
    assert!(!supports_with(&create, &[Some(Format::Avif)], &with_heif));
    assert!(!supports_with(
        &create,
        &[Some(Format::Jpg), Some(Format::Avif)],
        &with_heif
    ));
}

fn sorted(mut formats: Vec<Format>) -> Vec<Format> {
    formats.sort_by_key(|format| format.key());
    formats
}
