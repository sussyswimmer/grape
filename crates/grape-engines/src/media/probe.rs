//! `ffprobe`: the argv that asks for just what the planner needs, and a
//! parser for its `-of flat` output.
//!
//! Flat output is one `key=value` line per field, for example
//! `streams.stream.0.codec_type="video"`. Strings are quoted, with `\n`,
//! `\r`, `\\`, `\"`, `` \` `` and `\$` escaped; integers are bare; a
//! missing value is the string `"N/A"`. Checked against FFmpeg 6.1.1 and
//! the 9.0.2 source (`fftools/textformat/tf_flat.c`), which print the same
//! keys.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use super::args::{file_url, DEMUXERS};

/// The fields `-show_entries` asks for. Never tags: titles and comments
/// could contain anything, and the planner doesn't need them (the tools
/// read tag keys with their own probe, [`super::tags`]).
const ENTRIES: &str = "format=format_name,duration,bit_rate\
    :stream=index,codec_type,codec_name,codec_tag_string,width,height,has_b_frames,pix_fmt,\
    color_transfer,avg_frame_rate,sample_rate,channels,channel_layout,sample_fmt,\
    bits_per_sample,bits_per_raw_sample,bit_rate\
    :stream_disposition=attached_pic\
    :stream_side_data=rotation";

/// The `ffprobe` argv for `src` (an absolute path). No `-nostdin`:
/// `ffprobe` doesn't read stdin and rejects that option.
pub(crate) fn probe_args(src: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-v",
        "error",
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        DEMUXERS,
        "-show_entries",
        ENTRIES,
        "-of",
        "flat",
        "-i",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(file_url(src));
    args
}

/// What the planner needs to know about a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probe {
    /// The demuxer's name, such as `"mov,mp4,m4a,3gp,3g2,mj2"`.
    pub(crate) format_name: String,
    /// The container's duration, for progress. `None` when unknown.
    pub(crate) duration: Option<Duration>,
    /// The stream `-map 0:V:0` picks: the first video that isn't cover art.
    pub(crate) video: Option<Video>,
    /// The stream `-map 0:a:0` picks: the first audio.
    pub(crate) audio: Option<Audio>,
    /// Some video stream has B-frames: frames its decoder holds back to
    /// put them in order (`has_b_frames` above 0). FFmpeg then seeks to a
    /// little before the time asked for (`-ss`).
    pub(crate) b_frames: bool,
    /// The container's average bitrate in bits per second, when known.
    #[allow(dead_code)] // The tools (D26) read it from their first commit on.
    pub(crate) bit_rate: Option<u64>,
    /// Every stream, in index order. The tools map streams by their index,
    /// so they keep every track but what they remove.
    #[allow(dead_code)] // The tools (D26) read it from their first commit on.
    pub(crate) streams: Vec<StreamInfo>,
}

/// One stream of the source, for the tools (D26).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // The tools (D26) read it from their first commit on.
pub(crate) struct StreamInfo {
    /// Its index: `-map 0:<index>`.
    pub(crate) index: u32,
    pub(crate) kind: StreamKind,
    /// The codec, such as `"aac"`.
    pub(crate) codec: String,
    /// The codec tag as `ffprobe` prints it, lowercase, such as `"avc1"`,
    /// `"tmcd"` or `"[0][0][0][0]"`.
    pub(crate) tag: String,
    /// Cover art: a picture shown for the file, not a video.
    pub(crate) attached_pic: bool,
    /// Bits per second, when the container says.
    pub(crate) bit_rate: Option<u64>,
    /// Audio: channel count, 0 for other kinds.
    pub(crate) channels: u32,
    /// Audio: see [`Audio::has_layout`].
    pub(crate) has_layout: bool,
    /// Audio: samples per second, 0 for other kinds.
    pub(crate) sample_rate: u32,
    /// Audio: see [`Audio::depth`].
    pub(crate) depth: u32,
    /// Audio: the decoded sample format, such as `"s16"` or `"fltp"`.
    pub(crate) sample_fmt: String,
    /// Video: the transfer function, such as `"arib-std-b67"` (HLG) or
    /// `"smpte2084"` (PQ); empty when unknown.
    pub(crate) color_transfer: String,
    /// Video: the average frame rate as a fraction, when known.
    pub(crate) avg_fps: Option<(u32, u32)>,
}

/// What kind of stream a [`StreamInfo`] is, as `ffprobe`'s `codec_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamKind {
    Video,
    Audio,
    Subtitle,
    /// Data, such as a timecode or camera telemetry track; also a stream
    /// of a kind `ffprobe` calls `"unknown"`.
    Data,
    /// A Matroska attachment, such as a font.
    Attachment,
}

/// A video stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Video {
    /// The codec, such as `"h264"`.
    pub(crate) codec: String,
    /// Coded width in pixels, before rotation.
    pub(crate) width: u32,
    /// Coded height in pixels, before rotation.
    pub(crate) height: u32,
    /// The pixel format, such as `"yuv420p"`.
    pub(crate) pix_fmt: String,
    /// The display rotation in degrees, counter-clockwise as `ffprobe`
    /// reports it, rounded to a quarter turn: 0, 90, 180 or 270.
    pub(crate) rotation: i32,
}

/// An audio stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Audio {
    /// The codec, such as `"aac"`.
    pub(crate) codec: String,
    /// Samples per second.
    pub(crate) sample_rate: u32,
    /// Channel count.
    pub(crate) channels: u32,
    /// False when the stream has no channel layout, only a channel count
    /// (`ffprobe` says `"unknown"`): a WAV without a channel mask, such as
    /// a field recorder's polyphonic WAV or 4-channel ambisonics.
    pub(crate) has_layout: bool,
    /// The channel layout as `ffprobe` names it, such as `"5.1(side)"` or
    /// `"2.1"` (`"unknown"` without one); empty when it prints none.
    pub(crate) layout: String,
    /// Bits per sample of a lossless source (PCM, FLAC, ALAC, WavPack, TTA,
    /// APE); 16 for everything else, whose decoded depth says nothing about
    /// the original.
    pub(crate) depth: u32,
}

/// Parses `ffprobe -of flat` output. Unknown keys are ignored; a line
/// without `=`, an unterminated quoted value or a missing
/// `format.format_name` is an error, described for the log.
pub(crate) fn parse(flat: &str) -> Result<Probe, String> {
    let mut format_name = None;
    let mut duration = None;
    let mut bit_rate = None;
    let mut streams: BTreeMap<u32, Stream> = BTreeMap::new();
    for (n, line) in flat.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (key, raw) = line
            .split_once('=')
            .ok_or_else(|| format!("ffprobe output line {} has no '='", n + 1))?;
        let value = unquote(raw)
            .ok_or_else(|| format!("ffprobe output line {} ({key}) has a broken value", n + 1))?;
        if let Some(rest) = key.strip_prefix("streams.stream.") {
            let Some((position, field)) = rest.split_once('.') else {
                continue;
            };
            if let Ok(position) = position.parse() {
                streams.entry(position).or_default().set(field, value);
            }
        } else if key == "format.format_name" {
            format_name = Some(value);
        } else if key == "format.duration" {
            duration = seconds(&value);
        } else if key == "format.bit_rate" {
            bit_rate = value.parse().ok();
        }
    }
    let format_name = format_name.ok_or("ffprobe printed no format.format_name")?;

    // ffprobe lists streams in index order; sort by the printed index anyway.
    let mut streams: Vec<(u32, Stream)> = streams.into_iter().collect();
    streams.sort_by_key(|(position, stream)| stream.index.unwrap_or(*position));
    let video = streams
        .iter()
        .map(|(_, s)| s)
        .find(|s| s.codec_type == "video" && !s.attached_pic)
        .map(|s| Video {
            codec: s.codec_name.clone(),
            width: s.width,
            height: s.height,
            pix_fmt: s.pix_fmt.clone(),
            rotation: quarter_turn(s.rotation.unwrap_or(0.0)),
        });
    let audio = streams
        .iter()
        .map(|(_, s)| s)
        .find(|s| s.codec_type == "audio")
        .map(|s| Audio {
            codec: s.codec_name.clone(),
            sample_rate: s.sample_rate,
            channels: s.channels,
            // ffprobe 6.1 and 9.0 print "unknown" for a stream without one.
            has_layout: s.channel_layout.as_deref() != Some("unknown"),
            layout: s.channel_layout.clone().unwrap_or_default(),
            depth: depth(s),
        });
    let b_frames = streams
        .iter()
        .any(|(_, s)| s.codec_type == "video" && s.has_b_frames > 0);
    let streams = streams
        .iter()
        .map(|(position, s)| StreamInfo {
            index: s.index.unwrap_or(*position),
            kind: match s.codec_type.as_str() {
                "video" => StreamKind::Video,
                "audio" => StreamKind::Audio,
                "subtitle" => StreamKind::Subtitle,
                "attachment" => StreamKind::Attachment,
                _ => StreamKind::Data,
            },
            codec: s.codec_name.clone(),
            tag: s.codec_tag.to_ascii_lowercase(),
            attached_pic: s.attached_pic,
            bit_rate: s.bit_rate,
            channels: s.channels,
            has_layout: s.channel_layout.as_deref() != Some("unknown"),
            sample_rate: s.sample_rate,
            depth: depth(s),
            sample_fmt: s.sample_fmt.clone(),
            color_transfer: s.color_transfer.clone(),
            avg_fps: s.avg_fps,
        })
        .collect();
    Ok(Probe {
        format_name,
        duration,
        video,
        audio,
        b_frames,
        bit_rate,
        streams,
    })
}

/// The fields of one `streams.stream.N.` group. Numbers that are missing
/// or `"N/A"` are 0.
#[derive(Debug, Default)]
struct Stream {
    index: Option<u32>,
    codec_type: String,
    codec_name: String,
    codec_tag: String,
    width: u32,
    height: u32,
    has_b_frames: u32,
    pix_fmt: String,
    sample_rate: u32,
    channels: u32,
    channel_layout: Option<String>,
    bits_per_sample: u32,
    bits_per_raw_sample: u32,
    sample_fmt: String,
    color_transfer: String,
    avg_fps: Option<(u32, u32)>,
    bit_rate: Option<u64>,
    attached_pic: bool,
    rotation: Option<f64>,
}

impl Stream {
    fn set(&mut self, field: &str, value: String) {
        match field {
            "index" => self.index = value.parse().ok(),
            "codec_type" => self.codec_type = value,
            "codec_name" => self.codec_name = value,
            "codec_tag_string" => self.codec_tag = value,
            "width" => self.width = number(&value),
            "height" => self.height = number(&value),
            "has_b_frames" => self.has_b_frames = number(&value),
            "pix_fmt" => self.pix_fmt = value,
            "sample_rate" => self.sample_rate = number(&value),
            "channels" => self.channels = number(&value),
            "channel_layout" => self.channel_layout = Some(value),
            "bits_per_sample" => self.bits_per_sample = number(&value),
            "bits_per_raw_sample" => self.bits_per_raw_sample = number(&value),
            "sample_fmt" => self.sample_fmt = value,
            "color_transfer" if value != "unknown" => self.color_transfer = value,
            "avg_frame_rate" => self.avg_fps = fraction(&value),
            "bit_rate" => self.bit_rate = value.parse().ok(),
            "disposition.attached_pic" => self.attached_pic = value == "1",
            // 6.1 and 9.0 print side_data_list.side_data.N.rotation; any
            // other nesting a later version picks still ends this way.
            // The display matrix is the only side data with a rotation, so
            // the first one wins.
            _ if self.rotation.is_none()
                && (field == "rotation" || field.ends_with(".rotation")) =>
            {
                self.rotation = value.parse().ok();
            }
            _ => {}
        }
    }
}

/// The value after `=`: a bare token as is, or a quoted string unescaped.
/// `None` when the closing quote is missing or isn't the last character.
pub(crate) fn unquote(raw: &str) -> Option<String> {
    let Some(quoted) = raw.strip_prefix('"') else {
        return Some(raw.to_owned());
    };
    let mut out = String::with_capacity(quoted.len());
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                other => out.push(other),
            },
            '"' => return chars.as_str().is_empty().then_some(out),
            c => out.push(c),
        }
    }
    None
}

/// A non-negative integer field; 0 when missing or `"N/A"`.
fn number(value: &str) -> u32 {
    value.parse().unwrap_or(0)
}

/// A rate as `ffprobe` prints it (`"30000/1001"`); `None` for `"0/0"`,
/// `"N/A"` or a zero on either side.
fn fraction(value: &str) -> Option<(u32, u32)> {
    let (num, den) = value.split_once('/')?;
    let (num, den) = (num.parse().ok()?, den.parse().ok()?);
    (num > 0 && den > 0).then_some((num, den))
}

/// Seconds as `ffprobe` prints them (`"12.345000"`); `None` for `"N/A"`.
fn seconds(value: &str) -> Option<Duration> {
    let secs: f64 = value.parse().ok()?;
    Duration::try_from_secs_f64(secs).ok()
}

/// `degrees` rounded to the nearest quarter turn, as 0, 90, 180 or 270.
fn quarter_turn(degrees: f64) -> i32 {
    // `as` saturates, and NaN becomes 0.
    let quarters = (degrees / 90.0).round() as i64;
    (quarters.rem_euclid(4) * 90) as i32
}

/// See [`Audio::depth`].
fn depth(s: &Stream) -> u32 {
    let lossless = s.codec_name.starts_with("pcm_")
        || matches!(
            s.codec_name.as_str(),
            "flac" | "alac" | "wavpack" | "tta" | "ape"
        );
    if !lossless {
        return 16;
    }
    [s.bits_per_raw_sample, s.bits_per_sample]
        .into_iter()
        .find(|&bits| bits > 0)
        .unwrap_or(16)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    // Golden text: FFmpeg 6.1.1's output for `probe_args` on files made with
    // lavfi (testsrc 160x120 at 10 fps and a 1 s sine).

    /// `-c:v libx264 -pix_fmt yuv420p -c:a aac` into mp4.
    const H264_AAC_MP4: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_name="h264"
streams.stream.0.codec_type="video"
streams.stream.0.width=160
streams.stream.0.height=120
streams.stream.0.has_b_frames=2
streams.stream.0.pix_fmt="yuv420p"
streams.stream.0.bits_per_raw_sample="8"
streams.stream.0.disposition.attached_pic=0
streams.stream.1.index=1
streams.stream.1.codec_name="aac"
streams.stream.1.codec_type="audio"
streams.stream.1.sample_fmt="fltp"
streams.stream.1.sample_rate="44100"
streams.stream.1.channels=1
streams.stream.1.channel_layout="mono"
streams.stream.1.bits_per_sample=0
streams.stream.1.bits_per_raw_sample="N/A"
streams.stream.1.disposition.attached_pic=0
format.format_name="mov,mp4,m4a,3gp,3g2,mj2"
format.duration="1.000000"
"#;

    /// The mp4 above remuxed with `-display_rotation:v:0 -90`.
    const ROTATED_MOV: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_name="h264"
streams.stream.0.codec_type="video"
streams.stream.0.width=160
streams.stream.0.height=120
streams.stream.0.has_b_frames=2
streams.stream.0.pix_fmt="yuv420p"
streams.stream.0.bits_per_raw_sample="8"
streams.stream.0.disposition.attached_pic=0
streams.stream.0.side_data_list.side_data.0.rotation=-90
streams.stream.1.index=1
streams.stream.1.codec_name="aac"
streams.stream.1.codec_type="audio"
streams.stream.1.sample_fmt="fltp"
streams.stream.1.sample_rate="44100"
streams.stream.1.channels=1
streams.stream.1.channel_layout="mono"
streams.stream.1.bits_per_sample=0
streams.stream.1.bits_per_raw_sample="N/A"
streams.stream.1.disposition.attached_pic=0
format.format_name="mov,mp4,m4a,3gp,3g2,mj2"
format.duration="1.000000"
"#;

    /// An mp3 with a PNG cover (`-disposition:v:0 attached_pic`).
    const MP3_WITH_COVER: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_name="mp3"
streams.stream.0.codec_type="audio"
streams.stream.0.sample_fmt="fltp"
streams.stream.0.sample_rate="44100"
streams.stream.0.channels=1
streams.stream.0.channel_layout="mono"
streams.stream.0.bits_per_sample=0
streams.stream.0.bits_per_raw_sample="N/A"
streams.stream.0.disposition.attached_pic=0
streams.stream.1.index=1
streams.stream.1.codec_name="png"
streams.stream.1.codec_type="video"
streams.stream.1.width=64
streams.stream.1.height=64
streams.stream.1.has_b_frames=0
streams.stream.1.pix_fmt="rgb24"
streams.stream.1.bits_per_raw_sample="N/A"
streams.stream.1.disposition.attached_pic=1
format.format_name="mp3"
format.duration="1.044898"
"#;

    /// `-c:a flac -sample_fmt s32 -bits_per_raw_sample 24`.
    const FLAC_24: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_name="flac"
streams.stream.0.codec_type="audio"
streams.stream.0.sample_fmt="s32"
streams.stream.0.sample_rate="44100"
streams.stream.0.channels=1
streams.stream.0.channel_layout="mono"
streams.stream.0.bits_per_sample=0
streams.stream.0.bits_per_raw_sample="24"
streams.stream.0.disposition.attached_pic=0
format.format_name="flac"
format.duration="1.000000"
"#;

    /// A 4-channel WAV without a channel mask (a plain `WAVEFORMATEX`
    /// header), as 6.1.1 and 9.0.2 print it.
    const WAV_WITHOUT_LAYOUT: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_name="pcm_s16le"
streams.stream.0.codec_type="audio"
streams.stream.0.sample_fmt="s16"
streams.stream.0.sample_rate="48000"
streams.stream.0.channels=4
streams.stream.0.channel_layout="unknown"
streams.stream.0.bits_per_sample=16
streams.stream.0.bits_per_raw_sample="N/A"
streams.stream.0.disposition.attached_pic=0
format.format_name="wav"
format.duration="1.000000"
"#;

    // Golden text for the tools' fields (D26): the same probe's output for
    // files made with FFmpeg 6.1.1, read by Grape's 9.0.2 ffprobe; 6.1.1's
    // is the same unless a test says otherwise.

    /// H.264, English and French AAC, SRT subtitles, a TrueType font
    /// attachment and a JPEG attachment (cover art to the demuxer).
    const MOVIE_MKV: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_name="h264"
streams.stream.0.codec_type="video"
streams.stream.0.codec_tag_string="[0][0][0][0]"
streams.stream.0.width=160
streams.stream.0.height=120
streams.stream.0.has_b_frames=2
streams.stream.0.pix_fmt="yuv420p"
streams.stream.0.color_transfer="unknown"
streams.stream.0.avg_frame_rate="10/1"
streams.stream.0.bit_rate="N/A"
streams.stream.0.bits_per_raw_sample="8"
streams.stream.0.disposition.attached_pic=0
streams.stream.1.index=1
streams.stream.1.codec_name="aac"
streams.stream.1.codec_type="audio"
streams.stream.1.codec_tag_string="[0][0][0][0]"
streams.stream.1.sample_fmt="fltp"
streams.stream.1.sample_rate="44100"
streams.stream.1.channels=1
streams.stream.1.channel_layout="mono"
streams.stream.1.bits_per_sample=0
streams.stream.1.avg_frame_rate="0/0"
streams.stream.1.bit_rate="N/A"
streams.stream.1.bits_per_raw_sample="N/A"
streams.stream.1.disposition.attached_pic=0
streams.stream.2.index=2
streams.stream.2.codec_name="aac"
streams.stream.2.codec_type="audio"
streams.stream.2.codec_tag_string="[0][0][0][0]"
streams.stream.2.sample_fmt="fltp"
streams.stream.2.sample_rate="44100"
streams.stream.2.channels=1
streams.stream.2.channel_layout="mono"
streams.stream.2.bits_per_sample=0
streams.stream.2.avg_frame_rate="0/0"
streams.stream.2.bit_rate="N/A"
streams.stream.2.bits_per_raw_sample="N/A"
streams.stream.2.disposition.attached_pic=0
streams.stream.3.index=3
streams.stream.3.codec_name="subrip"
streams.stream.3.codec_type="subtitle"
streams.stream.3.codec_tag_string="[0][0][0][0]"
streams.stream.3.width="N/A"
streams.stream.3.height="N/A"
streams.stream.3.avg_frame_rate="0/0"
streams.stream.3.bit_rate="N/A"
streams.stream.3.bits_per_raw_sample="N/A"
streams.stream.3.disposition.attached_pic=0
streams.stream.4.index=4
streams.stream.4.codec_name="ttf"
streams.stream.4.codec_type="attachment"
streams.stream.4.codec_tag_string="[0][0][0][0]"
streams.stream.4.avg_frame_rate="0/0"
streams.stream.4.bit_rate="N/A"
streams.stream.4.bits_per_raw_sample="N/A"
streams.stream.4.disposition.attached_pic=0
streams.stream.5.index=5
streams.stream.5.codec_name="mjpeg"
streams.stream.5.codec_type="video"
streams.stream.5.codec_tag_string="[0][0][0][0]"
streams.stream.5.width=64
streams.stream.5.height=64
streams.stream.5.has_b_frames=0
streams.stream.5.pix_fmt="yuvj444p"
streams.stream.5.color_transfer="unknown"
streams.stream.5.avg_frame_rate="0/0"
streams.stream.5.bit_rate="N/A"
streams.stream.5.bits_per_raw_sample="8"
streams.stream.5.disposition.attached_pic=1
format.format_name="matroska,webm"
format.duration="1.023000"
format.bit_rate="209485"
"#;

    /// H.264 and AAC with a timecode track (`-timecode`). 9.0.2 lists the
    /// video and the timecode again in a stream group; 6.1.1 doesn't.
    const TIMECODE_MOV: &str = r#"stream_groups.stream_group.0.streams.stream.0.index=0
stream_groups.stream_group.0.streams.stream.0.codec_name="h264"
stream_groups.stream_group.0.streams.stream.0.codec_type="video"
stream_groups.stream_group.0.streams.stream.0.codec_tag_string="avc1"
stream_groups.stream_group.0.streams.stream.0.width=160
stream_groups.stream_group.0.streams.stream.0.height=120
stream_groups.stream_group.0.streams.stream.0.has_b_frames=2
stream_groups.stream_group.0.streams.stream.0.pix_fmt="yuv420p"
stream_groups.stream_group.0.streams.stream.0.color_transfer="unknown"
stream_groups.stream_group.0.streams.stream.0.avg_frame_rate="10/1"
stream_groups.stream_group.0.streams.stream.0.bit_rate="35544"
stream_groups.stream_group.0.streams.stream.0.bits_per_raw_sample="8"
stream_groups.stream_group.0.streams.stream.1.index=2
stream_groups.stream_group.0.streams.stream.1.codec_name="unknown"
stream_groups.stream_group.0.streams.stream.1.codec_type="data"
stream_groups.stream_group.0.streams.stream.1.codec_tag_string="tmcd"
stream_groups.stream_group.0.streams.stream.1.avg_frame_rate="10240/1024"
stream_groups.stream_group.0.streams.stream.1.bit_rate="32"
stream_groups.stream_group.0.streams.stream.1.bits_per_raw_sample="N/A"
streams.stream.0.index=0
streams.stream.0.codec_name="h264"
streams.stream.0.codec_type="video"
streams.stream.0.codec_tag_string="avc1"
streams.stream.0.width=160
streams.stream.0.height=120
streams.stream.0.has_b_frames=2
streams.stream.0.pix_fmt="yuv420p"
streams.stream.0.color_transfer="unknown"
streams.stream.0.avg_frame_rate="10/1"
streams.stream.0.bit_rate="35544"
streams.stream.0.bits_per_raw_sample="8"
streams.stream.0.disposition.attached_pic=0
streams.stream.1.index=1
streams.stream.1.codec_name="aac"
streams.stream.1.codec_type="audio"
streams.stream.1.codec_tag_string="mp4a"
streams.stream.1.sample_fmt="fltp"
streams.stream.1.sample_rate="44100"
streams.stream.1.channels=1
streams.stream.1.channel_layout="mono"
streams.stream.1.bits_per_sample=0
streams.stream.1.avg_frame_rate="0/0"
streams.stream.1.bit_rate="70233"
streams.stream.1.bits_per_raw_sample="N/A"
streams.stream.1.disposition.attached_pic=0
streams.stream.2.index=2
streams.stream.2.codec_name="unknown"
streams.stream.2.codec_type="data"
streams.stream.2.codec_tag_string="tmcd"
streams.stream.2.avg_frame_rate="10240/1024"
streams.stream.2.bit_rate="32"
streams.stream.2.bits_per_raw_sample="N/A"
streams.stream.2.disposition.attached_pic=0
format.format_name="mov,mp4,m4a,3gp,3g2,mj2"
format.duration="1.000000"
format.bit_rate="132296"
"#;

    /// H.264 at 59.94 fps tagged HLG (`-color_trc arib-std-b67`), and
    /// 24-bit PCM.
    const HLG_MOV: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_name="h264"
streams.stream.0.codec_type="video"
streams.stream.0.codec_tag_string="avc1"
streams.stream.0.width=160
streams.stream.0.height=120
streams.stream.0.has_b_frames=2
streams.stream.0.pix_fmt="yuv420p"
streams.stream.0.color_transfer="arib-std-b67"
streams.stream.0.avg_frame_rate="60000/1001"
streams.stream.0.bit_rate="52963"
streams.stream.0.bits_per_raw_sample="8"
streams.stream.0.disposition.attached_pic=0
streams.stream.1.index=1
streams.stream.1.codec_name="pcm_s24le"
streams.stream.1.codec_type="audio"
streams.stream.1.codec_tag_string="in24"
streams.stream.1.sample_fmt="s32"
streams.stream.1.sample_rate="44100"
streams.stream.1.channels=1
streams.stream.1.channel_layout="mono"
streams.stream.1.bits_per_sample=24
streams.stream.1.avg_frame_rate="0/0"
streams.stream.1.bit_rate="1058400"
streams.stream.1.bits_per_raw_sample="24"
streams.stream.1.disposition.attached_pic=0
format.format_name="mov,mp4,m4a,3gp,3g2,mj2"
format.duration="1.001000"
format.bit_rate="1130837"
"#;

    /// 32-bit float PCM.
    const FLOAT_WAV: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_name="pcm_f32le"
streams.stream.0.codec_type="audio"
streams.stream.0.codec_tag_string="[3][0][0][0]"
streams.stream.0.sample_fmt="flt"
streams.stream.0.sample_rate="44100"
streams.stream.0.channels=1
streams.stream.0.channel_layout="mono"
streams.stream.0.bits_per_sample=32
streams.stream.0.avg_frame_rate="0/0"
streams.stream.0.bit_rate="1411200"
streams.stream.0.bits_per_raw_sample="N/A"
streams.stream.0.disposition.attached_pic=0
format.format_name="wav"
format.duration="1.000000"
format.bit_rate="1412112"
"#;

    /// A GIF read from a pipe, where the demuxer can't sum the frame delays.
    const GIF_NO_DURATION: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_name="gif"
streams.stream.0.codec_type="video"
streams.stream.0.width=160
streams.stream.0.height=120
streams.stream.0.has_b_frames=0
streams.stream.0.pix_fmt="bgra"
streams.stream.0.bits_per_raw_sample="N/A"
streams.stream.0.disposition.attached_pic=0
format.format_name="gif"
format.duration="N/A"
"#;

    #[test]
    fn probe_args_are_exact() {
        let src = Path::new(if cfg!(windows) {
            r"C:\Users\me\clip.mp4"
        } else {
            "/home/me/clip.mp4"
        });
        let mut expected: Vec<OsString> = [
            "-hide_banner",
            "-v",
            "error",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            "mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif",
            "-show_entries",
            "format=format_name,duration,bit_rate\
             :stream=index,codec_type,codec_name,codec_tag_string,width,height,has_b_frames,\
             pix_fmt,color_transfer,avg_frame_rate,sample_rate,channels,channel_layout,\
             sample_fmt,bits_per_sample,bits_per_raw_sample,bit_rate\
             :stream_disposition=attached_pic\
             :stream_side_data=rotation",
            "-of",
            "flat",
            "-i",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let mut url = OsString::from("file:");
        url.push(src);
        expected.push(url);
        assert_eq!(probe_args(src), expected);
        assert!(!ENTRIES.contains("tags"));
        assert!(!ENTRIES.contains(' '));
    }

    #[test]
    fn h264_aac_mp4() {
        let probe = parse(H264_AAC_MP4).unwrap();
        assert_eq!(
            probe,
            Probe {
                format_name: "mov,mp4,m4a,3gp,3g2,mj2".into(),
                duration: Some(Duration::from_secs(1)),
                video: Some(Video {
                    codec: "h264".into(),
                    width: 160,
                    height: 120,
                    pix_fmt: "yuv420p".into(),
                    rotation: 0,
                }),
                audio: Some(Audio {
                    codec: "aac".into(),
                    sample_rate: 44100,
                    channels: 1,
                    has_layout: true,
                    layout: "mono".into(),
                    depth: 16,
                }),
                b_frames: true,
                bit_rate: None,
                streams: vec![
                    StreamInfo {
                        index: 0,
                        kind: StreamKind::Video,
                        codec: "h264".into(),
                        tag: String::new(),
                        attached_pic: false,
                        bit_rate: None,
                        channels: 0,
                        has_layout: true,
                        sample_rate: 0,
                        depth: 16,
                        sample_fmt: String::new(),
                        color_transfer: String::new(),
                        avg_fps: None,
                    },
                    StreamInfo {
                        index: 1,
                        kind: StreamKind::Audio,
                        codec: "aac".into(),
                        tag: String::new(),
                        attached_pic: false,
                        bit_rate: None,
                        channels: 1,
                        has_layout: true,
                        sample_rate: 44100,
                        depth: 16,
                        sample_fmt: "fltp".into(),
                        color_transfer: String::new(),
                        avg_fps: None,
                    },
                ],
            }
        );
    }

    /// The stream that `streams` lists at `index`.
    fn stream(probe: &Probe, index: u32) -> &StreamInfo {
        probe.streams.iter().find(|s| s.index == index).unwrap()
    }

    #[test]
    fn every_stream_is_listed_by_index() {
        let probe = parse(MOVIE_MKV).unwrap();
        let kinds: Vec<(u32, StreamKind, &str, bool)> = probe
            .streams
            .iter()
            .map(|s| (s.index, s.kind, s.codec.as_str(), s.attached_pic))
            .collect();
        assert_eq!(
            kinds,
            [
                (0, StreamKind::Video, "h264", false),
                (1, StreamKind::Audio, "aac", false),
                (2, StreamKind::Audio, "aac", false),
                (3, StreamKind::Subtitle, "subrip", false),
                (4, StreamKind::Attachment, "ttf", false),
                (5, StreamKind::Video, "mjpeg", true),
            ]
        );
        assert_eq!(probe.bit_rate, Some(209_485));
        let video = stream(&probe, 0);
        assert_eq!(video.avg_fps, Some((10, 1)));
        assert_eq!(video.color_transfer, "");
        assert_eq!(video.tag, "[0][0][0][0]");
        // Matroska keeps no per-stream bitrate.
        assert_eq!(video.bit_rate, None);
        let audio = stream(&probe, 2);
        assert_eq!(
            (audio.channels, audio.sample_rate, audio.has_layout),
            (1, 44100, true)
        );
        assert_eq!((audio.sample_fmt.as_str(), audio.avg_fps), ("fltp", None));
        // The old fields still pick the first video that isn't cover art and
        // the first audio.
        assert_eq!(probe.video.unwrap().codec, "h264");
        assert_eq!(probe.audio.unwrap().codec, "aac");
    }

    #[test]
    fn data_tracks_and_stream_groups() {
        let probe = parse(TIMECODE_MOV).unwrap();
        assert_eq!(probe.streams.len(), 3, "stream groups repeat streams");
        let timecode = stream(&probe, 2);
        assert_eq!(
            (
                timecode.kind,
                timecode.tag.as_str(),
                timecode.codec.as_str()
            ),
            (StreamKind::Data, "tmcd", "unknown")
        );
        assert_eq!(stream(&probe, 0).tag, "avc1");
        // Tags are kept lowercase, as AVI's "XVID" would be compared.
        let upper = TIMECODE_MOV.replace("\"tmcd\"", "\"TMCD\"");
        assert_eq!(stream(&parse(&upper).unwrap(), 2).tag, "tmcd");
        assert_eq!(stream(&probe, 1).bit_rate, Some(70_233));
        assert_eq!(probe.bit_rate, Some(132_296));
        // 6.1.1 prints no stream group.
        let older: String = TIMECODE_MOV
            .lines()
            .filter(|l| !l.starts_with("stream_groups."))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(parse(&older).unwrap(), probe);
        // A kind ffprobe doesn't know counts as data.
        let unknown = TIMECODE_MOV.replace("codec_type=\"data\"", "codec_type=\"unknown\"");
        assert_eq!(stream(&parse(&unknown).unwrap(), 2).kind, StreamKind::Data);
    }

    #[test]
    fn hlg_and_frame_rates_and_depths() {
        let probe = parse(HLG_MOV).unwrap();
        let video = stream(&probe, 0);
        assert_eq!(video.color_transfer, "arib-std-b67");
        assert_eq!(video.avg_fps, Some((60000, 1001)));
        assert_eq!(video.bit_rate, Some(52_963));
        let pcm = stream(&probe, 1);
        assert_eq!((pcm.depth, pcm.sample_fmt.as_str()), (24, "s32"));
        assert_eq!(pcm.tag, "in24");
        let wav = parse(FLOAT_WAV).unwrap();
        let float = stream(&wav, 0);
        assert_eq!(
            (float.codec.as_str(), float.depth, float.sample_fmt.as_str()),
            ("pcm_f32le", 32, "flt")
        );
        for (rate, fps) in [
            ("30/1", Some((30, 1))),
            ("30000/1001", Some((30000, 1001))),
            ("0/0", None),
            ("0/1", None),
            ("25/0", None),
            ("N/A", None),
            ("30", None),
            ("-30/1", None),
            ("99999999999/1", None),
        ] {
            assert_eq!(fraction(rate), fps, "{rate}");
        }
    }

    #[test]
    fn rotation_is_normalized_to_a_quarter_turn() {
        let probe = parse(ROTATED_MOV).unwrap();
        assert_eq!(probe.video.unwrap().rotation, 270);
        let turns = [
            (0.0, 0),
            (90.0, 90),
            (-90.0, 270),
            (180.0, 180),
            (-180.0, 180),
            (270.0, 270),
            (-270.0, 90),
            (360.0, 0),
            (89.0, 90),
            (f64::NAN, 0),
        ];
        for (degrees, expected) in turns {
            assert_eq!(quarter_turn(degrees), expected, "{degrees}");
        }
    }

    #[test]
    fn cover_art_is_not_the_video() {
        let probe = parse(MP3_WITH_COVER).unwrap();
        assert_eq!(probe.video, None);
        assert_eq!(probe.audio.unwrap().codec, "mp3");
        assert_eq!(probe.duration, Some(Duration::from_secs_f64(1.044898)));

        // A real video after the cover is the one picked.
        let with_video = format!(
            "{MP3_WITH_COVER}streams.stream.2.index=2\n\
             streams.stream.2.codec_type=\"video\"\n\
             streams.stream.2.codec_name=\"h264\"\n\
             streams.stream.2.disposition.attached_pic=0\n"
        );
        assert_eq!(parse(&with_video).unwrap().video.unwrap().codec, "h264");
    }

    #[test]
    fn b_frames_in_any_video_stream_are_noticed() {
        assert!(parse(H264_AAC_MP4).unwrap().b_frames);
        // WMV2, MPEG-4 without them, and images have none.
        let none = H264_AAC_MP4.replace("has_b_frames=2", "has_b_frames=0");
        assert!(!parse(&none).unwrap().b_frames);
        assert!(!parse(MP3_WITH_COVER).unwrap().b_frames);
        assert!(!parse(GIF_NO_DURATION).unwrap().b_frames);
        // FFmpeg's seek looks at every stream, so a second video counts.
        let second = format!(
            "{none}streams.stream.2.index=2\n\
             streams.stream.2.codec_type=\"video\"\n\
             streams.stream.2.codec_name=\"wmv3\"\n\
             streams.stream.2.has_b_frames=1\n"
        );
        assert!(parse(&second).unwrap().b_frames);
    }

    #[test]
    fn a_stream_without_a_channel_layout_is_told_apart() {
        let probe = parse(WAV_WITHOUT_LAYOUT).unwrap();
        let listed = stream(&probe, 0);
        assert_eq!((listed.channels, listed.has_layout), (4, false));
        let audio = probe.audio.unwrap();
        assert_eq!((audio.channels, audio.has_layout), (4, false));
        assert_eq!(audio.layout, "unknown");
        let flac = parse(FLAC_24).unwrap();
        assert!(stream(&flac, 0).has_layout);
        let flac = flac.audio.unwrap();
        assert!(flac.has_layout);
        assert_eq!(flac.layout, "mono");
        // A layout ffprobe doesn't print isn't taken for a missing one.
        let unprinted =
            WAV_WITHOUT_LAYOUT.replace("streams.stream.0.channel_layout=\"unknown\"\n", "");
        let audio = parse(&unprinted).unwrap().audio.unwrap();
        assert!(audio.has_layout);
        assert_eq!(audio.layout, "");
    }

    #[test]
    fn flac_keeps_its_24_bit_depth() {
        let audio = parse(FLAC_24).unwrap().audio.unwrap();
        assert_eq!(audio.depth, 24);
        assert_eq!(audio.codec, "flac");
    }

    #[test]
    fn depth_counts_only_for_lossless_codecs() {
        let stream = |codec: &str, raw: u32, bits: u32| Stream {
            codec_name: codec.into(),
            bits_per_raw_sample: raw,
            bits_per_sample: bits,
            ..Stream::default()
        };
        assert_eq!(depth(&stream("pcm_s16le", 0, 16)), 16);
        assert_eq!(depth(&stream("pcm_s24le", 0, 24)), 24);
        assert_eq!(depth(&stream("pcm_f32le", 0, 32)), 32);
        assert_eq!(depth(&stream("alac", 24, 0)), 24);
        assert_eq!(depth(&stream("flac", 0, 0)), 16);
        // A lossy decoder's depth is its output format's, not the source's.
        assert_eq!(depth(&stream("aac", 0, 32)), 16);
        assert_eq!(depth(&stream("mp3", 24, 0)), 16);
    }

    #[test]
    fn gif_without_duration() {
        let probe = parse(GIF_NO_DURATION).unwrap();
        assert_eq!(probe.format_name, "gif");
        assert_eq!(probe.duration, None);
        assert_eq!(probe.audio, None);
        let video = probe.video.unwrap();
        assert_eq!(
            (video.codec.as_str(), video.width, video.height),
            ("gif", 160, 120)
        );
    }

    #[test]
    fn strings_are_unescaped_and_unknown_keys_ignored() {
        let flat = "format.format_name=\"a\\\"b\\\\c\\nd\\$e\\`f\"\r\n\
                    format.nb_streams=1\r\n\
                    streams.stream.0.codec_type=\"audio\"\r\n\
                    streams.stream.0.codec_name=\"pcm_s24le\"\r\n\
                    streams.stream.0.tags.title=\"x=y\"\r\n\
                    streams.stream.0.sample_rate=\"N/A\"\r\n\
                    streams.stream.0.bits_per_sample=24\r\n\
                    streams.stream=\"no field\"\r\n\
                    chapters.chapter.0.id=0\r\n";
        let probe = parse(flat).unwrap();
        assert_eq!(probe.format_name, "a\"b\\c\nd$e`f");
        assert_eq!(probe.duration, None);
        let audio = probe.audio.unwrap();
        assert_eq!((audio.sample_rate, audio.depth), (0, 24));
    }

    #[test]
    fn malformed_output_is_an_error() {
        let no_equals = "format.format_name=\"wav\"\nError opening input\n";
        assert_eq!(
            parse(no_equals).unwrap_err(),
            "ffprobe output line 2 has no '='"
        );
        assert!(parse("format.format_name=\"wav\n").is_err());
        assert!(parse("format.format_name=\"wav\"x\n").is_err());
        assert!(parse("format.format_name=\"wav\\\"\n").is_err());
        assert!(parse("").is_err());
        assert!(parse("format.duration=\"1.0\"\n").is_err());
        // A negative or non-finite duration is unknown, not an error.
        let odd = "format.format_name=\"wav\"\nformat.duration=\"-1\"\n";
        assert_eq!(parse(odd).unwrap().duration, None);
    }
}
