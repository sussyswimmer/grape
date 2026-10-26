//! The tools that run through FFmpeg (D26), one file in and one out:
//! Remove Audio so far.
//!
//! [`run`] goes through the steps a conversion does ([`super::convert`]),
//! and every refusal, and every [`ErrorKind::NoChange`], comes before
//! anything is created:
//!
//! 1. [`super::begin`]: the sidecars, the source still a file, its probe.
//! 2. The tags probe ([`super::tags`]): what the source has, by key, for
//!    the tool to keep or remove.
//! 3. The tool's own checks (no sound to remove, no video to keep).
//! 4. The temp file, then one of the two FFmpeg slots.
//! 5. The tool's passes ([`argv`]), then the output's check: the
//!    conversion's (not empty, the stream it's for), then the tool's own.
//! 6. The publish ([`super::Begun::publish_checked`]).
//!
//! Tools keep every track but what they remove: streams are mapped by
//! index. Nothing is retried: a copy tool that re-encoded would lose the
//! quality it promises to keep.

mod argv;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use grape_core::{Format, Job, ToolKey, ToolParams};

use super::args::Pass;
use super::probe::{Probe, StreamKind};
use super::run::{self, SlotGuard};
use super::tags::{self, Tags};
use super::{
    absolute, begin, check_output, ffmpeg_failed, probe_failed, run_passes, source_is_a_file, Bar,
    Begun, PASSES_SHARE, PROBE_TIMEOUT,
};
use crate::output::TempFile;
use crate::publish;
use crate::{EngineCtx, ErrorKind, GrapeError, JobOutcome};

/// True when this engine runs `tool` on a file of format `from` (with
/// `ffmpeg` and `ffprobe`; the caller checks those).
pub(crate) fn runs_tool(tool: ToolKey, from: Format) -> bool {
    use Format::{Mkv, Mov, Mp4};
    match tool {
        ToolKey::MuteVideo => matches!(from, Mp4 | Mov | Mkv),
        _ => false,
    }
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
    _params: &ToolParams,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let mut bar = Bar::new(progress);
    let begun = begin(job, src, dest, from, to, ctx)?;
    let tags = read_tags(&begun, from, ctx)?;
    let tool = Tool {
        tool,
        begun: &begun,
        to,
        tags: &tags,
        ctx,
    };
    match tool.tool {
        ToolKey::MuteVideo => tool.mute(&mut bar),
        _ => Err(crate::not_yet_supported(job, Some(from))),
    }
}

/// The tags probe of the source: with `-export_xmp 1` for the MOV family,
/// whose demuxer otherwise drops an XMP packet unseen.
fn read_tags(begun: &Begun, from: Format, ctx: &EngineCtx) -> Result<Tags, GrapeError> {
    let mov = matches!(from, Format::Mp4 | Format::Mov | Format::M4a);
    tags::read(
        begun.ffprobe,
        &begun.src_abs,
        mov,
        &ctx.cancel,
        PROBE_TIMEOUT,
    )
    .map_err(|e| probe_failed(&begun.names, e, begun.src))
    .map_err(|e| publish::or_cancelled(&ctx.cancel, begun.src, e))
}

/// One tool job, once [`run`] has begun it and read the tags.
struct Tool<'a> {
    tool: ToolKey,
    begun: &'a Begun<'a>,
    /// The output's format.
    to: Format,
    tags: &'a Tags,
    ctx: &'a EngineCtx,
}

/// An output FFmpeg wrote and the conversion's check passed.
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
        let subtitles = argv::muted(self.to, info)
            .filter(|s| s.kind == StreamKind::Subtitle)
            .count();
        let written = self.write(bar, |out| {
            argv::mute(&self.begun.src_abs, self.to, info, self.tags, out)
        })?;
        let out = &written.probe;
        if count(out, StreamKind::Audio) > 0 || count(out, StreamKind::Subtitle) != subtitles {
            return Err(self.wrote_wrong(format!(
                "{} audio and {} subtitle streams, {subtitles} subtitles planned",
                count(out, StreamKind::Audio),
                count(out, StreamKind::Subtitle)
            )));
        }
        self.publish(written, bar)
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
        let probe = self.check(&out, &warnings)?;
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

    /// The conversion's check of the output `out`, with FFmpeg's
    /// `warnings` added to a failure's detail.
    fn check(&self, out: &Path, warnings: &str) -> Result<Probe, GrapeError> {
        let Begun {
            ffprobe,
            names,
            src,
            ..
        } = self.begun;
        check_output(ffprobe, out, self.to, names, self.ctx, src)
            .map_err(|mut e| {
                if !warnings.trim().is_empty() {
                    e.detail = format!("{}; FFmpeg said: {}", e.detail, warnings.trim());
                }
                e
            })
            .map_err(|e| publish::or_cancelled(&self.ctx.cancel, src, e))
    }

    /// The error for an output that isn't what the tool planned.
    fn wrote_wrong(&self, detail: String) -> GrapeError {
        self.begun
            .names
            .encode_failed(format!("check {}: {detail}", self.begun.src.display()))
    }

    /// Publishes a checked output.
    fn publish(&self, written: Written, bar: &mut Bar) -> Result<JobOutcome, GrapeError> {
        self.begun
            .publish_checked(written.temp, &written.out, self.ctx, bar)
    }
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

    use grape_core::{tools_enabled, Features, JobOutput, Operation};

    use super::*;
    use crate::media::run::tests::{one_at_a_time, ran, script, slots_held, Fake};
    use crate::media::tests::{assert_forward_to_done, ctx, flat, has, TestDir};

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
            assert_eq!(to, from, "{tool:?} on {from:?}: {planned}");
            assert!(crate::media::args::muxer(to).is_some());
        }
        assert_eq!(count, 3);
    }

    /// A tool job on `name` (the bytes "source") in a fresh folder, with
    /// the output name the plan would give it.
    struct Case {
        dir: TestDir,
        job: Job,
    }

    impl Case {
        fn new(name: &str, tool: ToolKey) -> Case {
            let dir = TestDir::new();
            let src = dir.0.join(name);
            fs::write(&src, b"source").unwrap();
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
            }
        }

        fn dest(&self) -> PathBuf {
            match &self.job.output {
                JobOutput::File(dest) => dest.clone(),
                other => panic!("{other:?}"),
            }
        }

        /// Runs the job, returning every progress value reported.
        fn run(&self) -> (Result<JobOutcome, GrapeError>, Vec<f32>) {
            let _serial = one_at_a_time();
            let mut seen = Vec::new();
            let result = crate::run_job(&self.job, &ctx(), &mut |f| seen.push(f));
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
            assert_eq!(fs::read(&self.job.inputs[0]).unwrap(), b"source");
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
        assert_eq!(slots_held(), 0);
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
