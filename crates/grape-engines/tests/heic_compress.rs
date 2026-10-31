//! Compress on HEIC with Grape's real image helper (feature `heif-it`,
//! D28), through `run_job` as the app runs it.
//!
//! - `GRAPE_TEST_HEIF` names the helper and `GRAPE_TEST_HEIF_FIXTURES` the
//!   folder `grape-heif-fixtures` wrote, as for `tests/heif.rs`. Without
//!   either, these tests fail rather than skip.
//! - On Windows, `grape-heif.exe` decodes and encodes (libheif, x265):
//!   each fixture is compressed, its output read again by the helper, and
//!   the metadata Grape grafted into it read from its bytes.
//! - On macOS, a Mac that can't write HEIC (GitHub's VMs) says "This Mac
//!   can't write HEIC files." before anything is decoded, with nothing
//!   written; the cases accept that, say so on stderr and stop there. One
//!   that can runs them whole.
//! - The fixtures are small, so a `free` box of 256 KiB is put after each
//!   to give Compress something to gain on: what's past the last box a
//!   reader needs is never carried.
//!
//! Every test checks that its source is unchanged, that nothing but the
//! output is left beside it, and that no `grape-heif-…` work folder is left
//! in the OS temp folder (so the tests take turns).

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use common::{assert_no_leftovers, plan_one, planned_file, SourceState, TestDir};
use grape_core::{CompressPreset, Format, Operation, ToolKey, ToolParams};
use grape_engines::{run_job, EngineCtx, ErrorKind, GrapeError, JobOutcome, Note, SidecarPaths};
use image::DynamicImage;

/// What a Mac without a HEVC encoder says.
const NO_ENCODER: &str = "This Mac can't write HEIC files.";

/// The tests take turns, so each can tell its own work folders.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn sidecars() -> SidecarPaths {
    let helper = std::env::var_os("GRAPE_TEST_HEIF")
        .map(PathBuf::from)
        .expect("set GRAPE_TEST_HEIF to the image helper to run the heif-it tests");
    assert!(helper.is_file(), "GRAPE_TEST_HEIF is not a file");
    SidecarPaths {
        heif: Some(helper),
        ..SidecarPaths::default()
    }
}

/// Fixture `name`, with a `free` box of 256 KiB after it.
fn padded(name: &str) -> Vec<u8> {
    let dir = std::env::var_os("GRAPE_TEST_HEIF_FIXTURES")
        .map(PathBuf::from)
        .expect("set GRAPE_TEST_HEIF_FIXTURES to grape-heif-fixtures' folder");
    let mut file = fs::read(dir.join(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    let pad = 256 * 1024u32;
    file.extend_from_slice(&(pad + 8).to_be_bytes());
    file.extend_from_slice(b"free");
    file.resize(file.len() + pad as usize, 0);
    file
}

/// The `grape-heif-…` folders in the OS temp folder.
fn work_folders() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("grape-heif-"))
        .collect();
    names.sort();
    names
}

/// Runs `op` on `bytes` named `name` with the helper, and checks the
/// source, the folder and the work folders. Gives the output's path too.
fn run(
    dir: &TestDir,
    name: &str,
    bytes: &[u8],
    op: Operation,
) -> (PathBuf, Result<JobOutcome, GrapeError>, Vec<f32>) {
    let before = work_folders();
    let (src, job) = plan_one(dir, name, bytes, op);
    let state = SourceState::of(&src);
    let ctx = EngineCtx {
        sidecars: sidecars(),
        ..EngineCtx::default()
    };
    let mut progress = Vec::new();
    let result = run_job(&job, &ctx, &mut |f| progress.push(f));
    state.assert_unchanged();
    assert_no_leftovers(src.parent().unwrap());
    assert_eq!(work_folders(), before, "a work folder was left");
    assert!(
        progress.windows(2).all(|w| w[0] <= w[1]),
        "progress went back: {progress:?}"
    );
    let dest = planned_file(&job);
    if result.is_err() {
        assert!(!dest.exists());
    }
    (dest, result, progress)
}

fn compress_op(preset: CompressPreset) -> Operation {
    Operation::Tool {
        tool: ToolKey::Compress,
        params: ToolParams {
            compress: preset,
            ..ToolParams::DEFAULT
        },
    }
}

/// Compress at `preset` on fixture `name` (padded): the output's bytes, or
/// `None` on a Mac that can't write HEIC (said on stderr).
fn compress(dir: &TestDir, name: &str, preset: CompressPreset) -> Option<Vec<u8>> {
    let source = padded(name);
    let (dest, result, progress) = run(dir, name, &source, compress_op(preset));
    match result {
        Ok(outcome) => {
            assert_eq!(outcome.outputs, std::slice::from_ref(&dest));
            let out = fs::read(&dest).unwrap();
            let note = Note::Compressed {
                before: source.len() as u64,
                after: out.len() as u64,
            };
            assert_eq!(outcome.notes, [note]);
            assert!(out.len() * 10 <= source.len() * 9);
            assert_eq!(progress.last(), Some(&1.0));
            fs::remove_file(&dest).unwrap();
            fs::remove_file(dir.join(name)).unwrap();
            Some(out)
        }
        Err(e) if cfg!(target_os = "macos") && e.user_message == NO_ENCODER => {
            eprintln!("this Mac can't encode HEIC: {}", e.detail);
            assert!(progress.is_empty(), "it decoded first: {progress:?}");
            fs::remove_file(dir.join(name)).unwrap();
            None
        }
        Err(e) => panic!("{name} at {preset:?}: {e:?}"),
    }
}

/// `heic` decoded by the helper (HEIC → PNG through `run_job`).
fn decoded(heic: &[u8]) -> DynamicImage {
    let dir = TestDir::new("heic-compress-it");
    let (dest, result, _) = run(&dir, "back.heic", heic, Operation::convert(Format::Png));
    result.unwrap();
    image::open(dest).unwrap()
}

/// Every place `needle` is in `hay`.
fn found(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// Where each Exif's TIFF starts in `heic`: after `Exif\0\0`, a TIFF header.
/// (`Exif\0\0` alone is found in `meta` too: an Exif item's `infe`, its
/// empty name, and the next box's size.)
fn exif_tiffs(heic: &[u8]) -> Vec<usize> {
    heic.windows(10)
        .enumerate()
        .filter(|(_, w)| w[..6] == *b"Exif\0\0" && matches!(&w[6..], b"MM\0*" | b"II*\0"))
        .map(|(at, _)| at + 6)
        .collect()
}

/// The one Exif in `heic`: IFD0's Orientation, the Exif IFD's
/// PixelXDimension and PixelYDimension, whether there's a GPS IFD, and
/// IFD0's next offset.
fn exif_of(heic: &[u8]) -> (u16, (u32, u32), bool, u32) {
    let [at] = exif_tiffs(heic)[..] else {
        panic!("not one Exif");
    };
    let tiff = &heic[at..];
    let big = &tiff[..2] == b"MM";
    let u16_at = |i: usize| {
        let b = [tiff[i], tiff[i + 1]];
        if big {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        }
    };
    let u32_at = |i: usize| {
        let b = [tiff[i], tiff[i + 1], tiff[i + 2], tiff[i + 3]];
        if big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        }
    };
    let entries = |ifd: usize| (0..usize::from(u16_at(ifd))).map(move |n| ifd + 2 + n * 12);
    let ifd0 = u32_at(4) as usize;
    let tag = |ifd: usize, tag: u16| entries(ifd).find(|&e| u16_at(e) == tag);
    let orientation = tag(ifd0, 0x0112).map(|e| u16_at(e + 8)).unwrap();
    let exif = tag(ifd0, 0x8769).map(|e| u32_at(e + 8) as usize).unwrap();
    // A SHORT or a LONG.
    let number = |e: usize| match u16_at(e + 2) {
        3 => u32::from(u16_at(e + 8)),
        _ => u32_at(e + 8),
    };
    let dims = (
        tag(exif, 0xA002).map(number).unwrap(),
        tag(exif, 0xA003).map(number).unwrap(),
    );
    let gps = tag(ifd0, 0x8825).is_some();
    let next = u32_at(ifd0 + 2 + usize::from(u16_at(ifd0)) * 12);
    (orientation, dims, gps, next)
}

/// The PSNR of `a` against `b`, in dB.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum();
    let mse = sum / a.len() as f64;
    10.0 * (255.0f64.powi(2) / mse.max(1e-9)).log10()
}

#[test]
fn a_turned_photos_metadata_comes_back_upright() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    let Some(out) = compress(&dir, "exif-xmp.heic", CompressPreset::Balanced) else {
        return;
    };
    // Turned by its irot: the helper hands it back 64 × 96, and that's
    // what the output holds, with no irot.
    assert_eq!(decoded(&out).to_rgb8().dimensions(), (64, 96));
    assert_eq!(found(&out, b"irot"), 0);
    // The source's Exif, once, saying so; its GPS kept.
    assert_eq!(exif_tiffs(&out).len(), 1);
    assert_eq!(exif_of(&out), (1, (64, 96), true, 0));
    assert_eq!(
        found(&out, b"Fixture Camera"),
        2,
        "the Exif's and the XMP's"
    );
    // The XMP, once, saying 1.
    assert_eq!(found(&out, b"<x:xmpmeta"), 1);
    assert_eq!(found(&out, b"tiff:Orientation=\"1\""), 1);
    assert_eq!(found(&out, b"tiff:Orientation=\"6\""), 0);
    assert_eq!(found(&out, b"48,51.4N"), 1);
}

#[test]
fn each_preset_shrinks_a_detailed_photo_that_reads_back_close() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    let original = decoded(&padded("q95.heic")).to_rgb8();
    assert_eq!(original.dimensions(), (1600, 1200));
    let mut sizes = Vec::new();
    for preset in [
        CompressPreset::Small,
        CompressPreset::Balanced,
        CompressPreset::High,
    ] {
        let Some(out) = compress(&dir, "q95.heic", preset) else {
            return;
        };
        let back = decoded(&out).to_rgb8();
        // Under every cap: never scaled.
        assert_eq!(back.dimensions(), (1600, 1200), "{preset:?}");
        let db = psnr(back.as_raw(), original.as_raw());
        eprintln!("q95.heic at {preset:?}: {} bytes, {db:.1} dB", out.len());
        assert!(db > 25.0, "{preset:?}: {db:.1} dB");
        sizes.push(out.len());
    }
    // A higher quality, a larger file.
    assert!(sizes[0] < sizes[1] && sizes[1] < sizes[2], "{sizes:?}");
}

#[test]
fn a_thumbnail_is_dropped_and_the_picture_kept() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    let Some(out) = compress(&dir, "thumb.heic", CompressPreset::Small) else {
        return;
    };
    assert_eq!(found(&out, b"thmb"), 0);
    let back = decoded(&out).to_rgb8();
    let original = decoded(&padded("thumb.heic")).to_rgb8();
    assert_eq!(back.dimensions(), original.dimensions());
    let db = psnr(back.as_raw(), original.as_raw());
    assert!(db > 25.0, "{db:.1} dB");
}

#[test]
fn a_wide_nclx_with_no_profile_back_is_refused_with_libheif() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    let source = padded("nclx-p3.heic");
    let (_, result, _) = run(
        &dir,
        "nclx-p3.heic",
        &source,
        compress_op(CompressPreset::Balanced),
    );
    match result {
        // libheif (grape-heif.exe) hands back no profile for an nclx.
        Err(e) if !cfg!(target_os = "macos") => {
            assert_eq!(e.kind, ErrorKind::NotYetSupported);
            assert_eq!(
                e.user_message,
                "nclx-p3.heic uses a wide color space that Compress can't keep yet."
            );
        }
        // ImageIO hands back the color space's profile: kept.
        Ok(_) if cfg!(target_os = "macos") => {}
        Err(e) if cfg!(target_os = "macos") && e.user_message == NO_ENCODER => {
            eprintln!("this Mac can't encode HEIC: {}", e.detail);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_10_bit_photo_is_refused_before_the_helper_starts() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    // The same refusals hold with the real helper there: a file the scan
    // refuses never starts it (nothing to time here but the words).
    let source = padded("exif-xmp.heic");
    let at = source.windows(4).position(|w| w == b"hvcC").unwrap() + 4;
    let mut deep = source.clone();
    deep[at + 17] |= 2;
    let (_, result, progress) = run(&dir, "deep.heic", &deep, compress_op(CompressPreset::Small));
    let e = result.unwrap_err();
    assert_eq!(
        e.user_message,
        "deep.heic is a 10-bit (HDR) photo, which Compress can't keep yet."
    );
    assert!(progress.is_empty());
}
