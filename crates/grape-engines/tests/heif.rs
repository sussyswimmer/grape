//! HEIC end to end with Grape's real HEIC helper (feature `heif-it`,
//! DECISIONS D33).
//!
//! - `GRAPE_TEST_HEIF` names the helper: `grape-heif.exe` from CI's
//!   grape-heif job on Windows, grape-platform's `grape-heif-worker` on
//!   macOS (`cargo build -p grape-platform --features heif-it --bin
//!   grape-heif-worker`).
//! - `GRAPE_TEST_HEIF_FIXTURES` names the folder `grape-heif-fixtures`
//!   wrote (heic-spec §5.6).
//!
//! Without either, these tests fail rather than skip. Jobs go through
//! `run_job` with `SidecarPaths::heif`, as the app runs them; on Windows
//! the helper is also contained as the app contains it (`set_contain` with
//! a Job Object per helper, so started detached, in a one-process job with
//! the 2 GiB cap). Every test
//! checks that its source is unchanged, that nothing is left next to it,
//! and that no `grape-heif-…` work folder is left in the OS temp folder
//! (so the tests take turns).
//!
//! On macOS a Mac without a HEVC encoder (GitHub's VMs among them) says
//! "This Mac can't write HEIC files."; the encode cases accept that, say so
//! on stderr and stop there. Mac encoding itself is manual QA.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use common::{assert_no_leftovers, plan_one, planned_file, SourceState, TestDir};
use grape_core::{Format, Operation};
use grape_engines::{
    run_job, supports_with, CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome, SidecarPaths,
};
use grape_heif_proto::{Limits, Message, Op, Request, PROTOCOL, WORKER_FLAG};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbImage, RgbaImage};

/// What a Mac without a HEVC encoder says.
const NO_ENCODER: &str = "This Mac can't write HEIC files.";

/// `opaque.heic`'s blocks, row by row (4 across, 3 down, 32×32 each).
const BLOCKS: [[u8; 3]; 12] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 255],
    [0, 0, 0],
    [128, 128, 128],
    [255, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [255, 128, 0],
    [128, 0, 255],
    [0, 128, 128],
];

/// The tests take turns, so each can tell its own work folders from
/// another's.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn helper() -> PathBuf {
    let path = std::env::var_os("GRAPE_TEST_HEIF")
        .map(PathBuf::from)
        .expect("set GRAPE_TEST_HEIF to the HEIC helper to run the heif-it tests");
    assert!(
        path.is_file(),
        "GRAPE_TEST_HEIF is not a file: {}",
        path.display()
    );
    path
}

fn fixture(name: &str) -> Vec<u8> {
    let dir = std::env::var_os("GRAPE_TEST_HEIF_FIXTURES")
        .map(PathBuf::from)
        .expect("set GRAPE_TEST_HEIF_FIXTURES to grape-heif-fixtures' folder");
    fs::read(dir.join(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn sidecars() -> SidecarPaths {
    contain_like_the_app();
    SidecarPaths {
        heif: Some(helper()),
        ..SidecarPaths::default()
    }
}

/// How many helpers [`contain_like_the_app`]'s hook has contained.
#[cfg(windows)]
static CONTAINED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// On Windows, sets the hook the app's `contain.rs` sets: a Job Object of
/// its own for every helper (`grape_platform::windows::job`), once for
/// this test binary. Under cargo the job is nested in cargo's, so it has
/// every limit but the UI ones. Elsewhere there's nothing to set.
fn contain_like_the_app() {
    #[cfg(windows)]
    {
        use std::process::Child;
        use std::sync::atomic::Ordering;
        use std::sync::Once;

        use grape_engines::{set_contain, Contain, Contained};
        use grape_platform::windows::job::JobGuard;

        #[derive(Debug)]
        struct Jobs;
        struct Job(JobGuard);
        impl Contained for Job {
            fn peak_memory(&self) -> Option<u64> {
                self.0.peak_memory()
            }
        }
        impl Contain for Jobs {
            fn contain(
                &self,
                child: &Child,
                max_memory: Option<u64>,
            ) -> Result<Box<dyn Contained>, String> {
                let guard = JobGuard::assign(child, max_memory).map_err(|e| e.to_string())?;
                CONTAINED.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Job(guard)))
            }
        }
        static ONCE: Once = Once::new();
        ONCE.call_once(|| assert!(set_contain(Arc::new(Jobs)), "the hook was set already"));
    }
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

/// One job's result.
struct Ran {
    dest: PathBuf,
    result: Result<JobOutcome, GrapeError>,
    progress: Vec<f32>,
}

/// Writes `bytes` as `name` in `dir`, converts it to `to` through
/// `run_job`, and checks the source, the folder and the work folders.
/// `on_progress` sees each fraction and may cancel through the token.
fn convert_with(
    dir: &TestDir,
    name: &str,
    bytes: &[u8],
    to: Format,
    on_progress: &mut dyn FnMut(f32, &CancelToken),
) -> Ran {
    let before = work_folders();
    let (src, job) = plan_one(dir, name, bytes, Operation::convert(to));
    let state = SourceState::of(&src);
    let ctx = EngineCtx {
        sidecars: sidecars(),
        ..EngineCtx::default()
    };
    let token = ctx.cancel.clone();
    let mut progress = Vec::new();
    let result = run_job(&job, &ctx, &mut |fraction| {
        progress.push(fraction);
        on_progress(fraction, &token);
    });
    state.assert_unchanged();
    assert_no_leftovers(src.parent().unwrap());
    assert_eq!(work_folders(), before, "a work folder was left");
    assert!(
        progress.windows(2).all(|w| w[0] <= w[1]),
        "progress went back: {progress:?}"
    );
    Ran {
        dest: planned_file(&job),
        result,
        progress,
    }
}

fn convert(dir: &TestDir, name: &str, bytes: &[u8], to: Format) -> Ran {
    convert_with(dir, name, bytes, to, &mut |_, _| {})
}

/// The output, or `None` on a Mac that can't encode HEIC (said on
/// stderr).
fn encoded(ran: Ran) -> Option<PathBuf> {
    match ran.result {
        Ok(outcome) => {
            assert_eq!(outcome.outputs, vec![ran.dest.clone()]);
            Some(ran.dest)
        }
        Err(e) if cfg!(target_os = "macos") && e.user_message == NO_ENCODER => {
            eprintln!("this Mac can't encode HEIC: {}", e.detail);
            None
        }
        Err(e) => panic!("{e:?}"),
    }
}

fn decoded(ran: Ran) -> DynamicImage {
    let outcome = ran.result.unwrap();
    assert_eq!(outcome.outputs, vec![ran.dest.clone()]);
    assert_eq!(ran.progress.last(), Some(&1.0));
    image::open(&ran.dest).unwrap()
}

fn refused(ran: Ran, kind: ErrorKind, message: &str) {
    let err = ran.result.unwrap_err();
    assert_eq!(
        (err.kind, err.user_message.as_str()),
        (kind, message),
        "{err:?}"
    );
    assert!(!ran.dest.exists());
}

/// Raster → HEIC → PNG, in two folders. `None` when this Mac can't encode.
fn round_trip(name: &str, bytes: &[u8]) -> Option<DynamicImage> {
    let first = TestDir::new("heif-it");
    let heic = encoded(convert(&first, name, bytes, Format::Heic))?;
    let heic = fs::read(heic).unwrap();
    assert!(heic.get(4..8) == Some(b"ftyp"), "the output isn't HEIF");
    let second = TestDir::new("heif-it");
    Some(decoded(convert(&second, "back.heic", &heic, Format::Png)))
}

fn png(img: &DynamicImage) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    img.write_to(&mut bytes, ImageFormat::Png).unwrap();
    bytes.into_inner()
}

fn mean_error(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let total: u64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| u64::from(x.abs_diff(y)))
        .sum();
    total as f64 / a.len() as f64
}

fn assert_near(found: &[u8], expected: &[u8], tolerance: u8, at: &str) {
    assert!(
        found
            .iter()
            .zip(expected)
            .all(|(&f, &e)| f.abs_diff(e) <= tolerance),
        "{at}: found {found:?}, expected {expected:?} ±{tolerance}"
    );
}

#[test]
fn the_helper_answers_a_probe() {
    let _turn = serial();
    let work = TestDir::new("heif-probe");
    let request = Request {
        protocol: PROTOCOL,
        work_dir: work.path.clone(),
        op: Op::Probe {},
        limits: Limits::DEFAULT,
    };
    let mut child = Command::new(helper())
        .arg(WORKER_FLAG)
        .env_remove("LIBHEIF_PLUGIN_PATH")
        .env_remove("LIBHEIF_SECURITY_LIMITS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut line = serde_json::to_vec(&request).unwrap();
    line.push(b'\n');
    stdin.write_all(&line).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{:?}", output.status);
    let text = String::from_utf8(output.stdout).unwrap();
    let last = text.lines().last().unwrap();
    let Ok(Message::Done(done)) = serde_json::from_str::<Message>(last) else {
        panic!("not a done line: {last}");
    };
    let caps = done.caps.unwrap();
    eprintln!("HEIC helper: {caps:?}");
    assert!(caps.decode);
    if cfg!(windows) {
        assert!(caps.encode);
        assert!(caps.codec.starts_with("libheif 1.23.7"), "{}", caps.codec);
    }
    assert!(fs::read_dir(&work.path).unwrap().next().is_none());
}

/// The way an installed Grape runs it: started detached (no console, so no
/// console host in its one-process job) and contained before it gets its
/// request, both ways.
#[cfg(windows)]
#[test]
fn the_helper_runs_contained_both_ways_as_the_app_runs_it() {
    use std::sync::atomic::Ordering;

    let _turn = serial();
    let dir = TestDir::new("heif-it");
    let before = {
        contain_like_the_app();
        CONTAINED.load(Ordering::SeqCst)
    };
    let img = decoded(convert(&dir, "odd.heic", &fixture("odd.heic"), Format::Png));
    assert_eq!((img.width(), img.height()), (3, 5));
    let blue = png(&DynamicImage::ImageRgb8(RgbImage::from_pixel(
        17,
        9,
        image::Rgb([0, 90, 200]),
    )));
    let other = TestDir::new("heif-it");
    let heic = encoded(convert(&other, "blue.png", &blue, Format::Heic)).unwrap();
    assert!(fs::read(heic).unwrap().get(4..8) == Some(b"ftyp"));
    assert_eq!(CONTAINED.load(Ordering::SeqCst), before + 2);
}

#[test]
fn the_dispatch_offers_heic_with_the_helper() {
    let _turn = serial();
    let sidecars = sidecars();
    for raster in [
        Format::Jpg,
        Format::Png,
        Format::Webp,
        Format::Tiff,
        Format::Bmp,
    ] {
        assert!(supports_with(
            &Operation::convert(raster),
            &[Some(Format::Heic)],
            &sidecars
        ));
        assert!(supports_with(
            &Operation::convert(Format::Heic),
            &[Some(raster)],
            &sidecars
        ));
    }
}

#[test]
fn a_smooth_gradient_round_trips() {
    let _turn = serial();
    let img = DynamicImage::ImageRgb8(RgbImage::from_fn(64, 48, |x, y| {
        image::Rgb([(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8])
    }));
    let Some(back) = round_trip("gradient.png", &png(&img)) else {
        return;
    };
    assert_eq!((back.width(), back.height()), (64, 48));
    let error = mean_error(back.to_rgb8().as_raw(), img.to_rgb8().as_raw());
    assert!(error < 3.0, "mean error {error}");
}

#[test]
fn solid_blocks_round_trip() {
    let _turn = serial();
    let img = DynamicImage::ImageRgb8(RgbImage::from_fn(128, 96, |x, y| {
        image::Rgb(BLOCKS[(y / 32 * 4 + x / 32) as usize])
    }));
    let Some(back) = round_trip("blocks.png", &png(&img)) else {
        return;
    };
    let back = back.to_rgb8();
    assert_eq!(back.dimensions(), (128, 96));
    for (i, block) in BLOCKS.iter().enumerate() {
        let (x, y) = ((i as u32 % 4) * 32 + 16, (i as u32 / 4) * 32 + 16);
        assert_near(&back.get_pixel(x, y).0, block, 8, &format!("block {i}"));
    }
}

#[test]
fn alpha_is_kept() {
    let _turn = serial();
    let img = DynamicImage::ImageRgba8(RgbaImage::from_fn(64, 64, |x, _| {
        image::Rgba([255, 0, 0, if x < 32 { 255 } else { 128 }])
    }));
    let Some(back) = round_trip("alpha.png", &png(&img)) else {
        return;
    };
    assert!(back.color().has_alpha());
    let back = back.to_rgba8();
    assert_near(
        &back.get_pixel(16, 32).0,
        &[255, 0, 0, 255],
        12,
        "opaque half",
    );
    assert_near(
        &back.get_pixel(48, 32).0,
        &[255, 0, 0, 128],
        12,
        "translucent half",
    );
}

#[test]
fn odd_sizes_round_trip() {
    let _turn = serial();
    // Every size is tried before the test fails, so a failure names them all.
    let mut wrong = Vec::new();
    for (width, height) in [(1, 1), (2, 2), (3, 5), (17, 9)] {
        let img =
            DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, image::Rgb([0, 200, 0])));
        let Some(back) = round_trip("odd.png", &png(&img)) else {
            return;
        };
        assert_eq!((back.width(), back.height()), (width, height));
        let found = back.to_rgb8().get_pixel(0, 0).0;
        if !found
            .iter()
            .zip([0u8, 200, 0])
            .all(|(&f, e)| f.abs_diff(e) <= 12)
        {
            wrong.push(format!("{width}x{height} {:?}: {found:?}", back.color()));
        }
    }
    assert!(wrong.is_empty(), "expected [0, 200, 0] ±12: {wrong:?}");
}

/// libheif stores a profile's bytes as they are; ImageIO reads them into a
/// color space, which a made-up profile isn't (the Mac's ICC case is
/// grape-platform's `heif_imageio`).
#[cfg(windows)]
#[test]
fn the_icc_profile_round_trips() {
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder};

    let _turn = serial();
    let mut profile: Vec<u8> = (0..=255u8).cycle().take(600).collect();
    profile[16..20].copy_from_slice(b"RGB ");
    let mut bytes = Vec::new();
    let mut encoder = PngEncoder::new(&mut bytes);
    encoder.set_icc_profile(profile.clone()).unwrap();
    encoder
        .write_image(&[10, 20, 30].repeat(16 * 8), 16, 8, ExtendedColorType::Rgb8)
        .unwrap();
    let first = TestDir::new("heif-it");
    let heic = encoded(convert(&first, "icc.png", &bytes, Format::Heic)).unwrap();
    let second = TestDir::new("heif-it");
    let ran = convert(&second, "icc.heic", &fs::read(heic).unwrap(), Format::Png);
    ran.result.unwrap();
    assert_eq!(png_profile(&ran.dest), Some(profile));
}

fn png_profile(path: &Path) -> Option<Vec<u8>> {
    let mut decoder = ImageReader::open(path)
        .unwrap()
        .with_guessed_format()
        .unwrap()
        .into_decoder()
        .unwrap();
    decoder.icc_profile().unwrap()
}

#[test]
fn every_fixture_decodes_as_expected() {
    let _turn = serial();
    let dir = TestDir::new("heif-it");
    let to_png = |name: &str| decoded(convert(&dir, name, &fixture(name), Format::Png));

    let opaque = to_png("opaque.heic").to_rgb8();
    assert_eq!(opaque.dimensions(), (128, 96));
    for (i, block) in BLOCKS.iter().enumerate() {
        let (x, y) = ((i as u32 % 4) * 32 + 16, (i as u32 / 4) * 32 + 16);
        assert_near(&opaque.get_pixel(x, y).0, block, 10, &format!("block {i}"));
    }

    let alpha = to_png("alpha.heic");
    assert!(alpha.color().has_alpha());
    let alpha = alpha.to_rgba8();
    assert_eq!(alpha.dimensions(), (64, 64));
    assert!(alpha.get_pixel(16, 32).0[3].abs_diff(255) <= 6);
    assert!(alpha.get_pixel(48, 32).0[3].abs_diff(128) <= 6);
    assert_near(
        &alpha.get_pixel(48, 32).0[..3],
        &[255, 0, 0],
        12,
        "straight alpha",
    );

    // Stored 96×64 and turned a quarter clockwise: upright it's 64×96,
    // with the red corner at the top right.
    let rotated = to_png("rotate90.heic").to_rgb8();
    assert_eq!(rotated.dimensions(), (64, 96));
    assert_near(&rotated.get_pixel(56, 8).0, &[255, 0, 0], 12, "red corner");
    assert_near(&rotated.get_pixel(8, 8).0, &[255, 255, 255], 12, "top left");
    assert_near(
        &rotated.get_pixel(8, 88).0,
        &[255, 255, 255],
        12,
        "bottom left",
    );

    let grid = to_png("grid.heic").to_rgb8();
    assert_eq!(grid.dimensions(), (128, 128));
    for ((x, y), color) in [
        ((32, 32), [255, 0, 0]),
        ((96, 32), [0, 255, 0]),
        ((32, 96), [0, 0, 255]),
        ((96, 96), [255, 255, 255]),
    ] {
        assert_near(&grid.get_pixel(x, y).0, &color, 10, "grid tile");
    }

    let odd = to_png("odd.heic").to_rgb8();
    assert_eq!(odd.dimensions(), (3, 5));
    assert_near(&odd.get_pixel(1, 2).0, &[0, 200, 0], 10, "odd");

    let icc = convert(&dir, "icc.heic", &fixture("icc.heic"), Format::Png);
    icc.result.unwrap();
    let profile = png_profile(&icc.dest).expect("icc.heic's profile is kept");
    assert_eq!(profile.get(16..20), Some(&b"RGB "[..]));
}

#[test]
fn a_damaged_heic_and_another_kind_of_heif_say_so() {
    let _turn = serial();
    let dir = TestDir::new("heif-it");
    refused(
        convert(
            &dir,
            "truncated.heic",
            &fixture("truncated.heic"),
            Format::Jpg,
        ),
        ErrorKind::DecodeFailed,
        "truncated.heic isn't a readable HEIC image.",
    );
    refused(
        convert(
            &dir,
            "avif-brand.heic",
            &fixture("avif-brand.heic"),
            Format::Jpg,
        ),
        ErrorKind::DecodeFailed,
        "avif-brand.heic is a kind of HEIF image Grape can't read yet.",
    );
}

#[test]
fn unicode_and_long_paths_work() {
    let _turn = serial();
    let dir = TestDir::new("heif-it");
    let img = decoded(convert(
        &dir,
        "ảnh chụp.heic",
        &fixture("odd.heic"),
        Format::Png,
    ));
    assert_eq!((img.width(), img.height()), (3, 5));

    // Over 260 characters in all.
    let long = TestDir::new("heif-it");
    let deep = long.path.join("a".repeat(120)).join("b".repeat(120));
    fs::create_dir_all(&deep).unwrap();
    let name = format!("{}.heic", "c".repeat(40));
    let folder = TestDir { path: deep.clone() };
    let ran = convert(&folder, &name, &fixture("odd.heic"), Format::Png);
    assert!(ran.dest.as_os_str().len() > 260, "{}", ran.dest.display());
    let img = decoded(ran);
    assert_eq!((img.width(), img.height()), (3, 5));
    // `folder` is inside `long`, which removes it.
    std::mem::forget(folder);
}

#[test]
fn cancel_ends_a_large_encode_quickly() {
    let _turn = serial();
    // 30 megapixels as BMP, which is quick to write and read in a debug
    // build.
    let img = DynamicImage::ImageRgb8(RgbImage::from_fn(6000, 5000, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
    }));
    let mut bmp = Cursor::new(Vec::new());
    img.write_to(&mut bmp, ImageFormat::Bmp).unwrap();
    drop(img);
    let dir = TestDir::new("heif-it");
    let cancelled_at = Arc::new(Mutex::new(None));
    let at = Arc::clone(&cancelled_at);
    let mut armed = false;
    // 0.4 comes once input.rgba is written, as the helper starts. x265
    // takes far longer than 2 s for 30 MP, so on Windows the cancel lands
    // mid-encode. A Mac's hardware encoder can finish 30 MP in under a
    // second, so there it lands while the worker reads its input.
    let delay = if cfg!(target_os = "macos") {
        Duration::from_millis(50)
    } else {
        Duration::from_secs(2)
    };
    let ran = convert_with(
        &dir,
        "large.bmp",
        &bmp.into_inner(),
        Format::Heic,
        &mut |fraction, token| {
            if fraction >= 0.4 && !armed {
                armed = true;
                let (token, at) = (token.clone(), Arc::clone(&at));
                thread::spawn(move || {
                    thread::sleep(delay);
                    *at.lock().unwrap() = Some(Instant::now());
                    token.cancel();
                });
            }
        },
    );
    let ended = Instant::now();
    match &ran.result {
        Err(e) if cfg!(target_os = "macos") && e.user_message == NO_ENCODER => {
            eprintln!("this Mac can't encode HEIC; nothing to cancel");
            return;
        }
        Err(e) => assert_eq!(e.kind, ErrorKind::Cancelled, "{e:?}"),
        Ok(_) => panic!("a 30 MP encode finished within {delay:?}; nothing was cancelled"),
    }
    let at = cancelled_at.lock().unwrap().expect("the job was cancelled");
    let took = ended.duration_since(at);
    assert!(
        took < Duration::from_secs(2),
        "the helper took {took:?} to stop"
    );
    assert!(!ran.dest.exists());
}
