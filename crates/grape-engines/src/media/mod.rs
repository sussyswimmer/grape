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

    use super::*;
    use crate::output::unique_suffix;

    /// A fresh folder under the system temp folder, removed on drop.
    #[cfg(unix)]
    struct TestDir(PathBuf);

    #[cfg(unix)]
    impl TestDir {
        fn new() -> TestDir {
            let path = std::env::temp_dir().join(format!("grape-media-unit-{}", unique_suffix()));
            std::fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }
    }

    #[cfg(unix)]
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An absolute path to a program that isn't there.
    #[cfg(unix)]
    fn no_program() -> PathBuf {
        std::env::temp_dir().join(format!("grape-no-sidecar-{}", unique_suffix()))
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
        let file = std::fs::File::create(&out).unwrap();
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
