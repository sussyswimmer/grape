//! Executes `grape_core` job plans: images, the FFmpeg sidecar, PDF,
//! archives and subtitles (v2: documents).
//!
//! The public contract is the one in `docs/design/m1-drag-wheel.md` §E.1:
//! the app builds a [`grape_core::Selection`] with [`build_selection`]
//! (which reads the disk so `grape-core` doesn't have to), plans it with
//! [`grape_core::plan`], asks [`supports_with`] whether this build can
//! execute it with the sidecars it found, and calls [`run_job`] once per
//! job with an [`EngineCtx`]. The rules
//! every engine follows (never touch the source,
//! no network, sidecars via [`SidecarPaths`], cancellation and progress)
//! are in `CLAUDE.md` in this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod archive;
mod cancel;
mod headers;
mod heif;
mod helper;
mod image;
mod output;
#[doc(hidden)] // Public only for the worker entry, `pdf::worker_main` (D29).
pub mod pdf;
mod publish;
mod selection;
mod subtitle;

pub use archive::LeftOut;
pub use helper::{set_contain, Contain, Contained};
pub use selection::build_selection;

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use grape_core::{Format, Job, JobOutput, Operation, ToolKey};

/// Sidecar locations injected by the app, which finds them next to its own
/// executable at startup (`apps/desktop/src-tauri/src/sidecars.rs`). One
/// that isn't there is `None`. The image and subtitle engines need none.
/// Engines never look sidecars up on `PATH`.
#[derive(Debug, Clone, Default)]
pub struct SidecarPaths {
    /// The bundled `ffmpeg` executable.
    pub ffmpeg: Option<PathBuf>,
    /// The bundled `ffprobe` executable.
    pub ffprobe: Option<PathBuf>,
    /// The PDFium library the PDF worker loads (D29): `pdfium.dll` next to
    /// Grape's executable on Windows, `libpdfium.dylib` in the app's
    /// `Frameworks` folder on macOS.
    pub pdfium: Option<PathBuf>,
    /// The program started as `<worker> --grape-pdf-worker` (D29): Grape's
    /// own executable.
    pub pdf_worker: Option<PathBuf>,
    /// The HEIC helper (D33), started as `<helper> --grape-heif-worker` on
    /// both systems: `grape-heif.exe` next to Grape's executable on
    /// Windows (Grape's own sidecar, on libheif), Grape's own executable on
    /// macOS (ImageIO).
    pub heif: Option<PathBuf>,
    /// The `bsdtar` sidecar that reads RAR archives (D27): `bsdtar.exe` next
    /// to Grape's executable on Windows, `Contents/MacOS/bsdtar` on macOS.
    pub bsdtar: Option<PathBuf>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobOutcome {
    /// Files/folders actually written (final paths).
    pub outputs: Vec<PathBuf>,
    /// True when the planned name was taken at finalize time and a " N" name was used.
    pub renamed: bool,
    /// What the toast may add besides the outputs: what was left out or
    /// removed. Facts, never words; the app words them. Empty for most jobs.
    /// In the order the engine found them, and at most one
    /// [`Note::MetadataRemoved`] per job.
    pub notes: Vec<Note>,
}

/// A fact about a finished job for the toast. Counts and kinds only:
/// never a name, a path or file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Note {
    /// Archive items that weren't carried over (D27). At least one count
    /// is above zero.
    LeftOut(LeftOut),
    /// PDF → images: this many pages were drawn below 300 DPI because they
    /// were too large (D29).
    PagesScaled(u32),
    /// Remove Metadata: what it found with a value and removed, sorted and
    /// without repeats. Empty when the file had none (the output is still
    /// written).
    MetadataRemoved(Vec<MetaField>),
}

/// A kind of metadata Remove Metadata reports. In the order the toast
/// names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum MetaField {
    /// A document's author (PDF Info `/Author`).
    Author,
    /// Its title (`/Title`).
    Title,
    /// Its subject (`/Subject`).
    Subject,
    /// Its keywords (`/Keywords`).
    Keywords,
    /// The app that made it (`/Creator`).
    Creator,
    /// The tool that wrote the PDF (`/Producer`).
    Producer,
    /// When it was made (`/CreationDate`).
    CreationDate,
    /// When it was last changed (`/ModDate`).
    ModDate,
    /// An XMP metadata packet (the PDF catalog's `/Metadata`).
    Xmp,
    /// Any other entry.
    Other,
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

/// Can this build execute `op` for an input of format `from`, with no
/// sidecars? So never for RAR, PDF or HEIC, which need theirs. The app asks
/// [`supports_with`], with the sidecars it found.
pub fn supports(op: &Operation, from: Option<Format>) -> bool {
    supports_with(op, std::slice::from_ref(&from), &SidecarPaths::default())
}

/// Can this build execute `op` on inputs of these `formats` (one per
/// selected item, `None` for one with no known format) with these
/// `sidecars`? True when `formats` isn't empty and every input has an
/// engine for `op`.
///
/// In this build: raster images among JPG, PNG, WebP, TIFF and BMP (20
/// pairs); SRT to VTT or TXT and VTT to SRT or TXT (4); ZIP, TAR and GZIP
/// among each other (6); RAR to ZIP, TAR or GZIP with `bsdtar` (3); PDF to
/// JPG, PNG, TXT or MD with `pdfium` and `pdf_worker` (4); HEIC to and from
/// JPG, PNG, WebP, TIFF and BMP with `heif` (10). Tools: Extract
/// Archive on ZIP, TAR, GZIP (and RAR with `bsdtar`); Split PDF, Merge PDFs
/// and Remove Metadata on PDFs with `pdfium` and `pdf_worker`.
pub fn supports_with(op: &Operation, formats: &[Option<Format>], sidecars: &SidecarPaths) -> bool {
    !formats.is_empty()
        && formats.iter().all(|from| match (op, *from) {
            (Operation::Convert { to }, Some(from)) => engine_for(from, *to, sidecars).is_some(),
            (Operation::Tool { tool, .. }, Some(from)) => {
                tool_engine(*tool, from, sidecars).is_some()
            }
            (_, None) => false,
        })
}

/// The engines in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    /// Raster images ([`image`]).
    Image,
    /// SRT and WebVTT subtitles ([`subtitle`]).
    Subtitle,
    /// ZIP, TAR, GZIP and RAR ([`archive`]).
    Archive,
    /// PDF, in a worker process ([`pdf`]).
    Pdf,
    /// HEIC, in a helper process ([`heif`]).
    Heif,
}

/// The engine that converts `from` to `to` with these sidecars, if this
/// build has one. The one place that decides, so [`supports_with`] and
/// [`run_job`] always agree.
fn engine_for(from: Format, to: Format, sidecars: &SidecarPaths) -> Option<Engine> {
    if image::supports(from, to) {
        Some(Engine::Image)
    } else if subtitle::supports(from, to) {
        Some(Engine::Subtitle)
    } else if archive::supports(from, to, sidecars.bsdtar.is_some()) {
        Some(Engine::Archive)
    } else if pdf::supports(from, to) && pdf::PdfPaths::from_sidecars(sidecars).is_some() {
        Some(Engine::Pdf)
    } else if heif::supports(from, to) && sidecars.heif.is_some() {
        Some(Engine::Heif)
    } else {
        None
    }
}

/// The engine that runs `tool` on an input of format `from` with these
/// sidecars, if this build has one: [`engine_for`]'s twin for tools.
fn tool_engine(tool: ToolKey, from: Format, sidecars: &SidecarPaths) -> Option<Engine> {
    if tool == ToolKey::ExtractArchive && archive::extracts(from, sidecars.bsdtar.is_some()) {
        Some(Engine::Archive)
    } else if pdf::runs_tool(tool, from) && pdf::PdfPaths::from_sidecars(sidecars).is_some() {
        Some(Engine::Pdf)
    } else {
        None
    }
}

/// The input's format, from its file name, as on the wheel.
fn format_of(path: &Path) -> Option<Format> {
    path.file_name()
        .and_then(|name| Format::from_file_name(&name.to_string_lossy()))
}

/// Runs one planned job. `progress` gets fractions in 0.0..=1.0 (may never be called).
/// Never opens a source for writing; never replaces an existing file; leaves no temp
/// file behind on error or cancel.
///
/// The inputs' formats come from their file names, as on the wheel; the
/// decoders then trust the content. A job this build can't run with
/// `ctx.sidecars` (see [`supports_with`]), or whose inputs and output don't
/// have the shape its operation plans (one input and a file for most
/// conversions; pages for PDF → JPG or PNG; a folder for Extract Archive
/// and Split PDF; every input for Merge PDFs), fails with
/// [`ErrorKind::NotYetSupported`] before anything is read or written. The
/// output usually lands at the planned path; if that name was taken after
/// planning, the next free `" N"` name is used and [`JobOutcome::renamed`]
/// is set. What the job left out or removed is in [`JobOutcome::notes`].
pub fn run_job(
    job: &Job,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let from = job.inputs.first().and_then(|path| format_of(path));
    let sidecars = &ctx.sidecars;
    match (from, job.inputs.as_slice(), &job.output, job.op) {
        (Some(from), [src], JobOutput::File(dest), Operation::Convert { to }) => {
            match engine_for(from, to, &ctx.sidecars) {
                Some(Engine::Image) => image::convert(job, src, dest, from, to, ctx, progress),
                Some(Engine::Subtitle) => {
                    subtitle::convert(job, src, dest, from, to, ctx, progress)
                }
                Some(Engine::Archive) => {
                    let rar = archive::rar::RarPaths::from_sidecars(sidecars);
                    archive::convert(job, src, dest, from, to, ctx, progress, rar.as_ref())
                        .map(archive::ArchiveOutcome::into_outcome)
                }
                // PDF → TXT or MD.
                Some(Engine::Pdf) => run_pdf(job, ctx, progress),
                Some(Engine::Heif) => heif::convert(job, src, dest, from, to, ctx, progress),
                None => Err(not_yet_supported(job, Some(from))),
            }
        }
        // PDF → JPG or PNG: one page writes the file, more the folder.
        (Some(from), [_], JobOutput::PagesFileOrFolder { .. }, Operation::Convert { to }) => {
            match engine_for(from, to, sidecars) {
                Some(Engine::Pdf) => run_pdf(job, ctx, progress),
                _ => Err(not_yet_supported(job, Some(from))),
            }
        }
        (Some(from), [src], JobOutput::Folder(dest), Operation::Tool { tool, .. })
            if tool_engine(tool, from, sidecars) == Some(Engine::Archive) =>
        {
            let rar = archive::rar::RarPaths::from_sidecars(sidecars);
            archive::extract(job, src, dest, from, ctx, progress, rar.as_ref())
                .map(archive::ArchiveOutcome::into_outcome)
        }
        // Split PDF and Remove Metadata (one input), Merge PDFs (all of
        // them): every input must be a PDF. The engine checks the shape.
        (Some(_), inputs, _, Operation::Tool { tool, .. })
            if inputs.iter().all(|path| {
                format_of(path).and_then(|f| tool_engine(tool, f, sidecars)) == Some(Engine::Pdf)
            }) =>
        {
            run_pdf(job, ctx, progress)
        }
        _ => Err(not_yet_supported(job, from)),
    }
}

/// A PDF job ([`pdf::run`] checks the shape), with the worker and the
/// library from the sidecars.
fn run_pdf(
    job: &Job,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let from = job.inputs.first().and_then(|path| format_of(path));
    match pdf::PdfPaths::from_sidecars(&ctx.sidecars) {
        Some(paths) => pdf::run(job, ctx, progress, &paths).map(|done| done.into_outcome(&job.op)),
        None => Err(not_yet_supported(job, from)),
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
