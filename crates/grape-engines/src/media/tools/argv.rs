//! The tools' `ffmpeg` argv (D26), pure: from the probe, the tags probe and
//! the panel's choices.
//!
//! Every argv has a conversion's shape ([`crate::media::args`]): the common
//! prefix (both whitelists, the source as a `file:` URL), the stream maps
//! and codecs, then `-progress pipe:1`, a forced `-f` and the temp output.
//! Streams are mapped by their index (`-map 0:<i>`), never with `0:V` or
//! `0:a`, so every track but what the tool removes is kept.

use std::ffi::OsString;
use std::path::Path;

use grape_core::Format;

use crate::media::args::{finish, muxer, prefix_at, push};
use crate::media::probe::{Probe, StreamInfo, StreamKind};
use crate::media::tags::Tags;

/// Maps the source's stream `index` into the output.
fn map(args: &mut Vec<OsString>, index: u32) {
    push(args, &["-map", &format!("0:{index}")]);
}

/// A timed-text subtitle, the only subtitle MP4 and MOV keep as it is.
fn is_mov_text(stream: &StreamInfo) -> bool {
    stream.kind == StreamKind::Subtitle && stream.codec == "mov_text"
}

/// The muxer of a tool's output. Every tool writes a format FFmpeg writes,
/// so this is never empty ([`super::tests`] checks every pair).
fn muxer_of(to: Format) -> &'static str {
    muxer(to).unwrap_or_default()
}

/// `KEEP`: what the tools that aren't about metadata append so the output
/// keeps what FFmpeg would otherwise drop or move.
///
/// - The source's `creation_time`: FFmpeg's CLI deletes it from the global
///   metadata it copies (`ffmpeg_mux_init.c`), and photo apps sort videos
///   by it.
/// - An iPhone's `com.apple.quicktime.*` keys (location, make, model,
///   date), in an MP4 or MOV only: `use_metadata_tags` writes them back as
///   keys, and the blanked brands stop them coming out twice. Never in an
///   M4A, where it would move the iTunes tags into keys.
/// - Otherwise each container's own options, as conversions have them:
///   `faststart` for MP4, MOV and M4A, ID3v2.3 for MP3, RF64 past 4 GiB for
///   WAV.
pub(super) fn keep(args: &mut Vec<OsString>, to: Format, tags: &Tags) {
    if let Some(time) = &tags.creation_time {
        push(args, &["-metadata", &format!("creation_time={time}")]);
    }
    match to {
        Format::Mp4 | Format::Mov if tags.apple_keys => push(
            args,
            &[
                "-movflags",
                "+faststart+use_metadata_tags",
                "-metadata",
                "major_brand=",
                "-metadata",
                "minor_version=",
                "-metadata",
                "compatible_brands=",
            ],
        ),
        Format::Mp4 | Format::Mov | Format::M4a => push(args, &["-movflags", "+faststart"]),
        Format::Mp3 => push(args, &["-id3v2_version", "3"]),
        Format::Wav => push(args, &["-rf64", "auto"]),
        _ => {}
    }
}

/// Remove Audio (`muteVideo`) on MP4, MOV and MKV: a stream copy of every
/// track but the audio.
///
/// - MP4 and MOV keep every video stream (cover art included) and every
///   `mov_text` subtitle, in the source's order. `-strict unofficial` lets
///   the muxer write Dolby Vision (`dvcC`/`dvvC`), stereo and spherical
///   boxes. The timecode track is rebuilt from the `timecode` tag, and the
///   chapter track from the chapters.
/// - MKV keeps every stream that isn't audio or data: video, subtitles and
///   attachments.
///
/// Not `-an`, which leaves the rest to FFmpeg's default selection (one
/// stream per kind), nor `-map 0 -map -0:a`, which copies MP4's chapter
/// track as well as rebuilding it.
pub(super) fn mute(
    src: &Path,
    to: Format,
    probe: &Probe,
    tags: &Tags,
    out: &Path,
) -> Vec<OsString> {
    let mut args = prefix_at(src, &[], "error");
    for stream in muted(to, probe) {
        map(&mut args, stream.index);
    }
    push(&mut args, &["-c", "copy"]);
    if matches!(to, Format::Mp4 | Format::Mov) {
        push(&mut args, &["-strict", "unofficial"]);
    }
    keep(&mut args, to, tags);
    finish(args, muxer_of(to), out)
}

/// The streams [`mute`] keeps.
pub(super) fn muted(to: Format, probe: &Probe) -> impl Iterator<Item = &StreamInfo> {
    probe.streams.iter().filter(move |s| match to {
        Format::Mkv => !matches!(s.kind, StreamKind::Audio | StreamKind::Data),
        _ => s.kind == StreamKind::Video || is_mov_text(s),
    })
}

/// Remove Metadata on MP3, M4A, WAV, FLAC, MP4, MOV and MKV: a stream
/// copy of what it keeps ([`stripped`]) with no tags, chapters or
/// encoder name (`-map_metadata -1 -map_chapters -1 -fflags +bitexact`).
///
/// - Each kept track's language is set again (`-metadata:s:<o>`), from
///   the tags probe's checked values only.
/// - An MKV font gets its own tags back (`-map_metadata:s:<o> 0:s:<i>`):
///   the muxer needs its file name and MIME type, and Grape never reads
///   them into an argv.
/// - MP4 and MOV: `-strict unofficial` keeps Dolby Vision, stereo and
///   spherical boxes; `-write_tmcd 0` drops the timecode, which is the
///   time of day it was filmed.
/// - MP3: `-id3v2_version 0` writes no ID3 tag at all (3 would write an
///   empty one); only the Xing frame stays.
/// - WAV: RF64 past 4 GiB; with `bitexact` it writes no INFO list.
pub(super) fn strip(
    src: &Path,
    to: Format,
    probe: &Probe,
    tags: &Tags,
    out: &Path,
) -> Vec<OsString> {
    let mut args = prefix_at(src, &[], "error");
    let kept: Vec<&StreamInfo> = stripped(to, probe, tags).collect();
    for stream in &kept {
        map(&mut args, stream.index);
    }
    push(
        &mut args,
        &[
            "-c",
            "copy",
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-fflags",
            "+bitexact",
        ],
    );
    for (o, stream) in kept.iter().enumerate() {
        if let Some(language) = tags.languages.get(&stream.index) {
            push(
                &mut args,
                &[&format!("-metadata:s:{o}"), &format!("language={language}")],
            );
        }
        if tags.font_streams.contains(&stream.index) {
            push(
                &mut args,
                &[
                    &format!("-map_metadata:s:{o}"),
                    &format!("0:s:{}", stream.index),
                ],
            );
        }
    }
    match to {
        Format::Mp4 | Format::Mov => push(
            &mut args,
            &[
                "-strict",
                "unofficial",
                "-movflags",
                "+faststart",
                "-write_tmcd",
                "0",
            ],
        ),
        Format::M4a => push(&mut args, &["-movflags", "+faststart"]),
        Format::Mp3 => push(&mut args, &["-id3v2_version", "0"]),
        Format::Wav => push(&mut args, &["-rf64", "auto"]),
        _ => {}
    }
    finish(args, muxer_of(to), out)
}

/// The streams [`strip`] keeps, in the source's order:
///
/// - MP4 and MOV: every video that isn't cover art, every audio track and
///   every `mov_text` subtitle;
/// - M4A: every audio track;
/// - MKV: every video that isn't cover art (an image attachment is cover
///   art to the demuxer), audio track and subtitle, and the fonts its
///   subtitles need;
/// - MP3, FLAC and WAV: the audio.
///
/// Cover art goes (its JPEG keeps the camera's EXIF, and Grape's FFmpeg
/// has no JPEG encoder to clean it), and so do data tracks and other
/// attachments.
pub(super) fn stripped<'a>(
    to: Format,
    probe: &'a Probe,
    tags: &'a Tags,
) -> impl Iterator<Item = &'a StreamInfo> {
    let first_audio = probe
        .streams
        .iter()
        .find(|s| s.kind == StreamKind::Audio)
        .map(|s| s.index);
    probe.streams.iter().filter(move |s| {
        let video = s.kind == StreamKind::Video && !s.attached_pic;
        let audio = s.kind == StreamKind::Audio;
        match to {
            Format::Mp4 | Format::Mov => video || audio || is_mov_text(s),
            Format::M4a => audio,
            Format::Mkv => {
                video
                    || audio
                    || s.kind == StreamKind::Subtitle
                    || (s.kind == StreamKind::Attachment && tags.font_streams.contains(&s.index))
            }
            _ => Some(s.index) == first_audio,
        }
    })
}

#[cfg(test)]
pub(super) mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use super::*;
    use crate::media::args::file_url;
    use crate::media::probe;

    /// A folder whose name has a '%', spaces, a leading '-' and non-ASCII
    /// letters, as conversions' argv tests use.
    fn dir() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"C:\Users\Zoë\100% -x y"
        } else {
            "/home/zoë/100% -x: y"
        })
    }

    pub(in crate::media::tools) fn src() -> PathBuf {
        dir().join("-clip ü%d.mp4")
    }

    pub(in crate::media::tools) fn out() -> PathBuf {
        dir().join(".grape-tmp-1-2-3.out")
    }

    /// `list` as an argv, with `<src>` and `<out>` replaced by those
    /// paths' URLs.
    pub(in crate::media::tools) fn argv(list: &[&str]) -> Vec<OsString> {
        list.iter()
            .map(|&arg| match arg {
                "<src>" => file_url(&src()),
                "<out>" => file_url(&out()),
                arg => OsString::from(arg),
            })
            .collect()
    }

    /// A tool argv at error level: the prefix with `reading`, `middle`,
    /// then progress, the muxer and the output.
    pub(in crate::media::tools) fn expect(
        reading: &[&str],
        middle: &[&str],
        muxer: &str,
    ) -> Vec<OsString> {
        let mut list = vec![
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-v",
            "error",
            "-y",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            "mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif",
        ];
        list.extend_from_slice(reading);
        list.extend_from_slice(&["-i", "<src>"]);
        list.extend_from_slice(middle);
        list.extend_from_slice(&["-progress", "pipe:1", "-f", muxer, "<out>"]);
        argv(&list)
    }

    /// One stream for a test probe.
    pub(in crate::media::tools) fn stream(index: u32, kind: StreamKind, codec: &str) -> StreamInfo {
        let audio = kind == StreamKind::Audio;
        StreamInfo {
            index,
            kind,
            codec: codec.to_owned(),
            tag: String::new(),
            attached_pic: false,
            bit_rate: None,
            channels: if audio { 2 } else { 0 },
            has_layout: true,
            sample_rate: if audio { 48_000 } else { 0 },
            depth: 16,
            sample_fmt: String::new(),
            color_transfer: String::new(),
            avg_fps: None,
        }
    }

    /// Cover art: a picture stream marked `attached_pic`.
    pub(in crate::media::tools) fn cover(index: u32) -> StreamInfo {
        StreamInfo {
            attached_pic: true,
            ..stream(index, StreamKind::Video, "mjpeg")
        }
    }

    /// A probe of `streams`, whose `video` and `audio` fields follow them
    /// as `probe::parse` sets them.
    pub(in crate::media::tools) fn probe_of(format_name: &str, streams: Vec<StreamInfo>) -> Probe {
        let video = streams
            .iter()
            .find(|s| s.kind == StreamKind::Video && !s.attached_pic)
            .map(|s| probe::Video {
                codec: s.codec.clone(),
                width: 1920,
                height: 1080,
                pix_fmt: "yuv420p".to_owned(),
                rotation: 0,
            });
        let audio = streams
            .iter()
            .find(|s| s.kind == StreamKind::Audio)
            .map(|s| probe::Audio {
                codec: s.codec.clone(),
                sample_rate: s.sample_rate,
                channels: s.channels,
                has_layout: s.has_layout,
                layout: String::new(),
                depth: s.depth,
            });
        Probe {
            format_name: format_name.to_owned(),
            duration: None,
            video,
            audio,
            b_frames: false,
            bit_rate: None,
            streams,
        }
    }

    /// An MP4 or MOV: H.264, English and French AAC, a `mov_text` and a DVD
    /// subtitle, a timecode track and a PNG cover.
    fn movie() -> Probe {
        probe_of(
            "mov,mp4,m4a,3gp,3g2,mj2",
            vec![
                stream(0, StreamKind::Video, "h264"),
                stream(1, StreamKind::Audio, "aac"),
                stream(2, StreamKind::Audio, "aac"),
                stream(3, StreamKind::Subtitle, "mov_text"),
                stream(4, StreamKind::Subtitle, "dvd_subtitle"),
                stream(5, StreamKind::Data, "none"),
                cover(6),
            ],
        )
    }

    /// An MKV: H.264, AAC, SRT, a font attachment, an image attachment
    /// (cover art to the demuxer) and a data stream.
    fn mkv() -> Probe {
        probe_of(
            "matroska,webm",
            vec![
                stream(0, StreamKind::Video, "h264"),
                stream(1, StreamKind::Audio, "aac"),
                stream(2, StreamKind::Subtitle, "subrip"),
                stream(3, StreamKind::Attachment, "ttf"),
                cover(4),
                stream(5, StreamKind::Data, "bin_data"),
            ],
        )
    }

    /// What the tags probe says of [`movie`]: languages, and everything to
    /// report.
    fn movie_tags() -> Tags {
        Tags {
            languages: BTreeMap::from([(1, "eng".to_owned()), (2, "fra".to_owned())]),
            ..dated(true)
        }
    }

    /// What the tags probe says of [`mkv`]: languages and the font.
    fn mkv_tags() -> Tags {
        Tags {
            languages: BTreeMap::from([(1, "deu".to_owned()), (2, "eng".to_owned())]),
            font_streams: BTreeSet::from([3]),
            ..Tags::default()
        }
    }

    /// An audio file with a cover, as MP3, FLAC and M4A have them.
    fn song(format_name: &str, codec: &str) -> Probe {
        probe_of(
            format_name,
            vec![stream(0, StreamKind::Audio, codec), cover(1)],
        )
    }

    /// Tags with a `creation_time`, and iPhone keys if `apple`.
    pub(in crate::media::tools) fn dated(apple: bool) -> Tags {
        Tags {
            creation_time: Some("2024-05-01T10:20:30.000000Z".to_owned()),
            apple_keys: apple,
            ..Tags::default()
        }
    }

    /// Every argv the tools build for the test sources, for
    /// [`every_tool_argv_keeps_the_invariants`].
    fn every_argv() -> Vec<Vec<OsString>> {
        let mut all = Vec::new();
        for tags in [Tags::default(), dated(true)] {
            for to in [Format::Mp4, Format::Mov] {
                all.push(mute(&src(), to, &movie(), &tags, &out()));
            }
            all.push(mute(&src(), Format::Mkv, &mkv(), &tags, &out()));
        }
        for to in [Format::Mp4, Format::Mov] {
            all.push(strip(&src(), to, &movie(), &movie_tags(), &out()));
        }
        all.push(strip(&src(), Format::Mkv, &mkv(), &mkv_tags(), &out()));
        for (to, probe) in [
            (Format::Mp3, song("mp3", "mp3")),
            (Format::Flac, song("flac", "flac")),
            (Format::M4a, song("mov,mp4,m4a,3gp,3g2,mj2", "aac")),
            (Format::Wav, song("wav", "pcm_s16le")),
        ] {
            all.push(strip(&src(), to, &probe, &Tags::default(), &out()));
        }
        all
    }

    #[test]
    fn every_tool_argv_keeps_the_invariants() {
        let src_url = file_url(&src());
        let planned = dir().join("-clip ü%d Muted.mp4");
        for args in every_argv() {
            let text: Vec<String> = args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let at = |arg: &str| text.iter().position(|a| a == arg).unwrap();
            // Both whitelists, before the one input, which is the source's
            // URL.
            assert_eq!(text[at("-protocol_whitelist") + 1], "file", "{text:?}");
            assert_eq!(
                text[at("-format_whitelist") + 1],
                crate::media::args::DEMUXERS
            );
            assert!(at("-format_whitelist") < at("-i"));
            assert_eq!(text.iter().filter(|a| *a == "-i").count(), 1);
            assert_eq!(args[at("-i") + 1], src_url);
            // A forced muxer, then the temp output, last.
            let n = args.len();
            assert_eq!(text[n - 3], "-f", "{text:?}");
            assert_eq!(args[n - 1], file_url(&out()));
            // No other path, and never the planned name.
            let dir = dir().to_string_lossy().into_owned();
            assert_eq!(text.iter().filter(|a| a.contains(&dir)).count(), 2);
            let planned = planned.to_string_lossy();
            assert!(!text.iter().any(|a| a.contains(&*planned)));
        }
    }

    /// Remove Metadata's options before the per-stream ones.
    const STRIP: [&str; 8] = [
        "-c",
        "copy",
        "-map_metadata",
        "-1",
        "-map_chapters",
        "-1",
        "-fflags",
        "+bitexact",
    ];

    #[test]
    fn strip_copies_what_it_keeps_without_tags() {
        // MP4 and MOV: the video, both audio tracks and the mov_text
        // subtitle, with their languages; no DVD subtitle, data or cover.
        for (to, muxer) in [(Format::Mp4, "mp4"), (Format::Mov, "mov")] {
            let mut middle = vec!["-map", "0:0", "-map", "0:1", "-map", "0:2", "-map", "0:3"];
            middle.extend(STRIP);
            middle.extend([
                "-metadata:s:1",
                "language=eng",
                "-metadata:s:2",
                "language=fra",
                "-strict",
                "unofficial",
                "-movflags",
                "+faststart",
                "-write_tmcd",
                "0",
            ]);
            assert_eq!(
                strip(&src(), to, &movie(), &movie_tags(), &out()),
                expect(&[], &middle, muxer),
                "{to:?}"
            );
        }
        // MKV: the video, audio, subtitle and the font, whose own tags come
        // back; not the image attachment or the data.
        let mut middle = vec!["-map", "0:0", "-map", "0:1", "-map", "0:2", "-map", "0:3"];
        middle.extend(STRIP);
        middle.extend([
            "-metadata:s:1",
            "language=deu",
            "-metadata:s:2",
            "language=eng",
            "-map_metadata:s:3",
            "0:s:3",
        ]);
        assert_eq!(
            strip(&src(), Format::Mkv, &mkv(), &mkv_tags(), &out()),
            expect(&[], &middle, "matroska")
        );
        // An attachment that isn't a font goes.
        let no_font = Tags {
            font_streams: BTreeSet::new(),
            ..mkv_tags()
        };
        let args = strip(&src(), Format::Mkv, &mkv(), &no_font, &out());
        assert!(!args.iter().any(|a| a == "0:3"));
        // Audio files: the audio alone, never the cover.
        let audio = |to, probe: &Probe, last: &[&'static str], muxer| {
            let mut middle = vec!["-map", "0:0"];
            middle.extend(STRIP);
            middle.extend(last);
            assert_eq!(
                strip(&src(), to, probe, &Tags::default(), &out()),
                expect(&[], &middle, muxer),
                "{to:?}"
            );
        };
        audio(
            Format::Mp3,
            &song("mp3", "mp3"),
            &["-id3v2_version", "0"],
            "mp3",
        );
        audio(Format::Flac, &song("flac", "flac"), &[], "flac");
        let m4a = song("mov,mp4,m4a,3gp,3g2,mj2", "aac");
        audio(Format::M4a, &m4a, &["-movflags", "+faststart"], "ipod");
        let wav = probe_of("wav", vec![stream(0, StreamKind::Audio, "pcm_s24le")]);
        audio(Format::Wav, &wav, &["-rf64", "auto"], "wav");
        // An M4A keeps every audio track, an MP3 its first.
        let two = probe_of(
            "mov,mp4,m4a,3gp,3g2,mj2",
            vec![
                stream(0, StreamKind::Audio, "aac"),
                stream(1, StreamKind::Audio, "alac"),
            ],
        );
        assert_eq!(stripped(Format::M4a, &two, &Tags::default()).count(), 2);
        assert_eq!(stripped(Format::Mp3, &two, &Tags::default()).count(), 1);
    }

    #[test]
    fn keep_sets_the_date_again_and_each_containers_options() {
        let argv = |to, tags: &Tags| {
            let mut args = Vec::new();
            keep(&mut args, to, tags);
            args.into_iter()
                .map(|a| a.into_string().unwrap())
                .collect::<Vec<_>>()
        };
        let date = ["-metadata", "creation_time=2024-05-01T10:20:30.000000Z"];
        let apple = [
            "-movflags",
            "+faststart+use_metadata_tags",
            "-metadata",
            "major_brand=",
            "-metadata",
            "minor_version=",
            "-metadata",
            "compatible_brands=",
        ];
        for to in [Format::Mp4, Format::Mov] {
            assert_eq!(argv(to, &dated(true)), [&date[..], &apple].concat());
            assert_eq!(
                argv(to, &dated(false)),
                [&date[..], &["-movflags", "+faststart"]].concat()
            );
            assert_eq!(
                argv(
                    to,
                    &Tags {
                        apple_keys: true,
                        ..Tags::default()
                    }
                ),
                apple
            );
        }
        // Never the keys in an M4A, whatever its tags.
        assert_eq!(
            argv(Format::M4a, &dated(true)),
            [&date[..], &["-movflags", "+faststart"]].concat()
        );
        let none = Tags::default();
        assert_eq!(argv(Format::Mp3, &none), ["-id3v2_version", "3"]);
        assert_eq!(argv(Format::Wav, &none), ["-rf64", "auto"]);
        assert!(argv(Format::Flac, &none).is_empty());
        assert!(argv(Format::Mkv, &none).is_empty());
        assert_eq!(argv(Format::Mkv, &dated(true)), date);
    }

    #[test]
    fn mute_maps_every_track_but_the_audio() {
        for (to, muxer) in [(Format::Mp4, "mp4"), (Format::Mov, "mov")] {
            assert_eq!(
                mute(&src(), to, &movie(), &dated(false), &out()),
                expect(
                    &[],
                    &[
                        "-map",
                        "0:0",
                        "-map",
                        "0:3",
                        "-map",
                        "0:6",
                        "-c",
                        "copy",
                        "-strict",
                        "unofficial",
                        "-metadata",
                        "creation_time=2024-05-01T10:20:30.000000Z",
                        "-movflags",
                        "+faststart",
                    ],
                    muxer
                ),
                "{to:?}"
            );
        }
        assert_eq!(
            mute(&src(), Format::Mkv, &mkv(), &Tags::default(), &out()),
            expect(
                &[],
                &["-map", "0:0", "-map", "0:2", "-map", "0:3", "-map", "0:4", "-c", "copy"],
                "matroska"
            )
        );
        // An iPhone MOV keeps its keys.
        let iphone = mute(&src(), Format::Mov, &movie(), &dated(true), &out());
        assert!(iphone
            .windows(2)
            .any(|w| w[0] == "-movflags" && w[1] == "+faststart+use_metadata_tags"));
    }
}
