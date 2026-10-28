//! Raster conversions end to end: plan with `grape_core`, run with
//! `run_job`, check the output and that the source never changed
//! (`docs/design/m1-drag-wheel.md` §E.4).
#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::io::Cursor;
use std::path::Path;

use common::{
    assert_no_leftovers, content_digest, names_in, plan_one, planned_file, SourceState, TestDir,
};
use grape_core::{
    plan, DirSnapshot, Format, Job, JobOutput, Operation, OutputLocation, Selection, SelectionItem,
    Settings, ToolKey,
};
use grape_engines::{build_selection, run_job, EngineCtx, ErrorKind, JobOutcome};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{
    DynamicImage, GenericImageView, GrayImage, ImageDecoder, ImageEncoder, ImageFormat,
    ImageReader, Rgb, RgbImage, Rgba, RgbaImage,
};

const RASTER: [Format; 5] = [
    Format::Jpg,
    Format::Png,
    Format::Webp,
    Format::Tiff,
    Format::Bmp,
];

fn image_format(f: Format) -> ImageFormat {
    match f {
        Format::Jpg => ImageFormat::Jpeg,
        Format::Png => ImageFormat::Png,
        Format::Webp => ImageFormat::WebP,
        Format::Tiff => ImageFormat::Tiff,
        Format::Bmp => ImageFormat::Bmp,
        other => panic!("not a raster format: {other:?}"),
    }
}

/// A 64×48 RGBA gradient with partial alpha (64..=221, never 0).
fn gradient() -> DynamicImage {
    DynamicImage::ImageRgba8(RgbaImage::from_fn(64, 48, |x, y| {
        Rgba([(x * 4) as u8, (y * 5) as u8, 128, (64 + x * 2 + y) as u8])
    }))
}

/// `img` encoded as `f` by the `image` crate itself (JPEG drops alpha).
fn encode(img: &DynamicImage, f: Format) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, image_format(f)).unwrap();
    buf.into_inner()
}

fn run(job: &Job) -> Result<JobOutcome, grape_engines::GrapeError> {
    run_job(job, &EngineCtx::default(), &mut |_| {})
}

fn decode(bytes: &[u8]) -> DynamicImage {
    image::load_from_memory(bytes).unwrap()
}

#[test]
fn all_twenty_raster_pairs_convert_next_to_the_source() {
    let source_img = gradient();
    let mut converted = 0;
    for from in RASTER {
        let source_bytes = encode(&source_img, from);
        let decoded_source = decode(&source_bytes);
        for to in RASTER {
            if from == to {
                continue;
            }
            let dir = TestDir::new("pairs");
            let name = format!("photo.{}", from.key());
            let (src, job) = plan_one(&dir, &name, &source_bytes, Operation::convert(to));
            let planned = planned_file(&job);
            assert_eq!(planned, dir.join(&format!("photo.{}", to.key())));
            assert!(!job.used_fallback);
            let before = SourceState::of(&src);

            let mut fractions = Vec::new();
            let outcome = run_job(&job, &EngineCtx::default(), &mut |f| fractions.push(f))
                .unwrap_or_else(|e| panic!("{from:?} -> {to:?}: {e} ({})", e.detail));

            assert_eq!(
                outcome,
                JobOutcome {
                    outputs: vec![planned.clone()],
                    renamed: false,
                    notes: Vec::new(),
                },
                "{from:?} -> {to:?}"
            );
            let bytes = fs::read(&planned).unwrap();
            assert_eq!(
                image::guess_format(&bytes).unwrap(),
                image_format(to),
                "{from:?} -> {to:?}: magic bytes"
            );
            let out = decode(&bytes);
            assert_eq!(out.dimensions(), (64, 48), "{from:?} -> {to:?}");
            if to != Format::Jpg {
                // Every target but JPEG is lossless: the pixels survive.
                assert_eq!(
                    out.to_rgba8(),
                    decoded_source.to_rgba8(),
                    "{from:?} -> {to:?}: pixels"
                );
            }
            assert_eq!(fractions.last(), Some(&1.0));
            assert!(fractions.iter().all(|f| (0.0..=1.0).contains(f)));
            before.assert_unchanged();
            assert_no_leftovers(&dir.path);
            let mut expected = vec![name.clone(), format!("photo.{}", to.key())];
            expected.sort();
            let written: Vec<String> = dir
                .names()
                .into_iter()
                .filter(|n| n != "fallback")
                .collect();
            assert_eq!(written, expected, "{from:?} -> {to:?}");
            converted += 1;
        }
    }
    assert_eq!(converted, 20);
}

#[test]
fn a_name_taken_after_planning_gets_the_next_number() {
    let dir = TestDir::new("collision");
    let (src, job) = plan_one(
        &dir,
        "photo.png",
        &encode(&gradient(), Format::Png),
        Operation::convert(Format::Jpg),
    );
    let planned = planned_file(&job);
    assert_eq!(planned, dir.join("photo.jpg"));
    // Someone else saves photo.jpg between planning and finalizing.
    fs::write(&planned, b"not ours").unwrap();
    let before = SourceState::of(&src);

    let outcome = run(&job).unwrap();

    assert_eq!(
        outcome,
        JobOutcome {
            outputs: vec![dir.join("photo 2.jpg")],
            renamed: true,
            notes: Vec::new(),
        }
    );
    assert_eq!(fs::read(&planned).unwrap(), b"not ours");
    let bytes = fs::read(dir.join("photo 2.jpg")).unwrap();
    assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::Jpeg);
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_case_variant_taken_after_planning_is_never_replaced() {
    let dir = TestDir::new("collision-case");
    let (src, job) = plan_one(
        &dir,
        "photo.png",
        &encode(&gradient(), Format::Png),
        Operation::convert(Format::Webp),
    );
    fs::write(dir.join("PHOTO.webp"), b"not ours").unwrap();
    let before = SourceState::of(&src);

    let outcome = run(&job).unwrap();

    before.assert_unchanged();
    assert_eq!(fs::read(dir.join("PHOTO.webp")).unwrap(), b"not ours");
    // A case-sensitive filesystem (Linux) has room for `photo.webp`; a
    // case-insensitive one (NTFS, APFS) refuses it and the name is re-planned.
    let out = &outcome.outputs[0];
    let same_name = *out == dir.join("photo.webp") && !outcome.renamed;
    let next_name = *out == dir.join("photo 2.webp") && outcome.renamed;
    assert!(same_name || next_name, "{outcome:?}");
    let bytes = fs::read(out).unwrap();
    assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::WebP);
    assert_no_leftovers(&dir.path);
}

/// A big-endian EXIF (TIFF) block with one entry: Orientation = 6
/// (rotate 90° clockwise to display).
fn exif_orientation_6() -> Vec<u8> {
    let mut exif = b"MM\0\x2a".to_vec();
    exif.extend_from_slice(&8u32.to_be_bytes()); // first IFD offset
    exif.extend_from_slice(&1u16.to_be_bytes()); // one entry
    exif.extend_from_slice(&0x0112u16.to_be_bytes()); // Orientation
    exif.extend_from_slice(&3u16.to_be_bytes()); // SHORT
    exif.extend_from_slice(&1u32.to_be_bytes()); // count
    exif.extend_from_slice(&[0, 6, 0, 0]); // value 6, padded
    exif.extend_from_slice(&0u32.to_be_bytes()); // no next IFD
    exif
}

#[test]
fn exif_orientation_is_applied() {
    // 40×20: the left half red, the right half blue.
    let img = RgbImage::from_fn(40, 20, |x, _| {
        if x < 20 {
            Rgb([255, 0, 0])
        } else {
            Rgb([0, 0, 255])
        }
    });
    let mut bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut bytes, 95);
    encoder.set_exif_metadata(exif_orientation_6()).unwrap();
    DynamicImage::ImageRgb8(img)
        .write_with_encoder(encoder)
        .unwrap();

    let dir = TestDir::new("orientation");
    let (src, job) = plan_one(&dir, "phone.jpg", &bytes, Operation::convert(Format::Png));
    let before = SourceState::of(&src);
    let outcome = run(&job).unwrap();

    let out = decode(&fs::read(&outcome.outputs[0]).unwrap()).to_rgb8();
    assert_eq!(out.dimensions(), (20, 40));
    // Rotated clockwise: the left (red) half is now on top.
    let top = out.get_pixel(10, 5).0;
    let bottom = out.get_pixel(10, 35).0;
    assert!(top[0] > 200 && top[2] < 60, "top {top:?}");
    assert!(bottom[2] > 200 && bottom[0] < 60, "bottom {bottom:?}");
    before.assert_unchanged();
}

#[test]
fn jpeg_output_flattens_alpha_onto_white() {
    // Left half fully transparent black, right half opaque green.
    let img = DynamicImage::ImageRgba8(RgbaImage::from_fn(32, 32, |x, _| {
        if x < 16 {
            Rgba([0, 0, 0, 0])
        } else {
            Rgba([0, 200, 0, 255])
        }
    }));
    let dir = TestDir::new("alpha");
    let (src, job) = plan_one(
        &dir,
        "logo.png",
        &encode(&img, Format::Png),
        Operation::convert(Format::Jpg),
    );
    let before = SourceState::of(&src);
    let outcome = run(&job).unwrap();

    let out = decode(&fs::read(&outcome.outputs[0]).unwrap()).to_rgb8();
    let clear = out.get_pixel(4, 16).0;
    let solid = out.get_pixel(28, 16).0;
    assert!(
        clear.iter().all(|&c| c >= 245),
        "transparent area {clear:?}"
    );
    assert!(solid[1] > 150 && solid[0] < 60, "opaque area {solid:?}");
    before.assert_unchanged();
}

/// A minimal RGB profile header padded with zeros to `len` bytes; the
/// codecs embed it as opaque bytes.
fn rgb_profile(len: usize) -> Vec<u8> {
    let mut profile = vec![0u8; len];
    profile[..4].copy_from_slice(&u32::try_from(len).unwrap().to_be_bytes());
    profile[16..20].copy_from_slice(b"RGB ");
    profile[20..24].copy_from_slice(b"XYZ ");
    profile[36..40].copy_from_slice(b"acsp");
    profile
}

/// The gradient as a PNG that embeds `profile`.
fn png_with_profile(profile: &[u8]) -> Vec<u8> {
    let mut source = Vec::new();
    let mut encoder = PngEncoder::new(&mut source);
    encoder.set_icc_profile(profile.to_vec()).unwrap();
    gradient().write_with_encoder(encoder).unwrap();
    source
}

/// The ICC profile embedded in an encoded image, if any.
fn embedded_profile(bytes: Vec<u8>) -> Option<Vec<u8>> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .unwrap()
        .into_decoder()
        .unwrap()
        .icc_profile()
        .unwrap()
}

#[test]
fn icc_profiles_are_carried_over_where_the_target_can_hold_them() {
    let profile = rgb_profile(132);
    let source = png_with_profile(&profile);

    for to in [Format::Jpg, Format::Webp, Format::Tiff, Format::Bmp] {
        let dir = TestDir::new("icc");
        let (src, job) = plan_one(&dir, "photo.png", &source, Operation::convert(to));
        let before = SourceState::of(&src);
        let outcome = run(&job).unwrap();
        before.assert_unchanged();
        let carried = embedded_profile(fs::read(&outcome.outputs[0]).unwrap());
        if to == Format::Bmp {
            assert_eq!(carried, None, "BMP can't hold a profile");
        } else {
            assert_eq!(carried.as_deref(), Some(&profile[..]), "{to:?}");
        }
    }
}

#[test]
fn a_profile_over_the_size_cap_is_left_out() {
    // Over the 255 × 65,519 bytes a JPEG can hold, the cap for every target
    // (the zeros compress well in the PNG). Then one that would inflate
    // past the PNG decoder's 128 MiB limit: it is dropped while the PNG is
    // read, and the image still converts.
    for len in [17 * 1024 * 1024, 136 * 1024 * 1024] {
        let source = png_with_profile(&rgb_profile(len));
        for to in [Format::Jpg, Format::Webp, Format::Tiff] {
            let dir = TestDir::new("icc-large");
            let (src, job) = plan_one(&dir, "wide.png", &source, Operation::convert(to));
            let before = SourceState::of(&src);

            let outcome = run(&job).unwrap_or_else(|e| panic!("{e} ({})", e.detail));

            let bytes = fs::read(&outcome.outputs[0]).unwrap();
            assert_eq!(image::guess_format(&bytes).unwrap(), image_format(to));
            assert_eq!(embedded_profile(bytes), None, "{len} bytes to {to:?}");
            before.assert_unchanged();
            assert_no_leftovers(&dir.path);
        }
    }
}

#[test]
fn corrupt_input_is_a_decode_error_and_leaves_nothing() {
    let mut seed: u32 = 0x1234_5678;
    let junk: Vec<u8> = (0..512)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        })
        .collect();
    let dir = TestDir::new("corrupt");
    let (src, job) = plan_one(&dir, "broken.png", &junk, Operation::convert(Format::Jpg));
    let before = SourceState::of(&src);
    let names_before = dir.names();

    let err = run(&job).unwrap_err();

    assert_eq!(err.kind, ErrorKind::DecodeFailed);
    assert_eq!(err.user_message, "broken.png isn't a readable PNG image.");
    assert_eq!(err.to_string(), err.user_message);
    assert!(err.detail.contains("broken.png"), "{}", err.detail);
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}

/// CRC-32 (IEEE 802.3), for patching and writing PNG chunks.
fn crc32(bytes: &[u8]) -> u32 {
    let table: Vec<u32> = (0..256u32)
        .map(|n| {
            (0..8).fold(n, |c, _| {
                if c & 1 == 1 {
                    (c >> 1) ^ 0xEDB8_8320
                } else {
                    c >> 1
                }
            })
        })
        .collect();
    !bytes.iter().fold(0xFFFF_FFFFu32, |crc, &byte| {
        table[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8)
    })
}

/// `img` as a small PNG whose header claims `width`×`height`, with the
/// header's CRC fixed: a decompression bomb.
fn png_claiming(img: &DynamicImage, width: u32, height: u32) -> Vec<u8> {
    claiming(encode(img, Format::Png), width, height)
}

/// `png` with its header changed to claim `width`×`height`.
fn claiming(mut png: Vec<u8>, width: u32, height: u32) -> Vec<u8> {
    // Signature (8), length (4), "IHDR", width, height, 5 more bytes, CRC.
    assert_eq!(&png[12..16], b"IHDR");
    png[16..20].copy_from_slice(&width.to_be_bytes());
    png[20..24].copy_from_slice(&height.to_be_bytes());
    let crc = crc32(&png[12..29]);
    png[29..33].copy_from_slice(&crc.to_be_bytes());
    png
}

/// A small JPEG whose frame header claims `width`×`height`.
fn jpeg_claiming(width: u16, height: u16) -> Vec<u8> {
    claiming_jpeg(
        encode(&DynamicImage::ImageRgb8(RgbImage::new(16, 16)), Format::Jpg),
        width,
        height,
    )
}

/// `jpeg` with its frame header changed to claim `width`×`height`.
fn claiming_jpeg(mut jpeg: Vec<u8>, width: u16, height: u16) -> Vec<u8> {
    // SOF0: FF C0, length (2), precision (1), height (2), width (2).
    let sof = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    jpeg[sof + 5..sof + 7].copy_from_slice(&height.to_be_bytes());
    jpeg[sof + 7..sof + 9].copy_from_slice(&width.to_be_bytes());
    jpeg
}

/// A small BMP whose info header claims `width`×`height`.
fn bmp_claiming(width: i32, height: i32) -> Vec<u8> {
    let mut bmp = encode(&DynamicImage::ImageRgb8(RgbImage::new(16, 16)), Format::Bmp);
    bmp[18..22].copy_from_slice(&width.to_le_bytes());
    bmp[22..26].copy_from_slice(&height.to_le_bytes());
    bmp
}

#[test]
fn an_image_too_large_to_convert_is_refused_before_decoding() {
    let rgb = DynamicImage::ImageRgb8(RgbImage::new(16, 16));
    let gray = DynamicImage::ImageLuma8(GrayImage::new(16, 16));
    let over_working_set = "over the 1024 MiB limit";
    let over_decode_limit = "Memory limit exceeded";
    let cases = [
        // Hundreds of gigabytes claimed by a few hundred bytes.
        (
            "bomb.png",
            png_claiming(&rgb, 200_000, 200_000),
            Format::Bmp,
            over_working_set,
        ),
        (
            "bomb.jpg",
            jpeg_claiming(65_000, 65_000),
            Format::Png,
            over_working_set,
        ),
        (
            "bomb.bmp",
            bmp_claiming(40_000, 40_000),
            Format::Tiff,
            over_working_set,
        ),
        // 588 MB of RGB kept as is: over the 512 MiB decode limit.
        (
            "big.png",
            png_claiming(&rgb, 14_000, 14_000),
            Format::Tiff,
            over_decode_limit,
        ),
        // 400 MB of gray fits the decode limit, but BMP's RGB copy takes
        // the conversion past the working-set limit.
        (
            "gray.png",
            png_claiming(&gray, 20_000, 20_000),
            Format::Bmp,
            over_working_set,
        ),
    ];
    for (name, bytes, to, why) in cases {
        let dir = TestDir::new("too-large");
        let (src, job) = plan_one(&dir, name, &bytes, Operation::convert(to));
        let before = SourceState::of(&src);
        let names_before = dir.names();

        let err = run(&job).unwrap_err();

        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{name}: {}", err.detail);
        assert_eq!(err.user_message, format!("{name} is too large to convert."));
        assert!(err.detail.contains(name), "{}", err.detail);
        assert!(err.detail.contains(why), "{}", err.detail);
        assert_eq!(dir.names(), names_before, "{name}");
        before.assert_unchanged();
    }
}

/// Extends the file at `path` with zeros to `len` bytes (sparse where the
/// file system allows), after its image data.
fn pad_to(path: &Path, len: u64) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
}

#[test]
fn a_jpeg_or_webp_file_over_the_size_limit_is_refused_before_reading() {
    let rgb = DynamicImage::ImageRgb8(RgbImage::new(16, 16));
    for (name, f) in [("huge.jpg", Format::Jpg), ("huge.webp", Format::Webp)] {
        let dir = TestDir::new("huge-file");
        let (src, job) = plan_one(
            &dir,
            name,
            &encode(&rgb, f),
            Operation::convert(Format::Bmp),
        );
        pad_to(&src, 512 * 1024 * 1024 + 1);
        let before = SourceState::of(&src);
        let names_before = dir.names();

        let err = run(&job).unwrap_err();

        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
        assert_eq!(err.user_message, format!("{name} is too large to convert."));
        assert!(
            err.detail
                .contains("over the 512 MiB limit for a JPEG or WebP source"),
            "{}",
            err.detail
        );
        assert_eq!(dir.names(), names_before);
        before.assert_unchanged();
    }
}

#[test]
fn png_and_tiff_files_of_any_size_convert_their_first_image() {
    // Their decoders read as they go, so a file that is mostly other layers
    // or pages (here padding) converts like a small one.
    let rgb = DynamicImage::ImageRgb8(RgbImage::new(16, 16));
    for (name, f) in [("layered.tiff", Format::Tiff), ("long.png", Format::Png)] {
        let dir = TestDir::new("big-file");
        let (src, job) = plan_one(
            &dir,
            name,
            &encode(&rgb, f),
            Operation::convert(Format::Bmp),
        );
        pad_to(&src, 512 * 1024 * 1024 + 1);
        let before = SourceState::of(&src);

        let outcome = run(&job).unwrap_or_else(|e| panic!("{name}: {e} ({})", e.detail));

        let output = decode(&fs::read(&outcome.outputs[0]).unwrap());
        assert_eq!(output.dimensions(), (16, 16));
        before.assert_unchanged();
        assert_no_leftovers(&dir.path);
    }
}

/// `jpeg` with its frame header marked progressive (SOF2), so its headers
/// say the decoder keeps coefficients for the whole image. (The scan data
/// stays sequential; it is refused before anyone tries to decode it.)
fn progressive(mut jpeg: Vec<u8>) -> Vec<u8> {
    let sof = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    jpeg[sof + 1] = 0xC2;
    jpeg
}

#[test]
fn a_jpeg_file_counts_toward_the_working_set() {
    // A progressive 10,901² RGB frame: 340 MiB decoded plus 680 MiB of
    // coefficients is 1,020 MiB, just under the 1 GiB limit. The JPEG
    // decoder also holds the whole file, here 8 MiB, and its copies of the
    // file's metadata, counted as much again, which tips it over.
    let dir = TestDir::new("jpeg-held");
    let (src, job) = plan_one(
        &dir,
        "tall.jpg",
        &progressive(jpeg_claiming(10_901, 10_901)),
        Operation::convert(Format::Bmp),
    );
    pad_to(&src, 8 * 1024 * 1024);
    let before = SourceState::of(&src);
    let names_before = dir.names();

    let err = run(&job).unwrap_err();

    assert_eq!(err.user_message, "tall.jpg is too large to convert.");
    assert!(
        err.detail
            .contains("needs about 1035 MiB, over the 1024 MiB limit"),
        "{}",
        err.detail
    );
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}

/// `jpeg` with `count` extended-XMP segments of `size` data bytes each
/// right after SOI, laid out as one XMP packet `total` bytes long.
fn with_extended_xmp(jpeg: &[u8], count: u32, size: u16, total: u32) -> Vec<u8> {
    let mut segments = Vec::new();
    for i in 0..count {
        let mut data = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
        data.extend_from_slice(b"0123456789ABCDEF0123456789ABCDEF");
        data.extend(total.to_be_bytes());
        data.extend((i * u32::from(size)).to_be_bytes());
        data.resize(data.len() + usize::from(size), b'x');
        segments.extend([0xFF, 0xE1]);
        segments.extend(u16::try_from(data.len() + 2).unwrap().to_be_bytes());
        segments.extend(data);
    }
    [&jpeg[..2], &segments, &jpeg[2..]].concat()
}

#[test]
fn a_jpegs_extended_xmp_counts_toward_the_working_set() {
    // The decoder copies each extended-XMP segment and then reassembles
    // them: up to as much again as the file, on top of the file and its
    // other metadata copies. A progressive 18,836² gray frame (kept gray
    // in a TIFF) and 64 segments of 65,000 bytes fit the 1 GiB limit
    // without that, and not with it.
    let gray = encode(
        &DynamicImage::ImageLuma8(GrayImage::new(16, 16)),
        Format::Jpg,
    );
    let jpeg = with_extended_xmp(
        &progressive(claiming_jpeg(gray, 18_836, 18_836)),
        64,
        65_000,
        64 * 65_000,
    );
    let mib = 1024 * 1024;
    let len = jpeg.len() as u64;
    // Decoded, and 2 bytes per pixel of coefficients.
    let pixels = 18_836u64 * 18_836;
    let without = 3 * pixels + 2 * len;
    assert!(without <= 1024 * mib, "{}", without / mib);
    assert_too_large_as(
        "scan.jpg",
        &jpeg,
        Format::Tiff,
        &format!(
            "needs about {} MiB, over the 1024 MiB limit",
            (without + len) / mib
        ),
    );
}

#[test]
fn a_jpeg_whose_header_parse_would_take_too_long_is_refused() {
    // 40,000 one-byte extended-XMP segments that never add up to the
    // packet's size: the decoder would re-check every pending one after
    // every marker, for minutes. Refused before any decoder parses it.
    let gray = encode(
        &DynamicImage::ImageLuma8(GrayImage::new(16, 16)),
        Format::Jpg,
    );
    assert_too_large(
        "slow.jpg",
        &with_extended_xmp(&gray, 40_000, 1, u32::MAX),
        "its 40000 extended XMP segments would take too long to parse",
    );
}

#[test]
fn the_profile_carried_over_counts_toward_the_working_set() {
    // 16,288² gray to BMP is estimated at 1,012 MiB: 1 byte decoded and 3
    // converted per pixel. A 15 MiB ICC profile, held from decoding to
    // encoding, takes it past 1 GiB.
    let mut png = Vec::new();
    let mut encoder = PngEncoder::new(&mut png);
    encoder
        .set_icc_profile(rgb_profile(15 * 1024 * 1024))
        .unwrap();
    DynamicImage::ImageLuma8(GrayImage::new(16, 16))
        .write_with_encoder(encoder)
        .unwrap();
    let dir = TestDir::new("icc-held");
    let (src, job) = plan_one(
        &dir,
        "scan.png",
        &claiming(png, 16_288, 16_288),
        Operation::convert(Format::Bmp),
    );
    let before = SourceState::of(&src);
    let names_before = dir.names();

    let err = run(&job).unwrap_err();

    assert_eq!(err.user_message, "scan.png is too large to convert.");
    assert!(
        err.detail
            .contains("needs about 1027 MiB, over the 1024 MiB limit"),
        "{}",
        err.detail
    );
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}

/// A RIFF chunk: tag, little-endian length, data, padded to even length.
fn riff_chunk(tag: &[u8], data: &[u8]) -> Vec<u8> {
    let mut chunk = tag.to_vec();
    chunk.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
    chunk.extend_from_slice(data);
    if data.len() % 2 == 1 {
        chunk.push(0);
    }
    chunk
}

/// An animated WebP with a `width`×`height` RGBA canvas and one small
/// lossless frame: a few kilobytes that claim a huge canvas.
fn animated_webp(width: u32, height: u32) -> Vec<u8> {
    let still = encode(&gradient(), Format::Webp);
    assert_eq!(&still[12..16], b"VP8L");
    let u24 = |v: u32| v.to_le_bytes()[..3].to_vec();
    // Flags: alpha (0x10) and animation (0x02).
    let mut vp8x = vec![0x12, 0, 0, 0];
    vp8x.extend(u24(width - 1));
    vp8x.extend(u24(height - 1));
    // The 64×48 frame at (0, 0), shown for 100 ms.
    let mut anmf = [u24(0), u24(0), u24(63), u24(47), u24(100)].concat();
    anmf.push(0);
    anmf.extend_from_slice(&still[12..]);
    let body = [
        b"WEBP".to_vec(),
        riff_chunk(b"VP8X", &vp8x),
        riff_chunk(b"ANIM", &[0; 6]),
        riff_chunk(b"ANMF", &anmf),
    ]
    .concat();
    riff_chunk(b"RIFF", &body)
}

/// A still extended WebP with alpha on a `width`×`height` canvas. A
/// `lossy` one carries `ALPH` and `VP8 ` chunks (whose data is never read:
/// it is refused before decoding); otherwise the small lossless gradient.
fn still_webp_with_alpha(width: u32, height: u32, lossy: bool) -> Vec<u8> {
    let simple = encode(&gradient(), Format::Webp);
    assert_eq!(&simple[12..16], b"VP8L");
    let u24 = |v: u32| v.to_le_bytes()[..3].to_vec();
    // Flags: alpha (0x10).
    let mut vp8x = vec![0x10, 0, 0, 0];
    vp8x.extend(u24(width - 1));
    vp8x.extend(u24(height - 1));
    let image = if lossy {
        [riff_chunk(b"ALPH", &[0; 4]), riff_chunk(b"VP8 ", &[0; 10])].concat()
    } else {
        simple[12..].to_vec()
    };
    let body = [b"WEBP".to_vec(), riff_chunk(b"VP8X", &vp8x), image].concat();
    riff_chunk(b"RIFF", &body)
}

#[test]
fn a_still_lossy_webp_with_alpha_counts_its_alpha_decoding() {
    // An 11,585² RGBA canvas: 4 bytes decoded, and 6.5 for what a lossy
    // image with an alpha chunk keeps while decoding, is 1,344 MiB. With
    // just an RGBA frame it would be 1,024 MiB, just under the limit.
    assert_too_large(
        "cutout.webp",
        &still_webp_with_alpha(11_585, 11_585, true),
        "needs about 1343 MiB, over the 1024 MiB limit",
    );
    // The same canvas holding a lossless image decodes into the output
    // directly, so it gets past the estimate; its 64×48 bitstream then
    // doesn't match the canvas, which the decoder reports.
    let dir = TestDir::new("webp-lossless");
    let (src, job) = plan_one(
        &dir,
        "cutout.webp",
        &still_webp_with_alpha(11_585, 11_585, false),
        Operation::convert(Format::Bmp),
    );
    let before = SourceState::of(&src);
    let err = run(&job).unwrap_err();
    assert_eq!(
        err.user_message, "cutout.webp isn't a readable WEBP image.",
        "{}",
        err.detail
    );
    before.assert_unchanged();
}

#[test]
fn an_animated_webp_counts_its_canvas() {
    // An 11,585² RGBA canvas: 4 bytes decoded, 4 for the decoder's frame
    // and 4 for its canvas per pixel is 1,536 MiB. Without the canvas it
    // would be 1,024 MiB, just under the limit.
    let dir = TestDir::new("webp-canvas");
    let (src, job) = plan_one(
        &dir,
        "loop.webp",
        &animated_webp(11_585, 11_585),
        Operation::convert(Format::Bmp),
    );
    let before = SourceState::of(&src);
    let names_before = dir.names();

    let err = run(&job).unwrap_err();

    assert_eq!(err.user_message, "loop.webp is too large to convert.");
    assert!(
        err.detail
            .contains("needs about 1535 MiB, over the 1024 MiB limit"),
        "{}",
        err.detail
    );
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}

/// The 16×16 RGB image as a PNG with a `len`-byte `tEXt` comment after its
/// header.
fn png_with_comment(len: usize) -> Vec<u8> {
    let mut png = encode(&DynamicImage::ImageRgb8(RgbImage::new(16, 16)), Format::Png);
    let mut chunk = b"tEXtComment\0".to_vec();
    chunk.resize(chunk.len() + len, b'a');
    let mut text = u32::try_from(chunk.len() - 4)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    text.extend_from_slice(&chunk);
    text.extend_from_slice(&crc32(&chunk).to_be_bytes());
    // After the signature (8) and IHDR (25).
    png.splice(33..33, text);
    png
}

/// Plans `bytes` as `name` for BMP, expects it refused as too large with
/// `why` in the detail, and checks that nothing changed.
fn assert_too_large(name: &str, bytes: &[u8], why: &str) {
    assert_too_large_as(name, bytes, Format::Bmp, why);
}

/// [`assert_too_large`], converting to `to`.
fn assert_too_large_as(name: &str, bytes: &[u8], to: Format, why: &str) {
    let dir = TestDir::new("too-large");
    let (src, job) = plan_one(&dir, name, bytes, Operation::convert(to));
    let before = SourceState::of(&src);
    let names_before = dir.names();

    let err = run(&job).unwrap_err();

    assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
    assert_eq!(err.user_message, format!("{name} is too large to convert."));
    assert!(err.detail.contains(why), "{}", err.detail);
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}

#[test]
fn a_png_with_more_metadata_than_its_decoder_may_hold_is_refused() {
    // Its text chunks are counted from their headers before decoding: a
    // text chunk can cost twice its size, so a 40 MiB comment (a bloated
    // XMP packet is about that) fits the 128 MiB limit, and an 80 MiB one
    // doesn't.
    let dir = TestDir::new("png-metadata");
    let (src, job) = plan_one(
        &dir,
        "notes.png",
        &png_with_comment(40 * 1024 * 1024),
        Operation::convert(Format::Bmp),
    );
    let before = SourceState::of(&src);
    run(&job).unwrap_or_else(|e| panic!("{e} ({})", e.detail));
    before.assert_unchanged();

    assert_too_large(
        "notes.png",
        &png_with_comment(80 * 1024 * 1024),
        "its metadata needs about 240 MiB, over the 128 MiB limit for PNG metadata",
    );
}

/// The 16×16 RGB image as a PNG with `count` empty `tEXt` chunks (keyword
/// "a") ahead of its image data.
fn png_with_empty_comments(count: usize) -> Vec<u8> {
    let png = encode(&DynamicImage::ImageRgb8(RgbImage::new(16, 16)), Format::Png);
    let data = b"tEXta\0";
    let mut text = 2u32.to_be_bytes().to_vec();
    text.extend_from_slice(data);
    text.extend_from_slice(&crc32(data).to_be_bytes());
    // After the signature (8) and IHDR (25).
    [&png[..33], &text.repeat(count), &png[33..]].concat()
}

#[test]
fn a_png_with_a_million_tiny_text_chunks_is_refused() {
    // The decoder keeps an entry of about a hundred bytes for every text
    // chunk, though each is only 14 bytes in the file: 1.03 million of them
    // come to about 130 MiB.
    assert_too_large(
        "chatty.png",
        &png_with_empty_comments(1_030_000),
        "its metadata needs about 129 MiB, over the 128 MiB limit for PNG metadata",
    );
    // Past about a million chunks, the walk stops counting.
    assert_too_large(
        "chatty.png",
        &png_with_empty_comments(1_100_000),
        "it has too many metadata chunks to count",
    );
}

#[test]
fn a_png_icc_profile_counts_while_decoding() {
    // The decoder may inflate an ICC profile up to its own 128 MiB limit,
    // so a PNG with one is charged that while decoding. 31,000² gray kept
    // as gray in a TIFF is 917 MiB decoded (more than the decode limit,
    // which is checked after the estimate): with the profile's 128 MiB,
    // past 1 GiB.
    let mut png = Vec::new();
    let mut encoder = PngEncoder::new(&mut png);
    encoder.set_icc_profile(rgb_profile(132)).unwrap();
    DynamicImage::ImageLuma8(GrayImage::new(16, 16))
        .write_with_encoder(encoder)
        .unwrap();
    assert_too_large_as(
        "scan.png",
        &claiming(png, 31_000, 31_000),
        Format::Tiff,
        "needs about 1044 MiB, over the 1024 MiB limit",
    );
}

#[test]
fn the_png_decoder_is_held_to_its_own_limit() {
    // A 16-bit RGBA row 2^24 pixels wide takes a 128 MiB line buffer: past
    // the decoder's own 128 MiB limit, though the image (one row) fits.
    let rgba16 = DynamicImage::ImageRgba16(::image::ImageBuffer::new(16, 16));
    assert_too_large(
        "strip.png",
        &png_claiming(&rgba16, (1 << 24) + 16, 1),
        "Memory limit exceeded",
    );
}

#[test]
fn a_truncated_file_is_not_a_readable_image() {
    for (from, name) in [(Format::Bmp, "half.bmp"), (Format::Png, "half.png")] {
        let mut bytes = encode(&gradient(), from);
        bytes.truncate(bytes.len() / 2);
        let dir = TestDir::new("truncated");
        let (src, job) = plan_one(&dir, name, &bytes, Operation::convert(Format::Tiff));
        let before = SourceState::of(&src);
        let names_before = dir.names();

        let err = run(&job).unwrap_err();

        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{name}: {}", err.detail);
        assert_eq!(
            err.user_message,
            format!("{name} isn't a readable {} image.", from.display_name())
        );
        assert_eq!(dir.names(), names_before, "{name}");
        before.assert_unchanged();
    }
}

#[test]
fn a_missing_source_is_a_read_error() {
    let dir = TestDir::new("missing");
    let (src, job) = plan_one(
        &dir,
        "gone.png",
        &encode(&gradient(), Format::Png),
        Operation::convert(Format::Bmp),
    );
    fs::remove_file(&src).unwrap();
    let names_before = dir.names();

    let err = run(&job).unwrap_err();

    assert_eq!(err.kind, ErrorKind::ReadFailed);
    assert_eq!(err.user_message, "Grape couldn't read gone.png.");
    assert_eq!(dir.names(), names_before);
}

#[test]
fn unsupported_targets_write_nothing() {
    for (to, message) in [
        (Format::Avif, "PNG to AVIF isn't in this build yet."),
        (Format::Heic, "PNG to HEIC isn't in this build yet."),
    ] {
        let dir = TestDir::new("unsupported");
        let (src, job) = plan_one(
            &dir,
            "photo.png",
            &encode(&gradient(), Format::Png),
            Operation::convert(to),
        );
        let before = SourceState::of(&src);
        let names_before = dir.names();

        let err = run(&job).unwrap_err();

        assert_eq!(err.kind, ErrorKind::NotYetSupported, "{to:?}");
        assert_eq!(err.user_message, message);
        assert_eq!(dir.names(), names_before, "{to:?}");
        before.assert_unchanged();
    }
}

#[test]
fn tools_multiple_inputs_and_folder_outputs_are_not_supported() {
    let dir = TestDir::new("shapes");
    let a = dir.join("a.png");
    let b = dir.join("b.png");
    fs::write(&a, encode(&gradient(), Format::Png)).unwrap();
    fs::write(&b, encode(&gradient(), Format::Png)).unwrap();
    let names_before = dir.names();
    let sources_before = [SourceState::of(&a), SourceState::of(&b)];
    let jobs = [
        Job {
            inputs: vec![a.clone()],
            output: JobOutput::File(dir.join("a Compressed.png")),
            op: Operation::tool(ToolKey::Compress),
            used_fallback: false,
        },
        Job {
            inputs: vec![a.clone(), b],
            output: JobOutput::File(dir.join("a.jpg")),
            op: Operation::convert(Format::Jpg),
            used_fallback: false,
        },
        Job {
            inputs: vec![a.clone()],
            output: JobOutput::Folder(dir.join("a")),
            op: Operation::convert(Format::Jpg),
            used_fallback: false,
        },
        Job {
            inputs: vec![],
            output: JobOutput::File(dir.join("x.jpg")),
            op: Operation::convert(Format::Jpg),
            used_fallback: false,
        },
    ];
    for job in &jobs {
        let err = run(job).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported, "{job:?}");
    }
    assert_eq!(
        run(&jobs[0]).unwrap_err().user_message,
        "Tools arrive in a later build."
    );
    assert_eq!(dir.names(), names_before);
    for before in &sources_before {
        before.assert_unchanged();
    }
}

#[test]
fn a_cancelled_job_writes_nothing() {
    let dir = TestDir::new("cancel");
    let (src, job) = plan_one(
        &dir,
        "photo.png",
        &encode(&gradient(), Format::Png),
        Operation::convert(Format::Jpg),
    );
    let before = SourceState::of(&src);
    let names_before = dir.names();
    let ctx = EngineCtx::default();
    // The app keeps a clone; cancelling it cancels the engine's copy.
    let app_side = ctx.cancel.clone();
    assert!(!ctx.cancel.is_cancelled());
    app_side.cancel();
    assert!(ctx.cancel.is_cancelled());

    let err = run_job(&job, &ctx, &mut |_| {}).unwrap_err();

    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}

#[test]
fn a_unicode_name_keeps_its_stem() {
    let dir = TestDir::new("unicode");
    let (src, job) = plan_one(
        &dir,
        "ảnh chụp.png",
        &encode(&gradient(), Format::Png),
        Operation::convert(Format::Jpg),
    );
    let before = SourceState::of(&src);
    let outcome = run(&job).unwrap();
    assert_eq!(outcome.outputs, [dir.join("ảnh chụp.jpg")]);
    assert!(!outcome.renamed);
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_fallback_job_creates_the_fallback_folder() {
    let dir = TestDir::new("fallback");
    let photos = dir.join("photos");
    fs::create_dir(&photos).unwrap();
    let src = photos.join("photo.png");
    fs::write(&src, encode(&gradient(), Format::Png)).unwrap();
    let fallback = dir.join("Downloads").join("Grape");
    let settings = Settings::new(&fallback);
    // The source folder is read-only; the fallback doesn't exist yet.
    let sel = Selection::new(vec![SelectionItem::file(&src)])
        .with_dir(&photos, DirSnapshot::new(["photo.png"], false))
        .with_dir(&fallback, DirSnapshot::new(Vec::<String>::new(), true));
    let job = plan(&sel, &Operation::convert(Format::Tiff), &settings)
        .unwrap()
        .jobs
        .remove(0);
    assert!(job.used_fallback);
    let before = SourceState::of(&src);

    let outcome = run(&job).unwrap();

    assert_eq!(outcome.outputs, [fallback.join("photo.tiff")]);
    assert_eq!(names_in(&photos), ["photo.png"]);
    assert_eq!(names_in(&fallback), ["photo.tiff"]);
    before.assert_unchanged();
}

/// The context the app gives every job: Grape's fallback folder.
fn ctx_with_fallback(fallback: &Path) -> EngineCtx {
    EngineCtx {
        fallback_dir: Some(fallback.to_path_buf()),
        ..EngineCtx::default()
    }
}

#[test]
fn an_output_folder_that_is_the_missing_fallback_is_created() {
    // The output setting names Grape's own fallback folder, which doesn't
    // exist yet: the job isn't a fallback job, but the folder is created.
    let dir = TestDir::new("output-is-fallback");
    let src = dir.join("photo.png");
    fs::write(&src, encode(&gradient(), Format::Png)).unwrap();
    let fallback = dir.join("Downloads").join("Grape");
    let settings = Settings {
        output: OutputLocation::Folder(fallback.clone()),
        ..Settings::new(&fallback)
    };
    let sel = build_selection(std::slice::from_ref(&src), &settings);
    let job = plan(&sel, &Operation::convert(Format::Webp), &settings)
        .unwrap()
        .jobs
        .remove(0);
    assert!(!job.used_fallback);
    let before = SourceState::of(&src);

    let outcome = run_job(&job, &ctx_with_fallback(&fallback), &mut |_| {})
        .unwrap_or_else(|e| panic!("{e} ({})", e.detail));

    assert_eq!(outcome.outputs, [fallback.join("photo.webp")]);
    assert_eq!(names_in(&fallback), ["photo.webp"]);
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

/// Renames `from` to `to`, retrying for a moment in case a scanner still
/// has a file in it open (Windows refuses to rename the folder then).
fn rename_retrying(from: &Path, to: &Path) {
    for _ in 0..20 {
        if fs::rename(from, to).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    fs::rename(from, to).unwrap();
}

#[test]
fn a_destination_that_vanishes_after_planning_is_not_recreated() {
    let dir = TestDir::new("vanished");
    let fallback = dir.join("Downloads").join("Grape");
    let ctx = ctx_with_fallback(&fallback);

    // The output folder setting's folder is moved away between planning
    // and the run.
    let src = dir.join("photo.png");
    fs::write(&src, encode(&gradient(), Format::Png)).unwrap();
    let work = dir.join("Work");
    let exports = work.join("Exports");
    fs::create_dir_all(&exports).unwrap();
    let settings = Settings {
        output: OutputLocation::Folder(exports.clone()),
        ..Settings::new(&fallback)
    };
    let sel = build_selection(std::slice::from_ref(&src), &settings);
    let job = plan(&sel, &Operation::convert(Format::Jpg), &settings)
        .unwrap()
        .jobs
        .remove(0);
    assert_eq!(planned_file(&job), exports.join("photo.jpg"));
    rename_retrying(&work, &dir.join("Archive"));
    let before = SourceState::of(&src);

    let err = run_job(&job, &ctx, &mut |_| {}).unwrap_err();

    assert_eq!(err.kind, ErrorKind::WriteFailed, "{}", err.detail);
    assert_eq!(err.user_message, "Grape couldn't save into Exports.");
    assert!(!work.exists(), "the old folder came back");
    assert!(names_in(&dir.join("Archive").join("Exports")).is_empty());
    assert!(!fallback.exists());
    before.assert_unchanged();

    // Next to the source: its folder is renamed while the image converts,
    // after the source was read.
    let shoot = dir.join("Shoot");
    fs::create_dir(&shoot).unwrap();
    let src = shoot.join("photo.png");
    fs::write(&src, encode(&gradient(), Format::Png)).unwrap();
    let settings = Settings::new(&fallback);
    let sel = build_selection(std::slice::from_ref(&src), &settings);
    let job = plan(&sel, &Operation::convert(Format::Jpg), &settings)
        .unwrap()
        .jobs
        .remove(0);
    let before = SourceState::of(&src);
    let renamed = dir.join("Shoot 2");

    let err = run_job(&job, &ctx, &mut |done| {
        if done >= 0.5 && !renamed.exists() {
            rename_retrying(&shoot, &renamed);
        }
    })
    .unwrap_err();

    assert_eq!(err.kind, ErrorKind::WriteFailed, "{}", err.detail);
    assert!(!shoot.exists(), "the old folder came back");
    assert_eq!(names_in(&renamed), ["photo.png"]);
    assert_eq!(content_digest(&renamed.join("photo.png")), before.content);
}

#[cfg(unix)]
#[test]
fn a_folder_that_turns_read_only_is_a_write_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDir::new("turns-read-only");
    let (src, job) = plan_one(
        &dir,
        "photo.png",
        &encode(&gradient(), Format::Png),
        Operation::convert(Format::Jpg),
    );
    let before = SourceState::of(&src);
    let names_before = dir.names();
    fs::set_permissions(&dir.path, fs::Permissions::from_mode(0o555)).unwrap();
    // Restore write access before `TestDir` deletes the tree.
    struct Unlock(std::path::PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    let _unlock = Unlock(dir.path.clone());
    // Root (and CAP_DAC_OVERRIDE) ignores permission bits: check first.
    let canary = dir.join("canary");
    if fs::write(&canary, b"x").is_ok() {
        let _ = fs::remove_file(&canary);
        println!("note: skipped, this user can write into a 0o555 folder (running as root?)");
        return;
    }

    let err = run(&job).unwrap_err();

    assert_eq!(err.kind, ErrorKind::WriteFailed);
    assert_eq!(
        err.user_message,
        format!(
            "Grape couldn't save into {}.",
            dir.path.file_name().unwrap().to_string_lossy()
        )
    );
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}
