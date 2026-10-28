//! Which conversions this build runs: [`supports`] and [`run_job`] decide
//! from the same table, so a pair is offered exactly when it can run.
//! [`supports`] assumes no sidecars; the PDF, RAR, HEIC, AVIF, SVG, audio
//! and video pairs need theirs ([`supports_with`]).
#![allow(clippy::unwrap_used)]

mod common;

use std::path::PathBuf;

use common::{plan_one, SourceState, TestDir};
use grape_core::{pairs, targets, tools_enabled, Category, Features, Format, Operation, ToolKey};
use grape_engines::{run_job, supports, supports_with, EngineCtx, ErrorKind, SidecarPaths};

const RASTER: [Format; 5] = [
    Format::Jpg,
    Format::Png,
    Format::Webp,
    Format::Tiff,
    Format::Bmp,
];

#[test]
fn supports_the_raster_subtitle_and_archive_pairs() {
    let supported: Vec<(Format, Format)> = pairs(Features::V1_ONLY)
        .into_iter()
        .filter(|&(from, to)| supports(&Operation::convert(to), Some(from)))
        .collect();
    let subtitles = [
        (Format::Srt, Format::Vtt),
        (Format::Srt, Format::Txt),
        (Format::Vtt, Format::Srt),
        (Format::Vtt, Format::Txt),
    ];
    let archives = [
        (Format::Zip, Format::Tar),
        (Format::Zip, Format::Gz),
        (Format::Tar, Format::Zip),
        (Format::Tar, Format::Gz),
        (Format::Gz, Format::Zip),
        (Format::Gz, Format::Tar),
    ];
    assert_eq!(supported.len(), 35);
    for pair @ (from, to) in &supported {
        assert!(
            (RASTER.contains(from) && RASTER.contains(to) && from != to)
                || (RASTER.contains(from) && *to == Format::Pdf)
                || subtitles.contains(pair)
                || archives.contains(pair),
            "{pair:?}"
        );
    }
    for pair in subtitles.into_iter().chain(archives) {
        assert!(supported.contains(&pair), "{pair:?}");
    }
    let full = pairs(Features::FULL)
        .into_iter()
        .filter(|&(from, to)| supports(&Operation::convert(to), Some(from)))
        .count();
    assert_eq!(full, 35);
    assert!(!supports(
        &Operation::convert(Format::Png),
        Some(Format::Png)
    ));
    assert!(!supports(&Operation::convert(Format::Png), None));
    assert!(!supports(
        &Operation::convert(Format::Png),
        Some(Format::Heic)
    ));
    // Compress runs on PNG with no sidecar (D26), not yet on TIFF.
    assert!(supports(
        &Operation::tool(ToolKey::Compress),
        Some(Format::Png)
    ));
    assert!(!supports(
        &Operation::tool(ToolKey::Compress),
        Some(Format::Tiff)
    ));
}

/// Every sidecar a full install has but the image helper (which ships on
/// Windows only); any path will do, nothing runs here.
fn all_sidecars() -> SidecarPaths {
    let some = || Some(PathBuf::from("x"));
    SidecarPaths {
        ffmpeg: some(),
        ffprobe: some(),
        pdfium: some(),
        pdf_worker: some(),
        bsdtar: some(),
        svg_worker: some(),
        ..SidecarPaths::default()
    }
}

/// The v1 SVG pairs that run with the SVG worker alone: every target the
/// matrix gives SVG but HEIC and AVIF, which need the image helper too
/// (D39).
fn svg_pairs() -> usize {
    targets(Format::Svg)
        .iter()
        .filter(|&&to| !matches!(to, Format::Heic | Format::Avif))
        .count()
}

/// The `(from, to)` pairs over every format that run with `sidecars`.
fn runnable_pairs(sidecars: &SidecarPaths) -> Vec<(Format, Format)> {
    Format::ALL
        .into_iter()
        .flat_map(|from| Format::ALL.into_iter().map(move |to| (from, to)))
        .filter(|&(from, to)| supports_with(&Operation::convert(to), &[Some(from)], sidecars))
        .collect()
}

/// The `(tool, format)` pairs over every tool and format that run with
/// `sidecars`.
fn runnable_tools(sidecars: &SidecarPaths) -> Vec<(ToolKey, Format)> {
    ToolKey::ALL
        .into_iter()
        .flat_map(|tool| Format::ALL.into_iter().map(move |format| (tool, format)))
        .filter(|&(tool, format)| supports_with(&Operation::tool(tool), &[Some(format)], sidecars))
        .collect()
}

#[test]
fn pdf_and_rar_pairs_need_their_sidecars() {
    let all = all_sidecars();
    let with: Vec<(Format, Format)> = pairs(Features::V1_ONLY)
        .into_iter()
        .filter(|&(from, to)| supports_with(&Operation::convert(to), &[Some(from)], &all))
        .collect();
    assert_eq!(with.len(), 42 + svg_pairs() + 104);
    for to in [Format::Jpg, Format::Png, Format::Txt, Format::Md] {
        assert!(with.contains(&(Format::Pdf, to)), "{to:?}");
        assert!(
            !supports(&Operation::convert(to), Some(Format::Pdf)),
            "{to:?}"
        );
    }
    for to in [Format::Zip, Format::Tar, Format::Gz] {
        assert!(with.contains(&(Format::Rar, to)), "{to:?}");
        assert!(
            !supports(&Operation::convert(to), Some(Format::Rar)),
            "{to:?}"
        );
    }
    assert!(!supports_with(&Operation::convert(Format::Png), &[], &all));
}

#[test]
fn nothing_runs_outside_the_v1_matrix_and_tools() {
    let all = all_sidecars();
    // Over every pair of formats, only the 42 + SVG's + 104 v1 pairs run.
    let v1 = pairs(Features::V1_ONLY);
    let runnable = runnable_pairs(&all);
    assert_eq!(runnable.len(), 42 + svg_pairs() + 104);
    for pair in &runnable {
        assert!(v1.contains(pair), "{pair:?} isn't a v1 pair");
    }
    // Over every tool and format, exactly these seven, Create PDF on four
    // image formats and SVG, and the D26 tools run, each offered on the
    // wheel for one item or for several.
    let tools = runnable_tools(&all);
    assert_eq!(
        tools.len(),
        7 + 4 + 1 + MEDIA_TOOLS.len() + IMAGE_TOOLS.len(),
        "{tools:?}"
    );
    assert!(tools.contains(&(ToolKey::CreatePdf, Format::Svg)));
    for pair in [
        (ToolKey::ExtractArchive, Format::Zip),
        (ToolKey::ExtractArchive, Format::Tar),
        (ToolKey::ExtractArchive, Format::Gz),
        (ToolKey::ExtractArchive, Format::Rar),
        (ToolKey::SplitPdf, Format::Pdf),
        (ToolKey::MergePdf, Format::Pdf),
        (ToolKey::RemoveMetadata, Format::Pdf),
    ]
    .into_iter()
    .chain(MEDIA_TOOLS)
    .chain(IMAGE_TOOLS)
    {
        assert!(tools.contains(&pair), "{pair:?}");
    }
    for &(tool, format) in &tools {
        assert!(
            tools_enabled(format, 1, Features::V1_ONLY).contains(&tool)
                || tools_enabled(format, 2, Features::V1_ONLY).contains(&tool),
            "{tool:?} on {format:?} isn't offered in v1"
        );
    }
    // PDF needs both PDFium and the worker.
    for without in [
        SidecarPaths {
            pdfium: None,
            ..all.clone()
        },
        SidecarPaths {
            pdf_worker: None,
            ..all.clone()
        },
    ] {
        assert!(
            runnable_pairs(&without)
                .iter()
                .all(|&(from, _)| from != Format::Pdf),
            "{without:?}"
        );
        assert!(
            runnable_tools(&without)
                .iter()
                .all(|&(_, format)| format != Format::Pdf),
            "{without:?}"
        );
        assert_eq!(
            runnable_pairs(&without).len(),
            38 + svg_pairs() + 104,
            "{without:?}"
        );
    }
    // RAR needs bsdtar.
    let no_bsdtar = SidecarPaths {
        bsdtar: None,
        ..all.clone()
    };
    assert!(runnable_pairs(&no_bsdtar)
        .iter()
        .all(|&(from, _)| from != Format::Rar));
    assert!(!runnable_tools(&no_bsdtar).contains(&(ToolKey::ExtractArchive, Format::Rar)));
    // SVG needs its worker.
    let no_svg_worker = SidecarPaths {
        svg_worker: None,
        ..all.clone()
    };
    assert!(runnable_pairs(&no_svg_worker)
        .iter()
        .all(|&(from, _)| from != Format::Svg));
    assert!(!runnable_tools(&no_svg_worker).contains(&(ToolKey::CreatePdf, Format::Svg)));
}

#[test]
fn every_v1_pair_runs_with_every_sidecar() {
    // With the image helper too, the matrix's v1 pairs are exactly what
    // runs: nothing is missing, nothing extra.
    let every = SidecarPaths {
        heif: Some(PathBuf::from("x")),
        ..all_sidecars()
    };
    let mut v1 = pairs(Features::V1_ONLY);
    let mut runnable = runnable_pairs(&every);
    v1.sort_by_key(|&(from, to)| (from.key(), to.key()));
    runnable.sort_by_key(|&(from, to)| (from.key(), to.key()));
    assert_eq!(runnable, v1);
    // SVG's every target among them, HEIC and AVIF with both helpers.
    for &to in targets(Format::Svg) {
        assert!(runnable.contains(&(Format::Svg, to)), "{to:?}");
    }
    for to in [Format::Heic, Format::Avif] {
        assert!(!runnable_pairs(&all_sidecars()).contains(&(Format::Svg, to)));
    }
}

/// Grape's own image tools (D26, D28), which need no sidecar.
const IMAGE_TOOLS: [(ToolKey, Format); 7] = [
    (ToolKey::Compress, Format::Jpg),
    (ToolKey::Compress, Format::Png),
    (ToolKey::RemoveMetadata, Format::Jpg),
    (ToolKey::RemoveMetadata, Format::Png),
    (ToolKey::RemoveMetadata, Format::Webp),
    (ToolKey::RemoveMetadata, Format::Gif),
    (ToolKey::RemoveMetadata, Format::Tiff),
];

/// The tools that run with `ffmpeg` and `ffprobe` (D26).
const MEDIA_TOOLS: [(ToolKey, Format); 25] = [
    (ToolKey::Compress, Format::Mp4),
    (ToolKey::Compress, Format::Mov),
    (ToolKey::Compress, Format::Mkv),
    (ToolKey::Compress, Format::Mp3),
    (ToolKey::Compress, Format::M4a),
    (ToolKey::Compress, Format::Wav),
    (ToolKey::Compress, Format::Flac),
    (ToolKey::MuteVideo, Format::Mp4),
    (ToolKey::MuteVideo, Format::Mov),
    (ToolKey::MuteVideo, Format::Mkv),
    (ToolKey::RemoveMetadata, Format::Mp3),
    (ToolKey::RemoveMetadata, Format::M4a),
    (ToolKey::RemoveMetadata, Format::Wav),
    (ToolKey::RemoveMetadata, Format::Flac),
    (ToolKey::RemoveMetadata, Format::Mp4),
    (ToolKey::RemoveMetadata, Format::Mov),
    (ToolKey::RemoveMetadata, Format::Mkv),
    (ToolKey::AudioChannels, Format::Mp3),
    (ToolKey::AudioChannels, Format::M4a),
    (ToolKey::AudioChannels, Format::Wav),
    (ToolKey::AudioChannels, Format::Flac),
    (ToolKey::NormalizeAudio, Format::Mp3),
    (ToolKey::NormalizeAudio, Format::M4a),
    (ToolKey::NormalizeAudio, Format::Wav),
    (ToolKey::NormalizeAudio, Format::Flac),
];

#[test]
fn media_tools_run_only_with_both_ffmpeg_sidecars() {
    let all = all_sidecars();
    for (without, name) in [
        (
            SidecarPaths {
                ffmpeg: None,
                ..all.clone()
            },
            "ffmpeg",
        ),
        (
            SidecarPaths {
                ffprobe: None,
                ..all.clone()
            },
            "ffprobe",
        ),
    ] {
        let tools = runnable_tools(&without);
        // The seven, Create PDF on four image formats and SVG, the image
        // tools.
        assert_eq!(
            tools.len(),
            7 + 4 + 1 + IMAGE_TOOLS.len(),
            "without {name}: {tools:?}"
        );
        for pair in MEDIA_TOOLS {
            assert!(!tools.contains(&pair), "{pair:?} without {name}");
        }
    }
    // Nothing but the archive tools, Create PDF on four image formats and
    // the image tools runs with no sidecar at all.
    let bare = runnable_tools(&SidecarPaths::default());
    assert_eq!(bare.len(), 3 + 4 + IMAGE_TOOLS.len(), "{bare:?}");
    for pair in IMAGE_TOOLS {
        assert!(bare.contains(&pair), "{pair:?}");
        assert!(supports(&Operation::tool(pair.0), Some(pair.1)));
    }
    for (tool, format) in MEDIA_TOOLS {
        assert!(!supports(&Operation::tool(tool), Some(format)));
    }
}

/// Sidecar paths that are set but point at nothing.
fn absent_sidecars(dir: &TestDir) -> SidecarPaths {
    let missing = |name: &str| -> PathBuf {
        let path = dir.join(name);
        std::path::absolute(path).unwrap()
    };
    SidecarPaths {
        ffmpeg: Some(missing("no-ffmpeg")),
        ffprobe: Some(missing("no-ffprobe")),
        svg_worker: Some(missing("no-svg-worker")),
        ..SidecarPaths::default()
    }
}

#[test]
fn audio_and_video_run_only_with_both_ffmpeg_sidecars() {
    let dir = TestDir::new("dispatch-media");
    let both = absent_sidecars(&dir);
    let count = |sidecars: &SidecarPaths| {
        pairs(Features::V1_ONLY)
            .into_iter()
            .filter(|&(from, to)| supports_with(&Operation::convert(to), &[Some(from)], sidecars))
            .count()
    };
    // The 35 that need no sidecar: raster, images → PDF, subtitle and
    // archive pairs. `both` has the SVG worker too.
    assert_eq!(count(&SidecarPaths::default()), 35);
    assert_eq!(count(&both), 35 + svg_pairs() + 104);
    let only_ffmpeg = SidecarPaths {
        ffprobe: None,
        ..both.clone()
    };
    assert_eq!(count(&only_ffmpeg), 35 + svg_pairs());
    let only_ffprobe = SidecarPaths {
        ffmpeg: None,
        ..both.clone()
    };
    assert_eq!(count(&only_ffprobe), 35 + svg_pairs());
    // Every media pair in the matrix, and nothing else, is added.
    for from in [
        Format::Mp3,
        Format::M4a,
        Format::Wav,
        Format::Flac,
        Format::Ogg,
        Format::Opus,
        Format::Aiff,
        Format::Wma,
        Format::Mp4,
        Format::Mov,
        Format::Mkv,
        Format::Webm,
        Format::Avi,
        Format::Wmv,
        Format::Gif,
    ] {
        assert!(matches!(from.category(), Category::Audio | Category::Video));
        for &to in targets(from) {
            assert!(
                supports_with(&Operation::convert(to), &[Some(from)], &both),
                "{from:?} to {to:?}"
            );
            assert!(
                !supports(&Operation::convert(to), Some(from)),
                "{from:?} to {to:?}"
            );
        }
    }
    assert!(!supports_with(
        &Operation::convert(Format::Mp3),
        &[Some(Format::Mp3)],
        &both
    ));
    assert!(!supports_with(
        &Operation::tool(ToolKey::Compress),
        &[Some(Format::Webm)],
        &both
    ));
}

#[test]
fn a_media_job_without_its_sidecars_is_not_in_this_build() {
    let dir = TestDir::new("media-unwired");
    let (src, job) = plan_one(&dir, "song.wav", b"RIFF", Operation::convert(Format::Mp3));
    let before = SourceState::of(&src);
    let names_before = dir.names();

    let err = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap_err();

    assert_eq!(err.kind, ErrorKind::NotYetSupported);
    assert_eq!(err.user_message, "WAV to MP3 isn't in this build yet.");
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}

#[test]
fn an_svg_job_whose_worker_is_gone_fails_cleanly() {
    let dir = TestDir::new("svg-missing");
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='4'/>";
    for to in [Format::Png, Format::Pdf] {
        let (src, job) = plan_one(&dir, "logo.svg", svg, Operation::convert(to));
        let before = SourceState::of(&src);
        let names_before = dir.names();
        let ctx = EngineCtx {
            sidecars: absent_sidecars(&dir),
            ..EngineCtx::default()
        };

        let err = run_job(&job, &ctx, &mut |_| {}).unwrap_err();

        assert_eq!(err.kind, ErrorKind::NotYetSupported, "{to:?}");
        assert_eq!(err.user_message, "Grape's SVG helper couldn't start.");
        assert_eq!(dir.names(), names_before);
        before.assert_unchanged();
        std::fs::remove_file(&src).unwrap();
    }
    // Without the worker, SVG isn't in this build.
    let (src, job) = plan_one(&dir, "logo.svg", svg, Operation::convert(Format::Png));
    let err = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported);
    assert_eq!(err.user_message, "SVG to PNG isn't in this build yet.");
    std::fs::remove_file(&src).unwrap();
}

#[test]
fn a_media_job_whose_sidecar_file_is_gone_fails_cleanly() {
    let dir = TestDir::new("media-missing");
    let (src, job) = plan_one(
        &dir,
        "clip.mp4",
        b"not really",
        Operation::convert(Format::Mov),
    );
    let before = SourceState::of(&src);
    let names_before = dir.names();
    let ctx = EngineCtx {
        sidecars: absent_sidecars(&dir),
        ..EngineCtx::default()
    };

    let err = run_job(&job, &ctx, &mut |_| {}).unwrap_err();

    assert_eq!(err.kind, ErrorKind::EncodeFailed);
    assert_eq!(
        err.user_message,
        "Grape's media converter is missing. Reinstall Grape."
    );
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}
