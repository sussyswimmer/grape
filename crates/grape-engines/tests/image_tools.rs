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

/// A classic gray 300 × 100 TIFF, uncompressed, every byte of its picture
/// at 8, that lists strips 3,000 bytes apart with `counts`, in strips of
/// `rows` rows or with no RowsPerStrip: libtiff reads as many strips as its
/// rows make (one with none), whatever's listed, and checks their counts.
fn listed_tiff(rows: Option<u32>, counts: &[u32]) -> Vec<u8> {
    let n = u32::try_from(counts.len()).unwrap();
    let mut entries: Vec<(u16, u16, u32, u32)> = vec![
        (256, 4, 1, 300),
        (257, 4, 1, 100),
        (258, 3, 1, 8),
        (262, 3, 1, 1),
        (273, 4, n, 0),
    ];
    entries.extend(rows.map(|rows| (278, 4, 1, rows)));
    entries.push((279, 4, n, 0));
    let ifd = 8 + 30_000;
    let offsets = ifd + 2 + 12 * u32::try_from(entries.len()).unwrap() + 4;
    let mut file = b"II*\0".to_vec();
    file.extend(ifd.to_le_bytes());
    file.extend((0..30_000).map(|i| (i % 251) as u8));
    file.extend(u16::try_from(entries.len()).unwrap().to_le_bytes());
    for (tag, kind, count, value) in entries {
        let value = match tag {
            273 => offsets,
            279 => offsets + 4 * n,
            _ => value,
        };
        file.extend(tag.to_le_bytes());
        file.extend(kind.to_le_bytes());
        file.extend(count.to_le_bytes());
        file.extend(value.to_le_bytes());
    }
    file.extend([0; 4]);
    file.extend((0..n).flat_map(|i| (8 + 3_000 * i).to_le_bytes()));
    file.extend(counts.iter().flat_map(|c| c.to_le_bytes()));
    file
}

#[test]
fn both_tiff_tools_refuse_counts_libtiff_reads_for_other_strips() {
    // Ten listed where libtiff, with no RowsPerStrip, reads one strip, and
    // reads the whole picture from it, its count short ("Bogus"); and two
    // listed where it reads ten of 10 rows, the eight missing at 0, and
    // ignores counts whose first two differ ("Wrong").
    let mut more = [3_000; 10];
    more[3] = 10;
    let dir = TestDir::new("tiff-listed");
    for (name, bytes) in [
        ("more.tif", listed_tiff(None, &more)),
        ("fewer.tif", listed_tiff(Some(10), &[3_000, 1_500])),
    ] {
        let ops = [
            Operation::tool(ToolKey::RemoveMetadata),
            compress_op(grape_core::CompressPreset::High),
        ];
        for op in ops {
            let err = run(&dir, name, &bytes, op).unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::NotYetSupported,
                "{name}: {}",
                err.detail
            );
            assert_eq!(
                err.user_message,
                format!(
                    "{name} says the wrong size for some of its image data, which Grape can't rewrite yet."
                )
            );
            fs::remove_file(dir.join(name)).unwrap();
        }
    }
}

/// The bytes of `n`, little-endian, `size` of them.
fn le(n: u64, size: usize) -> Vec<u8> {
    n.to_le_bytes()[..size].to_vec()
}

/// An entry for [`typed_tiff`]: its tag, type and value.
type Change = (u16, u16, Vec<u8>);

/// [`listed_tiff`]'s gray 300 × 100 picture, its ten strips 3,000 bytes
/// apart counted `counts`, with no RowsPerStrip, classic or BigTIFF (the
/// picture at 16 there), with `changes` to its entries: each
/// `(tag, type, value)` takes the place of the entry for its tag, or is
/// added; a value its entry can't hold goes after the IFD.
fn typed_tiff(big: bool, counts: &[u64; 10], changes: &[Change]) -> Vec<u8> {
    let (header, word, number) = if big { (16, 8, 16) } else { (8, 4, 4) };
    let mut entries: Vec<Change> = vec![
        (256, 4, le(300, 4)),
        (257, 4, le(100, 4)),
        (258, 3, le(8, 2)),
        (262, 3, le(1, 2)),
    ];
    for (tag, kind, value) in changes {
        entries.retain(|e| e.0 != *tag);
        entries.push((*tag, *kind, value.clone()));
    }
    let offsets = (0..10).flat_map(|i| le(header + 3_000 * i, word)).collect();
    entries.push((273, number, offsets));
    entries.push((
        279,
        number,
        counts.iter().flat_map(|&c| le(c, word)).collect(),
    ));
    let picture: Vec<u8> = (0..30_000).map(|i| (i % 251) as u8).collect();
    laid_out(big, &picture, entries)
}

/// A TIFF, classic or BigTIFF, of `data` right after its header, then one
/// IFD of `entries` (each `(tag, type, value)`, sorted here), then the
/// values its entries can't hold.
fn laid_out(big: bool, data: &[u8], mut entries: Vec<Change>) -> Vec<u8> {
    let (header, word) = if big { (16, 8) } else { (8, 4) };
    entries.sort_by_key(|e| e.0);
    let size = |kind: u16| match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 13 => 4,
        _ => 8,
    };
    let ifd = header + data.len() as u64;
    let (count_len, entry_len) = if big { (8, 20) } else { (2, 12) };
    let mut value_at = ifd + count_len + entry_len * entries.len() as u64 + word as u64;
    let mut file = if big {
        [&b"II+\0\x08\0\0\0"[..], &le(ifd, 8)].concat()
    } else {
        [&b"II*\0"[..], &le(ifd, 4)].concat()
    };
    file.extend_from_slice(data);
    file.extend(le(entries.len() as u64, count_len as usize));
    let mut values: Vec<u8> = Vec::new();
    for (tag, kind, value) in &entries {
        file.extend(tag.to_le_bytes());
        file.extend(kind.to_le_bytes());
        file.extend(le(value.len() as u64 / size(*kind), word));
        if value.len() <= word {
            let mut field = value.clone();
            field.resize(word, 0);
            file.extend(field);
        } else {
            file.extend(le(value_at, word));
            value_at += value.len() as u64;
            values.extend(value);
        }
    }
    file.extend(le(0, word));
    file.extend(values);
    file
}

/// A TIFF, classic or BigTIFF, of `data` as one strip right after its
/// header, counted as long as it is unless `entries` count it, and
/// `entries` ([`laid_out`]).
fn one_strip(big: bool, data: &[u8], mut entries: Vec<Change>) -> Vec<u8> {
    let (header, word, number) = if big { (16, 8, 16) } else { (8, 4, 4) };
    entries.push((273, number, le(header, word)));
    if !entries.iter().any(|e| e.0 == 279) {
        entries.push((279, number, le(data.len() as u64, word)));
    }
    laid_out(big, data, entries)
}

#[test]
fn both_tiff_tools_read_a_tiffs_layout_as_libtiff_reads_it() {
    // The round-2 review's file: ten strips listed, the fourth counted 10,
    // where libtiff with no RowsPerStrip reads one, short of the picture,
    // and reads the whole picture from it. Its layout tags, Compression
    // (1) and BitsPerSample in types libtiff reads too (signed but not
    // negative, a LONG8 a classic file holds out of line, or a value for
    // each sample), or with an ImageDepth, which strips don't count, it
    // reads the same: refused, as the plain file is.
    let mut short = [3_000; 10];
    short[3] = 10;
    let cases: [(&str, bool, Vec<Change>); 20] = [
        ("plain.tif", false, vec![]),
        ("sshort-none.tif", false, vec![(259, 8, le(1, 2))]),
        ("slong-none.tif", false, vec![(259, 9, le(1, 4))]),
        ("sbyte-none.tif", false, vec![(259, 6, le(1, 1))]),
        ("long8-none.tif", false, vec![(259, 16, le(1, 8))]),
        ("slong8-none.tif", true, vec![(259, 17, le(1, 8))]),
        (
            "each-none.tif",
            false,
            vec![(259, 3, [le(1, 2), le(1, 2)].concat())],
        ),
        ("byte-bits.tif", false, vec![(258, 1, le(8, 1))]),
        ("sbyte-bits.tif", false, vec![(258, 6, le(8, 1))]),
        ("sshort-bits.tif", false, vec![(258, 8, le(8, 2))]),
        ("slong-bits.tif", false, vec![(258, 9, le(8, 4))]),
        ("slong8-bits.tif", true, vec![(258, 17, le(8, 8))]),
        ("depth.tif", false, vec![(32997, 4, le(1, 4))]),
        ("slong-length.tif", false, vec![(257, 9, le(100, 4))]),
        ("sshort-width.tif", false, vec![(256, 8, le(300, 2))]),
        ("long8-length.tif", false, vec![(257, 16, le(100, 8))]),
        ("sshort-rows.tif", false, vec![(278, 8, le(100, 2))]),
        ("sbyte-rows.tif", false, vec![(278, 6, le(100, 1))]),
        ("sshort-planar.tif", false, vec![(284, 8, le(1, 2))]),
        ("slong8-planar.tif", true, vec![(284, 17, le(1, 8))]),
    ];
    let dir = TestDir::new("tiff-typed");
    for (name, big, changes) in cases {
        let bytes = typed_tiff(big, &short, &changes);
        for op in [
            Operation::tool(ToolKey::RemoveMetadata),
            compress_op(grape_core::CompressPreset::High),
        ] {
            let err = run(&dir, name, &bytes, op).unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::NotYetSupported,
                "{name}: {}",
                err.detail
            );
            assert_eq!(
                err.user_message,
                format!(
                    "{name} says the wrong size for some of its image data, which Grape can't rewrite yet."
                )
            );
            fs::remove_file(dir.join(name)).unwrap();
        }
    }
    // One whose layout libtiff can't read, an ImageWidth typed IFD, as
    // which strips it would read can't be told; and ten whole strips of 10
    // rows, each counted right, with a PhotometricInterpretation libtiff
    // can't read (a RATIONAL, which it ignores), as how other readers size
    // them can't be told: refused as a layout Grape can't read.
    let rows = (278, 3, le(10, 2));
    for (name, bytes) in [
        (
            "ifd-width.tif",
            typed_tiff(false, &short, &[(256, 13, le(300, 4))]),
        ),
        (
            "rational-photometric.tif",
            typed_tiff(
                false,
                &[3_000; 10],
                &[rows.clone(), (262, 5, [le(1, 4), le(1, 4)].concat())],
            ),
        ),
    ] {
        for op in [
            Operation::tool(ToolKey::RemoveMetadata),
            compress_op(grape_core::CompressPreset::High),
        ] {
            let err = run(&dir, name, &bytes, op).unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::NotYetSupported,
                "{name}: {}",
                err.detail
            );
            assert_eq!(
                err.user_message,
                format!(
                    "{name} says how its image data is laid out in a way Grape can't read yet."
                )
            );
            fs::remove_file(dir.join(name)).unwrap();
        }
    }
    // In strips of 10 rows, BitsPerSample a BYTE, the fourth strip short:
    // refused too, as libtiff reads it short.
    let rows = [(278, 3, le(10, 2)), (258, 1, le(8, 1))];
    let err = strip(&dir, "bits.tif", &typed_tiff(false, &short, &rows)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    // Ten whole strips of 10 rows, RowsPerStrip a SSHORT: libtiff reads
    // the ten listed, and so both tools take them.
    let bytes = typed_tiff(false, &[3_000; 10], &[(278, 8, le(10, 2))]);
    strip(&dir, "rows.tif", &bytes).unwrap();
    let op = compress_op(grape_core::CompressPreset::High);
    run(&dir, "rows.tif", &bytes, op).unwrap();
}

#[test]
fn both_tiff_tools_refuse_a_lone_strip_they_cant_size_as_libtiff_would() {
    // A gray 300 × 100 picture, all of it at the header, as one
    // uncompressed strip counted 10, where libtiff counts no strips or
    // tiles (a RowsPerStrip of 2^32 - 2, a lone TileWidth) or more of the
    // picture than it holds (a TileWidth of 304 beside a RowsPerStrip,
    // which it reads as one tile of 304 × 100), or one tile of the picture
    // (a TileLength beside a RowsPerStrip): Pillow reads the whole picture
    // from it, and the 10 bytes copied would leave it no picture. Both
    // tools refuse it: as a layout Grape can't read where libtiff counts
    // none, and as wrong-size where it reads one that's short.
    let picture: Vec<u8> = (0..30_000).map(|i| (i % 251) as u8).collect();
    let size = [
        (256, 4, le(300, 4)),
        (257, 4, le(100, 4)),
        (258, 3, le(8, 2)),
        (262, 3, le(1, 2)),
        (279, 4, le(10, 4)),
    ];
    let dir = TestDir::new("tiff-lone-unsized");
    let unread = "says how its image data is laid out in a way Grape can't read yet.";
    let missized = "says the wrong size for some of its image data, which Grape can't rewrite yet.";
    for (name, layout, why) in [
        ("rows.tif", vec![(278, 4, le(0xFFFF_FFFE, 4))], unread),
        ("width.tif", vec![(322, 3, le(304, 2))], unread),
        (
            "rows-width.tif",
            vec![(278, 3, le(100, 2)), (322, 3, le(304, 2))],
            missized,
        ),
        (
            "rows-length.tif",
            vec![(278, 3, le(100, 2)), (323, 3, le(100, 2))],
            missized,
        ),
    ] {
        let bytes = one_strip(false, &picture, [&size[..], &layout].concat());
        for op in [
            Operation::tool(ToolKey::RemoveMetadata),
            compress_op(grape_core::CompressPreset::High),
        ] {
            let err = run(&dir, name, &bytes, op).unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::NotYetSupported,
                "{name}: {}",
                err.detail
            );
            assert_eq!(err.user_message, format!("{name} {why}"));
            fs::remove_file(dir.join(name)).unwrap();
        }
    }
}

#[test]
fn both_tiff_tools_read_strip_offsets_and_counts_as_libtiff_reads_them() {
    // A gray picture right after the header as one strip, its
    // StripOffsets and StripByteCounts typed SBYTE (a 10 × 10 picture, so
    // its count fits), SSHORT, SLONG or SLONG8 (300 × 100): libtiff reads
    // them, with a warning, and both tools take them. Typed IFD or IFD8,
    // libtiff can't read the file ("Incompatible type"), and both tools
    // refuse it as damaged.
    let file = |big: bool, kind: u16, size: usize, (w, h): (u64, u64)| {
        let header = if big { 16 } else { 8 };
        let picture: Vec<u8> = (0..w * h).map(|i| (i % 251) as u8).collect();
        let entries = vec![
            (256, 4, le(w, 4)),
            (257, 4, le(h, 4)),
            (258, 3, le(8, 2)),
            (262, 3, le(1, 2)),
            (273, kind, le(header, size)),
            (278, 4, le(h, 4)),
            (279, kind, le(w * h, size)),
        ];
        laid_out(big, &picture, entries)
    };
    let dir = TestDir::new("tiff-typed-strips");
    for (name, big, kind, size, picture) in [
        ("sbyte.tif", false, 6, 1, (10, 10)),
        ("sshort.tif", false, 8, 2, (300, 100)),
        ("slong.tif", false, 9, 4, (300, 100)),
        ("slong8.tif", true, 17, 8, (300, 100)),
    ] {
        let bytes = file(big, kind, size, picture);
        let outcome = strip(&dir, name, &bytes).unwrap();
        fs::remove_file(&outcome.outputs[0]).unwrap();
        if picture.0 == 300 {
            let op = compress_op(grape_core::CompressPreset::High);
            let outcome = run(&dir, name, &bytes, op).unwrap();
            fs::remove_file(&outcome.outputs[0]).unwrap();
        }
        fs::remove_file(dir.join(name)).unwrap();
    }
    for (name, big, kind, size) in [("ifd.tif", false, 13, 4), ("ifd8.tif", true, 18, 8)] {
        let bytes = file(big, kind, size, (300, 100));
        for op in [
            Operation::tool(ToolKey::RemoveMetadata),
            compress_op(grape_core::CompressPreset::High),
        ] {
            let err = run(&dir, name, &bytes, op).unwrap_err();
            assert_eq!(err.kind, ErrorKind::DecodeFailed, "{name}: {}", err.detail);
            assert_eq!(
                err.user_message,
                format!("{name} isn't a readable TIFF image.")
            );
            fs::remove_file(dir.join(name)).unwrap();
        }
    }
}

#[test]
fn both_tiff_tools_take_a_tile_size_left_out_from_rows_per_strip() {
    // The review's file: gray 64 × 64 in four strips of 16 rows, 1,024
    // bytes each, with a TileLength of 16 and no TileWidth. libtiff sets a
    // RowsPerStrip listed before a tile size as the tile's too, as wide as
    // the picture, and reads four tiles of 64 × 16, the same bytes, and so
    // do Pillow and ImageMagick. Remove Metadata copies them whole;
    // Compress copies the page as it is, its tiles listed as strips.
    let picture: Vec<u8> = (0..4_096).map(|i| (i % 251) as u8).collect();
    let entries = vec![
        (256, 4, le(64, 4)),
        (257, 4, le(64, 4)),
        (258, 3, le(8, 2)),
        (262, 3, le(1, 2)),
        (273, 4, (0..4).flat_map(|i| le(8 + 1_024 * i, 4)).collect()),
        (278, 3, le(16, 2)),
        (279, 4, (0..4).flat_map(|_| le(1_024, 4)).collect()),
        (323, 3, le(16, 2)),
    ];
    let bytes = laid_out(false, &picture, entries);
    let dir = TestDir::new("tiff-rows-tile");
    let outcome = strip(&dir, "tiles.tif", &bytes).unwrap();
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert!(out.windows(4_096).any(|w| w == picture));
    let op = compress_op(grape_core::CompressPreset::High);
    let err = run(&dir, "tiles.tif", &bytes, op).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert!(
        err.detail
            .contains("lists strips where libtiff reads tiles"),
        "{}",
        err.detail
    );
}

#[test]
fn both_tiff_tools_size_rgb_by_the_bits_of_its_samples_alone() {
    // RGB, 100 × 100 in ten strips of 3,000 bytes, its BitsPerSample of
    // four values, as some writers leave it: libtiff reads the first three
    // (SamplesPerPixel's), and so does Grape.
    let rgb = [
        (256, 4, le(100, 4)),
        (
            258,
            3,
            [8u16; 4].iter().flat_map(|b| b.to_le_bytes()).collect(),
        ),
        (262, 3, le(2, 2)),
        (277, 3, le(3, 2)),
        (278, 3, le(10, 2)),
    ];
    let bytes = typed_tiff(false, &[3_000; 10], &rgb);
    let dir = TestDir::new("tiff-four-bits");
    strip(&dir, "rgb.tif", &bytes).unwrap();
    let op = compress_op(grape_core::CompressPreset::High);
    let outcome = run(&dir, "rgb.tif", &bytes, op).unwrap();
    assert!(
        matches!(outcome.notes[..], [Note::Compressed { .. }]),
        "{outcome:?}"
    );
}

#[test]
fn both_tiff_tools_refuse_a_strip_libtiff_reads_into_a_value() {
    // A gray 300 × 100 page of one strip counted 0, which libtiff works
    // out to the whole picture, but only 20,000 bytes of it, then an XMP
    // packet (700), then the IFD: libtiff reads the packet as pixels.
    let xmp = b"<x:xmpmeta>a secret</x:xmpmeta>".repeat(450);
    let at = 8 + 20_000;
    let ifd = at + u32::try_from(xmp.len()).unwrap();
    let entries: [(u16, u16, u32, u32); 8] = [
        (256, 4, 1, 300),
        (257, 4, 1, 100),
        (258, 3, 1, 8),
        (262, 3, 1, 1),
        (273, 4, 1, 8),
        (278, 4, 1, 100),
        (279, 4, 1, 0),
        (700, 1, u32::try_from(xmp.len()).unwrap(), at),
    ];
    let mut file = b"II*\0".to_vec();
    file.extend(ifd.to_le_bytes());
    file.extend((0..20_000).map(|i| (i % 251) as u8));
    file.extend(&xmp);
    file.extend(8u16.to_le_bytes());
    for (tag, kind, count, value) in entries {
        file.extend(tag.to_le_bytes());
        file.extend(kind.to_le_bytes());
        file.extend(count.to_le_bytes());
        file.extend(value.to_le_bytes());
    }
    file.extend([0; 4]);
    let dir = TestDir::new("tiff-into-xmp");
    for op in [
        Operation::tool(ToolKey::RemoveMetadata),
        compress_op(grape_core::CompressPreset::High),
    ] {
        let err = run(&dir, "packet.tif", &file, op).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
        assert_eq!(
            err.user_message,
            "packet.tif says the wrong size for some of its image data, which Grape can't rewrite yet."
        );
        fs::remove_file(dir.join("packet.tif")).unwrap();
    }
}

#[test]
fn remove_metadata_refuses_metadata_in_a_jpeg_strip_whatever_type_says_so() {
    // A 16 × 16 gray JPEG strip with an Exif APP1 that holds a GPS IFD,
    // its Compression (7) in each integer type libtiff reads, and as a
    // value for each sample: refused, as the SHORT is. An old-style JPEG
    // (6) in those types is refused as one.
    let gray = image::GrayImage::from_fn(16, 16, |x, y| image::Luma([(x * 16 + y) as u8]));
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&gray)
        .unwrap();
    let data = with(&jpeg, &[exif(1)], b"");
    let tags = |compression: Change| {
        vec![
            (256, 4, le(16, 4)),
            (257, 4, le(16, 4)),
            (258, 3, le(8, 2)),
            compression,
            (262, 3, le(1, 2)),
            (278, 4, le(16, 4)),
        ]
    };
    let dir = TestDir::new("strip-tiff-typed-jpeg");
    for (n, message) in [
        (7, "has metadata inside its image data"),
        (6, "uses an old kind of TIFF compression"),
    ] {
        for (name, big, value) in [
            ("short.tif", false, (259, 3, le(n, 2))),
            ("byte.tif", false, (259, 1, le(n, 1))),
            ("sbyte.tif", false, (259, 6, le(n, 1))),
            ("sshort.tif", false, (259, 8, le(n, 2))),
            ("long.tif", false, (259, 4, le(n, 4))),
            ("slong.tif", false, (259, 9, le(n, 4))),
            ("long8.tif", false, (259, 16, le(n, 8))),
            ("slong8.tif", true, (259, 17, le(n, 8))),
            ("each.tif", false, (259, 3, [le(n, 2), le(n, 2)].concat())),
        ] {
            let bytes = one_strip(big, &data, tags(value));
            let err = strip(&dir, name, &bytes).unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::NotYetSupported,
                "{n} {name}: {}",
                err.detail
            );
            assert!(
                err.user_message.starts_with(&format!("{name} {message}")),
                "{n} {name}: {}",
                err.user_message
            );
        }
    }
}

#[test]
fn both_tiff_tools_refuse_two_compressions_that_disagree() {
    // A 16 × 16 gray JPEG strip with an Exif APP1 that holds a GPS IFD,
    // under two Compression entries, 5 then 7: libtiff reads the first,
    // Pillow the last, and Grape read it as LZW, never scanned it, and
    // published it with the GPS in it. Both tools refuse it; the same
    // entry twice reads as one.
    let gray = image::GrayImage::from_fn(16, 16, |x, y| image::Luma([(x * 16 + y) as u8]));
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&gray)
        .unwrap();
    let data = with(&jpeg, &[exif(1)], b"");
    let tags = |compressions: &[u64]| {
        let mut tags = vec![
            (256, 4, le(16, 4)),
            (257, 4, le(16, 4)),
            (258, 3, le(8, 2)),
            (262, 3, le(1, 2)),
            (278, 4, le(16, 4)),
        ];
        tags.extend(compressions.iter().map(|&n| (259, 3, le(n, 2))));
        tags
    };
    let dir = TestDir::new("tiff-two-compressions");
    let bytes = one_strip(false, &data, tags(&[5, 7]));
    for op in [
        Operation::tool(ToolKey::RemoveMetadata),
        compress_op(grape_core::CompressPreset::High),
    ] {
        let err = run(&dir, "two.tif", &bytes, op).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
        assert_eq!(
            err.user_message,
            "two.tif says how it's compressed in a way Grape can't read yet."
        );
    }
    let err = strip(&dir, "same.tif", &one_strip(false, &data, tags(&[7, 7]))).unwrap_err();
    assert!(
        err.user_message
            .starts_with("same.tif has metadata inside its image data"),
        "{}",
        err.user_message
    );
}

#[test]
fn remove_metadata_refuses_jpeg_tables_with_more_than_tables() {
    // The review's layouts, around a 16 × 16 gray JPEG strip's tables (SOI,
    // DQT, DHT and EOI): an APP1 (a Make and a GPS IFD) after their EOI,
    // stray bytes before their SOI, and an APP1 before it. libjpeg reads
    // none of it, and Remove Metadata kept it.
    let gray = image::GrayImage::from_fn(16, 16, |x, y| image::Luma([(x * 16 + y) as u8]));
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&gray)
        .unwrap();
    // The encoder's DQT and DHT segments, as tables.
    let sos = jpeg.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
    let mut tables = vec![0xFF, 0xD8];
    let mut at = 2;
    while at < sos {
        let len = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        if matches!(jpeg[at + 1], 0xDB | 0xC4) {
            tables.extend_from_slice(&jpeg[at..at + 2 + len]);
        }
        at += 2 + len;
    }
    tables.extend([0xFF, 0xD9]);
    let entries = |tables: Vec<u8>| {
        vec![
            (256, 4, le(16, 4)),
            (257, 4, le(16, 4)),
            (258, 3, le(8, 2)),
            (259, 3, le(7, 2)),
            (262, 3, le(1, 2)),
            (278, 4, le(16, 4)),
            (347, 7, tables),
        ]
    };
    let dir = TestDir::new("strip-tiff-loose-tables");
    let outcome = strip(
        &dir,
        "tables.tif",
        &one_strip(false, &jpeg, entries(tables.clone())),
    )
    .unwrap();
    assert_eq!(outcome.notes, [Note::MetadataRemoved(vec![])]);
    for (name, tables) in [
        ("after.tif", [&tables[..], &exif(1)].concat()),
        ("stray.tif", [&b"\0\0\0\0"[..], &tables].concat()),
        ("before.tif", [&exif(1)[..], &tables].concat()),
    ] {
        let err = strip(&dir, name, &one_strip(false, &jpeg, entries(tables))).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::NotYetSupported,
            "{name}: {}",
            err.detail
        );
        assert_eq!(
            err.user_message,
            format!("{name} has JPEG data Grape can't check for metadata yet.")
        );
    }
}

#[test]
fn remove_metadata_writes_photoshops_kept_resources_back_without_their_names() {
    // A resolution resource named with a secret, and a path whose name
    // the clipping path names: the resolution kept its name, and so did
    // the path.
    let res = |id: u16, name: &str, data: &[u8]| {
        let mut r = b"8BIM".to_vec();
        r.extend(id.to_be_bytes());
        r.push(u8::try_from(name.len()).unwrap());
        r.extend(name.as_bytes());
        if name.len().is_multiple_of(2) {
            r.push(0);
        }
        r.extend(u32::try_from(data.len()).unwrap().to_be_bytes());
        r.extend(data);
        if data.len() % 2 == 1 {
            r.push(0);
        }
        r
    };
    let irb = [
        res(1005, "Jane Doe's scan", &[0, 72, 0, 0, 0, 1, 0, 1]),
        res(2000, "Jane's outline", &[0; 26]),
        res(2999, "", b"\x0EJane's outline\0\0"),
    ]
    .concat();
    let entries = vec![
        (256, 4, le(4, 4)),
        (257, 4, le(4, 4)),
        (258, 3, le(8, 2)),
        (262, 3, le(1, 2)),
        (278, 4, le(4, 4)),
        (34377, 7, irb),
    ];
    let dir = TestDir::new("strip-tiff-irb-names");
    let outcome = strip(&dir, "named.tif", &one_strip(false, &[9; 16], entries)).unwrap();
    assert_eq!(outcome.notes, [Note::MetadataRemoved(vec![])]);
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert!(!out.windows(4).any(|w| w == b"Jane"));
    let kept = [
        res(1005, "", &[0, 72, 0, 0, 0, 1, 0, 1]),
        res(2000, "Path 1", &[0; 26]),
        res(2999, "", b"\x06Path 1\0\0"),
    ]
    .concat();
    assert!(out.windows(kept.len()).any(|w| w == kept));
}

#[test]
fn remove_metadata_refuses_metadata_in_jpeg_tables_whatever_type_holds_them() {
    // The review's file: a 16 × 16 gray JPEG strip whose JPEGTables hold
    // SOI, an Exif APP1 (its Make and a GPS IFD) and EOI, each byte a
    // value of a type libtiff reads them in, SHORT among them: libtiff
    // hands the stream on, and Remove Metadata published it with the GPS.
    // Refused, as UNDEFINED tables are.
    let gray = image::GrayImage::from_fn(16, 16, |x, y| image::Luma([(x * 16 + y) as u8]));
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&gray)
        .unwrap();
    let tables = [&b"\xFF\xD8"[..], &exif(1), b"\xFF\xD9"].concat();
    let dir = TestDir::new("strip-tiff-typed-tables");
    for (name, big, kind, size) in [
        ("undefined.tif", false, 7, 1),
        ("ascii.tif", false, 2, 1),
        ("short.tif", false, 3, 2),
        ("sshort.tif", false, 8, 2),
        ("long.tif", false, 4, 4),
        ("slong.tif", false, 9, 4),
        ("long8.tif", true, 16, 8),
        ("slong8.tif", true, 17, 8),
    ] {
        let value = tables
            .iter()
            .flat_map(|&b| le(u64::from(b), size))
            .collect();
        let entries = vec![
            (256, 4, le(16, 4)),
            (257, 4, le(16, 4)),
            (258, 3, le(8, 2)),
            (259, 3, le(7, 2)),
            (262, 3, le(1, 2)),
            (278, 4, le(16, 4)),
            (347, kind, value),
        ];
        let err = strip(&dir, name, &one_strip(big, &jpeg, entries)).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::NotYetSupported,
            "{name}: {}",
            err.detail
        );
        assert_eq!(
            err.user_message,
            format!("{name} has metadata inside its image data, which Grape can't remove yet.")
        );
    }
}

#[test]
fn remove_metadata_refuses_metadata_in_a_webp_strip_and_data_it_cant_check() {
    // The review's file: RGB 64 × 48 in one strip of WebP (50001), a
    // lossless WebP with an Exif (a Make and a GPS IFD) and an XMP chunk,
    // which libtiff decodes: Remove Metadata published it with both.
    let mut lossless = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut lossless)
        .write_image(gradient().as_raw(), 64, 48, image::ExtendedColorType::Rgb8)
        .unwrap();
    let vp8l = lossless[12..].to_vec();
    let vp8x = |flags: u8| riff_chunk(b"VP8X", &[flags, 0, 0, 0, 63, 0, 0, 47, 0, 0]);
    let tiff = |compression: u64, data: &[u8]| {
        let entries = vec![
            (256, 4, le(64, 4)),
            (257, 4, le(48, 4)),
            (258, 3, [le(8, 2), le(8, 2), le(8, 2)].concat()),
            (259, 3, le(compression, 2)),
            (262, 3, le(2, 2)),
            (277, 3, le(3, 2)),
            (278, 4, le(48, 4)),
        ];
        one_strip(false, data, entries)
    };
    let dir = TestDir::new("strip-tiff-webp");
    let tagged = riff(&[
        vp8x(0x0C),
        vp8l.clone(),
        riff_chunk(b"EXIF", &exif(1)[10..]),
        riff_chunk(b"XMP ", b"<x:xmpmeta>secret</x:xmpmeta>"),
    ]);
    let err = strip(&dir, "tagged.tif", &tiff(50001, &tagged)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "tagged.tif has metadata inside its image data, which Grape can't remove yet."
    );
    // The review's other two: a container whose size covers only its
    // VP8L, its EXIF and XMP chunks after it in the strip; and its ALPH,
    // EXIF and VP8L chunks with no RIFF header, which libtiff decodes as
    // libwebp walks them. Both were published with their Exif.
    let mut short = riff(std::slice::from_ref(&vp8l));
    short.extend(riff_chunk(b"EXIF", &exif(1)[10..]));
    short.extend(riff_chunk(b"XMP ", b"<x:xmpmeta>secret</x:xmpmeta>"));
    let headless = [
        riff_chunk(b"ALPH", &[0; 9]),
        riff_chunk(b"EXIF", &exif(1)[10..]),
        vp8l.clone(),
    ]
    .concat();
    for (name, data) in [("short.tif", short), ("headless.tif", headless)] {
        let err = strip(&dir, name, &tiff(50001, &data)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
        assert_eq!(
            err.user_message,
            format!("{name} has WebP data Grape can't check for metadata yet.")
        );
    }
    // Without them, it's stripped, the strip byte for byte.
    let plain = riff(&[vp8x(0), vp8l]);
    let outcome = strip(&dir, "plain.tif", &tiff(50001, &plain)).unwrap();
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert!(out.windows(plain.len()).any(|w| w == plain));
    // JPEG 2000, JPEG XL, JPEG XR and PNG data, whose boxes and chunks
    // Grape doesn't read yet, and data of a code it doesn't know.
    for (name, compression, kind) in [
        ("j2k.tif", 34712, "JPEG 2000"),
        ("aperio.tif", 33005, "JPEG 2000"),
        ("jxl.tif", 50002, "JPEG XL"),
        ("dng-jxl.tif", 52546, "JPEG XL"),
        ("jxr.tif", 34934, "JPEG XR"),
        ("ndpi-jxr.tif", 22610, "JPEG XR"),
        ("png.tif", 34933, "PNG"),
        ("raw.tif", 34713, "image"),
    ] {
        let err = strip(&dir, name, &tiff(compression, b"\0\0\0\x0CjP  ")).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::NotYetSupported,
            "{name}: {}",
            err.detail
        );
        assert_eq!(
            err.user_message,
            format!("{name} has {kind} data Grape can't check for metadata yet.")
        );
    }
}

#[test]
fn remove_metadata_reads_jpeg_data_under_any_code_that_holds_it() {
    // The review's file: a 16 × 16 gray JPEG strip holding an Exif APP1
    // (a Make and a GPS IFD), under DNG's lossy JPEG code (34892) in a
    // TIFF that isn't a DNG, or Bio-Formats' (33007): published with
    // them, as only Compression 7 was read.
    let gray = image::GrayImage::from_fn(16, 16, |x, y| image::Luma([(x * 16 + y) as u8]));
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&gray)
        .unwrap();
    let tagged = with(&jpeg, &[exif(1)], b"");
    let dir = TestDir::new("strip-tiff-jpeg-codes");
    for (name, compression) in [("lossy.tif", 34892), ("alt.tif", 33007), ("leaf.tif", 99)] {
        let entries = |compression: u64| {
            vec![
                (256, 4, le(16, 4)),
                (257, 4, le(16, 4)),
                (258, 3, le(8, 2)),
                (259, 3, le(compression, 2)),
                (262, 3, le(1, 2)),
                (278, 4, le(16, 4)),
            ]
        };
        let err = strip(&dir, name, &one_strip(false, &tagged, entries(compression))).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::NotYetSupported,
            "{name}: {}",
            err.detail
        );
        assert_eq!(
            err.user_message,
            format!("{name} has metadata inside its image data, which Grape can't remove yet.")
        );
        // A plain one is stripped, its strip byte for byte.
        let plain = format!("plain-{name}");
        let outcome = strip(&dir, &plain, &one_strip(false, &jpeg, entries(compression))).unwrap();
        let out = fs::read(&outcome.outputs[0]).unwrap();
        assert!(out.windows(jpeg.len()).any(|w| w == jpeg), "{name}");
    }
}

#[test]
fn remove_metadata_reads_a_strip_a_webp_and_a_jpeg_page_share_as_both() {
    // The review's files: two RGB 64 × 48 pages listing one strip, the
    // first WebP (50001) and the second JPEG (7, YCbCr 1 × 1), the strip
    // a JPEG with an Exif APP1 (a Make and a GPS IFD); and the other way
    // round, the strip a lossless WebP with EXIF and XMP chunks. The strip
    // was read once, as the first page's kind, and passed the second's
    // check: published with its Exif, or its chunks.
    let jpeg = with(&plain_jpeg(), &[exif(1)], b"");
    let mut lossless = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut lossless)
        .write_image(gradient().as_raw(), 64, 48, image::ExtendedColorType::Rgb8)
        .unwrap();
    let webp = riff(&[
        riff_chunk(b"VP8X", &[0x0C, 0, 0, 0, 63, 0, 0, 47, 0, 0]),
        lossless[12..].to_vec(),
        riff_chunk(b"EXIF", &exif(1)[10..]),
        riff_chunk(b"XMP ", b"<x:xmpmeta>secret</x:xmpmeta>"),
    ]);
    let tags = |compression: u64| {
        let mut entries = vec![
            (256, 4, le(64, 4)),
            (257, 4, le(48, 4)),
            (258, 3, [le(8, 2), le(8, 2), le(8, 2)].concat()),
            (259, 3, le(compression, 2)),
            (262, 3, le(if compression == 7 { 6 } else { 2 }, 2)),
            (277, 3, le(3, 2)),
            (278, 4, le(48, 4)),
        ];
        if compression == 7 {
            entries.push((530, 3, [le(1, 2), le(1, 2)].concat()));
        }
        entries
    };
    let dir = TestDir::new("strip-tiff-shared-kinds");
    // (A JPEG stream isn't WebP data Grape can check, now, either.)
    for (name, data, first, second, why) in [
        (
            "webp-then-jpeg.tif",
            &jpeg,
            50001,
            7,
            "has WebP data Grape can't check for metadata yet.",
        ),
        (
            "jpeg-then-webp.tif",
            &webp,
            7,
            50001,
            "has metadata inside its image data, which Grape can't remove yet.",
        ),
    ] {
        // The second page lists the first's strip, at 8.
        let mut shared = tags(second);
        shared.extend([(273, 4, le(8, 4)), (279, 4, le(data.len() as u64, 4))]);
        let file = paged(&[(tags(first), data.clone()), (shared, Vec::new())]);
        let err = strip(&dir, name, &file).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::NotYetSupported,
            "{name}: {}",
            err.detail
        );
        assert_eq!(err.user_message, format!("{name} {why}"));
    }
}

#[test]
fn remove_metadata_keeps_the_tags_a_tiffs_codec_needs() {
    // The review's files: LERC strips with Deflate or ZSTD inside
    // (`tiffcp -c lerc:s1` or `lerc:s2`), which libtiff decodes as
    // LercParameters (50674) says. It went, reported as other fields, and
    // the output couldn't be decoded ("lerc_getBlobInfo() failed").
    let dir = TestDir::new("strip-tiff-lerc");
    for (name, inside) in [("deflate.tif", 1), ("zstd.tif", 2)] {
        let params = [le(4, 4), le(inside, 4)].concat();
        let entries = vec![
            (256, 4, le(16, 4)),
            (257, 4, le(16, 4)),
            (258, 3, le(8, 2)),
            (259, 3, le(34887, 2)),
            (262, 3, le(1, 2)),
            (278, 4, le(16, 4)),
            (50674, 4, params.clone()),
        ];
        let outcome = strip(&dir, name, &one_strip(false, &[9; 40], entries)).unwrap();
        assert_eq!(outcome.notes, [Note::MetadataRemoved(vec![])], "{name}");
        let out = fs::read(&outcome.outputs[0]).unwrap();
        // Its entry: LONG, 2, then where its 8 bytes are.
        let entry = [&50674_u16.to_le_bytes()[..], &le(4, 2), &le(2, 4)].concat();
        let at = out.windows(8).position(|w| w == entry).unwrap();
        let value =
            usize::try_from(u32::from_le_bytes(out[at + 8..at + 12].try_into().unwrap())).unwrap();
        assert_eq!(out[value..value + 8], params, "{name}");
    }
}

#[test]
fn remove_metadata_keeps_a_tag_only_as_libtiff_reads_it() {
    // The review's files: tags Remove Metadata keeps, in a type libtiff
    // ignores and Pillow shows (an XResolution as text, LercParameters as
    // eight LONGs on an uncompressed page, InkNames on an RGB page, GDAL's
    // no-data value as words), were published with nothing said. They go
    // now, as other fields; an Orientation as text is refused
    // (`remove_metadata_refuses_an_orientation_libtiff_cant_read`).
    let dir = TestDir::new("strip-tiff-types");
    let secret = b"SECRET_IN_ORIENTATION_51.5N_0.1W".to_vec();
    let words = b"shot by John at home\0".to_vec();
    for (name, tag, kind, value) in [
        ("resolution.tif", 282, 2, [&secret[..], b"\0"].concat()),
        ("lerc.tif", 50674, 4, secret.clone()),
        ("inks.tif", 333, 2, words.clone()),
        ("nodata.tif", 42113, 2, words.clone()),
        // An ICC profile of SHORTs and a CFAPattern of LONGs past a byte
        // (libtiff doesn't know CFAPattern, and shows it whole).
        ("icc.tif", 34675, 3, secret.clone()),
        ("cfa.tif", 33422, 4, secret.clone()),
        // A ColorMap of 64 SHORTs, where libtiff reads 3 × 256 or none.
        (
            "colormap.tif",
            320,
            3,
            [&b"SECRET shot by John at home 51.5N 0.1W"[..], &[0; 90]].concat(),
        ),
    ] {
        let entries = vec![
            (256, 4, le(1, 4)),
            (257, 4, le(1, 4)),
            (258, 3, [le(8, 2), le(8, 2), le(8, 2)].concat()),
            (262, 3, le(2, 2)),
            (277, 3, le(3, 2)),
            (278, 4, le(1, 4)),
            (tag, kind, value),
        ];
        let outcome = strip(&dir, name, &one_strip(false, &[9; 3], entries)).unwrap();
        assert_eq!(
            outcome.notes,
            [Note::MetadataRemoved(vec![MetaField::Other])],
            "{name}"
        );
        let out = fs::read(&outcome.outputs[0]).unwrap();
        for text in [&b"SECRET"[..], b"John"] {
            assert!(!out.windows(text.len()).any(|w| w == text), "{name}");
        }
    }
}

#[test]
fn remove_metadata_refuses_a_layout_tag_libtiff_cant_read() {
    // The review's file: an 8 × 8 page of 32-bit float gray whose
    // SampleFormat is FLOAT 3.0, which libtiff can't load and Pillow reads
    // as float. Dropped, both read integers: it's refused instead.
    let dir = TestDir::new("strip-tiff-layout");
    let samples: Vec<u8> = (0..64)
        .flat_map(|i| (i as f32 / 4.0).to_le_bytes())
        .collect();
    let entries = vec![
        (256, 4, le(8, 4)),
        (257, 4, le(8, 4)),
        (258, 3, le(32, 2)),
        (262, 3, le(1, 2)),
        (278, 4, le(8, 4)),
        (339, 11, 3.0_f32.to_le_bytes().to_vec()),
    ];
    let err = strip(&dir, "float.tif", &one_strip(false, &samples, entries)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "float.tif says how its image data is laid out in a way Grape can't read yet."
    );
}

#[test]
fn remove_metadata_refuses_an_orientation_libtiff_cant_read() {
    // The review's file: a 4 × 2 gray page whose Orientation is FLOAT 6.0,
    // which libtiff ignores ("Incompatible type for Orientation") and
    // Pillow turns the picture by (its first row 192, 128, 64, 0). Dropped,
    // Pillow read it unturned (0, 32, 64, 96). Refused now, and so is one
    // in a DOUBLE, a RATIONAL, an SRATIONAL, two SHORTs or text.
    let dir = TestDir::new("strip-tiff-orientation");
    let samples: Vec<u8> = (0..8).map(|i| i * 32).collect();
    let rational = [le(6, 4), le(1, 4)].concat();
    let secret = b"SECRET_IN_ORIENTATION_51.5N_0.1W\0".to_vec();
    for (name, kind, value) in [
        ("float.tif", 11, 6.0_f32.to_le_bytes().to_vec()),
        ("double.tif", 12, 6.0_f64.to_le_bytes().to_vec()),
        ("rational.tif", 5, rational.clone()),
        ("srational.tif", 10, rational),
        ("shorts.tif", 3, [le(6, 2), le(6, 2)].concat()),
        ("text.tif", 2, secret),
    ] {
        let entries = vec![
            (256, 3, le(4, 2)),
            (257, 3, le(2, 2)),
            (258, 3, le(8, 2)),
            (262, 3, le(1, 2)),
            (274, kind, value),
            (278, 3, le(2, 2)),
        ];
        let err = strip(&dir, name, &one_strip(false, &samples, entries)).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::NotYetSupported,
            "{name}: {}",
            err.detail
        );
        assert_eq!(
            err.user_message,
            format!("{name} says how its image data is laid out in a way Grape can't read yet.")
        );
    }
}

#[test]
fn remove_metadata_refuses_a_tag_it_keeps_listed_twice_differently() {
    // The review's files: a palette page whose ColorMap is listed twice,
    // red then blue (libtiff reads red, Pillow blue), which Remove
    // Metadata cut to the first, so Pillow's pixels changed; and a second
    // description that's ImageJ's, which went unread.
    let mut red = vec![0; 768];
    let mut blue = vec![0; 768];
    for i in 0..4 {
        red[i] = 0xFFFF;
        blue[512 + i] = 0xFFFF;
    }
    let map = |colors: &[u64]| colors.iter().flat_map(|&c| le(c, 2)).collect::<Vec<u8>>();
    let tags = |more: Vec<Change>| {
        let mut entries = vec![
            (256, 4, le(16, 4)),
            (257, 4, le(16, 4)),
            (258, 3, le(8, 2)),
            (262, 3, le(3, 2)),
            (278, 4, le(16, 4)),
        ];
        entries.extend(more);
        entries
    };
    let pixels: Vec<u8> = (0..256).map(|i| (i % 4) as u8).collect();
    let dir = TestDir::new("strip-tiff-twice");
    let twice = tags(vec![(320, 3, map(&red)), (320, 3, map(&blue))]);
    let err = strip(&dir, "twice.tif", &one_strip(false, &pixels, twice)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "twice.tif lists a tag twice with different values, which Grape can't rewrite yet."
    );
    let imagej = tags(vec![
        (320, 3, map(&red)),
        (270, 2, b"A scan\0".to_vec()),
        (270, 2, b"ImageJ=1.54f\nimages=3\n\0".to_vec()),
    ]);
    let err = strip(&dir, "imagej.tif", &one_strip(false, &pixels, imagej)).unwrap_err();
    assert_eq!(
        err.user_message,
        "imagej.tif keeps its layout in its description, so Remove Metadata would break it."
    );
}

#[test]
fn both_tiff_tools_refuse_two_exif_pointers_that_disagree() {
    // Both tools follow the first entry, where Pillow follows the last:
    // Compress moved the first's Exif IFD alone, and Remove Metadata
    // read only it for the DCF Exif and the report.
    let entries = vec![
        (256, 4, le(4, 4)),
        (257, 4, le(4, 4)),
        (258, 3, le(8, 2)),
        (262, 3, le(1, 2)),
        (278, 4, le(4, 4)),
        (34665, 4, le(0, 4)),
        (34665, 4, le(8, 4)),
    ];
    let file = one_strip(false, &[9; 16], entries);
    let dir = TestDir::new("tiff-two-exif-pointers");
    for (name, result) in [
        ("strip.tif", strip(&dir, "strip.tif", &file)),
        (
            "compress.tif",
            compress(
                &dir,
                "compress.tif",
                &file,
                grape_core::CompressPreset::Balanced,
            ),
        ),
    ] {
        let err = result.unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::NotYetSupported,
            "{name}: {}",
            err.detail
        );
        assert_eq!(
            err.user_message,
            format!(
                "{name} lists a tag twice with different values, which Grape can't rewrite yet."
            )
        );
    }
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
fn compress_keeps_both_of_a_tiffs_descriptions_in_their_order() {
    // tifffile's file: RGB 64 × 48, uncompressed, its description, then
    // its shape as a second ImageDescription, which tifffile reads the
    // series by. Compress kept only the first.
    let description = b"a description of the scan\0".to_vec();
    let shape = b"{\"shape\": [48, 64, 3]}\0".to_vec();
    let entries = vec![
        (256, 4, le(64, 4)),
        (257, 4, le(48, 4)),
        (258, 3, [le(8, 2), le(8, 2), le(8, 2)].concat()),
        (262, 3, le(2, 2)),
        (270, 2, description.clone()),
        (270, 2, shape.clone()),
        (277, 3, le(3, 2)),
        (278, 4, le(48, 4)),
    ];
    let dir = TestDir::new("compress-tiff-two-descriptions");
    let file = one_strip(false, gradient().as_raw(), entries);
    let outcome = compress(
        &dir,
        "shaped.tif",
        &file,
        grape_core::CompressPreset::Balanced,
    )
    .unwrap();
    let out = fs::read(&outcome.outputs[0]).unwrap();
    let at = |value: &[u8]| out.windows(value.len()).position(|w| w == value);
    // Both, the description first: written in the entries' order.
    let (Some(first), Some(second)) = (at(&description), at(&shape)) else {
        panic!("a description went")
    };
    assert!(first < second);
    assert_eq!(tiff_pages(&out), tiff_pages(&file));
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

/// A classic gray 64 × 64 TIFF, uncompressed, its picture at 8 as four
/// 32 × 32 tiles of 1,024 bytes, listed under `tags` (StripOffsets and
/// StripByteCounts, or TileOffsets and TileByteCounts), with RowsPerStrip
/// 16 and `layout`'s entries besides.
fn four_blocks(tags: (u16, u16), layout: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut entries: Vec<(u16, u16, u32, u32)> = vec![
        (256, 4, 1, 64),
        (257, 4, 1, 64),
        (258, 3, 1, 8),
        (262, 3, 1, 1),
        (278, 3, 1, 16),
        (tags.0, 4, 4, 0),
        (tags.1, 4, 4, 0),
    ];
    entries.extend(
        layout
            .iter()
            .map(|&(tag, kind, value)| (tag, kind, 1, value)),
    );
    entries.sort_by_key(|e| e.0);
    let ifd = 8 + 4_096;
    let arrays = ifd + 2 + 12 * u32::try_from(entries.len()).unwrap() + 4;
    let mut file = b"II*\0".to_vec();
    file.extend(ifd.to_le_bytes());
    file.extend((0..4_096u32).map(|i| (i / 1_024 * 40 + i % 32 + i % 1_024 / 32) as u8));
    file.extend(u16::try_from(entries.len()).unwrap().to_le_bytes());
    for (tag, kind, count, value) in entries {
        let value = match tag {
            _ if tag == tags.0 => arrays,
            _ if tag == tags.1 => arrays + 16,
            _ => value,
        };
        file.extend(tag.to_le_bytes());
        file.extend(kind.to_le_bytes());
        file.extend(count.to_le_bytes());
        file.extend(value.to_le_bytes());
    }
    file.extend([0; 4]);
    file.extend((0..4u32).flat_map(|i| (8 + 1_024 * i).to_le_bytes()));
    file.extend((0..4).flat_map(|_| 1_024u32.to_le_bytes()));
    file
}

#[test]
fn compress_copies_a_tiff_page_whose_blocks_libtiff_reads_as_the_other_kind() {
    // libtiff tells tiles by a TileWidth or TileLength, whichever tags
    // list the blocks: four blocks listed as strips with tile sizes are
    // its four tiles, which a recode as strips of 16 rows would garble;
    // four listed as tiles without them are its four strips. Both are
    // copied as they are, and so nothing is left to recode.
    let tiles = [(322, 3, 32), (323, 3, 32)];
    let dir = TestDir::new("compress-tiff-kind");
    for (name, bytes, why) in [
        (
            "strips.tif",
            four_blocks((273, 279), &tiles),
            "IFD 0 copied as it is: it lists strips where libtiff reads tiles",
        ),
        (
            "tiles.tif",
            four_blocks((324, 325), &[]),
            "IFD 0 copied as it is: it lists tiles where libtiff reads strips",
        ),
    ] {
        let err = compress(&dir, name, &bytes, grape_core::CompressPreset::High).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange, "{name}: {}", err.detail);
        assert!(err.detail.contains(why), "{name}: {}", err.detail);
        // Remove Metadata copies them as they are.
        strip(&dir, name, &bytes).unwrap();
    }
}

#[test]
fn compress_says_why_it_copied_a_tiff_page_when_it_ends_no_change() {
    use tiff::encoder::{colortype, TiffEncoder};
    use tiff::tags::Tag;
    // A page of JPEG data, copied as it is, and a small one: the output
    // passes 95 % of the source while the first is copied.
    let mut out = Cursor::new(Vec::new());
    let mut e = TiffEncoder::new(&mut out).unwrap();
    let mut jpeg = e.new_image::<colortype::Gray8>(256, 256).unwrap();
    jpeg.encoder().write_tag(Tag::Compression, 7u16).unwrap();
    let data: Vec<u8> = (0..256 * 256).map(|i| (i % 251) as u8).collect();
    jpeg.write_data(&data).unwrap();
    let gray: Vec<u8> = (0..64).collect();
    e.write_image::<colortype::Gray8>(8, 8, &gray).unwrap();
    let dir = TestDir::new("compress-tiff-log");
    let err = compress(
        &dir,
        "mostly.tif",
        &out.into_inner(),
        grape_core::CompressPreset::High,
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "mostly.tif is already as small as Grape makes it."
    );
    assert!(
        err.detail.contains(
            " bytes; IFD 0, copied as it is (its compression is 7), took the output past 95 % \
             of the source"
        ) && !err.detail.contains("IFD 0 copied"),
        "{}",
        err.detail
    );
}

/// A classic TIFF of `pages`, each its entries ([`Change`]s, sorted here,
/// its strip's offset and count added unless they list them) and its one
/// strip: each strip, then its IFD and the values its entries can't hold,
/// the IFDs chained. The first strip is at 8.
fn paged(pages: &[(Vec<Change>, Vec<u8>)]) -> Vec<u8> {
    let mut file = b"II*\0\0\0\0\0".to_vec();
    // Where the offset of the next IFD goes.
    let mut link = 4;
    for (entries, strip) in pages {
        let at = file.len() as u64;
        file.extend(strip);
        file.resize(file.len().next_multiple_of(2), 0);
        let mut entries = entries.clone();
        if !entries.iter().any(|e| e.0 == 273) {
            entries.extend([(273, 4, le(at, 4)), (279, 4, le(strip.len() as u64, 4))]);
        }
        entries.sort_by_key(|e| e.0);
        let ifd = file.len() as u64;
        file[link..link + 4].copy_from_slice(&le(ifd, 4));
        let mut value_at = ifd + 2 + 12 * entries.len() as u64 + 4;
        file.extend(le(entries.len() as u64, 2));
        let mut values: Vec<u8> = Vec::new();
        for (tag, kind, value) in &entries {
            let size = match kind {
                3 | 8 => 2,
                4 | 9 => 4,
                _ => 1,
            };
            file.extend(tag.to_le_bytes());
            file.extend(kind.to_le_bytes());
            file.extend(le(value.len() as u64 / size, 4));
            if value.len() <= 4 {
                let mut field = value.clone();
                field.resize(4, 0);
                file.extend(field);
            } else {
                file.extend(le(value_at, 4));
                value_at += value.len() as u64;
                values.extend(value);
            }
        }
        link = file.len();
        file.extend([0; 4]);
        file.extend(values);
        file.resize(file.len().next_multiple_of(2), 0);
    }
    file
}

#[test]
fn compress_says_first_which_tiff_pages_it_took_back_when_the_file_passes_95_percent() {
    // The review's files: twenty pages of 64 × 48 gray JPEG, then one of
    // uncompressed noise; and that noise first, then twenty of 16 × 16
    // JPEG. The noise grows recoded and is copied as it is, and the
    // finished file passes 95 % of the source: the detail said why each
    // JPEG page was copied, the first 16, and the one useful line was one
    // of "… and 5 more". It comes first now, whole.
    let jpeg_page = |w: u32, h: u32| {
        let gray = image::GrayImage::from_fn(w, h, |x, y| image::Luma([(x * 3 + y * 5) as u8]));
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&gray)
            .unwrap();
        let entries = vec![
            (256, 4, le(u64::from(w), 4)),
            (257, 4, le(u64::from(h), 4)),
            (258, 3, le(8, 2)),
            (259, 3, le(7, 2)),
            (262, 3, le(1, 2)),
            (278, 4, le(u64::from(h), 4)),
        ];
        (entries, jpeg)
    };
    let mut x: u32 = 0x1234_5678;
    let noise: Vec<u8> = (0..64 * 48)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect();
    let noisy = (
        vec![
            (256, 4, le(64, 4)),
            (257, 4, le(48, 4)),
            (258, 3, le(8, 2)),
            (262, 3, le(1, 2)),
            (278, 4, le(48, 4)),
        ],
        noise,
    );
    let last = [vec![jpeg_page(64, 48); 20], vec![noisy.clone()]].concat();
    let first = [vec![noisy], vec![jpeg_page(16, 16); 20]].concat();
    let dir = TestDir::new("compress-tiff-taken-back");
    for (name, pages, n) in [("last.tif", last, 20), ("first.tif", first, 0)] {
        let err =
            compress(&dir, name, &paged(&pages), grape_core::CompressPreset::High).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
        let (head, log) = err.detail.split_once("; ").unwrap();
        assert!(head.contains(" bytes from "), "{}", err.detail);
        assert!(
            log.starts_with(&format!("IFD {n} copied as it is: its blocks came to ")),
            "{}",
            err.detail
        );
        assert!(
            log.ends_with("its compression is 7; and 4 more"),
            "{}",
            err.detail
        );
    }
}

#[test]
fn compress_copies_a_tiff_page_pillow_would_fail_on_once_deflated() {
    // RGB 40 × 24 in one uncompressed strip, its RowsPerStrip 2^31: Pillow
    // 12.3 reads it, and failed on Compress's Deflate output of it
    // ("decoder error -9"), where libtiff reads both. It's copied as it
    // is, so there's nothing to recode.
    let picture: Vec<u8> = (0..40 * 24 * 3).map(|i| (i % 251) as u8).collect();
    let entries = vec![
        (256, 4, le(40, 4)),
        (257, 4, le(24, 4)),
        (258, 3, [le(8, 2), le(8, 2), le(8, 2)].concat()),
        (262, 3, le(2, 2)),
        (277, 3, le(3, 2)),
        (278, 4, le(1 << 31, 4)),
    ];
    let dir = TestDir::new("compress-tiff-rows");
    let bytes = one_strip(false, &picture, entries);
    let err = compress(&dir, "rows.tif", &bytes, grape_core::CompressPreset::High).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert!(
        err.detail.contains(
            "IFD 0 copied as it is: its RowsPerStrip is 2147483648, more than readers take"
        ),
        "{}",
        err.detail
    );
}

#[test]
fn compress_says_a_tiff_page_passed_95_percent_recoded_and_copied() {
    use tiff::encoder::{colortype, Compression, DeflateLevel, TiffEncoder};
    // One page of noise, Deflate already: recoded, it passes 95 % of the
    // source, and copied as it is too, so nothing is made of it.
    let mut x: u32 = 0x1234_5678;
    let noise: Vec<u8> = (0..128 * 128)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect();
    let mut out = Cursor::new(Vec::new());
    TiffEncoder::new(&mut out)
        .unwrap()
        .with_compression(Compression::Deflate(DeflateLevel::Best))
        .write_image::<colortype::Gray8>(128, 128, &noise)
        .unwrap();
    let dir = TestDir::new("compress-tiff-both");
    let err = compress(
        &dir,
        "noise.tif",
        &out.into_inner(),
        grape_core::CompressPreset::High,
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert!(
        err.detail.contains(
            "bytes; IFD 0 took the output past 95 % of the source both recoded and copied as it is"
        ) && !err.detail.contains("copied as it is:"),
        "{}",
        err.detail
    );
}

#[test]
fn compress_says_why_it_copied_a_tiff_page_when_it_cant_make_its_output() {
    use grape_core::JobOutput;
    use tiff::encoder::{colortype, TiffEncoder};
    use tiff::tags::Tag;
    // A page of JPEG data, which the plan copies as it is, and one it
    // recodes.
    let mut out = Cursor::new(Vec::new());
    let mut e = TiffEncoder::new(&mut out).unwrap();
    let mut jpeg = e.new_image::<colortype::Gray8>(8, 8).unwrap();
    jpeg.encoder().write_tag(Tag::Compression, 7u16).unwrap();
    jpeg.write_data(&[7; 64]).unwrap();
    e.write_image::<colortype::Gray8>(64, 64, &[9; 64 * 64])
        .unwrap();
    let bytes = out.into_inner();
    let dir = TestDir::new("compress-tiff-log-made");
    fs::write(dir.join("file"), b"not a folder").unwrap();
    // A folder gone since planning, where the temp file can't be made; and
    // a fallback folder that can't be, under a file.
    for (folder, fallback) in [("gone", false), ("file", true)] {
        let (src, mut job) = plan_one(
            &dir,
            "two.tif",
            &bytes,
            compress_op(grape_core::CompressPreset::High),
        );
        job.output = JobOutput::File(dir.join(folder).join("sub").join("two.tif"));
        job.used_fallback = fallback;
        let before = SourceState::of(&src);
        let err = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap_err();
        before.assert_unchanged();
        assert_eq!(err.kind, ErrorKind::WriteFailed, "{}", err.detail);
        assert!(
            err.detail
                .contains("IFD 0 copied as it is: its compression is 7"),
            "{folder}: {}",
            err.detail
        );
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

/// libwebp's file of `px` (`w`×`h`, RGB or RGBA), lossy at `quality`.
fn lossy_webp(px: &[u8], w: u32, h: u32, alpha: bool, quality: f32) -> Vec<u8> {
    let px = if alpha {
        grape_webp::Pixels::Rgba {
            data: px,
            width: w,
            height: h,
        }
    } else {
        grape_webp::Pixels::Rgb {
            data: px,
            width: w,
            height: h,
        }
    };
    let opts = grape_webp::Options {
        mode: grape_webp::Mode::Lossy { quality, method: 4 },
        threads: false,
    };
    grape_webp::encode(px, &opts, &mut |_| true).unwrap()
}

/// A WebP's top-level chunks, each its fourcc and data.
fn webp_chunks(file: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let riff = u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize;
    assert_eq!(riff + 8, file.len());
    let mut at = 12;
    let mut out = Vec::new();
    while at < file.len() {
        let size = u32::from_le_bytes(file[at + 4..at + 8].try_into().unwrap()) as usize;
        out.push((
            file[at..at + 4].try_into().unwrap(),
            file[at + 8..at + 8 + size].to_vec(),
        ));
        at += 8 + size + (size & 1);
    }
    assert_eq!(at, file.len());
    out
}

/// The data of `file`'s `fourcc` chunk.
fn webp_chunk(file: &[u8], fourcc: &[u8; 4]) -> Option<Vec<u8>> {
    webp_chunks(file)
        .into_iter()
        .find(|(f, _)| f == fourcc)
        .map(|(_, data)| data)
}

/// A VP8X chunk with `flags` and a `w`×`h` canvas.
fn vp8x_chunk(flags: u8, w: u32, h: u32) -> Vec<u8> {
    let (w, h) = ((w - 1).to_le_bytes(), (h - 1).to_le_bytes());
    riff_chunk(
        b"VP8X",
        &[flags, 0, 0, 0, w[0], w[1], w[2], h[0], h[1], h[2]],
    )
}

/// The pixel dimensions in a little-endian TIFF's Exif IFD, the second
/// entry of IFD0 pointing to it (as [`camera_exif`] writes it).
fn tiff_dimensions(tiff: &[u8]) -> (u32, u32) {
    let u32le = |i: usize| u32::from_le_bytes(tiff[i..i + 4].try_into().unwrap());
    let exif_ifd = usize::try_from(u32le(8 + 2 + 12 + 8)).unwrap();
    (u32le(exif_ifd + 2 + 8), u32le(exif_ifd + 2 + 12 + 8))
}

#[test]
fn compress_shrinks_a_lossy_webp_and_carries_its_metadata() {
    use grape_core::CompressPreset::{Balanced, High, Small};
    let dir = TestDir::new("compress-webp");
    // Wide, so Small scales it, and small, as libwebp is slow unoptimized.
    let img = photo(2560, 400);
    let lossy = lossy_webp(img.as_raw(), 2560, 400, false, 95.0);
    let vp8 = lossy[12..].to_vec();
    assert_eq!(&vp8[..4], b"VP8 ");
    let exif = camera_exif(2560, 400);
    let xmp = b"<x:xmpmeta>kept</x:xmpmeta>".to_vec();
    // (name, where the EXIF starts in `camera_exif`'s segment: after its
    // `Exif\0\0` or not, the presets)
    for (name, prefix, presets) in [
        ("photo.webp", 4, &[Small][..]),
        ("bare.webp", 10, &[Small, Balanced, High]),
    ] {
        let exif = &exif[prefix..];
        let mut src = riff(&[
            vp8x_chunk(0x2C, 2560, 400),
            riff_chunk(b"ICCP", b"an odd profile"),
            vp8.clone(),
            riff_chunk(b"EXIF", exif),
            riff_chunk(b"XMP ", &xmp),
            riff_chunk(b"JUNK", b"dropped"),
        ]);
        src.extend_from_slice(b"after the RIFF");
        for &preset in presets {
            let (w, h) = if preset == Small {
                (2048, 320)
            } else {
                (2560, 400)
            };
            let outcome = compress(&dir, name, &src, preset).unwrap();
            let stem = name.trim_end_matches(".webp");
            assert_eq!(
                outcome.outputs,
                [dir.join(&format!("{stem} Compressed.webp"))]
            );
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
            let kinds: Vec<[u8; 4]> = webp_chunks(&out).into_iter().map(|(f, _)| f).collect();
            assert_eq!(kinds, [*b"VP8X", *b"ICCP", *b"VP8 ", *b"EXIF", *b"XMP "]);
            // Its flags as the source's, its canvas the new size.
            let vp8x = webp_chunk(&out, b"VP8X").unwrap();
            assert_eq!(vp8x[0], 0x2C);
            let decoded = image::load_from_memory(&out).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (w, h), "{preset:?}");
            // Stored as it was: the EXIF still turns it (image reads it
            // only without `Exif\0\0`), and says its size.
            let carried = webp_chunk(&out, b"EXIF").unwrap();
            assert_eq!(carried.len(), exif.len());
            assert_eq!(carried[..10 - prefix], exif[..10 - prefix]);
            assert_eq!(tiff_dimensions(&carried[10 - prefix..]), (w, h));
            if prefix == 10 {
                assert_eq!(orientation(&out), Orientation::Rotate90);
            }
            assert_eq!(webp_chunk(&out, b"ICCP").unwrap(), b"an odd profile");
            assert_eq!(webp_chunk(&out, b"XMP ").unwrap(), xmp);
            fs::remove_file(&outcome.outputs[0]).unwrap();
            fs::remove_file(dir.join(name)).unwrap();
        }
    }
}

#[test]
fn compress_keeps_a_lossless_webp_exactly() {
    let dir = TestDir::new("compress-webp-lossless");
    // A photo with transparency, colors under its fully transparent
    // pixels, from image-webp's lossless encoder.
    let rgb = photo(300, 200);
    let shot = image::RgbaImage::from_fn(300, 200, |x, y| {
        let [r, g, b] = rgb.get_pixel(x, y).0;
        let a = if x < 40 {
            0
        } else {
            255 - u8::try_from(y).unwrap() / 2
        };
        image::Rgba([r, g, b, a])
    });
    let mut simple = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut simple)
        .write_image(shot.as_raw(), 300, 200, image::ExtendedColorType::Rgba8)
        .unwrap();
    // An extended one too, with an EXIF that has no `Exif\0\0`.
    let extended = riff(&[
        vp8x_chunk(0x18, 300, 200),
        simple[12..].to_vec(),
        riff_chunk(b"EXIF", &camera_exif(300, 200)[10..]),
    ]);
    for (name, src) in [("shot.webp", &simple), ("tagged.webp", &extended)] {
        for preset in [
            grape_core::CompressPreset::Small,
            grape_core::CompressPreset::High,
        ] {
            let outcome = compress(&dir, name, src, preset).unwrap();
            let out = fs::read(&outcome.outputs[0]).unwrap();
            assert!(out.len() * 20 <= src.len() * 19, "{name}");
            // Every pixel, the color under the transparent ones too.
            let back = image::load_from_memory(&out).unwrap().to_rgba8();
            assert_eq!(back.as_raw(), shot.as_raw(), "{name}");
            let vp8l = webp_chunk(&out, b"VP8L").unwrap();
            assert_eq!(vp8l[0], 0x2F);
            if src == &extended {
                assert_eq!(webp_chunk(&out, b"VP8X").unwrap()[0], 0x18);
                let exif = webp_chunk(&out, b"EXIF").unwrap();
                assert_eq!(tiff_dimensions(&exif), (300, 200));
            }
            fs::remove_file(dir.join(name)).unwrap();
            if preset == grape_core::CompressPreset::High {
                // Its own output can't be made 5 % smaller again, and
                // Small wouldn't do more.
                let err = compress(
                    &dir,
                    "again.webp",
                    &out,
                    grape_core::CompressPreset::Balanced,
                )
                .unwrap_err();
                assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
                assert_eq!(
                    err.user_message,
                    "again.webp is already as small as Grape makes it."
                );
                fs::remove_file(dir.join("again.webp")).unwrap();
            }
            fs::remove_file(&outcome.outputs[0]).unwrap();
        }
    }
}

#[test]
fn compress_keeps_a_lossy_webps_alpha_exactly() {
    let dir = TestDir::new("compress-webp-alpha");
    let rgb = photo(640, 480);
    let rgba = image::RgbaImage::from_fn(640, 480, |x, y| {
        let [r, g, b] = rgb.get_pixel(x, y).0;
        let a = if x < 100 {
            0
        } else {
            u8::try_from(y * 255 / 480).unwrap() | 0x40
        };
        image::Rgba([r, g, b, a])
    });
    let src = lossy_webp(rgba.as_raw(), 640, 480, true, 95.0);
    let source = image::load_from_memory(&src).unwrap().to_rgba8();
    let outcome = compress(&dir, "sticker.webp", &src, grape_core::CompressPreset::High).unwrap();
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert!(out.len() * 10 <= src.len() * 9);
    // libwebp's own file: VP8X with its alpha flag, ALPH, VP8.
    let kinds: Vec<[u8; 4]> = webp_chunks(&out).into_iter().map(|(f, _)| f).collect();
    assert_eq!(kinds, [*b"VP8X", *b"ALPH", *b"VP8 "]);
    assert_eq!(webp_chunk(&out, b"VP8X").unwrap()[0], 0x10);
    let back = image::load_from_memory(&out).unwrap().to_rgba8();
    assert!(back
        .pixels()
        .zip(source.pixels())
        .all(|(a, b)| a.0[3] == b.0[3]));
}

#[test]
fn compress_encodes_a_picture_its_scale_left_opaque_as_opaque() {
    use grape_core::CompressPreset::Small;
    let dir = TestDir::new("compress-webp-opaque");
    // One pixel at 254 in a photo: scaled by 2.5, nothing of it is left,
    // so Small encodes it as it encodes the opaque photo, at the opaque
    // method (libwebp's 6), not 5 with an alpha plane: 3.7 % larger before.
    // (The colors around the pixel are scaled by area, so not byte for
    // byte.)
    let (w, h) = (5120u32, 160u32);
    let rgb = photo(w, h);
    let opaque = lossy_webp(rgb.as_raw(), w, h, false, 95.0);
    let mut alpha = vec![255u8; (w * h) as usize];
    alpha[(80 * w + 2560) as usize] = 254;
    let rgba: Vec<u8> = rgb
        .as_raw()
        .chunks(3)
        .zip(&alpha)
        .flat_map(|(p, &a)| [p[0], p[1], p[2], a])
        .collect();
    let alph = webp_chunk(&lossy_webp(&rgba, w, h, true, 95.0), b"ALPH").unwrap();
    let dot = riff(&[
        vp8x_chunk(0x10, w, h),
        riff_chunk(b"ALPH", &alph),
        riff_chunk(b"VP8 ", &webp_chunk(&opaque, b"VP8 ").unwrap()),
    ]);
    let plain = compress(&dir, "plain.webp", &opaque, Small).unwrap();
    let with_dot = compress(&dir, "dot.webp", &dot, Small).unwrap();
    let (plain, with_dot) = (
        fs::read(&plain.outputs[0]).unwrap(),
        fs::read(&with_dot.outputs[0]).unwrap(),
    );
    assert!(webp_chunk(&with_dot, b"ALPH").is_none());
    assert!(
        with_dot.len() * 100 <= plain.len() * 101,
        "{} bytes, {} opaque",
        with_dot.len(),
        plain.len()
    );
}

#[test]
fn compress_scales_a_hard_edged_webp_cutout_without_growing_its_alpha() {
    use grape_core::CompressPreset::{Balanced, Small};
    // The review's picture: a white and a red disc with hard edges on
    // transparent black, 2600 × 1300, at q95. Scaled with Lanczos3, its
    // alpha rang, libwebp stored that losslessly, and Small came out
    // larger than its source: NoChange.
    let dir = TestDir::new("compress-webp-disc");
    let disc = image::RgbaImage::from_fn(2600, 1300, |x, y| {
        let inside = |cx: f32, cy: f32, r: f32| {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            dx * dx + dy * dy < r * r
        };
        if inside(850.0, 650.0, 550.0) {
            image::Rgba([255, 255, 255, 255])
        } else if inside(2000.0, 600.0, 500.0) {
            image::Rgba([255, 40, 40, 255])
        } else {
            image::Rgba([0, 0, 0, 0])
        }
    });
    let src = lossy_webp(disc.as_raw(), 2600, 1300, true, 95.0);
    let mut sizes = Vec::new();
    for preset in [Small, Balanced] {
        let outcome = compress(&dir, "disc.webp", &src, preset).unwrap();
        let out = fs::read(&outcome.outputs[0]).unwrap();
        sizes.push(out.len());
        if preset == Small {
            // Its edges take two steps at 1.27 to 1, half and whole, and no
            // edge darkens: each visible pixel of the white disc beside a
            // transparent one is white.
            let back = image::load_from_memory(&out).unwrap().to_rgba8();
            assert_eq!(back.dimensions(), (2048, 1024));
            assert!(back.pixels().all(|p| [0, 128, 255].contains(&p.0[3])));
            let mut edges = 0;
            for (x, y, p) in back.enumerate_pixels() {
                let beside = |dx: i32, dy: i32| {
                    let (x, y) = (x as i32 + dx, y as i32 + dy);
                    x >= 0 && y >= 0 && back.get_pixel(x as u32, y as u32).0[3] == 0
                };
                let edge = beside(-1, 0) || beside(0, -1) || beside(1, 0) || beside(0, 1);
                if x < 1100 && p.0[3] > 0 && edge {
                    assert!(p.0[..3].iter().all(|&c| c >= 230), "({x}, {y}): {p:?}");
                    edges += 1;
                }
            }
            assert!(edges > 1000, "{edges}");
        }
        fs::remove_file(&outcome.outputs[0]).unwrap();
        fs::remove_file(dir.join("disc.webp")).unwrap();
    }
    assert!(
        sizes[0] < sizes[1],
        "Small {} B, Balanced {} B",
        sizes[0],
        sizes[1]
    );
}

#[test]
fn compress_keeps_a_hard_edged_webps_thin_strokes_whole_and_their_color() {
    use grape_core::CompressPreset::Small;
    // A signature: 1 and 2 px curves, alpha only 0 and 255, blue over black
    // under the transparency, 6000 px wide at q90. Small scales it by 2.93:
    // every new pixel a stroke covered less than half of went transparent,
    // so the 1 px curve vanished, and the black under the transparency put
    // a color step at every stroke edge (the file came out 2.2 times as
    // large as with the strokes' own color there).
    let dir = TestDir::new("compress-webp-strokes");
    let (w, h) = (6000u32, 600u32);
    let curve = |x: f32, base: f32| base + 80.0 * (x / 90.0).sin() + 30.0 * (x / 23.0).sin();
    let mut img = image::RgbaImage::new(w, h);
    for (base, width) in [(150.0, 1.0), (430.0, 2.0)] {
        for x in 0..w {
            let (y0, y1) = (curve(x as f32, base), curve(x as f32 + 1.0, base));
            let top = y0.min(y1).floor() as u32;
            let bottom = (y0.max(y1) + width).floor() as u32;
            for y in top..bottom.max(top + width as u32) {
                img.put_pixel(x, y, image::Rgba([20, 20, 160, 255]));
            }
        }
    }
    let src = lossy_webp(img.as_raw(), w, h, true, 90.0);
    let outcome = compress(&dir, "signature.webp", &src, Small).unwrap();
    let out = fs::read(&outcome.outputs[0]).unwrap();
    assert!(
        out.len() * 10 <= src.len() * 9,
        "{} from {}",
        out.len(),
        src.len()
    );
    let back = image::load_from_memory(&out).unwrap().to_rgba8();
    assert_eq!(back.dimensions(), (2048, 205));
    let r = 6000.0 / 2048.0;
    for (base, width) in [(150.0, 1.0), (430.0, 2.0)] {
        // At each new column, a visible pixel within a pixel of the curve.
        for nx in 2..2046 {
            let y = curve((nx as f32 + 0.5) * r, base) / r;
            let near = (y.floor() as u32 - 1)..=((y + width / r).ceil() as u32 + 1);
            assert!(
                near.clone().any(|ny| back.get_pixel(nx, ny).0[3] > 0),
                "the {width} px curve at {nx}, {near:?}"
            );
        }
    }
    // Its alpha in three steps at 2.93 to 1, and the visible pixels as near
    // the strokes' blue as the source's own (libwebp's q90 over black left
    // them 16 off on average, 41 at most): with black under the
    // transparency again, they came out 45 off on average, 92 at most.
    let source = image::load_from_memory(&src).unwrap().to_rgba8();
    let off = |img: &image::RgbaImage| {
        let offs: Vec<u8> = img
            .pixels()
            .filter(|p| p.0[3] > 0)
            .map(|p| {
                let off = p.0[..3]
                    .iter()
                    .zip([20, 20, 160])
                    .map(|(&a, b)| a.abs_diff(b));
                off.max().unwrap()
            })
            .collect();
        let mean = offs.iter().map(|&o| f64::from(o)).sum::<f64>() / offs.len() as f64;
        (mean, offs.iter().copied().max().unwrap())
    };
    let (was, is) = (off(&source), off(&back));
    assert!(
        is.0 <= was.0 + 2.0 && is.1 <= was.1,
        "{is:?} against {was:?}"
    );
    assert!(back.pixels().all(|p| [0, 85, 170, 255].contains(&p.0[3])));
}

#[test]
fn compress_leaves_a_compact_webp_alone_and_refuses_an_animated_one() {
    use grape_core::CompressPreset::{Balanced, High, Small};
    let dir = TestDir::new("compress-webp-none");
    // At q40, q50 and q30, under 2048 px, Small can't do more: it isn't
    // sent there (D26). Nor for a fully transparent picture, whose VP8 is
    // flat whatever its quality, nor one whose metadata outweighs it.
    let picture = photo(960, 540);
    let transparent = lossy_webp(&vec![0; 256 * 256 * 4], 256, 256, true, 80.0);
    let fine = lossy_webp(picture.as_raw(), 960, 540, false, 90.0);
    let tagged = riff(&[
        vp8x_chunk(0x04, 960, 540),
        fine[12..].to_vec(),
        riff_chunk(b"XMP ", &vec![b'x'; fine.len() * 10]),
    ]);
    for (name, src) in [
        (
            "q40.webp",
            lossy_webp(picture.as_raw(), 960, 540, false, 40.0),
        ),
        (
            "q50.webp",
            lossy_webp(picture.as_raw(), 960, 540, false, 50.0),
        ),
        (
            "q30.webp",
            lossy_webp(picture.as_raw(), 960, 540, false, 30.0),
        ),
        ("clear.webp", transparent),
        ("tagged.webp", tagged),
    ] {
        for preset in [Small, Balanced, High] {
            let err = compress(&dir, name, &src, preset).unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::NoChange,
                "{name} {preset:?}: {}",
                err.detail
            );
            assert_eq!(
                err.user_message,
                format!("{name} is already as small as Grape makes it."),
                "{preset:?}"
            );
            fs::remove_file(dir.join(name)).unwrap();
        }
    }
    // At q80, Balanced and High can't make it 10 % smaller, and Small can.
    let q80 = lossy_webp(picture.as_raw(), 960, 540, false, 80.0);
    for preset in [Balanced, High] {
        let err = compress(&dir, "q80.webp", &q80, preset).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange, "{preset:?}: {}", err.detail);
        assert_eq!(err.user_message, "q80.webp is already compact. Try Small.");
        fs::remove_file(dir.join("q80.webp")).unwrap();
    }
    let out = compress(&dir, "q80.webp", &q80, Small).unwrap();
    fs::remove_file(&out.outputs[0]).unwrap();
    fs::remove_file(dir.join("q80.webp")).unwrap();
    let frame = lossy_webp(&[9; 16 * 16 * 3], 16, 16, false, 80.0);
    let anim = riff(&[
        vp8x_chunk(0x02, 16, 16),
        riff_chunk(b"ANIM", &[0; 6]),
        riff_chunk(b"ANMF", &[[0; 16].as_slice(), &frame[12..]].concat()),
    ]);
    let err = compress(&dir, "anim.webp", &anim, High).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "anim.webp is animated. Compress works on still WebP images for now."
    );
    // An animation's chunks in a still file (VP8X's flag off): libwebp's
    // still decoder skips them and its demuxer refuses the file.
    let still = riff(&[
        vp8x_chunk(0, 16, 16),
        riff_chunk(b"ANIM", &[0; 6]),
        frame[12..].to_vec(),
    ]);
    let err = compress(&dir, "still.webp", &still, High).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
    assert_eq!(err.user_message, "still.webp isn't a readable WEBP image.");
    // Alpha only libwebp reads (VP8X says none): refused, nothing written.
    let alpha = lossy_webp(&[200; 8 * 8 * 4], 8, 8, true, 80.0);
    let mut unflagged = alpha.clone();
    unflagged[20] = 0;
    let err = compress(&dir, "flag.webp", &unflagged, High).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
    assert_eq!(err.user_message, "flag.webp isn't a readable WEBP image.");
    // Two ALPH chunks before the VP8: libwebp reads the last, image-webp
    // the first.
    // (The second is uncompressed: its header byte 0, then 8 × 8 values.)
    let half = [[0u8].as_slice(), &[0x80; 64]].concat();
    let two = riff(&[
        vp8x_chunk(0x10, 8, 8),
        riff_chunk(b"ALPH", &webp_chunk(&alpha, b"ALPH").unwrap()),
        riff_chunk(b"ALPH", &half),
        riff_chunk(b"VP8 ", &webp_chunk(&alpha, b"VP8 ").unwrap()),
    ]);
    let err = compress(&dir, "two.webp", &two, High).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
    assert_eq!(err.user_message, "two.webp isn't a readable WEBP image.");
    // An ALPH chunk before a VP8L: libwebp's still decoder shows alpha
    // whenever there's one (ImageMagick read the picture RGBA) and its
    // demuxer refuses the file, while image-webp goes by VP8X's flag and
    // VP8L's alpha bit, both off here, though its pixels have alpha:
    // Compress wrote it opaque.
    let mut lossless = Vec::new();
    let pixels = image::RgbaImage::from_fn(8, 8, |x, _| {
        image::Rgba([90, 90, 90, if x < 4 { 40 } else { 255 }])
    });
    image::codecs::webp::WebPEncoder::new_lossless(&mut lossless)
        .write_image(pixels.as_raw(), 8, 8, image::ExtendedColorType::Rgba8)
        .unwrap();
    let mut vp8l = webp_chunk(&lossless, b"VP8L").unwrap();
    vp8l[4] &= !0x10;
    let before = riff(&[
        vp8x_chunk(0, 8, 8),
        riff_chunk(b"ALPH", &half),
        riff_chunk(b"VP8L", &vp8l),
    ]);
    let err = compress(&dir, "before.webp", &before, High).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
    assert_eq!(err.user_message, "before.webp isn't a readable WEBP image.");
}

/// Compress at Balanced and High on `src` named `name`: each that ends
/// `NoChange` says "Try Small" exactly when Small, run then, writes at most
/// 90 % of it (D26). Gives whether Balanced was told to try Small.
fn try_small_is_exact(dir: &TestDir, name: &str, src: &[u8]) -> bool {
    use grape_core::CompressPreset::{Balanced, High, Small};
    let small = compress(dir, name, src, Small);
    if let Ok(outcome) = &small {
        let out = fs::metadata(&outcome.outputs[0]).unwrap().len();
        assert!(out * 10 <= src.len() as u64 * 9, "{name}");
        fs::remove_file(&outcome.outputs[0]).unwrap();
    } else if let Err(e) = &small {
        assert_eq!(e.kind, ErrorKind::NoChange, "{name} Small: {}", e.detail);
    }
    fs::remove_file(dir.join(name)).unwrap();
    let mut told = Vec::new();
    for preset in [Balanced, High] {
        match compress(dir, name, src, preset) {
            Ok(outcome) => {
                fs::remove_file(&outcome.outputs[0]).unwrap();
                told.push(false);
            }
            Err(e) => {
                assert_eq!(
                    e.kind,
                    ErrorKind::NoChange,
                    "{name} {preset:?}: {}",
                    e.detail
                );
                let try_small = small.is_ok();
                let message = if try_small {
                    format!("{name} is already compact. Try Small.")
                } else {
                    format!("{name} is already as small as Grape makes it.")
                };
                assert_eq!(e.user_message, message, "{name} {preset:?}: {}", e.detail);
                told.push(try_small);
            }
        }
        fs::remove_file(dir.join(name)).unwrap();
    }
    told[0]
}

/// A smooth photo-like picture with a fine, faint texture, lossy at
/// `quality`: at q75, Balanced's q80 makes it larger and Small's q70 about
/// 83 %, as with a real photo.
fn smooth_photo(w: u32, h: u32, quality: f32) -> Vec<u8> {
    let img = RgbImage::from_fn(w, h, |x, y| {
        let (x, y) = (x as f32, y as f32);
        let grain = 4.0 * (x / 2.3).sin() * (y / 3.1).cos();
        let v = |base: f32| (base + grain).clamp(0.0, 255.0) as u8;
        Rgb([
            v(128.0 + 80.0 * (x / 41.0).sin() * (y / 53.0).cos()),
            v(128.0 + 70.0 * ((x + y) / 67.0).sin()),
            v(100.0 + 50.0 * (x / 29.0 - y / 31.0).cos()),
        ])
    });
    lossy_webp(img.as_raw(), w, h, false, quality)
}

#[test]
fn compress_says_try_small_only_when_small_would_do_more() {
    // "Try Small" is said when Small's own file, made as a trial, is at
    // most 90 % of the source, and only then. Each of these was told
    // otherwise by a prediction.
    let dir = TestDir::new("compress-webp-try-small");
    // A q75 photo Small doesn't scale (cwebp's default quality): told
    // it was as small as Grape makes it, though Small made it 58 to 85 %.
    assert!(try_small_is_exact(
        &dir,
        "q75.webp",
        &smooth_photo(800, 450, 75.0)
    ));
    // Under a grained alpha veil, a little over 2048 px: Small scales the
    // veil by area and makes it about half.
    let mut n = 1u32;
    let colors = photo(2400, 240);
    let veil = image::RgbaImage::from_fn(2400, 240, |x, y| {
        n = n.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let [r, g, b] = colors.get_pixel(x, y).0;
        image::Rgba([r, g, b, 168 + (n >> 16) as u8 % 64])
    });
    let veiled = lossy_webp(veil.as_raw(), 2400, 240, true, 80.0);
    assert!(try_small_is_exact(&dir, "veil.webp", &veiled));
    // A 1 × 500 strip, whose frame headers are much of it (told to try
    // Small, which made a Pillow one 94 %), and one pixel, whose frame
    // headers are all of it: whatever Small makes of them.
    let strip = lossy_webp(photo(1, 500).as_raw(), 1, 500, false, 80.0);
    try_small_is_exact(&dir, "strip.webp", &strip);
    let dot = lossy_webp(&[200, 30, 30], 1, 1, false, 90.0);
    assert!(!try_small_is_exact(&dir, "dot.webp", &dot));
    // Hard-edged strokes over a photo a little over 2048 px at q80, told to
    // try Small, which then grew them: its alpha plane is most of the file.
    let photo_colors = photo(2250, 300);
    let mut seed = 9u32;
    let mut strokes = vec![false; 2250 * 300];
    for _ in 0..1500 {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let (x0, y0) = ((seed >> 8) as usize % 2240, (seed >> 20) as usize % 290);
        let (across, len, wide) = (
            seed & 1 == 0,
            4 + (seed >> 4) as usize % 20,
            1 + (seed >> 2) as usize % 3,
        );
        for t in 0..len {
            for k in 0..wide {
                let (x, y) = if across {
                    (x0 + t, y0 + k)
                } else {
                    (x0 + k, y0 + t)
                };
                if x < 2250 && y < 300 {
                    strokes[y * 2250 + x] = true;
                }
            }
        }
    }
    let text = image::RgbaImage::from_fn(2250, 300, |x, y| {
        let [r, g, b] = photo_colors.get_pixel(x, y).0;
        image::Rgba([
            r,
            g,
            b,
            if strokes[(y * 2250 + x) as usize] {
                255
            } else {
                0
            },
        ])
    });
    let hard = lossy_webp(text.as_raw(), 2250, 300, true, 80.0);
    try_small_is_exact(&dir, "text.webp", &hard);
    // Metadata the size of the file: no file Small writes could be 90 %,
    // so the trial isn't run.
    let fine = smooth_photo(400, 300, 95.0);
    let tagged = riff(&[
        vp8x_chunk(0x04, 400, 300),
        fine[12..].to_vec(),
        riff_chunk(b"XMP ", &vec![b'x'; fine.len() * 10]),
    ]);
    assert!(!try_small_is_exact(&dir, "tagged.webp", &tagged));
}

#[test]
fn a_cancel_during_the_try_small_trial_is_a_cancel() {
    use grape_core::CompressPreset::Balanced;
    // Balanced can't make this q75 photo smaller, so Small is tried, after
    // the main encode (0.9): a cancel there ends the job as cancelled,
    // with nothing written, and the progress never goes back.
    let dir = TestDir::new("compress-webp-trial-cancel");
    let src = smooth_photo(800, 450, 75.0);
    let (path, job) = plan_one(&dir, "q75.webp", &src, compress_op(Balanced));
    let before = SourceState::of(&path);
    let ctx = EngineCtx::default();
    let cancel = ctx.cancel.clone();
    let mut seen = Vec::new();
    let err = run_job(&job, &ctx, &mut |p| {
        if p > 0.9 {
            cancel.cancel();
        }
        seen.push(p);
    })
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{}", err.detail);
    assert!(seen.iter().any(|&p| p > 0.9), "{seen:?}");
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
    assert!(!planned_file(&job).exists());
}

#[test]
fn a_cancel_at_the_try_small_trials_last_report_is_a_cancel() {
    use grape_core::CompressPreset::{Balanced, High};
    // Balanced and High can't make this q75 photo smaller, and Small is
    // tried after them: its last report (0.98) comes once its file is
    // made. A cancel there ended the job "already compact" (NoChange).
    let src = smooth_photo(800, 450, 75.0);
    for preset in [Balanced, High] {
        let dir = TestDir::new("compress-webp-trial-last");
        let (_, job) = plan_one(&dir, "q75.webp", &src, compress_op(preset));
        let mut seen = Vec::new();
        let err = run_job(&job, &EngineCtx::default(), &mut |p| seen.push(p)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange, "{preset:?}: {}", err.detail);
        assert_eq!(seen.last(), Some(&0.98), "{preset:?}: {seen:?}");
        cancelled_at("q75.webp", &src, compress_op(preset), seen.len());
    }
}

#[test]
fn compress_reads_a_webp_only_to_its_riffs_end() {
    use grape_core::CompressPreset::High;
    // A photo in the RIFF; past its end, where libwebp (so Pillow,
    // ImageMagick and browsers) never looks, chunks image-webp's walk of an
    // extended file still reached (as far again as the RIFF's size less 12
    // bytes from the end of VP8X): an ANMF frame of blue, which Compress
    // wrote instead of the photo, and an ALPH chunk, which made it half
    // transparent (refused since).
    let dir = TestDir::new("compress-webp-riff-end");
    let (w, h) = (256u32, 160u32);
    let rgb = photo(w, h);
    let rgba = image::RgbaImage::from_fn(w, h, |x, y| {
        let [r, g, b] = rgb.get_pixel(x, y).0;
        image::Rgba([r, g, b, if x < w / 2 { 40 } else { 255 }])
    });
    let with_alpha = lossy_webp(rgba.as_raw(), w, h, true, 95.0);
    let vp8 = riff_chunk(b"VP8 ", &webp_chunk(&with_alpha, b"VP8 ").unwrap());
    let alph = riff_chunk(b"ALPH", &webp_chunk(&with_alpha, b"ALPH").unwrap());
    let mut blue = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut blue)
        .write_image(
            &[10, 10, 220, 255].repeat((w * h) as usize),
            w,
            h,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    let u24 = |v: u32| v.to_le_bytes()[..3].to_vec();
    let frame = [u24(0), u24(0), u24(w - 1), u24(h - 1), u24(100), vec![0]].concat();
    let anmf = riff_chunk(b"ANMF", &[frame, blue[12..].to_vec()].concat());
    for (name, flags, after) in [("frame.webp", 0, anmf), ("alph.webp", 0x10, alph)] {
        let inside = riff(&[vp8x_chunk(flags, w, h), vp8.clone()]);
        let shown = image::load_from_memory(&riff(std::slice::from_ref(&vp8)))
            .unwrap()
            .to_rgb8();
        let file = [inside, after].concat();
        let outcome =
            compress(&dir, name, &file, High).unwrap_or_else(|e| panic!("{name}: {}", e.detail));
        let out = fs::read(&outcome.outputs[0]).unwrap();
        // Opaque, as libwebp shows it, and the photo.
        assert!(webp_chunk(&out, b"ALPH").is_none(), "{name}");
        let back = image::load_from_memory(&out).unwrap();
        assert!(!back.color().has_alpha(), "{name}");
        let back = back.to_rgb8();
        let off: u64 = back
            .as_raw()
            .iter()
            .zip(shown.as_raw())
            .map(|(&a, &b)| u64::from(a.abs_diff(b)))
            .sum();
        assert!(off < u64::from(w * h * 3) * 4, "{name}: {off}");
        fs::remove_file(&outcome.outputs[0]).unwrap();
    }
}

#[test]
fn compress_reads_a_webp_whose_vp8x_flags_a_chunk_it_lacks() {
    use grape_core::CompressPreset::High;
    let dir = TestDir::new("compress-webp-flags");
    let photo = photo(600, 400);
    let lossy = lossy_webp(photo.as_raw(), 600, 400, false, 95.0);
    let vp8 = riff_chunk(b"VP8 ", &webp_chunk(&lossy, b"VP8 ").unwrap());
    // VP8X says alpha, EXIF, XMP and ICC, and only an XMP chunk is there:
    // libwebp, Pillow and ImageMagick show the picture opaque, and
    // image-webp alone refused it ("An expected chunk was missing"). (A
    // missing profile alone it allows.)
    for (name, flags) in [
        ("alpha.webp", 0x10),
        ("tags.webp", 0x3C),
        ("icc.webp", 0x20),
    ] {
        let src = riff(&[
            vp8x_chunk(flags, 600, 400),
            vp8.clone(),
            riff_chunk(b"XMP ", b"<x:xmpmeta/>"),
        ]);
        let outcome = compress(&dir, name, &src, High).unwrap();
        let out = fs::read(&outcome.outputs[0]).unwrap();
        assert!(out.len() * 10 <= src.len() * 9, "{name}");
        let back = image::load_from_memory(&out).unwrap();
        assert!(!back.color().has_alpha(), "{name}");
        assert_eq!((back.width(), back.height()), (600, 400));
        // The XMP goes on with its flag as it was, and no flag says a
        // chunk that isn't there.
        assert_eq!(
            webp_chunk(&out, b"VP8X").unwrap()[0],
            flags & 0x04,
            "{name}"
        );
        assert_eq!(webp_chunk(&out, b"XMP ").unwrap(), b"<x:xmpmeta/>");
        fs::remove_file(&outcome.outputs[0]).unwrap();
    }
}

/// A WebP's image chunks (those a reader shows the picture from), in
/// order, in its RIFF.
fn webp_image_chunks(file: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let riff = u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize;
    webp_chunks(&file[..riff + 8])
        .into_iter()
        .filter(|(f, _)| [*b"VP8 ", *b"VP8L", *b"ALPH", *b"ANIM", *b"ANMF"].contains(f))
        .collect()
}

#[test]
fn a_webp_compress_refuses_is_refused_by_every_conversion_too() {
    use grape_core::CompressPreset::High;
    use grape_core::Format;
    let dir = TestDir::new("webp-refusals");
    let (w, h) = (256u32, 160u32);
    // Pillow's q95 red and lossless blue, as the review made them.
    let red = lossy_webp(&[200, 10, 10].repeat((w * h) as usize), w, h, false, 95.0);
    let red_vp8 = riff_chunk(b"VP8 ", &webp_chunk(&red, b"VP8 ").unwrap());
    let lossless = |px: &image::RgbaImage| {
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .write_image(
                px.as_raw(),
                px.width(),
                px.height(),
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        webp_chunk(&out, b"VP8L").unwrap()
    };
    let blue = lossless(&image::RgbaImage::from_pixel(
        w,
        h,
        image::Rgba([10, 10, 220, 255]),
    ));
    let blue_vp8l = riff_chunk(b"VP8L", &blue);
    let half = image::RgbaImage::from_fn(w, h, |x, _| {
        image::Rgba([90, 90, 90, if x < w / 2 { 40 } else { 255 }])
    });
    // Its alpha bit turned off or on.
    let bit = |on: bool| {
        let mut vp8l = lossless(&half);
        if on {
            vp8l[4] |= 0x10;
        } else {
            vp8l[4] &= !0x10;
        }
        riff_chunk(b"VP8L", &vp8l)
    };
    let with_alpha = lossy_webp(half.as_raw(), w, h, true, 95.0);
    let alph = riff_chunk(b"ALPH", &webp_chunk(&with_alpha, b"ALPH").unwrap());
    let vp8 = riff_chunk(b"VP8 ", &webp_chunk(&with_alpha, b"VP8 ").unwrap());
    let u24 = |v: u32| v.to_le_bytes()[..3].to_vec();
    let frame = [u24(0), u24(0), u24(w - 1), u24(h - 1), u24(100), vec![0]].concat();
    let anmf = |chunks: &[Vec<u8>]| riff_chunk(b"ANMF", &[frame.clone(), chunks.concat()].concat());
    let canvas = |flags: u8| vp8x_chunk(flags, w, h);
    for (name, file) in [
        // A still file holding an animation's frame of blue, after or
        // before the red picture: libwebp's still decoder showed red,
        // Pillow refused it, and every conversion wrote blue.
        (
            "frame-after.webp",
            riff(&[
                canvas(0),
                red_vp8.clone(),
                anmf(std::slice::from_ref(&blue_vp8l)),
            ]),
        ),
        (
            "frame-first.webp",
            riff(&[
                canvas(0),
                anmf(std::slice::from_ref(&blue_vp8l)),
                red_vp8.clone(),
            ]),
        ),
        // A frame's alpha: conversions applied it to the red picture.
        (
            "frame-alpha.webp",
            riff(&[
                canvas(0x10),
                red_vp8.clone(),
                anmf(&[alph.clone(), vp8.clone()]),
            ]),
        ),
        (
            "anim.webp",
            riff(&[canvas(0), riff_chunk(b"ANIM", &[0; 6]), red_vp8.clone()]),
        ),
        // An ALPH before a VP8L: converted opaque, ImageMagick shows alpha.
        (
            "alph-vp8l.webp",
            riff(&[canvas(0), alph.clone(), bit(false)]),
        ),
        (
            "two-alph.webp",
            riff(&[canvas(0x10), alph.clone(), alph.clone(), vp8.clone()]),
        ),
        (
            "unflagged.webp",
            riff(&[canvas(0), alph.clone(), vp8.clone()]),
        ),
        (
            "late-alph.webp",
            riff(&[canvas(0x10), vp8.clone(), alph.clone()]),
        ),
        // Transparent pixels in a VP8L whose bit says opaque, under
        // VP8X's alpha flag: refused once decoded.
        ("vp8l-bit-off.webp", riff(&[canvas(0x10), bit(false)])),
        (
            "canvas.webp",
            riff(&[vp8x_chunk(0, w + 1, h), red_vp8.clone()]),
        ),
    ] {
        let message = format!("{name} isn't a readable WEBP image.");
        let err = compress(&dir, name, &file, High).unwrap_err();
        assert_eq!(err.user_message, message, "{}", err.detail);
        fs::remove_file(dir.join(name)).unwrap();
        for to in [Format::Png, Format::Jpg, Format::Bmp] {
            let err = run(&dir, name, &file, Operation::convert(to)).unwrap_err();
            assert_eq!(
                err.kind,
                ErrorKind::DecodeFailed,
                "{name} {to:?}: {}",
                err.detail
            );
            assert_eq!(err.user_message, message, "{to:?}");
            fs::remove_file(dir.join(name)).unwrap();
        }
        // Remove Metadata changes no picture: its image chunks go on as
        // they are, so each reader shows what it showed.
        let outcome = strip(&dir, name, &file).unwrap_or_else(|e| panic!("{name}: {}", e.detail));
        let out = fs::read(&outcome.outputs[0]).unwrap();
        assert_eq!(webp_image_chunks(&out), webp_image_chunks(&file), "{name}");
        fs::remove_file(&outcome.outputs[0]).unwrap();
        fs::remove_file(dir.join(name)).unwrap();
    }
    // Read as libwebp reads them: an EXIF flag whose chunk is past the
    // RIFF's end, flags for chunks it lacks, and VP8X's alpha flag over an
    // opaque VP8L whose bit says none. Conversions refused the first two
    // ("An expected chunk was missing").
    let opaque = riff_chunk(
        b"VP8L",
        &lossless(&image::RgbaImage::from_pixel(
            w,
            h,
            image::Rgba([10, 10, 220, 255]),
        )),
    );
    for (name, file, want) in [
        (
            "exif-after.webp",
            [
                riff(&[canvas(0x08), red_vp8.clone()]),
                riff_chunk(b"EXIF", b"MM\0*\0\0\0\x08\0\0"),
            ]
            .concat(),
            [200u8, 10, 10],
        ),
        (
            "flags.webp",
            riff(&[canvas(0x3C), red_vp8.clone(), riff_chunk(b"XMP ", b"<x/>")]),
            [200, 10, 10],
        ),
        ("bit-off.webp", riff(&[canvas(0x10), opaque]), [10, 10, 220]),
    ] {
        let outcome = run(&dir, name, &file, Operation::convert(Format::Png))
            .unwrap_or_else(|e| panic!("{name}: {}", e.detail));
        let out = image::load_from_memory(&fs::read(&outcome.outputs[0]).unwrap()).unwrap();
        assert_eq!((out.width(), out.height()), (w, h), "{name}");
        for p in out.to_rgba8().pixels() {
            let off = p.0[..3].iter().zip(want).map(|(&a, b)| a.abs_diff(b));
            assert!(off.max().unwrap() <= 6 && p.0[3] == 255, "{name}: {p:?}");
        }
        fs::remove_file(&outcome.outputs[0]).unwrap();
        fs::remove_file(dir.join(name)).unwrap();
        // Compress reads them too (a flat picture is already compact).
        match compress(&dir, name, &file, High) {
            Ok(outcome) => fs::remove_file(&outcome.outputs[0]).unwrap(),
            Err(e) => assert_eq!(e.kind, ErrorKind::NoChange, "{name}: {}", e.detail),
        }
        fs::remove_file(dir.join(name)).unwrap();
        let outcome = strip(&dir, name, &file).unwrap_or_else(|e| panic!("{name}: {}", e.detail));
        let out = fs::read(&outcome.outputs[0]).unwrap();
        assert_eq!(webp_image_chunks(&out), webp_image_chunks(&file), "{name}");
        fs::remove_file(&outcome.outputs[0]).unwrap();
        fs::remove_file(dir.join(name)).unwrap();
    }
}

#[test]
fn a_vp8ls_alpha_bit_vp8x_doesnt_flag_is_read_as_libwebp_reads_it() {
    use grape_core::CompressPreset::High;
    use grape_core::Format;
    // A lossless picture whose VP8L header says alpha, in a VP8X whose
    // flags don't: libwebp's still decoder and its demuxer (Pillow) show
    // its alpha, so image-webp reads it with the flag on. Conversions and
    // Compress refused it.
    let dir = TestDir::new("webp-vp8l-bit");
    let (w, h) = (256u32, 160u32);
    let lossless = |px: &image::RgbaImage| {
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .write_image(px.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .unwrap();
        let mut vp8l = webp_chunk(&out, b"VP8L").unwrap();
        vp8l[4] |= 0x10;
        riff(&[vp8x_chunk(0, w, h), riff_chunk(b"VP8L", &vp8l)])
    };
    // Opaque blue, the bit set by hand; alpha 40 and 255, half and half.
    let blue = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 10, 220, 255]));
    let half = image::RgbaImage::from_fn(w, h, |x, _| {
        image::Rgba([90, 90, 90, if x < w / 2 { 40 } else { 255 }])
    });
    for (name, px) in [("blue.webp", &blue), ("half.webp", &half)] {
        let file = lossless(px);
        let outcome = run(&dir, name, &file, Operation::convert(Format::Png))
            .unwrap_or_else(|e| panic!("{name}: {}", e.detail));
        let out = image::load_from_memory(&fs::read(&outcome.outputs[0]).unwrap()).unwrap();
        assert_eq!(out.to_rgba8(), *px, "{name}");
        fs::remove_file(&outcome.outputs[0]).unwrap();
        fs::remove_file(dir.join(name)).unwrap();
        match compress(&dir, name, &file, High) {
            Ok(outcome) => {
                let out = fs::read(&outcome.outputs[0]).unwrap();
                let back = image::load_from_memory(&out).unwrap();
                assert_eq!(back.to_rgba8(), *px, "{name}");
                fs::remove_file(&outcome.outputs[0]).unwrap();
            }
            Err(e) => assert_eq!(e.kind, ErrorKind::NoChange, "{name}: {}", e.detail),
        }
        fs::remove_file(dir.join(name)).unwrap();
    }
}

#[test]
fn compress_refuses_a_webp_too_large_before_decoding_it() {
    let dir = TestDir::new("compress-webp-huge");
    // A lossless header of 16,383 × 16,383 and no image data: refused for
    // the memory its encode would take, before anything is decoded.
    let bits = 16_382u32 | 16_382 << 14;
    let src = riff(&[riff_chunk(
        b"VP8L",
        &[[0x2F].as_slice(), &bits.to_le_bytes()].concat(),
    )]);
    let err = compress(&dir, "big.webp", &src, grape_core::CompressPreset::High).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
    assert_eq!(err.user_message, "big.webp is too large to convert.");
    // An EXIF over 16 MiB, which Compress would have to drop: refused by
    // its header, before it's read (so it needn't be there).
    let mut big_exif = riff(&[
        vp8x_chunk(0x08, 16, 16),
        lossy_webp(&[9; 16 * 16 * 3], 16, 16, false, 80.0)[12..].to_vec(),
    ]);
    let size = (16u32 << 20) + 2;
    big_exif.extend_from_slice(b"EXIF");
    big_exif.extend_from_slice(&size.to_le_bytes());
    let riff_size = u32::try_from(big_exif.len() - 8).unwrap() + size;
    big_exif[4..8].copy_from_slice(&riff_size.to_le_bytes());
    let err = compress(
        &dir,
        "tags.webp",
        &big_exif,
        grape_core::CompressPreset::High,
    )
    .unwrap_err();
    assert_eq!(err.user_message, "tags.webp is too large to convert.");
    // A canvas other than its image's size is damaged, as libwebp has it,
    // not too large: a 16,383² canvas over a 96 × 64 picture, and a 1 × 1
    // one that would pass the memory check while image-webp decoded the
    // whole frame.
    let image = lossy_webp(photo(96, 64).as_raw(), 96, 64, false, 80.0);
    for (w, h) in [(16_383, 16_383), (1, 1)] {
        let src = riff(&[vp8x_chunk(0, w, h), image[12..].to_vec()]);
        let err =
            compress(&dir, "canvas.webp", &src, grape_core::CompressPreset::High).unwrap_err();
        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
        assert_eq!(err.user_message, "canvas.webp isn't a readable WEBP image.");
        fs::remove_file(dir.join("canvas.webp")).unwrap();
    }
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
fn a_cancel_at_webp_compress_last_report_is_a_cancel() {
    // A compact file, which would end NoChange: cancelled at the report
    // after libwebp's (0.9), it says Cancelled.
    let dir = TestDir::new("cancel-webp-late");
    let small = lossy_webp(photo(800, 600).as_raw(), 800, 600, false, 40.0);
    let (src, job) = plan_one(
        &dir,
        "pic.webp",
        &small,
        compress_op(grape_core::CompressPreset::High),
    );
    let before = SourceState::of(&src);
    let ctx = EngineCtx::default();
    let cancel = ctx.cancel.clone();
    let mut seen = Vec::new();
    let err = run_job(&job, &ctx, &mut |p| {
        seen.push(p);
        if p >= 0.9 {
            cancel.cancel();
        }
    })
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{}: {seen:?}", err.detail);
    assert_eq!(seen.iter().filter(|&&p| p >= 0.9).count(), 1, "{seen:?}");
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
    // A WebP: before it's read, and while libwebp encodes it.
    let webp = lossy_webp(photo(600, 600).as_raw(), 600, 600, false, 95.0);
    for nth in [1, 5] {
        cancelled_at(
            "photo.webp",
            &webp,
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
    // A WebP named .jpg is compressed as a WebP, and keeps its name.
    let webp = lossy_webp(photo(600, 400).as_raw(), 600, 400, false, 95.0);
    let out = compress(&dir, "shot.jpg", &webp, grape_core::CompressPreset::High).unwrap();
    assert!(out.outputs[0].to_string_lossy().ends_with(".jpg"));
    assert!(fs::read(&out.outputs[0]).unwrap().starts_with(b"RIFF"));
    // Compress reads only JPEG, PNG, TIFF and WebP: a GIF named .jpg
    // can't be read.
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
