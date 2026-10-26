//! The tools that run through FFmpeg (D26), end to end: sources made with
//! the FFmpeg under test (only from what Grape's build has, [`ffmpeg`]) or
//! written here, planned with `grape_core`, run with `run_job`, and the
//! outputs read back with `ffprobe`. Every run checks that the source never
//! changed and that no temp file was left.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grape_core::{Operation, Settings, ToolKey, ToolParams};
use grape_engines::{run_job, ErrorKind, GrapeError, JobOutcome};

use super::common::{assert_no_leftovers, planned_file, SourceState, TestDir};
use super::{ctx, ffmpeg, tool, SINE, TESTSRC};

/// Runs `tool` with `params` on `src` (in `dir`) as the app would plan it.
/// Checks that the source didn't change, that no temp file was left, and
/// that a failed job wrote nothing.
fn run_tool(
    dir: &TestDir,
    src: &Path,
    tool: ToolKey,
    params: ToolParams,
) -> Result<JobOutcome, GrapeError> {
    let settings = Settings::new(dir.join("fallback"));
    let sel = grape_engines::build_selection(&[src.to_path_buf()], &settings);
    let op = Operation::Tool { tool, params };
    let job = grape_core::plan(&sel, &op, &settings)
        .unwrap()
        .jobs
        .remove(0);
    let before = SourceState::of(src);
    let result = run_job(&job, &ctx(), &mut |_| {});
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
    if result.is_err() {
        assert!(!planned_file(&job).exists());
    }
    result
}

/// `ffprobe -show_entries <entries> -of flat` on `path`, as key=value
/// pairs with quotes removed.
fn flat(path: &Path, entries: &str) -> HashMap<String, String> {
    let out = Command::new(tool("GRAPE_TEST_FFPROBE"))
        .args(["-v", "error", "-show_entries", entries, "-of", "flat"])
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

/// Every tag, stream kind and chapter of `path`.
fn tags(path: &Path) -> HashMap<String, String> {
    flat(
        path,
        "format_tags:stream=index,codec_type,codec_name,codec_tag_string:stream_tags\
         :stream_disposition=attached_pic:chapter=id",
    )
}

/// The `codec_type` of each stream of `path`, in order.
fn kinds(path: &Path) -> Vec<String> {
    let tags = tags(path);
    (0..)
        .map_while(|i| tags.get(&format!("streams.stream.{i}.codec_type")).cloned())
        .collect()
}

/// How many chapters `path` has.
fn chapters(path: &Path) -> usize {
    tags(path)
        .keys()
        .filter(|k| k.starts_with("chapters.chapter.") && k.ends_with(".id"))
        .count()
}

/// The time (in the stream's time base) and size of every packet of
/// `path`'s stream `spec`.
fn packets(path: &Path, spec: &str) -> Vec<(i64, u64)> {
    let out = Command::new(tool("GRAPE_TEST_FFPROBE"))
        .args(["-v", "error", "-select_streams", spec])
        .args(["-show_entries", "packet=pts,size", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once(','))
        .map(|(pts, size)| (pts.parse().unwrap(), size.parse().unwrap()))
        .collect()
}

/// True when the packets of each of `specs` are the same in `out` as in
/// `src`, byte counts and times, up to one shift of every time that the
/// first video packet says. FFmpeg 6.1 starts a copy without the audio
/// at the source's start time (the audio's encoder delay, 23 ms for
/// AAC), keeping every track in sync; 9.0 keeps the times as they are.
fn same_packets(src: &Path, out: &Path, specs: &[&str]) -> bool {
    let first = |path| packets(path, "v:0").first().map_or(0, |&(pts, _)| pts);
    let shift = first(out) - first(src);
    specs.iter().all(|spec| {
        let moved: Vec<(i64, u64)> = packets(out, spec)
            .into_iter()
            .map(|(pts, size)| (pts - shift, size))
            .collect();
        !moved.is_empty() && moved == packets(src, spec)
    })
}

/// `path` as a string, for an FFmpeg argv.
fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// An OGG whose Vorbis comments name two chapters, at 0 and 0.5 s: FFmpeg
/// reads `CHAPTERxxx` comments as chapters, so `-map_chapters` copies them
/// into a test source (Grape's build reads no chapter file).
fn chapter_source(dir: &TestDir) -> PathBuf {
    let path = dir.join("chapters.ogg");
    let mut args = SINE.to_vec();
    args.extend([
        "-c:a",
        "libvorbis",
        "-metadata",
        "CHAPTER001=00:00:00.000",
        "-metadata",
        "CHAPTER001NAME=One",
        "-metadata",
        "CHAPTER002=00:00:00.500",
        "-metadata",
        "CHAPTER002NAME=Two",
        "-f",
        "ogg",
        s(&path),
    ]);
    ffmpeg(&args);
    path
}

/// An EBML element: its ID, its size as an 8-byte number, its body.
fn element(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.push(0x01);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

/// An EBML unsigned integer element.
fn uint(id: &[u8], value: u64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}

/// Writes a Matroska file with one English SRT subtitle track holding
/// `cues` (start and duration in ms, text): Grape's build has no subtitle
/// encoder and reads no `.srt`, so test sources copy their subtitles from
/// this.
fn write_subtitles(path: &Path, cues: &[(i16, u64, &str)]) {
    let header = element(
        &[0x1A, 0x45, 0xDF, 0xA3],
        &[
            uint(&[0x42, 0x86], 1),
            uint(&[0x42, 0xF7], 1),
            uint(&[0x42, 0xF2], 4),
            uint(&[0x42, 0xF3], 8),
            element(&[0x42, 0x82], b"matroska"),
            uint(&[0x42, 0x87], 4),
            uint(&[0x42, 0x85], 2),
        ]
        .concat(),
    );
    let info = element(
        &[0x15, 0x49, 0xA9, 0x66],
        &[
            uint(&[0x2A, 0xD7, 0xB1], 1_000_000),
            element(&[0x4D, 0x80], b"grape-test"),
            element(&[0x57, 0x41], b"grape-test"),
        ]
        .concat(),
    );
    let track = element(
        &[0xAE],
        &[
            uint(&[0xD7], 1),
            uint(&[0x73, 0xC5], 1),
            uint(&[0x83], 0x11),
            element(&[0x86], b"S_TEXT/UTF8"),
            element(&[0x22, 0xB5, 0x9C], b"eng"),
        ]
        .concat(),
    );
    let mut cluster = uint(&[0xE7], 0);
    for &(start, duration, text) in cues {
        // Track 1, the time from the cluster's, no flags, the text.
        let mut block = vec![0x81];
        block.extend_from_slice(&start.to_be_bytes());
        block.push(0);
        block.extend_from_slice(text.as_bytes());
        let group = [element(&[0xA1], &block), uint(&[0x9B], duration)].concat();
        cluster.extend(element(&[0xA0], &group));
    }
    let segment = element(
        &[0x18, 0x53, 0x80, 0x67],
        &[
            info,
            element(&[0x16, 0x54, 0xAE, 0x6B], &track),
            element(&[0x1F, 0x43, 0xB6, 0x75], &cluster),
        ]
        .concat(),
    );
    fs::write(path, [header, segment].concat()).unwrap();
}

/// A 1 s MP4 with H.264, AAC, two chapters, a title and a creation time.
fn chaptered_mp4(dir: &TestDir, name: &str) -> PathBuf {
    let chapters = chapter_source(dir);
    let path = dir.join(name);
    let mut args = TESTSRC.to_vec();
    args.extend(SINE);
    args.extend([
        "-i",
        s(&chapters),
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-map_chapters",
        "2",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-metadata",
        "title=Trip",
        "-metadata",
        "creation_time=2024-05-01T10:20:30.000000Z",
        "-f",
        "mp4",
        s(&path),
    ]);
    ffmpeg(&args);
    path
}

/// An iPhone-style 1 s MOV: H.264, AAC, `com.apple.quicktime.*` make,
/// model and location keys, a creation time and a timecode track.
fn iphone_mov(dir: &TestDir) -> PathBuf {
    let path = dir.join("IMG_0001.MOV");
    let mut args = TESTSRC.to_vec();
    args.extend(SINE);
    args.extend([
        "-shortest",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-timecode",
        "01:00:00:00",
        "-movflags",
        "use_metadata_tags",
        "-metadata",
        "com.apple.quicktime.make=Apple",
        "-metadata",
        "com.apple.quicktime.model=iPhone 15 Pro",
        "-metadata",
        "com.apple.quicktime.location.ISO6709=+48.8577+002.2950+035.000/",
        "-metadata",
        "creation_time=2024-05-01T10:20:30.000000Z",
        "-f",
        "mov",
        s(&path),
    ]);
    ffmpeg(&args);
    path
}

/// A 1 s MKV: H.264, English and French AAC, English SRT subtitles and a
/// TrueType font attachment.
fn movie_mkv(dir: &TestDir, name: &str) -> PathBuf {
    let subtitles = dir.join("subtitles.mkv");
    write_subtitles(&subtitles, &[(0, 400, "Hello"), (500, 400, "Bye")]);
    let font = dir.join("font.ttf");
    fs::write(&font, b"not really a font").unwrap();
    let path = dir.join(name);
    let mut args = TESTSRC.to_vec();
    args.extend(SINE);
    args.extend(SINE);
    args.extend([
        "-i",
        s(&subtitles),
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-map",
        "2:a",
        "-map",
        "3:s",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-c:s",
        "copy",
        "-metadata:s:a:0",
        "language=eng",
        "-metadata:s:a:1",
        "language=fra",
        "-attach",
        s(&font),
        "-metadata:s:t",
        "mimetype=application/x-truetype-font",
        "-t",
        "1",
        "-f",
        "matroska",
        s(&path),
    ]);
    ffmpeg(&args);
    path
}

#[test]
fn remove_audio_keeps_every_other_track_and_the_dates() {
    let dir = TestDir::new("tools-mute");
    let mut failures = Vec::new();

    // MP4: the video packets as they were, the chapters, title and date.
    let src = chaptered_mp4(&dir, "trip.mp4");
    let out = run_tool(&dir, &src, ToolKey::MuteVideo, ToolParams::default()).unwrap();
    let muted = &out.outputs[0];
    assert_eq!(muted, &dir.join("trip Muted.mp4"));
    assert!(out.notes.is_empty());
    let got = tags(muted);
    let mut check = |ok: bool, what: String| {
        if !ok {
            failures.push(what);
        }
    };
    // The chapter track comes back as a data stream after the video.
    check(
        kinds(muted) == ["video", "data"],
        format!("mp4 kinds {:?}", kinds(muted)),
    );
    check(
        same_packets(&src, muted, &["v:0"]),
        "mp4 video packets differ".into(),
    );
    check(
        chapters(muted) == 2,
        format!("mp4 chapters {}", chapters(muted)),
    );
    check(
        got.get("format.tags.title").map(String::as_str) == Some("Trip"),
        format!("mp4 title {got:?}"),
    );
    check(
        got.get("format.tags.creation_time").map(String::as_str)
            == Some("2024-05-01T10:20:30.000000Z"),
        format!("mp4 creation_time {got:?}"),
    );

    // An iPhone MOV: its keys and date, and the timecode track rebuilt.
    let src = iphone_mov(&dir);
    let out = run_tool(&dir, &src, ToolKey::MuteVideo, ToolParams::default()).unwrap();
    let muted = &out.outputs[0];
    assert_eq!(muted, &dir.join("IMG_0001 Muted.MOV"));
    let got = tags(muted);
    for (key, value) in [
        ("com.apple.quicktime.make", "Apple"),
        ("com.apple.quicktime.model", "iPhone 15 Pro"),
        (
            "com.apple.quicktime.location.ISO6709",
            "+48.8577+002.2950+035.000/",
        ),
    ] {
        let key = format!("format.tags.{}", key.replace('.', "_"));
        check(
            got.get(&key).map(String::as_str) == Some(value),
            format!("mov {key}: {got:?}"),
        );
    }
    check(
        got.get("format.tags.creation_time")
            .is_some_and(|t| t.starts_with("2024-05-01T10:20:30")),
        format!("mov creation_time {got:?}"),
    );
    check(
        got.get("format.tags.major_brand").map(String::as_str) != Some("qt  ;qt  "),
        format!("mov brands doubled {got:?}"),
    );
    check(
        kinds(muted) == ["video", "data"],
        format!("mov kinds {:?}", kinds(muted)),
    );
    check(
        got.get("streams.stream.1.codec_tag_string")
            .map(String::as_str)
            == Some("tmcd"),
        format!("mov timecode {got:?}"),
    );

    // MKV: the subtitles and the font stay, both audio tracks go.
    let src = movie_mkv(&dir, "movie.mkv");
    let out = run_tool(&dir, &src, ToolKey::MuteVideo, ToolParams::default()).unwrap();
    let muted = &out.outputs[0];
    let got = tags(muted);
    check(
        kinds(muted) == ["video", "subtitle", "attachment"],
        format!("mkv kinds {:?}", kinds(muted)),
    );
    check(
        got.get("streams.stream.1.tags.language")
            .map(String::as_str)
            == Some("eng"),
        format!("mkv subtitle language {got:?}"),
    );
    check(
        got.get("streams.stream.2.tags.filename")
            .map(String::as_str)
            == Some("font.ttf"),
        format!("mkv font {got:?}"),
    );
    check(
        same_packets(&src, muted, &["v:0", "s:0"]),
        "mkv video or subtitle packets differ".into(),
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn remove_audio_leaves_a_silent_video_and_refuses_a_song() {
    let dir = TestDir::new("tools-mute-none");
    let silent = dir.join("silent.mp4");
    let mut args = TESTSRC.to_vec();
    args.extend([
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "mp4",
        s(&silent),
    ]);
    ffmpeg(&args);
    let err = run_tool(&dir, &silent, ToolKey::MuteVideo, ToolParams::default()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert_eq!(err.user_message, "silent.mp4 has no sound to remove.");

    let song = dir.join("song.mov");
    let mut args = SINE.to_vec();
    args.extend(["-c:a", "aac", "-f", "mov", s(&song)]);
    ffmpeg(&args);
    let err = run_tool(&dir, &song, ToolKey::MuteVideo, ToolParams::default()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
    assert_eq!(err.user_message, "song.mov has no video to keep.");
    assert_eq!(dir.names(), ["silent.mp4", "song.mov"]);
}
