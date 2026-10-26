//! Audio and video conversions with the bundled FFmpeg sidecar
//! (`docs/ENGINES.md`).
//!
//! - [`probe`] asks `ffprobe` for the container, the duration (for
//!   progress) and the few stream fields the planner needs, and parses its
//!   `-of flat` output into a [`probe::Probe`].
//! - [`args`] turns the probe and the target format into the `ffmpeg`
//!   passes: one per conversion, two for GIF (palette, then the GIF).
//! - [`run`] starts `ffmpeg` and `ffprobe` from [`crate::SidecarPaths`]
//!   only, reads the output time from `-progress pipe:1`, kills the child
//!   on cancel and lets at most two FFmpeg jobs run at once.
//!
//! **Offline and contained.** Both tools run with `-protocol_whitelist
//! file` and `-format_whitelist` [`args::DEMUXERS`], and every path goes in
//! as a `file:` URL ([`args::file_url`]). So a source can't make FFmpeg
//! reach the network or read other files (an HLS, DASH or `ffconcat`
//! playlist renamed to `.mp4` is refused), and no file name is ever read as
//! an option or a protocol.
//!
//! **Only what the user sees.** A conversion keeps the first video stream
//! that isn't cover art and the first audio stream. Subtitles, data
//! streams, further audio tracks and cover art are dropped, and MP4 and
//! MOV get no timecode track.
//!
//! The source is only ever an input; the output is always the temp path the
//! caller passes, never the planned final name.
//!
//! [`convert`] runs them for one job: it probes the source, writes a temp
//! file next to the planned name (two passes for GIF, through a palette
//! temp file), checks the result with `ffprobe`, and publishes it with
//! [`crate::publish`] like every engine.

pub(crate) mod args;
pub(crate) mod probe;
pub(crate) mod run;

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use grape_core::{Category, Format, Job};

use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{EngineCtx, ErrorKind, GrapeError, JobOutcome, SidecarPaths};

use args::{Missing, Pass};
use probe::Probe;
use run::RunError;

/// How long `ffprobe` may take to read a source's headers.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Where the progress bar is once FFmpeg's passes are done: they fill it
/// up to here ([`Bar`]).
const PASSES_SHARE: f32 = 0.98;

/// How far a run that copies streams fills the bar: half of
/// [`PASSES_SHARE`], so a re-encode after a refused copy, however late it
/// was refused, always has the other half. A copy that works is fast, and
/// the bar goes from here to [`CHECKED`].
const COPY_SHARE: f32 = PASSES_SHARE / 2.0;

/// Where the progress bar is once the output is checked and synced; it
/// reaches 1.0 only once the output is published.
const CHECKED: f32 = 0.99;

/// True for every audio and video pair in the matrix: each audio format to
/// the other seven, each video format to the other video formats, GIF and
/// MP3, and GIF to the six video formats (104 pairs).
pub(crate) fn supports(from: Format, to: Format) -> bool {
    matches!(from.category(), Category::Audio | Category::Video)
        && grape_core::targets(from).contains(&to)
        && args::muxer(to).is_some()
}

/// True when the app passed both sidecars this engine runs.
pub(crate) fn has_sidecars(sidecars: &SidecarPaths) -> bool {
    sidecars.ffmpeg.is_some() && sidecars.ffprobe.is_some()
}

/// Converts the audio or video file `src` (of format `from`, by its name)
/// to `to` at `dest`, the path the plan reserved.
pub(crate) fn convert(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, to);
    let dir = publish::dest_dir(dest, &names)?;
    let cancel = &ctx.cancel;
    let mut bar = Bar::new(progress);
    let or_cancelled = |e| publish::or_cancelled(cancel, src, e);
    let (Some(ffmpeg), Some(ffprobe)) = (
        ctx.sidecars.ffmpeg.as_deref(),
        ctx.sidecars.ffprobe.as_deref(),
    ) else {
        return Err(missing_sidecar("no ffmpeg or ffprobe path".to_string()));
    };

    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    // FFmpeg gets absolute `file:` URLs, so nothing depends on its working
    // folder.
    let src_abs = absolute(src).map_err(|e| names.read_failed(e))?;
    source_is_a_file(&src_abs, src, &names)?;
    let dir_abs = absolute(dir).map_err(|e| names.write_failed(e))?;
    ffmpeg_can_name(&src_abs, &dir_abs, &names)?;
    let info = probe_source(ffprobe, &src_abs, src, &names, ctx).map_err(or_cancelled)?;
    // Refuse a source without the stream the target needs before anything
    // is created.
    if let Some(missing) = missing_stream(to, &info) {
        return Err(no_stream(&names, missing, src));
    }
    if to == Format::Aiff {
        if let Some(bytes) = args::aiff_bytes(&info).filter(|&b| b > args::AIFF_MAX_LEN) {
            let detail = format!("{bytes} bytes of samples from {}", src.display());
            return Err(too_long_for_aiff(&names, detail));
        }
    }

    publish::ensure_dir(job, ctx, dir, &names)?;
    // From here on, the temp files remove themselves on every return path.
    let create = |ext| {
        TempFile::create(dir, ext)
            .map(|(temp, _file)| temp)
            .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))
    };
    let temp = create(to.output_ext())?;
    let palette = if to == Format::Gif {
        Some(create("png")?)
    } else {
        None
    };
    let out = absolute(temp.path()).map_err(|e| names.write_failed(e))?;
    let palette_path = match &palette {
        Some(palette) => absolute(palette.path()).map_err(|e| names.write_failed(e))?,
        None => out.with_extension("png"),
    };

    // At most two FFmpeg jobs run at once; a cancel while waiting ends here.
    let Some(_slot) = run::acquire(cancel) else {
        return Err(crate::cancelled(src));
    };
    // The wait can be as long as two other encodes: a source moved or
    // deleted meanwhile can't be read.
    source_is_a_file(&src_abs, src, &names)?;
    let first = args::passes(&src_abs, to, &info, &out, &palette_path)
        .map_err(|missing| no_stream(&names, missing, src))?;
    let copies = args::copy_plan(to, &info).any();
    let share = if copies { COPY_SHARE } else { PASSES_SHARE };
    let warnings = match run_passes(ffmpeg, &first, info.duration, ctx, &mut bar, share) {
        Ok(warnings) => warnings,
        Err(RunError::Cancelled) => return Err(crate::cancelled(src)),
        Err(e) => {
            let refused = matches!(e, RunError::Exit { .. });
            let failure = or_cancelled(ffmpeg_failed(&names, e, src));
            if !(refused && copies && worth_a_retry(&failure)) {
                return Err(failure);
            }
            // A copied stream can fit by its codec and still not mux: one
            // retry re-encoding everything, filling what's left of the bar.
            let again = args::reencode_passes(&src_abs, to, &info, &out, &palette_path)
                .map_err(|missing| no_stream(&names, missing, src))?;
            run_passes(ffmpeg, &again, info.duration, ctx, &mut bar, PASSES_SHARE)
                .map_err(|e| or_cancelled(ffmpeg_failed(&names, e, src)))?
        }
    };
    drop(palette);

    check_output(ffprobe, &out, to, &names, ctx, src)
        .map_err(|mut e| {
            if !warnings.trim().is_empty() {
                e.detail = format!("{}; FFmpeg said: {}", e.detail, warnings.trim());
            }
            e
        })
        .map_err(or_cancelled)?;
    // FFmpeg doesn't sync its output. Windows only flushes a file opened for
    // writing; this never truncates or creates it. No test can tell whether
    // the sync ran: only a crash or power cut right after would show it.
    OpenOptions::new()
        .write(true)
        .open(&out)
        .and_then(|file| file.sync_all())
        .map_err(|e| names.write_failed(format!("sync {}: {e}", out.display())))?;
    bar.to(CHECKED);

    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    drop(temp);
    bar.published();
    Ok(outcome)
}

/// The job's progress bar, over `run_job`'s callback: it only ever moves
/// forward, and stays below 1.0 until the output is published.
struct Bar<'a> {
    report: &'a mut dyn FnMut(f32),
    /// The furthest it has been.
    shown: f32,
}

impl<'a> Bar<'a> {
    fn new(report: &'a mut dyn FnMut(f32)) -> Bar<'a> {
        Bar { report, shown: 0.0 }
    }

    /// Moves the bar to `fraction`, at most [`CHECKED`], unless it is
    /// already there or further.
    fn to(&mut self, fraction: f32) {
        let fraction = fraction.min(CHECKED);
        if fraction > self.shown {
            self.shown = fraction;
            (self.report)(fraction);
        }
    }

    /// Fills the bar: the output is published.
    fn published(&mut self) {
        self.shown = 1.0;
        (self.report)(1.0);
    }
}

/// Whether FFmpeg gets `path` as it is. On Windows FFmpeg rebuilds its argv
/// as UTF-8 (`prepare_app_arguments` in `cmdutils.c`), which turns an
/// unpaired surrogate, allowed in NTFS names, into U+FFFD: it would open or
/// write another file. Elsewhere it gets the bytes unchanged.
fn ffmpeg_gets_unchanged(path: &Path) -> bool {
    cfg!(not(windows)) || path.to_str().is_some()
}

/// Refuses a source, or an output folder, that FFmpeg wouldn't get
/// unchanged ([`ffmpeg_gets_unchanged`]). The temp files are that folder
/// plus an ASCII name, so they pass when it does.
fn ffmpeg_can_name(src_abs: &Path, dir_abs: &Path, names: &Names) -> Result<(), GrapeError> {
    let detail = |path: &Path| format!("not valid Unicode: {}", path.display());
    if !ffmpeg_gets_unchanged(src_abs) {
        return Err(GrapeError::new(
            ErrorKind::ReadFailed,
            format!(
                "{} or its folder has a name Grape's media converter can't open. \
                 Rename it and try again.",
                names.source
            ),
            detail(src_abs),
        ));
    }
    if !ffmpeg_gets_unchanged(dir_abs) {
        return Err(GrapeError::new(
            ErrorKind::WriteFailed,
            format!(
                "{} or a folder above it has a name Grape's media converter can't \
                 write to. Rename it and try again.",
                names.folder
            ),
            detail(dir_abs),
        ));
    }
    Ok(())
}

/// Checks that the source is still a file: a folder or a pipe named like
/// media isn't read, and FFmpeg would wait on a pipe.
fn source_is_a_file(src_abs: &Path, src: &Path, names: &Names) -> Result<(), GrapeError> {
    match std::fs::metadata(src_abs) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(_) => Err(names.read_failed(format!("not a file: {}", src.display()))),
        Err(e) => Err(names.read_failed(format!("open {}: {e}", src.display()))),
    }
}

/// Whether a copy that FFmpeg refused with `failure` is worth one retry
/// re-encoding everything: only for what a re-encode can change, a muxer
/// that refused a stream (any encode or decode failure). An output that
/// can't be written (a full disk, a folder gone) or a source that can't be
/// read would fail the same way after a whole encode, or quietly come out
/// re-encoded where it should have been copied; a cancel is final.
fn worth_a_retry(failure: &GrapeError) -> bool {
    matches!(
        failure.kind,
        ErrorKind::EncodeFailed | ErrorKind::DecodeFailed
    )
}

/// `path` made absolute against the current folder, without touching the
/// disk.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|e| format!("absolute path of {}: {e}", path.display()))
}

/// Probes `src_abs`. A source FFmpeg can't open, or one that isn't a
/// container Grape reads (a playlist renamed `.mp4`), isn't readable.
fn probe_source(
    ffprobe: &Path,
    src_abs: &Path,
    src: &Path,
    names: &Names,
    ctx: &EngineCtx,
) -> Result<Probe, GrapeError> {
    let flat = run::probe(
        ffprobe,
        &probe::probe_args(src_abs),
        &ctx.cancel,
        PROBE_TIMEOUT,
    )
    .map_err(|e| match e {
        RunError::Cancelled => crate::cancelled(src),
        RunError::NotAbsolute(_) | RunError::Spawn(_) => {
            missing_sidecar(format!("probe {}: {e}", src.display()))
        }
        RunError::Exit {
            ref stderr_tail, ..
        } => names.decode_failed(format!(
            "probe {}: {e}: {}",
            src.display(),
            stderr_tail.trim()
        )),
        RunError::TimedOut | RunError::Io(_) => {
            names.decode_failed(format!("probe {}: {e}", src.display()))
        }
    })?;
    probe::parse(&flat).map_err(|e| names.decode_failed(format!("probe {}: {e}", src.display())))
}

/// Runs `passes` in order, moving the bar from where it is to `end`
/// ([`COPY_SHARE`] for a run that copies streams, else [`PASSES_SHARE`])
/// by each pass's weight and output time against the source's duration
/// (only at the end of each pass when the duration is unknown), so a
/// retry fills what's left. The GIF palette pass reports no output time
/// until it ends. Returns what the last pass wrote to stderr: FFmpeg's
/// warnings, for the detail of a failed check.
fn run_passes(
    ffmpeg: &Path,
    passes: &[Pass],
    duration: Option<Duration>,
    ctx: &EngineCtx,
    bar: &mut Bar,
    end: f32,
) -> Result<String, RunError> {
    let total = duration.filter(|d| !d.is_zero());
    let start = bar.shown.min(end);
    let span = end - start;
    let mut done = 0.0f32;
    let mut warnings = String::new();
    for pass in passes {
        let weight = pass.weight;
        let mut on_time = |time: Duration| {
            if let Some(total) = total {
                let fraction = (time.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0);
                bar.to(start + span * (done + weight * fraction));
            }
        };
        warnings = run::transcode(ffmpeg, &pass.args, &ctx.cancel, &mut on_time)?.stderr_tail;
        done = (done + weight).min(1.0);
        bar.to(start + span * done);
    }
    Ok(warnings)
}

/// Checks that FFmpeg wrote a file of the target's kind: not empty, and
/// with the stream the target is for.
fn check_output(
    ffprobe: &Path,
    out: &Path,
    to: Format,
    names: &Names,
    ctx: &EngineCtx,
    src: &Path,
) -> Result<(), GrapeError> {
    let wrote = |detail: String| names.encode_failed(format!("check {}: {detail}", out.display()));
    let len = match std::fs::metadata(out) {
        Ok(meta) => meta.len(),
        // FFmpeg wrote into a folder that was removed meanwhile.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(names.write_failed(format!("check {}: {e}", out.display())))
        }
        Err(e) => return Err(wrote(e.to_string())),
    };
    if len == 0 {
        return Err(wrote("FFmpeg wrote nothing".to_string()));
    }
    // The duration checked before converting can be wrong.
    if to == Format::Aiff && len > args::AIFF_MAX_LEN {
        let detail = format!("check {}: {len} bytes", out.display());
        return Err(too_long_for_aiff(names, detail));
    }
    let flat =
        run::probe(ffprobe, &probe::probe_args(out), &ctx.cancel, PROBE_TIMEOUT).map_err(|e| {
            match e {
                RunError::Cancelled => crate::cancelled(src),
                other => wrote(other.to_string()),
            }
        })?;
    let output = probe::parse(&flat).map_err(wrote)?;
    match missing_stream(to, &output) {
        None => Ok(()),
        Some(Missing::Audio) => Err(wrote("no audio stream in the output".to_string())),
        Some(Missing::Video) => Err(wrote("no video stream in the output".to_string())),
    }
}

/// The stream a file needs to be a source for, or an output of, `to` but
/// doesn't have: audio for an audio target, video for a video target or
/// GIF.
fn missing_stream(to: Format, probe: &Probe) -> Option<Missing> {
    match to.category() {
        Category::Audio if probe.audio.is_none() => Some(Missing::Audio),
        Category::Video if probe.video.is_none() => Some(Missing::Video),
        _ => None,
    }
}

/// The error for a source without the stream the target needs.
fn no_stream(names: &Names, missing: Missing, src: &Path) -> GrapeError {
    let (message, what) = match missing {
        Missing::Audio => (
            format!("{} has no sound to save as {}.", names.source, names.to),
            "audio",
        ),
        Missing::Video => (
            format!("{} has no video to convert.", names.source),
            "video",
        ),
    };
    GrapeError::new(
        ErrorKind::DecodeFailed,
        message,
        format!("no {what} stream in {}", src.display()),
    )
}

/// The error for a source too long for AIFF's 32-bit sizes.
fn too_long_for_aiff(names: &Names, detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::EncodeFailed,
        format!("{} is too long to save as {}.", names.source, names.to),
        detail,
    )
}

/// The error when the bundled FFmpeg can't be started.
fn missing_sidecar(detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::EncodeFailed,
        "Grape's media converter is missing. Reinstall Grape.",
        detail,
    )
}

/// How FFmpeg (through `strerror`) words the OS errors of a file it can't
/// open, read or write, lowercased.
const FILE_ERRORS: [&str; 4] = [
    "no such file or directory",
    "permission denied",
    "access is denied",
    "input/output error",
];

/// What FFmpeg says when the output can't be written: the disk is full, the
/// folder or drive is read-only, unplugged or gone.
const WRITE_ERRORS: [&str; 7] = [
    "no space left",
    "disk full",
    "read-only file system",
    "no such file or directory",
    "permission denied",
    "access is denied",
    "input/output error",
];

/// What FFmpeg says about a source it can't decode.
const DECODE_ERRORS: [&str; 3] = [
    "invalid data found",
    "error while decoding",
    "could not find codec",
];

/// Maps a failed `ffmpeg` run to the job's error, judging a failure by the
/// end of what FFmpeg wrote to stderr: a source it couldn't open (moved or
/// deleted while the job waited, its drive gone) couldn't be read; one of
/// [`WRITE_ERRORS`] couldn't be saved; [`DECODE_ERRORS`] isn't readable;
/// anything else, a muxer refusing a stream included, couldn't be written.
fn ffmpeg_failed(names: &Names, e: RunError, src: &Path) -> GrapeError {
    match e {
        RunError::Cancelled => crate::cancelled(src),
        RunError::NotAbsolute(_) | RunError::Spawn(_) => {
            missing_sidecar(format!("convert {}: {e}", src.display()))
        }
        RunError::Exit {
            ref stderr_tail, ..
        } => {
            let detail = format!("convert {}: {e}: {}", src.display(), stderr_tail.trim());
            let tail = stderr_tail.to_ascii_lowercase();
            let says = |needles: &[&str]| needles.iter().any(|needle| tail.contains(needle));
            if tail.contains("error opening input") && says(&FILE_ERRORS) {
                names.read_failed(detail)
            } else if says(&WRITE_ERRORS) {
                names.write_failed(detail)
            } else if says(&DECODE_ERRORS) {
                names.decode_failed(detail)
            } else {
                names.encode_failed(detail)
            }
        }
        RunError::TimedOut | RunError::Io(_) => {
            names.encode_failed(format!("convert {}: {e}", src.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::ffi::OsString;
    use std::fs;

    use grape_core::{JobOutput, Operation};

    use super::*;
    use crate::output::unique_suffix;
    use run::tests::{one_at_a_time, ran, script, slots_held, Fake};

    /// A fresh folder under the system temp folder, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            let path = std::env::temp_dir().join(format!("grape-media-unit-{}", unique_suffix()));
            fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// An absolute path to a program that isn't there.
    fn no_program() -> PathBuf {
        std::env::temp_dir().join(format!("grape-no-sidecar-{}", unique_suffix()))
    }

    /// Sidecar paths for scripted runs: a run past the script fails to
    /// start, as a missing sidecar.
    fn ctx() -> EngineCtx {
        EngineCtx {
            sidecars: SidecarPaths {
                ffmpeg: Some(no_program()),
                ffprobe: Some(no_program()),
                heif_enc: None,
            },
            ..EngineCtx::default()
        }
    }

    /// `ffprobe -of flat` for a file of `format_name` with an H.264 video
    /// and an audio stream of `audio` (none if empty), 2 s long.
    fn flat(format_name: &str, video: bool, audio: &str) -> String {
        let mut text = String::new();
        let mut index = 0;
        if video {
            text.push_str(
                "streams.stream.0.index=0\n\
                 streams.stream.0.codec_name=\"h264\"\n\
                 streams.stream.0.codec_type=\"video\"\n\
                 streams.stream.0.width=160\n\
                 streams.stream.0.height=120\n\
                 streams.stream.0.pix_fmt=\"yuv420p\"\n\
                 streams.stream.0.disposition.attached_pic=0\n",
            );
            index = 1;
        }
        if !audio.is_empty() {
            text.push_str(&format!(
                "streams.stream.{index}.index={index}\n\
                 streams.stream.{index}.codec_name=\"{audio}\"\n\
                 streams.stream.{index}.codec_type=\"audio\"\n\
                 streams.stream.{index}.sample_rate=\"48000\"\n\
                 streams.stream.{index}.channels=2\n\
                 streams.stream.{index}.disposition.attached_pic=0\n"
            ));
        }
        text.push_str(&format!(
            "format.format_name=\"{format_name}\"\nformat.duration=\"2.000000\"\n"
        ));
        text
    }

    /// [`flat`] with `duration` (seconds, as ffprobe prints them) in place
    /// of 2 s, or none at all.
    fn lasting(flat: String, duration: Option<&str>) -> Fake {
        let two = "format.duration=\"2.000000\"\n";
        assert!(flat.contains(two));
        let line = duration.map_or(String::new(), |d| format!("format.duration=\"{d}\"\n"));
        Fake::Prints(flat.replace(two, &line))
    }

    /// An H.264 and AAC Matroska file, which MP4 copies.
    fn mkv_h264_aac() -> Fake {
        Fake::Prints(flat("matroska,webm", true, "aac"))
    }

    /// An H.264 and AAC MP4 file.
    fn mp4_h264_aac() -> Fake {
        Fake::Prints(flat("mov,mp4,m4a,3gp,3g2,mj2", true, "aac"))
    }

    /// True when `pair` appears as two adjacent arguments.
    fn has(args: &[OsString], pair: [&str; 2]) -> bool {
        args.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1])
    }

    /// A job converting `name` (the bytes `source`) to `to`, next to it, in
    /// a fresh folder.
    struct Case {
        dir: TestDir,
        job: Job,
    }

    impl Case {
        fn new(name: &str, to: Format) -> Case {
            let dir = TestDir::new();
            let src = dir.0.join(name);
            fs::write(&src, b"source").unwrap();
            let dest = dir.0.join(format!("converted.{}", to.output_ext()));
            let job = Job {
                inputs: vec![src],
                output: JobOutput::File(dest),
                op: Operation::convert(to),
                used_fallback: false,
            };
            Case { dir, job }
        }

        fn run(&self) -> Result<JobOutcome, GrapeError> {
            let _serial = one_at_a_time();
            crate::run_job(&self.job, &ctx(), &mut |_| {})
        }

        /// [`Case::run`], also returning every progress value reported.
        fn run_seeing_progress(&self) -> (Result<JobOutcome, GrapeError>, Vec<f32>) {
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

    #[test]
    fn only_windows_refuses_names_that_arent_unicode() {
        assert!(ffmpeg_gets_unchanged(Path::new("/Zoë/100% -x/clip.mp4")));
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            // The bytes reach FFmpeg's open() as they are.
            let path = Path::new(OsStr::from_bytes(b"/tmp/\xff\xfe clip.mp4"));
            assert!(ffmpeg_gets_unchanged(path));
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let mut wide: Vec<u16> = r"C:\x\clip".encode_utf16().collect();
            wide.push(0xD800);
            wide.extend(".mp4".encode_utf16());
            assert!(!ffmpeg_gets_unchanged(Path::new(&OsString::from_wide(
                &wide
            ))));
        }
    }

    /// A name with an unpaired surrogate, which NTFS allows.
    #[cfg(windows)]
    fn unpaired(prefix: &str, suffix: &str) -> OsString {
        use std::os::windows::ffi::OsStringExt;
        let mut wide: Vec<u16> = prefix.encode_utf16().collect();
        wide.push(0xD800);
        wide.extend(suffix.encode_utf16());
        OsString::from_wide(&wide)
    }

    #[cfg(windows)]
    #[test]
    fn a_name_ffmpeg_would_change_is_refused_before_it_runs() {
        // The source's name.
        let case = Case::new("clip.mp4", Format::Mov);
        let src = case.dir.0.join(unpaired("clip", ".mp4"));
        fs::rename(&case.job.inputs[0], &src).unwrap();
        let job = Job {
            inputs: vec![src],
            ..case.job.clone()
        };
        script([mp4_h264_aac()]);
        let err = crate::run_job(&job, &ctx(), &mut |_| {}).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReadFailed, "{}", err.detail);
        assert!(
            err.user_message.contains("can't open"),
            "{}",
            err.user_message
        );
        assert!(ran().is_empty());

        // The output folder's name.
        let case = Case::new("clip.mp4", Format::Mov);
        let out_dir = case.dir.0.join(unpaired("out", ""));
        fs::create_dir(&out_dir).unwrap();
        let job = Job {
            output: JobOutput::File(out_dir.join("clip.mov")),
            ..case.job.clone()
        };
        script([mp4_h264_aac()]);
        let err = crate::run_job(&job, &ctx(), &mut |_| {}).unwrap_err();
        assert_eq!(err.kind, ErrorKind::WriteFailed, "{}", err.detail);
        assert!(
            err.user_message.contains("can't write to"),
            "{}",
            err.user_message
        );
        assert!(ran().is_empty());
        assert_eq!(fs::read_dir(&out_dir).unwrap().count(), 0);
        case.assert_clean();
    }

    #[test]
    fn a_source_gone_before_ffmpeg_starts_isnt_read() {
        let case = Case::new("song.wav", Format::Mp3);
        let src = case.job.inputs[0].clone();
        // The source goes while the job waits for a slot, after the probe.
        script([
            Fake::Runs(Box::new(move || {
                fs::remove_file(&src).unwrap();
                flat("wav", false, "pcm_s16le")
            })),
            Fake::Child("write", "mp3".to_string()),
            Fake::Prints(flat("mp3", false, "mp3")),
        ]);
        let err = case.run().unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReadFailed, "{}", err.detail);
        assert_eq!(err.user_message, "Grape couldn't read song.wav.");
        // Only the probe ran.
        assert_eq!(ran().len(), 1);
        assert!(!case.dir.0.join("converted.mp3").exists());
    }

    #[test]
    fn a_copy_that_cant_be_written_fails_without_a_retry() {
        for stderr in [
            "[out#0/mp4 @ 0x1] Error writing trailer: No space left on device",
            "[out#0/mp4 @ 0x1] Error submitting a packet to the muxer: Permission denied",
            "[out#0/mp4 @ 0x1] Error opening output file:/gone/x.mp4: No such file or directory",
        ] {
            let case = Case::new("clip.mkv", Format::Mp4);
            script([mkv_h264_aac(), Fake::Child("error", stderr.to_string())]);
            let err = case.run().unwrap_err();
            assert_eq!(err.kind, ErrorKind::WriteFailed, "{stderr}: {}", err.detail);
            // The probe and the copy: no re-encode after it.
            let ran = ran();
            assert_eq!(ran.len(), 2, "{stderr}");
            assert!(has(&ran[1], ["-c", "copy"]));
            assert!(!case.dir.0.join("converted.mp4").exists());
            case.assert_clean();
        }
    }

    #[test]
    fn a_copy_whose_source_is_gone_fails_without_a_retry() {
        let case = Case::new("clip.mkv", Format::Mp4);
        let stderr = "[in#0 @ 0x1] Error opening input: No such file or directory";
        script([mkv_h264_aac(), Fake::Child("error", stderr.to_string())]);
        let err = case.run().unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReadFailed, "{}", err.detail);
        assert_eq!(err.user_message, "Grape couldn't read clip.mkv.");
        assert_eq!(ran().len(), 2);
        case.assert_clean();
    }

    #[test]
    fn ffmpeg_exiting_0_without_a_usable_output_publishes_nothing() {
        let wav = || Fake::Prints(flat("wav", false, "pcm_s16le"));
        let cases = [
            // Nothing written: no need to probe it.
            (
                Fake::Child("progress", String::new()),
                None,
                "FFmpeg wrote nothing",
            ),
            // Something ffprobe can't read.
            (
                Fake::Child("write", "not an mp3".to_string()),
                Some(Fake::Child(
                    "error",
                    "[in#0 @ 0x1] Error opening input: Invalid data found when processing input"
                        .to_string(),
                )),
                "the sidecar exited with code 1",
            ),
            // A file without the sound it's for.
            (
                Fake::Child("write", "an mp3".to_string()),
                Some(Fake::Prints(flat("mp3", false, ""))),
                "no audio stream in the output",
            ),
        ];
        for (ffmpeg, check, why) in cases {
            let case = Case::new("song.wav", Format::Mp3);
            let runs = 2 + usize::from(check.is_some());
            script([wav(), ffmpeg].into_iter().chain(check));
            let err = case.run().unwrap_err();
            assert_eq!(err.kind, ErrorKind::EncodeFailed, "{why}: {}", err.detail);
            assert_eq!(err.user_message, "Grape couldn't write a MP3 file.");
            assert!(err.detail.contains(why), "{why}: {}", err.detail);
            assert_eq!(ran().len(), runs, "{why}");
            assert!(!case.dir.0.join("converted.mp3").exists(), "{why}");
            case.assert_clean();
        }
    }

    /// Runs `case` with both FFmpeg slots busy and its output in Grape's
    /// fallback folder, which isn't there yet, with `probe` as the only
    /// scripted run: a refusal that waited for a slot, or made the folder,
    /// shows. Returns the error and how long it took; a job that waits for
    /// a slot is cancelled after 2 s.
    fn refuse_with_slots_busy(case: &Case, probe: Fake) -> (GrapeError, Duration) {
        let _serial = one_at_a_time();
        let token = crate::CancelToken::new();
        let busy = (run::acquire(&token).unwrap(), run::acquire(&token).unwrap());
        let JobOutput::File(dest) = &case.job.output else {
            panic!("{:?}", case.job.output);
        };
        let fallback = case.dir.0.join("fallback");
        let job = Job {
            output: JobOutput::File(fallback.join(dest.file_name().unwrap())),
            used_fallback: true,
            ..case.job.clone()
        };
        script([probe]);
        let ctx = ctx();
        let (finished, watching) = std::sync::mpsc::channel::<()>();
        let watchdog = {
            let cancel = ctx.cancel.clone();
            std::thread::spawn(move || {
                if watching.recv_timeout(Duration::from_secs(2)).is_err() {
                    cancel.cancel();
                }
            })
        };
        let started = std::time::Instant::now();
        let err = crate::run_job(&job, &ctx, &mut |_| {}).unwrap_err();
        let took = started.elapsed();
        drop(finished);
        watchdog.join().unwrap();
        drop(busy);
        assert!(!fallback.exists(), "{}", err.detail);
        assert_eq!(ran().len(), 1, "{}", err.detail);
        case.assert_clean();
        (err, took)
    }

    #[test]
    fn a_source_without_the_needed_stream_is_refused_before_anything_else() {
        let case = Case::new("silent.mp4", Format::Mp3);
        let silent = Fake::Prints(flat("mov,mp4,m4a,3gp,3g2,mj2", true, ""));
        let (err, took) = refuse_with_slots_busy(&case, silent);
        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{}", err.detail);
        assert_eq!(err.user_message, "silent.mp4 has no sound to save as MP3.");
        assert!(took < Duration::from_secs(1), "{took:?}");
    }

    #[test]
    fn a_source_too_long_for_aiff_is_refused_before_anything_else() {
        let case = Case::new("long.flac", Format::Aiff);
        // 16-bit stereo at 48 kHz for 100 000 s: 19.2 GB of samples.
        let long = lasting(flat("flac", false, "flac"), Some("100000.000000"));
        let (err, took) = refuse_with_slots_busy(&case, long);
        assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
        assert_eq!(err.user_message, "long.flac is too long to save as AIFF.");
        assert!(took < Duration::from_secs(1), "{took:?}");
    }

    #[test]
    fn a_cancel_after_the_check_publishes_nothing() {
        let _serial = one_at_a_time();
        let case = Case::new("song.wav", Format::Flac);
        script([
            Fake::Prints(flat("wav", false, "pcm_s16le")),
            Fake::Child("write", "flac".to_string()),
            Fake::Prints(flat("flac", false, "flac")),
        ]);
        let ctx = ctx();
        let cancel = ctx.cancel.clone();
        // CHECKED comes once the output is checked and synced, just before
        // the publish.
        let err = crate::run_job(&case.job, &ctx, &mut |f| {
            if f == CHECKED {
                cancel.cancel();
            }
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled, "{}", err.detail);
        assert_eq!(ran().len(), 3);
        assert!(!case.dir.0.join("converted.flac").exists());
        case.assert_clean();
    }

    #[test]
    fn a_job_holds_an_ffmpeg_slot_while_it_runs() {
        let _serial = one_at_a_time();
        let case = Case::new("song.wav", Format::Flac);
        script([
            Fake::Prints(flat("wav", false, "pcm_s16le")),
            Fake::Child("write", "flac".to_string()),
            Fake::Prints(flat("flac", false, "flac")),
        ]);
        // Progress comes while FFmpeg runs, and after, until the publish.
        let mut held = Vec::new();
        crate::run_job(&case.job, &ctx(), &mut |_| held.push(slots_held())).unwrap();
        assert!(held.len() > 3, "{held:?}");
        assert!(held.iter().all(|&slots| slots == 1), "{held:?}");
        assert_eq!(slots_held(), 0);
    }

    #[test]
    fn the_bar_only_moves_forward_and_fills_once_published() {
        let mut seen = Vec::new();
        let mut report = |f| seen.push(f);
        let mut bar = Bar::new(&mut report);
        bar.to(0.5);
        bar.to(0.2);
        bar.to(0.5);
        bar.to(0.7);
        bar.to(1.0);
        bar.to(2.0);
        bar.published();
        assert_eq!(seen, [0.5, 0.7, CHECKED, 1.0]);
    }

    /// Asserts that `seen` only grows, stays at most [`CHECKED`] until its
    /// last value, and ends at 1.0.
    fn assert_forward_to_done(seen: &[f32]) {
        let (last, before) = seen.split_last().unwrap();
        assert_eq!(*last, 1.0, "{seen:?}");
        assert!(before.iter().all(|&f| f <= CHECKED), "{seen:?}");
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
    }

    /// Asserts that `seen` is `want`, to f32 rounding.
    fn assert_progress(seen: &[f32], want: &[f32]) {
        let same =
            seen.len() == want.len() && seen.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-5);
        assert!(same, "seen {seen:?}, want {want:?}");
    }

    /// The scripted runs of a GIF job whose source `ffprobe` says lasts
    /// `duration`: its probe, the palette pass, the GIF pass and the check.
    /// Each pass reports the output times 0 to 2 s.
    fn gif_job(duration: Option<&str>) -> [Fake; 4] {
        [
            lasting(flat("mov,mp4,m4a,3gp,3g2,mj2", true, "aac"), duration),
            Fake::Child("write", "palette".to_string()),
            Fake::Child("write", "gif".to_string()),
            Fake::Prints(flat("gif", true, "")),
        ]
    }

    #[test]
    fn a_gif_fills_the_bar_through_both_passes_by_their_weights() {
        let case = Case::new("clip.mp4", Format::Gif);
        script(gif_job(Some("2.000000")));
        let (result, seen) = case.run_seeing_progress();
        result.unwrap();
        // The palette pass fills 30 % of the passes' 98 %, the GIF pass
        // the other 70 %, each by its output time against the 2 s.
        let want = [
            0.0735, 0.147, 0.2205, 0.294, 0.4655, 0.637, 0.8085, 0.98, CHECKED, 1.0,
        ];
        assert_progress(&seen, &want);
    }

    #[test]
    fn output_times_past_the_probed_duration_count_as_the_end() {
        let case = Case::new("clip.mp4", Format::Gif);
        // ffprobe says 1 s; each pass reports times up to 2 s.
        script(gif_job(Some("1.000000")));
        let (result, seen) = case.run_seeing_progress();
        result.unwrap();
        // The palette pass stops at its 30 %, not further.
        assert_progress(&seen, &[0.147, 0.294, 0.637, 0.98, CHECKED, 1.0]);
    }

    #[test]
    fn without_a_duration_the_bar_moves_at_the_end_of_each_pass() {
        let case = Case::new("clip.mp4", Format::Gif);
        script(gif_job(None));
        let (result, seen) = case.run_seeing_progress();
        result.unwrap();
        assert_progress(&seen, &[0.294, 0.98, CHECKED, 1.0]);
    }

    #[test]
    fn a_copy_refused_part_way_leaves_the_rest_of_the_bar_to_the_re_encode() {
        let case = Case::new("clip.mkv", Format::Mp4);
        // The copy gets to 2 s of 4 s, then the muxer refuses.
        let refused = "[out#0/mp4 @ 0x1] Error writing trailer: Invalid argument";
        script([
            lasting(flat("matroska,webm", true, "aac"), Some("4.000000")),
            Fake::Child("error", refused.to_string()),
            Fake::Child("write", "re-encoded".to_string()),
            mp4_h264_aac(),
        ]);
        let (result, seen) = case.run_seeing_progress();
        result.unwrap();
        assert_eq!(ran().len(), 4);
        // The copy fills half of the passes' 98 % by its output time and
        // gets halfway, to 24.5 %; the re-encode goes on from there,
        // filling what's left by its own output time.
        let want = [
            0.06125, 0.1225, 0.18375, 0.245, 0.336875, 0.42875, 0.520625, 0.6125, 0.98, CHECKED,
            1.0,
        ];
        assert_progress(&seen, &want);
    }

    #[test]
    fn a_copy_refused_at_its_end_leaves_the_re_encode_half_of_the_bar() {
        let case = Case::new("clip.mkv", Format::Mp4);
        // The copy gets to the end (2 s of 2 s), then the muxer refuses
        // its trailer.
        let refused = "[out#0/mp4 @ 0x1] Error writing trailer: Invalid argument";
        script([
            mkv_h264_aac(),
            Fake::Child("error", refused.to_string()),
            Fake::Child("write", "re-encoded".to_string()),
            mp4_h264_aac(),
        ]);
        let (result, seen) = case.run_seeing_progress();
        result.unwrap();
        assert_eq!(ran().len(), 4);
        assert_forward_to_done(&seen);
        // The copy stops at 49 %, and the re-encode moves the bar on from
        // there instead of sitting at its end.
        let want = [
            0.1225,
            0.245,
            0.3675,
            COPY_SHARE,
            0.6125,
            0.735,
            0.8575,
            PASSES_SHARE,
            CHECKED,
            1.0,
        ];
        assert_progress(&seen, &want);
    }

    #[test]
    fn a_copy_that_works_goes_from_half_of_the_bar_to_the_check() {
        let case = Case::new("clip.mkv", Format::Mp4);
        script([
            mkv_h264_aac(),
            Fake::Child("write", "copied".to_string()),
            mp4_h264_aac(),
        ]);
        let (result, seen) = case.run_seeing_progress();
        result.unwrap();
        assert!(has(&ran()[1], ["-c", "copy"]));
        assert_progress(&seen, &[0.1225, 0.245, 0.3675, COPY_SHARE, CHECKED, 1.0]);
    }

    #[test]
    fn progress_reaches_1_only_once_the_output_is_published() {
        let case = Case::new("song.wav", Format::Flac);
        script([
            Fake::Prints(flat("wav", false, "pcm_s16le")),
            Fake::Child("write", "flac".to_string()),
            Fake::Prints(flat("flac", false, "flac")),
        ]);
        let (result, seen) = case.run_seeing_progress();
        result.unwrap();
        assert_forward_to_done(&seen);
        assert_eq!(seen[seen.len() - 2..], [CHECKED, 1.0]);
        assert!(seen.len() > 3, "{seen:?}");
    }

    #[test]
    fn a_copy_the_muxer_refuses_is_retried_re_encoding_everything() {
        let case = Case::new("clip.mkv", Format::Mp4);
        let refused = "[mp4 @ 0x1] track 1: muxing mp3 at 8000hz is not standard\n\
                       [out#0/mp4 @ 0x1] Could not write header (incorrect codec parameters ?): \
                       Invalid argument";
        script([
            mkv_h264_aac(),
            Fake::Child("error", refused.to_string()),
            Fake::Child("write", "re-encoded".to_string()),
            mp4_h264_aac(),
        ]);
        let outcome = case.run().unwrap();
        assert_eq!(fs::read(&outcome.outputs[0]).unwrap(), b"re-encoded");
        let ran = ran();
        assert_eq!(ran.len(), 4);
        assert!(has(&ran[1], ["-c", "copy"]));
        assert!(has(&ran[2], ["-c:v", "libx264"]) && has(&ran[2], ["-c:a", "aac"]));
        case.assert_clean();
    }

    #[test]
    fn a_copy_ffmpeg_cant_decode_is_retried_re_encoding_everything() {
        let case = Case::new("clip.mkv", Format::Mp4);
        // Data FFmpeg finds invalid is its own refusal, judged not
        // readable, so the copy gets the retry as a muxer's refusal does.
        let unreadable =
            "[in#0 @ 0x1] Error during demuxing: Invalid data found when processing input";
        assert_eq!(failed(unreadable), ErrorKind::DecodeFailed);
        script([
            mkv_h264_aac(),
            Fake::Child("error", unreadable.to_string()),
            Fake::Child("write", "re-encoded".to_string()),
            mp4_h264_aac(),
        ]);
        let outcome = case.run().unwrap();
        assert_eq!(fs::read(&outcome.outputs[0]).unwrap(), b"re-encoded");
        // The probe, the copy, the re-encode and the check.
        let ran = ran();
        assert_eq!(ran.len(), 4);
        assert!(has(&ran[1], ["-c", "copy"]));
        assert!(has(&ran[2], ["-c:v", "libx264"]) && has(&ran[2], ["-c:a", "aac"]));
        case.assert_clean();
    }

    #[test]
    fn a_copy_whose_sidecar_cant_be_started_or_watched_isnt_retried() {
        let not_found = std::io::Error::from(std::io::ErrorKind::NotFound);
        let broken = std::io::Error::other("the pipe broke");
        for (error, message) in [
            (
                RunError::Spawn(not_found),
                "Grape's media converter is missing. Reinstall Grape.",
            ),
            (RunError::Io(broken), "Grape couldn't write a MP4 file."),
        ] {
            let case = Case::new("clip.mkv", Format::Mp4);
            script([
                mkv_h264_aac(),
                Fake::Fails(error),
                Fake::Child("write", "re-encoded".to_string()),
                mp4_h264_aac(),
            ]);
            let err = case.run().unwrap_err();
            assert_eq!(err.kind, ErrorKind::EncodeFailed, "{}", err.detail);
            assert_eq!(err.user_message, message);
            // The probe and the copy: no re-encode after it.
            let ran = ran();
            assert_eq!(ran.len(), 2, "{message}");
            assert!(has(&ran[1], ["-c", "copy"]));
            assert!(!case.dir.0.join("converted.mp4").exists());
            case.assert_clean();
        }
    }

    /// The kind of error a failed `ffmpeg` run that wrote `stderr` maps to.
    fn failed(stderr: &str) -> ErrorKind {
        let names = Names::new(
            Path::new("/in/clip.mkv"),
            Path::new("/out/clip.mp4"),
            Format::Mkv,
            Format::Mp4,
        );
        let e = RunError::Exit {
            code: Some(1),
            stderr_tail: stderr.to_string(),
        };
        ffmpeg_failed(&names, e, Path::new("/in/clip.mkv")).kind
    }

    #[test]
    fn ffmpeg_failures_are_judged_by_their_stderr() {
        use ErrorKind::*;
        // Lines as FFmpeg 6.1 and 9.0 print them.
        let cases = [
            // The output can't be written.
            ("[out#0/mp3 @ 0x1] Error writing trailer: No space left on device", WriteFailed),
            ("[out#0/mp4 @ 0x1] Error submitting a packet to the muxer: Disk full", WriteFailed),
            ("av_interleaved_write_frame(): Permission denied", WriteFailed),
            ("[out#0/wav @ 0x1] Error opening output file:/o/x.wav: Access is denied", WriteFailed),
            ("[out#0/mkv @ 0x1] Error writing trailer: Read-only file system", WriteFailed),
            ("[out#0/webm @ 0x1] Error submitting a packet to the muxer: Input/output error", WriteFailed),
            (
                "[out#0/wav @ 0x1] Error opening output file:/gone/.grape-tmp-1.wav: \
                 No such file or directory\nError opening output file file:/gone/.grape-tmp-1.wav.\n\
                 Error opening output files: No such file or directory",
                WriteFailed,
            ),
            // The source can't be opened any more.
            (
                "[in#0 @ 0x1] Error opening input: No such file or directory\n\
                 Error opening input file file:/in/clip.mkv.\n\
                 Error opening input files: No such file or directory",
                ReadFailed,
            ),
            ("[in#0 @ 0x1] Error opening input: Permission denied", ReadFailed),
            ("[in#0 @ 0x1] Error opening input: Input/output error", ReadFailed),
            // It opens but isn't what its name says.
            ("[in#0 @ 0x1] Error opening input: Invalid data found when processing input", DecodeFailed),
            ("[h264 @ 0x1] Error while decoding stream #0:0: Invalid data found", DecodeFailed),
            ("Could not find codec parameters for stream 0", DecodeFailed),
            // A muxer refusing a copied stream: the copy is worth a retry.
            (
                "[mp4 @ 0x1] track 1: muxing mp3 at 8000hz is not standard, to mux anyway set \
                 strict to -1\n[out#0/mp4 @ 0x1] Could not write header (incorrect codec \
                 parameters ?): Invalid argument\nError opening output file file:/o/x.mp4.\n\
                 Error opening output files: Invalid argument",
                EncodeFailed,
            ),
            ("[matroska @ 0x1] Can't write packet with unknown timestamp", EncodeFailed),
            ("Unknown encoder 'libx264'", EncodeFailed),
            ("", EncodeFailed),
        ];
        for (stderr, kind) in cases {
            assert_eq!(failed(stderr), kind, "{stderr}");
        }
        // A sidecar that won't start, and a cancel.
        let names = Names::new(
            Path::new("a.wav"),
            Path::new("o/a.mp3"),
            Format::Wav,
            Format::Mp3,
        );
        let spawn = RunError::Spawn(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(
            ffmpeg_failed(&names, spawn, Path::new("a.wav")).user_message,
            "Grape's media converter is missing. Reinstall Grape."
        );
        let cancelled = ffmpeg_failed(&names, RunError::Cancelled, Path::new("a.wav"));
        assert_eq!(cancelled.kind, Cancelled);
    }

    #[test]
    fn an_output_whose_folder_was_removed_couldnt_be_saved() {
        // FFmpeg writes on into a removed folder (macOS, Linux); the check
        // then finds no file.
        let gone = std::env::temp_dir().join(format!("grape-gone-{}", unique_suffix()));
        let out = gone.join(".grape-tmp-1.webm");
        let src = Path::new("clip.mp4");
        let names = Names::new(src, &gone.join("clip.webm"), Format::Mp4, Format::Webm);
        let err = check_output(
            Path::new("/x"),
            &out,
            Format::Webm,
            &names,
            &EngineCtx::default(),
            src,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::WriteFailed);
        let folder = gone.file_name().unwrap().to_string_lossy();
        assert_eq!(
            err.user_message,
            format!("Grape couldn't save into {folder}.")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_aiff_past_4_gib_fails_its_check() {
        let dir = TestDir::new();
        let src = dir.0.join("long.flac");
        let out = dir.0.join(".grape-tmp-1.aiff");
        let names = Names::new(&src, &dir.0.join("long.aiff"), Format::Flac, Format::Aiff);
        let ctx = EngineCtx::default();
        let file = fs::File::create(&out).unwrap();
        // Sparse: nothing is written.
        file.set_len(args::AIFF_MAX_LEN + 1).unwrap();
        let err = check_output(&no_program(), &out, Format::Aiff, &names, &ctx, &src).unwrap_err();
        assert_eq!(err.kind, ErrorKind::EncodeFailed);
        assert_eq!(err.user_message, "long.flac is too long to save as AIFF.");
        // At the limit the check goes on to ffprobe, missing here.
        file.set_len(args::AIFF_MAX_LEN).unwrap();
        let err = check_output(&no_program(), &out, Format::Aiff, &names, &ctx, &src).unwrap_err();
        assert_eq!(err.user_message, "Grape couldn't write a AIFF file.");
        // Other formats have no such limit.
        let names = Names::new(&src, &dir.0.join("long.wav"), Format::Flac, Format::Wav);
        file.set_len(args::AIFF_MAX_LEN + 1).unwrap();
        let err = check_output(&no_program(), &out, Format::Wav, &names, &ctx, &src).unwrap_err();
        assert_eq!(err.user_message, "Grape couldn't write a WAV file.");
    }
}
