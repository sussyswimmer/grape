//! Audio and video conversions end to end, through a real FFmpeg: plan with
//! `grape_core`, run with `run_job`, check the output with `ffprobe` and
//! that the source never changed. Needs `--features engines-it` and
//! `GRAPE_TEST_FFMPEG` / `GRAPE_TEST_FFPROBE` set to absolute paths (CI
//! uses the pinned sidecars); the tests fail, not skip, without them.
#![cfg(feature = "engines-it")]
#![allow(clippy::unwrap_used)]

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{assert_no_leftovers, plan_one, planned_file, SourceState, TestDir};
use grape_core::{targets, Category, Format, Operation};
use grape_engines::{run_job, EngineCtx, ErrorKind, GrapeError, JobOutcome, SidecarPaths};

/// The sidecar named by `var`, which must be set to an absolute path.
fn tool(var: &str) -> PathBuf {
    let path = std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("set {var} to the absolute path of the FFmpeg sidecar"));
    assert!(
        path.is_absolute(),
        "{var} must be absolute: {}",
        path.display()
    );
    path
}

fn sidecars() -> SidecarPaths {
    SidecarPaths {
        ffmpeg: Some(tool("GRAPE_TEST_FFMPEG")),
        ffprobe: Some(tool("GRAPE_TEST_FFPROBE")),
        heif_enc: None,
    }
}

fn ctx() -> EngineCtx {
    EngineCtx {
        sidecars: sidecars(),
        ..EngineCtx::default()
    }
}

/// Runs the test FFmpeg with `args`; panics with its stderr if it fails.
fn ffmpeg(args: &[&str]) {
    let out = Command::new(tool("GRAPE_TEST_FFMPEG"))
        .args(["-nostdin", "-hide_banner", "-v", "error", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `ffprobe`'s view of `path` as flat key=value pairs (quotes removed).
fn probe(path: &Path) -> HashMap<String, String> {
    let out = Command::new(tool("GRAPE_TEST_FFPROBE"))
        .args(["-v", "error", "-show_entries"])
        .arg("format=format_name,duration:stream=index,codec_type,codec_name,width,height,channels,sample_rate:stream_side_data=rotation")
        .args(["-of", "flat"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffprobe {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.trim_matches('"').to_string()))
        .collect()
}

/// The key prefix of the first stream of `kind` ("video" or "audio").
fn stream(probe: &HashMap<String, String>, kind: &str) -> Option<String> {
    (0..8)
        .map(|i| format!("streams.stream.{i}."))
        .find(|prefix| {
            probe
                .get(&format!("{prefix}codec_type"))
                .map(String::as_str)
                == Some(kind)
        })
}

fn field(probe: &HashMap<String, String>, kind: &str, name: &str) -> Option<String> {
    let prefix = stream(probe, kind)?;
    probe.get(&format!("{prefix}{name}")).cloned()
}

/// The demuxer `ffprobe` names for a file of each target format.
fn demuxer(f: Format) -> &'static str {
    match f {
        Format::Mp4 | Format::Mov | Format::M4a => "mov,mp4,m4a,3gp,3g2,mj2",
        Format::Mkv | Format::Webm => "matroska,webm",
        Format::Avi => "avi",
        Format::Wmv | Format::Wma => "asf",
        Format::Gif => "gif",
        Format::Mp3 => "mp3",
        Format::Wav => "wav",
        Format::Flac => "flac",
        Format::Ogg | Format::Opus => "ogg",
        Format::Aiff => "aiff",
        other => panic!("not a media format: {other:?}"),
    }
}

const SINE: &[&str] = &[
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:duration=1:sample_rate=44100",
];
const TESTSRC: &[&str] = &[
    "-f",
    "lavfi",
    "-i",
    "testsrc=size=160x120:rate=10:duration=1",
];

/// The encoder options that make a 1 s source of format `f`.
fn source_codecs(f: Format) -> &'static [&'static str] {
    match f {
        Format::Mp3 => &["-c:a", "libmp3lame", "-f", "mp3"],
        Format::M4a => &["-c:a", "aac", "-f", "ipod"],
        Format::Wav => &["-c:a", "pcm_s16le", "-f", "wav"],
        Format::Flac => &["-c:a", "flac", "-f", "flac"],
        Format::Ogg => &["-c:a", "libvorbis", "-f", "ogg"],
        Format::Opus => &["-c:a", "libopus", "-f", "opus"],
        Format::Aiff => &["-c:a", "pcm_s16be", "-f", "aiff"],
        Format::Wma => &["-c:a", "wmav2", "-f", "asf"],
        Format::Mp4 => &[
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-f", "mp4",
        ],
        Format::Mov => &[
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-f", "mov",
        ],
        Format::Mkv => &[
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-f", "matroska",
        ],
        Format::Webm => &["-c:v", "libvpx-vp9", "-c:a", "libopus", "-f", "webm"],
        Format::Avi => &["-c:v", "mpeg4", "-c:a", "libmp3lame", "-f", "avi"],
        Format::Wmv => &["-c:v", "wmv2", "-c:a", "wmav2", "-f", "asf"],
        Format::Gif => &["-c:v", "gif", "-f", "gif"],
        other => panic!("not a media format: {other:?}"),
    }
}

/// Writes a 1 s source of format `f` named `name` in `dir`.
fn make_source(dir: &Path, name: &str, f: Format) -> PathBuf {
    let path = dir.join(name);
    let mut args: Vec<&str> = Vec::new();
    match f.category() {
        Category::Audio => args.extend(SINE),
        _ if f == Format::Gif => args.extend(TESTSRC),
        _ => {
            args.extend(TESTSRC);
            args.extend(SINE);
            args.push("-shortest");
        }
    }
    args.extend(source_codecs(f));
    let path_str = path.to_str().unwrap();
    args.push(path_str);
    ffmpeg(&args);
    path
}

const MEDIA: [Format; 15] = [
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
];

/// Converts `src` (already in `dir`) to `to` and returns the outcome and the
/// progress values seen, checking that the source didn't change and nothing
/// was left behind.
fn convert_file(
    dir: &TestDir,
    src: &Path,
    to: Format,
) -> Result<(JobOutcome, Vec<f32>), GrapeError> {
    let settings = grape_core::Settings::new(dir.join("fallback"));
    let sel = grape_engines::build_selection(&[src.to_path_buf()], &settings);
    let job = grape_core::plan(&sel, &Operation::convert(to), &settings)
        .unwrap()
        .jobs
        .remove(0);
    let before = SourceState::of(src);
    let mut seen = Vec::new();
    let result = run_job(&job, &ctx(), &mut |f| seen.push(f));
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
    result.map(|outcome| (outcome, seen))
}

#[test]
fn every_audio_and_video_pair_converts() {
    let dir = TestDir::new("media-pairs");
    let mut failures = Vec::new();
    let mut count = 0;
    for from in MEDIA {
        let src = make_source(&dir.path, &format!("source.{}", from.key()), from);
        let source_duration: f64 = probe(&src)["format.duration"].parse().unwrap();
        for &to in targets(from) {
            count += 1;
            let outcome = match convert_file(&dir, &src, to) {
                Ok((outcome, seen)) => {
                    if seen.last() != Some(&1.0) || !seen.windows(2).all(|w| w[0] <= w[1]) {
                        failures.push(format!("{from:?} to {to:?}: progress {seen:?}"));
                    }
                    outcome
                }
                Err(e) => {
                    failures.push(format!("{from:?} to {to:?}: {e} ({})", e.detail));
                    continue;
                }
            };
            let out = &outcome.outputs[0];
            assert_eq!(out.extension().unwrap(), to.output_ext());
            let info = probe(out);
            let mut check = |ok: bool, what: String| {
                if !ok {
                    failures.push(format!("{from:?} to {to:?}: {what}"));
                }
            };
            check(
                info.get("format.format_name").map(String::as_str) == Some(demuxer(to)),
                format!("container {:?}", info.get("format.format_name")),
            );
            let duration: f64 = info
                .get("format.duration")
                .and_then(|d| d.parse().ok())
                .unwrap_or(0.0);
            check(
                (duration - source_duration).abs() <= (0.05 * source_duration).max(0.25),
                format!("duration {duration} vs {source_duration}"),
            );
            match to.category() {
                Category::Audio => {
                    check(
                        stream(&info, "video").is_none(),
                        "a video stream in audio".into(),
                    );
                    check(stream(&info, "audio").is_some(), "no audio".into());
                }
                _ => {
                    let width: u32 = field(&info, "video", "width").unwrap().parse().unwrap();
                    let height: u32 = field(&info, "video", "height").unwrap().parse().unwrap();
                    check(
                        width == 160 && height == 120,
                        format!("size {width}x{height}"),
                    );
                    let had_audio = from != Format::Gif;
                    if to != Format::Gif {
                        check(
                            stream(&info, "audio").is_some() == had_audio,
                            "audio kept or not".into(),
                        );
                    }
                }
            }
            match to {
                Format::Ogg => check(
                    field(&info, "audio", "codec_name").as_deref() == Some("vorbis"),
                    "ogg codec".into(),
                ),
                Format::Opus => check(
                    field(&info, "audio", "codec_name").as_deref() == Some("opus"),
                    "opus codec".into(),
                ),
                Format::Wma => check(
                    field(&info, "audio", "codec_name").as_deref() == Some("wmav2"),
                    "wma codec".into(),
                ),
                Format::Wmv => check(
                    field(&info, "video", "codec_name").as_deref() == Some("wmv2"),
                    "wmv codec".into(),
                ),
                Format::Webm => check(
                    field(&info, "video", "codec_name").as_deref() == Some("vp9"),
                    "webm codec".into(),
                ),
                _ => {}
            }
            fs::remove_file(out).unwrap();
        }
    }
    assert_eq!(count, 104);
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn odd_sizes_come_out_even_and_rotation_is_applied() {
    let dir = TestDir::new("media-geometry");
    // A 161×121 PNG-in-MOV source (any size encodes) to MP4.
    let odd = dir.join("odd.mov");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=161x121:rate=10:duration=1",
        "-c:v",
        "png",
        "-f",
        "mov",
        odd.to_str().unwrap(),
    ]);
    let (outcome, _) = convert_file(&dir, &odd, Format::Mp4).unwrap();
    let info = probe(&outcome.outputs[0]);
    assert_eq!(field(&info, "video", "width").as_deref(), Some("160"));
    assert_eq!(field(&info, "video", "height").as_deref(), Some("120"));

    // A phone-style rotated H.264 MP4 to MKV is re-encoded upright.
    let plain = dir.join("plain.mp4");
    ffmpeg(&[
        TESTSRC[0],
        TESTSRC[1],
        TESTSRC[2],
        TESTSRC[3],
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "mp4",
        plain.to_str().unwrap(),
    ]);
    let rotated = dir.join("rotated.mp4");
    ffmpeg(&[
        "-display_rotation:v:0",
        "90",
        "-i",
        plain.to_str().unwrap(),
        "-c",
        "copy",
        "-f",
        "mp4",
        rotated.to_str().unwrap(),
    ]);
    fs::remove_file(&plain).unwrap();
    let (outcome, _) = convert_file(&dir, &rotated, Format::Mkv).unwrap();
    let info = probe(&outcome.outputs[0]);
    assert_eq!(field(&info, "video", "width").as_deref(), Some("120"));
    assert_eq!(field(&info, "video", "height").as_deref(), Some("160"));
}

#[test]
fn audio_depth_channels_and_rate_follow_the_target() {
    let dir = TestDir::new("media-audio");
    // 24-bit, 96 kHz, 5.1 FLAC.
    let hi = dir.join("hires.flac");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=1:sample_rate=96000",
        "-af",
        "pan=5.1|c0=c0|c1=c0|c2=c0|c3=c0|c4=c0|c5=c0",
        "-c:a",
        "flac",
        "-sample_fmt",
        "s32",
        "-f",
        "flac",
        hi.to_str().unwrap(),
    ]);

    let (outcome, _) = convert_file(&dir, &hi, Format::Wav).unwrap();
    let info = probe(&outcome.outputs[0]);
    assert_eq!(
        field(&info, "audio", "codec_name").as_deref(),
        Some("pcm_s24le")
    );
    assert_eq!(field(&info, "audio", "channels").as_deref(), Some("6"));

    let (outcome, _) = convert_file(&dir, &hi, Format::Wma).unwrap();
    let info = probe(&outcome.outputs[0]);
    assert_eq!(field(&info, "audio", "channels").as_deref(), Some("2"));
    assert_eq!(
        field(&info, "audio", "sample_rate").as_deref(),
        Some("48000")
    );

    // A lossy source never becomes a 24-bit file.
    let mp3 = make_source(&dir.path, "song.mp3", Format::Mp3);
    let (outcome, _) = convert_file(&dir, &mp3, Format::Aiff).unwrap();
    let info = probe(&outcome.outputs[0]);
    assert_eq!(
        field(&info, "audio", "codec_name").as_deref(),
        Some("pcm_s16be")
    );
}

#[test]
fn a_source_without_the_needed_stream_is_refused_before_writing() {
    let dir = TestDir::new("media-missing-stream");
    let silent = dir.join("silent.mp4");
    ffmpeg(&[
        TESTSRC[0],
        TESTSRC[1],
        TESTSRC[2],
        TESTSRC[3],
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "mp4",
        silent.to_str().unwrap(),
    ]);
    let names_before = dir.names();
    let err = convert_file(&dir, &silent, Format::Mp3).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DecodeFailed);
    assert_eq!(err.user_message, "silent.mp4 has no sound to save as MP3.");
    assert_eq!(dir.names(), names_before);

    let audio_only = dir.join("voice.mp4");
    ffmpeg(&[
        SINE[0],
        SINE[1],
        SINE[2],
        SINE[3],
        "-c:a",
        "aac",
        "-f",
        "mp4",
        audio_only.to_str().unwrap(),
    ]);
    let names_before = dir.names();
    let err = convert_file(&dir, &audio_only, Format::Mov).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DecodeFailed);
    assert_eq!(err.user_message, "voice.mp4 has no video to convert.");
    assert_eq!(dir.names(), names_before);
}

#[test]
fn playlists_and_corrupt_files_named_like_media_are_refused() {
    let dir = TestDir::new("media-hostile");
    // A real file next to them that a playlist would pull in.
    make_source(&dir.path, "secret.mp4", Format::Mp4);
    let hostile: [(&str, &str); 4] = [
        ("concat.mp4", "ffconcat version 1.0\nfile secret.mp4\n"),
        ("hls.mp4", "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nsecret.mp4\n#EXT-X-ENDLIST\n"),
        (
            "dash.mp4",
            "<?xml version=\"1.0\"?><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" mediaPresentationDuration=\"PT1S\"><Period><AdaptationSet><Representation id=\"1\" bandwidth=\"1\"><BaseURL>secret.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>",
        ),
        ("garbage.mp4", "this is not a video"),
    ];
    for (name, text) in hostile {
        let src = dir.join(name);
        fs::write(&src, text).unwrap();
        let names_before = dir.names();
        let err = convert_file(&dir, &src, Format::Mov).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::DecodeFailed,
            "{name}: {err} ({})",
            err.detail
        );
        assert_eq!(
            err.user_message,
            format!("{name} isn't a readable MP4 video.")
        );
        assert_eq!(dir.names(), names_before, "{name}");
    }
}

#[test]
fn a_cancel_mid_conversion_stops_ffmpeg_and_leaves_nothing() {
    let dir = TestDir::new("media-cancel");
    let src = dir.join("long.mp4");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=640x480:rate=30:duration=20",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "mp4",
        src.to_str().unwrap(),
    ]);
    let settings = grape_core::Settings::new(dir.join("fallback"));
    let sel = grape_engines::build_selection(std::slice::from_ref(&src), &settings);
    // WebM re-encodes (VP9), so it takes a while.
    let job = grape_core::plan(&sel, &Operation::convert(Format::Webm), &settings)
        .unwrap()
        .jobs
        .remove(0);
    let before = SourceState::of(&src);
    let ctx = ctx();
    let app_side = ctx.cancel.clone();
    let mut cancelled_at = None;

    let err = run_job(&job, &ctx, &mut |f| {
        if f > 0.0 && cancelled_at.is_none() {
            app_side.cancel();
            cancelled_at = Some(Instant::now());
        }
    })
    .unwrap_err();

    assert_eq!(err.kind, ErrorKind::Cancelled);
    let after = cancelled_at.expect("progress was reported").elapsed();
    assert!(
        after < Duration::from_secs(3),
        "stopped {after:?} after the cancel"
    );
    assert!(!planned_file(&job).exists());
    assert_no_leftovers(&dir.path);
    before.assert_unchanged();
}

#[test]
fn a_name_taken_while_converting_gets_the_next_number() {
    let dir = TestDir::new("media-collision");
    let src = make_source(&dir.path, "song.wav", Format::Wav);
    let bytes = fs::read(&src).unwrap();
    fs::remove_file(&src).unwrap();
    let (src, job) = plan_one(&dir, "song.wav", &bytes, Operation::convert(Format::Flac));
    let planned = planned_file(&job);
    let before = SourceState::of(&src);
    let mut taken = false;

    let outcome = run_job(&job, &ctx(), &mut |_| {
        if !taken {
            fs::write(&planned, b"not ours").unwrap();
            taken = true;
        }
    })
    .unwrap();

    assert_eq!(outcome.outputs, [dir.join("song 2.flac")]);
    assert!(outcome.renamed);
    assert_eq!(fs::read(&planned).unwrap(), b"not ours");
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn names_with_percent_signs_colons_and_dashes_work() {
    let dir = TestDir::new("media-names");
    for name in [
        "100% done.gif",
        "-v error.wav",
        "a:b.mp3",
        "pipe:1.ogg",
        "ảnh chụp.mov",
    ] {
        let f = Format::from_file_name(name).unwrap();
        // ':' isn't allowed in Windows file names.
        if cfg!(windows) && name.contains(':') {
            continue;
        }
        let src = make_source(&dir.path, name, f);
        let to = targets(f)[0];
        let (outcome, _) =
            convert_file(&dir, &src, to).unwrap_or_else(|e| panic!("{name}: {e} ({})", e.detail));
        assert!(outcome.outputs[0].exists(), "{name}");
    }
}

#[test]
fn the_sidecar_has_everything_grape_uses_and_is_redistributable() {
    let run = |args: &[&str]| {
        let out = Command::new(tool("GRAPE_TEST_FFMPEG"))
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let version = run(&["-hide_banner", "-version"]);
    assert!(
        !version.contains("--enable-nonfree"),
        "a nonfree build can't be shipped"
    );
    let encoders = run(&["-hide_banner", "-encoders"]);
    for encoder in [
        "libx264",
        "aac",
        "libvpx-vp9",
        "libopus",
        "mpeg4",
        "libmp3lame",
        "wmv2",
        "wmav2",
        "gif",
        "png",
        "pcm_s16le",
        "pcm_s24le",
        "pcm_s16be",
        "pcm_s24be",
        "flac",
        "libvorbis",
    ] {
        assert!(
            encoders
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(encoder)),
            "encoder {encoder} missing"
        );
    }
    let muxers = run(&["-hide_banner", "-muxers"]);
    for muxer in [
        "mp4",
        "mov",
        "matroska",
        "webm",
        "avi",
        "asf",
        "gif",
        "mp3",
        "ipod",
        "wav",
        "flac",
        "ogg",
        "opus",
        "aiff",
        "image2pipe",
    ] {
        assert!(
            muxers
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(muxer)),
            "muxer {muxer} missing"
        );
    }
    let demuxers = run(&["-hide_banner", "-demuxers"]);
    for demuxer in [
        "mov,mp4,m4a,3gp,3g2,mj2",
        "matroska,webm",
        "avi",
        "asf",
        "mp3",
        "flac",
        "ogg",
        "wav",
        "aiff",
        "gif",
        "png_pipe",
    ] {
        assert!(
            demuxers
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(demuxer)),
            "demuxer {demuxer} missing"
        );
    }
}
