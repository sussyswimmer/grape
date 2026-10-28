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
        // `-attach` names it after the last `/` only: on Windows the
        // name would be the whole path.
        "-metadata:s:t",
        "filename=font.ttf",
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
    let wave =
        |t: f64, channel: usize| amplitude * (std::f64::consts::TAU * tones[channel] * t).sin();
    write_wave(path, rate, tones.len(), 1.0, mask, sample, wave);
}

/// Writes a WAV of `seconds` at `rate` with `channels` channels whose
/// sample at time `t` on channel `c` is `wave(t, c)` (full scale ±1),
/// stored as `sample`, with the channel mask `mask` or, for `None`, a
/// plain header with no layout.
fn write_wave(
    path: &Path,
    rate: u32,
    channels: usize,
    seconds: f64,
    mask: Option<u32>,
    sample: Sample,
    wave: impl Fn(f64, usize) -> f64,
) {
    let bits: u16 = match sample {
        Sample::I16 => 16,
        Sample::I24 => 24,
        Sample::I32 | Sample::F32 => 32,
    };
    let mut data = Vec::new();
    let frames = (f64::from(rate) * seconds) as u32;
    for n in 0..frames {
        let t = f64::from(n) / f64::from(rate);
        for c in 0..channels {
            let v = wave(t, c).clamp(-1.0, 1.0);
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
    let channels = u16::try_from(channels).unwrap();
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

/// The integrated loudness and true peak of `path`'s first audio track,
/// as the test FFmpeg's `ebur128` measures them (mono as dual mono when
/// `dual`).
fn loudness(path: &Path, dual: bool) -> (f64, f64) {
    let filter = if dual {
        "ebur128=peak=true:framelog=quiet:dualmono=true"
    } else {
        "ebur128=peak=true:framelog=quiet"
    };
    let out = Command::new(tool("GRAPE_TEST_FFMPEG"))
        .args(["-nostdin", "-hide_banner", "-nostats", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-af", filter, "-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let summary = stderr.rsplit("Summary:").next().unwrap();
    let value = |key: &str| -> f64 {
        let line = summary
            .lines()
            .find(|l| l.trim_start().starts_with(key))
            .unwrap();
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    };
    (value("I:"), value("Peak:"))
}

/// Normalize Volume `params` for `target_lufs`.
fn to_lufs(target_lufs: i8) -> ToolParams {
    ToolParams {
        target_lufs,
        ..ToolParams::default()
    }
}

/// A 3 s stereo 44.1 kHz voice-like WAV: a quiet tone with a loud click
/// every half second, like the plosives that make a linear gain clip.
fn plosives(path: &Path) {
    write_wave(path, 44_100, 2, 3.0, None, Sample::I16, |t, _| {
        let tone = 0.03 * (std::f64::consts::TAU * 300.0 * t).sin();
        let click = if t % 0.5 < 0.003 {
            0.9 * (std::f64::consts::TAU * 1000.0 * t).sin()
        } else {
            0.0
        };
        tone + click
    });
}

/// A 3 s WAV of a 997 Hz tone at `db` dBFS, with `channels` channels.
fn steady(path: &Path, db: f64, channels: usize, rate: u32, sample: Sample) {
    let amplitude = 10f64.powf(db / 20.0);
    write_wave(path, rate, channels, 3.0, None, sample, |t, _| {
        amplitude * (std::f64::consts::TAU * 997.0 * t).sin()
    });
}

#[test]
fn normalize_volume_reaches_the_target_under_the_ceiling() {
    let dir = TestDir::new("tools-normalize");
    let mut failures = Vec::new();
    // (source, target, lossy, path the plan takes)
    let quiet = dir.join("quiet.wav");
    steady(&quiet, -40.0, 2, 48_000, Sample::I16);
    let peaky = dir.join("peaky.wav");
    plosives(&peaky);
    // Its clicks would pass the ceiling at a linear gain: the limiter runs.
    let (lufs, peak) = loudness(&peaky, false);
    assert!(peak + (-16.0 - lufs) > 2.0, "{lufs} LUFS, {peak} dBTP");
    let mp3 = dir.join("voice.mp3");
    ffmpeg(&[
        "-i",
        s(&peaky),
        "-c:a",
        "libmp3lame",
        "-q:a",
        "2",
        "-f",
        "mp3",
        s(&mp3),
    ]);
    let m4a = dir.join("voice.m4a");
    ffmpeg(&[
        "-i",
        s(&peaky),
        "-c:a",
        "aac",
        "-b:a",
        "192k",
        "-f",
        "ipod",
        s(&m4a),
    ]);
    let flac = dir.join("quiet.flac");
    ffmpeg(&["-i", s(&quiet), "-c:a", "flac", "-f", "flac", s(&flac)]);
    for (src, target, lossy) in [
        (&quiet, -16, false),
        (&quiet, -23, false),
        (&peaky, -16, false),
        (&flac, -16, false),
        (&mp3, -16, true),
        (&m4a, -16, true),
    ] {
        let name = src.file_name().unwrap().to_string_lossy();
        let out = match run_tool(&dir, src, ToolKey::NormalizeAudio, to_lufs(target)) {
            Ok(out) => out,
            Err(e) => {
                failures.push(format!("{name} at {target}: {e} ({})", e.detail));
                continue;
            }
        };
        let (lufs, peak) = loudness(&out.outputs[0], false);
        let (near, ceiling) = if lossy { (1.0, -2.0) } else { (0.5, -1.0) };
        if (lufs - f64::from(target)).abs() > near || peak > ceiling + 0.3 {
            failures.push(format!("{name} at {target}: {lufs} LUFS, {peak} dBTP"));
        }
        fs::remove_file(&out.outputs[0]).unwrap();
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn normalize_volume_keeps_the_format_and_leaves_alone_what_it_cant_help() {
    let dir = TestDir::new("tools-normalize-keep");
    // Mono is measured as dual mono: at -16 it's -19 measured plainly.
    let mono = dir.join("mono.wav");
    steady(&mono, -30.0, 1, 48_000, Sample::I16);
    let out = run_tool(&dir, &mono, ToolKey::NormalizeAudio, to_lufs(-16)).unwrap();
    let (dual, _) = loudness(&out.outputs[0], true);
    let (plain, _) = loudness(&out.outputs[0], false);
    assert!((dual + 16.0).abs() <= 0.5, "{dual}");
    assert!((plain + 19.0).abs() <= 0.5, "{plain}");

    // A 24-bit 96 kHz WAV and a 24-bit ALAC stay as they are, and lose
    // their ReplayGain.
    let deep = dir.join("deep.wav");
    steady(&deep, -30.0, 2, 96_000, Sample::I24);
    let alac = dir.join("deep.m4a");
    ffmpeg(&[
        "-i",
        s(&deep),
        "-c:a",
        "alac",
        "-sample_fmt",
        "s32p",
        "-metadata",
        "REPLAYGAIN_TRACK_GAIN=+4.00 dB",
        "-f",
        "ipod",
        s(&alac),
    ]);
    for (src, codec) in [(&deep, "pcm_s24le"), (&alac, "alac")] {
        let out = run_tool(&dir, src, ToolKey::NormalizeAudio, to_lufs(-16)).unwrap();
        let got = flat(
            &out.outputs[0],
            "stream=codec_name,sample_rate,bits_per_raw_sample:format_tags",
        );
        assert_eq!(got["streams.stream.0.codec_name"], codec);
        assert_eq!(got["streams.stream.0.sample_rate"], "96000");
        assert_eq!(got["streams.stream.0.bits_per_raw_sample"], "24");
        assert!(
            !got.keys()
                .any(|k| k.to_ascii_lowercase().contains("replaygain")),
            "{got:?}"
        );
    }

    // Silence and a file already at the target are left alone.
    let silent = dir.join("silent.wav");
    steady(&silent, -200.0, 2, 48_000, Sample::I16);
    let err = run_tool(&dir, &silent, ToolKey::NormalizeAudio, to_lufs(-16)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "silent.wav is too short or too quiet to measure its loudness."
    );
    let there = dir.join("there.wav");
    steady(&there, -30.0, 2, 48_000, Sample::I16);
    let (lufs, _) = loudness(&there, false);
    let at = dir.join("at.wav");
    // The tone's level that measures -23 LUFS.
    steady(&at, -30.0 + (-23.0 - lufs), 2, 48_000, Sample::I16);
    let err = run_tool(&dir, &at, ToolKey::NormalizeAudio, to_lufs(-23)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert_eq!(err.user_message, "at.wav is already at \u{2212}23 LUFS.");
}

/// The keys of `got` that hold a gain: ReplayGain, R128 or `iTunNORM`.
fn gain_keys(got: &HashMap<String, String>) -> Vec<String> {
    let mut keys: Vec<String> = got
        .keys()
        .filter(|k| {
            let k = k.to_ascii_lowercase();
            k.contains("replaygain") || k.contains("r128") || k.contains("itunnorm")
        })
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[test]
fn normalize_volume_drops_the_gain_tags_its_change_makes_wrong() {
    let dir = TestDir::new("tools-normalize-gain");
    let quiet = dir.join("quiet.wav");
    steady(&quiet, -30.0, 2, 48_000, Sample::I16);
    let mut failures = Vec::new();
    for (name, codec, muxer) in [
        ("song.flac", "flac", "flac"),
        ("song.mp3", "libmp3lame", "mp3"),
    ] {
        let src = dir.join(name);
        let mut args = vec!["-i", s(&quiet), "-c:a", codec];
        args.extend(["-metadata", "REPLAYGAIN_TRACK_GAIN=-3.20 dB"]);
        args.extend(["-metadata", "R128_TRACK_GAIN=-819"]);
        args.extend(["-metadata", "iTunNORM= 0000044C 0000044C"]);
        args.extend(["-metadata", "title=Song", "-f", muxer, s(&src)]);
        ffmpeg(&args);
        assert_eq!(gain_keys(&tags(&src)).len(), 3, "{name}: {:?}", tags(&src));
        let out = match run_tool(&dir, &src, ToolKey::NormalizeAudio, to_lufs(-16)) {
            Ok(out) => out,
            Err(e) => {
                failures.push(format!("{name}: {e} ({})", e.detail));
                continue;
            }
        };
        let got = tags(&out.outputs[0]);
        if !gain_keys(&got).is_empty() {
            failures.push(format!("{name}: kept {:?}", gain_keys(&got)));
        }
        if got.get("format.tags.title").map(String::as_str) != Some("Song") {
            failures.push(format!("{name}: title {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_cancel_in_normalizes_second_pass_leaves_nothing() {
    let dir = TestDir::new("tools-normalize-cancel");
    let src = dir.join("long.wav");
    steady(&src, -40.0, 2, 48_000, Sample::I16);
    // 60 s, so the write pass is still running when the cancel comes.
    let long = dir.join("long60.wav");
    ffmpeg(&[
        "-stream_loop",
        "19",
        "-i",
        s(&src),
        "-c:a",
        "pcm_s16le",
        "-f",
        "wav",
        s(&long),
    ]);
    let settings = Settings::new(dir.join("fallback"));
    let sel = grape_engines::build_selection(std::slice::from_ref(&long), &settings);
    let op = Operation::Tool {
        tool: ToolKey::NormalizeAudio,
        params: to_lufs(-16),
    };
    let job = grape_core::plan(&sel, &op, &settings)
        .unwrap()
        .jobs
        .remove(0);
    let before = SourceState::of(&long);
    let ctx = ctx();
    let app_side = ctx.cancel.clone();
    let err = run_job(&job, &ctx, &mut |f| {
        // Past the measure's quarter of the bar: the write is running.
        if f > 0.3 {
            app_side.cancel();
        }
    })
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{}", err.detail);
    before.assert_unchanged();
    assert!(!planned_file(&job).exists());
    assert_no_leftovers(&dir.path);
}

fn to_preset(compress: grape_core::CompressPreset) -> ToolParams {
    ToolParams {
        compress,
        ..ToolParams::DEFAULT
    }
}

/// Writes `seconds` of stereo white noise at 44.1 kHz, 16-bit: what
/// codecs can't make much smaller.
fn noise(path: &Path, seconds: f64) {
    let mut state = 0x2545_f491_u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        f64::from(state) / f64::from(u32::MAX) * 2.0 - 1.0
    };
    let mut samples = Vec::new();
    let frames = (44_100.0 * seconds) as usize;
    for _ in 0..frames * 2 {
        samples.push(next() * 0.5);
    }
    let frame = |t: f64| (t * 44_100.0).round() as usize;
    write_wave(path, 44_100, 2, seconds, None, Sample::I16, |t, c| {
        samples.get(frame(t) * 2 + c).copied().unwrap_or(0.0)
    });
}

/// The size of `path` in bytes.
fn size(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

#[test]
fn compress_shrinks_audio_and_keeps_its_cover_and_tags() {
    use grape_core::CompressPreset::{Balanced, High, Small};
    use grape_engines::Note;
    let dir = TestDir::new("tools-compress-audio");
    let cover = cover_png(&dir);
    // A 320k MP3 and a 24-bit 96 kHz ALAC, with a cover and tags.
    let deep = dir.join("deep.wav");
    steady(&deep, -20.0, 2, 96_000, Sample::I24);
    let mut failures = Vec::new();
    let mp3 = dir.join("song.mp3");
    let alac = dir.join("song.m4a");
    // MP3 goes to 48 kHz at most.
    for (path, codec, muxer) in [
        (
            &mp3,
            ["-c:a", "libmp3lame", "-b:a", "320k", "-ar", "48000"],
            "mp3",
        ),
        (
            &alac,
            ["-c:a", "alac", "-sample_fmt", "s32p", "-ar", "96000"],
            "ipod",
        ),
    ] {
        let mut args = vec!["-i", s(&deep), "-i", s(&cover)];
        args.extend(["-map", "0:a", "-map", "1:v", "-c:v", "png"]);
        args.extend(["-disposition:v:0", "attached_pic"]);
        args.extend(codec);
        args.extend(["-metadata", "title=Song", "-metadata", "artist=Band"]);
        args.extend(["-metadata", "REPLAYGAIN_TRACK_GAIN=-3.00 dB"]);
        args.extend(["-f", muxer, s(path)]);
        ffmpeg(&args);
    }
    // (source, preset, codec, rate): AAC is at most 48 kHz.
    for (src, preset, codec, rate) in [
        (&mp3, Balanced, "mp3", "48000"),
        (&mp3, Small, "mp3", "48000"),
        (&alac, Balanced, "aac", "48000"),
        (&alac, High, "aac", "48000"),
    ] {
        let out = match run_tool(&dir, src, ToolKey::Compress, to_preset(preset)) {
            Ok(out) => out,
            Err(e) => {
                failures.push(format!("{} {preset:?}: {e} ({})", s(src), e.detail));
                continue;
            }
        };
        let path = &out.outputs[0];
        let note = Note::Compressed {
            before: size(src),
            after: size(path),
        };
        assert_eq!(out.notes, [note], "{}", s(src));
        assert!(size(path) * 10 <= size(src) * 9);
        let got = tags(path);
        let want = [
            ("streams.stream.0.codec_type", "audio"),
            ("streams.stream.1.codec_type", "video"),
            ("streams.stream.1.disposition.attached_pic", "1"),
        ];
        for (key, value) in want {
            if got.get(key).map(String::as_str) != Some(value) {
                failures.push(format!("{} {preset:?}: {key} {:?}", s(src), got.get(key)));
            }
        }
        // The ipod muxer has no ReplayGain tag, so the M4A had none.
        let values: Vec<&str> = got.values().map(String::as_str).collect();
        let replaygain = if codec == "mp3" { "-3.00 dB" } else { "Band" };
        for kept in ["Song", "Band", replaygain] {
            if !values.contains(&kept) {
                failures.push(format!("{} {preset:?}: lost {kept}: {got:?}", s(src)));
            }
        }
        let stream = flat(path, "stream=codec_name,sample_rate");
        if stream["streams.stream.0.codec_name"] != codec
            || stream["streams.stream.0.sample_rate"] != rate
        {
            failures.push(format!("{} {preset:?}: {stream:?}", s(src)));
        }
        fs::remove_file(path).unwrap();
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn compress_makes_flac_of_a_wav_by_preset() {
    use grape_core::CompressPreset::{Balanced, High, Small};
    let dir = TestDir::new("tools-compress-flac");
    let deep = dir.join("take.wav");
    steady(&deep, -20.0, 2, 96_000, Sample::I24);
    let mut failures = Vec::new();
    // (preset, sample rate, bits)
    for (preset, rate, bits) in [
        (Small, "48000", "16"),
        (Balanced, "48000", "24"),
        (High, "96000", "24"),
    ] {
        let out = match run_tool(&dir, &deep, ToolKey::Compress, to_preset(preset)) {
            Ok(out) => out,
            Err(e) => {
                failures.push(format!("{preset:?}: {e} ({})", e.detail));
                continue;
            }
        };
        let path = &out.outputs[0];
        assert_eq!(path.file_name().unwrap(), "take Compressed.flac");
        let got = flat(path, "stream=codec_name,sample_rate,bits_per_raw_sample");
        let want = [
            ("flac", "codec_name"),
            (rate, "sample_rate"),
            (bits, "bits_per_raw_sample"),
        ];
        for (value, key) in want {
            if got[&format!("streams.stream.0.{key}")] != value {
                failures.push(format!("{preset:?}: {key} {got:?}"));
            }
        }
        fs::remove_file(path).unwrap();
    }
    assert!(failures.is_empty(), "{failures:#?}");

    // Float, or more than FLAC's 24 bits: refused.
    for sample in [Sample::F32, Sample::I32] {
        let wav = dir.join("float.wav");
        write_tones(&wav, 48_000, &[440.0, 660.0], 0.25, None, sample);
        let err = run_tool(&dir, &wav, ToolKey::Compress, to_preset(Balanced)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
        assert_eq!(
            err.user_message,
            "float.wav is a 32-bit recording. FLAC would clip or cut it, so nothing was saved."
        );
    }
}

#[test]
fn compress_saves_nothing_when_it_cant_save_enough() {
    use grape_core::CompressPreset::{Balanced, Small};
    let dir = TestDir::new("tools-compress-none");
    // A 96k stereo MP3 is no bigger than Balanced or Small makes it (a
    // mono one would be: their averages are half for mono).
    let mp3 = dir.join("small.mp3");
    let mut args = SINE.to_vec();
    args.extend(["-ac", "2", "-c:a", "libmp3lame", "-b:a", "96k"]);
    args.extend(["-f", "mp3", s(&mp3)]);
    ffmpeg(&args);
    for preset in [Balanced, Small] {
        let err = run_tool(&dir, &mp3, ToolKey::Compress, to_preset(preset)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
        assert_eq!(
            err.user_message,
            "small.mp3 is already as small as Grape makes it."
        );
    }
    // White noise as 16-bit FLAC: FLAC can't make it 5 % smaller again.
    let wav = dir.join("noise.wav");
    noise(&wav, 2.0);
    let flac = dir.join("noise.flac");
    ffmpeg(&["-i", s(&wav), "-c:a", "flac", "-f", "flac", s(&flac)]);
    let err = run_tool(&dir, &flac, ToolKey::Compress, to_preset(Balanced)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "noise.flac is already as small as Grape makes it."
    );
}

/// A 1 s 1920×1080 60 fps H.264 `name` of format `muxer`, made big
/// (`-crf 5`), with stereo AAC at 256k, two chapters, a title, a creation
/// time, and the video options `video` (color tags, say).
fn camera(dir: &TestDir, name: &str, muxer: &str, video: &[&str]) -> PathBuf {
    let chapters = chapter_source(dir);
    let path = dir.join(name);
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=1920x1080:rate=60:duration=1",
    ];
    args.extend(SINE);
    args.extend(["-i", s(&chapters), "-map", "0:v", "-map", "1:a"]);
    args.extend(["-map_chapters", "2", "-c:v", "libx264", "-crf", "5"]);
    args.extend(["-preset", "ultrafast", "-pix_fmt", "yuv420p"]);
    args.extend(video);
    args.extend(["-c:a", "aac", "-ac", "2", "-b:a", "256k"]);
    args.extend(["-metadata", "title=Trip"]);
    args.extend(["-metadata", "creation_time=2024-05-01T10:20:30.000000Z"]);
    args.extend(["-f", muxer, s(&path)]);
    ffmpeg(&args);
    path
}

/// What `ffprobe` says of `path`'s video and first audio track, and its
/// tags.
fn picture_of(path: &Path) -> HashMap<String, String> {
    flat(
        path,
        "stream=codec_type,codec_name,width,height,avg_frame_rate,color_transfer,\
         color_primaries,bit_rate,sample_rate:stream_side_data=rotation:format_tags",
    )
}

#[test]
fn compress_shrinks_video_by_preset_and_keeps_chapters_and_dates() {
    use grape_core::CompressPreset::{Balanced, High, Small};
    use grape_engines::Note;
    let dir = TestDir::new("tools-compress-video");
    let mut failures = Vec::new();
    for muxer in ["mp4", "mov", "matroska"] {
        let ext = if muxer == "matroska" { "mkv" } else { muxer };
        let src = camera(&dir, &format!("camera.{ext}"), muxer, &[]);
        // (preset, width, height, fps)
        for (preset, w, h, fps) in [
            (Small, "1280", "720", "30/1"),
            (Balanced, "1920", "1080", "60/1"),
            (High, "1920", "1080", "60/1"),
        ] {
            let what = format!("{ext} {preset:?}");
            let out = match run_tool(&dir, &src, ToolKey::Compress, to_preset(preset)) {
                Ok(out) => out,
                Err(e) => {
                    failures.push(format!("{what}: {e} ({})", e.detail));
                    continue;
                }
            };
            let path = &out.outputs[0];
            let note = Note::Compressed {
                before: size(&src),
                after: size(path),
            };
            assert_eq!(out.notes, [note], "{what}");
            assert!(size(path) * 10 <= size(&src) * 9, "{what}");
            let got = picture_of(path);
            let want = [
                ("streams.stream.0.codec_name", "h264"),
                ("streams.stream.0.width", w),
                ("streams.stream.0.height", h),
                ("streams.stream.0.avg_frame_rate", fps),
                ("streams.stream.1.codec_name", "aac"),
                ("format.tags.title", "Trip"),
            ];
            for (key, value) in want {
                if got.get(key).map(String::as_str) != Some(value) {
                    failures.push(format!("{what}: {key} {:?}", got.get(key)));
                }
            }
            let dated = got
                .iter()
                .any(|(k, v)| k.ends_with("creation_time") && v.starts_with("2024-05-01T10:20:30"));
            if !dated {
                failures.push(format!("{what}: no creation_time: {got:?}"));
            }
            if chapters(path) != 2 {
                failures.push(format!("{what}: {} chapters", chapters(path)));
            }
            fs::remove_file(path).unwrap();
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn compress_turns_rotated_video_upright_and_keeps_hlg() {
    use grape_core::CompressPreset::Small;
    let dir = TestDir::new("tools-compress-upright");
    let plain = camera(&dir, "plain.mp4", "mp4", &[]);
    let rotated = dir.join("rotated.mp4");
    ffmpeg(&[
        "-display_rotation:v:0",
        "90",
        "-i",
        s(&plain),
        "-c",
        "copy",
        "-f",
        "mp4",
        s(&rotated),
    ]);
    let out = run_tool(&dir, &rotated, ToolKey::Compress, to_preset(Small)).unwrap();
    let got = picture_of(&out.outputs[0]);
    assert_eq!(got["streams.stream.0.width"], "720");
    assert_eq!(got["streams.stream.0.height"], "1280");
    assert!(!got.keys().any(|k| k.contains("rotation")), "{got:?}");

    // HLG stays tagged as HLG; PQ is refused before anything is written.
    // The tags go into the H.264 itself: FFmpeg 9.0's CLI gives x264 the
    // frames' (none from lavfi) over `-color_trc`.
    let tagged = |transfer: u8| {
        format!(
            "h264_metadata=colour_primaries=9:transfer_characteristics={transfer}:\
             matrix_coefficients=9"
        )
    };
    let hlg_tags = tagged(18);
    let hlg = camera(&dir, "hlg.mov", "mov", &["-bsf:v", &hlg_tags]);
    let out = run_tool(&dir, &hlg, ToolKey::Compress, to_preset(Small)).unwrap();
    let got = picture_of(&out.outputs[0]);
    assert_eq!(got["streams.stream.0.color_transfer"], "arib-std-b67");
    assert_eq!(got["streams.stream.0.color_primaries"], "bt2020");
    let pq = dir.join("pq.mov");
    let mut args = TESTSRC.to_vec();
    let pq_tags = tagged(16);
    args.extend(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-bsf:v", &pq_tags]);
    args.extend(["-f", "mov", s(&pq)]);
    ffmpeg(&args);
    let err = run_tool(&dir, &pq, ToolKey::Compress, to_preset(Small)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
    assert_eq!(
        err.user_message,
        "pq.mov is HDR10 video, which Compress can't keep yet."
    );
}

#[test]
fn compress_keeps_every_track_of_a_movie_and_an_iphones_keys() {
    use grape_core::CompressPreset::Small;
    let dir = TestDir::new("tools-compress-tracks");
    let mkv = movie_mkv(&dir, "movie.mkv");
    let out = run_tool(&dir, &mkv, ToolKey::Compress, to_preset(Small)).unwrap();
    let path = &out.outputs[0];
    assert_eq!(
        kinds(path),
        ["video", "audio", "audio", "subtitle", "attachment"]
    );
    let got = tags(path);
    assert_eq!(got["streams.stream.1.tags.language"], "eng");
    assert_eq!(got["streams.stream.2.tags.language"], "fra");

    let mov = iphone_mov(&dir);
    let out = run_tool(&dir, &mov, ToolKey::Compress, to_preset(Small)).unwrap();
    let got = tags(&out.outputs[0]);
    let values: Vec<&str> = got.values().map(String::as_str).collect();
    // The timecode track is written again from the `timecode` tag.
    for kept in [
        "Apple",
        "iPhone 15 Pro",
        "+48.8577+002.2950+035.000/",
        "01:00:00:00",
    ] {
        assert!(values.contains(&kept), "lost {kept}: {got:?}");
    }
    assert_eq!(kinds(&out.outputs[0]), ["video", "audio", "data"]);
}

#[test]
fn compress_saves_nothing_from_a_video_it_cant_shrink() {
    use grape_core::CompressPreset::{High, Small};
    let dir = TestDir::new("tools-compress-video-none");
    // Already at CRF 40: CRF 20 makes it bigger.
    let tiny = dir.join("tiny.mp4");
    let mut args = TESTSRC.to_vec();
    args.extend(["-c:v", "libx264", "-crf", "40", "-pix_fmt", "yuv420p"]);
    args.extend(["-f", "mp4", s(&tiny)]);
    ffmpeg(&args);
    let err = run_tool(&dir, &tiny, ToolKey::Compress, to_preset(High)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    assert_eq!(err.user_message, "tiny.mp4 is already compact. Try Small.");
    let err = run_tool(&dir, &tiny, ToolKey::Compress, to_preset(Small)).unwrap_err();
    assert_eq!(
        err.user_message,
        "tiny.mp4 is already as small as Grape makes it."
    );
}

#[test]
fn a_broadcast_wave_keeps_its_time_reference_through_the_audio_tools() {
    let dir = TestDir::new("tools-bwf");
    let plain = dir.join("plain.wav");
    steady(&plain, -30.0, 2, 48_000, Sample::I24);
    let take = dir.join("take.wav");
    ffmpeg(&[
        "-i",
        s(&plain),
        "-c:a",
        "pcm_s24le",
        "-write_bext",
        "1",
        "-metadata",
        "time_reference=172800000",
        "-metadata",
        "coding_history=A=PCM,F=48000,W=24",
        "-f",
        "wav",
        s(&take),
    ]);
    for (tool, params) in [
        (ToolKey::NormalizeAudio, to_lufs(-16)),
        (
            ToolKey::AudioChannels,
            to_channels(grape_core::Channels::Mono),
        ),
    ] {
        let out = run_tool(&dir, &take, tool, params).unwrap();
        let got = flat(&out.outputs[0], "format_tags");
        assert_eq!(
            got.get("format.tags.time_reference").map(String::as_str),
            Some("172800000"),
            "{tool:?}: {got:?}"
        );
        fs::remove_file(&out.outputs[0]).unwrap();
    }
}

/// A 1 s H.264 and AAC clip with a PNG cover, written by the MP4 muxer:
/// FFmpeg's MOV muxer can't write cover art, so named `.mov` it stands for
/// another app's MOV holding one in its iTunes-style metadata.
fn covered_clip(dir: &TestDir, name: &str) -> PathBuf {
    let cover = cover_png(dir);
    let path = dir.join(name);
    let mut args = TESTSRC.to_vec();
    args.extend(SINE);
    args.extend(["-i", s(&cover), "-map", "0:v", "-map", "1:a", "-map", "2:v"]);
    args.extend(["-c:v:0", "libx264", "-pix_fmt", "yuv420p", "-c:v:1", "png"]);
    args.extend(["-disposition:v:1", "attached_pic", "-c:a", "aac"]);
    args.extend(["-f", "mp4", s(&path)]);
    ffmpeg(&args);
    path
}

#[test]
fn remove_audio_and_compress_keep_an_mp4s_cover_and_leave_out_a_movs() {
    use grape_core::CompressPreset::Small;
    let dir = TestDir::new("tools-cover");
    let mut failures = Vec::new();
    // (source, Remove Audio's streams, Compress's)
    for (name, muted, compressed) in [
        (
            "clip.mp4",
            &["video", "video"][..],
            &["video", "audio", "video"][..],
        ),
        ("clip.mov", &["video"], &["video", "audio"]),
    ] {
        let src = covered_clip(&dir, name);
        assert_eq!(kinds(&src), ["video", "audio", "video"], "{name}");
        for (tool, params, want) in [
            (ToolKey::MuteVideo, ToolParams::DEFAULT, muted),
            (ToolKey::Compress, to_preset(Small), compressed),
        ] {
            let out = match run_tool(&dir, &src, tool, params) {
                Ok(out) => out,
                Err(e) => {
                    failures.push(format!("{name} {tool:?}: {e} ({})", e.detail));
                    continue;
                }
            };
            let path = &out.outputs[0];
            if kinds(path) != want {
                failures.push(format!("{name} {tool:?}: kinds {:?}", kinds(path)));
            }
            // The MP4's cover stays cover art.
            let last = format!("streams.stream.{}.disposition.attached_pic", want.len() - 1);
            let cover = tags(path).get(&last).map(String::as_str) == Some("1");
            if cover != (name == "clip.mp4") {
                failures.push(format!("{name} {tool:?}: {last} {:?}", tags(path)));
            }
            fs::remove_file(path).unwrap();
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn compress_and_remove_audio_leave_out_an_mkvs_cover_image() {
    use grape_core::CompressPreset::Small;
    let dir = TestDir::new("tools-mkv-cover");
    let cover = cover_png(&dir);
    let mkv = dir.join("movie.mkv");
    let mut args = TESTSRC.to_vec();
    args.extend(SINE);
    args.extend(["-map", "0:v", "-map", "1:a", "-c:v", "libx264", "-pix_fmt"]);
    args.extend(["yuv420p", "-c:a", "aac", "-t", "1", "-attach", s(&cover)]);
    args.extend(["-metadata:s:t", "mimetype=image/png", "-metadata:s:t"]);
    args.extend(["filename=cover.png", "-f", "matroska", s(&mkv)]);
    ffmpeg(&args);
    assert_eq!(kinds(&mkv), ["video", "audio", "video"]);
    let out = run_tool(&dir, &mkv, ToolKey::Compress, to_preset(Small)).unwrap();
    assert_eq!(kinds(&out.outputs[0]), ["video", "audio"]);
    let out = run_tool(&dir, &mkv, ToolKey::MuteVideo, ToolParams::DEFAULT).unwrap();
    assert_eq!(kinds(&out.outputs[0]), ["video"]);
}
