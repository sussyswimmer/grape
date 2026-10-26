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
use std::sync::atomic::{AtomicBool, Ordering};
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
        ..SidecarPaths::default()
    }
}

fn ctx() -> EngineCtx {
    EngineCtx {
        sidecars: sidecars(),
        ..EngineCtx::default()
    }
}

/// The encoders, filters (the lavfi sources among them) and muxers of
/// Grape's own FFmpeg build (DECISIONS D24), as `scripts/ffmpeg/build.sh`
/// lists them on the build's branch. Sources made with anything else
/// would only fail against that build, so [`ffmpeg`] refuses them with
/// every FFmpeg. Keep these in step with D24.
const BUILD_ENCODERS: [&str; 16] = [
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
];
const BUILD_FILTERS: [&str; 20] = [
    "scale",
    "fps",
    "palettegen",
    "paletteuse",
    "format",
    "aformat",
    "aresample",
    "null",
    "anull",
    "transpose",
    "hflip",
    "vflip",
    "rotate",
    "crop",
    "trim",
    "atrim",
    "premultiply_dynamic",
    "sine",
    "testsrc",
    "pan",
];
const BUILD_MUXERS: [&str; 15] = [
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
];

/// The first encoder, filter or format in an `ffmpeg` argv that Grape's
/// FFmpeg build lacks ([`BUILD_ENCODERS`]), named for a panic message.
fn lacking(args: &[&str]) -> Option<String> {
    let mut lavfi_input = false;
    for pair in args.windows(2) {
        let (option, value) = (pair[0], pair[1]);
        let missing = match option {
            "-c" | "-codec" | "-vcodec" | "-acodec" => encoder_lacking(value),
            _ if option.starts_with("-c:") || option.starts_with("-codec:") => {
                encoder_lacking(value)
            }
            "-f" if value == "lavfi" => {
                lavfi_input = true;
                None
            }
            "-f" => (!BUILD_MUXERS.contains(&value)).then(|| format!("format {value}")),
            "-i" if lavfi_input => {
                lavfi_input = false;
                filters_lacking(value)
            }
            "-vf" | "-af" | "-lavfi" | "-filter_complex" => filters_lacking(value),
            _ if option.starts_with("-filter:") => filters_lacking(value),
            _ => None,
        };
        if missing.is_some() {
            return missing;
        }
    }
    None
}

fn encoder_lacking(name: &str) -> Option<String> {
    (name != "copy" && !BUILD_ENCODERS.contains(&name)).then(|| format!("encoder {name}"))
}

/// The first filter of the filtergraph `graph` that the build lacks. A
/// ',' or ';' escaped with '\\' or inside quotes doesn't end a filter.
fn filters_lacking(graph: &str) -> Option<String> {
    let mut filters = vec![String::new()];
    let (mut quoted, mut escaped) = (false, false);
    for c in graph.chars() {
        let last = filters.last_mut()?;
        match c {
            _ if escaped => {
                escaped = false;
                last.push(c);
            }
            '\\' => escaped = true,
            '\'' => quoted = !quoted,
            ',' | ';' if !quoted => filters.push(String::new()),
            _ => last.push(c),
        }
    }
    filters.iter().find_map(|filter| {
        // Labels like `[0:v]` come before the name, and after it.
        let mut name = filter.trim();
        while let Some(rest) = name.strip_prefix('[') {
            name = rest
                .split_once(']')
                .map_or("", |(_, after)| after.trim_start());
        }
        let name = name.split(['=', '[', '@']).next().unwrap_or("").trim();
        (!name.is_empty() && !BUILD_FILTERS.contains(&name)).then(|| format!("filter {name}"))
    })
}

/// Runs the test FFmpeg with `args`; panics with its stderr if it fails,
/// or if `args` need what Grape's FFmpeg build lacks ([`lacking`]).
fn ffmpeg(args: &[&str]) {
    if let Some(missing) = lacking(args) {
        panic!("{missing} isn't in Grape's FFmpeg build (D24); make the source with what it has: {args:?}");
    }
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

#[test]
fn sources_needing_what_grapes_ffmpeg_lacks_are_refused() {
    // What earlier tests used, which the build leaves out.
    let lacks = |args: &[&str]| lacking(args).unwrap_or_default();
    let testsrc2 = [
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30",
        "x.mp4",
    ];
    assert_eq!(lacks(&testsrc2), "filter testsrc2");
    assert_eq!(
        lacks(&["-i", "a.mov", "-c:v", "ffv1", "b.mkv"]),
        "encoder ffv1"
    );
    let setpts = ["-i", "a.mov", "-vf", "fps=10,setpts=PTS-STARTPTS", "b.mkv"];
    assert_eq!(lacks(&setpts), "filter setpts");
    let split = ["-i", "a", "-lavfi", "[0:v]split[a][b];[a][b]hstack", "b"];
    assert_eq!(lacks(&split), "filter split");
    assert_eq!(
        lacks(&["-i", "a.wav", "-f", "s16le", "b.raw"]),
        "format s16le"
    );
    // What they use now, labels and escaped commas included.
    let sources = [
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=1x1:rate=1",
        "-f",
        "lavfi",
        "-i",
        "sine=duration=1,aformat=channel_layouts=5.1(side)",
        "-af",
        "pan=5.1|c0=c0|c1=c0",
        "-c:v",
        "libx264",
        "-c:a",
        "copy",
        "-f",
        "matroska",
        "x.mkv",
    ];
    assert_eq!(lacking(&sources), None);
    let gif = r"[0:V:0]fps=12,scale='min(iw\,720)':-1[x];[x][1:v]paletteuse";
    assert_eq!(
        lacking(&["-lavfi", gif, "-c:v", "gif", "-f", "gif", "x"]),
        None
    );
}

#[test]
fn the_guard_reads_codecs_escapes_and_quotes_each_on_its_own() {
    // Plain -c (and its spellings), not only -c:v and -c:a.
    for option in ["-c", "-codec", "-vcodec", "-acodec"] {
        let args = ["-i", "a.mov", option, "ffv1", "b.mkv"];
        assert_eq!(lacking(&args).as_deref(), Some("encoder ffv1"), "{option}");
        assert_eq!(lacking(&["-i", "a.mov", option, "copy", "b.mkv"]), None);
    }
    // An escaped comma outside quotes, and a plain one inside them, don't
    // end the filter: each alone, so neither hides the other.
    let escaped = r"scale=w=min(iw\,720):h=-1";
    assert_eq!(lacking(&["-vf", escaped]), None);
    assert_eq!(lacking(&["-vf", r"pan=stereo|c0=c0\;c1=c1"]), None);
    let quoted = "scale='min(iw,720)':-1";
    assert_eq!(lacking(&["-vf", quoted]), None);
    assert_eq!(lacking(&["-lavfi", "[0:v]scale='a;b'[x]"]), None);
    // After either, the next filter is read again.
    let after = |graph: &str| lacking(&["-vf", graph]);
    let escaped_then = format!("{escaped},setpts=PTS");
    assert_eq!(after(&escaped_then).as_deref(), Some("filter setpts"));
    let quoted_then = format!("{quoted};hstack");
    assert_eq!(after(&quoted_then).as_deref(), Some("filter hstack"));
}

/// Makes a source with [`ffmpeg`]: it must refuse, before running
/// anything, what Grape's FFmpeg build lacks.
#[test]
#[should_panic(expected = "filter testsrc2 isn't in Grape's FFmpeg build")]
fn the_source_maker_refuses_what_the_build_lacks() {
    let dir = TestDir::new("media-guard");
    let out = dir.join("never.mkv");
    // testsrc2 and FFV1 are in 6.1 and not in Grape's 9.0 build: without
    // the guard this would pass on one and fail differently on the other.
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=16x16:duration=0.1",
        "-c:v",
        "ffv1",
        out.to_str().unwrap(),
    ]);
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
    // PNG in MOV takes any size.
    for (size, expected) in [("9001x101", ("8190", "90")), ("101x9001", ("90", "8190"))] {
        let src = dir.join(&format!("{size}.mov"));
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={size}:rate=1:duration=1"),
            "-c:v",
            "png",
            "-f",
            "mov",
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
fn a_one_pixel_side_becomes_two_pixels() {
    let dir = TestDir::new("media-one-pixel");
    // yuv420p needs at least 2 px a side, which AVI's capped filter must
    // keep too. PNG in MOV takes any size, and no target copies it (MOV
    // itself isn't offered for a MOV).
    for (size, expected, targets) in [
        (
            "1x101",
            ("2", "100"),
            &[
                Format::Mp4,
                Format::Mkv,
                Format::Webm,
                Format::Avi,
                Format::Wmv,
            ][..],
        ),
        ("1x1", ("2", "2"), &[Format::Avi][..]),
    ] {
        let src = dir.join(&format!("{size}.mov"));
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={size}:rate=10:duration=0.5"),
            "-c:v",
            "png",
            "-f",
            "mov",
            src.to_str().unwrap(),
        ]);
        for &to in targets {
            let (outcome, _) = convert_file(&dir, &src, to)
                .unwrap_or_else(|e| panic!("{size} to {to:?}: {e} ({})", e.detail));
            let info = probe(&outcome.outputs[0]);
            let width = field(&info, "video", "width").unwrap();
            let height = field(&info, "video", "height").unwrap();
            assert_eq!(
                (width.as_str(), height.as_str()),
                expected,
                "{size} to {to:?}"
            );
            fs::remove_file(&outcome.outputs[0]).unwrap();
        }
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
    // Re-encoded: from an MKV that carries the timecode as a tag, with
    // MPEG-4 Part 2 video, which MP4 and MOV don't copy.
    let mkv = dir.join("camera.mkv");
    let mut args: Vec<&str> = vec!["-i", src.to_str().unwrap(), "-map", "0:v", "-map", "0:a"];
    args.extend(["-c:v", "mpeg4", "-c:a", "copy", "-timecode", "01:00:00:00"]);
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

/// The tone of each channel of [`write_unlabeled_wav`]'s files, in Hz.
/// The fourth, 900 Hz, is one an LFE's low-pass removes (libvorbis's 5.1
/// keeps up to about 250 Hz, libopus's surround mapping about 400 Hz).
const TONES: [f64; 10] = [
    300.0, 500.0, 700.0, 900.0, 1100.0, 1300.0, 1500.0, 1700.0, 1900.0, 2100.0,
];

/// Writes a 1 s, 48 kHz, 16-bit WAV whose channels are sines at `tones`,
/// with the channel mask `mask` (a `WAVEFORMATEXTENSIBLE` header) or, for
/// `None`, a plain `WAVEFORMATEX` header: no mask, so FFmpeg sees a channel
/// count and no layout.
fn write_wav(path: &Path, tones: &[f64], mask: Option<u32>) {
    let rate = 48_000u32;
    let mut data = Vec::new();
    for n in 0..rate {
        let t = f64::from(n) / f64::from(rate);
        for tone in tones {
            let sample = (8000.0 * (std::f64::consts::TAU * tone * t).sin()) as i16;
            data.extend_from_slice(&sample.to_le_bytes());
        }
    }
    let channels = u16::try_from(tones.len()).unwrap();
    // PCM, then channels, rate, bytes per second, block size and bits.
    let mut format = Vec::new();
    format.extend_from_slice(&if mask.is_some() { 0xFFFEu16 } else { 1 }.to_le_bytes());
    format.extend_from_slice(&channels.to_le_bytes());
    format.extend_from_slice(&rate.to_le_bytes());
    format.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    format.extend_from_slice(&(channels * 2).to_le_bytes());
    format.extend_from_slice(&16u16.to_le_bytes());
    if let Some(mask) = mask {
        // 22 more bytes: the valid bits, the mask, and PCM's subformat GUID.
        format.extend_from_slice(&22u16.to_le_bytes());
        format.extend_from_slice(&16u16.to_le_bytes());
        format.extend_from_slice(&mask.to_le_bytes());
        format.extend_from_slice(&[
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
    }
    let mut body = Vec::new();
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&u32::try_from(format.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&format);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&data);
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    wav.extend_from_slice(&body);
    fs::write(path, wav).unwrap();
}

/// [`write_wav`] of `channels` channels at their [`TONES`], without a mask.
fn write_unlabeled_wav(path: &Path, channels: usize) {
    write_wav(path, &TONES[..channels], None);
}

/// The samples of each channel of a 16-bit PCM WAV, and its sample rate.
fn read_wav(path: &Path) -> (Vec<Vec<f64>>, f64) {
    let bytes = fs::read(path).unwrap();
    let (mut channels, mut rate, mut data) = (0, 0.0, &[][..]);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = &bytes[at + 8..(at + 8 + size).min(bytes.len())];
        match &bytes[at..at + 4] {
            b"fmt " => {
                channels = usize::from(u16::from_le_bytes([body[2], body[3]]));
                rate = f64::from(u32::from_le_bytes(body[4..8].try_into().unwrap()));
            }
            b"data" => data = body,
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    let mut samples = vec![Vec::new(); channels];
    for (i, sample) in data.as_chunks::<2>().0.iter().enumerate() {
        samples[i % channels].push(f64::from(i16::from_le_bytes(*sample)));
    }
    (samples, rate)
}

/// How strong `tone` (Hz) is in `samples` (Goertzel, over half a second).
fn tone_power(samples: &[f64], rate: f64, tone: f64) -> f64 {
    let k = 2.0 * (std::f64::consts::TAU * tone / rate).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in samples.iter().take(24_000) {
        let s0 = x + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - k * s1 * s2
}

/// Where the channels of the WAV `src`, a sine at `tones` each, went in
/// `out`: the test FFmpeg decodes it with its own Opus or Vorbis decoder,
/// and source channel `i` must be in output channel `homes[i]`, within 6 dB
/// of its level, with no other tone there above a tenth of it; any other
/// output channel must be silent. Returns what went wrong, labelled.
fn misplaced(
    dir: &TestDir,
    (src, tones): (&Path, &[f64]),
    out: &Path,
    homes: &[usize],
    label: &str,
) -> Vec<String> {
    let decoded = dir.join("decoded.wav");
    let (out_str, dec_str) = (out.to_str().unwrap(), decoded.to_str().unwrap());
    ffmpeg(&[
        "-i",
        out_str,
        "-map",
        "0:a:0",
        "-c:a",
        "pcm_s16le",
        "-f",
        "wav",
        dec_str,
    ]);
    let (source, source_rate) = read_wav(src);
    let (samples, rate) = read_wav(&decoded);
    fs::remove_file(&decoded).unwrap();
    let full: Vec<f64> = (0..tones.len())
        .map(|i| tone_power(&source[i], source_rate, tones[i]))
        .collect();
    let mut wrong = Vec::new();
    if samples.len() <= homes.iter().copied().max().unwrap_or(0) {
        wrong.push(format!("{label}: {} channels", samples.len()));
    }
    for (c, channel) in samples.iter().enumerate() {
        let powers: Vec<f64> = tones
            .iter()
            .map(|&t| tone_power(channel, rate, t))
            .collect();
        let levels = || -> Vec<String> {
            (0..tones.len())
                .map(|j| format!("{:.1}", 10.0 * (powers[j] / full[j]).log10()))
                .collect()
        };
        match homes.iter().position(|&home| home == c) {
            Some(i) => {
                let mixed = (0..tones.len()).any(|j| j != i && powers[j] > powers[i] * 0.1);
                if powers[i] < full[i] * 0.25 || mixed {
                    wrong.push(format!(
                        "{label}: channel {c} (source {i}) holds {:?} dB",
                        levels()
                    ));
                }
            }
            None if (0..tones.len()).any(|j| powers[j] > full[j] * 0.001) => {
                wrong.push(format!("{label}: channel {c} holds {:?} dB", levels()));
            }
            None => {}
        }
    }
    wrong
}

#[test]
fn audio_without_a_layout_keeps_its_channels() {
    let dir = TestDir::new("media-unlabeled");
    let mut failures = Vec::new();
    // Field recorders' polyphonic WAVs and ambisonics have no channel mask.
    // Up to 8 channels keep their count, each in its own output channel at
    // full bandwidth, except that Opus puts 7 in 7.1 and Vorbis 6 in 6.1,
    // around the LFE, which stays silent; more are downmixed to 7.1.
    let vorbis = ["mono", "stereo", "3.0", "quad", "5.0", "5.1", "6.1", "7.1"];
    for channels in [3, 4, 5, 6, 7, 8, 10] {
        let wav = dir.join(&format!("{channels}ch.wav"));
        write_unlabeled_wav(&wav, channels);
        // WebM needs video: the same audio, copied, next to some in MKV.
        let mkv = dir.join(&format!("{channels}ch.mkv"));
        let mut args: Vec<&str> = TESTSRC.to_vec();
        args.extend(["-i", wav.to_str().unwrap(), "-map", "0:v", "-map", "1:a"]);
        args.extend(["-c:v", "mpeg4", "-c:a", "copy", "-f", "matroska"]);
        args.push(mkv.to_str().unwrap());
        ffmpeg(&args);
        for src in [&wav, &mkv] {
            let layout = field(&probe(src), "audio", "channel_layout");
            assert_eq!(layout.as_deref(), Some("unknown"), "{}", src.display());
        }
        for (src, to) in [
            (&wav, Format::Ogg),
            (&wav, Format::Opus),
            (&mkv, Format::Webm),
        ] {
            let label = format!("{channels} channels to {to:?}");
            let opus = to != Format::Ogg;
            let (expected, homes): (&str, Vec<usize>) = match channels {
                7 if opus => ("7.1", vec![0, 1, 2, 4, 5, 6, 7]),
                6 if !opus => ("6.1", vec![0, 1, 2, 4, 5, 6]),
                1..=8 => (vorbis[channels - 1], (0..channels).collect()),
                _ => ("7.1", Vec::new()),
            };
            let out = match convert_file(&dir, src, to) {
                Ok((outcome, _)) => outcome.outputs[0].clone(),
                Err(e) => {
                    failures.push(format!("{label}: {e} ({})", e.detail));
                    continue;
                }
            };
            let layout = field(&probe(&out), "audio", "channel_layout").unwrap_or_default();
            if layout != expected {
                failures.push(format!("{label}: {layout}"));
            }
            if !homes.is_empty() {
                let source = (wav.as_path(), &TONES[..channels]);
                failures.extend(misplaced(&dir, source, &out, &homes, &label));
            }
            fs::remove_file(&out).unwrap();
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The tones of [`surround_opus_keeps_each_channel_in_its_speaker`]'s
/// files, but for the LFE's.
const SURROUND_TONES: [f64; 8] = [500.0, 700.0, 1100.0, 1300.0, 1700.0, 2300.0, 2900.0, 3700.0];

/// The LFE's tone, in its band.
const LFE_TONE: f64 = 60.0;

#[test]
fn surround_opus_keeps_each_channel_in_its_speaker() {
    let dir = TestDir::new("media-opus-surround");
    // libopus's default mapping put the centre and back channels of 5.0
    // and 6.1 in other speakers. WAV and MOV keep these layouts, so OPUS
    // and WebM get them as they are, each channel in its place. The LFE's
    // tone is low: libopus keeps nothing above about 400 Hz there.
    let mut failures = Vec::new();
    let layouts: [(&str, u32); 6] = [
        ("3.0", 0x7),
        ("quad", 0x33),
        ("5.0", 0x37),
        ("5.1", 0x3f),
        ("6.1", 0x70f),
        ("7.1", 0x63f),
    ];
    for (layout, mask) in layouts {
        let channels = mask.count_ones() as usize;
        let lfe = (mask & 0x8 != 0).then(|| (mask & 0x7).count_ones() as usize);
        let tones: Vec<f64> = (0..channels)
            .map(|c| {
                if Some(c) == lfe {
                    LFE_TONE
                } else {
                    SURROUND_TONES[c]
                }
            })
            .collect();
        let name: String = layout.chars().filter(char::is_ascii_alphanumeric).collect();
        let wav = dir.join(&format!("{name}.wav"));
        write_wav(&wav, &tones, Some(mask));
        let mov = dir.join(&format!("{name}.mov"));
        let mut args: Vec<&str> = TESTSRC.to_vec();
        args.extend(["-i", wav.to_str().unwrap(), "-map", "0:v", "-map", "1:a"]);
        args.extend([
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "copy", "-f", "mov",
        ]);
        args.push(mov.to_str().unwrap());
        ffmpeg(&args);
        for (src, to) in [(&wav, Format::Opus), (&mov, Format::Webm)] {
            let label = format!("{layout} to {to:?}");
            assert_eq!(
                field(&probe(src), "audio", "channel_layout").as_deref(),
                Some(layout)
            );
            let out = match convert_file(&dir, src, to) {
                Ok((outcome, _)) => outcome.outputs[0].clone(),
                Err(e) => {
                    failures.push(format!("{label}: {e} ({})", e.detail));
                    continue;
                }
            };
            let got = field(&probe(&out), "audio", "channel_layout").unwrap_or_default();
            if got != layout {
                failures.push(format!("{label}: {got}"));
            }
            let homes: Vec<usize> = (0..channels).collect();
            let source = (wav.as_path(), tones.as_slice());
            failures.extend(misplaced(&dir, source, &out, &homes, &label));
            fs::remove_file(&out).unwrap();
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

/// How many of `path`'s video frames are B-frames.
fn b_frames(path: &Path) -> usize {
    let out = Command::new(tool("GRAPE_TEST_FFPROBE"))
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "frame=pict_type", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe {}", path.display());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.trim_end_matches(',') == "B")
        .count()
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
            "testsrc=size=320x240:rate=30:duration=1",
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
        assert!(b_frames(&src) > 0, "{name} has no B-frames");
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
fn a_copy_ffmpeg_refuses_is_retried_re_encoding_everything() {
    let dir = TestDir::new("media-retry");
    // H.264 and MP3 in Matroska fit MP4's copy rule, but the mp4 muxer
    // refuses MP3 below 16 kHz ("muxing mp3 at 8000hz is not standard").
    let src = dir.join("voice.mkv");
    ffmpeg(&[
        TESTSRC[0],
        TESTSRC[1],
        TESTSRC[2],
        TESTSRC[3],
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=1:sample_rate=8000",
        "-shortest",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "libmp3lame",
        src.to_str().unwrap(),
    ]);
    let info = probe(&src);
    assert_eq!(field(&info, "video", "codec_name").as_deref(), Some("h264"));
    assert_eq!(field(&info, "audio", "codec_name").as_deref(), Some("mp3"));
    assert_eq!(
        field(&info, "audio", "sample_rate").as_deref(),
        Some("8000")
    );

    let (outcome, seen) =
        convert_file(&dir, &src, Format::Mp4).unwrap_or_else(|e| panic!("{e} ({})", e.detail));
    let out = probe(&outcome.outputs[0]);
    // AAC: the retry re-encoded the audio the copy couldn't keep.
    assert_eq!(field(&out, "video", "codec_name").as_deref(), Some("h264"));
    assert_eq!(field(&out, "audio", "codec_name").as_deref(), Some("aac"));
    assert_eq!(seen.last(), Some(&1.0));
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
}

#[test]
fn a_clip_shorter_than_one_gif_frame_still_makes_a_gif() {
    let dir = TestDir::new("media-short-gif");
    // One frame (1/30 s), and two frames at 50 fps: both shorter than a
    // GIF frame at 12 fps.
    for (name, source, frames) in [
        ("one.mp4", "testsrc=size=320x240:rate=30", "1"),
        ("two.mp4", "testsrc=size=320x240:rate=50", "2"),
    ] {
        let src = dir.join(name);
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            source,
            "-frames:v",
            frames,
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

/// How many frames `path`'s video stream has, counted by decoding it.
fn frame_count(path: &Path) -> usize {
    let out = Command::new(tool("GRAPE_TEST_FFPROBE"))
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe {}", path.display());
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim().trim_end_matches(',').parse().unwrap()
}

/// Clears its flag when dropped, a panic included.
struct Lower<'a>(&'a AtomicBool);

impl Drop for Lower<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

#[test]
fn a_short_wmv_keeps_every_gif_frame() {
    let dir = TestDir::new("media-wmv-gif");
    // 1 s of WMV2 at 10 fps: the frames start at 0.046 s, after the
    // audio, and the last ends at 1.046 s, so at 12 fps the GIF has 12
    // frames and lasts 1 s. The ASF packets FFmpeg reads while probing
    // have no duration, and read only once, 9.0 now and then ended the GIF
    // a frame early, more often on a busy machine: twenty conversions, with
    // a busy thread per core. That only catches a race when it happens:
    // without the rewind, 14 of 15 runs failed here (4 cores), and a
    // quieter machine may miss it. The argv's unit test is the sure guard.
    let src = make_source(&dir.path, "clip.wmv", Format::Wmv);
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let busy = AtomicBool::new(true);
    let mut seen = Vec::new();
    std::thread::scope(|scope| {
        for _ in 0..cores {
            scope.spawn(|| {
                while busy.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
        }
        let _lower = Lower(&busy);
        for _ in 0..20 {
            let (outcome, _) = convert_file(&dir, &src, Format::Gif)
                .unwrap_or_else(|e| panic!("{e} ({})", e.detail));
            let gif = &outcome.outputs[0];
            let duration: f64 = probe(gif)["format.duration"].parse().unwrap();
            seen.push((frame_count(gif), (duration * 100.0).round() / 100.0));
            fs::remove_file(gif).unwrap();
        }
    });
    assert!(seen.iter().all(|&run| run == (12, 1.0)), "{seen:?}");
}

#[test]
fn b_frame_wmvs_and_avis_make_whole_gifs() {
    let dir = TestDir::new("media-b-frame-gif");
    // H.264 and MPEG-4 Part 2 with B-frames in every group (`-b_strategy
    // 0`), at 30 fps: read from its start again (`-ss 0`), a WMV's GIF lost
    // frames or had none. Each GIF covers the clip: 12 frames a second,
    // maybe one more where the decode times round up, and as long.
    let mut failures = Vec::new();
    for secs in [1u32, 5, 10] {
        let video = format!("testsrc=size=160x120:rate=30:duration={secs}");
        let tone = format!("sine=frequency=440:duration={secs}:sample_rate=44100");
        for (codec, most) in [("libx264", "3"), ("mpeg4", "2")] {
            for (ext, muxer, audio) in [("wmv", "asf", "wmav2"), ("avi", "avi", "libmp3lame")] {
                let name = format!("{codec}-{secs}s.{ext}");
                let src = dir.join(&name);
                let mut args = vec!["-f", "lavfi", "-i", &video, "-f", "lavfi", "-i", &tone];
                args.extend(["-shortest", "-c:v", codec, "-bf", most, "-b_strategy", "0"]);
                args.extend(["-pix_fmt", "yuv420p", "-c:a", audio, "-f", muxer]);
                args.push(src.to_str().unwrap());
                ffmpeg(&args);
                assert!(b_frames(&src) > 0, "{name} has no B-frames");
                let gif = match convert_file(&dir, &src, Format::Gif) {
                    Ok((outcome, _)) => outcome.outputs[0].clone(),
                    Err(e) => {
                        failures.push(format!("{name}: {e} ({})", e.detail));
                        continue;
                    }
                };
                let frames = frame_count(&gif);
                let duration: f64 = probe(&gif)["format.duration"].parse().unwrap();
                let whole = 12 * secs as usize;
                let late = duration - f64::from(secs);
                if !(whole..=whole + 1).contains(&frames) || !(-0.01..0.09).contains(&late) {
                    failures.push(format!("{name}: {frames} frames, {duration} s"));
                }
                fs::remove_file(&gif).unwrap();
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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

#[cfg(unix)]
#[test]
fn a_pipe_named_like_media_is_refused_at_once() {
    let dir = TestDir::new("media-fifo");
    let fifo = dir.join("pipe.wav");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    // Opening the pipe would block until someone writes to it, so the
    // source isn't hashed.
    let settings = grape_core::Settings::new(dir.join("fallback"));
    let sel = grape_engines::build_selection(std::slice::from_ref(&fifo), &settings);
    let job = grape_core::plan(&sel, &Operation::convert(Format::Mp3), &settings)
        .unwrap()
        .jobs
        .remove(0);
    let ctx = ctx();
    // ffprobe would wait on the pipe for its whole timeout; end it sooner.
    let (finished, watching) = std::sync::mpsc::channel::<()>();
    let cancel = ctx.cancel.clone();
    let watchdog = std::thread::spawn(move || {
        if watching.recv_timeout(Duration::from_secs(5)).is_err() {
            cancel.cancel();
        }
    });
    let started = Instant::now();
    let err = run_job(&job, &ctx, &mut |_| {}).unwrap_err();
    let took = started.elapsed();
    drop(finished);
    watchdog.join().unwrap();
    assert_eq!(err.kind, ErrorKind::ReadFailed, "{} ({})", err, err.detail);
    assert_eq!(err.user_message, "Grape couldn't read pipe.wav.");
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert!(!planned_file(&job).exists());
    assert_no_leftovers(&dir.path);
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
    // The filters Grape names, and the ones FFmpeg inserts for them: format
    // and rate conversion, turning rotated video upright, and (9.0) the
    // trim behind `-ss`.
    let filters = run(&["-hide_banner", "-filters"]);
    for filter in [
        "scale",
        "fps",
        "palettegen",
        "paletteuse",
        "aformat",
        "pan",
        "aresample",
        "format",
        "null",
        "anull",
        "transpose",
        "hflip",
        "vflip",
        "trim",
    ] {
        assert!(
            filters
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(filter)),
            "filter {filter} missing"
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
