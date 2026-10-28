//! The tools that run through FFmpeg (D26), one file in and one out:
//! Remove Audio, Remove Metadata, Audio Channels, Normalize Volume and
//! Compress.
//!
//! [`run`] goes through the steps a conversion does ([`super::convert`]),
//! and every refusal, and every [`ErrorKind::NoChange`], comes before
//! anything is created:
//!
//! 1. [`super::begin`]: the sidecars, the source still a file, its probe.
//! 2. The tags probe ([`super::tags`]): what the source has, by key, for
//!    the tool to keep or remove.
//! 3. The tool's own checks (no sound to remove, no video to keep,
//!    already mono, a 32-bit source a codec can't keep).
//! 4. The temp file, then one of the two FFmpeg slots. Normalize Volume
//!    takes the slot first: its measuring passes write nothing (the `null`
//!    muxer), and what they find can still end it with `NoChange`, so the
//!    temp file comes after them.
//! 5. The tool's passes ([`argv`]), then the output's check: the
//!    conversion's (not empty, `ffprobe` reads it), then the tool's own:
//!    the streams it planned and, for Remove Metadata, a second tags probe
//!    that must find nothing left.
//! 6. The publish ([`super::Begun::publish_checked`]), and the note.
//!
//! Tools keep every track but what they remove: streams are mapped by
//! index. Only Compress on a video retries, once, when the muxer refuses
//! an audio track it copied: a copy tool that re-encoded would lose the
//! quality it promises to keep.

mod argv;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use grape_core::{Channels, Format, Job, ToolKey, ToolParams};

use super::args::{Missing, Pass};
use super::loudness::{self, Correction, Plan};
use super::probe::{Probe, StreamInfo, StreamKind};
use super::run::{self, RunError, SlotGuard};
use super::tags::{self, Tags};
use super::{
    absolute, begin, ffmpeg_failed, no_stream, probe_failed, probe_output, run_passes,
    source_is_a_file, worth_a_retry, Bar, Begun, PASSES_SHARE, PROBE_TIMEOUT,
};
use crate::output::TempFile;
use crate::publish;
use crate::{EngineCtx, ErrorKind, GrapeError, JobOutcome, Note};

/// True when this engine runs `tool` on a file of format `from` (with
/// `ffmpeg` and `ffprobe`; the caller checks those).
pub(crate) fn runs_tool(tool: ToolKey, from: Format) -> bool {
    use Format::{Flac, M4a, Mkv, Mov, Mp3, Mp4, Wav};
    match tool {
        ToolKey::RemoveMetadata => matches!(from, Mp3 | M4a | Wav | Flac | Mp4 | Mov | Mkv),
        ToolKey::Compress => matches!(from, Mp3 | M4a | Wav | Flac | Mp4 | Mov | Mkv),
        ToolKey::AudioChannels | ToolKey::NormalizeAudio => matches!(from, Mp3 | M4a | Wav | Flac),
        ToolKey::MuteVideo => matches!(from, Mp4 | Mov | Mkv),
        _ => false,
    }
}

/// MP4, MOV and M4A: one demuxer, whose XMP the tags probe asks for.
fn mov_family(format: Format) -> bool {
    matches!(format, Format::Mp4 | Format::Mov | Format::M4a)
}

/// Runs `tool` on the file `src` (of format `from`, by its name) into
/// `dest`, the path the plan reserved, whose name says the output's format
/// `to`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    to: Format,
    tool: ToolKey,
    params: &ToolParams,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let mut bar = Bar::new(progress);
    let begun = begin(job, src, dest, from, to, ctx)?;
    let tags = read_tags(&begun, from, ctx)?;
    let tool = Tool {
        tool,
        params,
        begun: &begun,
        to,
        tags: &tags,
        ctx,
    };
    match tool.tool {
        ToolKey::MuteVideo => tool.mute(&mut bar),
        ToolKey::RemoveMetadata => tool.strip(&mut bar),
        ToolKey::AudioChannels => tool.channels(&mut bar),
        ToolKey::NormalizeAudio => tool.normalize(&mut bar),
        ToolKey::Compress => match to {
            Format::Mp4 | Format::Mov | Format::Mkv => tool.compress_video(&mut bar),
            _ => tool.compress_audio(&mut bar),
        },
        _ => Err(crate::not_yet_supported(job, Some(from))),
    }
}

/// The tags probe of the source: with `-export_xmp 1` for the MOV family,
/// whose demuxer otherwise drops an XMP packet unseen.
fn read_tags(begun: &Begun, from: Format, ctx: &EngineCtx) -> Result<Tags, GrapeError> {
    tags::read(
        begun.ffprobe,
        &begun.src_abs,
        mov_family(from),
        &ctx.cancel,
        PROBE_TIMEOUT,
    )
    .map_err(|e| probe_failed(&begun.names, e, begun.src))
    .map_err(|e| publish::or_cancelled(&ctx.cancel, begun.src, e))
}

/// One tool job, once [`run`] has begun it and read the tags.
struct Tool<'a> {
    tool: ToolKey,
    /// The panel's choices.
    params: &'a ToolParams,
    begun: &'a Begun<'a>,
    /// The output's format.
    to: Format,
    tags: &'a Tags,
    ctx: &'a EngineCtx,
}

/// An output FFmpeg wrote and `ffprobe` read.
struct Written {
    temp: TempFile,
    /// `temp`'s absolute path.
    out: PathBuf,
    /// What `ffprobe` said about it.
    probe: Probe,
    /// The FFmpeg slot, held until the job ends.
    _slot: SlotGuard<'static>,
}

impl Tool<'_> {
    /// Remove Audio: every track but the audio, copied
    /// ([`argv::mute`]). A video without sound has nothing to remove; one
    /// without a picture (an audio-only MP4, or only cover art) has
    /// nothing to keep.
    fn mute(&self, bar: &mut Bar) -> Result<JobOutcome, GrapeError> {
        let (names, info) = (&self.begun.names, &self.begun.info);
        let src = self.begun.src;
        if count(info, StreamKind::Audio) == 0 {
            let detail = format!("no audio stream in {}", src.display());
            return Err(names.no_change("has no sound to remove.", detail));
        }
        if info.video.is_none() {
            return Err(GrapeError::new(
                ErrorKind::DecodeFailed,
                format!("{} has no video to keep.", names.source),
                format!("no video stream but cover art in {}", src.display()),
            ));
        }
        let kept: Vec<&StreamInfo> = argv::muted(self.to, info, self.tags).collect();
        let subtitles = kept
            .iter()
            .filter(|s| s.kind == StreamKind::Subtitle)
            .count();
        let cover_art = covers(kept.iter().copied());
        let written = self.write(bar, |out| {
            argv::mute(&self.begun.src_abs, self.to, info, self.tags, out)
        })?;
        let out = &written.probe;
        let (video, audio) = (count(out, StreamKind::Video), count(out, StreamKind::Audio));
        let (wrote_subtitles, wrote_cover_art) =
            (count(out, StreamKind::Subtitle), covers(out.streams.iter()));
        if video == 0 || audio > 0 || wrote_subtitles != subtitles || wrote_cover_art != cover_art {
            return Err(self.wrote_wrong(
                &written.out,
                format!(
                    "{video} video, {audio} audio, {wrote_subtitles} subtitle and \
                     {wrote_cover_art} cover art streams, {subtitles} subtitles planned and \
                     {cover_art} cover art"
                ),
            ));
        }
        self.publish(written, bar)
    }

    /// Remove Metadata: every track it keeps, copied without its tags
    /// ([`argv::strip`]), then the output's tags probe, which must find
    /// only what may stay. The note says what the source's tags probe
    /// found, even when that's nothing.
    fn strip(&self, bar: &mut Bar) -> Result<JobOutcome, GrapeError> {
        let (names, info) = (&self.begun.names, &self.begun.info);
        let src = self.begun.src;
        let kept: Vec<&StreamInfo> = argv::stripped(self.to, info, self.tags).collect();
        if kept.is_empty() {
            return Err(match self.to {
                Format::Mp4 | Format::Mov | Format::Mkv => nothing_in_it(names, src),
                _ => no_stream(names, Missing::Audio, src),
            });
        }
        let written = self.write(bar, |out| {
            argv::strip(&self.begun.src_abs, self.to, info, self.tags, out)
        })?;
        let planned = kinds(kept.iter().copied());
        let wrote = kinds(written.probe.streams.iter());
        if wrote != planned {
            return Err(self.wrote_wrong(
                &written.out,
                format!("streams by kind {wrote:?}, {planned:?} planned"),
            ));
        }
        let left = tags::read(
            self.begun.ffprobe,
            &written.out,
            mov_family(self.to),
            &self.ctx.cancel,
            PROBE_TIMEOUT,
        )
        .map_err(|e| self.unchecked(&written.out, e))?;
        if !left.fields.is_empty() || left.unlisted {
            return Err(GrapeError::new(
                ErrorKind::EncodeFailed,
                format!(
                    "Grape couldn't remove all of {}'s metadata. Nothing was saved.",
                    names.source
                ),
                format!(
                    "check {}: left {:?}{}",
                    written.out.display(),
                    left.fields,
                    if left.unlisted {
                        " and keys it may not keep"
                    } else {
                        ""
                    }
                ),
            ));
        }
        let mut outcome = self.publish(written, bar)?;
        let removed = self.tags.fields.iter().copied().collect();
        outcome.notes.push(Note::MetadataRemoved(removed));
        Ok(outcome)
    }

    /// Audio Channels: the first audio track remixed to mono or stereo
    /// ([`argv::mix`]), the same codec, depth and rate ([`argv::same_quality`]),
    /// its cover art kept and ReplayGain blanked.
    fn channels(&self, bar: &mut Bar) -> Result<JobOutcome, GrapeError> {
        let (names, info) = (&self.begun.names, &self.begun.info);
        let src = self.begun.src;
        let audio = self.audio()?;
        let target = self.params.channels;
        let (filter, unlabeled) = match argv::mix(target, audio.channels, audio.has_layout) {
            argv::Mix::Already => {
                let why = match target {
                    Channels::Mono => "is already mono.",
                    Channels::Stereo => "is already stereo.",
                };
                let detail = format!("{} channels in {}", audio.channels, src.display());
                return Err(names.no_change(why, detail));
            }
            argv::Mix::NoSides => {
                return Err(GrapeError::new(
                    ErrorKind::NotYetSupported,
                    format!(
                        "{} has {} channels without speaker positions, so Grape can't tell \
                         which are left and right.",
                        names.source, audio.channels
                    ),
                    format!("no channel layout in {}", src.display()),
                ));
            }
            argv::Mix::Filter { filter, unlabeled } => (filter, unlabeled),
        };
        let channels = match target {
            Channels::Mono => 1,
            Channels::Stereo => 2,
        };
        let codec = self.same_quality(audio, channels)?;
        let reading: &[&str] = if unlabeled {
            &["-guess_layout_max", "0"]
        } else {
            &[]
        };
        let written = self.write(bar, |out| {
            let encode = argv::Encode {
                reading,
                filter: Some(&filter),
                codec: &codec,
                blank_replaygain: true,
            };
            argv::reencode(&self.begun.src_abs, self.to, info, self.tags, &encode, out)
        })?;
        let wrote = written
            .probe
            .streams
            .iter()
            .find(|s| s.kind == StreamKind::Audio)
            .map(|s| s.channels);
        if wrote != Some(channels) {
            return Err(self.wrote_wrong(
                &written.out,
                format!("{wrote:?} channels, {channels} asked"),
            ));
        }
        self.publish(written, bar)
    }

    /// Normalize Volume ([`loudness`]): measure, plan, check the limiter's
    /// result at most twice, then write with the gain, keeping the codec,
    /// depth and rate. The measuring and check passes write nothing, so
    /// only the last pass has a temp file.
    fn normalize(&self, bar: &mut Bar) -> Result<JobOutcome, GrapeError> {
        let (names, info) = (&self.begun.names, &self.begun.info);
        let src = self.begun.src;
        let target = self.params.target_lufs;
        if !ToolParams::lufs_ok(target) {
            return Err(GrapeError::new(
                ErrorKind::EncodeFailed,
                "Grape couldn't use that loudness.",
                format!(
                    "target {target} LUFS outside {}..={}",
                    ToolParams::LUFS_MIN,
                    ToolParams::LUFS_MAX
                ),
            ));
        }
        let audio = self.audio()?;
        let codec = self.same_quality(audio, audio.channels)?;
        let (goal, ceiling) = (f64::from(target), loudness::ceiling(codec.exact));
        let mono = audio.channels == 1;

        let slot = self.slot()?;
        let mut done = MEASURE_SHARE;
        let measured = self.measure(&loudness::measure(None, mono), bar, done)?;
        let chain = match loudness::plan(measured, goal, ceiling) {
            Plan::TooQuiet => {
                let detail = format!("measured {measured:?} in {}", src.display());
                let why = "is too short or too quiet to measure its loudness.";
                return Err(self.settled(names.no_change(why, detail)));
            }
            Plan::AtTarget => {
                let detail = format!("measured {measured:?} in {}", src.display());
                let why = format!("is already at {} LUFS.", minus(target));
                return Err(self.settled(names.no_change(&why, detail)));
            }
            Plan::Linear(gain) => loudness::linear(gain),
            Plan::Limited(mut gain) => {
                let rate = audio.sample_rate;
                let mut checks = 0;
                loop {
                    checks += 1;
                    done += CHECK_SHARE;
                    let chain = loudness::limited(gain, ceiling, rate);
                    let filter = loudness::measure(Some(&chain), mono);
                    let lufs = self.measure(&filter, bar, done)?.map(|m| m.lufs);
                    let Some(lufs) = lufs else {
                        return Err(names.encode_failed(format!(
                            "check {}: no loudness summary at gain {gain:.2} dB",
                            src.display()
                        )));
                    };
                    match loudness::correct(goal, gain, lufs, checks) {
                        Correction::Write(last) => break loudness::limited(last, ceiling, rate),
                        Correction::Check(next) => gain = next,
                    }
                }
            }
        };

        let (temp, out) = self.temp()?;
        let encode = argv::Encode {
            reading: &[],
            filter: Some(&chain),
            codec: &codec,
            blank_replaygain: true,
        };
        let pass = Pass {
            args: argv::reencode(&self.begun.src_abs, self.to, info, self.tags, &encode, &out),
            weight: 1.0,
        };
        let warnings = self.run(&[pass], bar, PASSES_SHARE)?;
        let probe = self.probe(&out, &warnings)?;
        let wrote = probe
            .streams
            .iter()
            .find(|s| s.kind == StreamKind::Audio)
            .map(|s| s.sample_rate);
        if wrote != Some(audio.sample_rate) {
            return Err(self.wrote_wrong(
                &out,
                format!("rate {wrote:?}, {} in the source", audio.sample_rate),
            ));
        }
        let written = Written {
            temp,
            out,
            probe,
            _slot: slot,
        };
        self.publish(written, bar)
    }

    /// Compress on audio ([`argv::squeeze`]): the first audio track
    /// encoded smaller, its cover art and tags kept, then the size rule
    /// ([`argv::small_enough`]). A source already no bigger than the preset
    /// makes it, or an output not enough smaller, saves nothing
    /// ([`ErrorKind::NoChange`]).
    fn compress_audio(&self, bar: &mut Bar) -> Result<JobOutcome, GrapeError> {
        let (names, info) = (&self.begun.names, &self.begun.info);
        let src = self.begun.src;
        let audio = self.audio()?;
        let bit_rate = audio.bit_rate.or(info.bit_rate);
        let squeeze = argv::squeeze(self.to, audio, bit_rate, self.params.compress)
            .map_err(|refusal| self.refused(audio, refusal))?;
        if squeeze.skip {
            let detail = format!("{} at {bit_rate:?} b/s in {}", audio.codec, src.display());
            return Err(names.not_smaller(squeeze.try_small, detail));
        }
        let before = self.source_size()?;
        let encode = argv::Encode {
            reading: squeeze.reading,
            filter: squeeze.filter.as_deref(),
            codec: &squeeze.codec,
            blank_replaygain: false,
        };
        let written = self.write(bar, |out| {
            argv::reencode(&self.begun.src_abs, self.to, info, self.tags, &encode, out)
        })?;
        let rates: Vec<u32> = written
            .probe
            .streams
            .iter()
            .filter(|s| s.kind == StreamKind::Audio)
            .map(|s| s.sample_rate)
            .collect();
        if rates != [squeeze.sample_rate] {
            return Err(self.wrote_wrong(
                &written.out,
                format!("audio at {rates:?} Hz, {} planned", squeeze.sample_rate),
            ));
        }
        self.publish_smaller(written, before, squeeze.lossy, squeeze.try_small, bar)
    }

    /// Compress on a video ([`argv::shrink`]): the picture through x264,
    /// smaller and at most 30 fps at Small; each audio track AAC unless it
    /// is small AAC already, when it's copied; everything else kept. A
    /// copied track the muxer refuses gets one retry, re-encoding every
    /// audio track. Then the size rule, at 90 %.
    fn compress_video(&self, bar: &mut Bar) -> Result<JobOutcome, GrapeError> {
        let Begun {
            ffmpeg,
            names,
            info,
            src,
            src_abs,
            ..
        } = self.begun;
        let kept: Vec<&StreamInfo> = argv::shrunk(self.to, info, self.tags).collect();
        if !kept
            .iter()
            .any(|s| !s.attached_pic && matches!(s.kind, StreamKind::Video | StreamKind::Audio))
        {
            return Err(nothing_in_it(names, src));
        }
        if argv::picture(info).is_some_and(argv::is_pq) {
            return Err(GrapeError::new(
                ErrorKind::NotYetSupported,
                format!(
                    "{} is HDR10 video, which Compress can't keep yet.",
                    names.source
                ),
                format!("PQ (smpte2084) video in {}", src.display()),
            ));
        }
        let preset = self.params.compress;
        let before = self.source_size()?;
        let (temp, out) = self.temp()?;
        let slot = self.slot()?;
        let shrink = |copy_audio| {
            let shrink = argv::shrink(src_abs, self.to, info, self.tags, preset, copy_audio, &out);
            let pass = Pass {
                args: shrink.args,
                weight: 1.0,
            };
            (pass, shrink.copies)
        };
        let (first, copies) = shrink(true);
        // A copy the muxer refuses is rare: the encode fills most of the
        // bar, and a retry what's left.
        let share = if copies { RETRY_FROM } else { PASSES_SHARE };
        let warnings = match run_passes(ffmpeg, &[first], info.duration, self.ctx, bar, share) {
            Ok(warnings) => warnings,
            Err(RunError::Cancelled) => return Err(crate::cancelled(src)),
            Err(e) => {
                let refused = matches!(e, RunError::Exit { .. });
                let failure =
                    publish::or_cancelled(&self.ctx.cancel, src, ffmpeg_failed(names, e, src));
                if !(refused && copies && worth_a_retry(&failure)) {
                    return Err(failure);
                }
                // An AAC track can fit by its codec and still not mux.
                let (again, _) = shrink(false);
                self.run(&[again], bar, PASSES_SHARE)?
            }
        };
        let probe = self.probe(&out, &warnings)?;
        let planned = kinds(kept.iter().copied());
        let wrote = kinds(probe.streams.iter());
        if wrote != planned {
            return Err(self.wrote_wrong(
                &out,
                format!("streams by kind {wrote:?}, {planned:?} planned"),
            ));
        }
        let written = Written {
            temp,
            out,
            probe,
            _slot: slot,
        };
        let try_small = argv::small_does_more(self.to, info, self.tags, preset);
        self.publish_smaller(written, before, true, try_small, bar)
    }

    /// The refusal of an audio file Compress can't make FLAC of.
    fn refused(&self, audio: &StreamInfo, refusal: argv::Refusal) -> GrapeError {
        let (names, src) = (&self.begun.names, self.begun.src);
        let message = match refusal {
            argv::Refusal::Deep => format!(
                "{} is a {}-bit recording. FLAC would clip or cut it, so nothing was saved.",
                names.source, audio.depth
            ),
            argv::Refusal::Channels => format!(
                "{} has {} channels, more than the 8 FLAC holds.",
                names.source, audio.channels
            ),
        };
        let detail = format!(
            "{}-bit {} with {} channels in {}",
            audio.depth,
            audio.codec,
            audio.channels,
            src.display()
        );
        GrapeError::new(ErrorKind::NotYetSupported, message, detail)
    }

    /// The source's size in bytes, for Compress's size rule.
    fn source_size(&self) -> Result<u64, GrapeError> {
        std::fs::metadata(&self.begun.src_abs)
            .map(|meta| meta.len())
            .map_err(|e| {
                let detail = format!("size of {}: {e}", self.begun.src.display());
                self.begun.names.read_failed(detail)
            })
    }

    /// Publishes Compress's checked output if it is small enough next to
    /// the source's `before` bytes ([`argv::small_enough`]), with
    /// [`Note::Compressed`]; else nothing is saved, and the message says
    /// whether Small would do more.
    fn publish_smaller(
        &self,
        written: Written,
        before: u64,
        lossy: bool,
        try_small: bool,
        bar: &mut Bar,
    ) -> Result<JobOutcome, GrapeError> {
        let after = std::fs::metadata(&written.out)
            .map(|meta| meta.len())
            .map_err(|e| {
                let detail = format!("size of {}: {e}", written.out.display());
                self.begun.names.write_failed(detail)
            })?;
        if !argv::small_enough(before, after, lossy) {
            let detail = format!(
                "{after} bytes from {before} in {}",
                self.begun.src.display()
            );
            return Err(self.settled(self.begun.names.not_smaller(try_small, detail)));
        }
        let mut outcome = self.publish(written, bar)?;
        outcome.notes.push(Note::Compressed { before, after });
        Ok(outcome)
    }

    /// Runs a measuring or check pass with `filter` ([`argv::measure`]),
    /// filling the bar to `share` of [`PASSES_SHARE`], and reads its
    /// summary. A failure's detail holds no stderr: at `-v info` it
    /// repeats the source's tags.
    fn measure(
        &self,
        filter: &str,
        bar: &mut Bar,
        share: f32,
    ) -> Result<Option<loudness::Measured>, GrapeError> {
        let Begun {
            ffmpeg,
            info,
            names,
            src,
            src_abs,
            ..
        } = self.begun;
        let pass = Pass {
            args: argv::measure(src_abs, info, filter),
            weight: 1.0,
        };
        let end = PASSES_SHARE * share;
        match run_passes(ffmpeg, &[pass], info.duration, self.ctx, bar, end) {
            Ok(stderr) => Ok(loudness::parse(&stderr)),
            Err(e) => {
                let detail = format!("measure {}: {e}", src.display());
                let mut failed = ffmpeg_failed(names, e, src);
                failed.detail = detail;
                Err(publish::or_cancelled(&self.ctx.cancel, src, failed))
            }
        }
    }

    /// The source's first audio track, which the audio tools work on.
    fn audio(&self) -> Result<&StreamInfo, GrapeError> {
        let info = &self.begun.info;
        info.streams
            .iter()
            .find(|s| s.kind == StreamKind::Audio)
            .ok_or_else(|| no_stream(&self.begun.names, Missing::Audio, self.begun.src))
    }

    /// [`argv::same_quality`] for `audio` and `channels` channels out, or
    /// the refusal of a source deeper than its codec writes.
    fn same_quality(&self, audio: &StreamInfo, channels: u32) -> Result<argv::Codec, GrapeError> {
        let bit_rate = audio.bit_rate.or(self.begun.info.bit_rate);
        argv::same_quality(self.to, audio, bit_rate, channels).map_err(|codec| {
            GrapeError::new(
                ErrorKind::NotYetSupported,
                format!(
                    "{} is a 32-bit recording. {codec} would cut it, so nothing was saved.",
                    self.begun.names.source
                ),
                format!(
                    "{}-bit {} in {}",
                    audio.depth,
                    audio.codec,
                    self.begun.src.display()
                ),
            )
        })
    }

    /// Creates the temp output, waits for an FFmpeg slot, runs the one pass
    /// `args` builds for it (filling the bar to [`PASSES_SHARE`]), and checks
    /// the output as a conversion's.
    fn write(
        &self,
        bar: &mut Bar,
        args: impl FnOnce(&Path) -> Vec<OsString>,
    ) -> Result<Written, GrapeError> {
        let (temp, out) = self.temp()?;
        let slot = self.slot()?;
        let pass = Pass {
            args: args(&out),
            weight: 1.0,
        };
        let warnings = self.run(&[pass], bar, PASSES_SHARE)?;
        let probe = self.probe(&out, &warnings)?;
        Ok(Written {
            temp,
            out,
            probe,
            _slot: slot,
        })
    }

    /// The temp file next to the planned name, in Grape's fallback folder
    /// made first if that's where the job goes, and its absolute path.
    fn temp(&self) -> Result<(TempFile, PathBuf), GrapeError> {
        let Begun {
            job, dir, names, ..
        } = self.begun;
        publish::ensure_dir(job, self.ctx, dir, names)?;
        let temp = TempFile::create(dir, self.to.output_ext())
            .map(|(temp, _file)| temp)
            .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
        let out = absolute(temp.path()).map_err(|e| names.write_failed(e))?;
        Ok((temp, out))
    }

    /// One of the two FFmpeg slots; a cancel while waiting ends the job.
    /// The wait can be as long as two other encodes, so the source is
    /// checked again after it.
    fn slot(&self) -> Result<SlotGuard<'static>, GrapeError> {
        let Some(slot) = run::acquire(&self.ctx.cancel) else {
            return Err(crate::cancelled(self.begun.src));
        };
        source_is_a_file(&self.begun.src_abs, self.begun.src, &self.begun.names)?;
        Ok(slot)
    }

    /// Runs `passes`, filling the bar to `end`; returns what the last one
    /// wrote to stderr.
    fn run(&self, passes: &[Pass], bar: &mut Bar, end: f32) -> Result<String, GrapeError> {
        let Begun {
            ffmpeg,
            info,
            names,
            src,
            ..
        } = self.begun;
        run_passes(ffmpeg, passes, info.duration, self.ctx, bar, end)
            .map_err(|e| ffmpeg_failed(names, e, src))
            .map_err(|e| publish::or_cancelled(&self.ctx.cancel, src, e))
    }

    /// The conversion's check of the output `out` but for its streams,
    /// which each tool checks itself, with FFmpeg's `warnings` added to a
    /// failure's detail.
    fn probe(&self, out: &Path, warnings: &str) -> Result<Probe, GrapeError> {
        let Begun {
            ffprobe,
            names,
            src,
            ..
        } = self.begun;
        probe_output(ffprobe, out, self.to, names, self.ctx, src)
            .map_err(|mut e| {
                if !warnings.trim().is_empty() {
                    e.detail = format!("{}; FFmpeg said: {}", e.detail, warnings.trim());
                }
                e
            })
            .map_err(|e| publish::or_cancelled(&self.ctx.cancel, src, e))
    }

    /// The error for an output `out` that isn't what the tool planned, or
    /// the cancel that may have cut it short.
    fn wrote_wrong(&self, out: &Path, detail: String) -> GrapeError {
        let e = self
            .begun
            .names
            .encode_failed(format!("check {}: {detail}", out.display()));
        publish::or_cancelled(&self.ctx.cancel, self.begun.src, e)
    }

    /// A verdict reached after a pass ([`ErrorKind::NoChange`], say), or the
    /// cancel that came meanwhile: a cancelled job always says so.
    fn settled(&self, e: GrapeError) -> GrapeError {
        publish::or_cancelled(&self.ctx.cancel, self.begun.src, e)
    }

    /// The error for an output `out` whose tags probe failed: it can't be
    /// shown to hold nothing.
    fn unchecked(&self, out: &Path, e: RunError) -> GrapeError {
        match e {
            RunError::Cancelled => crate::cancelled(self.begun.src),
            e => self.wrote_wrong(out, format!("tags probe: {e}")),
        }
    }

    /// Publishes a checked output.
    fn publish(&self, written: Written, bar: &mut Bar) -> Result<JobOutcome, GrapeError> {
        self.begun
            .publish_checked(written.temp, &written.out, self.ctx, bar)
    }
}

/// Where Compress on a video's first pass stops when it copies a track,
/// so a retry has the rest of the bar.
const RETRY_FROM: f32 = PASSES_SHARE * 0.9;

/// Normalize Volume's measuring pass's share of the passes' bar.
const MEASURE_SHARE: f32 = 0.25;

/// Each check pass's share of it.
const CHECK_SHARE: f32 = 0.2;

/// A loudness as the panel shows it: "−16", with U+2212 MINUS SIGN.
fn minus(lufs: i8) -> String {
    if lufs < 0 {
        format!("\u{2212}{}", lufs.unsigned_abs())
    } else {
        lufs.to_string()
    }
}

/// The error for a container with neither sound nor video to keep.
fn nothing_in_it(names: &publish::Names, src: &Path) -> GrapeError {
    GrapeError::new(
        ErrorKind::DecodeFailed,
        format!("{} has no sound or video in it.", names.source),
        format!("no audio or video stream in {}", src.display()),
    )
}

/// How many video (cover art not counted), audio, subtitle, attachment and
/// cover art streams there are among `streams`.
fn kinds<'a>(streams: impl Iterator<Item = &'a StreamInfo>) -> [usize; 5] {
    let mut counts = [0; 5];
    for s in streams {
        match s.kind {
            _ if s.attached_pic => counts[4] += 1,
            StreamKind::Video => counts[0] += 1,
            StreamKind::Audio => counts[1] += 1,
            StreamKind::Subtitle => counts[2] += 1,
            StreamKind::Attachment => counts[3] += 1,
            StreamKind::Data => {}
        }
    }
    counts
}

/// How many cover art streams there are among `streams`.
fn covers<'a>(streams: impl Iterator<Item = &'a StreamInfo>) -> usize {
    streams.filter(|s| s.attached_pic).count()
}

/// How many streams of `kind` `probe` lists, cover art not counted as
/// video.
fn count(probe: &Probe, kind: StreamKind) -> usize {
    probe
        .streams
        .iter()
        .filter(|s| s.kind == kind && !s.attached_pic)
        .count()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::HashSet;
    use std::fs;

    use grape_core::{tools_enabled, CompressPreset, Features, JobOutput, Operation};

    use super::*;
    use crate::media::run::tests::{one_at_a_time, ran, script, slots_held, Fake};
    use crate::media::tests::{assert_forward_to_done, ctx, flat, has, TestDir};
    use crate::Note;

    /// Every format and tool.
    fn all_pairs() -> impl Iterator<Item = (ToolKey, Format)> {
        ToolKey::ALL
            .into_iter()
            .flat_map(|tool| Format::ALL.into_iter().map(move |f| (tool, f)))
            .filter(|&(tool, from)| runs_tool(tool, from))
    }

    #[test]
    fn every_pair_is_offered_in_v1_and_writes_its_planned_format() {
        let mut count = 0;
        for (tool, from) in all_pairs() {
            count += 1;
            assert!(
                tools_enabled(from, 1, Features::V1_ONLY).contains(&tool),
                "{tool:?} on {from:?}"
            );
            // The planned name's format is the one the engine writes.
            let name = format!("in.{}", from.output_ext());
            let op = Operation::tool(tool);
            let planned = grape_core::output_name(&name, &op, &HashSet::new());
            let to = Format::from_file_name(&planned).unwrap();
            let wav_to_flac = tool == ToolKey::Compress && from == Format::Wav;
            let want = if wav_to_flac { Format::Flac } else { from };
            assert_eq!(to, want, "{tool:?} on {from:?}: {planned}");
            assert!(crate::media::args::muxer(to).is_some());
        }
        assert_eq!(count, 25);
    }

    /// A tool job on `name` (the bytes "source") in a fresh folder, with
    /// the output name the plan would give it.
    struct Case {
        dir: TestDir,
        job: Job,
        /// The source's bytes.
        source: Vec<u8>,
    }

    impl Case {
        fn new(name: &str, tool: ToolKey) -> Case {
            Case::sized(name, tool, b"source".to_vec())
        }

        /// A tool job on `name` holding `source`.
        fn sized(name: &str, tool: ToolKey, source: Vec<u8>) -> Case {
            let dir = TestDir::new();
            let src = dir.0.join(name);
            fs::write(&src, &source).unwrap();
            let op = Operation::tool(tool);
            let planned = grape_core::output_name(name, &op, &HashSet::new());
            Case {
                job: Job {
                    inputs: vec![src],
                    output: JobOutput::File(dir.0.join(planned)),
                    op,
                    used_fallback: false,
                },
                dir,
                source,
            }
        }

        fn dest(&self) -> PathBuf {
            match &self.job.output {
                JobOutput::File(dest) => dest.clone(),
                other => panic!("{other:?}"),
            }
        }

        /// Runs the job, returning every progress value reported. Every
        /// job, whatever its end, leaves no FFmpeg slot held: checked here,
        /// while no other test can take one.
        fn run(&self) -> (Result<JobOutcome, GrapeError>, Vec<f32>) {
            let _serial = one_at_a_time();
            let mut seen = Vec::new();
            let result = crate::run_job(&self.job, &ctx(), &mut |f| seen.push(f));
            assert_eq!(slots_held(), 0, "{result:?}");
            (result, seen)
        }

        /// Panics if a temp file was left behind or the source changed.
        fn assert_clean(&self) {
            for entry in fs::read_dir(&self.dir.0).unwrap() {
                let name = entry.unwrap().file_name();
                assert!(
                    !name.to_string_lossy().starts_with(".grape-"),
                    "{name:?} left"
                );
            }
            assert_eq!(fs::read(&self.job.inputs[0]).unwrap(), self.source);
        }
    }

    /// `ffprobe`'s view of an MP4 with H.264, and AAC unless `audio` is
    /// empty.
    fn mp4(audio: &str) -> Fake {
        Fake::Prints(flat("mov,mp4,m4a,3gp,3g2,mj2", true, audio))
    }

    /// A tags probe that found nothing.
    fn no_tags() -> Fake {
        Fake::Prints(String::new())
    }

    /// `ffprobe`'s lines for stream `index`, of `kind` and `codec`, cover
    /// art if `cover`.
    fn track(index: u32, kind: &str, codec: &str, cover: bool) -> String {
        format!(
            "streams.stream.{index}.index={index}\n\
             streams.stream.{index}.codec_name=\"{codec}\"\n\
             streams.stream.{index}.codec_type=\"{kind}\"\n\
             streams.stream.{index}.disposition.attached_pic={}\n",
            u8::from(cover)
        )
    }

    /// `ffprobe`'s view of a `format_name` file holding only cover art.
    fn cover_only(format_name: &str) -> Fake {
        Fake::Prints(format!(
            "{}format.format_name=\"{format_name}\"\nformat.duration=\"2.000000\"\n",
            track(0, "video", "mjpeg", true)
        ))
    }

    #[test]
    fn mute_copies_every_track_but_the_audio() {
        let case = Case::new("clip.mp4", ToolKey::MuteVideo);
        let tags = "format.tags.creation_time=\"2024-05-01T10:20:30Z\"\n";
        script([
            mp4("aac"),
            Fake::Prints(tags.to_owned()),
            Fake::Child("write", "muted".to_owned()),
            mp4(""),
        ]);
        let (result, seen) = case.run();
        let outcome = result.unwrap();
        assert_eq!(outcome.outputs, [case.dir.0.join("clip Muted.mp4")]);
        assert_eq!(fs::read(&outcome.outputs[0]).unwrap(), b"muted");
        assert!(outcome.notes.is_empty());
        let ran = ran();
        assert_eq!(ran.len(), 4);
        // The tags probe asks for XMP from the MOV family.
        assert!(has(&ran[1], ["-export_xmp", "1"]));
        assert!(has(&ran[2], ["-map", "0:0"]) && !has(&ran[2], ["-map", "0:1"]));
        assert!(has(&ran[2], ["-c", "copy"]));
        assert!(has(
            &ran[2],
            ["-metadata", "creation_time=2024-05-01T10:20:30Z"]
        ));
        assert_forward_to_done(&seen);
        case.assert_clean();
    }

    #[test]
    fn a_video_without_sound_is_left_alone_before_anything_runs() {
        let case = Case::new("silent.mov", ToolKey::MuteVideo);
        script([
            Fake::Prints(flat("mov,mp4,m4a,3gp,3g2,mj2", true, "")),
            no_tags(),
        ]);
        let (result, seen) = case.run();
        let err = result.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
        assert_eq!(err.user_message, "silent.mov has no sound to remove.");
        // The probe and the tags probe: no slot, no temp file, no bar.
        assert_eq!(ran().len(), 2);
        assert!(seen.is_empty());
        assert!(!case.dest().exists());
        case.assert_clean();
    }

    #[test]
    fn sound_without_a_picture_has_nothing_to_keep() {
        let case = Case::new("song.mkv", ToolKey::MuteVideo);
        script([Fake::Prints(flat("matroska,webm", false, "aac")), no_tags()]);
        let (result, _) = case.run();
        let err = result.unwrap_err();
        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
        assert_eq!(err.user_message, "song.mkv has no video to keep.");
        assert_eq!(ran().len(), 2);
        case.assert_clean();
    }

    #[test]
    fn an_output_that_kept_sound_is_not_published() {
        let case = Case::new("clip.mp4", ToolKey::MuteVideo);
        script([
            mp4("aac"),
            no_tags(),
            Fake::Child("write", "not muted".to_owned()),
            mp4("aac"),
        ]);
        let (result, _) = case.run();
        let err = result.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert_eq!(err.user_message, "Grape couldn't write a MP4 file.");
        assert!(err.detail.contains("1 audio"), "{}", err.detail);
        assert!(!case.dest().exists());
        case.assert_clean();

        // Nor one with only the cover of an MP4 that has one.
        let case = Case::new("clip.mp4", ToolKey::MuteVideo);
        let mp4 = flat("mov,mp4,m4a,3gp,3g2,mj2", true, "aac");
        script([
            Fake::Prints(format!("{}{mp4}", track(2, "video", "png", true))),
            no_tags(),
            Fake::Child("write", "cover".to_owned()),
            cover_only("mov,mp4,m4a,3gp,3g2,mj2"),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert!(err.detail.contains("0 video"), "{}", err.detail);
        assert!(!case.dest().exists());
        case.assert_clean();

        // Nor an MKV that lost its subtitles.
        let case = Case::new("clip.mkv", ToolKey::MuteVideo);
        let mkv = flat("matroska,webm", true, "aac");
        script([
            Fake::Prints(format!("{}{mkv}", track(2, "subtitle", "subrip", false))),
            no_tags(),
            Fake::Child("write", "muted".to_owned()),
            Fake::Prints(flat("matroska,webm", true, "")),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert!(err.detail.contains("1 subtitles planned"), "{}", err.detail);
        assert!(!case.dest().exists());
        case.assert_clean();

        // Nor an MP4 whose cover art the muxer dropped.
        let covered = format!("{}{mp4}", track(2, "video", "png", true));
        let case = Case::new("clip.mp4", ToolKey::MuteVideo);
        script([
            Fake::Prints(covered.clone()),
            no_tags(),
            Fake::Child("write", "muted".to_owned()),
            Fake::Prints(flat("mov,mp4,m4a,3gp,3g2,mj2", true, "")),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert!(err.detail.contains("0 cover art streams"), "{}", err.detail);
        assert!(!case.dest().exists());
        case.assert_clean();
        // A MOV's isn't even mapped: the MOV muxer has nowhere to put it.
        let case = Case::new("clip.mov", ToolKey::MuteVideo);
        script([
            Fake::Prints(covered),
            no_tags(),
            Fake::Child("write", "muted".to_owned()),
            Fake::Prints(flat("mov,mp4,m4a,3gp,3g2,mj2", true, "")),
        ]);
        case.run().0.unwrap();
        assert!(!ran_args(2).iter().any(|a| a == "0:2"), "{:?}", ran_args(2));
        case.assert_clean();
    }

    #[test]
    fn a_failed_tags_probe_fails_the_job_and_a_cancel_cancels_it() {
        let case = Case::new("clip.mp4", ToolKey::MuteVideo);
        let exit = run::RunError::Exit {
            code: Some(1),
            stderr_tail: "boom".to_owned(),
        };
        script([mp4("aac"), Fake::Fails(exit)]);
        let (result, _) = case.run();
        let err = result.unwrap_err();
        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
        assert_eq!(err.user_message, "clip.mp4 isn't a readable MP4 video.");
        case.assert_clean();

        script([mp4("aac"), Fake::Fails(run::RunError::Cancelled)]);
        let (result, _) = case.run();
        assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
        case.assert_clean();
    }

    #[test]
    fn a_copy_tool_is_never_retried() {
        let case = Case::new("clip.mkv", ToolKey::MuteVideo);
        let refused = "[out#0/matroska @ 0x1] Could not write header: Invalid argument";
        script([
            Fake::Prints(flat("matroska,webm", true, "aac")),
            no_tags(),
            Fake::Child("error", refused.to_owned()),
            Fake::Child("write", "re-encoded".to_owned()),
        ]);
        let (result, _) = case.run();
        let err = result.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert_eq!(ran().len(), 3);
        assert!(!case.dest().exists());
        case.assert_clean();
    }

    /// `ffprobe`'s view of an MP3 with a PNG cover.
    fn mp3_with_cover() -> Fake {
        let cover = "streams.stream.1.index=1\n\
                     streams.stream.1.codec_name=\"png\"\n\
                     streams.stream.1.codec_type=\"video\"\n\
                     streams.stream.1.disposition.attached_pic=1\n";
        Fake::Prints(format!("{cover}{}", flat("mp3", false, "mp3")))
    }

    #[test]
    fn remove_metadata_reports_what_the_source_had() {
        let case = Case::new("song.mp3", ToolKey::RemoveMetadata);
        let tags = "streams.stream.1.index=1\n\
                    streams.stream.1.codec_type=\"video\"\n\
                    streams.stream.1.disposition.attached_pic=1\n\
                    format.tags.title=\"Song\"\n\
                    format.tags.artist=\"Band\"\n\
                    format.tags.genre=\"Rock\"\n";
        script([
            mp3_with_cover(),
            Fake::Prints(tags.to_owned()),
            Fake::Child("write", "stripped".to_owned()),
            Fake::Prints(flat("mp3", false, "mp3")),
            // The output's own tags probe: nothing left.
            Fake::Prints(String::new()),
        ]);
        let (result, seen) = case.run();
        let outcome = result.unwrap();
        assert_eq!(outcome.outputs, [case.dir.0.join("song No Metadata.mp3")]);
        use crate::MetaField::*;
        assert_eq!(
            outcome.notes,
            [Note::MetadataRemoved(vec![Author, Title, CoverArt, Other])]
        );
        let ran = ran();
        assert_eq!(ran.len(), 5);
        // The tags probes ask for no XMP from an MP3; the copy keeps only
        // the audio, with no ID3 at all.
        assert!(!has(&ran[1], ["-export_xmp", "1"]));
        assert!(has(&ran[2], ["-map", "0:0"]) && !has(&ran[2], ["-map", "0:1"]));
        assert!(has(&ran[2], ["-id3v2_version", "0"]));
        assert!(ran[4]
            .last()
            .unwrap()
            .to_string_lossy()
            .contains(".grape-tmp-"));
        assert_forward_to_done(&seen);
        case.assert_clean();

        // A file with nothing to report still gets its (empty) note.
        script([
            Fake::Prints(flat("wav", false, "pcm_s16le")),
            no_tags(),
            Fake::Child("write", "stripped".to_owned()),
            Fake::Prints(flat("wav", false, "pcm_s16le")),
            no_tags(),
        ]);
        let case = Case::new("take.wav", ToolKey::RemoveMetadata);
        let (result, _) = case.run();
        assert_eq!(result.unwrap().notes, [Note::MetadataRemoved(Vec::new())]);
        case.assert_clean();
    }

    #[test]
    fn remove_metadata_asks_an_m4a_and_its_output_for_xmp() {
        let case = Case::new("song.m4a", ToolKey::RemoveMetadata);
        let m4a = || song("mov,mp4,m4a,3gp,3g2,mj2", "aac", 2, true, 16);
        script([
            m4a(),
            no_tags(),
            Fake::Child("write", "stripped".to_owned()),
            m4a(),
            no_tags(),
        ]);
        case.run().0.unwrap();
        // The source's tags probe, then the output's.
        let ran = ran();
        assert!(has(&ran[1], ["-export_xmp", "1"]));
        assert!(has(&ran[4], ["-export_xmp", "1"]));
        case.assert_clean();
    }

    #[test]
    fn metadata_left_in_the_output_fails_the_job() {
        for (left, why) in [
            ("format.tags.encoder=\"Lavf60.16.100\"\n", "Software"),
            ("format.tags.album=\"LP\"\n", "Other"),
            ("chapters.chapter.0.id=0\n", "Chapters"),
            (
                "streams.stream.0.tags.handler_name=\"Apple\"\n",
                "may not keep",
            ),
        ] {
            let case = Case::new("clip.mov", ToolKey::RemoveMetadata);
            script([
                mp4("aac"),
                no_tags(),
                Fake::Child("write", "stripped".to_owned()),
                mp4("aac"),
                Fake::Prints(left.to_owned()),
            ]);
            let (result, _) = case.run();
            let err = result.unwrap_err();
            assert_eq!(err.kind, ErrorKind::EncodeFailed, "{why}: {}", err.detail);
            assert_eq!(
                err.user_message,
                "Grape couldn't remove all of clip.mov's metadata. Nothing was saved."
            );
            assert!(err.detail.contains(why), "{why}: {}", err.detail);
            assert!(!case.dest().exists());
            case.assert_clean();
        }
        // An output missing a track it should keep, or whose tags can't be
        // read, isn't published either.
        let case = Case::new("clip.mp4", ToolKey::RemoveMetadata);
        script([
            mp4("aac"),
            no_tags(),
            Fake::Child("write", "stripped".to_owned()),
            mp4(""),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert!(err.detail.contains("streams by kind"), "{}", err.detail);
        case.assert_clean();
        script([
            mp4("aac"),
            no_tags(),
            Fake::Child("write", "stripped".to_owned()),
            mp4("aac"),
            Fake::Fails(RunError::TimedOut),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.user_message, "Grape couldn't write a MP4 file.");
        assert!(err.detail.contains("tags probe"), "{}", err.detail);
        case.assert_clean();
    }

    #[test]
    fn remove_metadata_needs_something_to_keep() {
        let case = Case::new("cover.m4a", ToolKey::RemoveMetadata);
        let cover_only = "streams.stream.0.index=0\n\
                          streams.stream.0.codec_name=\"mjpeg\"\n\
                          streams.stream.0.codec_type=\"video\"\n\
                          streams.stream.0.disposition.attached_pic=1\n\
                          format.format_name=\"mov,mp4,m4a,3gp,3g2,mj2\"\n";
        script([Fake::Prints(cover_only.to_owned()), no_tags()]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::DecodeFailed);
        assert_eq!(err.user_message, "cover.m4a has no sound to save as M4A.");
        let case = Case::new("empty.mkv", ToolKey::RemoveMetadata);
        let empty = cover_only.replace("mov,mp4,m4a,3gp,3g2,mj2", "matroska,webm");
        script([Fake::Prints(empty), no_tags()]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.user_message, "empty.mkv has no sound or video in it.");
        assert_eq!(ran().len(), 2);
        case.assert_clean();
    }

    /// A Channels job on `name` for `channels`.
    fn channels_case(name: &str, channels: Channels) -> Case {
        let mut case = Case::new(name, ToolKey::AudioChannels);
        let op = Operation::Tool {
            tool: ToolKey::AudioChannels,
            params: ToolParams {
                channels,
                ..ToolParams::default()
            },
        };
        let planned = grape_core::output_name(name, &op, &HashSet::new());
        case.job.op = op;
        case.job.output = JobOutput::File(case.dir.0.join(planned));
        case
    }

    /// `ffprobe`'s view of an audio file of `format_name` with `codec`
    /// audio of `channels` channels, with a layout or not, at `depth`
    /// bits.
    fn song(format_name: &str, codec: &str, channels: u32, layout: bool, depth: u32) -> Fake {
        let flat = flat(format_name, false, codec)
            .replace("channels=2", &format!("channels={channels}"))
            .replace(
                "streams.stream.0.disposition.attached_pic=0\n",
                &format!(
                    "streams.stream.0.disposition.attached_pic=0\n\
                     streams.stream.0.channel_layout=\"{}\"\n\
                     streams.stream.0.bits_per_raw_sample={depth}\n",
                    if layout { "stereo" } else { "unknown" }
                ),
            );
        Fake::Prints(flat)
    }

    #[test]
    fn channels_remixes_and_checks_the_count() {
        let case = channels_case("song.mp3", Channels::Mono);
        script([
            song("mp3", "mp3", 2, true, 16),
            no_tags(),
            Fake::Child("write", "mono".to_owned()),
            song("mp3", "mp3", 1, true, 16),
        ]);
        let (result, seen) = case.run();
        let outcome = result.unwrap();
        assert_eq!(outcome.outputs, [case.dir.0.join("song Mono.mp3")]);
        assert!(outcome.notes.is_empty());
        let ran = ran();
        assert!(has(
            &ran[2],
            ["-af", "aformat=sample_fmts=dbl,pan=mono|c0=0.5*c0+0.5*c1"]
        ));
        assert!(has(&ran[2], ["-c:a", "libmp3lame"]));
        assert!(has(&ran[2], ["-metadata", "REPLAYGAIN_TRACK_GAIN="]));
        assert_forward_to_done(&seen);
        case.assert_clean();

        // An output with the wrong count isn't published.
        let case = channels_case("take.wav", Channels::Stereo);
        script([
            song("wav", "pcm_s24le", 1, true, 24),
            no_tags(),
            Fake::Child("write", "still mono".to_owned()),
            song("wav", "pcm_s24le", 1, true, 24),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert!(has(&ran_args(2), ["-c:a", "pcm_s24le"]));
        assert!(!case.dest().exists());
        case.assert_clean();

        // 5.1 AAC at 384k made stereo: 64k a channel, for the two out.
        let case = channels_case("surround.m4a", Channels::Stereo);
        let m4a = |channels| song("mov,mp4,m4a,3gp,3g2,mj2", "aac", channels, true, 16);
        script([
            tuned(m4a(6), 48_000, Some(384_000)),
            no_tags(),
            Fake::Child("write", "stereo".to_owned()),
            m4a(2),
        ]);
        case.run().0.unwrap();
        assert!(has(&ran_args(2), ["-b:a", "128k"]), "{:?}", ran_args(2));
        case.assert_clean();
    }

    /// The argv of the scripted run `n`.
    fn ran_args(n: usize) -> Vec<std::ffi::OsString> {
        ran()[n].clone()
    }

    #[test]
    fn channels_refuses_before_anything_runs() {
        // (source, target, kind, message)
        let cases = [
            (
                song("flac", "flac", 1, true, 16),
                Channels::Mono,
                ErrorKind::NoChange,
                "song.flac is already mono.",
            ),
            (
                song("flac", "flac", 2, false, 16),
                Channels::Stereo,
                ErrorKind::NoChange,
                "song.flac is already stereo.",
            ),
            (
                song("flac", "flac", 4, false, 16),
                Channels::Stereo,
                ErrorKind::NotYetSupported,
                "song.flac has 4 channels without speaker positions, so Grape can't tell \
                 which are left and right.",
            ),
            (
                song("flac", "flac", 2, true, 32),
                Channels::Mono,
                ErrorKind::NotYetSupported,
                "song.flac is a 32-bit recording. FLAC would cut it, so nothing was saved.",
            ),
            (
                Fake::Prints(flat("flac", false, "")),
                Channels::Mono,
                ErrorKind::DecodeFailed,
                "song.flac has no sound to save as FLAC.",
            ),
        ];
        for (probe, channels, kind, message) in cases {
            let case = channels_case("song.flac", channels);
            script([probe, no_tags()]);
            let (result, seen) = case.run();
            let err = result.unwrap_err();
            assert_eq!(err.kind, kind, "{message}: {}", err.detail);
            assert_eq!(err.user_message, message);
            assert_eq!(ran().len(), 2, "{message}");
            assert!(seen.is_empty());
            case.assert_clean();
        }
        let case = channels_case("song.m4a", Channels::Mono);
        script([
            song("mov,mp4,m4a,3gp,3g2,mj2", "alac", 2, true, 32),
            no_tags(),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(
            err.user_message,
            "song.m4a is a 32-bit recording. ALAC would cut it, so nothing was saved."
        );
        case.assert_clean();
    }

    /// A Normalize job on `name` for `target_lufs`.
    fn normalize_case(name: &str, target_lufs: i8) -> Case {
        let mut case = Case::new(name, ToolKey::NormalizeAudio);
        case.job.op = Operation::Tool {
            tool: ToolKey::NormalizeAudio,
            params: ToolParams {
                target_lufs,
                ..ToolParams::default()
            },
        };
        case
    }

    /// A measuring pass that prints an `ebur128` summary of `lufs` and
    /// `peak`, after the input's tags.
    fn measured(lufs: &str, peak: &str) -> Fake {
        let stderr = format!(
            "Input #0, flac, from 'file:/x/song.flac':\n  Metadata:\n    title           : secret\n\
             [Parsed_ebur128_0 @ 0x1] Summary:\n\n  Integrated loudness:\n    I:  {lufs} LUFS\n\
             \x20   Threshold: -31.1 LUFS\n\n  True peak:\n    Peak:  {peak} dBFS\n"
        );
        Fake::Child("measure", stderr)
    }

    /// The `-af` filter of the scripted run's argv `args`.
    fn filter(args: &[std::ffi::OsString]) -> String {
        let at = args.iter().position(|a| a == "-af").unwrap();
        args[at + 1].to_string_lossy().into_owned()
    }

    fn stereo_flac() -> Fake {
        song("flac", "flac", 2, true, 16)
    }

    #[test]
    fn normalize_measures_then_writes_with_a_linear_gain() {
        let case = normalize_case("song.flac", -16);
        script([
            stereo_flac(),
            no_tags(),
            measured("-21.1", "-18.1"),
            Fake::Child("write", "louder".to_owned()),
            stereo_flac(),
        ]);
        let (result, seen) = case.run();
        let outcome = result.unwrap();
        assert_eq!(outcome.outputs, [case.dir.0.join("song Normalized.flac")]);
        assert_eq!(fs::read(&outcome.outputs[0]).unwrap(), b"louder");
        let ran = ran();
        assert_eq!(ran.len(), 5);
        // The measure: at info level, into nothing.
        assert!(has(&ran[2], ["-v", "info"]) && has(&ran[2], ["-f", "null"]));
        assert_eq!(ran[2].last().unwrap(), "-");
        assert_eq!(filter(&ran[2]), "ebur128=peak=true:framelog=quiet");
        // The write: the gain, dithered to 16 bits, FLAC as it was.
        assert_eq!(
            filter(&ran[3]),
            "aformat=sample_fmts=dbl,volume=5.10dB:precision=double,\
             aresample=osf=s16:dither_method=triangular"
        );
        assert!(has(&ran[3], ["-sample_fmt", "s16"]));
        // The gain makes ReplayGain wrong.
        assert!(has(&ran[3], ["-metadata", "REPLAYGAIN_TRACK_GAIN="]));
        // The measure fills a quarter of the passes' share, the write the
        // rest.
        assert_forward_to_done(&seen);
        assert!(seen.iter().any(|&f| (f - 0.245).abs() < 1e-6), "{seen:?}");
        case.assert_clean();

        // An output at another rate isn't published.
        let case = normalize_case("song.flac", -16);
        script([
            stereo_flac(),
            no_tags(),
            measured("-21.1", "-18.1"),
            Fake::Child("write", "louder".to_owned()),
            tuned(stereo_flac(), 44_100, None),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert!(err.detail.contains("rate Some(44100)"), "{}", err.detail);
        assert!(!case.dest().exists());
        case.assert_clean();
    }

    #[test]
    fn normalize_checks_a_limited_gain_at_most_twice() {
        // The design's peaky.wav (§5.4): G 11.0 measured -21.5, then 16.5
        // measured -17.0, written at 17.5. Mono is measured as dual mono.
        let case = normalize_case("take.wav", -16);
        let wav = || song("wav", "pcm_s24le", 1, true, 24);
        script([
            wav(),
            no_tags(),
            measured("-27.0", "-2.9"),
            measured("-21.5", "-1.6"),
            measured("-17.0", "-1.5"),
            Fake::Child("write", "limited".to_owned()),
            wav(),
        ]);
        let (result, seen) = case.run();
        result.unwrap();
        let ran = ran();
        assert_eq!(ran.len(), 7);
        let limiter = "aresample=192000,alimiter=limit=0.84140:attack=5:release=50:level=0:\
                       latency=1,aresample=48000";
        let dual = "ebur128=peak=true:framelog=quiet:dualmono=true";
        let gain = |g: &str| format!("aformat=sample_fmts=dbl,volume={g}dB:precision=double");
        assert_eq!(filter(&ran[2]), dual);
        assert_eq!(
            filter(&ran[3]),
            format!("{},{limiter},{dual}", gain("11.00"))
        );
        assert_eq!(
            filter(&ran[4]),
            format!("{},{limiter},{dual}", gain("16.50"))
        );
        // A 24-bit WAV: no dither.
        assert_eq!(filter(&ran[5]), format!("{},{limiter}", gain("17.50")));
        assert!(has(&ran[5], ["-c:a", "pcm_s24le"]));
        assert_forward_to_done(&seen);
        case.assert_clean();

        // A check within 0.3 LU writes at once.
        let case = normalize_case("loud.flac", -16);
        script([
            stereo_flac(),
            no_tags(),
            measured("-13.5", "1.6"),
            measured("-16.0", "-1.5"),
            Fake::Child("write", "limited".to_owned()),
            stereo_flac(),
        ]);
        case.run().0.unwrap();
        let again = crate::media::run::tests::ran();
        assert_eq!(again.len(), 6);
        assert!(filter(&again[4]).starts_with(&gain("-2.50")));
        case.assert_clean();
    }

    #[test]
    fn normalize_leaves_a_file_at_its_target_or_too_quiet_alone() {
        for (lufs, peak, target, message) in [
            (
                "-16.3",
                "-3.0",
                -16,
                "song.flac is already at \u{2212}16 LUFS.",
            ),
            (
                "-23.4",
                "-9.0",
                -23,
                "song.flac is already at \u{2212}23 LUFS.",
            ),
            (
                "-70.0",
                "-inf",
                -16,
                "song.flac is too short or too quiet to measure its loudness.",
            ),
        ] {
            let case = normalize_case("song.flac", target);
            script([stereo_flac(), no_tags(), measured(lufs, peak)]);
            let (result, _) = case.run();
            let err = result.unwrap_err();
            assert_eq!(err.kind, ErrorKind::NoChange, "{message}: {}", err.detail);
            assert_eq!(err.user_message, message);
            // Only the measure ran, and nothing was created.
            assert_eq!(ran().len(), 3);
            assert!(!case.dest().exists());
            case.assert_clean();
        }
        // A measure that printed no summary is too short to measure.
        let case = normalize_case("song.flac", -16);
        script([
            stereo_flac(),
            no_tags(),
            Fake::Child("measure", String::new()),
        ]);
        assert_eq!(case.run().0.unwrap_err().kind, ErrorKind::NoChange);
        case.assert_clean();
    }

    #[test]
    fn normalize_refuses_a_target_out_of_range_and_hides_the_tags() {
        for target in [-25, -9, 0] {
            let case = normalize_case("song.flac", target);
            script([stereo_flac(), no_tags()]);
            let err = case.run().0.unwrap_err();
            assert_eq!(err.kind, ErrorKind::EncodeFailed, "{target}");
            assert_eq!(err.user_message, "Grape couldn't use that loudness.");
            assert_eq!(ran().len(), 2);
            case.assert_clean();
        }
        // A measure that fails says only how: its stderr repeats the tags.
        let case = normalize_case("song.flac", -16);
        script([
            stereo_flac(),
            no_tags(),
            Fake::Child("error", "    title           : secret\n".to_owned()),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert!(!err.detail.contains("secret"), "{}", err.detail);
        assert!(err.detail.contains("exited with code 1"), "{}", err.detail);
        case.assert_clean();
    }

    #[test]
    fn a_cancel_between_normalize_passes_starts_nothing_more() {
        let _serial = one_at_a_time();
        let case = normalize_case("take.wav", -16);
        script([
            song("wav", "pcm_s16le", 2, true, 16),
            no_tags(),
            measured("-27.0", "-2.9"),
            measured("-21.5", "-1.6"),
        ]);
        let ctx = ctx();
        let cancel = ctx.cancel.clone();
        // The measure ends at a quarter of the passes' share.
        let err = crate::run_job(&case.job, &ctx, &mut |f| {
            if f >= 0.245 {
                cancel.cancel();
            }
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled, "{}", err.detail);
        assert_eq!(ran().len(), 3);
        assert_eq!(slots_held(), 0);
        assert!(!case.dest().exists());
        case.assert_clean();
    }

    /// A Compress job at `preset` on `name`, a source of 1000 bytes.
    fn compress_case(name: &str, preset: CompressPreset) -> Case {
        let op = Operation::Tool {
            tool: ToolKey::Compress,
            params: ToolParams {
                compress: preset,
                ..ToolParams::default()
            },
        };
        let mut case = Case::sized(name, ToolKey::Compress, vec![b'x'; 1000]);
        let planned = grape_core::output_name(name, &op, &HashSet::new());
        case.job.op = op;
        case.job.output = JobOutput::File(case.dir.0.join(planned));
        case
    }

    /// [`song`] at `rate` Hz, and `bit_rate` b/s if known.
    fn tuned(song: Fake, rate: u32, bit_rate: Option<u64>) -> Fake {
        let Fake::Prints(text) = song else {
            panic!("not a probe")
        };
        let mut text = text.replace("sample_rate=\"48000\"", &format!("sample_rate=\"{rate}\""));
        if let Some(bit_rate) = bit_rate {
            text.push_str(&format!("streams.stream.0.bit_rate=\"{bit_rate}\"\n"));
        }
        Fake::Prints(text)
    }

    /// An FFmpeg run that writes `bytes` bytes.
    fn writes(bytes: usize) -> Fake {
        Fake::Child("write", "y".repeat(bytes))
    }

    #[test]
    fn compress_writes_a_smaller_mp3_and_says_how_much() {
        let case = compress_case("song.mp3", CompressPreset::Balanced);
        let mp3 = || tuned(song("mp3", "mp3", 2, true, 16), 44_100, Some(320_000));
        script([mp3(), no_tags(), writes(900), mp3()]);
        let (result, seen) = case.run();
        let outcome = result.unwrap();
        assert_eq!(outcome.outputs, [case.dir.0.join("song Compressed.mp3")]);
        assert_eq!(
            outcome.notes,
            [Note::Compressed {
                before: 1000,
                after: 900
            }]
        );
        assert_eq!(fs::read(case.dest()).unwrap().len(), 900);
        let args = ran_args(2);
        assert!(has(&args, ["-q:a", "5"]));
        assert!(!args.iter().any(|a| a == "-af"), "{args:?}");
        assert!(!has(&args, ["-metadata", "REPLAYGAIN_TRACK_GAIN="]));
        assert_forward_to_done(&seen);
        case.assert_clean();

        // One byte more than 90 %: nothing is saved, and Small would do more.
        let case = compress_case("song.mp3", CompressPreset::Balanced);
        script([mp3(), no_tags(), writes(901), mp3()]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
        assert_eq!(err.user_message, "song.mp3 is already compact. Try Small.");
        assert!(err.detail.contains("901 bytes from 1000"), "{}", err.detail);
        assert!(!case.dest().exists());
        case.assert_clean();

        // At Small there's nothing smaller to suggest.
        let case = compress_case("song.mp3", CompressPreset::Small);
        script([mp3(), no_tags(), writes(901), mp3()]);
        let err = case.run().0.unwrap_err();
        assert_eq!(
            err.user_message,
            "song.mp3 is already as small as Grape makes it."
        );
        assert!(has(&ran_args(2), ["-q:a", "7"]));
        case.assert_clean();
    }

    #[test]
    fn compress_leaves_a_small_enough_source_alone_before_anything_runs() {
        // (preset, bit rate, message)
        for (preset, bit_rate, message) in [
            (
                CompressPreset::Balanced,
                128_000,
                "song.mp3 is already compact. Try Small.",
            ),
            (
                CompressPreset::Balanced,
                96_000,
                "song.mp3 is already as small as Grape makes it.",
            ),
            (
                CompressPreset::Small,
                96_000,
                "song.mp3 is already as small as Grape makes it.",
            ),
        ] {
            let case = compress_case("song.mp3", preset);
            let mp3 = tuned(song("mp3", "mp3", 2, true, 16), 44_100, Some(bit_rate));
            script([mp3, no_tags()]);
            let (result, seen) = case.run();
            let err = result.unwrap_err();
            assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
            assert_eq!(err.user_message, message);
            assert_eq!(ran().len(), 2);
            assert!(seen.is_empty());
            case.assert_clean();
        }
        // The container's rate stands in for a stream without one.
        let case = compress_case("song.m4a", CompressPreset::High);
        let Fake::Prints(m4a) = song("mov,mp4,m4a,3gp,3g2,mj2", "aac", 2, true, 16) else {
            unreachable!()
        };
        let m4a = m4a.replace(
            "format.duration",
            "format.bit_rate=\"200000\"\nformat.duration",
        );
        script([Fake::Prints(m4a), no_tags()]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.user_message, "song.m4a is already compact. Try Small.");
        assert_eq!(ran().len(), 2);
        case.assert_clean();
    }

    #[test]
    fn compress_makes_flac_of_a_wav_and_checks_its_rate() {
        let case = compress_case("take.wav", CompressPreset::Balanced);
        let wav = tuned(song("wav", "pcm_s24le", 2, true, 24), 96_000, None);
        let flac = || tuned(song("flac", "flac", 2, true, 24), 48_000, None);
        script([wav, no_tags(), writes(900), flac()]);
        let outcome = case.run().0.unwrap();
        assert_eq!(outcome.outputs, [case.dir.0.join("take Compressed.flac")]);
        let args = ran_args(2);
        assert!(has(&args, ["-af", "aresample=48000"]), "{args:?}");
        assert!(has(&args, ["-sample_fmt", "s32"]), "{args:?}");
        assert!(has(&args, ["-f", "flac"]), "{args:?}");
        case.assert_clean();

        // Bits and rate were cut, so 90 % is the rule.
        let case = compress_case("take.wav", CompressPreset::Balanced);
        let wav = tuned(song("wav", "pcm_s24le", 2, true, 24), 96_000, None);
        script([wav, no_tags(), writes(901), flac()]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.user_message, "take.wav is already compact. Try Small.");
        case.assert_clean();

        // Nothing was cut: 95 %.
        let case = compress_case("take.wav", CompressPreset::High);
        let wav = tuned(song("wav", "pcm_s16le", 2, true, 16), 44_100, None);
        let cd = || tuned(song("flac", "flac", 2, true, 16), 44_100, None);
        script([wav, no_tags(), writes(950), cd()]);
        let outcome = case.run().0.unwrap();
        assert_eq!(
            outcome.notes,
            [Note::Compressed {
                before: 1000,
                after: 950
            }]
        );
        case.assert_clean();
        let case = compress_case("take.wav", CompressPreset::High);
        let wav = tuned(song("wav", "pcm_s16le", 2, true, 16), 44_100, None);
        script([wav, no_tags(), writes(951), cd()]);
        let err = case.run().0.unwrap_err();
        assert_eq!(
            err.user_message,
            "take.wav is already as small as Grape makes it."
        );
        case.assert_clean();

        // An output at the wrong rate isn't published.
        let case = compress_case("take.wav", CompressPreset::Balanced);
        let wav = tuned(song("wav", "pcm_s24le", 2, true, 24), 96_000, None);
        let still = tuned(song("flac", "flac", 2, true, 24), 96_000, None);
        script([wav, no_tags(), writes(500), still]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert!(!case.dest().exists());
        case.assert_clean();
    }

    #[test]
    fn compress_refuses_what_flac_cannot_hold() {
        // (codec, depth, channels, message)
        for (codec, depth, channels, message) in [
            (
                "pcm_f32le",
                32,
                2,
                "take.wav is a 32-bit recording. FLAC would clip or cut it, so nothing was saved.",
            ),
            (
                "pcm_s32le",
                32,
                2,
                "take.wav is a 32-bit recording. FLAC would clip or cut it, so nothing was saved.",
            ),
            (
                "pcm_s24le",
                24,
                10,
                "take.wav has 10 channels, more than the 8 FLAC holds.",
            ),
        ] {
            let case = compress_case("take.wav", CompressPreset::Balanced);
            script([song("wav", codec, channels, false, depth), no_tags()]);
            let (result, seen) = case.run();
            let err = result.unwrap_err();
            assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
            assert_eq!(err.user_message, message);
            assert_eq!(ran().len(), 2);
            assert!(seen.is_empty());
            case.assert_clean();
        }
        // No sound at all.
        let case = compress_case("song.flac", CompressPreset::Balanced);
        script([Fake::Prints(flat("flac", false, "")), no_tags()]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
        case.assert_clean();
    }

    /// `ffprobe`'s view of an H.264 and AAC `format_name` file whose AAC
    /// is at `bit_rate`, and whose video has `transfer`.
    fn movie(format_name: &str, bit_rate: Option<u64>, transfer: &str) -> Fake {
        let mut text = flat(format_name, true, "aac");
        if let Some(bit_rate) = bit_rate {
            text.push_str(&format!("streams.stream.1.bit_rate=\"{bit_rate}\"\n"));
        }
        text.push_str(&format!("streams.stream.0.color_transfer=\"{transfer}\"\n"));
        Fake::Prints(text)
    }

    #[test]
    fn compress_encodes_a_video_and_says_how_much_it_saved() {
        let case = compress_case("clip.mp4", CompressPreset::Balanced);
        let mp4 = || movie("mov,mp4,m4a,3gp,3g2,mj2", Some(400_000), "bt709");
        script([mp4(), no_tags(), writes(500), mp4()]);
        let (result, seen) = case.run();
        let outcome = result.unwrap();
        assert_eq!(outcome.outputs, [case.dir.0.join("clip Compressed.mp4")]);
        assert_eq!(
            outcome.notes,
            [Note::Compressed {
                before: 1000,
                after: 500
            }]
        );
        let args = ran_args(2);
        assert!(has(&args, ["-c:0", "libx264"]) && has(&args, ["-crf:0", "23"]));
        assert!(has(&args, ["-c:1", "aac"]) && has(&args, ["-b:1", "128k"]));
        assert_forward_to_done(&seen);
        case.assert_clean();

        // Over 90 %: nothing saved. At Small, nothing smaller to suggest.
        for (preset, message) in [
            (
                CompressPreset::High,
                "clip.mkv is already compact. Try Small.",
            ),
            (
                CompressPreset::Small,
                "clip.mkv is already as small as Grape makes it.",
            ),
        ] {
            let case = compress_case("clip.mkv", preset);
            let mkv = || movie("matroska,webm", None, "");
            script([mkv(), no_tags(), writes(901), mkv()]);
            let err = case.run().0.unwrap_err();
            assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
            assert_eq!(err.user_message, message);
            assert!(!case.dest().exists());
            case.assert_clean();
        }

        // An output that lost its audio isn't published.
        let case = compress_case("clip.mov", CompressPreset::Balanced);
        script([
            movie("mov,mp4,m4a,3gp,3g2,mj2", None, ""),
            no_tags(),
            writes(100),
            Fake::Prints(flat("mov,mp4,m4a,3gp,3g2,mj2", true, "")),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert!(!case.dest().exists());
        case.assert_clean();

        // Nor one that lost its cover art.
        let case = compress_case("clip.mp4", CompressPreset::Balanced);
        let mp4 = flat("mov,mp4,m4a,3gp,3g2,mj2", true, "aac");
        script([
            Fake::Prints(format!("{}{mp4}", track(2, "video", "png", true))),
            no_tags(),
            writes(100),
            Fake::Prints(mp4),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        let counts = "streams by kind [1, 1, 0, 0, 0], [1, 1, 0, 0, 1] planned";
        assert!(err.detail.contains(counts), "{}", err.detail);
        assert!(!case.dest().exists());
        case.assert_clean();
    }

    #[test]
    fn compress_retries_once_when_the_muxer_refuses_a_copied_track() {
        let refused = "[out#0/mp4 @ 0x1] Could not write header: Invalid argument";
        let case = compress_case("clip.mp4", CompressPreset::Balanced);
        // AAC at 120k stereo: copied first.
        let mp4 = || movie("mov,mp4,m4a,3gp,3g2,mj2", Some(120_000), "");
        script([
            mp4(),
            no_tags(),
            Fake::Child("error", refused.to_owned()),
            writes(400),
            mp4(),
        ]);
        let (result, seen) = case.run();
        result.unwrap();
        let (first, again) = (ran_args(2), ran_args(3));
        assert!(!first.iter().any(|a| a == "-c:1"), "{first:?}");
        assert!(has(&again, ["-c:1", "aac"]), "{again:?}");
        assert_forward_to_done(&seen);
        // The copy fills the bar to RETRY_FROM, the retry the rest.
        let at = |f: f32| seen.iter().any(|&s| (s - f).abs() < 1e-4);
        let retry = RETRY_FROM + (PASSES_SHARE - RETRY_FROM) / 4.0;
        assert!(at(RETRY_FROM / 4.0) && at(retry), "{seen:?}");
        case.assert_clean();

        // A copy that timed out, or a full disk, would fail the same way
        // re-encoded.
        let full = "[out#0/mp4 @ 0x1] Error writing trailer: No space left on device";
        for (first, kind) in [
            (Fake::Fails(RunError::TimedOut), ErrorKind::EncodeFailed),
            (
                Fake::Child("error", full.to_owned()),
                ErrorKind::WriteFailed,
            ),
        ] {
            let case = compress_case("clip.mp4", CompressPreset::Balanced);
            script([mp4(), no_tags(), first, writes(400), mp4()]);
            let err = case.run().0.unwrap_err();
            assert_eq!(err.kind, kind, "{}", err.detail);
            assert_eq!(ran().len(), 3);
            assert!(!case.dest().exists());
            case.assert_clean();
        }

        // Nothing copied: nothing to retry.
        let case = compress_case("clip.mp4", CompressPreset::Balanced);
        script([
            movie("mov,mp4,m4a,3gp,3g2,mj2", Some(400_000), ""),
            no_tags(),
            Fake::Child("error", refused.to_owned()),
            writes(400),
        ]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert_eq!(ran().len(), 3);
        assert!(!case.dest().exists());
        case.assert_clean();
    }

    #[test]
    fn compress_refuses_hdr10_and_an_empty_file_before_anything_runs() {
        let case = compress_case("clip.mov", CompressPreset::Balanced);
        script([
            movie("mov,mp4,m4a,3gp,3g2,mj2", None, "smpte2084"),
            no_tags(),
        ]);
        let (result, seen) = case.run();
        let err = result.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotYetSupported, "{}", err.detail);
        assert_eq!(
            err.user_message,
            "clip.mov is HDR10 video, which Compress can't keep yet."
        );
        assert_eq!(ran().len(), 2);
        assert!(seen.is_empty());
        case.assert_clean();

        // HLG is encoded.
        let case = compress_case("clip.mov", CompressPreset::Balanced);
        let hlg = || movie("mov,mp4,m4a,3gp,3g2,mj2", None, "arib-std-b67");
        script([hlg(), no_tags(), writes(100), hlg()]);
        case.run().0.unwrap();
        case.assert_clean();

        let case = compress_case("empty.mkv", CompressPreset::Balanced);
        script([Fake::Prints(flat("matroska,webm", false, "")), no_tags()]);
        let err = case.run().0.unwrap_err();
        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
        assert_eq!(err.user_message, "empty.mkv has no sound or video in it.");
        assert_eq!(ran().len(), 2);
        case.assert_clean();

        // Cover art alone isn't sound or video.
        for (name, format_name) in [
            ("cover.mkv", "matroska,webm"),
            ("cover.mp4", "mov,mp4,m4a,3gp,3g2,mj2"),
        ] {
            let case = compress_case(name, CompressPreset::Balanced);
            script([cover_only(format_name), no_tags()]);
            let err = case.run().0.unwrap_err();
            assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
            assert_eq!(
                err.user_message,
                format!("{name} has no sound or video in it.")
            );
            assert_eq!(ran().len(), 2);
            case.assert_clean();
        }
    }

    #[test]
    fn a_cancel_during_the_pass_leaves_nothing() {
        let _serial = one_at_a_time();
        let case = Case::new("clip.mov", ToolKey::MuteVideo);
        script([
            Fake::Prints(flat("mov,mp4,m4a,3gp,3g2,mj2", true, "aac")),
            no_tags(),
            Fake::Child("write", "muted".to_owned()),
        ]);
        let ctx = ctx();
        let cancel = ctx.cancel.clone();
        let err = crate::run_job(&case.job, &ctx, &mut |_| cancel.cancel()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled, "{}", err.detail);
        assert!(!case.dest().exists());
        assert_eq!(slots_held(), 0);
        case.assert_clean();
    }
}
