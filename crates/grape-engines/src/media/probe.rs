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
/// could contain anything, and the planner doesn't need them.
const ENTRIES: &str = "format=format_name,duration\
    :stream=index,codec_type,codec_name,width,height,has_b_frames,pix_fmt,sample_rate,\
    channels,channel_layout,sample_fmt,bits_per_sample,bits_per_raw_sample\
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
    Ok(Probe {
        format_name,
        duration,
        video,
        audio,
        b_frames,
    })
}

/// The fields of one `streams.stream.N.` group. Numbers that are missing
/// or `"N/A"` are 0.
#[derive(Debug, Default)]
struct Stream {
    index: Option<u32>,
    codec_type: String,
    codec_name: String,
    width: u32,
    height: u32,
    has_b_frames: u32,
    pix_fmt: String,
    sample_rate: u32,
    channels: u32,
    channel_layout: Option<String>,
    bits_per_sample: u32,
    bits_per_raw_sample: u32,
    attached_pic: bool,
    rotation: Option<f64>,
}

impl Stream {
    fn set(&mut self, field: &str, value: String) {
        match field {
            "index" => self.index = value.parse().ok(),
            "codec_type" => self.codec_type = value,
            "codec_name" => self.codec_name = value,
            "width" => self.width = number(&value),
            "height" => self.height = number(&value),
            "has_b_frames" => self.has_b_frames = number(&value),
            "pix_fmt" => self.pix_fmt = value,
            "sample_rate" => self.sample_rate = number(&value),
            "channels" => self.channels = number(&value),
            "channel_layout" => self.channel_layout = Some(value),
            "bits_per_sample" => self.bits_per_sample = number(&value),
            "bits_per_raw_sample" => self.bits_per_raw_sample = number(&value),
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
fn unquote(raw: &str) -> Option<String> {
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
            "format=format_name,duration\
             :stream=index,codec_type,codec_name,width,height,has_b_frames,pix_fmt,\
             sample_rate,channels,channel_layout,sample_fmt,bits_per_sample,bits_per_raw_sample\
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
            }
        );
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
        let audio = parse(WAV_WITHOUT_LAYOUT).unwrap().audio.unwrap();
        assert_eq!((audio.channels, audio.has_layout), (4, false));
        assert_eq!(audio.layout, "unknown");
        let flac = parse(FLAC_24).unwrap().audio.unwrap();
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
