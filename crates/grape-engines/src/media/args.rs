//! The `ffmpeg` argv of every pass, from the probe and the target format.
//!
//! Every argv has the same shape: the common prefix (no stdin, quiet,
//! overwrite the temp output, the source as the first input, restricted
//! to local files read by one of the [`DEMUXERS`], after the few options
//! that apply to reading it), then the stream maps, codecs and filters,
//! then `-progress pipe:1`, `-f` and the muxer, and the output as the
//! last argument. Options must exist in both FFmpeg 6.1 and 9.0.
//!
//! - **Video targets** (MP4, MOV, MKV, WebM, AVI, WMV) keep `0:V:0` and
//!   `0:a:0?`: the first video that isn't cover art and the first audio, if
//!   any. MKV, MP4 and MOV copy what fits ([`copy_plan`]) as it is;
//!   everything re-encoded is scaled to even dimensions of at least 2×2,
//!   which yuv420p needs, and for AVI to at most 8190 px a side. FFmpeg
//!   turns a rotated source upright as it decodes.
//! - **GIF** takes two passes: a palette from the whole clip, then the GIF
//!   drawn with it, both at 12 fps and at most 720 px wide. The second
//!   reads an ASF (WMV) source without B-frames from its start again after
//!   probing ([`gif_passes`]).
//! - **Audio targets** (and video to MP3) keep `0:a:0`. Lossless targets
//!   keep 24 bits when the source has more than 16
//!   ([`super::probe::Audio::depth`]); WMA is held to stereo at 48 kHz;
//!   Opus and Vorbis (OPUS, OGG and WebM's audio) get a channel layout
//!   they take, and libopus its surround mapping for 3 to 8 channels
//!   ([`vorbis_layout`]); Vorbis gets at most 192 kHz above its 200 kHz
//!   limit.
//!
//! Only the mapped streams are written: subtitles, data streams, further
//! audio tracks and cover art are dropped, and MP4 and MOV get no timecode
//! track.

use std::ffi::OsString;
use std::path::Path;

use grape_core::Format;

use super::probe::{Audio, Probe};

/// The demuxers a source may be read with, one per container Grape opens:
/// `mov` also reads MP4 and M4A, `matroska` WebM, `asf` WMA and WMV, and
/// `ogg` Opus. Refusing every other demuxer keeps playlists (HLS, DASH,
/// `ffconcat`), which open further files or URLs, out whatever the source
/// is named.
pub(crate) const DEMUXERS: &str = "mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif";

/// Rounds the frame down to even dimensions, at least 2×2. Commas inside
/// an expression are escaped for the filtergraph.
const EVEN_SIZE: &str = r"scale='max(2\,trunc(iw/2)*2)':'max(2\,trunc(ih/2)*2)'";

/// [`EVEN_SIZE`] for AVI's MPEG-4 Part 2, whose encoder refuses a frame
/// 8192 px or more on either side (`mpeg4videoenc.c`): a larger frame is
/// first scaled down to fit 8190×8190, keeping its aspect. Smaller frames
/// come out exactly as with [`EVEN_SIZE`].
const EVEN_SIZE_MPEG4: &str = r"scale='max(2\,trunc(min(iw\,iw*8190/max(iw\,ih))/2)*2)':'max(2\,trunc(min(ih\,ih*8190/max(iw\,ih))/2)*2)'";

/// A GIF's frames: 12 per second, at most 720 px wide. `eof_action=pass`
/// keeps the last frame of a clip shorter than one GIF frame (1/12 s),
/// which `fps` would otherwise round away, leaving no frame at all.
const GIF_FRAMES: &str = r"fps=12:eof_action=pass,scale='min(iw\,720)':-1:flags=lanczos";

/// H.264 for MP4, MOV and MKV.
const X264: &[&str] = &[
    "-c:v", "libx264", "-crf", "20", "-preset", "medium", "-pix_fmt", "yuv420p",
];

/// AAC for MP4, MOV and MKV.
const AAC: &[&str] = &["-c:a", "aac", "-b:a", "192k"];

/// WMA for WMV and WMA, before [`wma_limits`].
const WMA: &[&str] = &["-c:a", "wmav2", "-b:a", "192k"];

/// Remixes the audio to the nearest layout libopus and libvorbis take: the
/// Vorbis ones, mono to 7.1 (`ff_vorbis_ch_layouts`). Neither encoder
/// converts on its own: libopus refuses 5.1(side), the layout AC-3, DTS
/// and 6-channel FLAC decode to, and every other layout (2.1, 4.0...),
/// and libvorbis writes them with the wrong speaker positions.
const VORBIS_LAYOUTS: &str = "aformat=channel_layouts=mono|stereo|3.0|quad|5.0|5.1|6.1|7.1";

/// The Vorbis layouts by channel count, 1 to 8.
const VORBIS_ORDER: [&str; 8] = ["mono", "stereo", "3.0", "quad", "5.0", "5.1", "6.1", "7.1"];

/// libopus's surround mapping ([`vorbis_layout`]).
const SURROUND: &[&str] = &["-mapping_family", "1"];

/// One run of `ffmpeg`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pass {
    /// The argv, without the program.
    pub(crate) args: Vec<OsString>,
    /// This pass's share of the job's progress. The weights of a job's
    /// passes sum to 1.0.
    pub(crate) weight: f32,
}

/// The stream a target needs that the source doesn't have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Missing {
    /// An audio target from a source without audio (a silent video to MP3).
    Audio,
    /// A video or GIF target from a source without video (an audio-only
    /// MP4, or one whose only picture is cover art).
    Video,
}

/// Which streams go into the output as they are (`-c copy`) instead of
/// being re-encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CopyPlan {
    /// The video is copied.
    pub(crate) video: bool,
    /// The audio is copied. False when the source has no audio.
    pub(crate) audio: bool,
}

impl CopyPlan {
    /// True when anything is copied, so a failed pass is worth one retry
    /// with [`reencode_passes`]: a stream can fit by its codec and still
    /// not mux (odd timestamps, missing headers).
    pub(crate) fn any(self) -> bool {
        self.video || self.audio
    }
}

/// `"file:"` followed by `p`, an absolute path, without any lossy
/// conversion here. The prefix makes FFmpeg's file protocol read the rest
/// as a path, so a name like `pipe:1`, `concat:a|b` or `-i` is never a
/// protocol or an option. The file protocol strips `file:`, so on Windows
/// `C:\x\y.mp4` becomes `file:C:\x\y.mp4`. FFmpeg itself, on Windows,
/// turns an unpaired surrogate in its argv into U+FFFD, so `convert`
/// refuses such a path before FFmpeg runs.
pub(crate) fn file_url(p: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(p.as_os_str());
    url
}

/// The muxer FFmpeg writes `to` with, or `None` when FFmpeg doesn't write
/// that format. M4A uses `ipod` (an MP4 that iTunes-era players accept);
/// WMV and WMA use `asf`.
pub(crate) fn muxer(to: Format) -> Option<&'static str> {
    Some(match to {
        Format::Mp4 => "mp4",
        Format::Mov => "mov",
        Format::Mkv => "matroska",
        Format::Webm => "webm",
        Format::Avi => "avi",
        Format::Wmv => "asf",
        Format::Gif => "gif",
        Format::Mp3 => "mp3",
        Format::M4a => "ipod",
        Format::Wav => "wav",
        Format::Flac => "flac",
        Format::Ogg => "ogg",
        Format::Opus => "opus",
        Format::Aiff => "aiff",
        Format::Wma => "asf",
        _ => return None,
    })
}

/// What a conversion to `to` can copy from the source instead of
/// re-encoding:
///
/// - **MKV** copies H.264, HEVC, VP8, VP9 and AV1 video that isn't rotated
///   (Matroska may not keep the rotation, so that video is re-encoded
///   upright), and AAC, MP3, Opus, Vorbis, FLAC, AC-3 and E-AC-3 audio.
///   Each stream is decided on its own.
/// - **MP4 and MOV** copy both or neither: H.264 in yuv420p that isn't
///   rotated, with no audio or AAC or MP3, plays everywhere as it is.
/// - Every other target re-encodes.
///
/// Video from an **AVI or ASF** (WMV) source is never copied: those
/// containers store no presentation times, so copied B-frames would be
/// shown in decode order, and FFmpeg writes that without an error.
pub(crate) fn copy_plan(to: Format, probe: &Probe) -> CopyPlan {
    let Some(video) = &probe.video else {
        return CopyPlan::default();
    };
    let audio = probe.audio.as_ref();
    let timed = !matches!(probe.format_name.as_str(), "avi" | "asf");
    match to {
        Format::Mkv => CopyPlan {
            video: timed
                && video.rotation == 0
                && matches!(
                    video.codec.as_str(),
                    "h264" | "hevc" | "vp8" | "vp9" | "av1"
                ),
            audio: audio.is_some_and(|a| {
                matches!(
                    a.codec.as_str(),
                    "aac" | "mp3" | "opus" | "vorbis" | "flac" | "ac3" | "eac3"
                )
            }),
        },
        Format::Mp4 | Format::Mov => {
            let fits = timed
                && video.codec == "h264"
                && video.pix_fmt == "yuv420p"
                && video.rotation == 0
                && audio.is_none_or(|a| matches!(a.codec.as_str(), "aac" | "mp3"));
            CopyPlan {
                video: fits,
                audio: fits && audio.is_some(),
            }
        }
        _ => CopyPlan::default(),
    }
}

/// The passes that convert `src` (an absolute path) to `to` into `out`,
/// the temp file the caller later publishes. `palette` is a temp path for
/// the GIF target's palette and is unused otherwise. Copies what
/// [`copy_plan`] allows; if that fails, [`reencode_passes`] is the retry.
///
/// `Err` when the source lacks the stream the target needs. Empty for a
/// format FFmpeg doesn't write ([`muxer`] is `None`).
pub(crate) fn passes(
    src: &Path,
    to: Format,
    probe: &Probe,
    out: &Path,
    palette: &Path,
) -> Result<Vec<Pass>, Missing> {
    build(src, to, probe, out, palette, copy_plan(to, probe))
}

/// [`passes`], re-encoding every stream.
pub(crate) fn reencode_passes(
    src: &Path,
    to: Format,
    probe: &Probe,
    out: &Path,
    palette: &Path,
) -> Result<Vec<Pass>, Missing> {
    build(src, to, probe, out, palette, CopyPlan::default())
}

fn build(
    src: &Path,
    to: Format,
    probe: &Probe,
    out: &Path,
    palette: &Path,
    copy: CopyPlan,
) -> Result<Vec<Pass>, Missing> {
    let Some(muxer) = muxer(to) else {
        return Ok(Vec::new());
    };
    if to == Format::Gif {
        probe.video.as_ref().ok_or(Missing::Video)?;
        return Ok(gif_passes(src, probe, out, palette));
    }
    // Opus and Vorbis: OPUS, OGG and WebM's audio.
    let layout = matches!(to, Format::Opus | Format::Ogg | Format::Webm)
        .then(|| vorbis_layout(probe.audio.as_ref(), to != Format::Ogg));
    let reading = layout.as_ref().map_or(&[][..], |layout| layout.reading);
    let mut args = prefix(src, reading);
    match video_encoders(to) {
        Some((video_codec, audio_codec)) => {
            probe.video.as_ref().ok_or(Missing::Video)?;
            let audio = probe.audio.as_ref();
            push(&mut args, &["-map", "0:V:0", "-map", "0:a:0?"]);
            if copy.video && (copy.audio || audio.is_none()) {
                push(&mut args, &["-c", "copy"]);
            } else {
                if copy.video {
                    push(&mut args, &["-c:v", "copy"]);
                } else {
                    push(&mut args, video_codec);
                    let size = if to == Format::Avi {
                        EVEN_SIZE_MPEG4
                    } else {
                        EVEN_SIZE
                    };
                    push(&mut args, &["-vf", size]);
                }
                if copy.audio {
                    push(&mut args, &["-c:a", "copy"]);
                } else {
                    push(&mut args, audio_codec);
                    if let (Format::Wmv, Some(audio)) = (to, audio) {
                        push(&mut args, &wma_limits(audio));
                    }
                    if let Some(layout) = &layout {
                        if layout.surround {
                            push(&mut args, SURROUND);
                        }
                        push(&mut args, &["-af", &layout.filter]);
                    }
                }
            }
            if matches!(to, Format::Mp4 | Format::Mov) {
                // Without `-write_tmcd 0` the muxer turns a camera's
                // timecode (GoPro, DJI, pro cameras) back into a data track.
                push(&mut args, &["-movflags", "+faststart", "-write_tmcd", "0"]);
            }
        }
        None => {
            let audio = probe.audio.as_ref().ok_or(Missing::Audio)?;
            push(&mut args, &["-map", "0:a:0"]);
            push(&mut args, &audio_encoder(to, audio, layout.as_ref()));
        }
    }
    Ok(vec![Pass {
        args: finish(args, muxer, out),
        weight: 1.0,
    }])
}

/// The two GIF passes. The first writes the palette as a single PNG with
/// `image2pipe` (`image2` would expand `%` in the path as a frame number),
/// and fails if it wrote none (`-abort_on empty_output`), which the second
/// would report as an unreadable source; the second reads it back as a
/// second input and draws the GIF with it.
///
/// The second reads an ASF (WMV) source without B-frames from its start
/// again once it's probed (`-ss 0`). The ASF demuxer gives the packets
/// FFmpeg reads while probing no duration unless a parser times them
/// (H.264, MPEG-4 Part 2, HEVC), so in a WMV2 clip of a few seconds every
/// frame has none. When such a source ends before the palette's frame has
/// reached the second pass's filter graph (now and then; more often on a
/// busy machine), FFmpeg 9.0 drops the end time the decoder worked out and
/// ends the stream at the last frame's start plus its duration
/// (`configure_filtergraph` closes the source with
/// `av_buffersrc_add_frame(NULL)`): `fps` then loses the GIF frames the
/// last source frame should cover. Read again after probing, every packet
/// has its duration. Other containers give them one, and the palette pass
/// has no second input to wait for.
///
/// A source with B-frames ([`Probe::b_frames`]) is read once: FFmpeg would
/// seek to 3/23 s before its start, which ASF doesn't take as a rewind but
/// looks up in its index or by a search, and for H.264 and MPEG-4 WMVs that
/// lands past the first frames or at the end. Their packets are timed by
/// the parser anyway; WMV3 and VC-1 ones aren't (a known limit).
fn gif_passes(src: &Path, probe: &Probe, out: &Path, palette: &Path) -> Vec<Pass> {
    let mut first = prefix(src, &[]);
    push(
        &mut first,
        &[
            "-abort_on",
            "empty_output",
            "-map",
            "0:V:0",
            "-vf",
            &format!("{GIF_FRAMES},palettegen=stats_mode=diff"),
            "-frames:v",
            "1",
            "-c:v",
            "png",
        ],
    );

    let rewind = probe.format_name == "asf" && !probe.b_frames;
    let mut second = prefix(src, if rewind { &["-ss", "0"] } else { &[] });
    push(
        &mut second,
        &["-f", "png_pipe", "-protocol_whitelist", "file", "-i"],
    );
    second.push(file_url(palette));
    push(
        &mut second,
        &[
            "-lavfi",
            &format!("[0:V:0]{GIF_FRAMES}[x];[x][1:v]paletteuse"),
            "-an",
        ],
    );

    vec![
        Pass {
            args: finish(first, "image2pipe", palette),
            weight: 0.3,
        },
        Pass {
            args: finish(second, "gif", out),
            weight: 0.7,
        },
    ]
}

/// The video and audio encoders of a video target; `None` for any other
/// format.
fn video_encoders(to: Format) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match to {
        Format::Mp4 | Format::Mov | Format::Mkv => Some((X264, AAC)),
        Format::Webm => Some((
            &[
                "-c:v",
                "libvpx-vp9",
                "-crf",
                "32",
                "-b:v",
                "0",
                "-row-mt",
                "1",
                "-deadline",
                "good",
                "-cpu-used",
                "4",
                "-pix_fmt",
                "yuv420p",
            ],
            &["-c:a", "libopus", "-b:a", "128k"],
        )),
        // XviD's tag, so players that pick a decoder by FourCC find one.
        Format::Avi => Some((
            &["-c:v", "mpeg4", "-q:v", "3", "-tag:v", "XVID"],
            &["-c:a", "libmp3lame", "-b:a", "192k"],
        )),
        Format::Wmv => Some((&["-c:v", "wmv2", "-q:v", "3"], WMA)),
        _ => None,
    }
}

/// How Opus or Vorbis gets a channel layout it takes ([`vorbis_layout`]).
#[derive(Debug, Clone, PartialEq)]
struct Layout {
    /// Options for reading the source, before its `-i`.
    reading: &'static [&'static str],
    /// The `-af` filter.
    filter: String,
    /// libopus gets its surround mapping ([`SURROUND`]).
    surround: bool,
}

/// How Opus (`opus`) or Vorbis gets a layout it takes.
///
/// - Audio with a channel layout is remixed to the nearest Vorbis one
///   ([`VORBIS_LAYOUTS`]).
/// - Audio without one keeps its 1 to 8 channels, unmixed, labelled with
///   the Vorbis layout of that count by `pan`: FFmpeg mustn't guess a
///   layout for it (`-guess_layout_max 0`), since its guesses for 3 and 4
///   channels (2.1, 4.0) aren't Vorbis layouts and would be remixed. Where
///   the encoder would cut the fourth channel as the LFE, they go to the
///   next layout instead, around its LFE, which stays silent: 6 for Vorbis
///   (libvorbis keeps only up to about 250 Hz of 5.1's LFE) and 7 for Opus
///   (below). More channels are downmixed to 7.1 from the layout FFmpeg
///   guesses, which 9.0 has for 10, 12, 14, 16 and 24 channels (6.1 not
///   for 14); other counts fail.
/// - With no audio (a silent WebM) the filter has nothing to do.
///
/// libopus's default mapping puts the channels of 5.0 and 6.1 in the wrong
/// speakers. Its surround mapping places every Vorbis layout right, but
/// codes the fourth channel of 5.1, 6.1 and 7.1 as the LFE, which keeps
/// nothing above about 400 Hz. So libopus gets it for 3 to 8 channels with
/// a layout, and for 3, 4, 5 and 7 without one; 7 go to 7.1 with the LFE
/// silent instead of 6.1, each in a channel of its own. 6 and 8 without a
/// layout, and more downmixed to 7.1, keep the default mapping, which puts
/// them right at full bandwidth. FFmpeg remixes 2.1 to stereo, the only
/// standard layout of 3 or more channels it brings below 3, and mono and
/// stereo keep the default mapping.
fn vorbis_layout(audio: Option<&Audio>, opus: bool) -> Layout {
    match audio {
        Some(audio) if !audio.has_layout && audio.channels > 8 => Layout {
            reading: &[],
            filter: "aformat=channel_layouts=7.1".to_string(),
            surround: false,
        },
        Some(audio) if !audio.has_layout && audio.channels > 0 => {
            let channels = audio.channels as usize;
            let around_lfe = channels == if opus { 7 } else { 6 };
            let layout = VORBIS_ORDER[channels - 1 + usize::from(around_lfe)];
            let mut pan = format!("pan={layout}");
            for channel in 0..channels {
                // The LFE is the fourth channel of 5.1, 6.1 and 7.1.
                let to = channel + usize::from(around_lfe && channel >= 3);
                pan.push_str(&format!("|c{to}=c{channel}"));
            }
            Layout {
                reading: &["-guess_layout_max", "0"],
                filter: pan,
                surround: opus && matches!(channels, 3 | 4 | 5 | 7),
            }
        }
        _ => Layout {
            reading: &[],
            filter: VORBIS_LAYOUTS.to_string(),
            surround: opus && audio.is_some_and(|a| a.channels >= 3 && a.layout != "2.1"),
        },
    }
}

/// The audio encoder of an audio target (`to` is one); `layout` is
/// [`vorbis_layout`]'s, for Opus and Vorbis.
fn audio_encoder<'a>(to: Format, audio: &Audio, layout: Option<&'a Layout>) -> Vec<&'a str> {
    let filter = layout.map_or("", |layout| layout.filter.as_str());
    let deep = audio.depth > 16;
    match to {
        Format::Mp3 => vec!["-c:a", "libmp3lame", "-q:a", "2", "-id3v2_version", "3"],
        Format::M4a => vec!["-c:a", "aac", "-b:a", "256k", "-movflags", "+faststart"],
        Format::Wav => {
            let codec = if deep { "pcm_s24le" } else { "pcm_s16le" };
            // RF64 only past 4 GiB, where plain WAV can't go.
            vec!["-c:a", codec, "-rf64", "auto"]
        }
        Format::Aiff => vec!["-c:a", if deep { "pcm_s24be" } else { "pcm_s16be" }],
        Format::Flac => {
            // FLAC stores s32 samples as 24 bits.
            let samples = if deep { "s32" } else { "s16" };
            vec![
                "-c:a",
                "flac",
                "-compression_level",
                "8",
                "-sample_fmt",
                samples,
            ]
        }
        Format::Ogg => {
            let mut args = vec!["-c:a", "libvorbis", "-q:a", "6", "-af", filter];
            if audio.sample_rate > VORBIS_MAX_RATE {
                args.extend(["-ar", "192000"]);
            }
            args
        }
        Format::Opus => {
            let mut args = vec!["-c:a", "libopus", "-b:a", "128k"];
            if layout.is_some_and(|layout| layout.surround) {
                args.extend(SURROUND);
            }
            args.extend(["-af", filter]);
            args
        }
        Format::Wma => {
            let mut args = WMA.to_vec();
            args.extend(wma_limits(audio));
            args
        }
        _ => Vec::new(),
    }
}

/// The longest AIFF file: 8 bytes of `FORM` header, then a size that is
/// 32 bits. FFmpeg writes a longer one without an error, with every size
/// in it wrapped, so players see a short or broken file.
pub(crate) const AIFF_MAX_LEN: u64 = u32::MAX as u64 + 8;

/// How many bytes of samples an AIFF of `probe`'s audio would hold, from
/// its duration, rate, channels and depth ([`audio_encoder`] writes 16 or
/// 24 bits). `None` when the duration or the audio is unknown.
pub(crate) fn aiff_bytes(probe: &Probe) -> Option<u64> {
    let audio = probe.audio.as_ref()?;
    let bytes_per_sample = if audio.depth > 16 { 3.0 } else { 2.0 };
    let bytes = probe.duration?.as_secs_f64()
        * f64::from(audio.sample_rate)
        * f64::from(audio.channels)
        * bytes_per_sample;
    // `as` saturates.
    Some(bytes as u64)
}

/// The highest sample rate libvorbis encodes: its setups end at 200 kHz
/// (`lib/modes/setup_X.h`, 1.3.7), and FFmpeg doesn't resample for it, so
/// a faster source (DXD's 352.8 kHz, 384 kHz) goes to 192 kHz.
const VORBIS_MAX_RATE: u32 = 200_000;

/// `wmav2` takes at most 2 channels and 48 kHz, and FFmpeg doesn't convert
/// for it on its own: it fails instead.
fn wma_limits(audio: &Audio) -> Vec<&'static str> {
    let mut args = Vec::new();
    if audio.channels > 2 {
        args.extend(["-ac", "2"]);
    }
    if audio.sample_rate > 48_000 {
        args.extend(["-ar", "48000"]);
    }
    args
}

/// The start of every conversion's argv: [`prefix_at`] with errors only.
fn prefix(src: &Path, reading: &[&str]) -> Vec<OsString> {
    prefix_at(src, reading, "error")
}

/// The start of every argv: no stdin, no banner or stats, FFmpeg's log at
/// `level` (`"error"`; `"info"` for a pass whose stderr is read, such as a
/// loudness measurement), overwrite the (temp) output, and the source as
/// input 0, read only from a local file by one of the [`DEMUXERS`], with
/// the options in `reading`.
pub(crate) fn prefix_at(src: &Path, reading: &[&str], level: &str) -> Vec<OsString> {
    let mut args = Vec::new();
    push(
        &mut args,
        &[
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-v",
            level,
            "-y",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            DEMUXERS,
        ],
    );
    push(&mut args, reading);
    push(&mut args, &["-i"]);
    args.push(file_url(src));
    args
}

/// The end of every argv: progress as `key=value` lines on stdout, the
/// muxer, and the output as the last argument.
pub(crate) fn finish(mut args: Vec<OsString>, muxer: &str, out: &Path) -> Vec<OsString> {
    push(&mut args, &["-progress", "pipe:1", "-f", muxer]);
    args.push(file_url(out));
    args
}

pub(crate) fn push(args: &mut Vec<OsString>, list: &[&str]) {
    args.extend(list.iter().map(OsString::from));
}

#[cfg(test)]
pub(in crate::media) mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::media::probe::Video;

    /// The even-size filter, spelled out so a change to it shows here.
    const EVEN: &str = r"scale='max(2\,trunc(iw/2)*2)':'max(2\,trunc(ih/2)*2)'";

    /// The even-size filter of AVI, capped at 8190 px a side.
    const EVEN_MPEG4: &str = r"scale='max(2\,trunc(min(iw\,iw*8190/max(iw\,ih))/2)*2)':'max(2\,trunc(min(ih\,ih*8190/max(iw\,ih))/2)*2)'";

    /// The layout filter of Opus and Vorbis, spelled out.
    const LAYOUTS: &str = "aformat=channel_layouts=mono|stereo|3.0|quad|5.0|5.1|6.1|7.1";

    /// The start of every argv, with `<src>` for the source's URL.
    const PREFIX: [&str; 12] = [
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
        "-i",
        "<src>",
    ];

    /// Every format FFmpeg converts, inputs and targets alike.
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

    /// A folder whose name has a '%', spaces, a leading '-', a ':' (not
    /// allowed on Windows) and non-ASCII letters.
    fn dir() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"C:\Users\Zoë\100% -x y"
        } else {
            "/home/zoë/100% -x: y"
        })
    }

    fn src() -> PathBuf {
        dir().join("-clip ü%d.mov")
    }

    fn out() -> PathBuf {
        dir().join(".grape-tmp-1-2-3.out")
    }

    fn palette() -> PathBuf {
        dir().join(".grape-tmp-1-2-4.png")
    }

    /// The planned final name, which must never reach FFmpeg.
    fn dest() -> PathBuf {
        dir().join("-clip ü%d.mp4")
    }

    /// `list` as an argv, with `<src>`, `<out>` and `<palette>` replaced by
    /// those paths' URLs.
    fn argv(list: &[&str]) -> Vec<OsString> {
        list.iter()
            .map(|&arg| match arg {
                "<src>" => file_url(&src()),
                "<out>" => file_url(&out()),
                "<palette>" => file_url(&palette()),
                arg => OsString::from(arg),
            })
            .collect()
    }

    /// A one-pass argv: the prefix, `middle`, then progress, the muxer and
    /// the output.
    fn expect(middle: &[&str], muxer: &str) -> Vec<OsString> {
        let mut list = PREFIX.to_vec();
        list.extend_from_slice(middle);
        list.extend_from_slice(&["-progress", "pipe:1", "-f", muxer, "<out>"]);
        argv(&list)
    }

    fn video(codec: &str, pix_fmt: &str, rotation: i32) -> Video {
        Video {
            codec: codec.into(),
            width: 160,
            height: 120,
            pix_fmt: pix_fmt.into(),
            rotation,
        }
    }

    fn audio(codec: &str, sample_rate: u32, channels: u32, depth: u32) -> Audio {
        Audio {
            codec: codec.into(),
            sample_rate,
            channels,
            has_layout: true,
            layout: String::new(),
            depth,
        }
    }

    /// PCM audio of `channels` channels without a channel layout.
    fn unlabeled(channels: u32) -> Audio {
        Audio {
            has_layout: false,
            ..audio("pcm_s16le", 48_000, channels, 16)
        }
    }

    fn probe(video: Option<Video>, audio: Option<Audio>) -> Probe {
        Probe {
            format_name: "test".into(),
            duration: Some(Duration::from_secs(1)),
            video,
            audio,
            b_frames: false,
            bit_rate: None,
            streams: Vec::new(),
        }
    }

    /// A typical file of each media format.
    fn typical(from: Format) -> Probe {
        let v = |codec: &str, pix_fmt: &str| Some(video(codec, pix_fmt, 0));
        let a = |codec: &str, rate: u32| Some(audio(codec, rate, 2, 16));
        match from {
            Format::Mp4 | Format::Mov | Format::Mkv => {
                probe(v("h264", "yuv420p"), a("aac", 48_000))
            }
            Format::Webm => probe(v("vp9", "yuv420p"), a("opus", 48_000)),
            Format::Avi => probe(v("mpeg4", "yuv420p"), a("mp3", 44_100)),
            Format::Wmv => probe(v("wmv2", "yuv420p"), a("wmav2", 44_100)),
            Format::Gif => probe(v("gif", "bgra"), None),
            Format::Mp3 => probe(None, a("mp3", 44_100)),
            Format::M4a => probe(None, a("aac", 44_100)),
            Format::Wav => probe(None, a("pcm_s16le", 44_100)),
            Format::Flac => probe(None, a("flac", 44_100)),
            Format::Ogg => probe(None, a("vorbis", 44_100)),
            Format::Opus => probe(None, a("opus", 48_000)),
            Format::Aiff => probe(None, a("pcm_s16be", 44_100)),
            Format::Wma => probe(None, a("wmav2", 44_100)),
            other => panic!("{other:?} isn't a media format"),
        }
    }

    fn passes_for(probe: &Probe, to: Format) -> Result<Vec<Pass>, Missing> {
        passes(&src(), to, probe, &out(), &palette())
    }

    /// Asserts that converting a `probe` source to `to` is one pass with
    /// exactly the argv `expect(middle, muxer)`.
    fn assert_one_pass(probe: &Probe, to: Format, middle: &[&str], muxer: &str) {
        let passes = passes_for(probe, to).unwrap();
        assert_eq!(passes.len(), 1, "{to:?}");
        assert_eq!(passes[0].args, expect(middle, muxer), "{to:?}");
        assert!((passes[0].weight - 1.0).abs() < f32::EPSILON);
    }

    /// True when `pair` appears as two adjacent arguments.
    fn has(args: &[OsString], pair: [&str; 2]) -> bool {
        args.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1])
    }

    #[test]
    fn video_targets_exact_argv() {
        let reencode = [
            "-map",
            "0:V:0",
            "-map",
            "0:a:0?",
            "-c:v",
            "libx264",
            "-crf",
            "20",
            "-preset",
            "medium",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            EVEN,
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-movflags",
            "+faststart",
            "-write_tmcd",
            "0",
        ];
        assert_one_pass(&typical(Format::Webm), Format::Mp4, &reencode, "mp4");
        assert_one_pass(&typical(Format::Avi), Format::Mov, &reencode, "mov");
        assert_one_pass(
            &typical(Format::Mp4),
            Format::Mkv,
            &["-map", "0:V:0", "-map", "0:a:0?", "-c", "copy"],
            "matroska",
        );
        assert_one_pass(
            &typical(Format::Mp4),
            Format::Webm,
            &[
                "-map",
                "0:V:0",
                "-map",
                "0:a:0?",
                "-c:v",
                "libvpx-vp9",
                "-crf",
                "32",
                "-b:v",
                "0",
                "-row-mt",
                "1",
                "-deadline",
                "good",
                "-cpu-used",
                "4",
                "-pix_fmt",
                "yuv420p",
                "-vf",
                EVEN,
                "-c:a",
                "libopus",
                "-b:a",
                "128k",
                "-af",
                LAYOUTS,
            ],
            "webm",
        );
        assert_one_pass(
            &typical(Format::Mov),
            Format::Avi,
            &[
                "-map",
                "0:V:0",
                "-map",
                "0:a:0?",
                "-c:v",
                "mpeg4",
                "-q:v",
                "3",
                "-tag:v",
                "XVID",
                "-vf",
                EVEN_MPEG4,
                "-c:a",
                "libmp3lame",
                "-b:a",
                "192k",
            ],
            "avi",
        );
        assert_one_pass(
            &typical(Format::Mkv),
            Format::Wmv,
            &[
                "-map", "0:V:0", "-map", "0:a:0?", "-c:v", "wmv2", "-q:v", "3", "-vf", EVEN,
                "-c:a", "wmav2", "-b:a", "192k",
            ],
            "asf",
        );
    }

    #[test]
    fn gif_target_exact_argv() {
        let passes = passes_for(&typical(Format::Mp4), Format::Gif).unwrap();
        let mut first = PREFIX.to_vec();
        first.extend_from_slice(&[
            "-abort_on",
            "empty_output",
            "-map",
            "0:V:0",
            "-vf",
            r"fps=12:eof_action=pass,scale='min(iw\,720)':-1:flags=lanczos,palettegen=stats_mode=diff",
            "-frames:v",
            "1",
            "-c:v",
            "png",
            "-progress",
            "pipe:1",
            "-f",
            "image2pipe",
            "<palette>",
        ]);
        let mut second = PREFIX.to_vec();
        second.extend_from_slice(&[
            "-f",
            "png_pipe",
            "-protocol_whitelist",
            "file",
            "-i",
            "<palette>",
            "-lavfi",
            r"[0:V:0]fps=12:eof_action=pass,scale='min(iw\,720)':-1:flags=lanczos[x];[x][1:v]paletteuse",
            "-an",
            "-progress",
            "pipe:1",
            "-f",
            "gif",
            "<out>",
        ]);
        assert_eq!(
            passes,
            [
                Pass {
                    args: argv(&first),
                    weight: 0.3
                },
                Pass {
                    args: argv(&second),
                    weight: 0.7
                },
            ]
        );
        // The paths have a '%' (their folder's), which image2pipe and the
        // gif muxer write as is.
        assert!(palette().to_string_lossy().contains('%'));
    }

    #[test]
    fn a_gif_pass_reads_an_asf_source_without_b_frames_from_its_start_again() {
        let wmv = Probe {
            format_name: "asf".into(),
            ..typical(Format::Wmv)
        };
        let passes = passes_for(&wmv, Format::Gif).unwrap();
        assert_eq!(passes.len(), 2);
        let rewound = |args: &[OsString]| args.iter().any(|arg| arg == "-ss");
        // The GIF pass, from the start; the palette pass reads it once.
        let opening = &PREFIX[..PREFIX.len() - 2];
        let mut start = opening.to_vec();
        start.extend_from_slice(&["-ss", "0", "-i", "<src>"]);
        assert!(passes[1].args.starts_with(&argv(&start)), "{passes:?}");
        assert_eq!(passes[1].args.iter().filter(|a| *a == "-ss").count(), 1);
        assert!(passes[0].args.starts_with(&argv(&PREFIX)), "{passes:?}");
        assert!(!rewound(&passes[0].args));
        // With B-frames, both passes read it once, as in any container.
        let b_frames = Probe {
            b_frames: true,
            ..wmv.clone()
        };
        let mut sources = vec![b_frames];
        for format_name in ["mov,mp4,m4a,3gp,3g2,mj2", "matroska,webm", "avi"] {
            for b_frames in [false, true] {
                sources.push(Probe {
                    format_name: format_name.into(),
                    b_frames,
                    ..typical(Format::Mp4)
                });
            }
        }
        for source in &sources {
            let passes = passes_for(source, Format::Gif).unwrap();
            assert!(!passes.iter().any(|pass| rewound(&pass.args)), "{passes:?}");
        }
        // Other targets from ASF read it once.
        for to in [Format::Mp4, Format::Webm, Format::Mp3, Format::Avi] {
            assert!(!rewound(&passes_for(&wmv, to).unwrap()[0].args), "{to:?}");
        }
    }

    #[test]
    fn audio_targets_exact_argv() {
        let cases: [(Format, Format, &[&str], &str); 8] = [
            (
                Format::Mp4,
                Format::Mp3,
                &["-c:a", "libmp3lame", "-q:a", "2", "-id3v2_version", "3"],
                "mp3",
            ),
            (
                Format::Mp3,
                Format::M4a,
                &["-c:a", "aac", "-b:a", "256k", "-movflags", "+faststart"],
                "ipod",
            ),
            (
                Format::Flac,
                Format::Wav,
                &["-c:a", "pcm_s16le", "-rf64", "auto"],
                "wav",
            ),
            (
                Format::Wav,
                Format::Flac,
                &[
                    "-c:a",
                    "flac",
                    "-compression_level",
                    "8",
                    "-sample_fmt",
                    "s16",
                ],
                "flac",
            ),
            (
                Format::Opus,
                Format::Ogg,
                &["-c:a", "libvorbis", "-q:a", "6", "-af", LAYOUTS],
                "ogg",
            ),
            (
                Format::Ogg,
                Format::Opus,
                &["-c:a", "libopus", "-b:a", "128k", "-af", LAYOUTS],
                "opus",
            ),
            (Format::Wav, Format::Aiff, &["-c:a", "pcm_s16be"], "aiff"),
            (
                Format::Mp3,
                Format::Wma,
                &["-c:a", "wmav2", "-b:a", "192k"],
                "asf",
            ),
        ];
        for (from, to, codec, muxer) in cases {
            let mut middle = vec!["-map", "0:a:0"];
            middle.extend_from_slice(codec);
            assert_one_pass(&typical(from), to, &middle, muxer);
        }
    }

    /// Checks one job's passes against the rules every argv follows.
    fn assert_invariants(passes: &[Pass], to: Format) {
        assert!(!passes.is_empty(), "{to:?}");
        let total: f32 = passes.iter().map(|pass| pass.weight).sum();
        assert!((total - 1.0).abs() < 1e-6, "{to:?}: weights sum to {total}");
        // The prefix, maybe with options for reading the source before its
        // `-i`.
        let (opening, input) = PREFIX.split_at(PREFIX.len() - 2);
        let (opening, input) = (argv(opening), argv(input));
        let (src, out) = (file_url(&src()), file_url(&out()));
        let dest = dest().into_os_string().into_string().unwrap();
        for (i, pass) in passes.iter().enumerate() {
            let args = &pass.args;
            let last = i + 1 == passes.len();
            assert!(args.starts_with(&opening), "{to:?}: {args:?}");
            let source = args.windows(2).position(|w| w == input).unwrap();
            let reading = &args[opening.len()..source];
            assert!(
                reading
                    .iter()
                    .all(|arg| arg == "-guess_layout_max" || arg == "-ss" || arg == "0"),
                "{to:?}: {args:?}"
            );
            let (muxer, output) = if last {
                (muxer(to).unwrap(), out.clone())
            } else {
                ("image2pipe", file_url(&palette()))
            };
            let tail: Vec<OsString> = vec![
                "-progress".into(),
                "pipe:1".into(),
                "-f".into(),
                muxer.into(),
                output,
            ];
            assert!(args.ends_with(&tail), "{to:?}: {args:?}");
            let count = |wanted: &OsString| args.iter().filter(|arg| *arg == wanted).count();
            assert_eq!(count(&src), 1, "the source is input 0 only");
            assert_eq!(count(&out), usize::from(last), "the output is last only");
            assert_eq!(count(&"-progress".into()), 1);
            for arg in args {
                let arg = arg.to_str().unwrap();
                assert!(!arg.contains(&dest), "{to:?}: the final name leaked: {arg}");
            }
            let maps: Vec<&OsString> = args
                .windows(2)
                .filter(|w| w[0] == "-map")
                .map(|w| &w[1])
                .collect();
            let expected: &[&str] = match to {
                Format::Gif if last => &[],
                Format::Gif => &["0:V:0"],
                Format::Mp4
                | Format::Mov
                | Format::Mkv
                | Format::Webm
                | Format::Avi
                | Format::Wmv => &["0:V:0", "0:a:0?"],
                _ => &["0:a:0"],
            };
            assert_eq!(maps, expected, "{to:?}");
            if has(args, ["-c", "copy"]) || has(args, ["-c:v", "copy"]) {
                assert!(
                    !args.iter().any(|arg| arg == "-vf"),
                    "{to:?}: filter on a copy"
                );
            }
            // MP4 and MOV never write a timecode track.
            let tmcd = matches!(to, Format::Mp4 | Format::Mov) && last;
            assert_eq!(has(args, ["-write_tmcd", "0"]), tmcd, "{to:?}");
            // Opus and Vorbis always get the layout filter (these sources
            // have layouts), as the only audio filter; nothing else gets
            // one.
            let vorbis_family = has(args, ["-c:a", "libopus"]) || has(args, ["-c:a", "libvorbis"]);
            let filters: Vec<&OsString> = args
                .windows(2)
                .filter(|w| w[0] == "-af")
                .map(|w| &w[1])
                .collect();
            let expected: &[&str] = if vorbis_family { &[LAYOUTS] } else { &[] };
            assert_eq!(filters, expected, "{to:?}");
            // These sources are mono or stereo: libopus keeps its default
            // mapping.
            assert!(!args.iter().any(|arg| arg == "-mapping_family"), "{to:?}");
        }
    }

    #[test]
    fn every_media_pair_keeps_the_argv_invariants() {
        let mut pairs = 0;
        for from in MEDIA {
            for &to in grape_core::targets(from) {
                assert!(MEDIA.contains(&to), "{from:?} -> {to:?}");
                pairs += 1;
                let probe = typical(from);
                assert_invariants(&passes_for(&probe, to).unwrap(), to);
                let retry = reencode_passes(&src(), to, &probe, &out(), &palette()).unwrap();
                assert_invariants(&retry, to);
            }
        }
        assert_eq!(pairs, 104);
    }

    #[test]
    fn h264_yuv420p_with_aac_or_mp3_is_copied_into_mp4_and_mov() {
        let both = CopyPlan {
            video: true,
            audio: true,
        };
        let aac = probe(
            Some(video("h264", "yuv420p", 0)),
            Some(audio("aac", 48_000, 2, 16)),
        );
        let mp3 = probe(
            Some(video("h264", "yuv420p", 0)),
            Some(audio("mp3", 44_100, 2, 16)),
        );
        let silent = probe(Some(video("h264", "yuv420p", 0)), None);
        let copy = [
            "-map",
            "0:V:0",
            "-map",
            "0:a:0?",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            "-write_tmcd",
            "0",
        ];
        for (to, muxer) in [(Format::Mp4, "mp4"), (Format::Mov, "mov")] {
            assert_eq!(copy_plan(to, &aac), both);
            assert_eq!(copy_plan(to, &mp3), both);
            assert_eq!(
                copy_plan(to, &silent),
                CopyPlan {
                    video: true,
                    audio: false
                }
            );
            for source in [&aac, &mp3, &silent] {
                assert_one_pass(source, to, &copy, muxer);
            }
        }

        // Anything else re-encodes both streams.
        let others = [
            probe(
                Some(video("h264", "yuv420p10le", 0)),
                Some(audio("aac", 48_000, 2, 16)),
            ),
            probe(
                Some(video("h264", "yuvj420p", 0)),
                Some(audio("aac", 48_000, 2, 16)),
            ),
            probe(
                Some(video("h264", "yuv420p", 0)),
                Some(audio("opus", 48_000, 2, 16)),
            ),
            probe(
                Some(video("hevc", "yuv420p", 0)),
                Some(audio("aac", 48_000, 2, 16)),
            ),
        ];
        for source in &others {
            assert_eq!(copy_plan(Format::Mp4, source), CopyPlan::default());
            let args = &passes_for(source, Format::Mp4).unwrap()[0].args;
            assert!(has(args, ["-c:v", "libx264"]) && has(args, ["-c:a", "aac"]));
        }
    }

    #[test]
    fn rotated_video_is_re_encoded_upright() {
        for rotation in [90, 180, 270] {
            let rotated = probe(
                Some(video("h264", "yuv420p", rotation)),
                Some(audio("aac", 48_000, 2, 16)),
            );
            for to in [Format::Mp4, Format::Mov] {
                assert_eq!(copy_plan(to, &rotated), CopyPlan::default());
                let args = &passes_for(&rotated, to).unwrap()[0].args;
                assert!(has(args, ["-c:v", "libx264"]) && has(args, ["-vf", EVEN]));
            }
            // MKV still copies the audio.
            assert_eq!(
                copy_plan(Format::Mkv, &rotated),
                CopyPlan {
                    video: false,
                    audio: true
                }
            );
            assert_one_pass(
                &rotated,
                Format::Mkv,
                &[
                    "-map", "0:V:0", "-map", "0:a:0?", "-c:v", "libx264", "-crf", "20", "-preset",
                    "medium", "-pix_fmt", "yuv420p", "-vf", EVEN, "-c:a", "copy",
                ],
                "matroska",
            );
        }
    }

    #[test]
    fn video_from_avi_or_asf_is_never_copied() {
        for format_name in ["avi", "asf"] {
            let source = Probe {
                format_name: format_name.into(),
                ..probe(
                    Some(video("h264", "yuv420p", 0)),
                    Some(audio("mp3", 44_100, 2, 16)),
                )
            };
            // MP4 and MOV copy both or neither.
            for to in [Format::Mp4, Format::Mov] {
                assert_eq!(copy_plan(to, &source), CopyPlan::default(), "{to:?}");
                let args = &passes_for(&source, to).unwrap()[0].args;
                assert!(
                    has(args, ["-c:v", "libx264"]) && has(args, ["-c:a", "aac"]),
                    "{format_name} to {to:?}"
                );
            }
            // MKV still copies the audio.
            assert_eq!(
                copy_plan(Format::Mkv, &source),
                CopyPlan {
                    video: false,
                    audio: true
                }
            );
            let args = &passes_for(&source, Format::Mkv).unwrap()[0].args;
            assert!(has(args, ["-c:v", "libx264"]) && has(args, ["-c:a", "copy"]));
        }
        // The same streams from a container with presentation times copy.
        let mkv = Probe {
            format_name: "matroska,webm".into(),
            ..probe(
                Some(video("h264", "yuv420p", 0)),
                Some(audio("mp3", 44_100, 2, 16)),
            )
        };
        assert!(copy_plan(Format::Mp4, &mkv).video && copy_plan(Format::Mkv, &mkv).video);
    }

    #[test]
    fn hevc_is_copied_into_mkv_but_re_encoded_for_mp4() {
        let hevc = probe(
            Some(video("hevc", "yuv420p10le", 0)),
            Some(audio("eac3", 48_000, 6, 16)),
        );
        assert_eq!(
            copy_plan(Format::Mkv, &hevc),
            CopyPlan {
                video: true,
                audio: true
            }
        );
        assert_one_pass(
            &hevc,
            Format::Mkv,
            &["-map", "0:V:0", "-map", "0:a:0?", "-c", "copy"],
            "matroska",
        );
        assert_eq!(copy_plan(Format::Mp4, &hevc), CopyPlan::default());
        let args = &passes_for(&hevc, Format::Mp4).unwrap()[0].args;
        assert!(has(args, ["-c:v", "libx264"]) && has(args, ["-pix_fmt", "yuv420p"]));
        for codec in ["h264", "vp8", "vp9", "av1"] {
            let source = probe(Some(video(codec, "yuv420p", 0)), None);
            assert!(copy_plan(Format::Mkv, &source).video, "{codec}");
        }
        for codec in ["mpeg4", "wmv2", "gif", "ffv1", "prores"] {
            let source = probe(Some(video(codec, "yuv420p", 0)), None);
            assert!(!copy_plan(Format::Mkv, &source).any(), "{codec}");
        }
    }

    #[test]
    fn pcm_audio_alone_is_re_encoded_for_mkv() {
        let pcm = probe(
            Some(video("h264", "yuv420p", 0)),
            Some(audio("pcm_s16le", 48_000, 2, 16)),
        );
        assert_eq!(
            copy_plan(Format::Mkv, &pcm),
            CopyPlan {
                video: true,
                audio: false
            }
        );
        assert_one_pass(
            &pcm,
            Format::Mkv,
            &[
                "-map", "0:V:0", "-map", "0:a:0?", "-c:v", "copy", "-c:a", "aac", "-b:a", "192k",
            ],
            "matroska",
        );
        for codec in ["aac", "mp3", "opus", "vorbis", "flac", "ac3", "eac3"] {
            let source = probe(
                Some(video("mpeg4", "yuv420p", 0)),
                Some(audio(codec, 48_000, 2, 16)),
            );
            assert!(copy_plan(Format::Mkv, &source).audio, "{codec}");
        }
    }

    #[test]
    fn the_retry_re_encodes_everything() {
        let source = typical(Format::Mp4);
        for (to, muxer) in [
            (Format::Mp4, "mp4"),
            (Format::Mov, "mov"),
            (Format::Mkv, "matroska"),
        ] {
            assert!(copy_plan(to, &source).any());
            let retry = reencode_passes(&src(), to, &source, &out(), &palette()).unwrap();
            let mut middle = vec![
                "-map", "0:V:0", "-map", "0:a:0?", "-c:v", "libx264", "-crf", "20", "-preset",
                "medium", "-pix_fmt", "yuv420p", "-vf", EVEN, "-c:a", "aac", "-b:a", "192k",
            ];
            if to != Format::Mkv {
                middle.extend_from_slice(&["-movflags", "+faststart", "-write_tmcd", "0"]);
            }
            assert_eq!(retry.len(), 1);
            assert_eq!(retry[0].args, expect(&middle, muxer), "{to:?}");
        }
        // Targets that never copy have nothing to retry.
        for to in [
            Format::Webm,
            Format::Avi,
            Format::Wmv,
            Format::Gif,
            Format::Mp3,
        ] {
            assert!(!copy_plan(to, &source).any(), "{to:?}");
        }
    }

    #[test]
    fn a_missing_stream_is_an_error() {
        let silent = probe(Some(video("h264", "yuv420p", 0)), None);
        assert_eq!(passes_for(&silent, Format::Mp3), Err(Missing::Audio));
        // A silent video still converts to the video targets.
        assert!(passes_for(&silent, Format::Webm).is_ok());

        let audio_only = probe(None, Some(audio("aac", 44_100, 2, 16)));
        assert_eq!(passes_for(&audio_only, Format::Mov), Err(Missing::Video));
        for to in [
            Format::Mp4,
            Format::Mkv,
            Format::Webm,
            Format::Avi,
            Format::Wmv,
            Format::Gif,
        ] {
            assert_eq!(passes_for(&audio_only, to), Err(Missing::Video), "{to:?}");
            assert_eq!(
                reencode_passes(&src(), to, &audio_only, &out(), &palette()),
                Err(Missing::Video)
            );
        }
        assert!(passes_for(&audio_only, Format::Mp3).is_ok());
    }

    #[test]
    fn deep_audio_keeps_24_bits() {
        let deep = probe(None, Some(audio("flac", 96_000, 2, 24)));
        assert_one_pass(
            &deep,
            Format::Wav,
            &["-map", "0:a:0", "-c:a", "pcm_s24le", "-rf64", "auto"],
            "wav",
        );
        assert_one_pass(
            &deep,
            Format::Aiff,
            &["-map", "0:a:0", "-c:a", "pcm_s24be"],
            "aiff",
        );
        let pcm32 = probe(None, Some(audio("pcm_s32le", 48_000, 2, 32)));
        assert_one_pass(
            &pcm32,
            Format::Flac,
            &[
                "-map",
                "0:a:0",
                "-c:a",
                "flac",
                "-compression_level",
                "8",
                "-sample_fmt",
                "s32",
            ],
            "flac",
        );
        // 16 bits or fewer stays 16.
        let cd = probe(None, Some(audio("flac", 44_100, 2, 16)));
        assert_one_pass(
            &cd,
            Format::Aiff,
            &["-map", "0:a:0", "-c:a", "pcm_s16be"],
            "aiff",
        );
    }

    #[test]
    fn hi_res_surround_is_brought_within_wma_limits() {
        let hires = probe(None, Some(audio("flac", 96_000, 6, 24)));
        assert_one_pass(
            &hires,
            Format::Wma,
            &[
                "-map", "0:a:0", "-c:a", "wmav2", "-b:a", "192k", "-ac", "2", "-ar", "48000",
            ],
            "asf",
        );
        let video_hires = probe(
            Some(video("h264", "yuv420p", 0)),
            Some(audio("flac", 96_000, 6, 24)),
        );
        assert_one_pass(
            &video_hires,
            Format::Wmv,
            &[
                "-map", "0:V:0", "-map", "0:a:0?", "-c:v", "wmv2", "-q:v", "3", "-vf", EVEN,
                "-c:a", "wmav2", "-b:a", "192k", "-ac", "2", "-ar", "48000",
            ],
            "asf",
        );
        // Each limit on its own.
        let surround = probe(None, Some(audio("ac3", 48_000, 6, 16)));
        let args = &passes_for(&surround, Format::Wma).unwrap()[0].args;
        assert!(has(args, ["-ac", "2"]) && !args.iter().any(|arg| arg == "-ar"));
        let fast = probe(None, Some(audio("flac", 88_200, 2, 24)));
        let args = &passes_for(&fast, Format::Wma).unwrap()[0].args;
        assert!(has(args, ["-ar", "48000"]) && !args.iter().any(|arg| arg == "-ac"));
        // Other targets take the source as it is.
        for to in [Format::M4a, Format::Flac, Format::Wav, Format::Opus] {
            let args = &passes_for(&hires, to).unwrap()[0].args;
            assert!(
                !args.iter().any(|arg| arg == "-ac" || arg == "-ar"),
                "{to:?}"
            );
        }
    }

    #[test]
    fn opus_and_vorbis_get_a_vorbis_channel_layout() {
        // 5.1(side), the layout AC-3 and 6-channel FLAC decode to, or any
        // other with a layout: remixed to the nearest Vorbis one.
        let surround = probe(None, Some(audio("ac3", 48_000, 6, 16)));
        for (to, codec) in [(Format::Opus, "libopus"), (Format::Ogg, "libvorbis")] {
            let args = &passes_for(&surround, to).unwrap()[0].args;
            assert!(
                has(args, ["-c:a", codec]) && has(args, ["-af", LAYOUTS]),
                "{to:?}"
            );
            // libopus places 5.1 with its surround mapping.
            assert_eq!(has(args, ["-mapping_family", "1"]), to == Format::Opus);
        }
        let movie = probe(
            Some(video("h264", "yuv420p", 0)),
            Some(audio("ac3", 48_000, 6, 16)),
        );
        let args = &passes_for(&movie, Format::Webm).unwrap()[0].args;
        assert!(has(args, ["-c:a", "libopus"]) && has(args, ["-af", LAYOUTS]));
        assert!(has(args, ["-mapping_family", "1"]));
        // The other encoders take any layout, or are held to stereo.
        for to in [
            Format::M4a,
            Format::Flac,
            Format::Wav,
            Format::Wma,
            Format::Mp3,
        ] {
            let args = &passes_for(&surround, to).unwrap()[0].args;
            assert!(!args.iter().any(|arg| arg == "-af"), "{to:?}");
        }
    }

    #[test]
    fn audio_without_a_layout_keeps_its_channels_for_opus_and_vorbis() {
        // Four channels, unmixed, as quad, with no guessed layout, placed
        // by libopus's surround mapping.
        let reading = ["-guess_layout_max", "0", "-i", "<src>"];
        let mut quad = PREFIX[..PREFIX.len() - 2].to_vec();
        quad.extend_from_slice(&reading);
        quad.extend_from_slice(&[
            "-map",
            "0:a:0",
            "-c:a",
            "libopus",
            "-b:a",
            "128k",
            "-mapping_family",
            "1",
            "-af",
            "pan=quad|c0=c0|c1=c1|c2=c2|c3=c3",
            "-progress",
            "pipe:1",
            "-f",
            "opus",
            "<out>",
        ]);
        let four = probe(None, Some(unlabeled(4)));
        let passes = passes_for(&four, Format::Opus).unwrap();
        assert_eq!(passes[0].args, argv(&quad));

        // Six to OGG, around 6.1's LFE: libvorbis would cut 5.1's.
        let mut ogg = PREFIX[..PREFIX.len() - 2].to_vec();
        ogg.extend_from_slice(&reading);
        ogg.extend_from_slice(&[
            "-map",
            "0:a:0",
            "-c:a",
            "libvorbis",
            "-q:a",
            "6",
            "-af",
            "pan=6.1|c0=c0|c1=c1|c2=c2|c4=c3|c5=c4|c6=c5",
            "-progress",
            "pipe:1",
            "-f",
            "ogg",
            "<out>",
        ]);
        let six = probe(None, Some(unlabeled(6)));
        assert_eq!(passes_for(&six, Format::Ogg).unwrap()[0].args, argv(&ogg));

        // Each count from 1 to 8 gets its Vorbis layout, channel for
        // channel, but Opus puts 7 in 7.1 and Vorbis 6 in 6.1, around the
        // LFE; more are downmixed to 7.1 from FFmpeg's guess.
        let layouts = ["mono", "stereo", "3.0", "quad", "5.0", "5.1", "6.1", "7.1"];
        for channels in 1..=12 {
            let pan = |opus: bool| {
                (1..=8).contains(&channels).then(|| {
                    if opus && channels == 7 {
                        return "pan=7.1|c0=c0|c1=c1|c2=c2|c4=c3|c5=c4|c6=c5|c7=c6".to_string();
                    }
                    if !opus && channels == 6 {
                        return "pan=6.1|c0=c0|c1=c1|c2=c2|c4=c3|c5=c4|c6=c5".to_string();
                    }
                    let layout = layouts[channels as usize - 1];
                    let map: String = (0..channels).map(|c| format!("|c{c}=c{c}")).collect();
                    format!("pan={layout}{map}")
                })
            };
            let audio_only = probe(None, Some(unlabeled(channels)));
            let movie = probe(Some(video("h264", "yuv420p", 0)), Some(unlabeled(channels)));
            for (source, to) in [
                (&audio_only, Format::Opus),
                (&audio_only, Format::Ogg),
                (&movie, Format::Webm),
            ] {
                let opus = to != Format::Ogg;
                let pan = pan(opus);
                let filter = pan
                    .clone()
                    .unwrap_or_else(|| "aformat=channel_layouts=7.1".to_string());
                let args = &passes_for(source, to).unwrap()[0].args;
                assert!(
                    has(args, ["-af", &filter]),
                    "{channels} to {to:?}: {args:?}"
                );
                // The surround mapping would code a track as the LFE of
                // 5.1 and 7.1 (6, 8 and more channels).
                let surround = opus && matches!(channels, 3 | 4 | 5 | 7);
                assert_eq!(
                    has(args, ["-mapping_family", "1"]),
                    surround,
                    "{channels} to {to:?}"
                );
                let guess_off = has(args, ["-guess_layout_max", "0"]);
                assert_eq!(guess_off, pan.is_some(), "{channels} to {to:?}");
                if guess_off {
                    let at = |arg: &str| args.iter().position(|a| a == arg).unwrap();
                    assert!(at("-guess_layout_max") < at("-i"), "{args:?}");
                }
            }
            // The other targets take it as it is.
            for to in [Format::Flac, Format::Wav, Format::M4a, Format::Mp3] {
                let args = &passes_for(&audio_only, to).unwrap()[0].args;
                let guess = args.iter().any(|arg| arg == "-guess_layout_max");
                assert!(!guess && !args.iter().any(|arg| arg == "-af"), "{to:?}");
            }
        }
    }

    #[test]
    fn libopus_gets_its_surround_mapping_for_3_to_8_channels() {
        // 5.0 and 6.1 need it to keep their speakers; 2.1 is remixed to
        // stereo, which, like mono, keeps the default mapping.
        let opus = |channels: u32, layout: &str| {
            let source = Audio {
                layout: layout.into(),
                ..audio("flac", 48_000, channels, 16)
            };
            let args = passes_for(&probe(None, Some(source)), Format::Opus).unwrap();
            args[0].args.clone()
        };
        let cases = [
            (1, "mono", false),
            (2, "stereo", false),
            (3, "2.1", false),
            (3, "3.0", true),
            (4, "quad", true),
            (5, "5.0", true),
            (5, "5.0(side)", true),
            (6, "5.1(side)", true),
            (7, "6.1", true),
            (8, "7.1", true),
            (8, "7.1(wide)", true),
            (24, "22.2", true),
            // A layout ffprobe doesn't name counts as one.
            (6, "", true),
        ];
        for (channels, layout, surround) in cases {
            let args = opus(channels, layout);
            assert_eq!(has(&args, ["-mapping_family", "1"]), surround, "{layout}");
            assert!(has(&args, ["-af", LAYOUTS]), "{layout}");
        }
        assert_one_pass(
            &probe(None, Some(audio("ac3", 48_000, 6, 16))),
            Format::Opus,
            &[
                "-map",
                "0:a:0",
                "-c:a",
                "libopus",
                "-b:a",
                "128k",
                "-mapping_family",
                "1",
                "-af",
                LAYOUTS,
            ],
            "opus",
        );
        // WebM's Opus too; Vorbis never.
        let movie = |channels| {
            probe(
                Some(video("h264", "yuv420p", 0)),
                Some(audio("ac3", 48_000, channels, 16)),
            )
        };
        for (channels, surround) in [(2, false), (6, true)] {
            let args = &passes_for(&movie(channels), Format::Webm).unwrap()[0].args;
            assert_eq!(has(args, ["-mapping_family", "1"]), surround);
            let at = |arg: &str| args.iter().position(|a| a == arg);
            if surround {
                assert_eq!(at("-mapping_family"), at("128k").map(|i| i + 1));
            }
            let source = probe(None, Some(audio("ac3", 48_000, channels, 16)));
            let args = &passes_for(&source, Format::Ogg).unwrap()[0].args;
            assert!(!args.iter().any(|arg| arg == "-mapping_family"));
        }
    }

    #[test]
    fn vorbis_is_held_to_its_sample_rates() {
        for (rate, capped) in [
            (44_100, false),
            (192_000, false),
            (200_000, false),
            (200_001, true),
            (352_800, true),
            (384_000, true),
        ] {
            let source = probe(None, Some(audio("pcm_s24le", rate, 2, 24)));
            let args = &passes_for(&source, Format::Ogg).unwrap()[0].args;
            assert_eq!(has(args, ["-ar", "192000"]), capped, "{rate}");
            assert_eq!(args.iter().any(|arg| arg == "-ar"), capped, "{rate}");
            // Opus resamples on its own (to 48 kHz).
            let args = &passes_for(&source, Format::Opus).unwrap()[0].args;
            assert!(!args.iter().any(|arg| arg == "-ar"), "{rate}");
        }
    }

    #[test]
    fn aiff_bytes_count_every_sample() {
        let long = |secs: u64, rate: u32, depth: u32| Probe {
            duration: Some(Duration::from_secs(secs)),
            ..probe(None, Some(audio("flac", rate, 2, depth)))
        };
        // 24-bit stereo 48 kHz fills 4 GiB in 4 h 8 min 33 s; 16-bit CD
        // audio in 6 h 45 min 49 s.
        let max = AIFF_MAX_LEN;
        assert!(aiff_bytes(&long(4 * 3600 + 8 * 60, 48_000, 24)).unwrap() < max);
        assert!(aiff_bytes(&long(4 * 3600 + 9 * 60, 48_000, 24)).unwrap() > max);
        assert!(aiff_bytes(&long(6 * 3600 + 45 * 60, 44_100, 16)).unwrap() < max);
        assert!(aiff_bytes(&long(6 * 3600 + 46 * 60, 44_100, 16)).unwrap() > max);
        assert_eq!(aiff_bytes(&long(1, 48_000, 24)), Some(288_000));
        // A lossy source is written as 16 bits.
        assert_eq!(aiff_bytes(&long(1, 44_100, 16)), Some(176_400));
        let unknown = Probe {
            duration: None,
            ..long(1, 48_000, 24)
        };
        assert_eq!(aiff_bytes(&unknown), None);
        assert_eq!(
            aiff_bytes(&probe(Some(video("h264", "yuv420p", 0)), None)),
            None
        );
        assert_eq!(AIFF_MAX_LEN, 4 * 1024 * 1024 * 1024 + 7);
    }

    #[test]
    fn odd_dimensions_get_the_even_size_filter() {
        let odd = probe(
            Some(Video {
                codec: "ffv1".into(),
                width: 161,
                height: 121,
                pix_fmt: "bgr0".into(),
                rotation: 0,
            }),
            Some(audio("pcm_s16le", 44_100, 1, 16)),
        );
        for to in [
            Format::Mp4,
            Format::Mov,
            Format::Mkv,
            Format::Webm,
            Format::Wmv,
        ] {
            let args = &passes_for(&odd, to).unwrap()[0].args;
            assert!(has(args, ["-vf", EVEN]), "{to:?}");
        }
        // AVI's filter also caps the size; it gives the same even size
        // below the cap (`the_size_filters_give_even_sizes_of_2_px_or_more`).
        let args = &passes_for(&odd, Format::Avi).unwrap()[0].args;
        assert!(has(args, ["-vf", EVEN_MPEG4]));
    }

    /// The width and height a `scale='W':'H'` filter of Grape's gives an
    /// `iw`×`ih` frame, evaluated like FFmpeg's `scale`: the expressions
    /// in doubles, then truncated to whole pixels.
    pub(in crate::media) fn scaled(filter: &str, iw: u32, ih: u32) -> (u32, u32) {
        let sides = filter
            .strip_prefix("scale='")
            .unwrap()
            .strip_suffix('\'')
            .unwrap();
        let (w, h) = sides.split_once("':'").unwrap();
        let side = |expr: &str| {
            let expr = expr.replace("\\,", ",");
            let mut parser = Expr {
                text: expr.as_bytes(),
                at: 0,
                iw: f64::from(iw),
                ih: f64::from(ih),
            };
            let value = parser.sum();
            assert_eq!(parser.at, expr.len(), "{expr}");
            value as u32
        };
        (side(w), side(h))
    }

    /// A parser and evaluator for the part of FFmpeg's expression language
    /// the size filters use: numbers, `iw`, `ih`, `+ - * /`, parentheses
    /// and `max`, `min` and `trunc`.
    struct Expr<'a> {
        text: &'a [u8],
        at: usize,
        iw: f64,
        ih: f64,
    }

    impl Expr<'_> {
        fn peek(&self) -> Option<u8> {
            self.text.get(self.at).copied()
        }

        fn eat(&mut self, byte: u8) -> bool {
            let found = self.peek() == Some(byte);
            self.at += usize::from(found);
            found
        }

        fn sum(&mut self) -> f64 {
            let mut value = self.product();
            loop {
                if self.eat(b'+') {
                    value += self.product();
                } else if self.eat(b'-') {
                    value -= self.product();
                } else {
                    return value;
                }
            }
        }

        fn product(&mut self) -> f64 {
            let mut value = self.factor();
            loop {
                if self.eat(b'*') {
                    value *= self.factor();
                } else if self.eat(b'/') {
                    value /= self.factor();
                } else {
                    return value;
                }
            }
        }

        fn factor(&mut self) -> f64 {
            if self.eat(b'(') {
                let value = self.sum();
                assert!(self.eat(b')'));
                return value;
            }
            let start = self.at;
            while self
                .peek()
                .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'.')
            {
                self.at += 1;
            }
            let word = std::str::from_utf8(&self.text[start..self.at]).unwrap();
            if !self.eat(b'(') {
                return match word {
                    "iw" => self.iw,
                    "ih" => self.ih,
                    number => number.parse().unwrap(),
                };
            }
            let mut args = vec![self.sum()];
            while self.eat(b',') {
                args.push(self.sum());
            }
            assert!(self.eat(b')'));
            match (word, args.as_slice()) {
                ("max", [a, b]) => a.max(*b),
                ("min", [a, b]) => a.min(*b),
                ("trunc", [a]) => a.trunc(),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn the_size_filters_give_even_sizes_of_2_px_or_more() {
        for filter in [EVEN_SIZE, EVEN_SIZE_MPEG4] {
            // Odd sides round down; a 1 px side becomes 2.
            assert_eq!(scaled(filter, 161, 121), (160, 120), "{filter}");
            assert_eq!(scaled(filter, 1, 1), (2, 2), "{filter}");
            assert_eq!(scaled(filter, 1, 101), (2, 100), "{filter}");
            assert_eq!(scaled(filter, 3, 2), (2, 2), "{filter}");
            assert_eq!(scaled(filter, 1920, 1080), (1920, 1080), "{filter}");
        }
        // AVI's fits 8190 px a side, keeping the aspect, and is the same
        // as the other below that.
        assert_eq!(scaled(EVEN_SIZE_MPEG4, 9001, 101), (8190, 90));
        assert_eq!(scaled(EVEN_SIZE_MPEG4, 101, 9001), (90, 8190));
        assert_eq!(scaled(EVEN_SIZE_MPEG4, 8192, 8192), (8190, 8190));
        assert_eq!(scaled(EVEN_SIZE_MPEG4, 16384, 2), (8190, 2));
        for (w, h) in [(8190, 8190), (8191, 4000), (2, 8191), (7680, 4320)] {
            let capped = scaled(EVEN_SIZE_MPEG4, w, h);
            assert!(capped.0 <= 8190 && capped.1 <= 8190, "{w}x{h}: {capped:?}");
        }
        assert_eq!(scaled(EVEN_SIZE_MPEG4, 8191, 4000), (8190, 3998));
        assert_eq!(scaled(EVEN_SIZE, 9001, 101), (9000, 100));
    }

    #[test]
    fn file_url_prefixes_the_path_unchanged() {
        let path = src();
        let url = file_url(&path);
        assert_eq!(url, format!("file:{}", path.to_str().unwrap()).as_str());
        let text = url.to_str().unwrap();
        for part in ["100% -x", "-clip ü%d"] {
            assert!(text.contains(part), "{text} lacks {part}");
        }
        if cfg!(windows) {
            assert_eq!(file_url(Path::new(r"C:\x\y.mp4")), r"file:C:\x\y.mp4");
        } else {
            assert!(text.contains("-x: y"));
            // Names FFmpeg would otherwise read as a protocol.
            assert_eq!(file_url(Path::new("/tmp/pipe:1")), "file:/tmp/pipe:1");
            assert_eq!(file_url(Path::new("/concat:a|b")), "file:/concat:a|b");
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_url_keeps_non_utf8_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"/tmp/\xff\xfe clip.mp4"));
        assert_eq!(
            file_url(path).as_bytes(),
            b"file:/tmp/\xff\xfe clip.mp4".as_slice()
        );
    }

    /// `file_url` itself is lossless; FFmpeg's own argv conversion isn't,
    /// which is why `convert` refuses these names on Windows.
    #[cfg(windows)]
    #[test]
    fn file_url_keeps_unpaired_surrogates() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let mut wide: Vec<u16> = r"C:\x\".encode_utf16().collect();
        wide.push(0xD800);
        wide.extend(".mp4".encode_utf16());
        let path = PathBuf::from(OsString::from_wide(&wide));
        let mut expected: Vec<u16> = "file:".encode_utf16().collect();
        expected.extend_from_slice(&wide);
        assert_eq!(
            file_url(&path).encode_wide().collect::<Vec<u16>>(),
            expected
        );
    }

    #[test]
    fn only_media_formats_have_a_muxer() {
        for format in Format::ALL {
            assert_eq!(
                muxer(format).is_some(),
                MEDIA.contains(&format),
                "{format:?}"
            );
        }
        let any = typical(Format::Mp4);
        assert_eq!(passes_for(&any, Format::Png), Ok(Vec::new()));
    }

    /// Runs the passes with a real FFmpeg on one-second sources made with
    /// lavfi, in a temp folder whose name has a '%', spaces, a leading '-'
    /// and non-ASCII letters (and a ':' outside Windows), and probes every
    /// output with [`probe_args`]. Ignored: set `GRAPE_TEST_FFMPEG` and
    /// `GRAPE_TEST_FFPROBE` to the tools' absolute paths (nothing is looked
    /// up on `PATH`) and run with `--ignored`. The sources need FFmpeg's
    /// usual encoders, libx265 included.
    mod live {
        use std::env;
        use std::fs;
        use std::process::{Command, Output, Stdio};

        use super::*;
        use crate::media::probe::{parse, probe_args};
        use crate::output::unique_suffix;

        const VIDEO: &str = "-f lavfi -i testsrc=size=160x120:rate=10:duration=1";
        const SINE: &str = "-f lavfi -i sine=duration=1";
        const HIRES: &str = "-f lavfi -i sine=duration=1:sample_rate=96000";
        const SURROUND: &str = "-af pan=5.1|c0=c0|c1=c0|c2=c0|c3=c0|c4=c0|c5=c0";

        /// Each source: file name, then its ffmpeg options, split on
        /// whitespace.
        const SOURCES: [(&str, &[&str]); 22] = [
            (
                "src.mp4",
                &[
                    VIDEO,
                    SINE,
                    "-shortest -c:v libx264 -pix_fmt yuv420p -c:a aac",
                ],
            ),
            (
                "src.mov",
                &[
                    VIDEO,
                    SINE,
                    "-shortest -c:v libx264 -pix_fmt yuv420p -c:a pcm_s16le",
                ],
            ),
            (
                "src.mkv",
                &[
                    VIDEO,
                    SINE,
                    "-shortest -c:v libx265 -x265-params log-level=error",
                    "-pix_fmt yuv420p -c:a aac",
                ],
            ),
            (
                "src.webm",
                &[
                    VIDEO,
                    SINE,
                    "-shortest -c:v libvpx-vp9 -pix_fmt yuv420p -c:a libopus",
                ],
            ),
            (
                "src.avi",
                &[VIDEO, SINE, "-shortest -c:v mpeg4 -c:a libmp3lame"],
            ),
            ("src.wmv", &[VIDEO, SINE, "-shortest -c:v wmv2 -c:a wmav2"]),
            ("src.gif", &[VIDEO]),
            ("src.mp3", &[SINE, "-c:a libmp3lame"]),
            ("src.m4a", &[SINE, "-c:a aac"]),
            ("src.wav", &[SINE, "-c:a pcm_s16le"]),
            // 24 bits.
            ("src.flac", &[SINE, "-c:a flac -sample_fmt s32"]),
            ("src.ogg", &[SINE, "-c:a libvorbis"]),
            ("src.opus", &[SINE, "-c:a libopus"]),
            ("src.aiff", &[SINE, "-c:a pcm_s16be"]),
            ("src.wma", &[SINE, "-c:a wmav2"]),
            (
                "odd.mkv",
                &[
                    "-f lavfi -i testsrc=size=161x121:rate=10:duration=1",
                    SINE,
                    "-shortest -c:v ffv1 -c:a pcm_s16le",
                ],
            ),
            (
                "one.mkv",
                &["-f lavfi -i testsrc=size=16x16:rate=10:duration=1 -vf scale=1:1 -c:v ffv1"],
            ),
            (
                "hires.flac",
                &[HIRES, SURROUND, "-c:a flac -sample_fmt s32"],
            ),
            (
                "hires.mkv",
                &[
                    VIDEO,
                    HIRES,
                    SURROUND,
                    "-shortest -c:v libx264 -pix_fmt yuv420p",
                    "-c:a flac -sample_fmt s32",
                ],
            ),
            (
                "h264.mkv",
                &[
                    VIDEO,
                    SINE,
                    "-shortest -c:v libx264 -pix_fmt yuv420p -c:a aac",
                ],
            ),
            ("silent.mp4", &[VIDEO, "-c:v libx264 -pix_fmt yuv420p"]),
            ("voice.mp4", &[SINE, "-c:a aac"]),
        ];

        struct Tools {
            ffmpeg: PathBuf,
            ffprobe: PathBuf,
            /// The temp folder, removed on drop.
            base: PathBuf,
            /// The oddly named folder inside it that holds every file.
            dir: PathBuf,
        }

        impl Drop for Tools {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.base);
            }
        }

        impl Tools {
            fn new() -> Tools {
                let tool = |var| PathBuf::from(env::var_os(var).expect(var));
                let base = env::temp_dir().join(format!("grape-media-live-{}", unique_suffix()));
                let name = if cfg!(windows) {
                    "100% -x ü y"
                } else {
                    "100% -x: ü y"
                };
                let dir = base.join(name);
                fs::create_dir_all(&dir).unwrap();
                Tools {
                    ffmpeg: tool("GRAPE_TEST_FFMPEG"),
                    ffprobe: tool("GRAPE_TEST_FFPROBE"),
                    base,
                    dir,
                }
            }

            fn run(&self, tool: &Path, args: &[OsString]) -> Output {
                let output = Command::new(tool)
                    .args(args)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{} {args:?}\n{}",
                    tool.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
                output
            }

            /// Makes the source `name` with `options` (whitespace-separated).
            fn make(&self, name: &str, options: &[&str]) {
                let mut args: Vec<OsString> = ["-nostdin", "-hide_banner", "-v", "error", "-y"]
                    .into_iter()
                    .map(OsString::from)
                    .collect();
                for option in options {
                    args.extend(option.split_whitespace().map(OsString::from));
                }
                args.push(file_url(&self.dir.join(name)));
                self.run(&self.ffmpeg, &args);
            }

            fn probe(&self, path: &Path) -> Probe {
                let output = self.run(&self.ffprobe, &probe_args(path));
                parse(&String::from_utf8(output.stdout).unwrap()).unwrap()
            }

            /// The kinds of the streams in `path`, cover art included.
            fn stream_kinds(&self, path: &Path) -> Vec<String> {
                let args: Vec<OsString> = [
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=codec_type",
                    "-of",
                    "csv=p=0",
                ]
                .into_iter()
                .map(OsString::from)
                .chain([file_url(path)])
                .collect();
                let output = self.run(&self.ffprobe, &args);
                String::from_utf8(output.stdout)
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect()
            }

            /// Converts `name` to `to` (re-encoding everything if `reencode`)
            /// and checks the output against the plan.
            fn convert(&self, name: &str, to: Format, reencode: bool) -> Result<Probe, Missing> {
                let src = self.dir.join(name);
                let source = self.probe(&src);
                let out = self
                    .dir
                    .join(format!(".grape-tmp-{}.{}", unique_suffix(), to.key()));
                let palette = self
                    .dir
                    .join(format!(".grape-tmp-{}-%d.png", unique_suffix()));
                let copy = if reencode {
                    CopyPlan::default()
                } else {
                    copy_plan(to, &source)
                };
                let passes = if reencode {
                    reencode_passes(&src, to, &source, &out, &palette)?
                } else {
                    passes(&src, to, &source, &out, &palette)?
                };
                for pass in &passes {
                    let output = self.run(&self.ffmpeg, &pass.args);
                    // `-v error`: anything on stderr is an error, even on success.
                    let errors = String::from_utf8_lossy(&output.stderr);
                    assert!(errors.is_empty(), "{name} -> {to:?}: {errors}");
                    let progress = String::from_utf8(output.stdout).unwrap();
                    assert!(progress.contains("progress=end"), "{name} -> {to:?}");
                }
                let result = self.probe(&out);
                let label = format!("{name} -> {to:?} (re-encode: {reencode})");
                assert_eq!(result.format_name, demuxer(to), "{label}");
                let (video, audio) = expected_codecs(to, &source, copy);
                assert_eq!(
                    result.video.as_ref().map(|v| v.codec.as_str()),
                    video,
                    "{label}"
                );
                assert_eq!(
                    result.audio.as_ref().map(|a| a.codec.as_str()),
                    audio,
                    "{label}"
                );
                if let Some(video) = &result.video {
                    assert_eq!(video.rotation, 0, "{label}");
                }
                let streams = usize::from(video.is_some()) + usize::from(audio.is_some());
                assert_eq!(self.stream_kinds(&out).len(), streams, "{label}");
                Ok(result)
            }

            fn size(&self, name: &str, to: Format) -> (u32, u32) {
                let video = self.convert(name, to, false).unwrap().video.unwrap();
                (video.width, video.height)
            }
        }

        /// The demuxer `ffprobe` reads a file of format `to` with.
        fn demuxer(to: Format) -> &'static str {
            match to {
                Format::Mp4 | Format::Mov | Format::M4a => "mov,mp4,m4a,3gp,3g2,mj2",
                Format::Mkv | Format::Webm => "matroska,webm",
                Format::Wmv | Format::Wma => "asf",
                Format::Ogg | Format::Opus => "ogg",
                other => muxer(other).unwrap(),
            }
        }

        /// The video and audio codecs a conversion of `source` to `to`
        /// writes.
        fn expected_codecs(
            to: Format,
            source: &Probe,
            copy: CopyPlan,
        ) -> (Option<&str>, Option<&str>) {
            let source_video = source.video.as_ref().map(|v| v.codec.as_str());
            let source_audio = source.audio.as_ref();
            let deep = source_audio.is_some_and(|a| a.depth > 16);
            let (video, audio) = match to {
                Format::Mp4 | Format::Mov | Format::Mkv => ("h264", "aac"),
                Format::Webm => ("vp9", "opus"),
                Format::Avi => ("mpeg4", "mp3"),
                Format::Wmv => ("wmv2", "wmav2"),
                Format::Gif => return (Some("gif"), None),
                Format::Mp3 => return (None, Some("mp3")),
                Format::M4a => return (None, Some("aac")),
                Format::Wav if deep => return (None, Some("pcm_s24le")),
                Format::Wav => return (None, Some("pcm_s16le")),
                Format::Flac => return (None, Some("flac")),
                Format::Ogg => return (None, Some("vorbis")),
                Format::Opus => return (None, Some("opus")),
                Format::Aiff if deep => return (None, Some("pcm_s24be")),
                Format::Aiff => return (None, Some("pcm_s16be")),
                Format::Wma => return (None, Some("wmav2")),
                other => panic!("{other:?}"),
            };
            let video = if copy.video {
                source_video
            } else {
                Some(video)
            };
            let audio = source_audio.map(|a| if copy.audio { a.codec.as_str() } else { audio });
            (video, audio)
        }

        #[test]
        #[ignore = "runs FFmpeg: set GRAPE_TEST_FFMPEG and GRAPE_TEST_FFPROBE"]
        fn real_ffmpeg_runs_every_pass() {
            let tools = Tools::new();
            for (name, options) in SOURCES {
                tools.make(name, options);
            }
            // A remux with a display matrix: shown upright, the clip is 120x160.
            let rotate: Vec<OsString> = [
                "-nostdin",
                "-hide_banner",
                "-v",
                "error",
                "-display_rotation:v:0",
                "-90",
                "-i",
            ]
            .into_iter()
            .map(OsString::from)
            .chain([file_url(&tools.dir.join("src.mp4"))])
            .chain(["-c", "copy"].map(OsString::from))
            .chain([file_url(&tools.dir.join("rotated.mov"))])
            .collect();
            tools.run(&tools.ffmpeg, &rotate);
            // An MP3 with cover art.
            tools.make(
                "cover.mp3",
                &[
                    SINE,
                    "-f lavfi -i color=c=blue:size=64x64:duration=1",
                    "-map 0:a -map 1:v -frames:v 1 -c:a libmp3lame -c:v png",
                    "-disposition:v:0 attached_pic -id3v2_version 3",
                ],
            );

            // Every pair, and the re-encode retry wherever a pass copies.
            for from in MEDIA {
                let name = format!("src.{}", from.key());
                let source = tools.probe(&tools.dir.join(&name));
                for &to in grape_core::targets(from) {
                    tools.convert(&name, to, false).unwrap();
                    if copy_plan(to, &source).any() {
                        tools.convert(&name, to, true).unwrap();
                    }
                }
            }

            // Copies: H.264 and AAC into MP4, MOV and MKV (and the retry),
            // PCM re-encoded alone.
            for to in [Format::Mp4, Format::Mov] {
                let source = tools.probe(&tools.dir.join("h264.mkv"));
                assert!(copy_plan(to, &source).video, "{to:?}");
                tools.convert("h264.mkv", to, false).unwrap();
                tools.convert("h264.mkv", to, true).unwrap();
            }
            let mov = tools.convert("src.mp4", Format::Mov, false).unwrap();
            assert_eq!(mov.video.unwrap().codec, "h264");
            let mkv = tools.convert("src.mov", Format::Mkv, false).unwrap();
            assert_eq!(mkv.audio.unwrap().codec, "aac");
            let webm = tools.convert("src.webm", Format::Mkv, false).unwrap();
            assert_eq!(webm.video.unwrap().codec, "vp9");

            // Rotated: re-encoded upright, even for MKV.
            for to in [Format::Mp4, Format::Mkv, Format::Webm, Format::Gif] {
                assert_eq!(tools.size("rotated.mov", to), (120, 160), "{to:?}");
            }
            // Odd and tiny sizes become even, at least 2x2; GIF keeps them.
            for to in [
                Format::Mp4,
                Format::Mov,
                Format::Webm,
                Format::Avi,
                Format::Wmv,
            ] {
                assert_eq!(tools.size("odd.mkv", to), (160, 120), "{to:?}");
                assert_eq!(tools.size("one.mkv", to), (2, 2), "{to:?}");
            }
            assert_eq!(tools.size("odd.mkv", Format::Gif), (161, 121));
            assert_eq!(tools.size("one.mkv", Format::Gif), (1, 1));

            // 96 kHz 5.1 into every target; WMA gets stereo at 48 kHz.
            for to in grape_core::targets(Format::Flac) {
                let result = tools.convert("hires.flac", *to, false).unwrap();
                let audio = result.audio.unwrap();
                if *to == Format::Wma {
                    assert_eq!((audio.channels, audio.sample_rate), (2, 48_000));
                }
            }
            for to in grape_core::targets(Format::Mkv) {
                let result = tools.convert("hires.mkv", *to, false).unwrap();
                if *to == Format::Wmv {
                    let audio = result.audio.unwrap();
                    assert_eq!((audio.channels, audio.sample_rate), (2, 48_000));
                }
            }
            let wav = tools.convert("hires.flac", Format::Wav, false).unwrap();
            let audio = wav.audio.unwrap();
            assert_eq!(
                (audio.codec.as_str(), audio.channels, audio.sample_rate),
                ("pcm_s24le", 6, 96_000)
            );

            // Missing streams; a silent video still converts to video.
            assert_eq!(
                tools.convert("silent.mp4", Format::Mp3, false).err(),
                Some(Missing::Audio)
            );
            for to in [
                Format::Mov,
                Format::Mkv,
                Format::Webm,
                Format::Avi,
                Format::Wmv,
                Format::Gif,
            ] {
                assert_eq!(
                    tools.convert("silent.mp4", to, false).unwrap().audio,
                    None,
                    "{to:?}"
                );
            }
            assert_eq!(
                tools.convert("voice.mp4", Format::Mov, false).err(),
                Some(Missing::Video)
            );
            assert!(tools.convert("voice.mp4", Format::Mp3, false).is_ok());

            // Cover art is dropped (`convert` counts the streams).
            assert_eq!(
                tools.stream_kinds(&tools.dir.join("cover.mp3")),
                ["audio", "video"]
            );
            for to in grape_core::targets(Format::Mp3) {
                tools.convert("cover.mp3", *to, false).unwrap();
            }
        }
    }
}
