//! SVG end to end with Grape's real SVG worker (DECISIONS D39), always on:
//! the worker is this package's `grape-svg-worker` binary, which installs
//! the same heap-bound allocator as Grape and runs the same
//! `grape_engines::svg::worker_main`, started as Grape starts itself,
//! `<worker> --grape-svg-worker`. Everything is generated here; nothing
//! is fetched.
//!
//! Jobs go through `run_job` with `SidecarPaths::svg_worker`, as the app
//! runs them, or through `svg::it::run` where a test lowers a bound (the
//! deadline, the heap) or names its own work folder. On Windows the worker
//! is also contained as the app contains it (a Job Object per helper,
//! `set_contain`). Every test checks that its source is unchanged, that
//! nothing is left next to it, and that no `grape-svg-…` work folder is
//! left (so the tests take turns).

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use common::{assert_no_leftovers, plan_one, planned_file, SourceState, TestDir};
use flate2::write::GzEncoder;
use flate2::Compression;
use grape_core::{Format, Operation};
use grape_engines::svg::it::{self, Options};
use grape_engines::{run_job, EngineCtx, ErrorKind, GrapeError, JobOutcome, Note, SidecarPaths};
use image::{DynamicImage, GenericImageView, ImageFormat, RgbImage};

/// The worker, built with this package.
const WORKER: &str = env!("CARGO_BIN_EXE_grape-svg-worker");

/// The tests take turns, so each can tell its own work folders from
/// another's.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn sidecars() -> SidecarPaths {
    contain_like_the_app();
    SidecarPaths {
        svg_worker: Some(PathBuf::from(WORKER)),
        ..SidecarPaths::default()
    }
}

/// How many workers [`contain_like_the_app`]'s hook has contained.
#[cfg(windows)]
static CONTAINED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// On Windows, sets the hook the app's `contain.rs` sets: a Job Object of
/// its own for every helper (`grape_platform::windows::job`), once for this
/// test binary. Elsewhere there's nothing to set.
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

/// The `grape-svg-…` folders in `parent`.
fn work_folders(parent: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("grape-svg-"))
        .collect();
    names.sort();
    names
}

/// One job's result.
struct Ran {
    dest: PathBuf,
    result: Result<JobOutcome, GrapeError>,
    progress: Vec<f32>,
    /// When the cancel was asked for, if it was.
    cancelled_at: Option<Instant>,
}

/// How a job is run.
enum Via {
    /// `run_job`, as the app runs it.
    RunJob,
    /// `svg::it::run` with these options (the work folder in the test's
    /// own folder).
    Door(Options),
}

/// Writes `bytes` as `name` in `dir`, plans `op` for it, runs it `via`,
/// and checks the source, the folder and the work folders. A cancel is
/// asked for `cancel_after` the start, if given.
fn run(
    dir: &TestDir,
    name: &str,
    bytes: &[u8],
    op: Operation,
    via: Via,
    cancel_after: Option<Duration>,
) -> Ran {
    let work_parent = match &via {
        Via::RunJob => std::env::temp_dir(),
        Via::Door(_) => {
            let work = dir.join("work");
            fs::create_dir_all(&work).unwrap();
            work
        }
    };
    let before = work_folders(&work_parent);
    let (src, job) = plan_one(dir, name, bytes, op);
    let state = SourceState::of(&src);
    let ctx = EngineCtx {
        sidecars: sidecars(),
        ..EngineCtx::default()
    };
    let cancelled_at = Arc::new(Mutex::new(None));
    if let Some(delay) = cancel_after {
        let (token, at) = (ctx.cancel.clone(), Arc::clone(&cancelled_at));
        thread::spawn(move || {
            thread::sleep(delay);
            *at.lock().unwrap() = Some(Instant::now());
            token.cancel();
        });
    }
    let mut progress = Vec::new();
    let result = {
        let mut seen = |fraction: f32| progress.push(fraction);
        match via {
            Via::RunJob => run_job(&job, &ctx, &mut seen),
            Via::Door(options) => {
                let options = Options {
                    work_parent: Some(work_parent.clone()),
                    ..options
                };
                it::run(&job, &ctx, &mut seen, Path::new(WORKER), &options)
            }
        }
    };
    state.assert_unchanged();
    assert_no_leftovers(src.parent().unwrap());
    assert_eq!(work_folders(&work_parent), before, "a work folder was left");
    assert!(
        progress.windows(2).all(|w| w[0] <= w[1]),
        "progress went back: {progress:?}"
    );
    let cancelled_at = *cancelled_at.lock().unwrap();
    Ran {
        dest: planned_file(&job),
        result,
        progress,
        cancelled_at,
    }
}

fn convert(dir: &TestDir, name: &str, bytes: &[u8], to: Format) -> Ran {
    run(dir, name, bytes, Operation::convert(to), Via::RunJob, None)
}

/// The output, decoded, and the job's notes.
fn written(ran: Ran) -> (DynamicImage, Vec<Note>) {
    let outcome = ran.result.unwrap();
    assert_eq!(outcome.outputs, vec![ran.dest.clone()]);
    assert_eq!(ran.progress.last(), Some(&1.0));
    (image::open(&ran.dest).unwrap(), outcome.notes)
}

fn refused(ran: Ran, kind: ErrorKind, message: &str) -> GrapeError {
    let err = ran.result.unwrap_err();
    assert_eq!(
        (err.kind, err.user_message.as_str()),
        (kind, message),
        "{err:?}"
    );
    assert!(!ran.dest.exists(), "an output was written");
    err
}

/// An icon: a 24 × 24 viewBox, a red square in a transparent margin.
const ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
    <rect x="6" y="6" width="12" height="12" fill="#ff0000"/></svg>"##;

/// 300 × 200, its left half red and its right half blue, opaque.
const FLAG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="200">
    <rect width="150" height="200" fill="#ff0000"/>
    <rect x="150" width="150" height="200" fill="#0000ff"/></svg>"##;

fn png(img: &DynamicImage) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    img.write_to(&mut bytes, ImageFormat::Png).unwrap();
    bytes.into_inner()
}

/// A solid red PNG.
fn red_png(width: u32, height: u32) -> Vec<u8> {
    png(&DynamicImage::ImageRgb8(RgbImage::from_pixel(
        width,
        height,
        image::Rgb([255, 0, 0]),
    )))
}

fn is_red(pixel: [u8; 4]) -> bool {
    pixel[0] > 200 && pixel[1] < 60 && pixel[2] < 60 && pixel[3] > 200
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = GzEncoder::new(Vec::new(), Compression::fast());
    out.write_all(bytes).unwrap();
    out.finish().unwrap()
}

/// Standard base64, for `data:` URLs.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn every_raster_target_draws_the_icon_at_twice_its_size() {
    let _turn = serial();
    for to in [
        Format::Jpg,
        Format::Png,
        Format::Webp,
        Format::Tiff,
        Format::Bmp,
    ] {
        let dir = TestDir::new("svg-raster");
        let (out, notes) = written(convert(&dir, "icon.svg", ICON, to));
        assert!(notes.is_empty(), "{to:?}: {notes:?}");
        assert_eq!(out.dimensions(), (48, 48), "{to:?}");
        assert!(is_red(out.get_pixel(24, 24).0), "{to:?}");
        let corner = out.get_pixel(1, 1).0;
        match to {
            // JPEG has no alpha: the margin is white.
            Format::Jpg => assert!(corner[..3].iter().all(|&c| c > 240), "{corner:?}"),
            Format::Png | Format::Webp | Format::Tiff => {
                assert_eq!(corner[3], 0, "{to:?} keeps the alpha")
            }
            _ => assert!(!is_red(corner), "{to:?}"),
        }
    }
}

#[test]
fn an_opaque_svg_of_256_px_or_more_is_drawn_at_its_size_in_rgb() {
    let _turn = serial();
    for to in [Format::Png, Format::Tiff, Format::Webp] {
        let dir = TestDir::new("svg-flag");
        let (out, _) = written(convert(&dir, "flag.svg", FLAG, to));
        assert_eq!(out.dimensions(), (300, 200), "{to:?}");
        assert_eq!(out.color(), image::ColorType::Rgb8, "{to:?}");
        assert_eq!(out.get_pixel(10, 10).0[..3], [255, 0, 0]);
        assert_eq!(out.get_pixel(290, 190).0[..3], [0, 0, 255]);
    }
}

#[test]
fn a_linked_image_is_never_read() {
    let _turn = serial();
    let dir = TestDir::new("svg-leak");
    let secret = dir.join("secret.png");
    fs::write(&secret, red_png(16, 16)).unwrap();
    let absolute = std::path::absolute(&secret).unwrap();
    for href in [
        absolute.to_string_lossy().into_owned(),
        "secret.png".to_string(),
    ] {
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="300" height="300">
                 <image xlink:href="{href}" width="300" height="300"/>
                 <image href="{href}" x="10" width="100" height="100"/>
                 <filter id="f"><feImage href="{href}"/></filter>
                 <rect width="300" height="300" filter="url(#f)"/></svg>"#
        );
        let (out, notes) = written(convert(&dir, "invoice.svg", svg.as_bytes(), Format::Png));
        assert_eq!(notes, vec![Note::ImagesLeftOut(1)], "{href}");
        let red = out.to_rgba8().pixels().filter(|p| is_red(p.0)).count();
        assert_eq!(red, 0, "{href}: the linked picture was drawn");
        fs::remove_file(dir.join("invoice.svg")).unwrap();
        fs::remove_file(dir.join("invoice.png")).unwrap();
    }
}

#[test]
fn an_embedded_png_is_drawn() {
    let _turn = serial();
    let dir = TestDir::new("svg-data");
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="300">
             <image href="data:image/png;base64,{}" width="300" height="300"/></svg>"#,
        base64(&red_png(4, 4))
    );
    let (out, notes) = written(convert(&dir, "badge.svg", svg.as_bytes(), Format::Png));
    assert!(notes.is_empty(), "{notes:?}");
    assert!(is_red(out.get_pixel(150, 150).0));
}

#[test]
fn text_is_drawn_with_the_systems_fonts() {
    let _turn = serial();
    let dir = TestDir::new("svg-text");
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="100">
        <text x="10" y="70" font-family="sans-serif" font-size="60" fill="black">Grape</text></svg>"#;
    let (out, _) = written(convert(&dir, "label.svg", svg, Format::Png));
    let inked = out.to_rgba8().pixels().filter(|p| p.0[3] > 0).count();
    if inked == 0 {
        let mut fonts = resvg::usvg::fontdb::Database::new();
        fonts.load_system_fonts();
        panic!(
            "no text was drawn; this machine has {} font faces, and the test needs one",
            fonts.len()
        );
    }
    // Inked within the text's box only.
    assert!(out.get_pixel(395, 5).0[3] == 0);
}

#[test]
fn a_huge_svg_is_drawn_smaller_and_says_so() {
    let _turn = serial();
    let dir = TestDir::new("svg-huge");
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1e6" height="10">
        <rect width="1e6" height="10" fill="red"/></svg>"#;
    let (out, notes) = written(convert(&dir, "banner.svg", svg, Format::Png));
    assert_eq!(notes, vec![Note::DrawnSmaller]);
    assert_eq!(out.dimensions(), (16_384, 1));
}

#[test]
fn gzip_svgs_convert_and_a_gzip_bomb_is_too_large() {
    let _turn = serial();
    let dir = TestDir::new("svg-gzip");
    for name in ["icon.svg", "icon.svgz"] {
        let (out, _) = written(convert(&dir, name, &gzip(ICON), Format::Png));
        assert_eq!(out.dimensions(), (48, 48), "{name}");
        fs::remove_file(dir.join(name)).unwrap();
        fs::remove_file(dir.join("icon.png")).unwrap();
    }
    // 64 MiB and one byte of spaces, a few hundred KiB packed.
    let mut bomb = GzEncoder::new(Vec::new(), Compression::fast());
    let spaces = vec![b' '; 1 << 20];
    for _ in 0..64 {
        bomb.write_all(&spaces).unwrap();
    }
    bomb.write_all(b" ").unwrap();
    let bomb = bomb.finish().unwrap();
    refused(
        convert(&dir, "bomb.svg", &bomb, Format::Png),
        ErrorKind::DecodeFailed,
        "bomb.svg is too large to convert.",
    );
}

#[test]
fn damaged_xml_isnt_a_readable_svg() {
    let _turn = serial();
    let dir = TestDir::new("svg-damaged");
    let err = refused(
        convert(
            &dir,
            "broken.svg",
            b"<svg xmlns='http://www.w3.org/2000/svg'><g>",
            Format::Png,
        ),
        ErrorKind::DecodeFailed,
        "broken.svg isn't a readable SVG image.",
    );
    assert!(!err.detail.contains("broken"), "{}", err.detail);
}

#[test]
fn a_png_named_svg_converts_through_the_raster_engine() {
    let _turn = serial();
    let dir = TestDir::new("svg-png");
    let (out, notes) = written(convert(&dir, "photo.svg", &red_png(5, 3), Format::Jpg));
    assert!(notes.is_empty());
    assert_eq!(out.dimensions(), (5, 3));
}

#[test]
fn svg_to_pdf_and_create_pdf_run_through_run_job() {
    let _turn = serial();
    let dir = TestDir::new("svg-pdf");
    let ran = convert(&dir, "flag.svg", FLAG, Format::Pdf);
    let outcome = ran.result.unwrap();
    assert_eq!(outcome.outputs, vec![ran.dest.clone()]);
    assert!(fs::read(&ran.dest).unwrap().starts_with(b"%PDF-"));
    // Create PDF: the SVG and a PNG.
    let dir = TestDir::new("svg-create");
    let svg_path = dir.join("a.svg");
    let png_path = dir.join("b.png");
    fs::write(&svg_path, FLAG).unwrap();
    fs::write(&png_path, red_png(3, 3)).unwrap();
    let settings = grape_core::Settings::new(dir.join("fallback"));
    let selection =
        grape_engines::build_selection(&[svg_path.clone(), png_path.clone()], &settings);
    let mut plan = grape_core::plan(
        &selection,
        &Operation::tool(grape_core::ToolKey::CreatePdf),
        &settings,
    )
    .unwrap();
    let job = plan.jobs.remove(0);
    let states = [SourceState::of(&svg_path), SourceState::of(&png_path)];
    let ctx = EngineCtx {
        sidecars: sidecars(),
        ..EngineCtx::default()
    };
    let outcome = run_job(&job, &ctx, &mut |_| {}).unwrap();
    assert_eq!(outcome.outputs, vec![planned_file(&job)]);
    for state in &states {
        state.assert_unchanged();
    }
    assert_no_leftovers(&dir.path);
}

/// A 500 × 500 feMorphology of radius 60: many seconds of work.
const MORPHOLOGY: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="500" height="500">
    <filter id="f"><feMorphology operator="dilate" radius="60"/></filter>
    <rect width="500" height="500" fill="red" filter="url(#f)"/>
    <rect x="100" y="100" width="300" height="300" fill="blue" filter="url(#f)"/></svg>"#;

#[test]
fn a_cancel_ends_the_worker_at_once() {
    let _turn = serial();
    let dir = TestDir::new("svg-cancel");
    let ran = run(
        &dir,
        "slow.svg",
        MORPHOLOGY,
        Operation::convert(Format::Png),
        Via::Door(Options::default()),
        Some(Duration::from_millis(500)),
    );
    // The job returns once the worker is killed and reaped.
    let returned = Instant::now();
    let at = ran.cancelled_at.unwrap();
    assert!(
        returned - at < Duration::from_secs(2),
        "{:?}",
        returned - at
    );
    refused(ran, ErrorKind::Cancelled, "Cancelled. Nothing was saved.");
}

#[test]
fn a_worker_past_its_deadline_is_stopped() {
    let _turn = serial();
    let dir = TestDir::new("svg-timeout");
    let started = Instant::now();
    let ran = run(
        &dir,
        "slow.svg",
        MORPHOLOGY,
        Operation::convert(Format::Png),
        Via::Door(Options {
            timeout_base: Duration::from_secs(1),
            ..Options::default()
        }),
        None,
    );
    // 1 s, and 1 s for the canvas's started megapixel.
    let took = started.elapsed();
    refused(
        ran,
        ErrorKind::DecodeFailed,
        "slow.svg took too long, so Grape stopped.",
    );
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(10),
        "{took:?}"
    );
}

#[test]
fn a_worker_past_its_heap_bound_needs_too_much_memory() {
    let _turn = serial();
    let dir = TestDir::new("svg-heap");
    // 40 nested isolated groups at 1000²: each holds a canvas-sized layer.
    let mut svg =
        String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="1000">"#);
    svg.push_str(&r#"<g opacity="0.9">"#.repeat(40));
    svg.push_str(r#"<rect width="1000" height="1000" fill="red"/>"#);
    svg.push_str(&"</g>".repeat(40));
    svg.push_str("</svg>");
    let ran = run(
        &dir,
        "layers.svg",
        svg.as_bytes(),
        Operation::convert(Format::Png),
        Via::Door(Options {
            max_heap_bytes: 64 << 20,
            ..Options::default()
        }),
        None,
    );
    refused(
        ran,
        ErrorKind::DecodeFailed,
        "layers.svg needs too much memory, so Grape stopped.",
    );
    // With Grape's own bound it draws.
    let dir = TestDir::new("svg-heap");
    let (out, _) = written(convert(&dir, "layers.svg", svg.as_bytes(), Format::Png));
    assert_eq!(out.dimensions(), (1000, 1000));
}

#[test]
fn a_worker_that_crashes_stops_unexpectedly() {
    let _turn = serial();
    let dir = TestDir::new("svg-crash");
    // A chain of 30,000 masks, each masked by the one before (2.7 MB):
    // usvg follows it recursively past even a 64 MiB stack.
    let mut svg = String::from(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><defs><mask id="m0"><rect width="100" height="100" fill="white"/></mask>"#,
    );
    for i in 1..30_000 {
        svg.push_str(&format!(
            r#"<mask id="m{i}" mask="url(#m{})"><rect width="100" height="100" fill="white"/></mask>"#,
            i - 1
        ));
    }
    svg.push_str(r#"</defs><rect width="100" height="100" mask="url(#m29999)"/></svg>"#);
    refused(
        convert(&dir, "chain.svg", svg.as_bytes(), Format::Png),
        ErrorKind::DecodeFailed,
        "Grape's SVG helper stopped unexpectedly.",
    );
}

#[test]
fn the_worker_answers_only_to_its_flag() {
    let _turn = serial();
    for args in [
        &[][..],
        &["--grape-pdf-worker"][..],
        &["--grape-svg-worker", "x"][..],
    ] {
        let status = std::process::Command::new(WORKER)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(2), "{args:?}");
    }
}

/// On Windows the worker runs in its own Job Object, as in the app, and
/// still converts.
#[cfg(windows)]
#[test]
fn a_contained_worker_still_converts() {
    use std::sync::atomic::Ordering;

    let _turn = serial();
    let dir = TestDir::new("svg-contained");
    let before = CONTAINED.load(Ordering::SeqCst);
    let (out, _) = written(convert(&dir, "flag.svg", FLAG, Format::Png));
    assert_eq!(out.dimensions(), (300, 200));
    assert!(CONTAINED.load(Ordering::SeqCst) > before, "not contained");
}
