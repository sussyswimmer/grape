//! Executes `grape_core` job plans: images, the FFmpeg sidecar, PDF,
//! archives and subtitles (v2: documents).
//!
//! The public contract is the one in `docs/design/m1-drag-wheel.md` §E.1:
//! the app builds a [`grape_core::Selection`] with [`build_selection`]
//! (which reads the disk so `grape-core` doesn't have to), plans it with
//! [`grape_core::plan`], asks [`supports`] whether this build can execute
//! it, and calls [`run_job`] once per job with an [`EngineCtx`]. The rules
//! every engine follows (never touch the source,
//! no network, sidecars via [`SidecarPaths`], cancellation and progress)
//! are in `CLAUDE.md` in this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cancel;
mod headers;
mod image;
mod output;
mod publish;
mod selection;
mod subtitle;

pub use selection::build_selection;

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use grape_core::{Format, Job, JobOutput, Operation};

/// Sidecar locations injected by the app. All `None` in this slice: the
/// image and subtitle engines need no sidecar. Engines never look sidecars up on `PATH`.
#[derive(Debug, Clone, Default)]
pub struct SidecarPaths {
    /// The bundled `ffmpeg` executable.
    pub ffmpeg: Option<PathBuf>,
    /// The bundled `ffprobe` executable.
    pub ffprobe: Option<PathBuf>,
    /// The bundled `heif-enc` executable (Windows HEIC output).
    pub heif_enc: Option<PathBuf>,
}

/// A shared cancel flag. Clones share the flag, so the app keeps one clone
/// and hands another to the engine through [`EngineCtx`].
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks every holder of this token (and its clones) to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// True once [`CancelToken::cancel`] was called on this token or a clone.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What every engine gets besides the job: sidecar paths, a cancel token
/// and Grape's fallback folder.
#[derive(Debug, Clone, Default)]
pub struct EngineCtx {
    /// Where the bundled sidecar executables are.
    pub sidecars: SidecarPaths,
    /// Checked between steps, at every read of the source and write of the
    /// output, and while Grape's own pixel loops run.
    pub cancel: CancelToken,
    /// `Settings::fallback_dir` (`Downloads\Grape`), the one folder an
    /// engine creates when it is missing: for a job that fell back
    /// (`Job::used_fallback`) or whose output setting names this folder.
    /// Any other missing destination is a write error. `None` creates it
    /// only for jobs that fell back.
    pub fallback_dir: Option<PathBuf>,
}

/// What a successful job wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutcome {
    /// Files/folders actually written (final paths).
    pub outputs: Vec<PathBuf>,
    /// True when the planned name was taken at finalize time and a " N" name was used.
    pub renamed: bool,
}

/// Why a job failed, for the app's toast logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// This build can't run the operation (format pair, tool or output shape).
    NotYetSupported,
    /// The source couldn't be opened or read.
    ReadFailed,
    /// The source was read but isn't a valid file of its format.
    DecodeFailed,
    /// The encoder rejected the image (e.g. too large for the target format).
    EncodeFailed,
    /// The output couldn't be written into the destination folder.
    WriteFailed,
    /// The output name kept being taken by other files; nothing was written.
    NameTaken,
    /// The job was cancelled; nothing was written.
    Cancelled,
}

/// A job failure. `Display` is [`GrapeError::user_message`], ready for a
/// toast; [`GrapeError::detail`] is for the log only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{user_message}")]
pub struct GrapeError {
    /// What went wrong.
    pub kind: ErrorKind,
    /// One short sentence in Grape's words, naming the file or folder.
    pub user_message: String,
    /// The underlying `io` or codec error and the paths involved. Never
    /// contains file contents.
    pub detail: String,
}

impl GrapeError {
    pub(crate) fn new(
        kind: ErrorKind,
        user_message: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        GrapeError {
            kind,
            user_message: user_message.into(),
            detail: detail.into(),
        }
    }
}

/// Can this build execute `op` for an input of format `from`?
///
/// In this build: conversions between two different raster formats among
/// JPG, PNG, WebP, TIFF and BMP (20 pairs), and SRT to VTT or TXT and VTT
/// to SRT or TXT (4 pairs). Everything else (HEIC, AVIF, SVG, PDF, audio,
/// video, documents, archives, every tool) is `false`.
pub fn supports(op: &Operation, from: Option<Format>) -> bool {
    match (op, from) {
        (Operation::Convert { to }, Some(from)) => engine_for(from, *to).is_some(),
        _ => false,
    }
}

/// The engines in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    /// Raster images ([`image`]).
    Image,
    /// SRT and WebVTT subtitles ([`subtitle`]).
    Subtitle,
}

/// The engine that converts `from` to `to`, if this build has one. The one
/// place that decides, so [`supports`] and [`run_job`] always agree.
fn engine_for(from: Format, to: Format) -> Option<Engine> {
    if image::supports(from, to) {
        Some(Engine::Image)
    } else if subtitle::supports(from, to) {
        Some(Engine::Subtitle)
    } else {
        None
    }
}

/// Runs one planned job. `progress` gets fractions in 0.0..=1.0 (may never be called).
/// Never opens a source for writing; never replaces an existing file; leaves no temp
/// file behind on error or cancel.
///
/// The input's format comes from its file name, as on the wheel; the
/// decoder then trusts the content. A job this build can't run (see
/// [`supports`]), one without exactly one input, or one whose output isn't
/// a single file fails with [`ErrorKind::NotYetSupported`] before anything
/// is read or written. The output usually lands at the planned path; if
/// that name was taken after planning, the next free `" N"` name is used
/// and [`JobOutcome::renamed`] is set.
pub fn run_job(
    job: &Job,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let from = job
        .inputs
        .first()
        .and_then(|path| path.file_name())
        .and_then(|name| Format::from_file_name(&name.to_string_lossy()));
    match (from, job.inputs.as_slice(), &job.output, job.op) {
        (Some(from), [src], JobOutput::File(dest), Operation::Convert { to }) => {
            match engine_for(from, to) {
                Some(Engine::Image) => image::convert(job, src, dest, from, to, ctx, progress),
                Some(Engine::Subtitle) => {
                    subtitle::convert(job, src, dest, from, to, ctx, progress)
                }
                None => Err(not_yet_supported(job, Some(from))),
            }
        }
        _ => Err(not_yet_supported(job, from)),
    }
}

/// The error for a job this build can't run.
fn not_yet_supported(job: &Job, from: Option<Format>) -> GrapeError {
    let user_message = match (job.op, from) {
        (Operation::Convert { to }, Some(from)) => format!(
            "{} to {} isn't in this build yet.",
            from.display_name(),
            to.display_name()
        ),
        (Operation::Convert { to }, None) => format!(
            "Converting this file to {} isn't in this build yet.",
            to.display_name()
        ),
        // The design note's toast copy for tools (§C).
        (Operation::Tool { .. }, _) => "Tools arrive in a later build.".to_string(),
    };
    GrapeError::new(
        ErrorKind::NotYetSupported,
        user_message,
        format!(
            "no engine for {:?} from {:?} with {} input(s) into {:?}",
            job.op,
            from,
            job.inputs.len(),
            job.output
        ),
    )
}

/// The error for a cancelled job.
fn cancelled(src: &Path) -> GrapeError {
    GrapeError::new(
        ErrorKind::Cancelled,
        "Cancelled. Nothing was saved.",
        format!("cancelled while converting {}", src.display()),
    )
}
