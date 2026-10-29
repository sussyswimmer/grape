//! Executes `grape_core` job plans: images (SVG drawn in Grape's SVG
//! worker), the FFmpeg sidecar, PDF, archives and subtitles (v2:
//! documents).
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
mod imagetool;
mod media;
mod output;
#[doc(hidden)] // Public only for the worker entry, `pdf::worker_main` (D29).
pub mod pdf;
mod publish;
mod selection;
mod subtitle;
#[doc(hidden)] // Public only for the worker entry, `svg::worker_main` and `svg::WORKER_FLAG` (D39).
pub mod svg;

pub use archive::LeftOut;
pub use helper::{remove_stale_work_dirs, set_contain, Contain, Contained};
pub use selection::build_selection;

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use grape_core::{Category, Format, Job, JobOutput, Operation, ToolKey};

/// Sidecar locations injected by the app: absolute paths, which it finds
/// next to its own executable at startup
/// (`apps/desktop/src-tauri/src/sidecars.rs`). One that isn't there is
/// `None`. The image, subtitle and ZIP/TAR/GZIP engines need none; audio and
/// video need `ffmpeg` and `ffprobe`. Engines never look sidecars up on
/// `PATH`.
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
    /// Grape's image helper for HEIC and AVIF (D33, D41), started as
    /// `<helper> --grape-heif-worker` on both systems: `grape-heif.exe` next
    /// to Grape's executable on Windows (Grape's own sidecar, on libheif),
    /// Grape's own executable on macOS (ImageIO, and rav1e for AVIF
    /// output).
    pub heif: Option<PathBuf>,
    /// The `bsdtar` sidecar that reads RAR archives (D27): `bsdtar.exe` next
    /// to Grape's executable on Windows, `Contents/MacOS/bsdtar` on macOS.
    pub bsdtar: Option<PathBuf>,
    /// The program started as `<svg_worker> --grape-svg-worker` (D39):
    /// Grape's own executable, on every system. It draws every SVG.
    pub svg_worker: Option<PathBuf>,
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
    /// What the toast may add besides the outputs: what was left out,
    /// removed or saved. Facts, never words; the app words them. Empty for
    /// most jobs. In the order the engine found them, and at most one
    /// [`Note::MetadataRemoved`] or [`Note::Compressed`] per job.
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
    /// This many pages were drawn below 300 DPI because they were too
    /// large: PDF → images (D29), and SVG pages in a PDF (D39).
    PagesScaled(u32),
    /// Remove Metadata: what it found with a value and removed, sorted and
    /// without repeats. Empty when the file had none (the output is still
    /// written).
    MetadataRemoved(Vec<MetaField>),
    /// Compress: the source's and the output's sizes in bytes (D26).
    Compressed {
        /// The source's size.
        before: u64,
        /// The output's size.
        after: u64,
    },
    /// SVG (D39): this many distinct images an SVG links to, or embeds
    /// past a limit or in a type Grape doesn't draw, were left out. Above
    /// zero; summed over the SVGs of a PDF.
    ImagesLeftOut(u32),
    /// SVG → an image (D39): the SVG was drawn smaller than its size (or
    /// twice it) to fit the target's limits.
    DrawnSmaller,
}

/// A kind of metadata Remove Metadata reports. In the order the toast
/// names them (D26). PDFs report only their Info keys (Author, Title,
/// Subject, Keywords, Creator, Producer and the two dates), Xmp and Other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum MetaField {
    /// Where it was made: GPS, ©xyz, ISO 6709, telemetry tracks.
    Location,
    /// The camera: make, model, lens, serials, firmware, MakerNote.
    Camera,
    /// An author (PDF Info `/Author`; artist, composer, EXIF Artist).
    Author,
    /// A title (`/Title`; a file's or a track's title).
    Title,
    /// A subject (`/Subject`).
    Subject,
    /// Keywords (`/Keywords`).
    Keywords,
    /// Comments, descriptions, synopsis, lyrics, UserComment.
    Comment,
    /// A copyright notice.
    Copyright,
    /// The app that made it (`/Creator`).
    Creator,
    /// The tool that wrote the PDF (`/Producer`).
    Producer,
    /// The software that wrote it: encoder, Software tag, writing app.
    Software,
    /// When it was made (`/CreationDate`; recording dates, timecode).
    CreationDate,
    /// When it was last changed (`/ModDate`).
    ModDate,
    /// An XMP metadata packet (the PDF catalog's `/Metadata`).
    Xmp,
    /// An IPTC-IIM/Photoshop block.
    Iptc,
    /// A thumbnail: EXIF IFD1, JFXX, Photoshop thumbnail.
    Thumbnail,
    /// An attached picture or MKV image attachment.
    CoverArt,
    /// Chapters.
    Chapters,
    /// Extra images or video after a JPEG's end, MPF.
    Embedded,
    /// Any other entry.
    Other,
}

/// Why a job failed, for the app's toast logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The tool would change nothing worth a new file (already compact,
    /// already mono, no sound to remove, already at the target loudness,
    /// too quiet to measure). Nothing was written (D26).
    NoChange,
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
/// sidecars? So never for RAR, PDF, HEIC, AVIF or SVG, which need theirs.
/// The app asks [`supports_with`], with the sidecars it found.
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
/// JPG, PNG, TXT or MD with `pdfium` and `pdf_worker` (4); audio and video
/// among each other (104) with `ffmpeg` and `ffprobe`; with `heif` (D41),
/// HEIC and AVIF to and from JPG, PNG, WebP, TIFF and BMP (20) and HEIC to
/// AVIF and back (2); JPG, PNG, WebP, TIFF and BMP to PDF (5), and HEIC and
/// AVIF to PDF with `heif` (2); with `svg_worker` (D39), SVG to JPG, PNG,
/// WebP, TIFF, BMP and PDF (6), and to HEIC and AVIF with `heif` too (2).
/// With every sidecar, that is every v1 pair. Tools (none for AVIF):
/// Extract Archive on ZIP, TAR, GZIP (and RAR with `bsdtar`); Split PDF,
/// Merge PDFs and Remove Metadata on PDFs with `pdfium` and `pdf_worker`;
/// Create PDF on JPG, PNG, WebP and TIFF (and HEIC with `heif`, SVG with
/// `svg_worker`); with `ffmpeg` and `ffprobe` (D26),
/// Remove Audio on MP4, MOV and MKV, Remove Metadata and Compress on MP3,
/// M4A, WAV, FLAC, MP4, MOV and MKV, and Audio Channels and Normalize
/// Volume on MP3, M4A, WAV and FLAC; with no sidecar (D26, D28), Remove
/// Metadata on JPG, PNG, WebP, GIF, TIFF and HEIC and Compress on JPG,
/// PNG and TIFF.
/// Pure: it doesn't check that the sidecar files are there; [`run_job`]
/// fails cleanly if one is missing.
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
    /// Audio and video through the FFmpeg sidecar ([`media`]).
    Media,
    /// HEIC and AVIF, in a helper process ([`heif`]).
    Heif,
    /// SVG drawn in Grape's SVG worker ([`svg`]).
    Svg,
    /// Images → PDF in Grape's own process (`pdf::create`).
    PdfCreate,
    /// Grape's own image tools ([`imagetool`]).
    ImageTool,
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
    } else if to == Format::Pdf && pdf::create::reads(from, sidecars) {
        Some(Engine::PdfCreate)
    } else if media::supports(from, to) && media::has_sidecars(sidecars) {
        Some(Engine::Media)
    } else if svg::supports(from, to, sidecars) {
        Some(Engine::Svg)
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
        // PDF Remove Metadata stays the PDF engine's.
        Some(Engine::Pdf)
    } else if tool == ToolKey::CreatePdf && pdf::create::combines(from, sidecars) {
        Some(Engine::PdfCreate)
    } else if media::tools::runs_tool(tool, from) && media::has_sidecars(sidecars) {
        Some(Engine::Media)
    } else if imagetool::runs_tool(tool, from, sidecars.heif.is_some()) {
        Some(Engine::ImageTool)
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
/// decoders then trust the content (a `.svg`, `.heic` or `.avif` that is a
/// PNG converts as one). An SVG is drawn in Grape's SVG worker
/// (`svg_worker`), for every target and for Create PDF. A job this build
/// can't run with
/// `ctx.sidecars` (see [`supports_with`]), or whose inputs and output don't
/// have the shape its operation plans (one input and a file for most
/// conversions; pages for PDF → JPG or PNG; a folder for Extract Archive
/// and Split PDF; every input for Merge PDFs, and every input and a file
/// for Create PDF), fails with
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
                Some(Engine::Media) => media::convert(job, src, dest, from, to, ctx, progress),
                Some(Engine::Heif) => heif::convert(job, src, dest, from, to, ctx, progress),
                Some(Engine::Svg) => svg::convert(job, src, dest, to, ctx, progress),
                Some(Engine::PdfCreate) => {
                    pdf::create::convert(job, src, dest, from, ctx, progress)
                }
                // The image tools convert nothing.
                Some(Engine::ImageTool) | None => Err(not_yet_supported(job, Some(from))),
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
        // The tools on one file into one file (D26). A PDF never gets here:
        // `tool_engine` picks the PDF engine for it first.
        (Some(from), [src], JobOutput::File(dest), Operation::Tool { tool, params })
            if tool_engine(tool, from, sidecars) == Some(Engine::Media) =>
        {
            // The planned name says what the output is: the source's
            // format, or FLAC for Compress on a WAV (by its content, which
            // may be named otherwise).
            let to = format_of(dest).unwrap_or(from);
            let compress = tool == ToolKey::Compress;
            let to_flac = compress && to == Format::Flac && from.category() == Category::Audio;
            // Compress on a WAV always writes FLAC.
            if (to != from && !to_flac) || (compress && to == Format::Wav) {
                return Err(not_yet_supported(job, Some(from)));
            }
            media::tools::run(job, src, dest, from, to, tool, &params, ctx, progress)
        }
        (Some(from), [src], JobOutput::File(dest), Operation::Tool { tool, params })
            if tool_engine(tool, from, sidecars) == Some(Engine::ImageTool) =>
        {
            // The image tools write the source's own format.
            if format_of(dest).is_some_and(|to| to != from) {
                return Err(not_yet_supported(job, Some(from)));
            }
            imagetool::run(job, src, dest, from, tool, &params, ctx, progress)
        }
        // Create PDF: one page per input, every input an image this build
        // reads (D37).
        (Some(_), inputs, JobOutput::File(dest), Operation::Tool { tool, .. })
            if tool == ToolKey::CreatePdf
                && inputs.iter().all(|path| {
                    format_of(path).and_then(|f| tool_engine(tool, f, sidecars))
                        == Some(Engine::PdfCreate)
                }) =>
        {
            pdf::create::combine(job, dest, ctx, progress)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_fields_sort_in_the_toasts_order() {
        use MetaField::*;
        // D26's order, which the toast names them in.
        let order = [
            Location,
            Camera,
            Author,
            Title,
            Subject,
            Keywords,
            Comment,
            Copyright,
            Creator,
            Producer,
            Software,
            CreationDate,
            ModDate,
            Xmp,
            Iptc,
            Thumbnail,
            CoverArt,
            Chapters,
            Embedded,
            Other,
        ];
        let mut sorted = order;
        sorted.reverse();
        sorted.sort();
        assert_eq!(sorted, order);
    }
}
