//! Raster conversions end to end: plan with `grape_core`, run with
//! `run_job`, check the output and that the source never changed
//! (`docs/design/m1-drag-wheel.md` §E.4).
#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use common::{assert_no_leftovers, names_in, TestDir};
use grape_core::{
    pairs, plan, DirSnapshot, Features, Format, Job, JobOutput, Operation, Selection,
    SelectionItem, Settings, ToolKey,
};
use grape_engines::{build_selection, run_job, supports, EngineCtx, ErrorKind, JobOutcome};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{
    DynamicImage, GenericImageView, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Rgb,
    RgbImage, Rgba, RgbaImage,
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

/// The source's bytes and mtime, to prove it was never touched.
struct SourceState {
    path: PathBuf,
    bytes: Vec<u8>,
    modified: SystemTime,
}

impl SourceState {
    fn of(path: &Path) -> SourceState {
        SourceState {
            path: path.to_path_buf(),
            bytes: fs::read(path).unwrap(),
            modified: fs::metadata(path).unwrap().modified().unwrap(),
        }
    }

    fn assert_unchanged(&self) {
        assert_eq!(
            fs::read(&self.path).unwrap(),
            self.bytes,
            "source bytes changed: {}",
            self.path.display()
        );
        assert_eq!(
            fs::metadata(&self.path).unwrap().modified().unwrap(),
            self.modified,
            "source mtime changed: {}",
            self.path.display()
        );
    }
}

/// Writes `bytes` as `name` in `dir` and plans `op` for it the way the app
/// does (`build_selection`, outputs next to the source, fallback
/// `dir/fallback`). Returns the source and the job.
fn plan_one(dir: &TestDir, name: &str, bytes: &[u8], op: Operation) -> (PathBuf, Job) {
    let src = dir.join(name);
    fs::write(&src, bytes).unwrap();
    let settings = Settings::new(dir.join("fallback"));
    let sel = build_selection(std::slice::from_ref(&src), &settings);
    let mut plan = plan(&sel, &op, &settings).unwrap();
    assert_eq!(plan.jobs.len(), 1);
    (src, plan.jobs.remove(0))
}

fn planned_file(job: &Job) -> PathBuf {
    match &job.output {
        JobOutput::File(path) => path.clone(),
        other => panic!("expected a file output, got {other:?}"),
    }
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
                    renamed: false
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
            renamed: true
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
    let (_src, job) = plan_one(
        &dir,
        "photo.png",
        &encode(&gradient(), Format::Png),
        Operation::convert(Format::Webp),
    );
    fs::write(dir.join("PHOTO.webp"), b"not ours").unwrap();

    let outcome = run(&job).unwrap();

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
    let (_src, job) = plan_one(
        &dir,
        "logo.png",
        &encode(&img, Format::Png),
        Operation::convert(Format::Jpg),
    );
    let outcome = run(&job).unwrap();

    let out = decode(&fs::read(&outcome.outputs[0]).unwrap()).to_rgb8();
    let clear = out.get_pixel(4, 16).0;
    let solid = out.get_pixel(28, 16).0;
    assert!(
        clear.iter().all(|&c| c >= 245),
        "transparent area {clear:?}"
    );
    assert!(solid[1] > 150 && solid[0] < 60, "opaque area {solid:?}");
}

#[test]
fn icc_profiles_are_carried_over_where_the_target_can_hold_them() {
    // A minimal RGB profile header; the codecs embed it as opaque bytes.
    let mut profile = vec![0u8; 132];
    profile[..4].copy_from_slice(&132u32.to_be_bytes());
    profile[16..20].copy_from_slice(b"RGB ");
    profile[20..24].copy_from_slice(b"XYZ ");
    profile[36..40].copy_from_slice(b"acsp");
    let mut source = Vec::new();
    let mut encoder = PngEncoder::new(&mut source);
    encoder.set_icc_profile(profile.clone()).unwrap();
    gradient().write_with_encoder(encoder).unwrap();

    for to in [Format::Jpg, Format::Webp, Format::Tiff, Format::Bmp] {
        let dir = TestDir::new("icc");
        let (_src, job) = plan_one(&dir, "photo.png", &source, Operation::convert(to));
        let outcome = run(&job).unwrap();
        let bytes = fs::read(&outcome.outputs[0]).unwrap();
        let mut decoder = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        let carried = decoder.icc_profile().unwrap();
        if to == Format::Bmp {
            assert_eq!(carried, None, "BMP can't hold a profile");
        } else {
            assert_eq!(carried.as_deref(), Some(&profile[..]), "{to:?}");
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
        (Format::Pdf, "PNG to PDF isn't in this build yet."),
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
fn supports_exactly_the_twenty_raster_pairs() {
    let supported: Vec<(Format, Format)> = pairs(Features::V1_ONLY)
        .into_iter()
        .filter(|&(from, to)| supports(&Operation::convert(to), Some(from)))
        .collect();
    assert_eq!(supported.len(), 20);
    for (from, to) in &supported {
        assert!(RASTER.contains(from) && RASTER.contains(to) && from != to);
    }
    let full = pairs(Features::FULL)
        .into_iter()
        .filter(|&(from, to)| supports(&Operation::convert(to), Some(from)))
        .count();
    assert_eq!(full, 20);
    assert!(!supports(
        &Operation::convert(Format::Png),
        Some(Format::Png)
    ));
    assert!(!supports(&Operation::convert(Format::Png), None));
    assert!(!supports(
        &Operation::convert(Format::Png),
        Some(Format::Heic)
    ));
    assert!(!supports(
        &Operation::tool(ToolKey::Compress),
        Some(Format::Png)
    ));
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
    struct Unlock(PathBuf);
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
