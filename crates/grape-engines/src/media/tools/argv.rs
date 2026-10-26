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

use grape_core::{Channels, Format};

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

/// `RG`: the tags a change of loudness or channels makes wrong, blanked
/// (an empty value deletes a key, whatever its case; a missing one is
/// left alone).
const REPLAYGAIN: [&str; 14] = [
    "-metadata",
    "REPLAYGAIN_TRACK_GAIN=",
    "-metadata",
    "REPLAYGAIN_TRACK_PEAK=",
    "-metadata",
    "REPLAYGAIN_ALBUM_GAIN=",
    "-metadata",
    "REPLAYGAIN_ALBUM_PEAK=",
    "-metadata",
    "R128_TRACK_GAIN=",
    "-metadata",
    "R128_ALBUM_GAIN=",
    "-metadata",
    "iTunNORM=",
];

/// The end of a 16-bit output's filters: TPDF dither down to 16 bits.
const DITHER: &str = "aresample=osf=s16:dither_method=triangular";

/// How an audio tool's output is encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Codec {
    /// The encoder's options.
    pub(super) args: Vec<String>,
    /// A 16-bit output, whose filters end with TPDF dither.
    pub(super) dither: bool,
    /// A lossless output (FLAC, WAV, ALAC).
    pub(super) exact: bool,
}

/// `SQ`: Normalize Volume's and Audio Channels' output codec, the same
/// quality as the source's `audio` (D26, U7), for `channels` channels out:
///
/// - MP3: LAME VBR from the source's bitrate per channel (`bit_rate`, the
///   stream's or else the container's): at least 112k V0, 80k V2, 56k
///   V4, else V6; V2 when unknown.
/// - M4A: ALAC stays ALAC at 16 or 24 bits; anything else becomes AAC at
///   the source's bitrate per channel, 64k to 160k, rounded up to 8k
///   ([`aac_rate`]); 96k a channel when unknown.
/// - WAV: the same PCM (16, 24 or 32-bit integer, or 32-bit float; an
///   8-bit, 64-bit float or compressed WAV becomes 16-bit or 32-bit
///   float).
/// - FLAC: FLAC at level 8 with exact Rice parameters, 16 or 24 bits.
///
/// `Err` names the codec when the source is deeper than it writes:
/// FFmpeg's ALAC and FLAC encoders write at most 24 bits (without
/// FLAC's experimental mode), so a 32-bit ALAC or FLAC source would lose
/// its last 8 bits.
pub(super) fn same_quality(
    to: Format,
    audio: &StreamInfo,
    bit_rate: Option<u64>,
    channels: u32,
) -> Result<Codec, &'static str> {
    let per_channel = bit_rate.map(|rate| rate / u64::from(audio.channels.max(1)));
    let lossy = |args: Vec<String>| Codec {
        args,
        dither: false,
        exact: false,
    };
    let exact = |args: Vec<&str>, dither: bool| Codec {
        args: args.into_iter().map(str::to_owned).collect(),
        dither,
        exact: true,
    };
    let deep = audio.depth > 16;
    Ok(match to {
        Format::Mp3 => {
            let quality = match per_channel {
                Some(rate) if rate >= 112_000 => "0",
                Some(rate) if rate >= 80_000 => "2",
                Some(rate) if rate >= 56_000 => "4",
                Some(_) => "6",
                None => "2",
            };
            lossy(owned(&["-c:a", "libmp3lame", "-q:a", quality]))
        }
        Format::M4a if audio.codec == "alac" => {
            if audio.depth > 24 {
                return Err("ALAC");
            }
            let format = if deep { "s32p" } else { "s16p" };
            exact(vec!["-c:a", "alac", "-sample_fmt", format], !deep)
        }
        Format::M4a => {
            let rate = aac_rate(
                per_channel.map_or(96_000, |r| r.clamp(64_000, 160_000)),
                channels,
            );
            lossy(owned(&[
                "-c:a",
                "aac",
                "-b:a",
                &format!("{}k", rate / 1000),
            ]))
        }
        Format::Wav if audio.codec.starts_with("pcm_f") => exact(vec!["-c:a", "pcm_f32le"], false),
        Format::Wav => match audio.depth {
            0..=16 => exact(vec!["-c:a", "pcm_s16le"], true),
            17..=24 => exact(vec!["-c:a", "pcm_s24le"], false),
            _ => exact(vec!["-c:a", "pcm_s32le"], false),
        },
        _ => {
            if audio.depth > 24 {
                return Err("FLAC");
            }
            let format = if deep { "s32" } else { "s16" };
            exact(flac(format), !deep)
        }
    })
}

/// FLAC at level 8 with exact Rice parameters, samples as `format` (`s16`,
/// or `s32` for 24 bits). Levels 11 and 12 would leave the streamable
/// subset (`flacenc.c`).
fn flac(format: &str) -> Vec<&str> {
    vec![
        "-c:a",
        "flac",
        "-compression_level",
        "8",
        "-exact_rice_parameters",
        "1",
        "-sample_fmt",
        format,
    ]
}

/// An AAC bitrate of `per_channel` bits a second for each of `channels`
/// channels, rounded up to 8k a channel and at most 512k in all.
pub(super) fn aac_rate(per_channel: u64, channels: u32) -> u64 {
    let per_channel = per_channel.div_ceil(8_000) * 8_000;
    (per_channel * u64::from(channels.max(1))).min(512_000)
}

fn owned(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// What Audio Channels does with a source of `channels` channels, with a
/// channel layout or not, for `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Mix {
    /// It already has what was asked.
    Already,
    /// Stereo from 3 or more channels without a layout: there's no left
    /// and right to make it of (ambisonics, a recorder's polyphonic WAV).
    NoSides,
    /// The `-af` filter that remixes it; `unlabeled` reads it with
    /// `-guess_layout_max 0`, so FFmpeg doesn't guess a layout to remix
    /// from.
    Filter { filter: String, unlabeled: bool },
}

/// [`Mix`] for a source of `channels` channels (`has_layout` when it has a
/// channel layout) and `target` (D26):
///
/// - stereo → mono is (L+R)/2 (`pan`), with a layout or without;
/// - mono → stereo duplicates at full level (`pan`), where swresample's
///   matrix would give 0.707;
/// - 3 or more labeled channels go through swresample's matrix,
///   normalized with `rematrix_maxval=1` so it can't clip, on every codec
///   (a plain `-ac 2` is unnormalized for a float encoder such as AAC:
///   FL 1.0, FC 0.707, SL 0.707). 5.1 → stereo: FL 0.414, FC 0.293, SL
///   0.293, no LFE;
/// - 3 or more unlabeled channels average to mono, and can't be stereo.
pub(super) fn mix(target: Channels, channels: u32, has_layout: bool) -> Mix {
    let filter = |filter: String, unlabeled| Mix::Filter { filter, unlabeled };
    match (target, channels) {
        (Channels::Mono, 1) | (Channels::Stereo, 2) => Mix::Already,
        (Channels::Mono, 2) => filter("pan=mono|c0=0.5*c0+0.5*c1".to_owned(), false),
        (Channels::Stereo, 1) => filter("pan=stereo|c0=c0|c1=c0".to_owned(), false),
        (_, _) if has_layout => {
            let layout = match target {
                Channels::Mono => "mono",
                Channels::Stereo => "stereo",
            };
            filter(format!("aresample=ochl={layout}:rematrix_maxval=1"), false)
        }
        (Channels::Stereo, _) => Mix::NoSides,
        (Channels::Mono, n) => {
            let share = 1.0 / f64::from(n.max(1));
            let terms: Vec<String> = (0..n).map(|c| format!("{share:.6}*c{c}")).collect();
            filter(format!("pan=mono|c0={}", terms.join("+")), true)
        }
    }
}

/// The formats an audio tool keeps cover art in.
fn holds_cover(to: Format) -> bool {
    matches!(to, Format::Mp3 | Format::M4a | Format::Flac)
}

/// An audio tool's write pass (Audio Channels, Normalize Volume): the
/// source's first audio track through `filter` (TPDF dither added for a
/// 16-bit `codec`) into `codec`, its cover art copied (MP3, M4A, FLAC),
/// ReplayGain blanked ([`REPLAYGAIN`]) and [`keep`]. `reading` goes before
/// the source.
#[allow(clippy::too_many_arguments)]
pub(super) fn reencode(
    src: &Path,
    to: Format,
    probe: &Probe,
    tags: &Tags,
    reading: &[&str],
    filter: &str,
    codec: &Codec,
    out: &Path,
) -> Vec<OsString> {
    let mut args = prefix_at(src, reading, "error");
    if let Some(audio) = probe.streams.iter().find(|s| s.kind == StreamKind::Audio) {
        map(&mut args, audio.index);
    }
    let covers: Vec<u32> = probe
        .streams
        .iter()
        .filter(|s| s.attached_pic && holds_cover(to))
        .map(|s| s.index)
        .collect();
    for &index in &covers {
        map(&mut args, index);
    }
    if !covers.is_empty() {
        push(&mut args, &["-c:v", "copy"]);
    }
    let filter = if codec.dither {
        format!("{filter},{DITHER}")
    } else {
        filter.to_owned()
    };
    push(&mut args, &["-af", &filter]);
    args.extend(codec.args.iter().map(OsString::from));
    push(&mut args, &REPLAYGAIN);
    keep(&mut args, to, tags);
    finish(args, muxer_of(to), out)
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
        for to in [Format::Mp3, Format::M4a, Format::Wav, Format::Flac] {
            let probe = song("mp3", "mp3");
            let codec = same_quality(to, &probe.streams[0], None, 1).unwrap();
            let unlabeled = ["-guess_layout_max", "0"];
            let args = reencode(
                &src(),
                to,
                &probe,
                &dated(true),
                &unlabeled,
                "anull",
                &codec,
                &out(),
            );
            all.push(args);
        }
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

    /// An audio stream of `codec` at `depth` bits.
    fn audio(codec: &str, depth: u32) -> StreamInfo {
        StreamInfo {
            depth,
            ..stream(0, StreamKind::Audio, codec)
        }
    }

    /// [`same_quality`]'s options for `audio` at `bit_rate`, `channels`
    /// out, joined, and whether it dithers.
    fn sq(to: Format, audio: &StreamInfo, bit_rate: Option<u64>, channels: u32) -> (String, bool) {
        let codec = same_quality(to, audio, bit_rate, channels).unwrap();
        (codec.args.join(" "), codec.dither)
    }

    #[test]
    fn same_quality_keeps_the_codec_depth_and_about_the_bitrate() {
        let mp3 = audio("mp3", 16);
        for (rate, quality) in [
            (Some(320_000), "0"),
            (Some(224_000), "0"),
            (Some(223_999), "2"),
            (Some(192_000), "2"),
            (Some(160_000), "2"),
            (Some(128_000), "4"),
            (Some(112_000), "4"),
            (Some(96_000), "6"),
            (None, "2"),
        ] {
            let want = format!("-c:a libmp3lame -q:a {quality}");
            assert_eq!(sq(Format::Mp3, &mp3, rate, 2), (want, false), "{rate:?}");
        }
        // Per channel: a mono source at half the rate is as good.
        let mono = StreamInfo {
            channels: 1,
            ..mp3.clone()
        };
        assert_eq!(
            sq(Format::Mp3, &mono, Some(112_000), 2).0,
            "-c:a libmp3lame -q:a 0"
        );
        let aac = audio("aac", 16);
        for (rate, channels, want) in [
            (Some(256_000), 2, "256k"),
            (Some(256_000), 1, "128k"),
            (Some(400_000), 2, "320k"),
            (Some(96_000), 2, "128k"),
            (Some(130_000), 2, "144k"),
            (None, 2, "192k"),
            (None, 1, "96k"),
            (Some(1_000_000), 6, "512k"),
        ] {
            let want = format!("-c:a aac -b:a {want}");
            assert_eq!(
                sq(Format::M4a, &aac, rate, channels),
                (want, false),
                "{rate:?}"
            );
        }
        let alac = |depth| sq(Format::M4a, &audio("alac", depth), None, 2);
        assert_eq!(alac(16), ("-c:a alac -sample_fmt s16p".to_owned(), true));
        assert_eq!(alac(24), ("-c:a alac -sample_fmt s32p".to_owned(), false));
        let wav = |codec, depth| sq(Format::Wav, &audio(codec, depth), None, 2);
        assert_eq!(wav("pcm_s16le", 16), ("-c:a pcm_s16le".to_owned(), true));
        assert_eq!(wav("pcm_u8", 8), ("-c:a pcm_s16le".to_owned(), true));
        assert_eq!(wav("adpcm_ms", 16), ("-c:a pcm_s16le".to_owned(), true));
        assert_eq!(wav("pcm_s24le", 24), ("-c:a pcm_s24le".to_owned(), false));
        assert_eq!(wav("pcm_s32le", 32), ("-c:a pcm_s32le".to_owned(), false));
        assert_eq!(wav("pcm_f32le", 32), ("-c:a pcm_f32le".to_owned(), false));
        assert_eq!(wav("pcm_f64le", 64), ("-c:a pcm_f32le".to_owned(), false));
        let flac = |depth| sq(Format::Flac, &audio("flac", depth), None, 2);
        let level = "-c:a flac -compression_level 8 -exact_rice_parameters 1";
        assert_eq!(flac(16), (format!("{level} -sample_fmt s16"), true));
        assert_eq!(flac(24), (format!("{level} -sample_fmt s32"), false));
        // 32 bits in ALAC or FLAC would come out as 24.
        assert_eq!(
            same_quality(Format::M4a, &audio("alac", 32), None, 2),
            Err("ALAC")
        );
        assert_eq!(
            same_quality(Format::Flac, &audio("flac", 32), None, 2),
            Err("FLAC")
        );
        assert!(same_quality(Format::Wav, &audio("pcm_s32le", 32), None, 2).is_ok());
    }

    #[test]
    fn channels_mix_by_count_and_layout() {
        use Channels::{Mono, Stereo};
        let filter = |filter: &str, unlabeled| Mix::Filter {
            filter: filter.to_owned(),
            unlabeled,
        };
        for has_layout in [true, false] {
            assert_eq!(mix(Mono, 1, has_layout), Mix::Already);
            assert_eq!(mix(Stereo, 2, has_layout), Mix::Already);
            assert_eq!(
                mix(Mono, 2, has_layout),
                filter("pan=mono|c0=0.5*c0+0.5*c1", false)
            );
            assert_eq!(
                mix(Stereo, 1, has_layout),
                filter("pan=stereo|c0=c0|c1=c0", false)
            );
        }
        assert_eq!(
            mix(Mono, 6, true),
            filter("aresample=ochl=mono:rematrix_maxval=1", false)
        );
        assert_eq!(
            mix(Stereo, 6, true),
            filter("aresample=ochl=stereo:rematrix_maxval=1", false)
        );
        assert_eq!(
            mix(Mono, 4, false),
            filter(
                "pan=mono|c0=0.250000*c0+0.250000*c1+0.250000*c2+0.250000*c3",
                true
            )
        );
        assert_eq!(
            mix(Mono, 3, false),
            filter("pan=mono|c0=0.333333*c0+0.333333*c1+0.333333*c2", true)
        );
        assert_eq!(mix(Stereo, 4, false), Mix::NoSides);
        assert_eq!(aac_rate(64_001, 2), 144_000);
        assert_eq!(aac_rate(160_000, 8), 512_000);
    }

    #[test]
    fn reencode_keeps_the_cover_and_blanks_replaygain() {
        let mut rg: Vec<&str> = REPLAYGAIN.to_vec();
        let mp3 = song("mp3", "mp3");
        let codec = same_quality(Format::Mp3, &mp3.streams[0], Some(256_000), 1).unwrap();
        let mut middle = vec![
            "-map",
            "0:0",
            "-map",
            "0:1",
            "-c:v",
            "copy",
            "-af",
            "pan=mono|c0=0.5*c0+0.5*c1",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "0",
        ];
        middle.append(&mut rg);
        middle.extend(["-id3v2_version", "3"]);
        assert_eq!(
            reencode(
                &src(),
                Format::Mp3,
                &mp3,
                &Tags::default(),
                &[],
                "pan=mono|c0=0.5*c0+0.5*c1",
                &codec,
                &out()
            ),
            expect(&[], &middle, "mp3")
        );
        // A 16-bit FLAC from an unlabeled WAV: dithered, read without a
        // guessed layout; a WAV source has no cover to keep.
        let wav = probe_of(
            "wav",
            vec![stream(0, StreamKind::Audio, "pcm_s16le"), cover(1)],
        );
        let codec = same_quality(Format::Flac, &wav.streams[0], None, 1).unwrap();
        let args = reencode(
            &src(),
            Format::Wav,
            &wav,
            &Tags::default(),
            &["-guess_layout_max", "0"],
            "anull",
            &codec,
            &out(),
        );
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(text[10..12], ["-guess_layout_max", "0"]);
        assert!(!text.contains(&"0:1".to_owned()), "{text:?}");
        assert!(text.contains(&format!("anull,{DITHER}")), "{text:?}");
        assert!(text.ends_with(&[
            "-rf64".to_owned(),
            "auto".to_owned(),
            "-progress".to_owned(),
            "pipe:1".to_owned(),
            "-f".to_owned(),
            "wav".to_owned(),
            file_url(&out()).to_string_lossy().into_owned(),
        ]));
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
