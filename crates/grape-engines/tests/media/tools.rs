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
    // Side data (an AAC packet's skip samples) comes as lines of its own.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pts, size) = line.split_once(',')?;
            Some((pts.parse().ok()?, size.parse().ok()?))
        })
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

/// What's left in `path` that Remove Metadata's output may not keep: a
/// tag other than the structural ones (with FFmpeg's own values), a
/// chapter, or cover art.
fn left_over(path: &Path) -> Vec<String> {
    let got = tags(path);
    let mut left: Vec<String> = got
        .iter()
        .filter(|(key, value)| {
            let Some((_, name)) = key.split_once(".tags.") else {
                return key.starts_with("chapters.")
                    || (key.ends_with("disposition.attached_pic") && *value == "1");
            };
            let ok = match name.to_ascii_lowercase().as_str() {
                "major_brand" | "minor_version" | "compatible_brands" | "vendor_id"
                | "language" | "duration" | "filename" | "mimetype" => true,
                "handler_name" => matches!(
                    value.as_str(),
                    "VideoHandler" | "SoundHandler" | "SubtitleHandler" | "DataHandler"
                ),
                "encoder" => matches!(value.as_str(), "Lavf" | "Lavc"),
                _ => false,
            };
            !ok
        })
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    left.sort();
    left
}

/// A 64×64 PNG cover, made with the FFmpeg under test.
fn cover_png(dir: &TestDir) -> PathBuf {
    let path = dir.join("cover.png");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=64x64:duration=0.1",
        "-frames:v",
        "1",
        "-c:v",
        "png",
        "-f",
        "image2pipe",
        s(&path),
    ]);
    path
}

/// A 1 s song of format `muxer` with `codec`, a PNG cover (unless the
/// format has none) and title, artist, album and date tags.
fn tagged_song(dir: &TestDir, name: &str, codec: &str, muxer: &str) -> PathBuf {
    let cover = cover_png(dir);
    let path = dir.join(name);
    let mut args = SINE.to_vec();
    let with_cover = muxer != "wav";
    if with_cover {
        args.extend(["-i", s(&cover), "-map", "0:a", "-map", "1:v", "-c:v", "png"]);
        args.extend(["-disposition:v:0", "attached_pic"]);
    }
    args.extend(["-c:a", codec]);
    args.extend(["-metadata", "title=Song", "-metadata", "artist=Band"]);
    args.extend(["-metadata", "album=LP", "-metadata", "date=2023"]);
    if muxer == "mp3" {
        args.extend(["-id3v2_version", "3"]);
    }
    args.extend(["-f", muxer, s(&path)]);
    ffmpeg(&args);
    path
}

#[test]
fn remove_metadata_strips_audio_files_and_says_what_went() {
    use grape_engines::MetaField::*;
    use grape_engines::Note;
    let dir = TestDir::new("tools-strip-audio");
    let mut failures = Vec::new();
    for (name, codec, muxer, cover) in [
        ("song.mp3", "libmp3lame", "mp3", true),
        ("song.flac", "flac", "flac", true),
        ("song.m4a", "aac", "ipod", true),
        ("take.wav", "pcm_s24le", "wav", false),
    ] {
        let src = tagged_song(&dir, name, codec, muxer);
        let out = match run_tool(&dir, &src, ToolKey::RemoveMetadata, ToolParams::default()) {
            Ok(out) => out,
            Err(e) => {
                failures.push(format!("{name}: {e} ({})", e.detail));
                continue;
            }
        };
        let stripped = &out.outputs[0];
        let left = left_over(stripped);
        if !left.is_empty() {
            failures.push(format!("{name}: left {left:?}"));
        }
        if kinds(stripped) != ["audio"] {
            failures.push(format!("{name}: kinds {:?}", kinds(stripped)));
        }
        // Author, title, the date and the album, and the cover; the
        // encoder's name too where the muxer wrote one with its version.
        let Note::MetadataRemoved(fields) = &out.notes[0] else {
            panic!("{:?}", out.notes);
        };
        let mut want = vec![Author, Title, CreationDate];
        if cover {
            want.push(CoverArt);
        }
        want.push(Other);
        let reported: Vec<_> = fields.iter().filter(|f| **f != Software).copied().collect();
        if out.notes.len() != 1 || reported != want {
            failures.push(format!("{name}: notes {:?}", out.notes));
        }
        if same_packets_audio(&src, stripped) {
            continue;
        }
        failures.push(format!("{name}: audio packets differ"));
    }
    // MP3 has no ID3 tag at all: the file starts with a frame.
    let mp3 = fs::read(dir.join("song No Metadata.mp3")).unwrap();
    assert!(
        mp3.len() > 2 && mp3[0] == 0xFF && mp3[1] & 0xE0 == 0xE0,
        "{:?}",
        &mp3[..4]
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// True when the audio packets of `out` are `src`'s, byte counts and
/// times.
fn same_packets_audio(src: &Path, out: &Path) -> bool {
    let (a, b) = (packets(src, "a:0"), packets(out, "a:0"));
    !a.is_empty() && a == b
}

#[test]
fn remove_metadata_strips_video_and_keeps_tracks_languages_and_fonts() {
    use grape_engines::MetaField::*;
    use grape_engines::Note;
    let dir = TestDir::new("tools-strip-video");
    let mut failures = Vec::new();

    // An MP4 with chapters, a cover, a location and a rotation.
    let plain = chaptered_mp4(&dir, "plain.mp4");
    let cover = cover_png(&dir);
    let src = dir.join("trip.mp4");
    ffmpeg(&[
        "-display_rotation:v:0",
        "90",
        "-i",
        s(&plain),
        "-i",
        s(&cover),
        "-map",
        "0",
        "-map",
        "1",
        "-c",
        "copy",
        "-c:v:1",
        "png",
        "-disposition:v:1",
        "attached_pic",
        "-metadata",
        "location=+48.8577+002.2950/",
        "-metadata",
        "artist=Ann",
        // The remux drops it, as it would any copy's.
        "-metadata",
        "creation_time=2024-05-01T10:20:30.000000Z",
        "-metadata:s:a:0",
        "language=eng",
        "-f",
        "mp4",
        s(&src),
    ]);
    fs::remove_file(&plain).unwrap();
    let out = run_tool(&dir, &src, ToolKey::RemoveMetadata, ToolParams::default()).unwrap();
    let stripped = &out.outputs[0];
    let left = left_over(stripped);
    let got = tags(stripped);
    let mut check = |ok: bool, what: String| {
        if !ok {
            failures.push(what);
        }
    };
    check(left.is_empty(), format!("mp4 left {left:?}"));
    check(
        kinds(stripped) == ["video", "audio"],
        format!("mp4 kinds {:?}", kinds(stripped)),
    );
    check(chapters(stripped) == 0, "mp4 chapters".into());
    check(
        got.get("streams.stream.1.tags.language")
            .map(String::as_str)
            == Some("eng"),
        format!("mp4 language {got:?}"),
    );
    let rotation = flat(stripped, "stream_side_data=rotation");
    check(
        rotation.values().any(|r| r == "-90" || r == "90"),
        format!("mp4 rotation {rotation:?}"),
    );
    check(
        same_packets(&src, stripped, &["v:0", "a:0"]),
        "mp4 packets differ".into(),
    );
    let Note::MetadataRemoved(fields) = &out.notes[0] else {
        panic!("{:?}", out.notes);
    };
    check(
        *fields
            == [
                Location,
                Author,
                Title,
                Software,
                CreationDate,
                CoverArt,
                Chapters,
            ],
        format!("mp4 note {fields:?}"),
    );

    // An iPhone MOV: its keys and timecode go.
    let src = iphone_mov(&dir);
    let out = run_tool(&dir, &src, ToolKey::RemoveMetadata, ToolParams::default()).unwrap();
    let stripped = &out.outputs[0];
    let left = left_over(stripped);
    check(left.is_empty(), format!("mov left {left:?}"));
    check(
        kinds(stripped) == ["video", "audio"],
        format!("mov kinds {:?}", kinds(stripped)),
    );
    let Note::MetadataRemoved(fields) = &out.notes[0] else {
        panic!("{:?}", out.notes);
    };
    check(
        *fields == [Location, Camera, Software, CreationDate],
        format!("mov note {fields:?}"),
    );

    // An MKV: both languages and the font stay, its title goes.
    let src = movie_mkv(&dir, "movie.mkv");
    let out = run_tool(&dir, &src, ToolKey::RemoveMetadata, ToolParams::default()).unwrap();
    let stripped = &out.outputs[0];
    let left = left_over(stripped);
    let got = tags(stripped);
    check(left.is_empty(), format!("mkv left {left:?}"));
    check(
        kinds(stripped) == ["video", "audio", "audio", "subtitle", "attachment"],
        format!("mkv kinds {:?}", kinds(stripped)),
    );
    for (index, language) in [(1, "eng"), (2, "fra"), (3, "eng")] {
        let key = format!("streams.stream.{index}.tags.language");
        check(
            got.get(&key).map(String::as_str) == Some(language),
            format!("mkv {key}: {got:?}"),
        );
    }
    check(
        got.get("streams.stream.4.tags.filename")
            .map(String::as_str)
            == Some("font.ttf"),
        format!("mkv font {got:?}"),
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// How the samples of a test WAV are stored.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Sample {
    I16,
    I24,
    I32,
    F32,
}

/// Writes a 1 s WAV at `rate` whose channels are sines at `tones`, each at
/// `amplitude` of full scale, stored as `sample`, with the channel mask
/// `mask` (a `WAVEFORMATEXTENSIBLE` header) or, for `None`, a plain
/// header with no layout.
fn write_tones(
    path: &Path,
    rate: u32,
    tones: &[f64],
    amplitude: f64,
    mask: Option<u32>,
    sample: Sample,
) {
    let bits: u16 = match sample {
        Sample::I16 => 16,
        Sample::I24 => 24,
        Sample::I32 | Sample::F32 => 32,
    };
    let mut data = Vec::new();
    for n in 0..rate {
        let t = f64::from(n) / f64::from(rate);
        for tone in tones {
            let v = amplitude * (std::f64::consts::TAU * tone * t).sin();
            match sample {
                Sample::I16 => data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes()),
                Sample::I24 => {
                    data.extend_from_slice(&((v * 8_388_607.0) as i32).to_le_bytes()[..3])
                }
                Sample::I32 => {
                    data.extend_from_slice(&((v * 2_147_483_647.0) as i32).to_le_bytes())
                }
                Sample::F32 => data.extend_from_slice(&(v as f32).to_le_bytes()),
            }
        }
    }
    let channels = u16::try_from(tones.len()).unwrap();
    let block = channels * bits / 8;
    let float = sample == Sample::F32;
    let mut format = Vec::new();
    let tag: u16 = match (mask, float) {
        (Some(_), _) => 0xFFFE,
        (None, true) => 3,
        (None, false) => 1,
    };
    format.extend_from_slice(&tag.to_le_bytes());
    format.extend_from_slice(&channels.to_le_bytes());
    format.extend_from_slice(&rate.to_le_bytes());
    format.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    format.extend_from_slice(&block.to_le_bytes());
    format.extend_from_slice(&bits.to_le_bytes());
    if let Some(mask) = mask {
        // The valid bits, the mask, and the PCM or float subformat GUID.
        format.extend_from_slice(&22u16.to_le_bytes());
        format.extend_from_slice(&bits.to_le_bytes());
        format.extend_from_slice(&mask.to_le_bytes());
        format.push(if float { 3 } else { 1 });
        format.extend_from_slice(&[
            0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
    }
    let mut body = b"WAVEfmt ".to_vec();
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

/// The first audio track of `path`, decoded by the test FFmpeg to 32-bit
/// float: each channel's samples, and the rate.
fn decode(path: &Path) -> (Vec<Vec<f64>>, f64) {
    let wav = path.with_extension("decoded.wav");
    ffmpeg(&[
        "-i",
        s(path),
        "-map",
        "0:a:0",
        "-c:a",
        "pcm_f32le",
        "-f",
        "wav",
        s(&wav),
    ]);
    let bytes = fs::read(&wav).unwrap();
    fs::remove_file(&wav).unwrap();
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
    for (i, sample) in data.as_chunks::<4>().0.iter().enumerate() {
        samples[i % channels].push(f64::from(f32::from_le_bytes(*sample)));
    }
    (samples, rate)
}

/// The amplitude of `tone` (Hz) in `samples` at `rate`, over half a
/// second (Goertzel; every test tone fits it a whole number of times).
fn level(samples: &[f64], rate: f64, tone: f64) -> f64 {
    let n = (rate / 2.0) as usize;
    let k = 2.0 * (std::f64::consts::TAU * tone / rate).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in samples.iter().take(n) {
        let s0 = x + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - k * s1 * s2;
    2.0 * power.max(0.0).sqrt() / n as f64
}

/// The tones of the channel-mix tests, one a channel, each a whole number
/// of cycles in half a second at 48 and 96 kHz.
const MIX_TONES: [f64; 6] = [440.0, 660.0, 880.0, 1100.0, 1320.0, 1540.0];

/// Channels `params` for `channels`.
fn to_channels(channels: grape_core::Channels) -> ToolParams {
    ToolParams {
        channels,
        ..ToolParams::default()
    }
}

/// Runs Audio Channels on a WAV of `tones` (at `amplitude`, with `mask`)
/// and returns, for each output channel, the level of each tone relative
/// to `amplitude`.
fn mixed(
    dir: &TestDir,
    name: &str,
    tones: &[f64],
    mask: Option<u32>,
    channels: grape_core::Channels,
) -> Vec<Vec<f64>> {
    let src = dir.join(name);
    write_tones(&src, 48_000, tones, 0.25, mask, Sample::I16);
    let out = run_tool(dir, &src, ToolKey::AudioChannels, to_channels(channels))
        .unwrap_or_else(|e| panic!("{name}: {e} ({})", e.detail));
    let (samples, rate) = decode(&out.outputs[0]);
    samples
        .iter()
        .map(|channel| {
            tones
                .iter()
                .map(|&tone| (level(channel, rate, tone) / 0.25 * 1000.0).round() / 1000.0)
                .collect()
        })
        .collect()
}

/// Asserts that every level in `got` is within 0.01 of `want`'s.
fn assert_levels(got: &[Vec<f64>], want: &[&[f64]], what: &str) {
    let close = got.len() == want.len()
        && got.iter().zip(want).all(|(g, w)| {
            g.len() == w.len() && g.iter().zip(*w).all(|(a, b)| (a - b).abs() <= 0.01)
        });
    assert!(close, "{what}: got {got:?}, want {want:?}");
}

#[test]
fn audio_channels_mix_at_the_documented_levels() {
    use grape_core::Channels::{Mono, Stereo};
    let dir = TestDir::new("tools-channels-mix");
    // Stereo to mono is (L+R)/2; mono to stereo a full-level copy.
    let got = mixed(&dir, "lr.wav", &MIX_TONES[..2], None, Mono);
    assert_levels(&got, &[&[0.5, 0.5]], "stereo to mono");
    let got = mixed(&dir, "mono.wav", &MIX_TONES[..1], None, Stereo);
    assert_levels(&got, &[&[1.0], &[1.0]], "mono to stereo");
    // 5.1(side): FL FR FC LFE SL SR, through swresample's normalized
    // matrix.
    let got = mixed(&dir, "surround.wav", &MIX_TONES, Some(0x60F), Stereo);
    let left: &[f64] = &[0.414, 0.0, 0.293, 0.0, 0.293, 0.0];
    let right: &[f64] = &[0.0, 0.414, 0.293, 0.0, 0.0, 0.293];
    assert_levels(&got, &[left, right], "5.1 to stereo");
    let got = mixed(&dir, "surround2.wav", &MIX_TONES, Some(0x60F), Mono);
    let mono: &[f64] = &[0.207, 0.207, 0.293, 0.0, 0.146, 0.146];
    assert_levels(&got, &[mono], "5.1 to mono");
    // Four channels without a layout average to mono, and can't be
    // stereo.
    let got = mixed(&dir, "four.wav", &MIX_TONES[..4], None, Mono);
    assert_levels(&got, &[&[0.25; 4]], "4 unlabeled to mono");
    let four = dir.join("four2.wav");
    write_tones(&four, 48_000, &MIX_TONES[..4], 0.25, None, Sample::I16);
    let err = run_tool(&dir, &four, ToolKey::AudioChannels, to_channels(Stereo)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "four2.wav has 4 channels without speaker positions, so Grape can't tell which are \
         left and right."
    );
    let err = run_tool(
        &dir,
        &dir.join("mono.wav"),
        ToolKey::AudioChannels,
        to_channels(Mono),
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert_eq!(err.user_message, "mono.wav is already mono.");
}

#[test]
fn audio_channels_keep_codec_depth_rate_and_cover_and_never_clip() {
    use grape_core::Channels::{Mono, Stereo};
    let dir = TestDir::new("tools-channels-keep");
    // Full-scale correlated 5.1 in AAC to stereo: with a plain `-ac 2` it
    // would reach 1.9 of full scale. (5.1 with back speakers, AAC's own
    // layout: FFmpeg 6.1 reads back 5.1(side) AAC as having none.)
    let wav = dir.join("loud51.wav");
    write_tones(&wav, 48_000, &[440.0; 6], 0.8, Some(0x3F), Sample::I16);
    let m4a = dir.join("loud51.m4a");
    ffmpeg(&[
        "-i",
        s(&wav),
        "-c:a",
        "aac",
        "-b:a",
        "384k",
        "-f",
        "ipod",
        s(&m4a),
    ]);
    let out = run_tool(&dir, &m4a, ToolKey::AudioChannels, to_channels(Stereo)).unwrap();
    let (samples, _) = decode(&out.outputs[0]);
    let peak = samples.iter().flatten().fold(0.0f64, |m, x| m.max(x.abs()));
    assert_eq!(samples.len(), 2);
    assert!(peak <= 1.0, "peak {peak}");
    assert!(peak > 0.6, "peak {peak}");
    let got = flat(&out.outputs[0], "stream=codec_name");
    assert_eq!(got["streams.stream.0.codec_name"], "aac");

    // Each source's codec, depth and rate stay.
    let mut failures = Vec::new();
    let s24 = dir.join("take24.wav");
    write_tones(&s24, 96_000, &MIX_TONES[..2], 0.25, None, Sample::I24);
    let f32 = dir.join("takef.wav");
    write_tones(&f32, 48_000, &MIX_TONES[..2], 0.25, None, Sample::F32);
    let alac = dir.join("song.m4a");
    ffmpeg(&[
        "-i",
        s(&s24),
        "-c:a",
        "alac",
        "-sample_fmt",
        "s32p",
        "-metadata",
        "creation_time=2024-05-01T10:20:30.000000Z",
        "-f",
        "ipod",
        s(&alac),
    ]);
    for (src, codec, raw, rate) in [
        (&s24, "pcm_s24le", "24", "96000"),
        (&f32, "pcm_f32le", "N/A", "48000"),
        (&alac, "alac", "24", "96000"),
    ] {
        let out = match run_tool(&dir, src, ToolKey::AudioChannels, to_channels(Mono)) {
            Ok(out) => out,
            Err(e) => {
                failures.push(format!("{}: {e} ({})", src.display(), e.detail));
                continue;
            }
        };
        let got = flat(
            &out.outputs[0],
            "stream=codec_name,channels,sample_rate,bits_per_raw_sample",
        );
        let want = [
            ("codec_name", codec),
            ("channels", "1"),
            ("sample_rate", rate),
            ("bits_per_raw_sample", raw),
        ];
        for (key, value) in want {
            let key = format!("streams.stream.0.{key}");
            if got.get(&key).map(String::as_str) != Some(value) {
                failures.push(format!("{}: {key} {:?}", src.display(), got.get(&key)));
            }
        }
        // An M4A keeps the date FFmpeg drops from every new file.
        if src == &alac {
            let got = tags(&out.outputs[0]);
            let date = got.get("format.tags.creation_time").map(String::as_str);
            if date != Some("2024-05-01T10:20:30.000000Z") {
                failures.push(format!("m4a creation_time {got:?}"));
            }
        }
    }

    // A mono MP3 and FLAC made stereo keep their cover; ReplayGain goes.
    for (name, codec, muxer) in [
        ("song.mp3", "libmp3lame", "mp3"),
        ("song.flac", "flac", "flac"),
    ] {
        let src = tagged_song(&dir, name, codec, muxer);
        let tagged = dir.join(&format!("rg-{name}"));
        ffmpeg(&[
            "-i",
            s(&src),
            "-map",
            "0",
            "-c",
            "copy",
            "-metadata",
            "REPLAYGAIN_TRACK_GAIN=-3.20 dB",
            "-f",
            muxer,
            s(&tagged),
        ]);
        let out = run_tool(&dir, &tagged, ToolKey::AudioChannels, to_channels(Stereo)).unwrap();
        let got = tags(&out.outputs[0]);
        if kinds(&out.outputs[0]) != ["audio", "video"]
            || got
                .get("streams.stream.1.disposition.attached_pic")
                .map(String::as_str)
                != Some("1")
        {
            failures.push(format!("{name}: cover {got:?}"));
        }
        if got
            .keys()
            .any(|k| k.to_ascii_lowercase().contains("replaygain"))
        {
            failures.push(format!("{name}: ReplayGain kept {got:?}"));
        }
        if got.get("format.tags.title").map(String::as_str) != Some("Song") {
            failures.push(format!("{name}: title {got:?}"));
        }
    }

    // A 32-bit FLAC (written in FLAC's experimental mode) would come out
    // 24-bit, so it's refused.
    let s32 = dir.join("take32.wav");
    write_tones(&s32, 48_000, &MIX_TONES[..2], 0.3, None, Sample::I32);
    let flac32 = dir.join("take32.flac");
    ffmpeg(&[
        "-i",
        s(&s32),
        "-c:a",
        "flac",
        "-sample_fmt",
        "s32",
        "-strict",
        "experimental",
        "-f",
        "flac",
        s(&flac32),
    ]);
    let err = run_tool(&dir, &flac32, ToolKey::AudioChannels, to_channels(Mono)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "take32.flac is a 32-bit recording. FLAC would cut it, so nothing was saved."
    );
    // A 32-bit WAV stays a 32-bit WAV.
    let out = run_tool(&dir, &s32, ToolKey::AudioChannels, to_channels(Mono)).unwrap();
    let got = flat(&out.outputs[0], "stream=codec_name");
    assert_eq!(got["streams.stream.0.codec_name"], "pcm_s32le");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
