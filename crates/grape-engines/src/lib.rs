//! Executes `grape_core` job plans: images, the FFmpeg sidecar, PDF,
//! archives and subtitles (v2: documents).
//!
//! The public contract is the one in `docs/design/m1-drag-wheel.md` §E.1:
//! the app builds a [`grape_core::JobPlan`], asks [`supports`] whether this
//! build can execute it, and calls [`run_job`] once per job with an
//! [`EngineCtx`]. The rules every engine follows (never touch the source,
//! no network, sidecars via [`SidecarPaths`], cancellation and progress)
//! are in `CLAUDE.md` in this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use grape_core::{Format, Job, Operation};

/// Sidecar locations injected by the app. All `None` in this slice: the
/// image engine needs no sidecar. Engines never look sidecars up on `PATH`.
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

/// What every engine gets besides the job: sidecar paths and a cancel token.
#[derive(Debug, Clone, Default)]
pub struct EngineCtx {
    /// Where the bundled sidecar executables are.
    pub sidecars: SidecarPaths,
    /// Checked before reading and before the output gets its final name.
    pub cancel: CancelToken,
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
pub fn supports(op: &Operation, from: Option<Format>) -> bool {
    let _ = (op, from);
    false
}

/// Runs one planned job. `progress` gets fractions in 0.0..=1.0 (may never be called).
/// Never opens a source for writing; never replaces an existing file; leaves no temp
/// file behind on error or cancel.
pub fn run_job(
    job: &Job,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let _ = (ctx, progress);
    Err(GrapeError::new(
        ErrorKind::NotYetSupported,
        "This isn't in this build yet.",
        format!("no engine for {:?}", job.op),
    ))
}
