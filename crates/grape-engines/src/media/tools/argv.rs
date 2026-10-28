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

use grape_core::{Channels, CompressPreset, Format};

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
///   WAV, and a Broadcast Wave's `bext` chunk written again
///   (`-write_bext 1`) when the source had one.
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
        Format::Wav => {
            push(args, &["-rf64", "auto"]);
            if tags.bext {
                push(args, &["-write_bext", "1"]);
            }
        }
        _ => {}
    }
}

/// Remove Audio (`muteVideo`) on MP4, MOV and MKV: a stream copy of every
/// track but the audio.
///
/// - MP4 and MOV keep every video stream and every `mov_text` subtitle, in
///   the source's order, and cover art where it can be written
///   ([`keeps_cover`]). `-strict unofficial` lets the muxer write Dolby
///   Vision (`dvcC`/`dvvC`), stereo and spherical boxes. The timecode
///   track is rebuilt from the `timecode` tag, and the chapter track from
///   the chapters.
/// - MKV keeps every stream that isn't audio, data or cover art: video,
///   subtitles and attachments.
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
    for stream in muted(to, probe, tags) {
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
pub(super) fn muted<'a>(
    to: Format,
    probe: &'a Probe,
    tags: &Tags,
) -> impl Iterator<Item = &'a StreamInfo> {
    let cover = keeps_cover(to, tags);
    probe.streams.iter().filter(move |s| match to {
        _ if s.attached_pic => cover,
        Format::Mkv => !matches!(s.kind, StreamKind::Audio | StreamKind::Data),
        _ => s.kind == StreamKind::Video || is_mov_text(s),
    })
}

/// Whether Remove Audio and Compress can keep a video's cover art in `to`:
/// only in an MP4 without iPhone keys.
///
/// - The MP4 and MOV muxer writes cover art only into iTunes-style
///   metadata (`covr`), which a MOV never gets and an MP4 with the keys
///   gets in its place ([`keep`]'s `use_metadata_tags`): there it would
///   be dropped without a word (6.1 and 9.0.2 alike).
/// - An MKV's cover art is an image attachment, which the demuxer shows
///   as cover art and a copy writes back as a one-frame video track (6.1
///   and 9.0.2 alike, whatever the disposition).
fn keeps_cover(to: Format, tags: &Tags) -> bool {
    to == Format::Mp4 && !tags.apple_keys
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
///   art to the demuxer, and so is a still image beside real video: a
///   cover FFmpeg copied as a one-frame video track, [`picture`]), audio
///   track and subtitle, and the fonts its subtitles need;
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
    let real_video = probe
        .streams
        .iter()
        .any(|s| s.kind == StreamKind::Video && !s.attached_pic && !is_still(s));
    probe.streams.iter().filter(move |s| {
        let video = s.kind == StreamKind::Video && !s.attached_pic;
        let audio = s.kind == StreamKind::Audio;
        match to {
            Format::Mp4 | Format::Mov => video || audio || is_mov_text(s),
            Format::M4a => audio,
            Format::Mkv => {
                (video && !(real_video && is_still(s)))
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
///
/// Each mixes in double precision ([`MIX_IN`]): `pan` and swresample
/// otherwise mix 16-bit audio in 16 bits, rounding without the dither
/// [`DITHER`] adds after, which then has nothing to do.
pub(super) fn mix(target: Channels, channels: u32, has_layout: bool) -> Mix {
    let filter = |filter: String, unlabeled| Mix::Filter {
        filter: format!("{MIX_IN},{filter}"),
        unlabeled,
    };
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

/// The start of Audio Channels' filters: the samples as doubles.
const MIX_IN: &str = "aformat=sample_fmts=dbl";

/// The formats an audio tool keeps cover art in.
fn holds_cover(to: Format) -> bool {
    matches!(to, Format::Mp3 | Format::M4a | Format::Flac)
}

/// How an audio tool's write pass encodes ([`reencode`]).
pub(super) struct Encode<'a> {
    /// Options for reading the source, before its `-i`.
    pub(super) reading: &'a [&'a str],
    /// The `-af` chain, if any. A 16-bit [`Codec::dither`] adds TPDF
    /// dither to its end.
    pub(super) filter: Option<&'a str>,
    pub(super) codec: &'a Codec,
    /// Blank ReplayGain, R128 and iTunNORM ([`REPLAYGAIN`]), which a
    /// change of loudness or channels makes wrong (Normalize, Channels).
    pub(super) blank_replaygain: bool,
}

/// An audio tool's write pass (Audio Channels, Normalize Volume, Compress
/// on audio): the source's first audio track, filtered and encoded as
/// `encode` says, its cover art copied (MP3, M4A, FLAC), and [`keep`].
pub(super) fn reencode(
    src: &Path,
    to: Format,
    probe: &Probe,
    tags: &Tags,
    encode: &Encode,
    out: &Path,
) -> Vec<OsString> {
    let mut args = prefix_at(src, encode.reading, "error");
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
    let filter = match (encode.filter, encode.codec.dither) {
        (Some(filter), true) => Some(format!("{filter},{DITHER}")),
        (Some(filter), false) => Some(filter.to_owned()),
        (None, true) => Some(DITHER.to_owned()),
        (None, false) => None,
    };
    if let Some(filter) = &filter {
        push(&mut args, &["-af", filter]);
    }
    args.extend(encode.codec.args.iter().map(OsString::from));
    if encode.blank_replaygain {
        push(&mut args, &REPLAYGAIN);
    }
    keep(&mut args, to, tags);
    finish(args, muxer_of(to), out)
}

/// The layout of each channel count, 1 to 8, that has no LFE ([`label`]).
/// FFmpeg's AAC encoder (6.1 and 9.0.2) takes each and its decoder reads
/// each back in the same order; 6.0 it reads back as hexagonal, its back
/// centre moved last.
const NO_LFE: [&str; 8] = [
    "mono",
    "stereo",
    "3.0",
    "quad",
    "5.0",
    "hexagonal",
    "7.0",
    "octagonal",
];

/// Whether `audio` is 3 to 8 channels without a layout (a field
/// recorder's polyphonic WAV, say), each a track of its own. FFmpeg would
/// guess one for it, with an LFE for 3, 6, 7 and 8 (2.1, 5.1, 6.1, 7.1):
/// AAC codes that channel band-limited, wiping its track, and FLAC says
/// it's there.
fn unlabeled(audio: &StreamInfo) -> bool {
    !audio.has_layout && (3..=8).contains(&audio.channels)
}

/// The `pan` that labels `audio`, a track without a layout, with the
/// [`NO_LFE`] layout of its count, each channel kept in its own, for a
/// source read with `-guess_layout_max 0`. `None` for a track with a
/// layout, or more than 8 channels.
fn label(audio: &StreamInfo) -> Option<String> {
    let channels = usize::try_from(audio.channels).ok()?;
    let layout = NO_LFE.get(channels.checked_sub(1)?)?;
    if audio.has_layout {
        return None;
    }
    let mut pan = format!("pan={layout}");
    for c in 0..channels {
        pan.push_str(&format!("|c{c}=c{c}"));
    }
    Some(pan)
}

/// What Compress does with an audio file ([`squeeze`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Squeeze {
    pub(super) codec: Codec,
    /// Options for reading the source, before its `-i`: for FLAC,
    /// `-guess_layout_max 0` for audio [`label`] labels.
    pub(super) reading: &'static [&'static str],
    /// The `-af` chain, if any: for FLAC, [`label`]'s `pan`; a lower
    /// rate, and for FLAC fewer bits with TPDF dither.
    pub(super) filter: Option<String>,
    /// The output's sample rate.
    pub(super) sample_rate: u32,
    /// The output must be at most 90 % of the source's size (a lossy
    /// one), else 95 % (an exact one) ([`small_enough`]).
    pub(super) lossy: bool,
    /// Whether Small would make it smaller still: for "Try Small".
    pub(super) try_small: bool,
    /// The source is already no bigger than the preset makes it, so
    /// nothing is encoded.
    pub(super) skip: bool,
}

/// Why Compress refuses an audio file before it runs ([`squeeze`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// Float samples, or more than 24 bits: FFmpeg's FLAC encoder writes
    /// at most 24-bit integers, so it would clip or cut them.
    Deep,
    /// More than the 8 channels FLAC holds.
    Channels,
}

/// MP3's VBR quality for a preset, and the bitrate it averages for
/// stereo (half that for mono).
fn lame(preset: CompressPreset) -> (&'static str, u64) {
    match preset {
        CompressPreset::Small => ("7", 100_000),
        CompressPreset::Balanced => ("5", 130_000),
        CompressPreset::High => ("2", 190_000),
    }
}

/// AAC's bitrate a channel for a preset.
fn aac_per_channel(preset: CompressPreset) -> u64 {
    match preset {
        CompressPreset::Small => 48_000,
        CompressPreset::Balanced => 64_000,
        CompressPreset::High => 96_000,
    }
}

/// Whether `bit_rate` is at most 1.1 × `nominal`: a source no bigger than
/// that isn't worth another lossy generation. Unknown is never.
fn within(bit_rate: Option<u64>, nominal: u64) -> bool {
    bit_rate.is_some_and(|rate| u128::from(rate) * 10 <= u128::from(nominal) * 11)
}

/// `rate` brought down to at most 48 kHz: 88.2, 176.4 and 352.8 kHz
/// become 44.1, any other rate over 48 kHz becomes 48.
fn at_most_48k(rate: u32) -> u32 {
    match rate {
        0..=48_000 => rate,
        _ if rate.is_multiple_of(44_100) => 44_100,
        _ => 48_000,
    }
}

/// Whether Compress's output of `after` bytes is enough smaller than its
/// source's `before` to be worth saving (D26): at most 90 % for a lossy
/// output, 95 % for an exact one.
pub(super) fn small_enough(before: u64, after: u64, lossy: bool) -> bool {
    let (after, before) = (u128::from(after), u128::from(before));
    if lossy {
        after * 10 <= before * 9
    } else {
        after * 20 <= before * 19
    }
}

/// Compress on MP3, M4A, FLAC, and WAV into FLAC (D26), for `audio` at
/// `bit_rate` (the stream's, or else the container's):
///
/// - MP3: LAME VBR V7, V5 or V2. Skipped when the source is within 1.1 ×
///   the preset's average (about 100k, 130k or 190k for stereo, half that
///   for mono).
/// - M4A: AAC at 48k, 64k or 96k a channel ([`aac_rate`]), at most 48 kHz
///   ([`at_most_48k`]; FFmpeg's AAC encoder takes at most 96 kHz). An AAC
///   source within 1.1 × that bitrate is skipped.
/// - FLAC: level 8 with exact Rice parameters. Small writes 16 bits (TPDF
///   dither) at most 48 kHz, Balanced the source's depth at most 48 kHz,
///   High the source as it is. Its size rule is the lossy one only when
///   bits or rate were cut. A WAV of 3 to 8 channels without a layout
///   keeps them as they are, labelled without an LFE ([`label`]).
///
/// "Try Small" ([`Squeeze::try_small`]) when Small would do more: for MP3
/// and M4A whenever the preset isn't Small (and Small wouldn't skip the
/// file too), for FLAC when the source is over 16 bits, or over 48 kHz at
/// High.
pub(super) fn squeeze(
    to: Format,
    audio: &StreamInfo,
    bit_rate: Option<u64>,
    preset: CompressPreset,
) -> Result<Squeeze, Refusal> {
    let small = preset == CompressPreset::Small;
    let rate = audio.sample_rate;
    // A lossy output: skipped at `preset` when `skips` says so; "Try
    // Small" unless Small would skip it too.
    let lossy =
        |args: Vec<String>, filter, sample_rate, skips: &dyn Fn(CompressPreset) -> bool| Squeeze {
            codec: Codec {
                args,
                dither: false,
                exact: false,
            },
            reading: &[],
            filter,
            sample_rate,
            lossy: true,
            try_small: !small && !skips(CompressPreset::Small),
            skip: skips(preset),
        };
    match to {
        Format::Mp3 => {
            let nominal = |preset| {
                let (_, stereo) = lame(preset);
                if audio.channels == 1 {
                    stereo / 2
                } else {
                    stereo
                }
            };
            let (quality, _) = lame(preset);
            Ok(lossy(
                owned(&["-c:a", "libmp3lame", "-q:a", quality]),
                None,
                rate,
                &|preset| within(bit_rate, nominal(preset)),
            ))
        }
        Format::M4a => {
            let target = |preset| aac_rate(aac_per_channel(preset), audio.channels);
            let aac = audio.codec == "aac";
            let capped = at_most_48k(rate);
            let bits = format!("{}k", target(preset) / 1000);
            Ok(lossy(
                owned(&["-c:a", "aac", "-b:a", &bits]),
                (capped != rate).then(|| format!("aresample={capped}")),
                capped,
                &|preset| aac && within(bit_rate, target(preset)),
            ))
        }
        _ => {
            if audio.codec.starts_with("pcm_f") || audio.depth > 24 {
                return Err(Refusal::Deep);
            }
            if audio.channels > 8 {
                return Err(Refusal::Channels);
            }
            let capped = match preset {
                CompressPreset::High => rate,
                _ => at_most_48k(rate),
            };
            let deep = audio.depth > 16;
            let fewer_bits = small && deep;
            let slower = capped != rate;
            let sixteen = small || !deep;
            let mut options = Vec::new();
            if slower {
                options.push(capped.to_string());
            }
            if sixteen && (fewer_bits || slower) {
                options.push("osf=s16:dither_method=triangular".to_owned());
            }
            let resample =
                (!options.is_empty()).then(|| format!("aresample={}", options.join(":")));
            // FFmpeg's guess for 3, 6, 7 or 8 channels without a layout
            // has an LFE, which FLAC would say is there.
            let pan = if unlabeled(audio) { label(audio) } else { None };
            let reading: &'static [&'static str] = if pan.is_some() {
                &["-guess_layout_max", "0"]
            } else {
                &[]
            };
            let filter = match (pan, resample) {
                (Some(pan), Some(resample)) => Some(format!("{pan},{resample}")),
                (pan, resample) => pan.or(resample),
            };
            Ok(Squeeze {
                codec: Codec {
                    args: owned(&flac(if sixteen { "s16" } else { "s32" })),
                    dither: false,
                    exact: true,
                },
                reading,
                filter,
                sample_rate: capped,
                lossy: fewer_bits || slower,
                // Small cuts more only bits Balanced keeps, or a rate High
                // keeps.
                try_small: !small && (deep || (rate > 48_000 && preset == CompressPreset::High)),
                skip: false,
            })
        }
    }
}

/// A video's x264 CRF and the short side it is brought down to, at a
/// preset.
fn x264(preset: CompressPreset) -> (&'static str, u32) {
    match preset {
        CompressPreset::Small => ("28", 720),
        CompressPreset::Balanced => ("23", 1080),
        CompressPreset::High => ("20", 2160),
    }
}

/// A `scale` filter's size that brings the short side down to `short`,
/// never up, keeping the shape, both sides even and at least 2 px. The
/// long side is worked out as `iw*short/min(iw,ih)`, so a short side
/// that is cut comes out exactly `short`. FFmpeg turns a rotated source
/// upright before the filters, so `iw` and `ih` are the shown size.
pub(super) fn fit(short: u32) -> String {
    format!(
        r"scale='max(2\,trunc(min(iw\,iw*{short}/min(iw\,ih))/2)*2)':'max(2\,trunc(min(ih\,ih*{short}/min(iw\,ih))/2)*2)'"
    )
}

/// The `fps` filter's rate for Small: an average over 31 fps divided by
/// the least whole number that brings it to 30 or less (60 → 30, 50 → 25,
/// 120 → 30, 59.94 → 29.97); `None` for 31 fps or less, or unknown.
pub(super) fn fps_cap(avg: Option<(u32, u32)>) -> Option<String> {
    let (num, den) = avg.filter(|&(num, den)| den > 0 && num > 0)?;
    let (num, den) = (u64::from(num), u64::from(den));
    if num <= 31 * den {
        return None;
    }
    let by = num.div_ceil(30 * den);
    Some(format!("{num}/{}", den * by))
}

/// Whether a video's transfer is PQ (`smpte2084`, HDR10), which Compress
/// can't keep: x264 writes 8 bits, and PQ in 8 bits bands.
pub(super) fn is_pq(video: &StreamInfo) -> bool {
    video.color_transfer == "smpte2084"
}

/// The video Compress encodes: the first that isn't cover art, and isn't
/// a still image either when there's real video too. (A cover FFmpeg
/// copied into an MKV comes back as a one-frame PNG or JPEG video track
/// without the cover-art mark, often before the movie.)
pub(super) fn picture(probe: &Probe) -> Option<&StreamInfo> {
    let mut videos = probe
        .streams
        .iter()
        .filter(|s| s.kind == StreamKind::Video && !s.attached_pic);
    let first = videos.clone().next();
    videos.find(|s| !is_still(s)).or(first)
}

/// Whether `stream` is coded as a still image ([`STILL_CODECS`]).
fn is_still(stream: &StreamInfo) -> bool {
    STILL_CODECS.contains(&stream.codec.as_str())
}

/// Codecs of still images, which a video track rarely is but cover art
/// copied into one is.
const STILL_CODECS: [&str; 5] = ["png", "mjpeg", "bmp", "webp", "gif"];

/// The streams Compress on a video keeps, in the source's order: the
/// first video that isn't cover art (encoded), every audio track, and what
/// [`mute`] keeps besides (cover art where it can be written,
/// [`keeps_cover`]; `mov_text` in MP4 and MOV; subtitles and attachments
/// in MKV). Further videos and data tracks are dropped.
pub(super) fn shrunk<'a>(
    to: Format,
    probe: &'a Probe,
    tags: &Tags,
) -> impl Iterator<Item = &'a StreamInfo> {
    let first = picture(probe).map(|s| s.index);
    let muted: Vec<u32> = muted(to, probe, tags).map(|s| s.index).collect();
    probe.streams.iter().filter(move |s| match s.kind {
        StreamKind::Video if !s.attached_pic => Some(s.index) == first,
        StreamKind::Audio => true,
        _ => muted.contains(&s.index),
    })
}

/// Whether Small would make a video smaller than `preset` does: always
/// with a picture to encode; for sound alone, when some audio track isn't
/// already AAC within Small's bitrate, which Small would copy too.
pub(super) fn small_does_more(
    to: Format,
    probe: &Probe,
    tags: &Tags,
    preset: CompressPreset,
) -> bool {
    preset != CompressPreset::Small
        && (picture(probe).is_some()
            || shrunk(to, probe, tags).any(|s| {
                let target = aac_rate(aac_per_channel(CompressPreset::Small), s.channels);
                s.kind == StreamKind::Audio && !(s.codec == "aac" && within(s.bit_rate, target))
            }))
}

/// Compress's argv for a video ([`shrink`]), and whether it copies an
/// audio track, which the muxer may still refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Shrink {
    pub(super) args: Vec<OsString>,
    pub(super) copies: bool,
}

/// Compress on MP4, MOV and MKV (D26): the streams [`shrunk`] keeps,
/// copied but for
///
/// - the picture ([`picture`]): x264 at CRF 28, 23 or 20 (`medium`,
///   `high`, `yuv420p`), its short side at most 720, 1080 or 2160
///   ([`fit`], Lanczos), and at Small at most 30 fps ([`fps_cap`]), which
///   leaves out an MP4 or MOV timecode (`-write_tmcd 0`). HLG stays
///   tagged as HLG;
/// - each audio track: AAC at 48k, 64k or 96k a channel ([`aac_rate`]),
///   at most 48 kHz, unless it is AAC already within 1.1 × that bitrate
///   and `copy_audio`, when it is copied. When one it encodes has 3 to 8
///   channels without a layout ([`unlabeled`]), the source is read with
///   `-guess_layout_max 0` and each track without a layout is labelled
///   with one that has no LFE ([`label`]).
///
/// Tags, chapters and dates stay ([`keep`]).
#[allow(clippy::too_many_arguments)]
pub(super) fn shrink(
    src: &Path,
    to: Format,
    probe: &Probe,
    tags: &Tags,
    preset: CompressPreset,
    copy_audio: bool,
    out: &Path,
) -> Shrink {
    let kept: Vec<&StreamInfo> = shrunk(to, probe, tags).collect();
    let copied = |s: &StreamInfo| {
        let target = aac_rate(aac_per_channel(preset), s.channels);
        copy_audio && s.codec == "aac" && within(s.bit_rate, target)
    };
    let unguessed = kept
        .iter()
        .any(|s| s.kind == StreamKind::Audio && !copied(s) && unlabeled(s));
    let reading: &[&str] = if unguessed {
        &["-guess_layout_max", "0"]
    } else {
        &[]
    };
    let mut args = prefix_at(src, reading, "error");
    for stream in &kept {
        map(&mut args, stream.index);
    }
    push(&mut args, &["-c", "copy"]);
    let picture = picture(probe).map(|s| s.index);
    let mut copies = false;
    let mut retimed = false;
    for (o, stream) in kept.iter().enumerate() {
        let o = o.to_string();
        let spec = |option: &str| format!("-{option}:{o}");
        if Some(stream.index) == picture && !stream.attached_pic {
            let (crf, short) = x264(preset);
            let mut chain = Vec::new();
            if preset == CompressPreset::Small {
                if let Some(rate) = fps_cap(stream.avg_fps) {
                    chain.push(format!("fps={rate}"));
                    // A camera timecode's frame number would be read at
                    // the new rate, so it's left out.
                    retimed = true;
                }
            }
            chain.push(format!("{}:flags=lanczos", fit(short)));
            push(
                &mut args,
                &[
                    &spec("c"),
                    "libx264",
                    &spec("crf"),
                    crf,
                    &spec("preset"),
                    "medium",
                    &spec("profile"),
                    "high",
                    &spec("pix_fmt"),
                    "yuv420p",
                    &spec("filter"),
                    &chain.join(","),
                ],
            );
        } else if stream.kind == StreamKind::Audio {
            if copied(stream) {
                copies = true;
                continue;
            }
            let target = aac_rate(aac_per_channel(preset), stream.channels);
            let bits = format!("{}k", target / 1000);
            push(&mut args, &[&spec("c"), "aac", &spec("b"), &bits]);
            // Read unguessed, every track without a layout needs one.
            if let Some(pan) = label(stream).filter(|_| unguessed) {
                push(&mut args, &[&spec("filter"), &pan]);
            }
            let capped = at_most_48k(stream.sample_rate);
            if capped != stream.sample_rate {
                push(&mut args, &[&spec("ar"), &capped.to_string()]);
            }
        }
    }
    if retimed && matches!(to, Format::Mp4 | Format::Mov) {
        push(&mut args, &["-write_tmcd", "0"]);
    }
    keep(&mut args, to, tags);
    Shrink {
        args: finish(args, muxer_of(to), out),
        copies,
    }
}

/// A measuring or check pass of Normalize Volume: the source's first
/// audio track through `filter` (which ends with `ebur128`), logged at
/// `info` so the summary is printed, into the `null` muxer. It opens no
/// file (`AVFMT_NOFILE`), so the output is `-` and nothing is written:
/// these passes run before the temp file exists.
pub(super) fn measure(src: &Path, probe: &Probe, filter: &str) -> Vec<OsString> {
    let mut args = prefix_at(src, &[], "info");
    if let Some(audio) = probe.streams.iter().find(|s| s.kind == StreamKind::Audio) {
        map(&mut args, audio.index);
    }
    push(
        &mut args,
        &["-af", filter, "-progress", "pipe:1", "-f", "null", "-"],
    );
    args
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
            let encode = Encode {
                reading: &["-guess_layout_max", "0"],
                filter: Some("anull"),
                codec: &codec,
                blank_replaygain: true,
            };
            all.push(reencode(&src(), to, &probe, &dated(true), &encode, &out()));
        }
        for (to, codec) in [
            (Format::Mp3, "mp3"),
            (Format::M4a, "alac"),
            (Format::Flac, "flac"),
            (Format::Flac, "pcm_s24le"),
        ] {
            for preset in [
                CompressPreset::Small,
                CompressPreset::Balanced,
                CompressPreset::High,
            ] {
                let mut probe = song("mp3", codec);
                probe.streams[0].depth = 24;
                probe.streams[0].sample_rate = 96_000;
                let squeeze = squeeze(to, &probe.streams[0], None, preset).unwrap();
                let encode = Encode {
                    reading: &[],
                    filter: squeeze.filter.as_deref(),
                    codec: &squeeze.codec,
                    blank_replaygain: false,
                };
                all.push(reencode(&src(), to, &probe, &dated(true), &encode, &out()));
            }
        }
        for preset in [
            CompressPreset::Small,
            CompressPreset::Balanced,
            CompressPreset::High,
        ] {
            for copy_audio in [true, false] {
                for (to, probe) in [(Format::Mp4, movie()), (Format::Mkv, mkv())] {
                    let tags = dated(true);
                    let shrink = shrink(&src(), to, &probe, &tags, preset, copy_audio, &out());
                    all.push(shrink.args);
                }
            }
        }
        all.push(measure(&src(), &song("mp3", "mp3"), "ebur128=peak=true"));
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
            // A forced muxer, then the temp output, last; or the null
            // muxer, which writes nothing.
            let n = args.len();
            assert_eq!(text[n - 3], "-f", "{text:?}");
            let null = text[n - 2] == "null";
            if null {
                assert_eq!(text[n - 1], "-");
            } else {
                assert_eq!(args[n - 1], file_url(&out()));
            }
            // No other path, and never the planned name.
            let dir = dir().to_string_lossy().into_owned();
            let paths = text.iter().filter(|a| a.contains(&dir)).count();
            assert_eq!(paths, if null { 1 } else { 2 });
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
        // In an MKV, a still image beside real video is a cover FFmpeg
        // copied as a video track; alone, it's the video.
        let indexes = |to, probe: &Probe| -> Vec<u32> {
            stripped(to, probe, &Tags::default())
                .map(|s| s.index)
                .collect()
        };
        let copied = |format_name| {
            probe_of(
                format_name,
                vec![
                    stream(0, StreamKind::Video, "h264"),
                    stream(1, StreamKind::Audio, "aac"),
                    stream(2, StreamKind::Video, "png"),
                ],
            )
        };
        assert_eq!(indexes(Format::Mkv, &copied("matroska,webm")), [0, 1]);
        let still = probe_of(
            "matroska,webm",
            vec![
                stream(0, StreamKind::Video, "mjpeg"),
                stream(1, StreamKind::Audio, "aac"),
            ],
        );
        assert_eq!(indexes(Format::Mkv, &still), [0, 1]);
        let mp4 = copied("mov,mp4,m4a,3gp,3g2,mj2");
        assert_eq!(indexes(Format::Mp4, &mp4), [0, 1, 2]);
    }

    #[test]
    fn strip_sets_languages_and_font_tags_by_output_index() {
        // A data track before the audio: the audio is output 1, input 2.
        let probe = probe_of(
            "mov,mp4,m4a,3gp,3g2,mj2",
            vec![
                stream(0, StreamKind::Video, "h264"),
                stream(1, StreamKind::Data, "none"),
                stream(2, StreamKind::Audio, "aac"),
            ],
        );
        let tags = Tags {
            languages: BTreeMap::from([(2, "fra".to_owned())]),
            ..Tags::default()
        };
        let args = strip(&src(), Format::Mp4, &probe, &tags, &out());
        assert!(has(&args, ["-metadata:s:1", "language=fra"]), "{args:?}");
        // Cover art before the font: the font is output 1, input 2.
        let mkv = probe_of(
            "matroska,webm",
            vec![
                stream(0, StreamKind::Video, "h264"),
                cover(1),
                stream(2, StreamKind::Attachment, "ttf"),
            ],
        );
        let tags = Tags {
            font_streams: BTreeSet::from([2]),
            ..Tags::default()
        };
        let args = strip(&src(), Format::Mkv, &mkv, &tags, &out());
        assert!(has(&args, ["-map_metadata:s:1", "0:s:2"]), "{args:?}");
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
        // No channels (ffprobe didn't say): nothing divides by zero.
        let none = StreamInfo {
            channels: 0,
            ..mp3.clone()
        };
        assert!(same_quality(Format::Mp3, &none, Some(128_000), 2).is_ok());
        assert_eq!(aac_rate(64_000, 0), 64_000);
    }

    #[test]
    fn channels_mix_by_count_and_layout() {
        use Channels::{Mono, Stereo};
        let filter = |filter: &str, unlabeled| Mix::Filter {
            filter: format!("aformat=sample_fmts=dbl,{filter}"),
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
        // Seven keys, each blanked by a `-metadata` of its own.
        let (options, keys): (Vec<&str>, Vec<&str>) =
            REPLAYGAIN.chunks(2).map(|pair| (pair[0], pair[1])).unzip();
        assert_eq!(options, ["-metadata"; 7]);
        assert_eq!(
            keys,
            [
                "REPLAYGAIN_TRACK_GAIN=",
                "REPLAYGAIN_TRACK_PEAK=",
                "REPLAYGAIN_ALBUM_GAIN=",
                "REPLAYGAIN_ALBUM_PEAK=",
                "R128_TRACK_GAIN=",
                "R128_ALBUM_GAIN=",
                "iTunNORM=",
            ]
        );
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
        let encode = Encode {
            reading: &[],
            filter: Some("pan=mono|c0=0.5*c0+0.5*c1"),
            codec: &codec,
            blank_replaygain: true,
        };
        assert_eq!(
            reencode(&src(), Format::Mp3, &mp3, &Tags::default(), &encode, &out()),
            expect(&[], &middle, "mp3")
        );
        // Compress's: no filter, ReplayGain kept.
        let encode = Encode {
            filter: None,
            blank_replaygain: false,
            ..encode
        };
        let middle = [
            "-map",
            "0:0",
            "-map",
            "0:1",
            "-c:v",
            "copy",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "0",
            "-id3v2_version",
            "3",
        ];
        assert_eq!(
            reencode(&src(), Format::Mp3, &mp3, &Tags::default(), &encode, &out()),
            expect(&[], &middle, "mp3")
        );
        // A 16-bit FLAC from an unlabeled WAV: dithered, read without a
        // guessed layout; a WAV source has no cover to keep.
        let wav = probe_of(
            "wav",
            vec![stream(0, StreamKind::Audio, "pcm_s16le"), cover(1)],
        );
        let codec = same_quality(Format::Flac, &wav.streams[0], None, 1).unwrap();
        let encode = Encode {
            reading: &["-guess_layout_max", "0"],
            filter: Some("anull"),
            codec: &codec,
            blank_replaygain: true,
        };
        let args = reencode(&src(), Format::Wav, &wav, &Tags::default(), &encode, &out());
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(text[10..12], ["-guess_layout_max", "0"]);
        assert!(!text.contains(&"0:1".to_owned()), "{text:?}");
        assert!(text.contains(&format!("anull,{DITHER}")), "{text:?}");
        let encode = Encode {
            filter: None,
            ..encode
        };
        let args = reencode(&src(), Format::Wav, &wav, &Tags::default(), &encode, &out());
        assert!(has(&args, ["-af", DITHER]), "{args:?}");
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

    /// `audio` at `rate` Hz with `channels` channels.
    fn at(audio: StreamInfo, rate: u32, channels: u32) -> StreamInfo {
        StreamInfo {
            sample_rate: rate,
            channels,
            ..audio
        }
    }

    /// [`squeeze`]'s plan, as (options, filter, rate, lossy, try small,
    /// skip).
    fn plan(
        to: Format,
        audio: &StreamInfo,
        bit_rate: Option<u64>,
        preset: CompressPreset,
    ) -> (String, Option<String>, u32, bool, bool, bool) {
        let s = squeeze(to, audio, bit_rate, preset).unwrap();
        assert!(!s.codec.dither, "the filter dithers");
        assert_eq!(s.codec.exact, !matches!(to, Format::Mp3 | Format::M4a));
        (
            s.codec.args.join(" "),
            s.filter,
            s.sample_rate,
            s.lossy,
            s.try_small,
            s.skip,
        )
    }

    #[test]
    fn compress_on_mp3_picks_the_vbr_level_and_skips_a_small_source() {
        use CompressPreset::{Balanced, High, Small};
        let stereo = at(audio("mp3", 16), 44_100, 2);
        let mono = at(audio("mp3", 16), 44_100, 1);
        let lame = |q: &str| format!("-c:a libmp3lame -q:a {q}");
        // (source, bit rate, preset, quality, try small, skip)
        for (source, rate, preset, q, try_small, skip) in [
            (&stereo, Some(320_000), Small, "7", false, false),
            (&stereo, Some(320_000), Balanced, "5", true, false),
            (&stereo, Some(320_000), High, "2", true, false),
            // 1.1 × 190k is 209k: just over, encoded; at it, skipped.
            (&stereo, Some(209_001), High, "2", true, false),
            (&stereo, Some(209_000), High, "2", true, true),
            // 143k at Balanced: skipped, and Small (110k) would still do more.
            (&stereo, Some(143_000), Balanced, "5", true, true),
            (&stereo, Some(143_001), Balanced, "5", true, false),
            // 110k: Small skips it too, so there's nothing to suggest.
            (&stereo, Some(110_000), Balanced, "5", false, true),
            (&stereo, Some(110_000), Small, "7", false, true),
            (&stereo, Some(110_001), Small, "7", false, false),
            // Mono's averages are half stereo's.
            (&mono, Some(55_000), Small, "7", false, true),
            (&mono, Some(56_000), Small, "7", false, false),
            (&mono, Some(104_500), High, "2", true, true),
            // Unknown: encoded, the size rule decides.
            (&stereo, None, Small, "7", false, false),
            (&stereo, None, Balanced, "5", true, false),
        ] {
            assert_eq!(
                plan(Format::Mp3, source, rate, preset),
                (lame(q), None, 44_100, true, try_small, skip),
                "{} channels at {rate:?}, {preset:?}",
                source.channels
            );
        }
    }

    #[test]
    fn compress_on_m4a_writes_aac_and_skips_only_a_small_aac() {
        use CompressPreset::{Balanced, High, Small};
        let aac = at(audio("aac", 16), 48_000, 2);
        let alac = at(audio("alac", 24), 96_000, 2);
        let surround = at(audio("aac", 16), 48_000, 6);
        let opts = |k: u64| format!("-c:a aac -b:a {k}k");
        // (source, bit rate, preset, kbit/s, filter, rate, try small, skip)
        let cases = [
            (&aac, Some(256_000), Small, 96, None, 48_000, false, false),
            (
                &aac,
                Some(256_000),
                Balanced,
                128,
                None,
                48_000,
                true,
                false,
            ),
            (&aac, Some(256_000), High, 192, None, 48_000, true, false),
            // 1.1 × 128k = 140.8k.
            (&aac, Some(140_800), Balanced, 128, None, 48_000, true, true),
            (
                &aac,
                Some(140_801),
                Balanced,
                128,
                None,
                48_000,
                true,
                false,
            ),
            // At 105.6k Small (96k) skips it too.
            (
                &aac,
                Some(105_600),
                Balanced,
                128,
                None,
                48_000,
                false,
                true,
            ),
            // ALAC is never skipped, and AAC is at most 48 kHz.
            (
                &alac,
                Some(50_000),
                Small,
                96,
                Some("aresample=48000"),
                48_000,
                false,
                false,
            ),
            (
                &alac,
                None,
                High,
                192,
                Some("aresample=48000"),
                48_000,
                true,
                false,
            ),
            // 5.1 at 64k a channel; at High, 512k at most.
            (&surround, None, Balanced, 384, None, 48_000, true, false),
            (&surround, None, High, 512, None, 48_000, true, false),
        ];
        for (source, rate, preset, k, filter, out_rate, try_small, skip) in cases {
            assert_eq!(
                plan(Format::M4a, source, rate, preset),
                (
                    opts(k),
                    filter.map(str::to_owned),
                    out_rate,
                    true,
                    try_small,
                    skip
                ),
                "{} at {rate:?}, {preset:?}",
                source.codec
            );
        }
        let cd = at(audio("alac", 16), 88_200, 2);
        assert_eq!(
            plan(Format::M4a, &cd, None, Balanced).1.as_deref(),
            Some("aresample=44100")
        );
    }

    #[test]
    fn compress_into_flac_cuts_bits_and_rate_by_preset() {
        use CompressPreset::{Balanced, High, Small};
        let flac = |format: &str| {
            format!("-c:a flac -compression_level 8 -exact_rice_parameters 1 -sample_fmt {format}")
        };
        let dither = "osf=s16:dither_method=triangular";
        // (codec, depth, rate, preset, sample format, filter, out rate,
        // lossy, try small)
        let cases = [
            (
                "pcm_s16le",
                16,
                44_100,
                Small,
                "s16",
                None,
                44_100,
                false,
                false,
            ),
            (
                "pcm_s16le",
                16,
                44_100,
                Balanced,
                "s16",
                None,
                44_100,
                false,
                false,
            ),
            (
                "pcm_s16le",
                16,
                44_100,
                High,
                "s16",
                None,
                44_100,
                false,
                false,
            ),
            (
                "flac",
                24,
                48_000,
                Small,
                "s16",
                Some(format!("aresample={dither}")),
                48_000,
                true,
                false,
            ),
            (
                "flac", 24, 48_000, Balanced, "s32", None, 48_000, false, true,
            ),
            ("flac", 24, 48_000, High, "s32", None, 48_000, false, true),
            (
                "pcm_s24le",
                24,
                96_000,
                Small,
                "s16",
                Some(format!("aresample=48000:{dither}")),
                48_000,
                true,
                false,
            ),
            (
                "pcm_s24le",
                24,
                96_000,
                Balanced,
                "s32",
                Some("aresample=48000".to_owned()),
                48_000,
                true,
                true,
            ),
            (
                "pcm_s24le",
                24,
                96_000,
                High,
                "s32",
                None,
                96_000,
                false,
                true,
            ),
            // Small would write the same 16-bit 44.1 kHz file.
            (
                "pcm_s16le",
                16,
                88_200,
                Balanced,
                "s16",
                Some(format!("aresample=44100:{dither}")),
                44_100,
                true,
                false,
            ),
            (
                "pcm_s16le",
                16,
                88_200,
                High,
                "s16",
                None,
                88_200,
                false,
                true,
            ),
            (
                "flac",
                16,
                192_000,
                Small,
                "s16",
                Some(format!("aresample=48000:{dither}")),
                48_000,
                true,
                false,
            ),
            (
                "flac",
                24,
                352_800,
                Balanced,
                "s32",
                Some("aresample=44100".to_owned()),
                44_100,
                true,
                true,
            ),
            // An 8-bit WAV is 16 bits in FLAC, without dither.
            (
                "pcm_u8", 8, 22_050, Balanced, "s16", None, 22_050, false, false,
            ),
            // 16 bits at 48 kHz: Small has nothing more to cut.
            (
                "flac", 16, 48_000, Balanced, "s16", None, 48_000, false, false,
            ),
            ("flac", 16, 48_000, High, "s16", None, 48_000, false, false),
        ];
        for (codec, depth, rate, preset, format, filter, out_rate, lossy, try_small) in cases {
            assert_eq!(
                plan(
                    Format::Flac,
                    &at(audio(codec, depth), rate, 2),
                    None,
                    preset
                ),
                (flac(format), filter, out_rate, lossy, try_small, false),
                "{depth}-bit {codec} at {rate}, {preset:?}"
            );
        }
        // Float, or deeper than FLAC's 24 bits, or more than 8 channels.
        for (codec, depth, channels, refusal) in [
            ("pcm_f32le", 32, 2, Refusal::Deep),
            ("pcm_f64le", 64, 2, Refusal::Deep),
            ("pcm_f16le", 16, 2, Refusal::Deep),
            ("pcm_s32le", 32, 2, Refusal::Deep),
            ("flac", 32, 2, Refusal::Deep),
            ("pcm_s24le", 24, 10, Refusal::Channels),
        ] {
            let source = at(audio(codec, depth), 48_000, channels);
            assert_eq!(
                squeeze(Format::Flac, &source, None, High),
                Err(refusal),
                "{codec}"
            );
        }
        let eight = at(audio("pcm_s24le", 24), 48_000, 8);
        assert!(squeeze(Format::Flac, &eight, None, High).is_ok());
    }

    #[test]
    fn the_size_rule_is_ninety_percent_for_lossy_and_ninety_five_for_exact() {
        assert!(small_enough(1000, 900, true));
        assert!(!small_enough(1000, 901, true));
        assert!(small_enough(1000, 950, false));
        assert!(!small_enough(1000, 951, false));
        assert!(small_enough(0, 0, true));
        assert!(!small_enough(0, 1, false));
        assert!(small_enough(u64::MAX, u64::MAX / 10 * 9, true));
        assert!(!small_enough(u64::MAX, u64::MAX, false));
    }

    #[test]
    fn rates_over_48k_come_down_to_their_family() {
        for (rate, capped) in [
            (8_000, 8_000),
            (44_100, 44_100),
            (48_000, 48_000),
            (48_001, 48_000),
            (88_200, 44_100),
            (96_000, 48_000),
            (176_400, 44_100),
            (192_000, 48_000),
            (352_800, 44_100),
            (384_000, 48_000),
            (64_000, 48_000),
        ] {
            assert_eq!(at_most_48k(rate), capped, "{rate}");
        }
    }

    #[test]
    fn compress_fits_the_short_side_without_scaling_up() {
        use crate::media::args::tests::scaled;
        // (short side, width, height, out)
        for (short, iw, ih, want) in [
            (720, 1920, 1080, (1280, 720)),
            (720, 1080, 1920, (720, 1280)),
            (1080, 3840, 2160, (1920, 1080)),
            (2160, 3840, 2160, (3840, 2160)),
            (1080, 1280, 720, (1280, 720)),
            (720, 4000, 3000, (960, 720)),
            (720, 1918, 1080, (1278, 720)),
            (1080, 1279, 719, (1278, 718)),
            (720, 720, 720, (720, 720)),
            (720, 1, 1, (2, 2)),
            (720, 4096, 2, (4096, 2)),
            (720, 9000, 1000, (6480, 720)),
        ] {
            assert_eq!(scaled(&fit(short), iw, ih), want, "{iw}x{ih} to {short}");
        }
    }

    #[test]
    fn small_caps_the_rate_at_30_by_a_whole_divisor() {
        for (avg, want) in [
            (Some((60, 1)), Some("60/2")),
            (Some((60_000, 1001)), Some("60000/2002")),
            (Some((50, 1)), Some("50/2")),
            (Some((120, 1)), Some("120/4")),
            (Some((240, 1)), Some("240/8")),
            (Some((32, 1)), Some("32/2")),
            (Some((31, 1)), None),
            (Some((30, 1)), None),
            (Some((30_000, 1001)), None),
            (Some((0, 0)), None),
            (Some((60, 0)), None),
            (None, None),
        ] {
            assert_eq!(fps_cap(avg).as_deref(), want, "{avg:?}");
        }
    }

    #[test]
    fn compress_keeps_the_first_picture_audio_cover_and_subtitles() {
        let indexes = |to, probe: &Probe| -> Vec<u32> {
            shrunk(to, probe, &Tags::default())
                .map(|s| s.index)
                .collect()
        };
        // MP4: the second video and data go; mov_text, not DVD subtitles.
        let mut two = movie();
        two.streams.push(stream(7, StreamKind::Video, "hevc"));
        assert_eq!(indexes(Format::Mp4, &two), [0, 1, 2, 3, 6]);
        // Cover art only where the muxer writes it: not in a MOV, nor an
        // MP4 with iPhone keys, nor an MKV, where a copy would make it a
        // video track.
        assert_eq!(indexes(Format::Mov, &two), [0, 1, 2, 3]);
        let keyed: Vec<u32> = shrunk(Format::Mp4, &two, &dated(true))
            .map(|s| s.index)
            .collect();
        assert_eq!(keyed, [0, 1, 2, 3]);
        assert_eq!(indexes(Format::Mkv, &mkv()), [0, 1, 2, 3]);
        // Cover art first doesn't count as the picture.
        let covered = probe_of(
            "mov,mp4,m4a,3gp,3g2,mj2",
            vec![cover(0), stream(1, StreamKind::Video, "h264")],
        );
        assert_eq!(picture(&covered).map(|s| s.index), Some(1));
        // A cover FFmpeg wrote as a video track, before the movie.
        let copied = probe_of(
            "matroska,webm",
            vec![
                stream(0, StreamKind::Video, "png"),
                stream(1, StreamKind::Video, "h264"),
            ],
        );
        assert_eq!(picture(&copied).map(|s| s.index), Some(1));
        let stills = probe_of("matroska,webm", vec![stream(0, StreamKind::Video, "mjpeg")]);
        assert_eq!(picture(&stills).map(|s| s.index), Some(0));
        let song = probe_of(
            "mov,mp4,m4a,3gp,3g2,mj2",
            vec![stream(0, StreamKind::Audio, "aac"), cover(1)],
        );
        assert_eq!(picture(&song), None);
        assert_eq!(indexes(Format::Mp4, &song), [0, 1]);
        let mut pq = stream(0, StreamKind::Video, "hevc");
        assert!(!is_pq(&pq));
        pq.color_transfer = "arib-std-b67".to_owned();
        assert!(!is_pq(&pq));
        pq.color_transfer = "smpte2084".to_owned();
        assert!(is_pq(&pq));
    }

    #[test]
    fn compress_encodes_the_picture_and_copies_small_aac() {
        let mut probe = movie();
        probe.streams[0].avg_fps = Some((60, 1));
        // Track 1 is AAC at 120k stereo: within 1.1 × Balanced's 128k.
        probe.streams[1].bit_rate = Some(120_000);
        // Track 2 is AAC at 200k: re-encoded.
        probe.streams[2].bit_rate = Some(200_000);
        let shrink = shrink(
            &src(),
            Format::Mp4,
            &probe,
            &Tags::default(),
            CompressPreset::Balanced,
            true,
            &out(),
        );
        assert!(shrink.copies);
        let scale = format!("{}:flags=lanczos", fit(1080));
        let middle = [
            "-map",
            "0:0",
            "-map",
            "0:1",
            "-map",
            "0:2",
            "-map",
            "0:3",
            "-map",
            "0:6",
            "-c",
            "copy",
            "-c:0",
            "libx264",
            "-crf:0",
            "23",
            "-preset:0",
            "medium",
            "-profile:0",
            "high",
            "-pix_fmt:0",
            "yuv420p",
            "-filter:0",
            &scale,
            "-c:2",
            "aac",
            "-b:2",
            "128k",
            "-movflags",
            "+faststart",
        ];
        assert_eq!(shrink.args, expect(&[], &middle, "mp4"));

        // Without the copy (the retry), and at Small: both tracks at 96k,
        // and 60 fps at 30.
        let shrink = super::shrink(
            &src(),
            Format::Mp4,
            &probe,
            &Tags::default(),
            CompressPreset::Small,
            false,
            &out(),
        );
        assert!(!shrink.copies);
        let chain = format!("fps=60/2,{}:flags=lanczos", fit(720));
        assert!(
            has(&shrink.args, ["-filter:0", &chain]),
            "{:?}",
            shrink.args
        );
        assert!(has(&shrink.args, ["-crf:0", "28"]));
        assert!(has(&shrink.args, ["-write_tmcd", "0"]));
        assert!(has(&shrink.args, ["-b:1", "96k"]));
        assert!(has(&shrink.args, ["-b:2", "96k"]));

        // High: 2160, CRF 20, no rate cap; 5.1 PCM at 96 kHz becomes AAC
        // at 512k (96k a channel, capped) and 48 kHz.
        let mut pcm = stream(1, StreamKind::Audio, "pcm_s24le");
        pcm.channels = 6;
        pcm.sample_rate = 96_000;
        let mut video = stream(0, StreamKind::Video, "prores");
        video.avg_fps = Some((60, 1));
        let probe = probe_of("mov,mp4,m4a,3gp,3g2,mj2", vec![video, pcm]);
        let shrink = super::shrink(
            &src(),
            Format::Mov,
            &probe,
            &dated(true),
            CompressPreset::High,
            true,
            &out(),
        );
        assert!(!shrink.copies);
        let scale = format!("{}:flags=lanczos", fit(2160));
        assert!(has(&shrink.args, ["-filter:0", &scale]));
        assert!(has(&shrink.args, ["-crf:0", "20"]));
        assert!(has(&shrink.args, ["-b:1", "512k"]));
        assert!(has(&shrink.args, ["-ar:1", "48000"]));
        assert!(has(
            &shrink.args,
            ["-movflags", "+faststart+use_metadata_tags"]
        ));

        // An audio-only MKV: no x264 at all, and its cover art left out.
        let probe = probe_of(
            "matroska,webm",
            vec![stream(0, StreamKind::Audio, "flac"), cover(1)],
        );
        let shrink = super::shrink(
            &src(),
            Format::Mkv,
            &probe,
            &Tags::default(),
            CompressPreset::Balanced,
            true,
            &out(),
        );
        let middle = ["-map", "0:0", "-c", "copy", "-c:0", "aac", "-b:0", "128k"];
        assert_eq!(shrink.args, expect(&[], &middle, "matroska"));

        // Only AAC is copied: MP3 at 120k is re-encoded.
        let mut mp3 = stream(1, StreamKind::Audio, "mp3");
        mp3.bit_rate = Some(120_000);
        let probe = probe_of(
            "matroska,webm",
            vec![stream(0, StreamKind::Video, "h264"), mp3],
        );
        let shrink = super::shrink(
            &src(),
            Format::Mkv,
            &probe,
            &Tags::default(),
            CompressPreset::Balanced,
            true,
            &out(),
        );
        assert!(!shrink.copies);
        assert!(has(&shrink.args, ["-c:1", "aac"]), "{:?}", shrink.args);
    }

    #[test]
    fn audio_without_a_layout_keeps_its_tracks_without_an_lfe() {
        let unlabeled = |codec, channels| StreamInfo {
            has_layout: false,
            ..at(audio(codec, 16), 48_000, channels)
        };
        let pan = |layout: &str, channels| {
            let map: String = (0..channels).map(|c| format!("|c{c}=c{c}")).collect();
            format!("pan={layout}{map}")
        };
        for (channels, layout) in [
            (1, "mono"),
            (2, "stereo"),
            (3, "3.0"),
            (4, "quad"),
            (5, "5.0"),
            (6, "hexagonal"),
            (7, "7.0"),
            (8, "octagonal"),
        ] {
            let track = unlabeled("pcm_s16le", channels);
            assert_eq!(label(&track), Some(pan(layout, channels)));
            assert_eq!(super::unlabeled(&track), channels >= 3);
        }
        assert_eq!(label(&unlabeled("pcm_s16le", 9)), None);
        assert_eq!(label(&unlabeled("pcm_s16le", 0)), None);
        assert_eq!(label(&at(audio("pcm_s16le", 16), 48_000, 6)), None);

        // Compress on a video: 6 tracks without a layout, a small AAC that
        // is copied, and a stereo pair without a layout, which the guess
        // being off leaves to `pan` too.
        let mut aac = stream(2, StreamKind::Audio, "aac");
        aac.bit_rate = Some(120_000);
        let six = StreamInfo {
            index: 1,
            ..unlabeled("pcm_s24le", 6)
        };
        let pair = StreamInfo {
            index: 3,
            ..unlabeled("pcm_s16le", 2)
        };
        let streams = vec![stream(0, StreamKind::Video, "h264"), six, aac, pair];
        let probe = probe_of("matroska,webm", streams.clone());
        let shrink = |probe: &Probe| {
            let tags = Tags::default();
            super::shrink(
                &src(),
                Format::Mkv,
                probe,
                &tags,
                CompressPreset::Balanced,
                true,
                &out(),
            )
            .args
        };
        let args = shrink(&probe);
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(text[10..12], ["-guess_layout_max", "0"]);
        assert!(has(&args, ["-b:1", "384k"]), "{args:?}");
        assert!(has(&args, ["-filter:1", &pan("hexagonal", 6)]), "{args:?}");
        assert!(!args.iter().any(|a| a == "-c:2"), "{args:?}");
        assert!(has(&args, ["-filter:3", &pan("stereo", 2)]), "{args:?}");
        // Without the six, the pair is left to FFmpeg's guess, as before.
        let probe = probe_of(
            "matroska,webm",
            vec![streams[0].clone(), streams[2].clone(), streams[3].clone()],
        );
        let args = shrink(&probe);
        assert!(!args.iter().any(|a| a == "-guess_layout_max"), "{args:?}");
        assert!(!args.iter().any(|a| a == "-filter:2"), "{args:?}");

        // Compress into FLAC: the same labels, before any other filter.
        let wav = StreamInfo {
            depth: 24,
            ..at(unlabeled("pcm_s24le", 6), 96_000, 6)
        };
        let squeeze = squeeze(Format::Flac, &wav, None, CompressPreset::Small).unwrap();
        assert_eq!(squeeze.reading, ["-guess_layout_max", "0"]);
        let dither = "aresample=48000:osf=s16:dither_method=triangular";
        assert_eq!(
            squeeze.filter,
            Some(format!("{},{dither}", pan("hexagonal", 6)))
        );
        let cd = unlabeled("pcm_s16le", 4);
        let squeeze = super::squeeze(Format::Flac, &cd, None, CompressPreset::High).unwrap();
        assert_eq!(squeeze.filter, Some(pan("quad", 4)));
        // A stereo WAV keeps FFmpeg's guess, and one with a layout its own.
        for wav in [
            unlabeled("pcm_s16le", 2),
            at(audio("pcm_s16le", 16), 48_000, 6),
        ] {
            let squeeze = super::squeeze(Format::Flac, &wav, None, CompressPreset::High).unwrap();
            assert!(squeeze.reading.is_empty());
            assert_eq!(squeeze.filter, None);
        }
    }

    #[test]
    fn small_does_more_unless_it_would_copy_every_track_too() {
        use CompressPreset::{Balanced, High, Small};
        let none = Tags::default();
        // A picture: always, but at Small.
        assert!(small_does_more(Format::Mp4, &movie(), &none, Balanced));
        assert!(!small_does_more(Format::Mp4, &movie(), &none, Small));
        // Sound alone: AAC at 90k stereo is within Small's 96k, so Small
        // copies it as Balanced does.
        let mut aac = stream(0, StreamKind::Audio, "aac");
        aac.bit_rate = Some(90_000);
        let song = probe_of("mov,mp4,m4a,3gp,3g2,mj2", vec![aac.clone()]);
        assert!(!small_does_more(Format::Mp4, &song, &none, High));
        aac.bit_rate = Some(160_000);
        let song = probe_of("mov,mp4,m4a,3gp,3g2,mj2", vec![aac]);
        assert!(small_does_more(Format::Mp4, &song, &none, Balanced));
        let flac = probe_of("matroska,webm", vec![stream(0, StreamKind::Audio, "flac")]);
        assert!(small_does_more(Format::Mkv, &flac, &none, High));
    }

    /// Whether `args` has `pair` one after the other.    /// Whether `args` has `pair` one after the other.
    fn has(args: &[OsString], pair: [&str; 2]) -> bool {
        crate::media::tests::has(args, pair)
    }

    #[test]
    fn measuring_writes_nothing() {
        let probe = probe_of("mp3", vec![cover(0), stream(1, StreamKind::Audio, "mp3")]);
        let args = measure(&src(), &probe, "ebur128=peak=true:framelog=quiet");
        let mut want = argv(&[
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-v",
            "info",
            "-y",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            "mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif",
            "-i",
            "<src>",
            "-map",
            "0:1",
            "-af",
            "ebur128=peak=true:framelog=quiet",
            "-progress",
            "pipe:1",
            "-f",
            "null",
        ]);
        want.push(OsString::from("-"));
        assert_eq!(args, want);
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
        let bwf = Tags {
            bext: true,
            ..Tags::default()
        };
        assert_eq!(
            argv(Format::Wav, &bwf),
            ["-rf64", "auto", "-write_bext", "1"]
        );
        assert_eq!(argv(Format::Flac, &bwf), Vec::<String>::new());
        assert!(argv(Format::Flac, &none).is_empty());
        assert!(argv(Format::Mkv, &none).is_empty());
        assert_eq!(argv(Format::Mkv, &dated(true)), date);
    }

    #[test]
    fn mute_maps_every_track_but_the_audio() {
        // The cover art (0:6) only in an MP4: the MOV muxer drops it.
        for (to, muxer, cover) in [
            (Format::Mp4, "mp4", &["-map", "0:6"][..]),
            (Format::Mov, "mov", &[]),
        ] {
            let mut middle = vec!["-map", "0:0", "-map", "0:3"];
            middle.extend(cover);
            middle.extend([
                "-c",
                "copy",
                "-strict",
                "unofficial",
                "-metadata",
                "creation_time=2024-05-01T10:20:30.000000Z",
                "-movflags",
                "+faststart",
            ]);
            assert_eq!(
                mute(&src(), to, &movie(), &dated(false), &out()),
                expect(&[], &middle, muxer),
                "{to:?}"
            );
        }
        // Nor in an MP4 with iPhone keys, whose metadata takes the place of
        // the iTunes-style metadata a cover goes in.
        let keyed = mute(&src(), Format::Mp4, &movie(), &dated(true), &out());
        assert!(!keyed.iter().any(|a| a == "0:6"), "{keyed:?}");
        assert_eq!(
            mute(&src(), Format::Mkv, &mkv(), &Tags::default(), &out()),
            expect(
                &[],
                // Not the cover art (0:4), which a copy makes a video.
                &["-map", "0:0", "-map", "0:2", "-map", "0:3", "-c", "copy"],
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
