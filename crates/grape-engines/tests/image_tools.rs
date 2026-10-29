//! Grape's own image tools (D26) end to end: plan with `grape_core`, run
//! with `run_job`, and check the output, the note, and that the source
//! never changed and no temp file was left.
#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::io::Cursor;
use std::path::Path;

use common::{assert_no_leftovers, plan_one, planned_file, SourceState, TestDir};
use grape_core::{Operation, ToolKey};
use grape_engines::{run_job, EngineCtx, ErrorKind, GrapeError, JobOutcome, MetaField, Note};
use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{ImageDecoder, ImageEncoder, Rgb, RgbImage};

/// Plans `op` on `bytes` named `name` and runs it. Checks that the source
/// didn't change, that no temp file was left, and that a failed job wrote
/// nothing.
fn run(dir: &TestDir, name: &str, bytes: &[u8], op: Operation) -> Result<JobOutcome, GrapeError> {
    let (src, job) = plan_one(dir, name, bytes, op);
    let before = SourceState::of(&src);
    let result = run_job(&job, &EngineCtx::default(), &mut |_| {});
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
    if result.is_err() {
        assert!(!planned_file(&job).exists());
    }
    result
}

fn strip(dir: &TestDir, name: &str, bytes: &[u8]) -> Result<JobOutcome, GrapeError> {
    run(dir, name, bytes, Operation::tool(ToolKey::RemoveMetadata))
}

/// A 64×48 gradient.
fn gradient() -> RgbImage {
    RgbImage::from_fn(64, 48, |x, y| {
        Rgb([
            u8::try_from(x * 4).unwrap(),
            u8::try_from(y * 5).unwrap(),
            128,
        ])
    })
}

/// The gradient as a JPEG with no metadata but JFIF.
fn plain_jpeg() -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&gradient())
        .unwrap();
    out
}

/// A JPEG segment.
fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut s = vec![0xFF, marker];
    s.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
    s.extend_from_slice(payload);
    s
}

/// A little-endian EXIF: make "Canon", orientation `orientation`, and a
/// GPS IFD with a latitude.
fn exif(orientation: u16) -> Vec<u8> {
    let mut t = b"Exif\0\0II*\0\x08\0\0\0".to_vec();
    // IFD0 at 8: 3 entries.
    t.extend_from_slice(&3u16.to_le_bytes());
    let entry = |t: &mut Vec<u8>, tag: u16, kind: u16, count: u32, field: [u8; 4]| {
        t.extend_from_slice(&tag.to_le_bytes());
        t.extend_from_slice(&kind.to_le_bytes());
        t.extend_from_slice(&count.to_le_bytes());
        t.extend_from_slice(&field);
    };
    entry(&mut t, 0x010F, 2, 4, *b"Can\0");
    let o = orientation.to_le_bytes();
    entry(&mut t, 0x0112, 3, 1, [o[0], o[1], 0, 0]);
    // The GPS IFD right after IFD0: 8 + 2 + 36 + 4 = 50.
    entry(&mut t, 0x8825, 4, 1, 50u32.to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes());
    t.extend_from_slice(&1u16.to_le_bytes());
    entry(&mut t, 0x0001, 2, 2, *b"N\0\0\0");
    t.extend_from_slice(&0u32.to_le_bytes());
    seg(0xE1, &t)
}

/// `jpeg` with `segments` after its SOI and `trailer` after its EOI.
fn with(jpeg: &[u8], segments: &[Vec<u8>], trailer: &[u8]) -> Vec<u8> {
    let mut out = jpeg[..2].to_vec();
    for s in segments {
        out.extend_from_slice(s);
    }
    out.extend_from_slice(&jpeg[2..]);
    out.extend_from_slice(trailer);
    out
}

fn pixels(bytes: &[u8]) -> Vec<u8> {
    image::load_from_memory(bytes).unwrap().to_rgb8().into_raw()
}

fn orientation(bytes: &[u8]) -> Orientation {
    let mut decoder = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .unwrap()
        .into_decoder()
        .unwrap();
    decoder.orientation().unwrap()
}

#[test]
fn remove_metadata_strips_a_jpeg_and_keeps_its_pixels_and_orientation() {
    use MetaField::*;
    let dir = TestDir::new("strip-jpeg");
    let plain = plain_jpeg();
    let mut xmp = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    xmp.extend_from_slice(b"<x:xmpmeta>secret place</x:xmpmeta>");
    let src = with(
        &plain,
        &[exif(6), seg(0xE1, &xmp), seg(0xFE, b"secret comment")],
        b"\xFF\xD8 secret gain map",
    );
    let outcome = strip(&dir, "photo.jpg", &src).unwrap();
    assert_eq!(outcome.outputs, [dir.join("photo No Metadata.jpg")]);
    assert_eq!(
        outcome.notes,
        [Note::MetadataRemoved(vec![
            Location, Camera, Comment, Xmp, Embedded
        ])]
    );
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert_eq!(pixels(&out), pixels(&plain));
    assert_eq!(orientation(&out), Orientation::Rotate90);
    for secret in [&b"secret"[..], b"Can\0"] {
        assert!(
            !out.windows(secret.len()).any(|w| w == secret),
            "{secret:?} left"
        );
    }
}

#[test]
fn remove_metadata_on_a_clean_jpeg_says_nothing_went() {
    let dir = TestDir::new("strip-jpeg-clean");
    let plain = plain_jpeg();
    let outcome = strip(&dir, "clean.JPG", &plain).unwrap();
    assert_eq!(outcome.outputs, [dir.join("clean No Metadata.JPG")]);
    assert_eq!(outcome.notes, [Note::MetadataRemoved(vec![])]);
    assert_eq!(fs::read(&outcome.outputs[0]).unwrap(), plain);
}

#[test]
fn remove_metadata_refuses_a_broken_jpeg_and_writes_nothing() {
    let dir = TestDir::new("strip-jpeg-broken");
    let plain = plain_jpeg();
    for (name, bytes) in [
        ("cut.jpg", &plain[..plain.len() / 2]),
        ("text.jpg", &b"not a picture at all"[..]),
        ("empty.jpg", &b""[..]),
    ] {
        let err = strip(&dir, name, bytes).unwrap_err();
        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{name}: {}", err.detail);
        assert_eq!(
            err.user_message,
            format!("{name} isn't a readable JPG image.")
        );
        fs::remove_file(dir.join(name)).unwrap();
    }
}

#[test]
fn a_cancelled_strip_leaves_nothing() {
    let dir = TestDir::new("strip-jpeg-cancel");
    let (src, job) = plan_one(
        &dir,
        "photo.jpg",
        &with(&plain_jpeg(), &[exif(1)], b""),
        Operation::tool(ToolKey::RemoveMetadata),
    );
    let ctx = EngineCtx::default();
    ctx.cancel.cancel();
    let err = run_job(&job, &ctx, &mut |_| {}).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(!planned_file(&job).exists());
    assert_no_leftovers(&dir.path);
    assert!(Path::new(&src).exists());
}

/// The gradient as a PNG.
fn plain_png() -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(gradient().as_raw(), 64, 48, image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// A PNG chunk with its CRC.
fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    let mut out = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
    out
}

/// `png` with `chunks` right after IHDR.
fn png_with(png: &[u8], chunks: &[Vec<u8>]) -> Vec<u8> {
    // The signature (8) and IHDR (25).
    let mut out = png[..33].to_vec();
    for c in chunks {
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&png[33..]);
    out
}

#[test]
fn remove_metadata_strips_a_png_even_one_named_jpg() {
    use MetaField::*;
    let dir = TestDir::new("strip-png");
    let plain = plain_png();
    let exif = exif(1);
    // A PNG's eXIf holds the TIFF without "Exif\0\0".
    let src = png_with(
        &plain,
        &[
            chunk(b"tEXt", b"Author\0secret author"),
            chunk(b"eXIf", &exif[10..]),
            chunk(b"tIME", &[7, 232, 5, 1, 10, 20, 30]),
        ],
    );
    for name in ["shot.png", "shot.jpg"] {
        let outcome = strip(&dir, name, &src).unwrap();
        let (stem, ext) = name.split_once('.').unwrap();
        assert_eq!(
            outcome.outputs,
            [dir.join(&format!("{stem} No Metadata.{ext}"))]
        );
        assert_eq!(
            outcome.notes,
            [Note::MetadataRemoved(vec![
                Location, Camera, Author, ModDate
            ])]
        );
        let out = fs::read(&outcome.outputs[0]).unwrap();
        assert_eq!(out, plain);
        fs::remove_file(&outcome.outputs[0]).unwrap();
        fs::remove_file(dir.join(name)).unwrap();
    }
}

/// A RIFF chunk, padded to even.
fn riff_chunk(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = fourcc.to_vec();
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    let body = chunks.concat();
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend(body);
    out
}

#[test]
fn remove_metadata_strips_a_webp_and_fixes_its_header() {
    use MetaField::*;
    let dir = TestDir::new("strip-webp");
    let mut lossless = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut lossless)
        .write_image(gradient().as_raw(), 64, 48, image::ExtendedColorType::Rgb8)
        .unwrap();
    // The encoder's VP8L chunk, put in an extended file with EXIF and XMP.
    let vp8l = lossless[12..].to_vec();
    let vp8x = |flags: u8| riff_chunk(b"VP8X", &[flags, 0, 0, 0, 63, 0, 0, 47, 0, 0]);
    let exif = exif(6);
    let src = riff(&[
        vp8x(0x0C),
        vp8l.clone(),
        riff_chunk(b"EXIF", &exif[4..]),
        riff_chunk(b"XMP ", b"<x:xmpmeta>secret</x:xmpmeta>"),
    ]);
    let outcome = strip(&dir, "pic.webp", &src).unwrap();
    assert_eq!(
        outcome.notes,
        [Note::MetadataRemoved(vec![Location, Camera, Xmp])]
    );
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert_eq!(pixels(&out), pixels(&lossless));
    assert_eq!(orientation(&out), Orientation::Rotate90);
    assert!(!out.windows(6).any(|w| w == b"secret"));
    assert_eq!(
        u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize,
        out.len() - 8
    );
    // VP8X keeps only its EXIF flag, for the orientation written back.
    assert_eq!(out[20], 0x08);
}

#[test]
fn remove_metadata_strips_a_gif_and_keeps_its_frames() {
    let dir = TestDir::new("strip-gif");
    // Two 1×1 frames, looping, with a 2-color palette (the image crate
    // here has no GIF encoder).
    let mut plain = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\0\0\0\xff\xff\xff".to_vec();
    plain.extend_from_slice(b"\x21\xFF\x0BNETSCAPE2.0\x03\x01\x00\x00\x00");
    for _ in 0..2 {
        plain.extend_from_slice(&[0x21, 0xF9, 4, 4, 10, 0, 0, 0]);
        plain.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 0x01, 0]);
    }
    let mut src = plain.clone();
    plain.push(0x3B);
    src.extend_from_slice(&[0x21, 0xFE, 6]);
    src.extend_from_slice(b"secret");
    src.extend_from_slice(&[0, 0x3B]);
    let outcome = strip(&dir, "anim.gif", &src).unwrap();
    assert_eq!(
        outcome.notes,
        [Note::MetadataRemoved(vec![MetaField::Comment])]
    );
    assert_eq!(fs::read(&outcome.outputs[0]).unwrap(), plain);
}

/// The samples of a page the tiff crate decoded, as bytes.
fn tiff_samples(page: tiff::decoder::DecodingResult) -> Vec<u8> {
    use tiff::decoder::DecodingResult::*;
    match page {
        U8(v) => v,
        U16(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        _ => panic!("another sample type"),
    }
}

/// Every page of a TIFF, decoded by the tiff crate.
fn tiff_pages(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut decoder = tiff::decoder::Decoder::new(Cursor::new(bytes)).unwrap();
    let mut pages = vec![tiff_samples(decoder.read_image().unwrap())];
    while decoder.more_images() {
        decoder.next_image().unwrap();
        pages.push(tiff_samples(decoder.read_image().unwrap()));
    }
    pages
}

/// A two-page TIFF from the tiff crate, classic or BigTIFF: an LZW RGB
/// page with a camera's and an editor's metadata, an Exif IFD and a GPS
/// IFD, then a Deflate 16-bit gray page with a description.
fn tagged_tiff(big: bool) -> Vec<u8> {
    use tiff::encoder::{colortype, Compression, DeflateLevel, Rational, TiffEncoder, TiffKind};
    use tiff::tags::Tag;
    fn pages<K: TiffKind>(mut e: TiffEncoder<&mut Cursor<Vec<u8>>, K>) {
        e = e.with_compression(Compression::Lzw);
        let mut exif = e.extra_directory().unwrap();
        exif.write_tag(Tag::Unknown(0x9003), "2024:05:01 10:20:30")
            .unwrap();
        exif.write_tag(Tag::Unknown(0x927C), &b"secret maker note"[..])
            .unwrap();
        let exif = exif.finish_with_offsets().unwrap().offset;
        let mut gps = e.extra_directory().unwrap();
        gps.write_tag(Tag::Unknown(1), "N").unwrap();
        gps.write_tag(Tag::Unknown(2), Rational { n: 48, d: 1 })
            .unwrap();
        let gps = gps.finish_with_offsets().unwrap().offset;
        let rgb = photo(40, 30).into_raw();
        let mut first = e.new_image::<colortype::RGB8>(40, 30).unwrap();
        let tags = first.encoder();
        for (tag, text) in [
            (Tag::ImageDescription, "secret description"),
            (Tag::Make, "secret make"),
            (Tag::Software, "secret software"),
            (Tag::DateTime, "2024:05:01 10:20:30"),
            (Tag::Artist, "secret artist"),
            (Tag::Copyright, "secret copyright"),
        ] {
            tags.write_tag(tag, text).unwrap();
        }
        tags.write_tag(Tag::Orientation, 6u16).unwrap();
        tags.write_tag(Tag::IccProfile, &b"an ICC profile"[..])
            .unwrap();
        tags.write_tag(Tag::Unknown(700), &b"<x:xmpmeta>secret</x:xmpmeta>"[..])
            .unwrap();
        tags.write_tag(Tag::ExifDirectory, exif).unwrap();
        tags.write_tag(Tag::GpsDirectory, gps).unwrap();
        first.write_data(&rgb).unwrap();
        e = e.with_compression(Compression::Deflate(DeflateLevel::Best));
        let gray: Vec<u16> = (0..20 * 10).map(|i| i * 300).collect();
        let mut second = e.new_image::<colortype::Gray16>(20, 10).unwrap();
        second
            .encoder()
            .write_tag(Tag::ImageDescription, "secret page two")
            .unwrap();
        second.write_data(&gray).unwrap();
    }
    let mut out = Cursor::new(Vec::new());
    if big {
        pages(TiffEncoder::new_big(&mut out).unwrap());
    } else {
        pages(TiffEncoder::new(&mut out).unwrap());
    }
    out.into_inner()
}

#[test]
fn remove_metadata_strips_every_page_of_a_tiff_and_keeps_its_pixels() {
    use tiff::tags::Tag;
    use MetaField::*;
    let dir = TestDir::new("strip-tiff");
    for (big, name) in [(false, "scan.tif"), (true, "big.TIFF")] {
        let src = tagged_tiff(big);
        let outcome = strip(&dir, name, &src).unwrap();
        let (stem, ext) = name.split_once('.').unwrap();
        assert_eq!(
            outcome.outputs,
            [dir.join(&format!("{stem} No Metadata.{ext}"))]
        );
        assert_eq!(
            outcome.notes,
            [Note::MetadataRemoved(vec![
                Location,
                Camera,
                Author,
                Comment,
                Copyright,
                Software,
                CreationDate,
                ModDate,
                Xmp
            ])]
        );
        let out = fs::read(&outcome.outputs[0]).unwrap();
        assert_eq!(tiff_pages(&out), tiff_pages(&src), "{name}");
        let mut decoder = tiff::decoder::Decoder::new(Cursor::new(&out)).unwrap();
        assert_eq!(decoder.get_tag_u32(Tag::Orientation).unwrap(), 6);
        assert!(decoder.find_tag(Tag::IccProfile).unwrap().is_some());
        for gone in [
            Tag::Software,
            Tag::ExifDirectory,
            Tag::GpsDirectory,
            Tag::Unknown(700),
        ] {
            assert!(decoder.find_tag(gone).unwrap().is_none(), "{gone:?}");
        }
        assert_eq!(out[..4], src[..4]);
        for secret in [&b"secret"[..], b"2024:05"] {
            assert!(
                !out.windows(secret.len()).any(|w| w == secret),
                "{secret:?} left in {name}"
            );
        }
    }
}

/// A classic gray 64 × 64 TIFF of two uncompressed strips, every byte of
/// them there, whose byte counts say the second is half its size: libtiff
/// takes that at its word, and Pillow and ImageMagick read past it.
fn miscounted_tiff() -> Vec<u8> {
    let entries: [(u16, u16, u32, u32); 7] = [
        (256, 4, 1, 64),
        (257, 4, 1, 64),
        (258, 3, 1, 8),
        (262, 3, 1, 1),
        (273, 4, 2, 98),
        (278, 4, 1, 32),
        (279, 4, 2, 106),
    ];
    let mut file = b"II*\0\x08\0\0\0\x07\0".to_vec();
    for (tag, kind, count, value) in entries {
        file.extend(tag.to_le_bytes());
        file.extend(kind.to_le_bytes());
        file.extend(count.to_le_bytes());
        file.extend(value.to_le_bytes());
    }
    file.extend([0; 4]);
    file.extend([114u32, 114 + 2_048].iter().flat_map(|n| n.to_le_bytes()));
    file.extend([2_048u32, 1_024].iter().flat_map(|n| n.to_le_bytes()));
    file.extend((0..4_096).map(|i| (i % 64 * 4) as u8));
    file
}

#[test]
fn remove_metadata_refuses_a_tiff_it_cant_strip_and_writes_nothing() {
    use tiff::encoder::{colortype, TiffEncoder};
    use tiff::tags::Tag;
    let dir = TestDir::new("strip-tiff-refused");
    let one = |tag: Tag, value: &[u8]| {
        let mut out = Cursor::new(Vec::new());
        let mut e = TiffEncoder::new(&mut out).unwrap();
        let mut image = e.new_image::<colortype::Gray8>(2, 2).unwrap();
        image.encoder().write_tag(tag, value).unwrap();
        image.write_data(&[1, 2, 3, 4]).unwrap();
        out.into_inner()
    };
    // A strip of JPEG data (compression 7): a progressive JPEG whose first
    // scan runs past the first 64 KiB, so what follows can't be seen.
    let progressive = {
        let jpeg = [
            &b"\xFF\xD8\xFF\xC2\0\x0B\x08\0\x08\0\x08\x01\x01\x11\0"[..],
            b"\xFF\xDA\0\x08\x01\x01\0\0\x3F\0",
            &[0x55; 70_000],
            b"\xFF\xD9",
        ]
        .concat();
        let mut out = Cursor::new(Vec::new());
        let mut e = TiffEncoder::new(&mut out).unwrap();
        let width = u32::try_from(jpeg.len()).unwrap();
        let mut image = e.new_image::<colortype::Gray8>(width, 1).unwrap();
        image.encoder().write_tag(Tag::Compression, 7u16).unwrap();
        image.write_data(&jpeg).unwrap();
        out.into_inner()
    };
    for (name, bytes, kind, message) in [
        (
            "raw.tif",
            one(Tag::Unknown(50706), &[1, 4, 0, 0]),
            ErrorKind::NotYetSupported,
            "raw.tif is a camera raw (DNG) file, which Grape can't change yet.",
        ),
        (
            "stack.tif",
            one(Tag::ImageDescription, b"ImageJ=1.54f\nimages=2\n\0"),
            ErrorKind::NotYetSupported,
            "stack.tif keeps its layout in its description, so Remove Metadata would break it.",
        ),
        (
            "scans.tif",
            progressive,
            ErrorKind::NotYetSupported,
            "scans.tif has JPEG data Grape can't check for metadata yet.",
        ),
        (
            "counts.tif",
            miscounted_tiff(),
            ErrorKind::NotYetSupported,
            "counts.tif says the wrong size for some of its image data, which Grape can't rewrite yet.",
        ),
        (
            "cut.tif",
            tagged_tiff(false)[..1000].to_vec(),
            ErrorKind::DecodeFailed,
            "cut.tif isn't a readable TIFF image.",
        ),
    ] {
        let err = strip(&dir, name, &bytes).unwrap_err();
        assert_eq!(err.kind, kind, "{name}: {}", err.detail);
        assert_eq!(err.user_message, message);
        fs::remove_file(dir.join(name)).unwrap();
    }
}

/// An ISOBMFF box.
fn bmff(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
    let mut b = u32::try_from(8 + content.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(content);
    b
}

/// An ISOBMFF full box.
fn full_box(kind: &[u8; 4], version: u8, flags: u32, content: &[u8]) -> Vec<u8> {
    let mut c = (u32::from(version) << 24 | flags).to_be_bytes().to_vec();
    c.extend_from_slice(content);
    bmff(kind, &c)
}

/// The meta boxes of [`heif`] in libheif's order, and in an iPhone's.
const LIBHEIF: [&[u8; 4]; 6] = [b"hdlr", b"iloc", b"iinf", b"pitm", b"iprp", b"iref"];
const IPHONE: [&[u8; 4]; 7] = [
    b"hdlr", b"dinf", b"pitm", b"iinf", b"iref", b"iprp", b"iloc",
];

/// A HEIF file of `brand` with `meta`'s boxes in `order`: a primary
/// picture (item 1, `primary` in mdat), its thumbnail (item 4, 0xC4s), its
/// Exif (item 2: a make, a GPS position, a secret) and its XMP (item 3),
/// then `after`. The pictures' bytes aren't HEVC: nothing here decodes
/// them. Gives the file and where items 1 to 4 are in it.
fn heif(
    brand: &[u8; 4],
    order: &[&[u8; 4]],
    primary: &[u8],
    after: &[u8],
) -> (Vec<u8>, Vec<std::ops::Range<usize>>) {
    turned_heif(1, brand, order, primary, after)
}

/// [`heif`] with its Exif's Orientation `orientation`.
fn turned_heif(
    orientation: u16,
    brand: &[u8; 4],
    order: &[&[u8; 4]],
    primary: &[u8],
    after: &[u8],
) -> (Vec<u8>, Vec<std::ops::Range<usize>>) {
    let mut payload = 6u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&exif(orientation)[4..]);
    payload.extend_from_slice(b"SecretSerial");
    let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'>SecretPlace</x:xmpmeta>".to_vec();
    let thumb = vec![0xC4; 40];
    let datas = [primary.to_vec(), payload, xmp, thumb];
    let ftyp = bmff(b"ftyp", &[&brand[..], b"\0\0\0\0mif1", brand].concat());
    let meta = |mdat_at: usize| -> Vec<u8> {
        let mut at = mdat_at + 8;
        let mut iloc = vec![0x44, 0x00];
        iloc.extend_from_slice(&4u16.to_be_bytes());
        for (id, data) in (1u16..).zip(&datas) {
            iloc.extend_from_slice(&id.to_be_bytes());
            iloc.extend_from_slice(&[0, 0, 0, 1]);
            iloc.extend_from_slice(&u32::try_from(at).unwrap().to_be_bytes());
            iloc.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
            at += data.len();
        }
        let mut iinf = 4u16.to_be_bytes().to_vec();
        for (id, kind, hidden, text) in [
            (1u16, b"hvc1", 0, ""),
            (2, b"Exif", 1, ""),
            (3, b"mime", 1, "application/rdf+xml\0"),
            (4, b"hvc1", 0, ""),
        ] {
            let mut c = id.to_be_bytes().to_vec();
            c.extend_from_slice(&[0, 0]);
            c.extend_from_slice(kind);
            c.push(0);
            c.extend_from_slice(text.as_bytes());
            iinf.extend_from_slice(&full_box(b"infe", 2, hidden, &c));
        }
        let mut iref = Vec::new();
        for (kind, from) in [(b"cdsc", 2u16), (b"cdsc", 3), (b"thmb", 4)] {
            iref.extend_from_slice(&bmff(kind, &[from.to_be_bytes(), [0, 1], [0, 1]].concat()));
        }
        let ipco = [
            bmff(b"hvcC", &[1; 23]),
            full_box(b"ispe", 0, 0, &[0, 0, 0, 64, 0, 0, 0, 48]),
            bmff(b"colr", b"nclx\0\x01\0\x0d\0\x06\x80"),
            bmff(b"irot", &[1]),
        ]
        .concat();
        let ipma = full_box(
            b"ipma",
            0,
            0,
            &[
                &[0, 0, 0, 2, 0, 1, 4, 0x81, 2, 3, 0x84][..],
                &[0, 4, 2, 0x81, 2],
            ]
            .concat(),
        );
        let url = full_box(b"url ", 0, 1, b"");
        let boxes: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (
                b"hdlr",
                full_box(b"hdlr", 0, 0, &[&[0; 4][..], b"pict", &[0; 13]].concat()),
            ),
            (
                b"dinf",
                bmff(
                    b"dinf",
                    &full_box(b"dref", 0, 0, &[&1u32.to_be_bytes()[..], &url].concat()),
                ),
            ),
            (b"pitm", full_box(b"pitm", 0, 0, &[0, 1])),
            (b"iinf", full_box(b"iinf", 0, 0, &iinf)),
            (b"iref", full_box(b"iref", 0, 0, &iref)),
            (
                b"iprp",
                bmff(b"iprp", &[bmff(b"ipco", &ipco), ipma].concat()),
            ),
            (b"iloc", full_box(b"iloc", 0, 0, &iloc)),
        ];
        let mut content = vec![0; 4];
        for kind in order {
            content.extend_from_slice(&boxes.iter().find(|(k, _)| k == kind).unwrap().1);
        }
        bmff(b"meta", &content)
    };
    let mdat_at = ftyp.len() + meta(0).len();
    let mut file = ftyp;
    file.extend_from_slice(&meta(mdat_at));
    file.extend_from_slice(&bmff(b"mdat", &datas.concat()));
    file.extend_from_slice(after);
    let mut at = mdat_at + 8;
    let ranges = datas
        .iter()
        .map(|d| {
            at += d.len();
            at - d.len()..at
        })
        .collect();
    (file, ranges)
}

#[test]
fn remove_metadata_strips_a_heic_in_place_and_keeps_its_pictures() {
    use MetaField::*;
    let dir = TestDir::new("strip-heic");
    let primary: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    for (name, order, brand) in [
        ("photo.heic", &LIBHEIF[..], b"heic"),
        ("IMG_0001.HEIC", &IPHONE[..], b"heic"),
        ("pic.heif", &LIBHEIF[..], b"mif1"),
        ("camera.hif", &IPHONE[..], b"heix"),
        // AVIF content named .heic: nothing is decoded, so it's stripped.
        ("avif.heic", &LIBHEIF[..], b"avif"),
    ] {
        let (src, at) = heif(brand, order, &primary, b"");
        let outcome = strip(&dir, name, &src).unwrap();
        let (stem, ext) = name.split_once('.').unwrap();
        assert_eq!(
            outcome.outputs,
            [dir.join(&format!("{stem} No Metadata.{ext}"))]
        );
        assert_eq!(
            outcome.notes,
            [Note::MetadataRemoved(vec![Location, Camera, Xmp])],
            "{name}"
        );
        let out = fs::read(&outcome.outputs[0]).unwrap();
        // The same size; the pictures where they were, byte for byte; the
        // Exif and the XMP zeroed.
        assert_eq!(out.len(), src.len(), "{name}");
        assert_eq!(out[at[0].clone()], src[at[0].clone()]);
        assert_eq!(out[at[3].clone()], src[at[3].clone()]);
        assert!(out[at[1].start..at[2].end].iter().all(|&b| b == 0));
        assert!(!out.windows(6).any(|w| w == b"Secret"), "{name}");
        assert!(
            !out.windows(4).any(|w| w == b"Exif" || w == b"cdsc"),
            "{name}"
        );
        // meta shrank behind a free box; ftyp and mdat are as they were.
        let meta_end = 8
            + u32::from_be_bytes(src[..4].try_into().unwrap()) as usize
            + u32::from_be_bytes(src[24..28].try_into().unwrap()) as usize
            - 8;
        assert_eq!(out[..24], src[..24]);
        assert_eq!(out[meta_end..at[0].start], src[meta_end..at[0].start]);
        assert!(out.windows(4).any(|w| w == b"free"));
        assert!(out.windows(4).any(|w| w == b"thmb"));
    }
}

#[test]
fn remove_metadata_on_a_portrait_heic_keeps_its_orientation_in_its_exif() {
    use MetaField::*;
    let dir = TestDir::new("strip-heic-portrait");
    let (src, at) = turned_heif(6, b"heic", &IPHONE, &[0xA1; 100], b"");
    let outcome = strip(&dir, "portrait.heic", &src).unwrap();
    assert_eq!(
        outcome.notes,
        [Note::MetadataRemoved(vec![Location, Camera, Xmp])]
    );
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert_eq!(out.len(), src.len());
    // The Exif item stays, where it was: its header offset, `Exif\0\0`
    // and a big-endian TIFF of Orientation 6 alone, then zeros.
    let data = &out[at[1].clone()];
    let mut payload = b"\0\0\0\x06Exif\0\0MM\0\x2a\0\0\0\x08\0\x01".to_vec();
    payload.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
    assert_eq!(data[..payload.len()], payload[..]);
    assert!(data[payload.len()..].iter().all(|&b| b == 0));
    assert!(out.windows(4).any(|w| w == b"Exif" || w == b"cdsc"));
    assert!(!out.windows(6).any(|w| w == b"Secret"));
    assert!(!out.windows(5).any(|w| w == b"Canon"));
}

#[test]
fn remove_metadata_cuts_a_motion_photo_and_says_so() {
    use MetaField::*;
    let dir = TestDir::new("strip-heic-motion");
    let mut after = bmff(b"mpvd", b"SecretVideo");
    after.extend_from_slice(b"\0\0\0");
    let (src, at) = heif(b"heic", &IPHONE, &[0xA1; 100], &after);
    let outcome = strip(&dir, "motion.heic", &src).unwrap();
    assert_eq!(
        outcome.notes,
        [Note::MetadataRemoved(vec![Location, Camera, Xmp, Embedded])]
    );
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert_eq!(out.len(), src.len() - after.len());
    assert_eq!(out[at[0].clone()], src[at[0].clone()]);
}

#[test]
fn remove_metadata_on_a_heic_with_none_copies_it() {
    let dir = TestDir::new("strip-heic-none");
    let (mut src, _) = heif(b"heic", &LIBHEIF, &[0xA1; 100], b"");
    // Items 2 and 3 become pictures: nothing is metadata. Their types
    // are the first two of these in the file, in meta.
    let types: Vec<usize> = src
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == b"Exif" || *w == b"mime")
        .map(|(i, _)| i)
        .take(2)
        .collect();
    for i in types {
        src[i..i + 4].copy_from_slice(b"hvc1");
    }
    let outcome = strip(&dir, "plain.heic", &src).unwrap();
    assert_eq!(outcome.notes, [Note::MetadataRemoved(Vec::new())]);
    assert_eq!(fs::read(&outcome.outputs[0]).unwrap(), src);
}

#[test]
fn remove_metadata_refuses_a_heic_it_cant_strip_and_writes_nothing() {
    let dir = TestDir::new("strip-heic-refused");
    let (src, _) = heif(b"heic", &LIBHEIF, &[0xA1; 100], b"");
    let sequence = [src.clone(), bmff(b"moov", b"")].concat();
    // The primary item is the Exif.
    let mut exif_primary = src.clone();
    let pitm = exif_primary.windows(4).position(|w| w == b"pitm").unwrap();
    exif_primary[pitm + 8..pitm + 10].copy_from_slice(&[0, 2]);
    for (name, bytes, kind, message) in [
        (
            "burst.heic",
            sequence,
            ErrorKind::NotYetSupported,
            "burst.heic is an image sequence. Remove Metadata works on still HEIC photos for now.",
        ),
        (
            "odd.heic",
            exif_primary,
            ErrorKind::NotYetSupported,
            "odd.heic is laid out in a way Remove Metadata can't handle yet.",
        ),
        (
            "cut.heic",
            src[..src.len() - 50].to_vec(),
            ErrorKind::DecodeFailed,
            "cut.heic isn't a readable HEIC image.",
        ),
    ] {
        let err = strip(&dir, name, &bytes).unwrap_err();
        assert_eq!(err.kind, kind, "{name}: {}", err.detail);
        assert_eq!(err.user_message, message);
        fs::remove_file(dir.join(name)).unwrap();
    }
}

/// A `w`×`h` photo-like picture: a gradient with fine texture, which a
/// lower quality makes smaller.
fn photo(w: u32, h: u32) -> RgbImage {
    RgbImage::from_fn(w, h, |x, y| {
        let n = (x.wrapping_mul(7919) ^ y.wrapping_mul(104_729)) % 23;
        Rgb([
            u8::try_from((x * 255 / w + n) % 256).unwrap(),
            u8::try_from((y * 255 / h + 2 * n) % 256).unwrap(),
            u8::try_from((128 + 3 * n) % 256).unwrap(),
        ])
    })
}

fn jpeg_of(img: &RgbImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(img)
        .unwrap();
    out
}

/// A little-endian EXIF with orientation 6 and an Exif IFD whose
/// PixelXDimension and PixelYDimension are `w` and `h`.
fn camera_exif(w: u32, h: u32) -> Vec<u8> {
    let mut t = b"Exif\0\0II*\0\x08\0\0\0".to_vec();
    let entry = |t: &mut Vec<u8>, tag: u16, kind: u16, field: [u8; 4]| {
        t.extend_from_slice(&tag.to_le_bytes());
        t.extend_from_slice(&kind.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&field);
    };
    t.extend_from_slice(&2u16.to_le_bytes());
    entry(&mut t, 0x0112, 3, [6, 0, 0, 0]);
    // The Exif IFD after IFD0: 8 + 2 + 24 + 4 = 38.
    entry(&mut t, 0x8769, 4, 38u32.to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes());
    t.extend_from_slice(&2u16.to_le_bytes());
    entry(&mut t, 0xA002, 4, w.to_le_bytes());
    entry(&mut t, 0xA003, 4, h.to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes());
    seg(0xE1, &t)
}

fn compress_op(preset: grape_core::CompressPreset) -> Operation {
    Operation::Tool {
        tool: ToolKey::Compress,
        params: grape_core::ToolParams {
            compress: preset,
            ..grape_core::ToolParams::DEFAULT
        },
    }
}

fn compress(
    dir: &TestDir,
    name: &str,
    bytes: &[u8],
    preset: grape_core::CompressPreset,
) -> Result<JobOutcome, GrapeError> {
    run(dir, name, bytes, compress_op(preset))
}

/// The Exif IFD's pixel dimensions in a JPEG Grape wrote.
fn exif_dimensions(jpeg: &[u8]) -> (u32, u32) {
    let at = jpeg.windows(6).position(|w| w == b"Exif\0\0").unwrap() + 6;
    let tiff = &jpeg[at..];
    let u16le = |i: usize| u16::from_le_bytes([tiff[i], tiff[i + 1]]);
    let u32le = |i: usize| u32::from_le_bytes([tiff[i], tiff[i + 1], tiff[i + 2], tiff[i + 3]]);
    let exif_ifd = usize::try_from(u32le(8 + 2 + 12 + 8)).unwrap();
    assert_eq!(u16le(exif_ifd + 2), 0xA002);
    (u32le(exif_ifd + 2 + 8), u32le(exif_ifd + 2 + 12 + 8))
}

#[test]
fn compress_shrinks_a_jpeg_and_carries_its_metadata() {
    use grape_core::CompressPreset::{Balanced, High, Small};
    let dir = TestDir::new("compress-jpeg");
    let source = jpeg_of(&photo(2400, 1600), 95);
    let mut xmp = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    xmp.extend_from_slice(b"<x:xmpmeta>kept</x:xmpmeta>");
    let src = with(
        &source,
        &[
            camera_exif(2400, 1600),
            seg(0xE1, &xmp),
            seg(0xFE, b"kept comment"),
        ],
        b"",
    );
    // (preset, width, height)
    for (preset, w, h) in [
        (Small, 2048, 1365),
        (Balanced, 2400, 1600),
        (High, 2400, 1600),
    ] {
        let outcome = compress(&dir, "photo.jpg", &src, preset).unwrap();
        assert_eq!(outcome.outputs, [dir.join("photo Compressed.jpg")]);
        let out = fs::read(&outcome.outputs[0]).unwrap();
        assert_eq!(
            outcome.notes,
            [Note::Compressed {
                before: src.len() as u64,
                after: out.len() as u64
            }],
            "{preset:?}"
        );
        assert!(out.len() * 10 <= src.len() * 9, "{preset:?}");
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!((img.width(), img.height()), (w, h), "{preset:?}");
        // Stored as it was: the EXIF still turns it, and says its size.
        assert_eq!(orientation(&out), Orientation::Rotate90);
        assert_eq!(exif_dimensions(&out), (w, h));
        for kept in [&b"<x:xmpmeta>kept"[..], b"kept comment"] {
            assert!(out.windows(kept.len()).any(|x| x == kept), "{preset:?}");
        }
        fs::remove_file(&outcome.outputs[0]).unwrap();
        fs::remove_file(dir.join("photo.jpg")).unwrap();
    }
}

#[test]
fn compress_leaves_a_compact_jpeg_alone_before_decoding() {
    use grape_core::CompressPreset::{Balanced, Small};
    let dir = TestDir::new("compress-jpeg-none");
    // Quality 70 and under Small's 2048 px: nothing to gain.
    let small = jpeg_of(&photo(800, 600), 70);
    for (preset, message) in [
        (Small, "pic.jpg is already as small as Grape makes it."),
        (Balanced, "pic.jpg is already as small as Grape makes it."),
    ] {
        let err = compress(&dir, "pic.jpg", &small, preset).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
        assert_eq!(err.user_message, message);
        fs::remove_file(dir.join("pic.jpg")).unwrap();
    }
    // At 2400 px, Small would still scale it.
    let big = jpeg_of(&photo(2400, 1600), 75);
    let err = compress(&dir, "big.jpg", &big, Balanced).unwrap_err();
    assert_eq!(err.user_message, "big.jpg is already compact. Try Small.");
}

#[test]
fn compress_drops_xmp_that_describes_a_gain_map() {
    let dir = TestDir::new("compress-jpeg-gainmap");
    let mut xmp = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    xmp.extend_from_slice(b"<x:xmpmeta>hdrgm</x:xmpmeta>");
    let src = with(
        &jpeg_of(&photo(1200, 800), 95),
        &[seg(0xE1, &xmp)],
        b"\xFF\xD8 a gain map",
    );
    let outcome = compress(&dir, "hdr.jpg", &src, grape_core::CompressPreset::Balanced).unwrap();
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert!(!out.windows(5).any(|w| w == b"hdrgm"));
    assert!(!out.windows(10).any(|w| w == b"a gain map"));
}

#[test]
fn compress_refuses_a_cmyk_jpeg() {
    let dir = TestDir::new("compress-cmyk");
    // A frame with 4 components; the rest only has to be well formed.
    let mut src = vec![0xFF, 0xD8];
    src.extend(seg(0xDB, &[0; 65]));
    src.extend(seg(
        0xC0,
        &[
            8, 0, 8, 0, 8, 4, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 4, 0x11, 0,
        ],
    ));
    src.extend(seg(0xDA, &[1, 1, 0, 0, 63, 0]));
    src.extend_from_slice(&[0x12, 0x34, 0xFF, 0xD9]);
    let err = compress(&dir, "print.jpg", &src, grape_core::CompressPreset::Small).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "print.jpg is a print (CMYK) JPEG, which Compress can't keep yet."
    );
}

#[test]
fn compress_refuses_a_jpeg_over_the_limit_before_reading_it() {
    let dir = TestDir::new("compress-huge");
    // A JPEG start whose headers would be refused as broken (a scan before
    // any frame) if they were read, then zeros to just past 512 MiB:
    // sparse where the file system allows, so the test stays cheap.
    let mut head = vec![0xFF, 0xD8];
    head.extend(seg(0xDA, &[1, 1, 0, 0, 63, 0]));
    let (src, job) = plan_one(
        &dir,
        "big.jpg",
        &head,
        compress_op(grape_core::CompressPreset::Balanced),
    );
    fs::OpenOptions::new()
        .write(true)
        .open(&src)
        .unwrap()
        .set_len(512 * 1024 * 1024 + 1)
        .unwrap();
    let mut reports = 0;
    let err = run_job(&job, &EngineCtx::default(), &mut |_| reports += 1).unwrap_err();
    assert_eq!(
        err.user_message, "big.jpg is too large to convert.",
        "{}",
        err.detail
    );
    assert_eq!(reports, 0);
    assert!(!planned_file(&job).exists());
    assert_no_leftovers(&dir.path);
    assert_eq!(fs::metadata(&src).unwrap().len(), 512 * 1024 * 1024 + 1);
}

#[test]
fn compress_says_a_png_too_wide_to_read_is_too_large() {
    let dir = TestDir::new("compress-wide");
    // One gray row of 170 M pixels: more than the header read may hold.
    // The image data needn't be whole: the size is refused before it.
    let mut ihdr = 170_000_000u32.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    let mut src = b"\x89PNG\r\n\x1a\n".to_vec();
    src.extend(chunk(b"IHDR", &ihdr));
    src.extend(chunk(b"IDAT", &[0x78, 0x9C, 0x03, 0x00]));
    src.extend(chunk(b"IEND", &[]));
    let err = compress(&dir, "wide.png", &src, grape_core::CompressPreset::High).unwrap_err();
    assert_eq!(
        err.user_message, "wide.png is too large to convert.",
        "{}",
        err.detail
    );
}

#[test]
fn compress_repacks_a_png_exactly_and_keeps_its_text() {
    let dir = TestDir::new("compress-png");
    // A screenshot-like picture written fast and as 16-bit RGBA, though
    // it's opaque and 8-bit.
    let shot = image::ImageBuffer::from_fn(160, 90, |x, y| {
        let v = u16::from(u8::try_from((x / 20 + y / 15) % 4 * 60).unwrap()) * 257;
        image::Rgba([v, v / 2, 65535 - v, u16::MAX])
    });
    let mut wide = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut wide,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(
        bytemuck_u16(shot.as_raw()).as_slice(),
        160,
        90,
        image::ExtendedColorType::Rgba16,
    )
    .unwrap();
    let src = png_with(&wide, &[chunk(b"tEXt", b"Title\0kept title")]);
    for name in ["shot.png", "shot.jpg"] {
        let outcome = compress(&dir, name, &src, grape_core::CompressPreset::Small).unwrap();
        let out = fs::read(&outcome.outputs[0]).unwrap();
        assert_eq!(
            outcome.notes,
            [Note::Compressed {
                before: src.len() as u64,
                after: out.len() as u64
            }]
        );
        assert!(out.len() * 20 <= src.len() * 19);
        assert_eq!(
            image::load_from_memory(&out)
                .unwrap()
                .to_rgba16()
                .into_raw(),
            shot.clone().into_raw()
        );
        assert!(out.windows(10).any(|w| w == b"kept title"));
        fs::remove_file(&outcome.outputs[0]).unwrap();
        fs::remove_file(dir.join(name)).unwrap();
    }
}

/// A scanner's TIFF, classic or BigTIFF: an uncompressed RGB page with an
/// Exif IFD, a GPS IFD, ICC, XMP and text tags, then an LZW 16-bit gray
/// page.
fn scan_tiff(big: bool) -> Vec<u8> {
    use tiff::encoder::{colortype, Compression, Rational, TiffEncoder, TiffKind};
    use tiff::tags::Tag;
    fn pages<K: TiffKind>(mut e: TiffEncoder<&mut Cursor<Vec<u8>>, K>) {
        let mut exif = e.extra_directory().unwrap();
        exif.write_tag(Tag::Unknown(0x9003), "2024:05:01 10:20:30")
            .unwrap();
        let exif = exif.finish_with_offsets().unwrap().offset;
        let mut gps = e.extra_directory().unwrap();
        gps.write_tag(Tag::Unknown(2), Rational { n: 48, d: 1 })
            .unwrap();
        let gps = gps.finish_with_offsets().unwrap().offset;
        let mut first = e.new_image::<colortype::RGB8>(300, 200).unwrap();
        let tags = first.encoder();
        tags.write_tag(Tag::Software, "Scanner 2000").unwrap();
        tags.write_tag(Tag::Orientation, 6u16).unwrap();
        tags.write_tag(Tag::IccProfile, &b"an ICC profile"[..])
            .unwrap();
        tags.write_tag(Tag::Unknown(700), &b"<x:xmpmeta>kept</x:xmpmeta>"[..])
            .unwrap();
        tags.write_tag(Tag::ExifDirectory, exif).unwrap();
        tags.write_tag(Tag::GpsDirectory, gps).unwrap();
        first.write_data(&photo(300, 200).into_raw()).unwrap();
        let mut e = e.with_compression(Compression::Lzw);
        let gray: Vec<u16> = (0..120 * 80).map(|i| (i % 120 * 400) as u16).collect();
        e.write_image::<colortype::Gray16>(120, 80, &gray).unwrap();
    }
    let mut out = Cursor::new(Vec::new());
    if big {
        pages(TiffEncoder::new_big(&mut out).unwrap());
    } else {
        pages(TiffEncoder::new(&mut out).unwrap());
    }
    out.into_inner()
}

#[test]
fn compress_deflates_every_page_of_a_tiff_and_keeps_its_tags() {
    use tiff::tags::Tag;
    let dir = TestDir::new("compress-tiff");
    for (big, name) in [(false, "scan.tif"), (true, "Scan.TIF")] {
        let src = scan_tiff(big);
        let mut outputs = Vec::new();
        // Lossless at every preset: the same file each time.
        for preset in [
            grape_core::CompressPreset::Small,
            grape_core::CompressPreset::Balanced,
            grape_core::CompressPreset::High,
        ] {
            let outcome = compress(&dir, name, &src, preset).unwrap();
            let (stem, ext) = name.split_once('.').unwrap();
            assert_eq!(
                outcome.outputs,
                [dir.join(&format!("{stem} Compressed.{ext}"))]
            );
            let out = fs::read(&outcome.outputs[0]).unwrap();
            assert_eq!(
                outcome.notes,
                [Note::Compressed {
                    before: src.len() as u64,
                    after: out.len() as u64
                }]
            );
            fs::remove_file(&outcome.outputs[0]).unwrap();
            fs::remove_file(dir.join(name)).unwrap();
            outputs.push(out);
        }
        assert!(outputs.iter().all(|out| *out == outputs[0]));
        let out = &outputs[0];
        assert!(
            out.len() * 20 <= src.len() * 19,
            "{} of {}",
            out.len(),
            src.len()
        );
        assert_eq!(out[..4], src[..4]);
        assert_eq!(tiff_pages(out), tiff_pages(&src), "{name}");
        // Deflated and differenced; every other tag kept.
        let mut decoder = tiff::decoder::Decoder::new(Cursor::new(out)).unwrap();
        assert_eq!(decoder.get_tag_u32(Tag::Compression).unwrap(), 8);
        assert_eq!(decoder.get_tag_u32(Tag::Predictor).unwrap(), 2);
        assert_eq!(decoder.get_tag_u32(Tag::Orientation).unwrap(), 6);
        assert_eq!(
            decoder.get_tag_ascii_string(Tag::Software).unwrap(),
            "Scanner 2000"
        );
        for kept in [Tag::IccProfile, Tag::ExifDirectory, Tag::GpsDirectory] {
            assert!(decoder.find_tag(kept).unwrap().is_some(), "{kept:?}");
        }
        for kept in [&b"2024:05:01 10:20:30"[..], b"<x:xmpmeta>kept"] {
            assert!(out.windows(kept.len()).any(|w| w == kept));
        }
        decoder.next_image().unwrap();
        assert_eq!(decoder.get_tag_u32(Tag::Compression).unwrap(), 8);
    }
}

#[test]
fn compress_leaves_a_tiff_it_cant_shrink_alone_and_refuses_what_it_cant_rewrite() {
    use tiff::encoder::{colortype, DirectoryEncoder, Ifd, TiffEncoder, TiffKindStandard};
    use tiff::tags::Tag;
    type Tags<'a, 'b> = DirectoryEncoder<'a, &'b mut Cursor<Vec<u8>>, TiffKindStandard>;
    /// A gray 64 × 64 page, uncompressed, with the tags `tag` writes.
    fn one(tag: impl FnOnce(&mut Tags)) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut e = TiffEncoder::new(&mut out).unwrap();
        let mut image = e.new_image::<colortype::Gray8>(64, 64).unwrap();
        tag(image.encoder());
        let gray: Vec<u8> = (0..64 * 64).map(|i| (i % 64 * 4) as u8).collect();
        image.write_data(&gray).unwrap();
        out.into_inner()
    }
    let dir = TestDir::new("compress-tiff-none");
    let jpeg = one(|t| t.write_tag(Tag::Compression, 7u16).unwrap());
    let dng = one(|t| {
        t.write_tag(Tag::Unknown(50706), &[1u8, 4, 0, 0][..])
            .unwrap()
    });
    // A GlobalParametersIFD (400) typed IFD, which Compress doesn't follow.
    let pointer = one(|t| t.write_tag(Tag::Unknown(400), Ifd(8)).unwrap());
    let plain = one(|t| t.write_tag(Tag::Software, "x").unwrap());
    let packed = compress(&dir, "plain.tif", &plain, grape_core::CompressPreset::High).unwrap();
    let packed = fs::read(&packed.outputs[0]).unwrap();
    for (name, bytes, kind, message) in [
        (
            "photo.tif",
            jpeg,
            ErrorKind::NoChange,
            "photo.tif is already as small as Grape makes it.",
        ),
        (
            "again.tif",
            packed,
            ErrorKind::NoChange,
            "again.tif is already as small as Grape makes it.",
        ),
        (
            "raw.tif",
            dng,
            ErrorKind::NotYetSupported,
            "raw.tif is a camera raw (DNG) file, which Grape can't change yet.",
        ),
        (
            "globe.tif",
            pointer,
            ErrorKind::NotYetSupported,
            "globe.tif has data Compress can't move yet.",
        ),
        (
            "counts.tif",
            miscounted_tiff(),
            ErrorKind::NotYetSupported,
            "counts.tif says the wrong size for some of its image data, which Grape can't rewrite yet.",
        ),
        (
            "cut.tif",
            scan_tiff(false)[..2_000].to_vec(),
            ErrorKind::DecodeFailed,
            "cut.tif isn't a readable TIFF image.",
        ),
    ] {
        let err = compress(&dir, name, &bytes, grape_core::CompressPreset::High).unwrap_err();
        assert_eq!(err.kind, kind, "{name}: {}", err.detail);
        assert_eq!(err.user_message, message);
        fs::remove_file(dir.join(name)).unwrap();
    }
}

/// `samples` as bytes in native order, as image's encoder takes 16-bit
/// pixels.
fn bytemuck_u16(samples: &[u16]) -> Vec<u8> {
    samples.iter().flat_map(|v| v.to_ne_bytes()).collect()
}

#[test]
fn compress_leaves_an_optimal_png_alone_and_refuses_an_animated_one() {
    let dir = TestDir::new("compress-png-none");
    let tiny = plain_png();
    let first = compress(&dir, "tiny.png", &tiny, grape_core::CompressPreset::High).unwrap();
    let packed = fs::read(&first.outputs[0]).unwrap();
    // Its own output can't be made 5 % smaller again.
    let err = compress(&dir, "again.png", &packed, grape_core::CompressPreset::High).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "again.png is already as small as Grape makes it."
    );
    let apng = png_with(&tiny, &[chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0])]);
    let err = compress(&dir, "anim.png", &apng, grape_core::CompressPreset::High).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "anim.png is animated. Compress works on still PNGs for now."
    );
}

/// Runs `op` on `bytes` named `name`, cancelling at the `nth` progress
/// report: it must say Cancelled and leave nothing.
fn cancelled_at(name: &str, bytes: &[u8], op: Operation, nth: usize) {
    let dir = TestDir::new("cancel-midway");
    let (src, job) = plan_one(&dir, name, bytes, op);
    let before = SourceState::of(&src);
    let ctx = EngineCtx::default();
    let cancel = ctx.cancel.clone();
    let mut reports = 0;
    let err = run_job(&job, &ctx, &mut |_| {
        reports += 1;
        if reports == nth {
            cancel.cancel();
        }
    })
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{name}: {}", err.detail);
    before.assert_unchanged();
    assert!(!planned_file(&job).exists());
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_cancel_midway_says_cancelled_and_leaves_nothing() {
    let big = jpeg_of(&photo(1200, 900), 95);
    assert!(big.len() > 2 * 64 * 1024);
    // The strip's second report comes after its temp file exists.
    cancelled_at(
        "photo.jpg",
        &big,
        Operation::tool(ToolKey::RemoveMetadata),
        2,
    );
    // Compress's first report is before it decodes.
    cancelled_at(
        "photo.jpg",
        &big,
        compress_op(grape_core::CompressPreset::Small),
        1,
    );
    cancelled_at(
        "plain.png",
        &plain_png(),
        compress_op(grape_core::CompressPreset::High),
        1,
    );
    // A TIFF: before its walk, and while its strips are copied.
    let mut tiff = Cursor::new(Vec::new());
    tiff::encoder::TiffEncoder::new(&mut tiff)
        .unwrap()
        .write_image::<tiff::encoder::colortype::RGB8>(600, 600, &photo(600, 600).into_raw())
        .unwrap();
    for nth in [1, 4] {
        cancelled_at(
            "scan.tif",
            tiff.get_ref(),
            Operation::tool(ToolKey::RemoveMetadata),
            nth,
        );
    }
    // Compress on a TIFF: before its walk, and while its strips are
    // deflated.
    for nth in [1, 3] {
        cancelled_at(
            "scan.tif",
            tiff.get_ref(),
            compress_op(grape_core::CompressPreset::High),
            nth,
        );
    }
    // A HEIC: before it's read, and while its mdat is copied.
    let (heic, _) = heif(b"heic", &LIBHEIF, &vec![0xA1; 600_000], b"");
    for nth in [1, 4] {
        cancelled_at(
            "photo.heic",
            &heic,
            Operation::tool(ToolKey::RemoveMetadata),
            nth,
        );
    }
}

#[test]
fn the_content_decides_what_each_tool_reads() {
    let dir = TestDir::new("misnamed");
    let jpeg = with(&plain_jpeg(), &[exif(1)], b"");
    // A JPEG named .png is stripped and compressed as a JPEG, and keeps
    // its name.
    let out = strip(&dir, "photo.png", &jpeg).unwrap();
    assert!(out.outputs[0].to_string_lossy().ends_with(".png"));
    assert!(fs::read(&out.outputs[0])
        .unwrap()
        .starts_with(&[0xFF, 0xD8]));
    let big = jpeg_of(&photo(2400, 1800), 95);
    let out = compress(&dir, "big.png", &big, grape_core::CompressPreset::Small).unwrap();
    assert!(fs::read(&out.outputs[0])
        .unwrap()
        .starts_with(&[0xFF, 0xD8]));
    // A WebP named .png is stripped as a WebP.
    let webp = riff(&[riff_chunk(b"VP8L", b"\x2f\0\0\0\0")]);
    let out = strip(&dir, "pic.png", &webp).unwrap();
    assert!(fs::read(&out.outputs[0]).unwrap().starts_with(b"RIFF"));
    // A TIFF named .jpg is stripped as a TIFF.
    let out = strip(&dir, "scan.jpg", &tagged_tiff(false)).unwrap();
    assert!(out.outputs[0].to_string_lossy().ends_with(".jpg"));
    assert_eq!(tiff_pages(&fs::read(&out.outputs[0]).unwrap()).len(), 2);
    // A HEIC named .jpg is stripped as a HEIC, and a JPEG named .heic as
    // a JPEG.
    let (heic, _) = heif(b"heic", &LIBHEIF, &[0xA1; 100], b"");
    let out = strip(&dir, "phone.jpg", &heic).unwrap();
    assert!(out.outputs[0].to_string_lossy().ends_with(".jpg"));
    let bytes = fs::read(&out.outputs[0]).unwrap();
    assert_eq!(bytes.len(), heic.len());
    assert_eq!(bytes[4..8], *b"ftyp");
    let out = strip(&dir, "photo.heic", &jpeg).unwrap();
    assert!(fs::read(&out.outputs[0])
        .unwrap()
        .starts_with(&[0xFF, 0xD8]));
    // A TIFF named .png is compressed as a TIFF, and keeps its name.
    let out = compress(
        &dir,
        "scan.png",
        &scan_tiff(false),
        grape_core::CompressPreset::High,
    )
    .unwrap();
    assert!(out.outputs[0].to_string_lossy().ends_with(".png"));
    assert!(fs::read(&out.outputs[0]).unwrap().starts_with(b"II*\0"));
    // Compress reads only JPEG, PNG and TIFF: a GIF named .jpg can't be
    // read.
    let gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\0\0\0\xff\xff\xff,\0\0\0\0\x01\0\x01\0\0\x02\x02\x44\x01\0;";
    let err = compress(&dir, "anim.jpg", gif, grape_core::CompressPreset::Small).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
}

#[test]
fn an_image_tool_writes_only_its_sources_format() {
    let dir = TestDir::new("tool-format");
    let src = dir.join("a.jpg");
    fs::write(&src, plain_jpeg()).unwrap();
    for (tool, out) in [
        (ToolKey::Compress, "a Compressed.webp"),
        (ToolKey::RemoveMetadata, "a No Metadata.png"),
    ] {
        let job = grape_core::Job {
            inputs: vec![src.clone()],
            output: grape_core::JobOutput::File(dir.join(out)),
            op: Operation::tool(tool),
            used_fallback: false,
        };
        let err = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported, "{tool:?}");
        assert!(!dir.join(out).exists());
    }
    assert_no_leftovers(&dir.path);
}
