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
        .arg("format=format_name,duration:stream=index,codec_type,codec_name,width,height,channels,channel_layout,sample_rate:stream_side_data=rotation")
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
                    // Forward only, and 1.0 only once the file is there.
                    let done = seen.split_last().is_some_and(|(&last, before)| {
                        last == 1.0 && before.iter().all(|&f| f <= 0.99)
                    });
                    if !done || !seen.windows(2).all(|w| w[0] < w[1]) {
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
fn video_too_large_for_avi_is_scaled_to_fit() {
    let dir = TestDir::new("media-avi-size");
    // MPEG-4 Part 2 refuses 8192 px or more on either side. A panorama and
    // a tall strip, one frame each; odd sizes, to check the even rounding.
    for (size, expected) in [("9001x101", ("8190", "90")), ("101x9001", ("90", "8190"))] {
        let src = dir.join(&format!("{size}.mkv"));
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={size}:rate=1:duration=1"),
            "-c:v",
            "ffv1",
            src.to_str().unwrap(),
        ]);
        let (outcome, _) = convert_file(&dir, &src, Format::Avi)
            .unwrap_or_else(|e| panic!("{size}: {e} ({})", e.detail));
        let info = probe(&outcome.outputs[0]);
        let width = field(&info, "video", "width").unwrap();
        let height = field(&info, "video", "height").unwrap();
        assert_eq!((width.as_str(), height.as_str()), expected, "{size}");
    }
}

#[test]
fn a_camera_timecode_doesnt_become_a_track() {
    let dir = TestDir::new("media-timecode");
    // Cameras write a timecode, which MOV keeps as a data (tmcd) track.
    let src = dir.join("camera.mov");
    let mut args: Vec<&str> = TESTSRC.to_vec();
    args.extend(SINE);
    args.extend(["-shortest", "-c:v", "libx264", "-pix_fmt", "yuv420p"]);
    args.extend(["-c:a", "aac", "-timecode", "01:00:00:00"]);
    args.push(src.to_str().unwrap());
    ffmpeg(&args);
    let kinds = |path: &Path| -> Vec<String> {
        let info = probe(path);
        (0..8)
            .filter_map(|i| info.get(&format!("streams.stream.{i}.codec_type")).cloned())
            .collect()
    };
    assert_eq!(kinds(&src), ["video", "audio", "data"]);
    // Copied: to MP4 and MKV.
    for to in [Format::Mp4, Format::Mkv] {
        let (outcome, _) = convert_file(&dir, &src, to).unwrap();
        assert_eq!(kinds(&outcome.outputs[0]), ["video", "audio"], "{to:?}");
        fs::remove_file(&outcome.outputs[0]).unwrap();
    }
    // Re-encoded: from an MKV that carries the timecode as a tag.
    let mkv = dir.join("camera.mkv");
    let mut args: Vec<&str> = vec!["-i", src.to_str().unwrap(), "-map", "0:v", "-map", "0:a"];
    args.extend(["-c:v", "ffv1", "-c:a", "copy", "-timecode", "01:00:00:00"]);
    args.push(mkv.to_str().unwrap());
    ffmpeg(&args);
    for to in [Format::Mp4, Format::Mov] {
        let (outcome, _) = convert_file(&dir, &mkv, to).unwrap();
        assert_eq!(kinds(&outcome.outputs[0]), ["video", "audio"], "{to:?}");
        fs::remove_file(&outcome.outputs[0]).unwrap();
    }
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

    // libvorbis has no setup above 200 kHz: a 384 kHz source is resampled.
    let fast = dir.join("fast.wav");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=0.5:sample_rate=384000",
        "-c:a",
        "pcm_s24le",
        fast.to_str().unwrap(),
    ]);
    let (outcome, _) = convert_file(&dir, &fast, Format::Ogg).unwrap();
    let info = probe(&outcome.outputs[0]);
    assert_eq!(
        field(&info, "audio", "codec_name").as_deref(),
        Some("vorbis")
    );
    assert_eq!(
        field(&info, "audio", "sample_rate").as_deref(),
        Some("192000")
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
fn surround_becomes_a_layout_opus_and_vorbis_take() {
    let dir = TestDir::new("media-layouts");
    // Each source layout and what Opus, Vorbis and WebM's Opus make of it.
    let layouts: [(&str, &[&str]); 7] = [
        ("mono", &["mono"]),
        ("stereo", &["stereo"]),
        // The LFE is dropped, as in any downmix.
        ("2.1", &["stereo"]),
        ("4.0", &["5.0"]),
        // FFmpeg 9 moves the side pair to the back; 6.1 (whose layout
        // match tests the LFE by index) keeps it and adds two silent
        // channels.
        ("5.0(side)", &["5.0", "6.1"]),
        // AC-3, E-AC-3, DTS and 6-channel FLAC decode to this one.
        ("5.1(side)", &["5.1"]),
        ("7.1", &["7.1"]),
    ];
    let mut failures = Vec::new();
    for (layout, expected) in layouts {
        let tone = format!("sine=frequency=440:duration=1,aformat=channel_layouts={layout}");
        let name: String = layout.chars().filter(char::is_ascii_alphanumeric).collect();
        // WAV and MOV keep the layout exactly.
        let wav = dir.join(&format!("{name}.wav"));
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &tone,
            "-c:a",
            "pcm_s16le",
            wav.to_str().unwrap(),
        ]);
        let mov = dir.join(&format!("{name}.mov"));
        let mut args: Vec<&str> = TESTSRC.to_vec();
        args.extend(["-f", "lavfi", "-i", &tone, "-shortest"]);
        args.extend([
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "pcm_s16le",
        ]);
        args.push(mov.to_str().unwrap());
        ffmpeg(&args);
        for (src, to) in [
            (&wav, Format::Opus),
            (&wav, Format::Ogg),
            (&mov, Format::Webm),
        ] {
            match convert_file(&dir, src, to) {
                Ok((outcome, _)) => {
                    let out = &outcome.outputs[0];
                    let got = field(&probe(out), "audio", "channel_layout").unwrap_or_default();
                    if !expected.contains(&got.as_str()) {
                        failures.push(format!("{layout} to {to:?}: {got}"));
                    }
                    fs::remove_file(out).unwrap();
                }
                Err(e) => failures.push(format!("{layout} to {to:?}: {e} ({})", e.detail)),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The presentation times of `path`'s video frames, in the order they're
/// decoded and shown.
fn frame_times(path: &Path) -> Vec<i64> {
    let out = Command::new(tool("GRAPE_TEST_FFPROBE"))
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "frame=pts", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe {}", path.display());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| line.trim_end_matches(',').parse().unwrap())
        .collect()
}

#[test]
fn h264_with_b_frames_from_avi_or_wmv_plays_in_order() {
    let dir = TestDir::new("media-b-frames");
    // AVI and ASF keep only decode times, so a copy would show these
    // B-frames out of order.
    for (name, muxer) in [("bframes.avi", "avi"), ("bframes.wmv", "asf")] {
        let src = dir.join(name);
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30:duration=1",
            SINE[0],
            SINE[1],
            SINE[2],
            SINE[3],
            "-shortest",
            "-c:v",
            "libx264",
            "-bf",
            "3",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "libmp3lame",
            "-f",
            muxer,
            src.to_str().unwrap(),
        ]);
        let from = Format::from_file_name(name).unwrap();
        for to in [Format::Mp4, Format::Mov, Format::Mkv] {
            if !targets(from).contains(&to) {
                continue;
            }
            let (outcome, _) = convert_file(&dir, &src, to)
                .unwrap_or_else(|e| panic!("{name} to {to:?}: {e} ({})", e.detail));
            let out = &outcome.outputs[0];
            let times = frame_times(out);
            assert!(times.len() >= 25, "{name} to {to:?}: {times:?}");
            assert!(
                times.windows(2).all(|w| w[0] < w[1]),
                "{name} to {to:?}: frames out of order: {times:?}"
            );
            fs::remove_file(out).unwrap();
        }
    }
}

#[test]
fn a_clip_shorter_than_one_gif_frame_still_makes_a_gif() {
    let dir = TestDir::new("media-short-gif");
    // One frame (1/30 s), and two frames at 50 fps: both shorter than a
    // GIF frame at 12 fps.
    for (name, source) in [
        ("one.mp4", "testsrc2=size=320x240:rate=30,trim=end_frame=1"),
        ("two.mp4", "testsrc2=size=320x240:rate=50:duration=0.04"),
    ] {
        let src = dir.join(name);
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            source,
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            src.to_str().unwrap(),
        ]);
        let (outcome, _) = convert_file(&dir, &src, Format::Gif)
            .unwrap_or_else(|e| panic!("{name}: {e} ({})", e.detail));
        let info = probe(&outcome.outputs[0]);
        assert_eq!(field(&info, "video", "codec_name").as_deref(), Some("gif"));
        assert_eq!(field(&info, "video", "width").as_deref(), Some("320"));
    }
}

#[test]
fn a_source_too_long_for_aiff_is_refused_before_writing() {
    let dir = TestDir::new("media-aiff-limit");
    // A one-second FLAC whose STREAMINFO says it holds 2^36 - 1 samples
    // (16 days at 48 kHz): ffprobe takes the duration from there.
    let src = dir.join("long.flac");
    ffmpeg(&[
        SINE[0],
        SINE[1],
        "-i",
        "sine=duration=1:sample_rate=48000",
        "-ac",
        "2",
        "-c:a",
        "flac",
        src.to_str().unwrap(),
    ]);
    let mut bytes = fs::read(&src).unwrap();
    // "fLaC", then STREAMINFO's block header (type 0).
    assert_eq!(&bytes[..4], b"fLaC");
    assert_eq!(bytes[4] & 0x7f, 0);
    // STREAMINFO's bytes 10..18 end with the 36-bit total sample count.
    let field = &mut bytes[18..26];
    let packed = u64::from_be_bytes(field.try_into().unwrap()) | ((1 << 36) - 1);
    field.copy_from_slice(&packed.to_be_bytes());
    fs::write(&src, &bytes).unwrap();
    let duration: f64 = probe(&src)["format.duration"].parse().unwrap();
    assert!(duration > 1_000_000.0, "{duration}");

    let names_before = dir.names();
    let err = convert_file(&dir, &src, Format::Aiff).unwrap_err();
    assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
    assert_eq!(err.user_message, "long.flac is too long to save as AIFF.");
    assert_eq!(dir.names(), names_before);
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
