//! The parent side of a PDF job: plan the worker's request, run the
//! worker, publish what it wrote.
//!
//! 1. The sources are sniffed (`%PDF-` near the start) before anything is
//!    created.
//! 2. A hidden temp folder `.grape-tmp-…` is made in the destination
//!    folder; the worker writes into it only.
//! 3. The worker ([`Launch`]) gets the request on stdin. Its stdout is read
//!    on a thread; the job polls every 100 ms and kills the worker when the
//!    job is cancelled or the time is up ([`Limits::timeout_base`] plus
//!    [`Limits::timeout_per_page`] for each page it reports, counting at
//!    most [`Bounds::max_pages`] pages). Progress is passed on, never
//!    backwards. A worker that exits with [`OVER_HEAP_EXIT`] went past its
//!    heap bound.
//! 4. A file output is moved onto a temp file in the destination folder and
//!    published with [`publish::finalize`]; a folder output (pages, split)
//!    is renamed page by page and published with
//!    [`folder::publish_folder`]. Neither ever replaces anything.
//! 5. The temp folder is removed on every path, by its drop.
//!
//! No detail written here holds a path, a name or file contents.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use grape_core::{Format, Job, JobOutput, Operation, ToolKey};

use super::heap::OVER_HEAP_EXIT;
use super::protocol::{
    names, Bounds, Done, FailKind, Failure, ImageFormat, Message, MetaKey, Request, TextFormat,
    WorkerOp, MAX_LINE_BYTES,
};
use super::worker::is_pdf;
use super::WORKER_FLAG;
use crate::archive::folder::{self, PublishFailure, TempDir};
use crate::output::TempFile;
use crate::publish::{self, file_name, Names};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// How often the job checks for a cancel and the deadline.
const POLL: Duration = Duration::from_millis(100);

/// How far the `%PDF-` signature may be from the start (FORMAT_MATRIX
/// rule 3).
const SNIFF_BYTES: u64 = 1024;

/// `CREATE_NO_WINDOW`: a console-subsystem worker (a debug build, the test
/// binary) opens no console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Where the PDF worker and the PDFium library are. The app fills it from
/// `SidecarPaths` (D29): the worker is Grape's own executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PdfPaths {
    /// The executable started as `<worker> --grape-pdf-worker`.
    pub(crate) worker: PathBuf,
    /// The PDFium library (`pdfium.dll`, `libpdfium.dylib`).
    pub(crate) pdfium: PathBuf,
}

/// The bounds of a PDF job.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Limits {
    /// The time every job gets.
    pub(crate) timeout_base: Duration,
    /// The time added for each page the worker reports.
    pub(crate) timeout_per_page: Duration,
    /// What the worker holds to.
    pub(crate) bounds: Bounds,
}

impl Default for Limits {
    /// 10 minutes plus 5 s a page (at most 2,000 pages counted, so at most
    /// about 2.9 hours); pages at 300 DPI, at most 100 megapixels and
    /// 65,535 pixels a side; at most 2,000 pages exported as images and
    /// 100,000 split into files; at most 8 GiB written and 256 MiB of
    /// text; streams decompress to at most 512 MiB while loading; the
    /// worker's heap holds at most 4 GiB.
    fn default() -> Self {
        Limits {
            timeout_base: Duration::from_secs(10 * 60),
            timeout_per_page: Duration::from_secs(5),
            bounds: Bounds {
                dpi: 300.0,
                max_pages: 2_000,
                max_split_pages: 100_000,
                max_page_pixels: 100_000_000,
                max_side: 65_535,
                max_output_bytes: 8 * GIB,
                max_text_bytes: 256 * MIB,
                max_decompressed: 512 * MIB,
                max_heap_bytes: 4 * GIB,
            },
        }
    }
}

/// What a successful PDF job wrote, and what the toast may want to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PdfOutcome {
    /// The output, as every engine reports it.
    pub(crate) outcome: JobOutcome,
    /// Pages written (render, split) or in the output.
    pub(crate) pages: u32,
    /// Render: the pages (from 1) drawn below 300 DPI.
    pub(crate) scaled_pages: Vec<u32>,
    /// Remove Metadata: what was removed.
    pub(crate) removed: Vec<MetaKey>,
    /// Split and merge: PDFium's page import did the job.
    pub(crate) used_pdfium: bool,
}

/// True for the four v1 conversions: PDF → JPG, PNG, TXT, MD.
pub(crate) fn supports(from: Format, to: Format) -> bool {
    from == Format::Pdf && matches!(to, Format::Jpg | Format::Png | Format::Txt | Format::Md)
}

/// True for the v1 PDF tools: Split PDF, Merge PDFs, Remove Metadata.
pub(crate) fn runs_tool(tool: ToolKey, from: Format) -> bool {
    from == Format::Pdf
        && matches!(
            tool,
            ToolKey::SplitPdf | ToolKey::MergePdf | ToolKey::RemoveMetadata
        )
}

/// Runs a PDF job (conversion or tool) as `run_job` will dispatch it.
pub(crate) fn run(
    job: &Job,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    paths: &PdfPaths,
) -> Result<PdfOutcome, GrapeError> {
    run_with(
        job,
        ctx,
        progress,
        &Launch::worker(paths),
        &paths.pdfium,
        &Limits::default(),
    )
}

/// Converts the PDF `src` to `to` into the planned `dest`: pages
/// (`JobOutput::PagesFileOrFolder`) for JPG and PNG, a file for TXT and MD.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert(
    job: &Job,
    src: &Path,
    dest: &JobOutput,
    from: Format,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    paths: &PdfPaths,
) -> Result<PdfOutcome, GrapeError> {
    let launch = Launch::worker(paths);
    convert_with(
        job,
        src,
        dest,
        from,
        to,
        ctx,
        progress,
        &launch,
        &paths.pdfium,
        &Limits::default(),
    )
}

/// Runs the PDF tool `tool` on the job's inputs.
pub(crate) fn tool(
    job: &Job,
    tool: ToolKey,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    paths: &PdfPaths,
) -> Result<PdfOutcome, GrapeError> {
    let launch = Launch::worker(paths);
    tool_with(
        job,
        tool,
        ctx,
        progress,
        &launch,
        &paths.pdfium,
        &Limits::default(),
    )
}

/// The input's format, from its file name.
fn format_of(path: &Path) -> Option<Format> {
    path.file_name()
        .and_then(|name| Format::from_file_name(&name.to_string_lossy()))
}

/// [`run`] with the worker, library and limits given.
pub(crate) fn run_with(
    job: &Job,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    launch: &Launch,
    pdfium: &Path,
    limits: &Limits,
) -> Result<PdfOutcome, GrapeError> {
    let from = job.inputs.first().and_then(|path| format_of(path));
    match (job.op, job.inputs.as_slice()) {
        (Operation::Convert { to }, [src]) if from == Some(Format::Pdf) => convert_with(
            job,
            src,
            &job.output,
            Format::Pdf,
            to,
            ctx,
            progress,
            launch,
            pdfium,
            limits,
        ),
        (Operation::Tool { tool, .. }, _) if from == Some(Format::Pdf) => {
            tool_with(job, tool, ctx, progress, launch, pdfium, limits)
        }
        _ => Err(crate::not_yet_supported(job, from)),
    }
}

/// [`convert`] with the worker, library and limits given.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_with(
    job: &Job,
    src: &Path,
    dest: &JobOutput,
    from: Format,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    launch: &Launch,
    pdfium: &Path,
    limits: &Limits,
) -> Result<PdfOutcome, GrapeError> {
    if !supports(from, to) {
        return Err(crate::not_yet_supported(job, Some(from)));
    }
    let image = match to {
        Format::Jpg => Some(ImageFormat::Jpg),
        Format::Png => Some(ImageFormat::Png),
        _ => None,
    };
    let (op, shape) = match (image, dest) {
        (
            Some(format),
            JobOutput::PagesFileOrFolder {
                file,
                folder,
                page_stem,
            },
        ) => (
            WorkerOp::Render {
                source: src.to_path_buf(),
                format,
            },
            Shape::Pages {
                file: file.clone(),
                folder: folder.clone(),
                page_stem: page_stem.clone(),
                worker_ext: names::image_ext(format),
                ext: to.output_ext(),
            },
        ),
        (None, JobOutput::File(dest)) => {
            let format = if to == Format::Md {
                TextFormat::Md
            } else {
                TextFormat::Txt
            };
            (
                WorkerOp::Text {
                    source: src.to_path_buf(),
                    format,
                },
                Shape::File {
                    dest: dest.clone(),
                    produced: names::text(format),
                },
            )
        }
        _ => return Err(crate::not_yet_supported(job, Some(from))),
    };
    execute(job, ctx, progress, launch, pdfium, limits, op, shape, to)
}

/// [`tool`] with the worker, library and limits given.
pub(crate) fn tool_with(
    job: &Job,
    tool: ToolKey,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    launch: &Launch,
    pdfium: &Path,
    limits: &Limits,
) -> Result<PdfOutcome, GrapeError> {
    let all_pdf = !job.inputs.is_empty()
        && job
            .inputs
            .iter()
            .all(|path| format_of(path) == Some(Format::Pdf));
    if !all_pdf || !runs_tool(tool, Format::Pdf) {
        let from = job.inputs.first().and_then(|path| format_of(path));
        return Err(crate::not_yet_supported(job, from));
    }
    let (op, shape) = match (tool, job.inputs.as_slice(), &job.output) {
        (ToolKey::SplitPdf, [src], JobOutput::Folder(dest)) => (
            WorkerOp::Split {
                source: src.clone(),
            },
            Shape::Split { dest: dest.clone() },
        ),
        (ToolKey::RemoveMetadata, [src], JobOutput::File(dest)) => (
            WorkerOp::RemoveMetadata {
                source: src.clone(),
            },
            Shape::File {
                dest: dest.clone(),
                produced: names::CLEAN,
            },
        ),
        (ToolKey::MergePdf, inputs, JobOutput::File(dest)) => (
            WorkerOp::Merge {
                sources: inputs.to_vec(),
            },
            Shape::File {
                dest: dest.clone(),
                produced: names::MERGED,
            },
        ),
        _ => return Err(crate::not_yet_supported(job, Some(Format::Pdf))),
    };
    execute(
        job,
        ctx,
        progress,
        launch,
        pdfium,
        limits,
        op,
        shape,
        Format::Pdf,
    )
}

/// What the job publishes.
#[derive(Debug)]
enum Shape {
    /// One file the worker wrote as `produced`, published at `dest`.
    File {
        dest: PathBuf,
        produced: &'static str,
    },
    /// A render: one page at `file`; more in `folder`, named after
    /// `page_stem`.
    Pages {
        file: PathBuf,
        folder: PathBuf,
        page_stem: String,
        worker_ext: &'static str,
        ext: &'static str,
    },
    /// Split PDF: a folder at `dest`, `{stem} {n}.pdf` inside.
    Split { dest: PathBuf },
}

impl Shape {
    /// The planned path messages and the destination folder go by.
    fn planned(&self) -> &Path {
        match self {
            Shape::File { dest, .. } | Shape::Split { dest } => dest,
            Shape::Pages { file, .. } => file,
        }
    }
}

/// Sniffs the sources, runs the worker on `op` and publishes the result
/// as `shape`.
#[allow(clippy::too_many_arguments)]
fn execute(
    job: &Job,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    launch: &Launch,
    pdfium: &Path,
    limits: &Limits,
    op: WorkerOp,
    shape: Shape,
    to: Format,
) -> Result<PdfOutcome, GrapeError> {
    let first = job
        .inputs
        .first()
        .ok_or_else(|| crate::not_yet_supported(job, None))?;
    let planned = shape.planned().to_path_buf();
    let names_of = |src: &Path| Names::new(src, &planned, Format::Pdf, to);
    let names = names_of(first);
    let dir = publish::dest_dir(&planned, &names)?;
    if ctx.cancel.is_cancelled() {
        return Err(cancelled());
    }
    for (i, src) in job.inputs.iter().enumerate() {
        sniff(src)
            .map_err(|failure| report(failure.about(i), &names_of(&job.inputs[i]), limits))?;
    }
    publish::ensure_dir(job, ctx, dir, &names)?;
    // From here on, `temp` removes the temp folder on every return path.
    let temp = TempDir::create(dir)
        .map_err(|e| names.write_failed(format!("creating the temp folder: {:?}", e.kind())))?;
    let request = Request {
        pdfium: absolute(pdfium),
        out_dir: absolute(temp.path()),
        op: absolute_op(op),
        bounds: limits.bounds,
    };
    let mut last = 0.0f32;
    let ended = {
        let mut forward = |fraction: f32| {
            if fraction > last {
                last = fraction;
                progress(fraction);
            }
        };
        drive(launch, &request, &ctx.cancel, limits, &mut forward)
    };
    let done = match ended {
        Ended::Done(done) => done,
        Ended::Failed(failure) => {
            let about = failure
                .input
                .and_then(|i| job.inputs.get(i))
                .unwrap_or(first);
            return Err(report(failure, &names_of(about), limits));
        }
        Ended::Cancelled => return Err(cancelled()),
        Ended::TimedOut => {
            return Err(GrapeError::new(
                ErrorKind::DecodeFailed,
                format!("{} took too long, so Grape stopped.", names.source),
                "the PDF worker ran past its time",
            ))
        }
        Ended::OverHeap => {
            return Err(GrapeError::new(
                ErrorKind::DecodeFailed,
                format!("{} needs too much memory, so Grape stopped.", names.source),
                format!(
                    "the PDF worker went past its {} MiB heap bound",
                    limits.bounds.max_heap_bytes / MIB
                ),
            ))
        }
        Ended::Crashed(detail) => return Err(helper_stopped(detail)),
        Ended::Spawn(kind) => {
            return Err(GrapeError::new(
                ErrorKind::NotYetSupported,
                "Grape's PDF helper couldn't start.",
                format!("starting the PDF worker: {kind:?}"),
            ))
        }
    };
    if ctx.cancel.is_cancelled() {
        return Err(cancelled());
    }
    let outcome = match shape {
        Shape::File { dest, produced } => {
            publish_file(&temp, produced, job, first, &dest, dir, &names)?
        }
        Shape::Pages {
            file,
            folder,
            page_stem,
            worker_ext,
            ext,
        } => {
            if done.pages == 1 {
                let produced = names::page(1, worker_ext);
                publish_file(&temp, &produced, job, first, &file, dir, &names)?
            } else {
                let source_name = file_name(first);
                let folder_names = Names::new(first, &folder, Format::Pdf, to);
                publish_pages(
                    &temp,
                    done.pages,
                    worker_ext,
                    |n, count| grape_core::page_file_name(&page_stem, n, count, ext),
                    &folder,
                    dir,
                    &folder_names,
                    |taken| grape_core::pages_folder_name(&source_name, taken),
                )?
            }
        }
        Shape::Split { dest } => {
            let source_name = file_name(first);
            let stem = grape_core::naming::stem(&source_name).to_string();
            publish_pages(
                &temp,
                done.pages,
                "pdf",
                |n, count| grape_core::page_file_name(&stem, n, count, "pdf"),
                &dest,
                dir,
                &names,
                |taken| grape_core::output_name(&source_name, &job.op, taken),
            )?
        }
    };
    drop(temp);
    if last < 1.0 {
        progress(1.0);
    }
    Ok(PdfOutcome {
        outcome,
        pages: done.pages,
        scaled_pages: done.scaled_pages,
        removed: done.removed,
        used_pdfium: done.used_pdfium,
    })
}

/// `path` made absolute against the current folder (the worker refuses
/// relative paths). Unchanged if that fails.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn absolute_op(op: WorkerOp) -> WorkerOp {
    match op {
        WorkerOp::Render { source, format } => WorkerOp::Render {
            source: absolute(&source),
            format,
        },
        WorkerOp::Text { source, format } => WorkerOp::Text {
            source: absolute(&source),
            format,
        },
        WorkerOp::Split { source } => WorkerOp::Split {
            source: absolute(&source),
        },
        WorkerOp::Merge { sources } => WorkerOp::Merge {
            sources: sources.iter().map(|s| absolute(s)).collect(),
        },
        WorkerOp::RemoveMetadata { source } => WorkerOp::RemoveMetadata {
            source: absolute(&source),
        },
    }
}

/// Checks for `%PDF-` in the first 1024 bytes of `src`.
fn sniff(src: &Path) -> Result<(), Failure> {
    let mut head = Vec::new();
    File::open(src)
        .and_then(|file| file.take(SNIFF_BYTES).read_to_end(&mut head))
        .map_err(|e| {
            Failure::new(
                FailKind::Read,
                format!("opening the source: {:?}", e.kind()),
            )
        })?;
    if is_pdf(&head) {
        Ok(())
    } else {
        Err(Failure::new(
            FailKind::NotPdf,
            "no %PDF- in the first 1024 bytes",
        ))
    }
}

/// The error for a cancelled job (its detail names no file).
fn cancelled() -> GrapeError {
    GrapeError::new(
        ErrorKind::Cancelled,
        "Cancelled. Nothing was saved.",
        "cancelled",
    )
}

/// The error for a worker that ended without a result.
fn helper_stopped(detail: String) -> GrapeError {
    GrapeError::new(
        ErrorKind::DecodeFailed,
        "Grape's PDF helper stopped unexpectedly.",
        format!("PDF worker: {detail}"),
    )
}

/// The worker's failure in Grape's words, naming the file it was about.
fn report(failure: Failure, names: &Names, limits: &Limits) -> GrapeError {
    let source = &names.source;
    let detail = format!("PDF worker: {:?}: {}", failure.kind, failure.detail);
    let bad_input = |message: String| GrapeError::new(ErrorKind::DecodeFailed, message, &detail);
    match failure.kind {
        FailKind::NotPdf | FailKind::Damaged => {
            bad_input(format!("{source} is damaged or isn't a PDF."))
        }
        FailKind::Encrypted => bad_input(format!("{source} is password-protected.")),
        FailKind::NoPages => bad_input(format!("{source} has no pages.")),
        FailKind::NoText => bad_input(format!(
            "{source} has no text to extract — it may be a scan."
        )),
        FailKind::TooManyPages => bad_input(format!(
            "{source} has more than {} pages, more than Grape exports as images.",
            thousands(u64::from(limits.bounds.max_pages))
        )),
        FailKind::TooManyPagesToSplit => bad_input(format!(
            "{source} has more than {} pages, more than Grape splits.",
            thousands(u64::from(limits.bounds.max_split_pages))
        )),
        FailKind::TooMuchOutput => bad_input(format!(
            "{source} would make more than {} GiB of output.",
            limits.bounds.max_output_bytes / GIB
        )),
        FailKind::TooMuchText => bad_input(format!(
            "{source} has more than {} MiB of text, more than Grape extracts.",
            limits.bounds.max_text_bytes / MIB
        )),
        FailKind::TooLarge => bad_input(format!(
            "{source} is too large for Grape's PDF library to read."
        )),
        FailKind::Read => names.read_failed(detail),
        FailKind::Write => names.write_failed(detail),
        FailKind::Library => GrapeError::new(
            ErrorKind::NotYetSupported,
            "Grape's PDF library is missing or couldn't be loaded.",
            detail,
        ),
        FailKind::Internal => helper_stopped(format!("{:?}: {}", failure.kind, failure.detail)),
    }
}

/// `n` with thousands separators: `2,000`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Publishes the worker's one output `produced` at `dest` without
/// replacing anything: it is moved onto a fresh temp file in `dir`, which
/// [`publish::finalize`] links to the planned name (re-planned if taken).
fn publish_file(
    temp: &TempDir,
    produced: &str,
    job: &Job,
    src: &Path,
    dest: &Path,
    dir: &Path,
    names: &Names,
) -> Result<JobOutcome, GrapeError> {
    let produced = temp.path().join(produced);
    if !produced.is_file() {
        return Err(helper_stopped("its output is missing".to_string()));
    }
    let dest_name = file_name(dest);
    let ext = grape_core::split_name(&dest_name).1.unwrap_or("pdf");
    let (file, handle) = TempFile::create(dir, ext)
        .map_err(|e| names.write_failed(format!("creating the temp file: {:?}", e.kind())))?;
    drop(handle);
    fs::rename(&produced, file.path())
        .map_err(|e| names.write_failed(format!("moving the output: {:?}", e.kind())))?;
    publish::finalize(&file, job, src, dest, dir, names)
}

/// Publishes `count` pages the worker wrote as `page-{n}.{worker_ext}` as
/// a new folder at `dest`, each renamed to `page_name(n, count)`.
#[allow(clippy::too_many_arguments)]
fn publish_pages(
    temp: &TempDir,
    count: u32,
    worker_ext: &str,
    page_name: impl Fn(usize, usize) -> String,
    dest: &Path,
    dir: &Path,
    names: &Names,
    replan: impl FnMut(&HashSet<String>) -> String,
) -> Result<JobOutcome, GrapeError> {
    if count == 0 {
        return Err(helper_stopped("it reported no pages".to_string()));
    }
    let staged = temp.path().join("pages");
    fs::create_dir(&staged)
        .map_err(|e| names.write_failed(format!("staging the pages: {:?}", e.kind())))?;
    let total = count as usize;
    for n in 1..=count {
        let from = temp.path().join(names::page(n, worker_ext));
        let to = staged.join(page_name(n as usize, total));
        fs::rename(&from, &to).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                helper_stopped("a page is missing".to_string())
            } else {
                names.write_failed(format!("staging a page: {:?}", e.kind()))
            }
        })?;
    }
    folder::publish_folder(&staged, dest, dir, names, replan).map_err(|failure| match failure {
        PublishFailure::Taken(error) => error,
        PublishFailure::Write(detail) => names.write_failed(detail),
    })
}

/// How to start the worker.
#[derive(Debug, Clone)]
pub(crate) struct Launch {
    /// The executable.
    pub(crate) program: PathBuf,
    /// Its arguments.
    pub(crate) args: Vec<OsString>,
    /// Extra environment variables (tests).
    pub(crate) envs: Vec<(OsString, OsString)>,
}

impl Launch {
    /// `<worker> --grape-pdf-worker`.
    pub(crate) fn worker(paths: &PdfPaths) -> Launch {
        Launch {
            program: paths.worker.clone(),
            args: vec![OsString::from(WORKER_FLAG)],
            envs: Vec::new(),
        }
    }
}

/// How a run of the worker ended.
#[derive(Debug)]
enum Ended {
    /// `done` and exit code 0.
    Done(Done),
    /// An `error` line and a failing exit code.
    Failed(Failure),
    /// The job was cancelled; the worker was killed.
    Cancelled,
    /// The deadline passed; the worker was killed.
    TimedOut,
    /// The worker exited with [`OVER_HEAP_EXIT`]: past its heap bound.
    OverHeap,
    /// It ended without a consistent result (a crash, an abort, a stack
    /// overflow, a lie).
    Crashed(String),
    /// It couldn't be started.
    Spawn(io::ErrorKind),
}

/// The worker process, killed (if still running) and reaped when dropped.
struct Worker(Child);

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What the stdout thread passes on.
enum Event {
    Message(Message),
    End,
}

/// Runs the worker on `request` until it ends, is cancelled or runs out of
/// time, passing progress to `progress`.
fn drive(
    launch: &Launch,
    request: &Request,
    cancel: &CancelToken,
    limits: &Limits,
    progress: &mut dyn FnMut(f32),
) -> Ended {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .envs(launch.envs.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut worker = match command.spawn() {
        Ok(child) => Worker(child),
        Err(e) => return Ended::Spawn(e.kind()),
    };
    let Some(stdout) = worker.0.stdout.take() else {
        return Ended::Crashed("no stdout".to_string());
    };
    let (tx, rx) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("grape-pdf-stdout".to_string())
        .spawn(move || read_messages(stdout, tx));
    if reader.is_err() {
        return Ended::Crashed("no reader thread".to_string());
    }
    if let (Some(mut stdin), Ok(mut line)) = (worker.0.stdin.take(), serde_json::to_vec(request)) {
        line.push(b'\n');
        // On a thread, so a worker that doesn't read can't block the job
        // (cancel and the deadline still apply). A worker that died at once
        // shows up as a crash below; a killed one ends the write. Dropping
        // stdin closes it: the worker reads to the end.
        let writer = thread::Builder::new()
            .name("grape-pdf-stdin".to_string())
            .spawn(move || {
                let _ = stdin.write_all(&line);
            });
        if writer.is_err() {
            return Ended::Crashed("no writer thread".to_string());
        }
    }
    watch(&mut worker, &rx, cancel, limits, progress)
}

/// The job's side of [`drive`]: polls the messages, the cancel token and
/// the deadline.
fn watch(
    worker: &mut Worker,
    rx: &Receiver<Event>,
    cancel: &CancelToken,
    limits: &Limits,
    progress: &mut dyn FnMut(f32),
) -> Ended {
    let start = Instant::now();
    let mut deadline = start.checked_add(limits.timeout_base);
    let mut done = None;
    let mut failure = None;
    let mut stream_open = true;
    let status: ExitStatus = loop {
        if cancel.is_cancelled() {
            return Ended::Cancelled;
        }
        let now = Instant::now();
        let left = match deadline {
            Some(deadline) if now >= deadline => return Ended::TimedOut,
            Some(deadline) => deadline - now,
            None => POLL,
        };
        let wait = left.min(POLL);
        if !stream_open {
            match worker.0.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(wait.min(Duration::from_millis(10))),
                Err(e) => return Ended::Crashed(format!("waiting: {:?}", e.kind())),
            }
            continue;
        }
        match rx.recv_timeout(wait) {
            Ok(Event::Message(Message::Pages(pages))) => {
                // The worker's count isn't trusted: a file's /Count can
                // claim a million pages it doesn't have.
                let pages = pages.min(limits.bounds.max_pages);
                let more = limits.timeout_per_page.saturating_mul(pages);
                let extended = limits
                    .timeout_base
                    .checked_add(more)
                    .and_then(|total| start.checked_add(total));
                if let (Some(current), Some(extended)) = (deadline, extended) {
                    deadline = Some(current.max(extended));
                }
            }
            Ok(Event::Message(Message::Progress(fraction))) => {
                if fraction.is_finite() {
                    progress(fraction.clamp(0.0, 1.0));
                }
            }
            Ok(Event::Message(Message::Done(result))) => done = Some(result),
            Ok(Event::Message(Message::Error(result))) => failure = Some(result),
            Ok(Event::End) | Err(RecvTimeoutError::Disconnected) => stream_open = false,
            Err(RecvTimeoutError::Timeout) => {}
        }
    };
    if status.code() == Some(OVER_HEAP_EXIT) {
        return Ended::OverHeap;
    }
    match (done, failure, status.success()) {
        (Some(done), None, true) => Ended::Done(done),
        (None, Some(failure), false) => Ended::Failed(failure),
        (done, failure, success) => Ended::Crashed(format!(
            "exit {:?} (success {success}) with done {} and error {}",
            status.code(),
            done.is_some(),
            failure.is_some()
        )),
    }
}

/// The stdout thread: one [`Event::Message`] per line that is a message
/// (other lines, and lines over [`MAX_LINE_BYTES`], are skipped), then
/// [`Event::End`].
fn read_messages(stdout: ChildStdout, tx: Sender<Event>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    while let Ok(Some(())) = read_line(&mut reader, &mut line) {
        if let Ok(message) = serde_json::from_slice::<Message>(&line) {
            if tx.send(Event::Message(message)).is_err() {
                return;
            }
        }
    }
    let _ = tx.send(Event::End);
}

/// Reads the next line into `line` (without its `\n`), keeping at most
/// [`MAX_LINE_BYTES`]: a longer line comes back empty. `None` at the end.
fn read_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<()>> {
    line.clear();
    let mut seen_any = false;
    let mut overlong = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(seen_any.then_some(()));
        }
        seen_any = true;
        let (take, end) = match buf.iter().position(|&b| b == b'\n') {
            Some(at) => (at + 1, true),
            None => (buf.len(), false),
        };
        let content = if end { &buf[..take - 1] } else { &buf[..take] };
        if !overlong {
            if line.len() + content.len() > MAX_LINE_BYTES {
                overlong = true;
                line.clear();
            } else {
                line.extend_from_slice(content);
            }
        }
        reader.consume(take);
        if end {
            return Ok(Some(()));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    use grape_core::{plan, Settings};
    use lopdf::Document;

    use super::super::copy::tests::{contains, fixture, save_classic, save_modern};
    use super::*;
    use crate::output::unique_suffix;

    /// The environment variable that turns the ignored test below into a
    /// fake worker when the test binary runs itself.
    const FAKE: &str = "GRAPE_PDF_FAKE_WORKER";

    /// Not a test: the worker the tests start. The test binary runs only
    /// this "test" (`--exact … --ignored`) with [`FAKE`] set to a mode.
    #[test]
    #[ignore = "a fake PDF worker, run by the tests below"]
    fn fake_worker() {
        let Ok(mode) = std::env::var(FAKE) else {
            return;
        };
        if mode == "real" {
            std::process::exit(if super::super::worker::serve() { 0 } else { 1 });
        }
        let mut input = String::new();
        io::stdin().read_to_string(&mut input).unwrap();
        let request: Request = serde_json::from_str(&input).unwrap();
        let mut out = io::stdout();
        let mut say = |line: &str| {
            out.write_all(line.as_bytes()).unwrap();
            out.write_all(b"\n").unwrap();
            out.flush().unwrap();
        };
        match mode.as_str() {
            "sleep" => {
                say(r#"{"pages":1}"#);
                say(r#"{"progress":0.25}"#);
                thread::sleep(Duration::from_secs(60));
            }
            "pages-then-sleep" => {
                say(r#"{"pages":2}"#);
                thread::sleep(Duration::from_secs(60));
            }
            "huge-count-then-sleep" => {
                say(r#"{"pages":4294967295}"#);
                thread::sleep(Duration::from_secs(60));
            }
            "over-heap" => {
                say(r#"{"progress":0.1}"#);
                std::process::exit(OVER_HEAP_EXIT);
            }
            "crash" => {
                say(r#"{"progress":0.5}"#);
                std::process::exit(3);
            }
            "lie" => {
                fs::write(request.out_dir.join("clean.pdf"), b"%PDF-1.7").unwrap();
                say(r#"{"done":{"pages":1}}"#);
                std::process::exit(1);
            }
            "error" => {
                say(r#"{"error":{"kind":"encrypted","detail":"fake","input":1}}"#);
                std::process::exit(1);
            }
            "noisy" => {
                say("not a message");
                say(&"x".repeat(MAX_LINE_BYTES + 10));
                say(r#"{"progress":0.6}"#);
                say(r#"{"progress":0.3}"#);
                say(r#"{"progress":"NaN"}"#);
                fs::write(request.out_dir.join("clean.pdf"), b"%PDF-1.7 fake").unwrap();
                fs::write(request.out_dir.join("stray.txt"), b"left behind").unwrap();
                say(r#"{"done":{"pages":1,"removed":["Title"]}}"#);
                std::process::exit(0);
            }
            other => panic!("unknown fake mode {other}"),
        }
    }

    /// A launch of this test binary as a fake worker in `mode`.
    fn fake(mode: &str) -> Launch {
        Launch {
            program: std::env::current_exe().unwrap(),
            args: [
                "--exact",
                "pdf::run::tests::fake_worker",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ]
            .iter()
            .map(OsString::from)
            .collect(),
            envs: vec![(OsString::from(FAKE), OsString::from(mode))],
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            let path = std::env::temp_dir().join(format!("grape-pdf-unit-{}", unique_suffix()));
            fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn assert_no_leftovers(dir: &Path) {
        let left: Vec<String> = listing(dir)
            .into_iter()
            .filter(|name| name.starts_with(".grape-"))
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    fn digest(path: &Path) -> (usize, u64) {
        let bytes = fs::read(path).unwrap();
        let mut hasher = DefaultHasher::new();
        hasher.write(&bytes);
        (bytes.len(), hasher.finish())
    }

    /// Sources in one folder, planned the way the app plans them.
    struct Case {
        dir: TestDir,
        sources: Vec<PathBuf>,
        digests: Vec<(usize, u64)>,
        job: Job,
    }

    fn case(files: &[(&str, Vec<u8>)], op: Operation) -> Case {
        let dir = TestDir::new();
        let sources: Vec<PathBuf> = files
            .iter()
            .map(|(name, bytes)| {
                let path = dir.0.join(name);
                fs::write(&path, bytes).unwrap();
                path
            })
            .collect();
        let settings = Settings::new(dir.0.join("fallback"));
        let selection = crate::build_selection(&sources, &settings);
        let mut jobs = plan(&selection, &op, &settings).unwrap().jobs;
        assert_eq!(jobs.len(), 1);
        Case {
            digests: sources.iter().map(|p| digest(p)).collect(),
            sources,
            job: jobs.remove(0),
            dir,
        }
    }

    impl Case {
        fn run(
            &self,
            launch: &Launch,
            limits: &Limits,
        ) -> (Result<PdfOutcome, GrapeError>, Vec<f32>) {
            self.run_cancelling(launch, limits, None)
        }

        /// Runs the job; cancels it once progress reaches `cancel_at`.
        fn run_cancelling(
            &self,
            launch: &Launch,
            limits: &Limits,
            cancel_at: Option<f32>,
        ) -> (Result<PdfOutcome, GrapeError>, Vec<f32>) {
            let ctx = EngineCtx::default();
            let token = ctx.cancel.clone();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&seen);
            let mut progress = move |fraction: f32| {
                log.lock().unwrap().push(fraction);
                if cancel_at.is_some_and(|at| fraction >= at) {
                    token.cancel();
                }
            };
            let pdfium = self.dir.0.join("no-pdfium");
            let result = run_with(&self.job, &ctx, &mut progress, launch, &pdfium, limits);
            let seen = seen.lock().unwrap().clone();
            (result, seen)
        }

        fn assert_sources_unchanged(&self) {
            for (source, before) in self.sources.iter().zip(&self.digests) {
                assert_eq!(&digest(source), before, "a source changed");
            }
        }
    }

    fn assert_monotone(progress: &[f32]) {
        assert!(progress.windows(2).all(|w| w[0] <= w[1]), "{progress:?}");
        assert!(
            progress.iter().all(|p| (0.0..=1.0).contains(p)),
            "{progress:?}"
        );
    }

    #[test]
    fn split_publishes_a_folder_of_pages() {
        let case = case(
            &[("doc.pdf", save_modern(&fixture("A")))],
            Operation::tool(ToolKey::SplitPdf),
        );
        let (result, progress) = case.run(&fake("real"), &Limits::default());
        let outcome = result.unwrap();
        let folder = case.dir.0.join("doc Pages");
        assert_eq!(outcome.outcome.outputs, std::slice::from_ref(&folder));
        assert!(!outcome.outcome.renamed);
        assert_eq!(outcome.pages, 3);
        assert_eq!(listing(&folder), ["doc 1.pdf", "doc 2.pdf", "doc 3.pdf"]);
        let page = Document::load(folder.join("doc 2.pdf")).unwrap();
        assert!(page.extract_text(&[1]).unwrap().contains("A page 2"));
        assert_eq!(progress.last(), Some(&1.0));
        assert_monotone(&progress);
        assert_no_leftovers(&case.dir.0);
        case.assert_sources_unchanged();
    }

    #[test]
    fn a_taken_folder_name_is_replanned() {
        let case = case(
            &[("doc.pdf", save_classic(&fixture("A")))],
            Operation::tool(ToolKey::SplitPdf),
        );
        // Someone made the planned folder after planning.
        fs::create_dir(case.dir.0.join("doc Pages")).unwrap();
        let (result, _) = case.run(&fake("real"), &Limits::default());
        let outcome = result.unwrap();
        assert_eq!(outcome.outcome.outputs, [case.dir.0.join("doc Pages 2")]);
        assert!(outcome.outcome.renamed);
        assert!(listing(&case.dir.0.join("doc Pages")).is_empty());
        assert_no_leftovers(&case.dir.0);
    }

    #[test]
    fn merge_publishes_one_file_named_after_the_first() {
        let case = case(
            &[
                ("b.pdf", save_classic(&fixture("B"))),
                ("a.pdf", save_modern(&fixture("A"))),
            ],
            Operation::tool(ToolKey::MergePdf),
        );
        let (result, progress) = case.run(&fake("real"), &Limits::default());
        let outcome = result.unwrap();
        let merged = case.dir.0.join("b Merged.pdf");
        assert_eq!(outcome.outcome.outputs, std::slice::from_ref(&merged));
        assert_eq!(outcome.pages, 6);
        let doc = Document::load(&merged).unwrap();
        assert!(doc.extract_text(&[1]).unwrap().contains("B page 1"));
        assert!(doc.extract_text(&[6]).unwrap().contains("A page 3"));
        assert_monotone(&progress);
        assert_no_leftovers(&case.dir.0);
        case.assert_sources_unchanged();
    }

    #[test]
    fn remove_metadata_reports_and_never_replaces() {
        let case = case(
            &[("doc.pdf", save_classic(&fixture("A")))],
            Operation::tool(ToolKey::RemoveMetadata),
        );
        // The planned name appears after planning: the output gets " 2".
        let planned = case.dir.0.join("doc No Metadata.pdf");
        assert_eq!(case.job.output, JobOutput::File(planned.clone()));
        fs::write(&planned, b"someone else's").unwrap();
        let (result, _) = case.run(&fake("real"), &Limits::default());
        let outcome = result.unwrap();
        let output = case.dir.0.join("doc No Metadata 2.pdf");
        assert_eq!(outcome.outcome.outputs, std::slice::from_ref(&output));
        assert!(outcome.outcome.renamed);
        assert_eq!(fs::read(&planned).unwrap(), b"someone else's");
        assert_eq!(
            outcome.removed,
            [
                MetaKey::Title,
                MetaKey::Author,
                MetaKey::Producer,
                MetaKey::Other,
                MetaKey::Xmp
            ]
        );
        assert!(!contains(&fs::read(&output).unwrap(), "MARK-AUTHOR"));
        assert_no_leftovers(&case.dir.0);
        case.assert_sources_unchanged();
    }

    #[test]
    fn worker_failures_name_the_file_they_are_about() {
        let case = case(
            &[
                ("a.pdf", save_classic(&fixture("A"))),
                ("b.pdf", save_classic(&fixture("B"))),
            ],
            Operation::tool(ToolKey::MergePdf),
        );
        let (result, _) = case.run(&fake("error"), &Limits::default());
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::DecodeFailed);
        assert_eq!(error.user_message, "b.pdf is password-protected.");
        assert!(!error.detail.contains(&case.dir.0.display().to_string()));
        assert_no_leftovers(&case.dir.0);
    }

    #[test]
    fn a_crash_or_a_lie_is_reported_and_nothing_is_kept() {
        for mode in ["crash", "lie"] {
            let case = case(
                &[("doc.pdf", save_classic(&fixture("A")))],
                Operation::tool(ToolKey::RemoveMetadata),
            );
            let (result, _) = case.run(&fake(mode), &Limits::default());
            let error = result.unwrap_err();
            assert_eq!(
                error.user_message, "Grape's PDF helper stopped unexpectedly.",
                "{mode}"
            );
            assert_eq!(listing(&case.dir.0), ["doc.pdf"], "{mode}");
        }
    }

    #[test]
    fn noise_on_stdout_is_skipped_and_stray_files_are_not_published() {
        let case = case(
            &[("doc.pdf", save_classic(&fixture("A")))],
            Operation::tool(ToolKey::RemoveMetadata),
        );
        let (result, progress) = case.run(&fake("noisy"), &Limits::default());
        let outcome = result.unwrap();
        assert_eq!(outcome.removed, [MetaKey::Title]);
        assert_eq!(progress, [0.6, 1.0]);
        assert_eq!(listing(&case.dir.0), ["doc No Metadata.pdf", "doc.pdf"]);
    }

    #[test]
    fn cancel_kills_the_worker_quickly() {
        let case = case(
            &[("doc.pdf", save_classic(&fixture("A")))],
            Operation::tool(ToolKey::SplitPdf),
        );
        let started = Instant::now();
        let (result, _) = case.run_cancelling(&fake("sleep"), &Limits::default(), Some(0.2));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
        assert_eq!(listing(&case.dir.0), ["doc.pdf"]);
        case.assert_sources_unchanged();
    }

    #[test]
    fn a_worker_past_its_time_is_stopped() {
        let case = case(
            &[("doc.pdf", save_classic(&fixture("A")))],
            Operation::tool(ToolKey::SplitPdf),
        );
        let limits = Limits {
            timeout_base: Duration::from_secs(2),
            timeout_per_page: Duration::from_secs(1),
            ..Limits::default()
        };
        // Two pages reported: 2 s + 2 × 1 s.
        let started = Instant::now();
        let (result, _) = case.run(&fake("pages-then-sleep"), &limits);
        let took = started.elapsed();
        let error = result.unwrap_err();
        assert_eq!(
            error.user_message,
            "doc.pdf took too long, so Grape stopped."
        );
        assert!(took >= Duration::from_secs(4), "{took:?}");
        assert!(took < Duration::from_secs(15), "{took:?}");
        assert_eq!(listing(&case.dir.0), ["doc.pdf"]);
    }

    #[test]
    fn a_page_count_the_worker_makes_up_doesnt_stretch_the_time() {
        let case = case(
            &[("doc.pdf", save_classic(&fixture("A")))],
            Operation::convert(Format::Txt),
        );
        // u32::MAX pages reported, but at most 2 count: 1 s + 2 × 1 s.
        let mut limits = Limits {
            timeout_base: Duration::from_secs(1),
            timeout_per_page: Duration::from_secs(1),
            ..Limits::default()
        };
        limits.bounds.max_pages = 2;
        let started = Instant::now();
        let (result, _) = case.run(&fake("huge-count-then-sleep"), &limits);
        let took = started.elapsed();
        assert_eq!(
            result.unwrap_err().user_message,
            "doc.pdf took too long, so Grape stopped."
        );
        assert!(took >= Duration::from_secs(3), "{took:?}");
        assert!(took < Duration::from_secs(15), "{took:?}");
        assert_eq!(listing(&case.dir.0), ["doc.pdf"]);
        case.assert_sources_unchanged();
    }

    #[test]
    fn a_worker_past_its_heap_bound_is_reported_as_such() {
        let case = case(
            &[("doc.pdf", save_classic(&fixture("A")))],
            Operation::tool(ToolKey::SplitPdf),
        );
        let (result, _) = case.run(&fake("over-heap"), &Limits::default());
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::DecodeFailed);
        assert_eq!(
            error.user_message,
            "doc.pdf needs too much memory, so Grape stopped."
        );
        assert_eq!(listing(&case.dir.0), ["doc.pdf"]);
        case.assert_sources_unchanged();
    }

    #[test]
    fn a_missing_worker_or_a_non_pdf_fails_before_anything_is_made() {
        let pdf = case(
            &[("doc.pdf", save_classic(&fixture("A")))],
            Operation::tool(ToolKey::SplitPdf),
        );
        let missing = Launch {
            program: pdf.dir.0.join("no-such-worker"),
            args: Vec::new(),
            envs: Vec::new(),
        };
        let (result, _) = pdf.run(&missing, &Limits::default());
        assert_eq!(
            result.unwrap_err().user_message,
            "Grape's PDF helper couldn't start."
        );
        assert_eq!(listing(&pdf.dir.0), ["doc.pdf"]);

        let text = case(
            &[("doc.pdf", b"not a pdf".to_vec())],
            Operation::tool(ToolKey::SplitPdf),
        );
        let (result, _) = text.run(&fake("real"), &Limits::default());
        assert_eq!(
            result.unwrap_err().user_message,
            "doc.pdf is damaged or isn't a PDF."
        );
        assert_eq!(listing(&text.dir.0), ["doc.pdf"]);
    }

    #[test]
    fn this_build_takes_only_its_pairs_and_tools() {
        assert!(supports(Format::Pdf, Format::Png));
        assert!(supports(Format::Pdf, Format::Md));
        assert!(!supports(Format::Pdf, Format::Docx));
        assert!(!supports(Format::Png, Format::Pdf));
        assert!(runs_tool(ToolKey::SplitPdf, Format::Pdf));
        assert!(!runs_tool(ToolKey::Compress, Format::Pdf));
        assert!(!runs_tool(ToolKey::SplitPdf, Format::Png));

        // Compress on PDF is v2, so the planner won't make this job.
        let mut compress = case(
            &[("doc.pdf", save_classic(&fixture("A")))],
            Operation::tool(ToolKey::RemoveMetadata),
        );
        compress.job.op = Operation::tool(ToolKey::Compress);
        let (result, _) = compress.run(&fake("real"), &Limits::default());
        assert_eq!(result.unwrap_err().kind, ErrorKind::NotYetSupported);
        assert_eq!(listing(&compress.dir.0), ["doc.pdf"]);
    }

    #[test]
    fn lines_are_bounded() {
        let mut long = vec![b'a'; MAX_LINE_BYTES + 1];
        long.extend_from_slice(b"\n{\"pages\":1}\nlast");
        let mut reader = BufReader::with_capacity(16, Cursor::new(long));
        let mut line = Vec::new();
        assert_eq!(read_line(&mut reader, &mut line).unwrap(), Some(()));
        assert!(line.is_empty());
        assert_eq!(read_line(&mut reader, &mut line).unwrap(), Some(()));
        assert_eq!(line, b"{\"pages\":1}");
        assert_eq!(read_line(&mut reader, &mut line).unwrap(), Some(()));
        assert_eq!(line, b"last");
        assert_eq!(read_line(&mut reader, &mut line).unwrap(), None);
    }

    #[test]
    fn limits_and_messages() {
        let limits = Limits::default();
        assert_eq!(limits.timeout_base, Duration::from_secs(600));
        assert_eq!(limits.bounds.max_page_pixels, 100_000_000);
        assert_eq!(limits.bounds.max_output_bytes, 8 << 30);
        assert_eq!(limits.bounds.max_split_pages, 100_000);
        assert_eq!(limits.bounds.max_heap_bytes, 4 << 30);
        let names = Names::new(
            Path::new("in/a.pdf"),
            Path::new("out/a.png"),
            Format::Pdf,
            Format::Png,
        );
        let message = |kind| report(Failure::new(kind, "x"), &names, &limits).user_message;
        assert_eq!(
            message(FailKind::NoText),
            "a.pdf has no text to extract — it may be a scan."
        );
        assert_eq!(
            message(FailKind::TooManyPages),
            "a.pdf has more than 2,000 pages, more than Grape exports as images."
        );
        assert_eq!(
            message(FailKind::TooManyPagesToSplit),
            "a.pdf has more than 100,000 pages, more than Grape splits."
        );
        assert_eq!(
            message(FailKind::TooMuchOutput),
            "a.pdf would make more than 8 GiB of output."
        );
        assert_eq!(
            message(FailKind::TooMuchText),
            "a.pdf has more than 256 MiB of text, more than Grape extracts."
        );
        assert_eq!(
            message(FailKind::TooLarge),
            "a.pdf is too large for Grape's PDF library to read."
        );
        assert_eq!(message(FailKind::Read), "Grape couldn't read a.pdf.");
        assert_eq!(message(FailKind::Write), "Grape couldn't save into out.");
    }
}
