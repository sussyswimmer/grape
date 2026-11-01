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
//! - CI writes the fixtures once, on Windows (`grape-heif-fixtures` with
//!   `--icc`), and both OSes' jobs read the same folder: `exif-xmp.heic`,
//!   `q95.heic`, `thumb.heic`, `nclx-p3.heic`, `alpha.heic`, `icc.heic`
//!   and `grid.heic` are there on each. Two cases change a copy of
//!   `exif-xmp.heic` in memory: its Exif swapped for one with an IFD1
//!   thumbnail, and its `irot` blanked (as Samsung's) to see whether the
//!   helper's decode turns a picture by its Exif (it says on stderr;
//!   libheif must not).
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

/// Fixture `name` as it is.
fn fixture(name: &str) -> Vec<u8> {
    let dir = std::env::var_os("GRAPE_TEST_HEIF_FIXTURES")
        .map(PathBuf::from)
        .expect("set GRAPE_TEST_HEIF_FIXTURES to grape-heif-fixtures' folder");
    fs::read(dir.join(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

/// Fixture `name`, with a `free` box of 256 KiB after it.
fn padded(name: &str) -> Vec<u8> {
    pad(fixture(name))
}

/// `file` with a `free` box of 256 KiB after it.
fn pad(mut file: Vec<u8>) -> Vec<u8> {
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
    compress_bytes(dir, name, &padded(name), preset)
}

/// [`compress`] on `source`, named `name`.
fn compress_bytes(
    dir: &TestDir,
    name: &str,
    source: &[u8],
    preset: CompressPreset,
) -> Option<Vec<u8>> {
    let (dest, result, progress) = run(dir, name, source, compress_op(preset));
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

/// The content of `heic`'s first `colr` box of type `prof`: its ICC
/// profile.
fn profile(heic: &[u8]) -> Option<Vec<u8>> {
    let at = heic.windows(8).position(|w| w == b"colrprof")?;
    let size = u32::from_be_bytes(heic[at - 4..at].try_into().unwrap()) as usize;
    Some(heic[at + 8..at - 4 + size].to_vec())
}

/// Where the red in `img` is: the corner its red pixels' centre is
/// nearest, as (right, bottom).
fn red_corner(img: &image::RgbImage) -> (bool, bool) {
    let (w, h) = img.dimensions();
    let (mut sx, mut sy, mut n) = (0u64, 0u64, 0u64);
    for (x, y, p) in img.enumerate_pixels() {
        if p[0] > 200 && p[1] < 80 && p[2] < 80 {
            (sx, sy, n) = (sx + u64::from(x), sy + u64::from(y), n + 1);
        }
    }
    assert!(n > 0, "no red");
    (sx / n > u64::from(w) / 2, sy / n > u64::from(h) / 2)
}

/// `img` turned as a viewer turns it by Exif Orientation `o` (1, 3, 6, 8).
fn shown(img: image::RgbImage, o: u16) -> image::RgbImage {
    match o {
        3 => image::imageops::rotate180(&img),
        6 => image::imageops::rotate90(&img),
        8 => image::imageops::rotate270(&img),
        _ => img,
    }
}

/// The top-level boxes of `data` from `at` to `end`: type and range.
fn boxes(data: &[u8], mut at: usize, end: usize) -> Vec<([u8; 4], std::ops::Range<usize>)> {
    let mut out = Vec::new();
    while at + 8 <= end {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        assert!(size >= 8 && at + size <= end, "a box this test can't read");
        out.push((data[at + 4..at + 8].try_into().unwrap(), at..at + size));
        at += size;
    }
    out
}

/// `heic` with its primary's `irot` blanked into a `free` box, as Samsung
/// writes a portrait, and that property's association made not essential:
/// nothing in the file turns the picture any more.
fn without_irot(heic: &[u8]) -> Vec<u8> {
    let mut out = heic.to_vec();
    let top = boxes(heic, 0, heic.len());
    let (_, meta) = top.iter().find(|(k, _)| k == b"meta").unwrap();
    let (_, iprp) = boxes(heic, meta.start + 12, meta.end)
        .into_iter()
        .find(|(k, _)| k == b"iprp")
        .unwrap();
    let kids = boxes(heic, iprp.start + 8, iprp.end);
    let (_, ipco) = kids.iter().find(|(k, _)| k == b"ipco").unwrap();
    let props = boxes(heic, ipco.start + 8, ipco.end);
    let index = props.iter().position(|(k, _)| k == b"irot").unwrap() + 1;
    let irot = &props[index - 1].1;
    out[irot.start + 4..irot.start + 8].copy_from_slice(b"free");
    let (_, ipma) = kids.iter().find(|(k, _)| k == b"ipma").unwrap();
    let (version, wide) = (heic[ipma.start + 8], heic[ipma.start + 11] & 1 == 1);
    let mut at = ipma.start + 12;
    let count = u32::from_be_bytes(heic[at..at + 4].try_into().unwrap());
    at += 4;
    for _ in 0..count {
        at += if version < 1 { 2 } else { 4 };
        let n = heic[at];
        at += 1;
        for _ in 0..n {
            let (value, len) = if wide {
                (
                    usize::from(u16::from_be_bytes([heic[at], heic[at + 1]]) & 0x7FFF),
                    2,
                )
            } else {
                (usize::from(heic[at] & 0x7F), 1)
            };
            if value == index {
                out[at] &= 0x7F;
            }
            at += len;
        }
    }
    out
}

/// `heic`'s one Exif's IFD0 Orientation set to `o` (a SHORT), in place.
fn with_orientation(heic: &[u8], o: u16) -> Vec<u8> {
    let mut out = heic.to_vec();
    let [at] = exif_tiffs(heic)[..] else {
        panic!("not one Exif");
    };
    assert_eq!(&heic[at..at + 2], b"MM");
    let ifd0 = at + u32::from_be_bytes(heic[at + 4..at + 8].try_into().unwrap()) as usize;
    let n = usize::from(u16::from_be_bytes([heic[ifd0], heic[ifd0 + 1]]));
    let e = (0..n)
        .map(|k| ifd0 + 2 + 12 * k)
        .find(|&e| heic[e..e + 2] == [0x01, 0x12])
        .unwrap();
    assert_eq!(&heic[e + 2..e + 8], &[0, 3, 0, 0, 0, 1]);
    out[e + 8..e + 10].copy_from_slice(&o.to_be_bytes());
    out
}

/// How far the big-endian TIFF at `at` in `heic` reaches: past IFD0, the
/// IFDs it points to, and every value stored outside its entry.
fn tiff_extent(heic: &[u8], at: usize) -> usize {
    let t = &heic[at..];
    let u16_at = |i: usize| usize::from(u16::from_be_bytes([t[i], t[i + 1]]));
    let u32_at = |i: usize| u32::from_be_bytes(t[i..i + 4].try_into().unwrap()) as usize;
    let mut end = 8;
    let mut ifds = vec![u32_at(4)];
    while let Some(ifd) = ifds.pop() {
        let n = u16_at(ifd);
        end = end.max(ifd + 2 + 12 * n + 4);
        for e in (0..n).map(|k| ifd + 2 + 12 * k) {
            let size = [0, 1, 1, 2, 4, 8, 1, 1, 2, 4, 8][u16_at(e + 2).min(10)] * u32_at(e + 4);
            if matches!(u16_at(e), 0x8769 | 0x8825) {
                ifds.push(u32_at(e + 8));
            } else if size > 4 {
                end = end.max(u32_at(e + 8) + size);
            }
        }
    }
    end
}

#[test]
fn alpha_is_kept() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    let Some(out) = compress(&dir, "alpha.heic", CompressPreset::Balanced) else {
        return;
    };
    let back = decoded(&out).to_rgba8();
    assert_eq!(back.dimensions(), (64, 64));
    // Left half opaque, right half 128, red under both.
    for (x, alpha) in [(8, 255), (56, 128)] {
        let p = back.get_pixel(x, 32);
        assert!(p[0] > 230 && p[1] < 30 && p[2] < 30, "{x}: {p:?}");
        assert!(p[3].abs_diff(alpha) <= 8, "{x}: {p:?}");
    }
}

#[test]
fn an_icc_profile_is_kept() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    let source = profile(&fixture("icc.heic")).expect("icc.heic has no prof");
    let Some(out) = compress(&dir, "icc.heic", CompressPreset::Balanced) else {
        return;
    };
    let kept = profile(&out).expect("no profile in the output");
    // libheif writes the profile it's given; ImageIO writes its color
    // space's, which may be laid out its own way.
    if cfg!(target_os = "macos") {
        eprintln!(
            "icc.heic: ImageIO wrote a {}-byte profile for a {}-byte one, {}",
            kept.len(),
            source.len(),
            if kept == source {
                "the same bytes"
            } else {
                "other bytes"
            }
        );
    } else {
        assert_eq!(kept, source);
    }
    let back = decoded(&out).to_rgb8();
    let p = back.get_pixel(32, 32);
    assert!(p.0.iter().all(|&c| c.abs_diff(128) <= 6), "{p:?}");
}

#[test]
fn a_grid_is_written_as_one_picture() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    let source = fixture("grid.heic");
    assert_eq!(found(&source, b"grid"), 1);
    let Some(out) = compress(&dir, "grid.heic", CompressPreset::Balanced) else {
        return;
    };
    // libheif writes one picture of 128 × 128: no grid, no tiles.
    if !cfg!(target_os = "macos") {
        assert_eq!(found(&out, b"grid"), 0);
        assert_eq!(found(&out, b"dimg"), 0);
    }
    let back = decoded(&out).to_rgb8();
    assert_eq!(back.dimensions(), (128, 128));
    for ((x, y), color) in [
        ((32, 32), [255, 0, 0]),
        ((96, 32), [0, 255, 0]),
        ((32, 96), [0, 0, 255]),
        ((96, 96), [255, 255, 255]),
    ] {
        let p = back.get_pixel(x, y);
        let near = p.0.iter().zip(color).all(|(&a, b)| a.abs_diff(b) <= 24);
        assert!(near, "({x}, {y}): {p:?}, not {color:?}");
    }
}

#[test]
fn a_turned_photos_thumbnail_goes() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    // exif-xmp.heic's Exif (turned by its irot), its TIFF replaced in place
    // by one with Orientation 6, an Exif IFD with the stored size, and an
    // IFD1 thumbnail; the rest of the old TIFF zeroed.
    let mut source = fixture("exif-xmp.heic");
    let [at] = exif_tiffs(&source)[..] else {
        panic!("not one Exif");
    };
    let room = tiff_extent(&source, at);
    let mut tiff = b"MM\0*\0\0\0\x08".to_vec();
    let entry = |t: &mut Vec<u8>, tag: u16, kind: u16, value: [u8; 4]| {
        t.extend_from_slice(&tag.to_be_bytes());
        t.extend_from_slice(&kind.to_be_bytes());
        t.extend_from_slice(&1u32.to_be_bytes());
        t.extend_from_slice(&value);
    };
    // IFD0 at 8 (2 entries), the Exif IFD at 38 (2), IFD1 at 68 (3), the
    // thumbnail at 110 (16 bytes).
    tiff.extend_from_slice(&2u16.to_be_bytes());
    entry(&mut tiff, 0x0112, 3, [0, 6, 0, 0]);
    entry(&mut tiff, 0x8769, 4, 38u32.to_be_bytes());
    tiff.extend_from_slice(&68u32.to_be_bytes());
    tiff.extend_from_slice(&2u16.to_be_bytes());
    entry(&mut tiff, 0xA002, 4, 96u32.to_be_bytes());
    entry(&mut tiff, 0xA003, 4, 64u32.to_be_bytes());
    tiff.extend_from_slice(&0u32.to_be_bytes());
    tiff.extend_from_slice(&3u16.to_be_bytes());
    entry(&mut tiff, 0x0103, 3, [0, 6, 0, 0]);
    entry(&mut tiff, 0x0201, 4, 110u32.to_be_bytes());
    entry(&mut tiff, 0x0202, 4, 16u32.to_be_bytes());
    tiff.extend_from_slice(&0u32.to_be_bytes());
    tiff.extend_from_slice(b"\xFF\xD8thumbnail...\xFF\xD9");
    assert_eq!(tiff.len(), 126);
    assert!(tiff.len() <= room, "{room}");
    source[at..at + room].fill(0);
    source[at..at + tiff.len()].copy_from_slice(&tiff);
    let source = pad(source);
    assert_eq!(exif_of(&source), (6, (96, 64), false, 68));
    let Some(out) = compress_bytes(&dir, "thumb-exif.heic", &source, CompressPreset::Balanced)
    else {
        return;
    };
    // Upright now: Orientation 1, the new size, and no IFD1.
    assert_eq!(exif_of(&out), (1, (64, 96), false, 0));
}

#[test]
fn whether_the_decoder_turns_a_picture_by_its_exif() {
    let _turn = serial();
    let dir = TestDir::new("heic-compress-it");
    // exif-xmp.heic stored 96 × 64, its red block top left, with nothing
    // that turns it but its Exif (its irot blanked, as Samsung's).
    let stored = without_irot(&fixture("exif-xmp.heic"));
    for o in [6, 3] {
        let heic = with_orientation(&stored, o);
        let back = decoded(&heic).to_rgb8();
        let turned = (back.dimensions(), red_corner(&back)) != ((96, 64), (false, false));
        eprintln!(
            "Orientation {o}: the helper decoded {:?}, red at {:?}: {} turns by Exif: {}",
            back.dimensions(),
            red_corner(&back),
            if cfg!(target_os = "macos") {
                "ImageIO"
            } else {
                "libheif"
            },
            if turned { "yes" } else { "no" }
        );
        // libheif never does.
        if !cfg!(target_os = "macos") {
            assert!(!turned, "{o}");
        }
        // Compress on it: upright as a viewer shows it, for 6 whichever
        // way the helper decoded it (D28's swapped-sides rule); for 3 a
        // helper that turns by Exif can't be told (D28's limit), and
        // libheif doesn't.
        let source = pad(heic);
        let name = format!("exif-{o}.heic");
        let Some(out) = compress_bytes(&dir, &name, &source, CompressPreset::Balanced) else {
            continue;
        };
        // As a viewer shows it: one on this decoder turns by the Exif if
        // the decoder did (Preview on ImageIO), else as the Exif says.
        let (kept, ..) = exif_of(&out);
        let back = decoded(&out).to_rgb8();
        let display = if turned { back } else { shown(back, kept) };
        let upright = match o {
            6 => ((64, 96), (true, false)),
            _ => ((96, 64), (true, true)),
        };
        let ok = (display.dimensions(), red_corner(&display)) == upright;
        eprintln!("Orientation {o}: Compress kept {kept}; shown upright: {ok}");
        if o == 6 || !cfg!(target_os = "macos") {
            assert!(
                ok,
                "{o}: {:?}",
                (display.dimensions(), red_corner(&display))
            );
        }
    }
}
