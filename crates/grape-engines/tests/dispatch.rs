//! Which conversions this build runs: [`supports`] and [`run_job`] decide
//! from the same table, so a pair is offered exactly when it can run.
#![allow(clippy::unwrap_used)]

mod common;

use std::path::PathBuf;

use common::{plan_one, SourceState, TestDir};
use grape_core::{pairs, targets, Category, Features, Format, Operation, ToolKey};
use grape_engines::{run_job, supports, supports_with, EngineCtx, ErrorKind, SidecarPaths};

const RASTER: [Format; 5] = [
    Format::Jpg,
    Format::Png,
    Format::Webp,
    Format::Tiff,
    Format::Bmp,
];

#[test]
fn supports_the_raster_and_subtitle_pairs() {
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
    assert_eq!(supported.len(), 24);
    for pair @ (from, to) in &supported {
        assert!(
            (RASTER.contains(from) && RASTER.contains(to) && from != to)
                || subtitles.contains(pair),
            "{pair:?}"
        );
    }
    for pair in subtitles {
        assert!(supported.contains(&pair), "{pair:?}");
    }
    let full = pairs(Features::FULL)
        .into_iter()
        .filter(|&(from, to)| supports(&Operation::convert(to), Some(from)))
        .count();
    assert_eq!(full, 24);
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

/// Sidecar paths that are set but point at nothing.
fn absent_sidecars(dir: &TestDir) -> SidecarPaths {
    let missing = |name: &str| -> PathBuf {
        let path = dir.join(name);
        std::path::absolute(path).unwrap()
    };
    SidecarPaths {
        ffmpeg: Some(missing("no-ffmpeg")),
        ffprobe: Some(missing("no-ffprobe")),
        heif_enc: None,
    }
}

#[test]
fn audio_and_video_run_only_with_both_ffmpeg_sidecars() {
    let dir = TestDir::new("dispatch-media");
    let both = absent_sidecars(&dir);
    let count = |sidecars: &SidecarPaths| {
        pairs(Features::V1_ONLY)
            .into_iter()
            .filter(|&(from, to)| supports_with(sidecars, &Operation::convert(to), Some(from)))
            .count()
    };
    assert_eq!(count(&SidecarPaths::default()), 24);
    assert_eq!(count(&both), 24 + 104);
    let only_ffmpeg = SidecarPaths {
        ffprobe: None,
        ..both.clone()
    };
    assert_eq!(count(&only_ffmpeg), 24);
    let only_ffprobe = SidecarPaths {
        ffmpeg: None,
        ..both.clone()
    };
    assert_eq!(count(&only_ffprobe), 24);
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
                supports_with(&both, &Operation::convert(to), Some(from)),
                "{from:?} to {to:?}"
            );
            assert!(
                !supports(&Operation::convert(to), Some(from)),
                "{from:?} to {to:?}"
            );
        }
    }
    assert!(!supports_with(
        &both,
        &Operation::convert(Format::Mp3),
        Some(Format::Mp3)
    ));
    assert!(!supports_with(
        &both,
        &Operation::tool(ToolKey::Compress),
        Some(Format::Mp4)
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
