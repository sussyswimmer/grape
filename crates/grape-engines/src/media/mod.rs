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
//! streams, further audio tracks and cover art are dropped.
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

/// The share of the progress bar the passes fill; the rest is the check and
/// the publish.
const PASSES_SHARE: f32 = 0.99;

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
    match std::fs::metadata(&src_abs) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(names.read_failed(format!("not a file: {}", src.display()))),
        Err(e) => return Err(names.read_failed(format!("open {}: {e}", src.display()))),
    }
    let info = probe_source(ffprobe, &src_abs, src, &names, ctx).map_err(or_cancelled)?;
    // Refuse a source without the stream the target needs before anything
    // is created.
    if let Some(missing) = missing_stream(to, &info) {
        return Err(no_stream(&names, missing, src));
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
    let first = args::passes(&src_abs, to, &info, &out, &palette_path)
        .map_err(|missing| no_stream(&names, missing, src))?;
    let warnings = match run_passes(ffmpeg, &first, info.duration, ctx, progress) {
        Ok(warnings) => warnings,
        Err(RunError::Cancelled) => return Err(crate::cancelled(src)),
        // A copied stream can fit by its codec and still not mux: one retry
        // re-encoding everything.
        Err(_) if args::copy_plan(to, &info).any() => {
            let again = args::reencode_passes(&src_abs, to, &info, &out, &palette_path)
                .map_err(|missing| no_stream(&names, missing, src))?;
            run_passes(ffmpeg, &again, info.duration, ctx, progress)
                .map_err(|e| or_cancelled(ffmpeg_failed(&names, e, src)))?
        }
        Err(e) => return Err(or_cancelled(ffmpeg_failed(&names, e, src))),
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
    // writing; this never truncates or creates it.
    OpenOptions::new()
        .write(true)
        .open(&out)
        .and_then(|file| file.sync_all())
        .map_err(|e| names.write_failed(format!("sync {}: {e}", out.display())))?;

    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    drop(temp);
    progress(1.0);
    Ok(outcome)
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

/// Runs `passes` in order, reporting the job's progress from each pass's
/// output time against the source's duration (none when it's unknown).
/// Returns what the last pass wrote to stderr: FFmpeg's warnings, for the
/// detail of a failed check.
fn run_passes(
    ffmpeg: &Path,
    passes: &[Pass],
    duration: Option<Duration>,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<String, RunError> {
    let total = duration.filter(|d| !d.is_zero());
    let mut done = 0.0f32;
    let mut warnings = String::new();
    for pass in passes {
        let weight = pass.weight;
        let mut on_time = |time: Duration| {
            if let Some(total) = total {
                let fraction = (time.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0);
                progress((done + weight * fraction) * PASSES_SHARE);
            }
        };
        warnings = run::transcode(ffmpeg, &pass.args, &ctx.cancel, &mut on_time)?.stderr_tail;
        done += weight;
        progress(done.min(1.0) * PASSES_SHARE);
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
    let len = std::fs::metadata(out)
        .map_err(|e| wrote(e.to_string()))?
        .len();
    if len == 0 {
        return Err(wrote("FFmpeg wrote nothing".to_string()));
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

/// The error when the bundled FFmpeg can't be started.
fn missing_sidecar(detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::EncodeFailed,
        "Grape's media converter is missing. Reinstall Grape.",
        detail,
    )
}

/// Maps a failed `ffmpeg` run to the job's error, judging a failure by the
/// end of what FFmpeg wrote to stderr.
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
            if [
                "no space left",
                "permission denied",
                "access is denied",
                "disk full",
            ]
            .iter()
            .any(|needle| tail.contains(needle))
            {
                names.write_failed(detail)
            } else if [
                "invalid data found",
                "error while decoding",
                "could not find codec",
            ]
            .iter()
            .any(|needle| tail.contains(needle))
            {
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
