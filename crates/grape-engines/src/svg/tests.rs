//! The SVG engine's parent side, with the test binary itself as the
//! worker: it runs only [`worker`] (`--exact … --ignored`) with [`MODE`]
//! set. Mode `real` is the real worker (`worker::serve`, without the heap
//! bound's allocator, which only Grape's and `grape-svg-worker`'s
//! executables install); the others misbehave on purpose. SVG → HEIC and
//! AVIF go through heif.rs's fake image helper; SVG → PDF and Create PDF
//! through `pdf::create`. Every test checks that its source is unchanged,
//! that nothing is left next to it, and that no work folder is left.

#![allow(clippy::unwrap_used)]

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::fs;
use std::hash::Hasher;
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ::image::codecs::jpeg::JpegEncoder;
use ::image::{DynamicImage, GenericImageView, ImageFormat, RgbImage};
use grape_core::{plan, Format, Job, JobOutput, Operation, Settings, ToolKey};
use lopdf::{Document, Object};

use super::protocol::{ImageInfo, Mode};
use super::run::{caps, check_image, is_raster, per_megapixel, Limits, Renderer};
use super::*;
use crate::heif::tests::{fake as fake_helper, nowhere as no_helper};
use crate::helper::{Contain, Contained};
use crate::output::unique_suffix;
use crate::pdf::create::{self, Helpers};
use crate::{CancelToken, ErrorKind};

/// The variable that turns [`worker`] into the worker, and names its mode.
const MODE: &str = "GRAPE_SVG_TEST_WORKER";

/// An `ftyp` box with major brand `heic` or `avif`: what heif.rs's fake
/// image helper writes first.
const FTYP_HEIC: &[u8; 24] = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
const FTYP_AVIF: &[u8; 24] = b"\0\0\0\x18ftypavif\0\0\0\0mif1avif";

/// Not a test: the worker the tests start. The test binary runs only this
/// "test" (`--exact … --ignored`) with [`MODE`] set.
#[test]
#[ignore = "the SVG worker the tests below start"]
fn worker() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let say = |line: &str| {
        let mut out = io::stdout().lock();
        out.write_all(line.as_bytes()).unwrap();
        out.write_all(b"\n").unwrap();
        out.flush().unwrap();
    };
    let wait = || thread::sleep(Duration::from_secs(60));
    let small = r#"{"image":{"width":4,"height":4,"cssWidth":2.0,"cssHeight":2.0,"scaled":false}}"#;
    match mode.as_str() {
        "crash" => {
            say(r#"{"progress":0.5}"#);
            std::process::exit(3);
        }
        "over-heap" => {
            say(r#"{"progress":0.2}"#);
            std::process::exit(crate::pdf::heap::OVER_HEAP_EXIT);
        }
        "over-caps" => {
            say(
                r#"{"image":{"width":20000,"height":20000,"cssWidth":20000.0,"cssHeight":20000.0,"scaled":false}}"#,
            );
            wait();
        }
        "lie" => {
            say(r#"{"done":{"width":4,"height":4,"channels":3,"leftOut":0,"scaled":false}}"#);
            std::process::exit(0);
        }
        "mismatch" => {
            say(small);
            say(r#"{"done":{"width":5,"height":4,"channels":3,"leftOut":0,"scaled":false}}"#);
            std::process::exit(0);
        }
        "slow" => {
            say(small);
            wait();
        }
        "noisy" => {
            say("not a message");
            say(&"x".repeat(super::protocol::MAX_LINE_BYTES + 10));
            say(r#"{"progress":0.9}"#);
            say(r#"{"progress":0.1}"#);
            say(r#"{"progress":"NaN"}"#);
            say(r#"{"unknown":1}"#);
        }
        _ => {}
    }
    if let Some(kind) = mode.strip_prefix("error-") {
        say(&format!(
            r#"{{"error":{{"kind":"{kind}","detail":"fake failure"}}}}"#
        ));
        std::process::exit(1);
    }
    let done = super::worker::serve(io::stdin().lock(), io::stdout());
    if mode == "bad-pixels" {
        let here = std::env::current_dir().unwrap();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(here.join(super::protocol::PIXELS))
            .unwrap();
        file.write_all(b"!").unwrap();
    }
    std::process::exit(if done { 0 } else { 1 });
}

/// The test binary as the worker in `mode`.
fn fake(mode: &str) -> Launch {
    Launch {
        program: std::env::current_exe().unwrap(),
        args: [
            "--exact",
            "svg::tests::worker",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ]
        .iter()
        .map(OsString::from)
        .collect(),
        envs: vec![(OsString::from(MODE), OsString::from(mode))],
        contain: None,
    }
}

/// A worker that doesn't exist: a job that starts it fails with "couldn't
/// start", so success proves nothing was started.
fn nowhere() -> Launch {
    Launch {
        program: std::env::temp_dir().join(format!("grape-no-svg-worker-{}", unique_suffix())),
        args: Vec::new(),
        envs: Vec::new(),
        contain: None,
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> TestDir {
        let path = std::env::temp_dir().join(format!("grape-svg-tests-{}", unique_suffix()));
        fs::create_dir_all(path.join("work")).unwrap();
        TestDir(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn digest(path: &Path) -> (usize, u64) {
    let bytes = fs::read(path).unwrap();
    let mut hasher = DefaultHasher::new();
    hasher.write(&bytes);
    (bytes.len(), hasher.finish())
}

/// When a run cancels its job.
#[derive(Clone, Copy)]
enum CancelAt {
    Never,
    After(Duration),
}

/// What a run gave.
struct Ran {
    result: Result<JobOutcome, GrapeError>,
    progress: Vec<f32>,
    cancelled_at: Option<Instant>,
}

/// Sources in their own folder, planned the way the app plans them.
struct Case {
    dir: TestDir,
    paths: Vec<PathBuf>,
    before: Vec<(usize, u64)>,
    job: Job,
}

fn one(name: &str, bytes: &[u8], to: Format) -> Case {
    planned(&[(name, bytes.to_vec())], Operation::convert(to))
}

fn planned(files: &[(&str, Vec<u8>)], op: Operation) -> Case {
    let dir = TestDir::new();
    let mut paths = Vec::new();
    for (name, bytes) in files {
        let path = dir.0.join(name);
        fs::write(&path, bytes).unwrap();
        paths.push(path);
    }
    let settings = Settings::new(dir.0.join("fallback"));
    let selection = crate::build_selection(&paths, &settings);
    let mut jobs = plan(&selection, &op, &settings).unwrap().jobs;
    assert_eq!(jobs.len(), 1);
    Case {
        before: paths.iter().map(|p| digest(p)).collect(),
        paths,
        job: jobs.remove(0),
        dir,
    }
}

impl Case {
    fn work(&self) -> PathBuf {
        self.dir.0.join("work")
    }

    fn limits(&self) -> Limits {
        Limits {
            work_parent: self.work(),
            ..Limits::default()
        }
    }

    fn dest(&self) -> PathBuf {
        match &self.job.output {
            JobOutput::File(path) => path.clone(),
            other => panic!("{other:?}"),
        }
    }

    fn run(&self, launch: Launch) -> Ran {
        self.run_with(launch, self.limits(), CancelAt::Never)
    }

    /// Runs the job as `run_job` would: SVG → an image through
    /// `convert_with` (HEIC and AVIF with heif.rs's fake helper), → PDF and
    /// Create PDF through `pdf::create`.
    fn run_with(&self, launch: Launch, limits: Limits, cancel: CancelAt) -> Ran {
        let ctx = EngineCtx::default();
        let token = ctx.cancel.clone();
        let cancelled_at = Arc::new(Mutex::new(None));
        if let CancelAt::After(delay) = cancel {
            let (token, at) = (token.clone(), Arc::clone(&cancelled_at));
            thread::spawn(move || {
                thread::sleep(delay);
                *at.lock().unwrap() = Some(Instant::now());
                token.cancel();
            });
        }
        let renderer = Renderer::new(launch, limits);
        let helper = fake_helper("ok");
        let helper_limits = heif::Limits {
            work_parent: self.work(),
            ..heif::Limits::default()
        };
        let mut seen = Vec::new();
        let result = {
            let mut progress = |fraction: f32| seen.push(fraction);
            let dest = self.dest();
            match self.job.op {
                Operation::Convert { to: Format::Pdf } => {
                    let helpers = Helpers {
                        heif: None,
                        svg: Some(renderer),
                    };
                    create::convert_with(
                        &self.job,
                        &self.paths[0],
                        &dest,
                        Format::Svg,
                        &ctx,
                        &mut progress,
                        &helpers,
                    )
                }
                Operation::Convert { to } => convert_with(
                    &self.job,
                    &self.paths[0],
                    &dest,
                    to,
                    &ctx,
                    &mut progress,
                    &renderer,
                    Some((&helper, &helper_limits)),
                ),
                Operation::Tool { .. } => {
                    let helpers = Helpers {
                        heif: None,
                        svg: Some(renderer),
                    };
                    create::combine_with(&self.job, &dest, &ctx, &mut progress, &helpers)
                }
            }
        };
        let cancelled_at = *cancelled_at.lock().unwrap();
        Ran {
            result,
            progress: seen,
            cancelled_at,
        }
    }

    /// The sources are unchanged, and no temp file or work folder is left.
    fn check(&self) {
        let now: Vec<(usize, u64)> = self.paths.iter().map(|p| digest(p)).collect();
        assert_eq!(now, self.before, "a source changed");
        let left: Vec<String> = listing(&self.dir.0)
            .into_iter()
            .filter(|name| name.starts_with(".grape-"))
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
        let work = listing(&self.work());
        assert!(work.is_empty(), "work folders left: {work:?}");
    }

    /// Writes `dest` and checks the folder; the outcome.
    fn done(&self, ran: Ran) -> JobOutcome {
        let outcome = ran.result.unwrap();
        assert_eq!(outcome.outputs, vec![self.dest()]);
        assert!(
            ran.progress.windows(2).all(|w| w[0] <= w[1]),
            "{:?}",
            ran.progress
        );
        assert_eq!(ran.progress.last(), Some(&1.0));
        self.check();
        outcome
    }

    /// Fails with exactly `kind` and `message`, writes nothing, and leaves
    /// nothing.
    fn refused(&self, ran: Ran, kind: ErrorKind, message: &str) -> GrapeError {
        let err = ran.result.unwrap_err();
        assert_eq!(
            (err.kind, err.user_message.as_str()),
            (kind, message),
            "{err:?}"
        );
        assert!(!self.dest().exists(), "an output was written");
        self.check();
        err
    }
}

/// A red and blue SVG: `css` CSS px, a `viewBox` of the same size, its
/// left half red and its right half blue, both opaque.
fn halves(width: u32, height: u32) -> Vec<u8> {
    let half = f64::from(width) / 2.0;
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
             <rect width="{half}" height="{height}" fill="#ff0000"/>
             <rect x="{half}" width="{half}" height="{height}" fill="#0000ff"/></svg>"##
    )
    .into_bytes()
}

/// An icon: a 24 × 24 viewBox with a red square in a transparent margin.
const ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
    <rect x="6" y="6" width="12" height="12" fill="#ff0000"/></svg>"##;

fn png(img: &DynamicImage) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    img.write_to(&mut bytes, ImageFormat::Png).unwrap();
    bytes.into_inner()
}

fn gradient(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
        ::image::Rgb([(x * 40) as u8, (y * 50) as u8, 90])
    }))
}

/// heif.rs's fake HEIC or AVIF, real HEIF around raw pixels: its ftyp,
/// then the size, the channels and the rows its primary item holds.
fn parse_fake(bytes: &[u8]) -> ([u8; 24], u32, u32, u8, Vec<u8>) {
    let ftyp: [u8; 24] = bytes[..24].try_into().unwrap();
    let fake = crate::heif::tests::parse_fake(bytes).unwrap();
    (ftyp, fake.width, fake.height, fake.channels, fake.rows)
}

#[test]
fn the_caps_follow_the_target() {
    assert_eq!(caps(Mode::Raster, Format::Png), (16_384, 67_108_864));
    assert_eq!(caps(Mode::Raster, Format::Jpg), (16_384, 67_108_864));
    assert_eq!(caps(Mode::Raster, Format::Tiff), (16_384, 67_108_864));
    assert_eq!(caps(Mode::Raster, Format::Bmp), (16_384, 67_108_864));
    assert_eq!(caps(Mode::Raster, Format::Webp), (16_383, 67_108_864));
    // `Format::Heic` stands for AVIF too: one picture of either.
    assert_eq!(caps(Mode::Raster, Format::Heic), (16_384, 35_651_584));
    assert_eq!(
        caps(Mode::Raster, Format::Avif),
        caps(Mode::Raster, Format::Heic)
    );
    assert_eq!(caps(Mode::Page, Format::Png), (16_384, 67_108_864));
    // The request sends them, never more than the worker's own.
    let limits = Limits::default();
    let sent = limits.request_limits(Mode::Raster, Format::Webp);
    assert_eq!((sent.max_side, sent.max_pixels), (16_383, 67_108_864));
    assert_eq!(sent.max_heap_bytes, 2 << 30);
    let lowered = Limits {
        worker: super::protocol::Limits {
            max_side: 100,
            ..super::protocol::Limits::DEFAULT
        },
        ..Limits::default()
    };
    assert_eq!(
        lowered.request_limits(Mode::Page, Format::Png).max_side,
        100
    );
    // D39's table.
    assert_eq!(limits.timeout_base, Duration::from_secs(60));
    assert_eq!(limits.per_megapixel, Duration::from_secs(1));
    assert_eq!(limits.worker_memory, 2560 << 20);
    // A started megapixel is 2^20 pixels: the 64 MP canvas gives 124 s in
    // all, and the QA item's 2000 × 2000 `blur.svg` 64 s.
    let deadline = |pixels: u64| limits.timeout_base + per_megapixel(limits.per_megapixel, pixels);
    assert_eq!(deadline(67_108_864), Duration::from_secs(124));
    assert_eq!(deadline(67_108_863), Duration::from_secs(124));
    assert_eq!(deadline(2000 * 2000), Duration::from_secs(64));
    assert_eq!(deadline(1 << 20), Duration::from_secs(61));
    assert_eq!(deadline((1 << 20) + 1), Duration::from_secs(62));
    assert_eq!(deadline(1), Duration::from_secs(61));
    assert_eq!(deadline(0), Duration::from_secs(60));
    assert_eq!(
        per_megapixel(Duration::from_secs(1), u64::MAX),
        Duration::from_secs(u64::from(u32::MAX))
    );
}

#[test]
fn an_image_line_past_its_caps_or_the_working_set_is_refused() {
    let sent = super::protocol::Limits::DEFAULT;
    let info = |width, height, css| ImageInfo {
        width,
        height,
        css_width: css,
        css_height: css,
        scaled: false,
    };
    assert!(check_image(&info(48, 48, 24.0), &sent, Format::Png).is_ok());
    for bad in [
        info(0, 48, 24.0),
        info(16_385, 1, 24.0),
        info(8193, 8192, 24.0),
        info(48, 48, 0.0),
        info(48, 48, f32::NAN),
        info(48, 48, f32::INFINITY),
    ] {
        assert!(check_image(&bad, &sent, Format::Png).is_err(), "{bad:?}");
    }
    // Within caps that were raised, but not within the working set: too
    // large, not a lie.
    let raised = super::protocol::Limits {
        max_side: 65_535,
        max_pixels: u64::MAX,
        ..sent
    };
    match check_image(&info(30_000, 30_000, 24.0), &raised, Format::Png) {
        Err(super::run::Ended::TooLarge(detail)) => assert!(detail.contains("MiB"), "{detail}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn raster_content_named_svg_goes_to_the_raster_engine_with_nothing_started() {
    let photo = gradient(5, 3);
    let mut bmp = Cursor::new(Vec::new());
    photo.write_to(&mut bmp, ImageFormat::Bmp).unwrap();
    let mut tiff = Cursor::new(Vec::new());
    photo.write_to(&mut tiff, ImageFormat::Tiff).unwrap();
    let mut webp = Cursor::new(Vec::new());
    photo.write_to(&mut webp, ImageFormat::WebP).unwrap();
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&photo)
        .unwrap();
    for (what, bytes) in [
        ("png", png(&photo)),
        ("jpeg", jpeg),
        ("webp", webp.into_inner()),
        ("tiff", tiff.into_inner()),
        ("bmp", bmp.into_inner()),
    ] {
        assert!(is_raster(&bytes[..16]), "{what}");
        for to in [Format::Png, Format::Pdf] {
            let case = one("logo.svg", &bytes, to);
            let outcome = case.done(case.run(nowhere()));
            assert!(outcome.notes.is_empty());
            if to == Format::Png {
                let out = ::image::open(case.dest()).unwrap();
                assert_eq!(out.dimensions(), (5, 3), "{what}");
            }
        }
    }
    // HEIC through the raster engine's decode and the helper's encode.
    let case = one("logo.svg", &png(&photo), Format::Heic);
    case.done(case.run(nowhere()));
    let (ftyp, width, height, channels, _) = parse_fake(&fs::read(case.dest()).unwrap());
    assert_eq!((ftyp, width, height, channels), (*FTYP_HEIC, 5, 3, 3));
    // Gzip and text go to the worker.
    for bytes in [
        &b"\x1f\x8b\x08\0\0\0\0\0"[..],
        &b"<svg/>"[..],
        &b"\xef\xbb\xbf<svg/>"[..],
        &b"  "[..],
        &b"BN"[..],
    ] {
        assert!(!is_raster(bytes));
        let case = one("logo.svg", bytes, Format::Png);
        case.refused(
            case.run(nowhere()),
            ErrorKind::NotYetSupported,
            "Grape's SVG helper couldn't start.",
        );
    }
}

#[test]
fn every_raster_target_draws_the_svg() {
    for to in image::RASTER {
        let case = one("logo.svg", ICON, to);
        let outcome = case.done(case.run(fake("real")));
        assert!(outcome.notes.is_empty(), "{to:?}");
        let out = ::image::open(case.dest()).unwrap();
        // 24 × 24 is drawn at twice its size.
        assert_eq!(out.dimensions(), (48, 48), "{to:?}");
        let center = out.get_pixel(24, 24).0;
        assert!(
            center[0] > 200 && center[1] < 60 && center[2] < 60,
            "{to:?}: {center:?}"
        );
        let corner = out.get_pixel(1, 1).0;
        match to {
            // JPEG flattens onto white.
            Format::Jpg => assert!(corner[..3].iter().all(|&c| c > 240), "{corner:?}"),
            // BMP keeps it too, but readers differ on 32-bit BMP alpha.
            Format::Bmp => assert_ne!(corner, center, "{to:?}"),
            _ => assert_eq!(corner[3], 0, "{to:?}: the margin keeps its alpha"),
        }
    }
}

#[test]
fn an_opaque_svg_converts_as_rgb() {
    let case = one("flag.svg", &halves(300, 200), Format::Png);
    case.done(case.run(fake("real")));
    let out = ::image::open(case.dest()).unwrap();
    assert_eq!(out.dimensions(), (300, 200));
    assert_eq!(out.color(), ::image::ColorType::Rgb8);
    assert_eq!(out.get_pixel(10, 10).0[..3], [255, 0, 0]);
    assert_eq!(out.get_pixel(290, 190).0[..3], [0, 0, 255]);
}

#[test]
fn heic_and_avif_go_through_the_image_helper() {
    for (to, ftyp) in [(Format::Heic, FTYP_HEIC), (Format::Avif, FTYP_AVIF)] {
        let case = one("flag.svg", &halves(300, 200), to);
        let ran = case.run(fake("real"));
        assert!(ran.progress.contains(&0.5), "{:?}", ran.progress);
        case.done(ran);
        let (found, width, height, channels, rows) = parse_fake(&fs::read(case.dest()).unwrap());
        assert_eq!((found, width, height, channels), (*ftyp, 300, 200, 3));
        assert_eq!(&rows[..3], &[255, 0, 0]);
        assert_eq!(&rows[rows.len() - 3..], &[0, 0, 255]);
        // With alpha: 4 channels.
        let case = one("logo.svg", ICON, to);
        case.done(case.run(fake("real")));
        let (_, width, height, channels, _) = parse_fake(&fs::read(case.dest()).unwrap());
        assert_eq!((width, height, channels), (48, 48, 4));
    }
    // Without the helper, HEIC is refused before anything starts.
    let case = one("flag.svg", &halves(300, 200), Format::Heic);
    let renderer = Renderer::new(nowhere(), case.limits());
    let err = convert_with(
        &case.job,
        &case.paths[0],
        &case.dest(),
        Format::Heic,
        &EngineCtx::default(),
        &mut |_| {},
        &renderer,
        None,
    )
    .unwrap_err();
    assert_eq!(
        (err.kind, err.user_message.as_str()),
        (
            ErrorKind::NotYetSupported,
            "Grape's image helper is missing."
        )
    );
    case.check();
    // The helper's own failures are its own.
    let case = one("flag.svg", &halves(300, 200), Format::Heic);
    let renderer = Renderer::new(fake("real"), case.limits());
    let broken = no_helper();
    let helper_limits = heif::Limits {
        work_parent: case.work(),
        ..heif::Limits::default()
    };
    let err = convert_with(
        &case.job,
        &case.paths[0],
        &case.dest(),
        Format::Heic,
        &EngineCtx::default(),
        &mut |_| {},
        &renderer,
        Some((&broken, &helper_limits)),
    )
    .unwrap_err();
    assert_eq!(err.user_message, "Grape's image helper couldn't start.");
    case.check();
}

#[test]
fn left_out_references_and_a_fitted_canvas_are_noted() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1000000" height="10">
        <image href="/no/such/photo.png" width="10" height="10"/></svg>"#;
    let case = one("wide.svg", svg, Format::Png);
    let outcome = case.done(case.run(fake("real")));
    assert_eq!(
        outcome.notes,
        vec![Note::ImagesLeftOut(1), Note::DrawnSmaller]
    );
    let out = ::image::open(case.dest()).unwrap();
    assert_eq!(out.dimensions(), (16_384, 1));
}

/// A PDF as lopdf reads it: each page's MediaBox, and its first image's
/// filter and size.
fn pdf_pages(path: &Path) -> Vec<(Vec<f32>, String, (i64, i64))> {
    let doc = Document::load(path).unwrap();
    let mut pages = Vec::new();
    for (_, id) in doc.get_pages() {
        let page = doc.get_object(id).unwrap().as_dict().unwrap();
        let media_box: Vec<f32> = page
            .get(b"MediaBox")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_float().unwrap())
            .collect();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let image = xobjects.get(b"Im0").unwrap().as_reference().unwrap();
        let image = doc.get_object(image).unwrap().as_stream().unwrap();
        let filter = match image.dict.get(b"Filter").unwrap() {
            Object::Name(name) => String::from_utf8(name.clone()).unwrap(),
            other => panic!("{other:?}"),
        };
        let size = (
            image.dict.get(b"Width").unwrap().as_i64().unwrap(),
            image.dict.get(b"Height").unwrap().as_i64().unwrap(),
        );
        pages.push((media_box, filter, size));
    }
    pages
}

#[test]
fn an_svg_page_is_its_own_size_drawn_at_300_dpi() {
    // 96 × 48 CSS px: 72 × 36 pt, drawn 300 × 150.
    let case = one("flag.svg", &halves(96, 48), Format::Pdf);
    let outcome = case.done(case.run(fake("real")));
    assert!(outcome.notes.is_empty());
    let pages = pdf_pages(&case.dest());
    assert_eq!(
        pages,
        vec![(
            vec![0.0, 0.0, 72.0, 36.0],
            "FlateDecode".to_string(),
            (300, 150)
        )]
    );
    // A 105 mm SVG makes an A6-wide page (A4 works the same, only larger).
    let a6 = br#"<svg xmlns="http://www.w3.org/2000/svg" width="105mm" height="148mm"><rect width="10" height="10"/></svg>"#;
    let case = one("a6.svg", a6, Format::Pdf);
    case.done(case.run(fake("real")));
    let pages = pdf_pages(&case.dest());
    let media = &pages[0].0;
    assert!(
        (media[2] - 297.64).abs() < 0.01 && (media[3] - 419.53).abs() < 0.01,
        "{media:?}"
    );
    assert_eq!(pages[0].2, (1241, 1749));
}

#[test]
fn create_pdf_takes_an_svg_among_the_images() {
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&gradient(3, 5))
        .unwrap();
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="96" height="48">
        <image href="secret.png" width="4" height="4"/><rect width="96" height="48"/></svg>"#;
    let case = planned(
        &[("a.svg", svg.to_vec()), ("b.jpg", jpeg)],
        Operation::tool(ToolKey::CreatePdf),
    );
    let ran = case.run(fake("real"));
    assert!(
        ran.progress.contains(&0.25) && ran.progress.contains(&0.5),
        "{:?}",
        ran.progress
    );
    let outcome = case.done(ran);
    assert_eq!(outcome.notes, vec![Note::ImagesLeftOut(1)]);
    let pages = pdf_pages(&case.dest());
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].0, vec![0.0, 0.0, 72.0, 36.0]);
    assert_eq!(pages[0].1, "FlateDecode");
    assert_eq!(pages[1].0, vec![0.0, 0.0, 3.0, 5.0]);
    assert_eq!(pages[1].1, "DCTDecode");
}

#[test]
fn svg_pages_below_300_dpi_are_counted() {
    // 5300 CSS px wide is 16,563 px at 300 DPI: over 16,384.
    let wide = br#"<svg xmlns="http://www.w3.org/2000/svg" width="5300" height="10"><rect width="5300" height="10"/></svg>"#;
    let case = planned(
        &[("a.svg", wide.to_vec()), ("b.svg", wide.to_vec())],
        Operation::tool(ToolKey::CreatePdf),
    );
    let outcome = case.done(case.run(fake("real")));
    assert_eq!(outcome.notes, vec![Note::PagesScaled(2)]);
    let pages = pdf_pages(&case.dest());
    assert_eq!(pages[0].0, vec![0.0, 0.0, 3975.0, 7.5]);
    assert_eq!(pages[0].2 .0, 16_384);
}

#[test]
fn the_pages_size_is_held_like_a_pictures() {
    assert_eq!(create::svg_page((96.0, 48.0)), (72.0, 36.0));
    // 14,400 pt at most, scaled down evenly; 1 pt at least.
    assert_eq!(create::svg_page((40_000.0, 20_000.0)), (14_400.0, 7_200.0));
    assert_eq!(create::svg_page((40_000.0, 1.0)), (14_400.0, 1.0));
    assert_eq!(create::svg_page((0.5, 0.5)), (1.0, 1.0));
}

#[test]
fn the_workers_failures_in_grapes_words() {
    let svg = halves(10, 10);
    for (mode, kind, message) in [
        (
            "error-notSvg",
            ErrorKind::DecodeFailed,
            "logo.svg isn't a readable SVG image.",
        ),
        (
            "error-tooLarge",
            ErrorKind::DecodeFailed,
            "logo.svg is too large to convert.",
        ),
        (
            "error-read",
            ErrorKind::ReadFailed,
            "Grape couldn't read logo.svg.",
        ),
        (
            "error-write",
            ErrorKind::WriteFailed,
            "Grape couldn't write its temp files.",
        ),
        (
            "error-protocol",
            ErrorKind::DecodeFailed,
            "Grape's SVG helper stopped unexpectedly.",
        ),
        (
            "error-internal",
            ErrorKind::DecodeFailed,
            "Grape's SVG helper stopped unexpectedly.",
        ),
        (
            "crash",
            ErrorKind::DecodeFailed,
            "Grape's SVG helper stopped unexpectedly.",
        ),
        (
            "lie",
            ErrorKind::DecodeFailed,
            "Grape's SVG helper stopped unexpectedly.",
        ),
        (
            "mismatch",
            ErrorKind::DecodeFailed,
            "Grape's SVG helper stopped unexpectedly.",
        ),
        (
            "over-caps",
            ErrorKind::DecodeFailed,
            "Grape's SVG helper stopped unexpectedly.",
        ),
        (
            "bad-pixels",
            ErrorKind::DecodeFailed,
            "Grape's SVG helper stopped unexpectedly.",
        ),
        (
            "over-heap",
            ErrorKind::DecodeFailed,
            "logo.svg needs too much memory, so Grape stopped.",
        ),
    ] {
        let case = one("logo.svg", &svg, Format::Png);
        let err = case.refused(case.run(fake(mode)), kind, message);
        assert!(!err.detail.contains("logo"), "{mode}: {}", err.detail);
    }
    // A real worker on a broken SVG.
    let case = one(
        "logo.svg",
        b"<svg xmlns='http://www.w3.org/2000/svg'><g>",
        Format::Png,
    );
    case.refused(
        case.run(fake("real")),
        ErrorKind::DecodeFailed,
        "logo.svg isn't a readable SVG image.",
    );
}

#[test]
fn noise_is_skipped_and_progress_goes_forward() {
    let case = one("flag.svg", &halves(300, 200), Format::Png);
    let ran = case.run(fake("noisy"));
    assert!(
        ran.progress.iter().any(|p| (p - 0.46).abs() < 1e-6),
        "{:?}",
        ran.progress
    );
    case.done(ran);
}

#[test]
fn a_cancel_or_the_deadline_kills_the_worker() {
    let case = one("logo.svg", &halves(10, 10), Format::Png);
    let ran = case.run_with(
        fake("slow"),
        case.limits(),
        CancelAt::After(Duration::from_millis(500)),
    );
    let returned = Instant::now();
    let at = ran.cancelled_at.unwrap();
    case.refused(ran, ErrorKind::Cancelled, "Cancelled. Nothing was saved.");
    assert!(
        returned - at < Duration::from_secs(2),
        "{:?}",
        returned - at
    );

    // 1 s, and 1 s more for the 16 pixels' started megapixel.
    let limits = Limits {
        timeout_base: Duration::from_secs(1),
        ..case.limits()
    };
    let started = Instant::now();
    let ran = case.run_with(fake("slow"), limits, CancelAt::Never);
    let took = started.elapsed();
    case.refused(
        ran,
        ErrorKind::DecodeFailed,
        "logo.svg took too long, so Grape stopped.",
    );
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(10),
        "{took:?}"
    );
}

/// Contains nothing; can refuse, and reports a peak.
#[derive(Debug)]
struct FakeContain {
    fail: bool,
    peak: Option<u64>,
    max_memory: Mutex<Option<Option<u64>>>,
}

struct FakeContained(Option<u64>);

impl Contained for FakeContained {
    fn peak_memory(&self) -> Option<u64> {
        self.0
    }
}

impl Contain for FakeContain {
    fn contain(&self, _: &Child, max_memory: Option<u64>) -> Result<Box<dyn Contained>, String> {
        *self.max_memory.lock().unwrap() = Some(max_memory);
        if self.fail {
            return Err("fake: no container".to_string());
        }
        Ok(Box::new(FakeContained(self.peak)))
    }
}

#[test]
fn the_container_caps_the_worker_and_its_peak_is_read() {
    let contained = |mode: &str, hook: &Arc<FakeContain>| Launch {
        contain: Some(Arc::clone(hook) as Arc<dyn Contain>),
        ..fake(mode)
    };
    let hook = Arc::new(FakeContain {
        fail: false,
        peak: None,
        max_memory: Mutex::new(None),
    });
    let case = one("flag.svg", &halves(30, 20), Format::Png);
    case.done(case.run(contained("real", &hook)));
    assert_eq!(*hook.max_memory.lock().unwrap(), Some(Some(2560 << 20)));

    let refusing = Arc::new(FakeContain {
        fail: true,
        peak: None,
        max_memory: Mutex::new(None),
    });
    let case = one("flag.svg", &halves(30, 20), Format::Png);
    case.refused(
        case.run(contained("real", &refusing)),
        ErrorKind::NotYetSupported,
        "Grape's SVG helper couldn't start.",
    );

    // A crash near the container's cap needed too much memory.
    let full = Arc::new(FakeContain {
        fail: false,
        peak: Some(2400 << 20),
        max_memory: Mutex::new(None),
    });
    let case = one("flag.svg", &halves(30, 20), Format::Png);
    case.refused(
        case.run(contained("crash", &full)),
        ErrorKind::DecodeFailed,
        "flag.svg needs too much memory, so Grape stopped.",
    );
}

#[test]
fn a_cancelled_job_starts_nothing() {
    let case = one("logo.svg", &halves(10, 10), Format::Png);
    let ctx = EngineCtx::default();
    ctx.cancel.cancel();
    let renderer = Renderer::new(nowhere(), case.limits());
    let err = convert_with(
        &case.job,
        &case.paths[0],
        &case.dest(),
        Format::Png,
        &ctx,
        &mut |_| {},
        &renderer,
        None,
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    case.check();
    let token = CancelToken::new();
    token.cancel();
    let names = crate::publish::Names::new(&case.paths[0], &case.dest(), Format::Svg, Format::Png);
    let err = renderer
        .render(
            &case.paths[0],
            &names,
            Mode::Raster,
            Format::Png,
            &token,
            (0.0, 1.0),
            &mut |_| {},
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    case.check();
}

#[test]
fn the_dispatch_needs_the_worker() {
    let worker = SidecarPaths {
        svg_worker: Some(PathBuf::from("grape")),
        ..SidecarPaths::default()
    };
    let both = SidecarPaths {
        heif: Some(PathBuf::from("grape-heif")),
        ..worker.clone()
    };
    let runs = |to: Format, sidecars: &SidecarPaths| {
        crate::supports_with(&Operation::convert(to), &[Some(Format::Svg)], sidecars)
    };
    for to in image::RASTER.into_iter().chain([Format::Pdf]) {
        assert!(runs(to, &worker), "{to:?}");
        assert!(!runs(to, &SidecarPaths::default()), "{to:?}");
        assert!(!crate::supports(&Operation::convert(to), Some(Format::Svg)));
    }
    for to in [Format::Heic, Format::Avif] {
        assert!(runs(to, &both), "{to:?}");
        assert!(!runs(to, &worker), "{to:?}");
    }
    // Every target the matrix gives SVG, and nothing else.
    let targets: Vec<Format> = Format::ALL
        .into_iter()
        .filter(|&to| runs(to, &both))
        .collect();
    let mut expected = grape_core::targets(Format::Svg).to_vec();
    expected.sort_by_key(|f| f.key());
    let mut found = targets;
    found.sort_by_key(|f| f.key());
    assert_eq!(found, expected);
    // Create PDF takes an SVG with the worker.
    let create = Operation::tool(ToolKey::CreatePdf);
    assert!(crate::supports_with(
        &create,
        &[Some(Format::Svg), Some(Format::Jpg)],
        &worker
    ));
    assert!(!crate::supports_with(
        &create,
        &[Some(Format::Svg), Some(Format::Jpg)],
        &SidecarPaths::default()
    ));
    // Without a worker in the sidecars, a direct call says so.
    let case = one("logo.svg", ICON, Format::Png);
    let err = convert(
        &case.job,
        &case.paths[0],
        &case.dest(),
        Format::Png,
        &EngineCtx::default(),
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(err.user_message, "Grape's SVG helper is missing.");
    case.check();
}
