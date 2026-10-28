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
