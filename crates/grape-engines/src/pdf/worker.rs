//! The PDF worker: one job per process, started as
//! `<exe> --grape-pdf-worker` by the parent ([`super::run`]).
//!
//! It reads one [`Request`] from stdin, arms its heap bound ([`heap`]),
//! does the job on a thread with a large stack (lopdf's parser and PDFium
//! recurse), prints [`Message`]s on stdout and exits 0 after `done` or 1
//! after `error` (or [`heap::OVER_HEAP_EXIT`] past the heap bound). It opens sources
//! read-only and writes only new files into the request's temp folder. It
//! never prints a path, a name or file contents, and it never logs. A panic
//! is caught and reported as an error; a crash, an abort or a stack
//! overflow just ends the process, and the parent reports that.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::raw::c_ulong;
use std::path::Path;
use std::process::ExitCode;
use std::thread;

use lopdf::{Document, LoadOptions};
use pdfium_render::prelude::{
    PdfDocument, PdfSecurityHandlerRevision, Pdfium, PdfiumError, PdfiumInternalError,
};

use super::copy::{Output, Tree};
use super::heap;
use super::meta;
use super::protocol::{
    names, Bounds, Done, FailKind, Failure, ImageFormat, Message, Request, TextFormat, WorkerOp,
    MAX_REQUEST_BYTES,
};
use super::render::{self, Budget};
use super::text::{page_glyphs, Text};

/// The job thread's stack: lopdf's parser nests up to 100 levels, and
/// PDFium's parser recurses too. The main thread's is 1 MiB on Windows.
const STACK_BYTES: usize = 64 * 1024 * 1024;

/// How far the `%PDF-` signature may be from the start (FORMAT_MATRIX
/// rule 3).
const SNIFF_BYTES: usize = 1024;

/// The worker's entry point: `Grape --grape-pdf-worker` and the
/// `grape-pdf-worker` test binary both call this and exit with its code.
/// It reads its request from stdin and answers on stdout (DECISIONS D29).
pub fn worker_main() -> ExitCode {
    if serve() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// [`worker_main`]'s work: true when the job is done and said so.
pub(crate) fn serve() -> bool {
    // A panic message could name what the job was working on; the error
    // line says enough.
    std::panic::set_hook(Box::new(|_| {}));
    let request = match read_request(io::stdin().lock()) {
        Ok(request) => request,
        Err(failure) => return finish(Err(failure)),
    };
    // stdout is set up (and its buffer allocated) before the heap bound is
    // armed, so an exit at the bound never finds it half made.
    let _ = io::stdout();
    heap::arm(request.bounds.max_heap_bytes);
    let job = thread::Builder::new()
        .name("grape-pdf-job".to_string())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let mut emit = Emitter::new(io::stdout());
            execute(&request, &mut emit)
        });
    let result = match job {
        Ok(handle) => handle
            .join()
            .unwrap_or_else(|_| Err(Failure::new(FailKind::Internal, "the job panicked"))),
        Err(e) => Err(Failure::new(
            FailKind::Internal,
            format!("starting the job thread: {:?}", e.kind()),
        )),
    };
    finish(result)
}

/// Prints the last line; true when it is `done`.
fn finish(result: Result<Done, Failure>) -> bool {
    let mut emit = Emitter::new(io::stdout());
    match result {
        Ok(done) => emit.send(&Message::Done(done)).is_ok(),
        Err(failure) => {
            let _ = emit.send(&Message::Error(failure));
            false
        }
    }
}

/// Reads and checks the request: every path absolute, the temp folder a
/// folder.
fn read_request(input: impl Read) -> Result<Request, Failure> {
    let mut bytes = Vec::new();
    input
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            Failure::new(
                FailKind::Internal,
                format!("reading the request: {:?}", e.kind()),
            )
        })?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(Failure::new(FailKind::Internal, "the request is too long"));
    }
    // serde_json's messages can quote the input: report the position only.
    let request: Request = serde_json::from_slice(&bytes).map_err(|e| {
        Failure::new(
            FailKind::Internal,
            format!(
                "bad request ({:?} at {}:{})",
                e.classify(),
                e.line(),
                e.column()
            ),
        )
    })?;
    let sources: Vec<&Path> = match &request.op {
        WorkerOp::Render { source, .. }
        | WorkerOp::Text { source, .. }
        | WorkerOp::Split { source }
        | WorkerOp::RemoveMetadata { source } => vec![source],
        WorkerOp::Merge { sources } => sources.iter().map(|p| p.as_path()).collect(),
    };
    if sources.is_empty() {
        return Err(Failure::new(
            FailKind::Internal,
            "the request has no source",
        ));
    }
    let all_absolute = sources
        .iter()
        .chain([&request.pdfium.as_path(), &request.out_dir.as_path()])
        .all(|path| path.is_absolute());
    if !all_absolute {
        return Err(Failure::new(
            FailKind::Internal,
            "a path in the request isn't absolute",
        ));
    }
    if !request.out_dir.is_dir() {
        return Err(Failure::new(FailKind::Write, "the temp folder is missing"));
    }
    Ok(request)
}

/// Prints messages as JSON lines, flushing each.
struct Emitter<W: Write> {
    out: W,
    progress: f32,
}

impl<W: Write> Emitter<W> {
    fn new(out: W) -> Emitter<W> {
        Emitter { out, progress: 0.0 }
    }

    fn send(&mut self, message: &Message) -> io::Result<()> {
        let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
        line.push(b'\n');
        self.out.write_all(&line)?;
        self.out.flush()
    }

    /// Reports the page count. The parent may be gone: then the next
    /// write fails too, and the job ends when the parent kills it.
    fn pages(&mut self, pages: usize) {
        let pages = u32::try_from(pages).unwrap_or(u32::MAX);
        let _ = self.send(&Message::Pages(pages));
    }

    /// Reports progress when it moved by at least 1% (or reached 1).
    fn progress(&mut self, fraction: f32) {
        let fraction = fraction.clamp(0.0, 1.0);
        if fraction >= self.progress + 0.01 || (fraction >= 1.0 && self.progress < 1.0) {
            self.progress = fraction;
            let _ = self.send(&Message::Progress(fraction));
        }
    }
}

/// Runs the request's operation.
fn execute<W: Write>(request: &Request, emit: &mut Emitter<W>) -> Result<Done, Failure> {
    let out_dir = request.out_dir.as_path();
    let bounds = &request.bounds;
    match &request.op {
        WorkerOp::Render { source, format } => {
            sniff(source)?;
            let pdfium = bind(&request.pdfium)?;
            render_pages(&pdfium, source, *format, out_dir, bounds, emit)
        }
        WorkerOp::Text { source, format } => {
            sniff(source)?;
            let pdfium = bind(&request.pdfium)?;
            extract_text(&pdfium, source, *format, out_dir, bounds, emit)
        }
        WorkerOp::Split { source } => {
            sniff(source)?;
            split(&request.pdfium, source, out_dir, bounds, emit)
        }
        WorkerOp::Merge { sources } => {
            for (i, source) in sources.iter().enumerate() {
                sniff(source).map_err(|f| f.about(i))?;
            }
            merge(&request.pdfium, sources, out_dir, bounds, emit)
        }
        WorkerOp::RemoveMetadata { source } => {
            sniff(source)?;
            remove_metadata(source, out_dir, bounds, emit)
        }
    }
}

/// Checks for `%PDF-` in the first [`SNIFF_BYTES`] bytes.
fn sniff(source: &Path) -> Result<(), Failure> {
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    File::open(source)
        .and_then(|file| file.take(SNIFF_BYTES as u64).read_to_end(&mut head))
        .map_err(|e| read_failed(&e))?;
    if is_pdf(&head) {
        Ok(())
    } else {
        Err(Failure::new(
            FailKind::NotPdf,
            "no %PDF- in the first 1024 bytes",
        ))
    }
}

/// True when `head` holds `%PDF-`.
pub(crate) fn is_pdf(head: &[u8]) -> bool {
    head.windows(5).any(|window| window == b"%PDF-")
}

fn read_failed(e: &io::Error) -> Failure {
    Failure::new(FailKind::Read, format!("reading a source: {:?}", e.kind()))
}

fn write_failed(e: &io::Error) -> Failure {
    Failure::new(
        FailKind::Write,
        format!("writing the output: {:?}", e.kind()),
    )
}

/// Binds PDFium from its absolute path (never a search).
fn bind(library: &Path) -> Result<Pdfium, Failure> {
    let bindings = Pdfium::bind_to_library(library).map_err(|e| {
        let detail = match e {
            // libloading's message holds the path.
            PdfiumError::LoadLibraryError(_) => "the library didn't load".to_string(),
            PdfiumError::LoadLibraryFunctionNameError(name) => {
                format!("the library lacks {name}")
            }
            other => format!("binding failed: {other:?}"),
        };
        Failure::new(FailKind::Library, detail)
    })?;
    Ok(Pdfium::new(bindings))
}

/// The largest file PDFium can be handed: pdfium-render passes its length
/// and every offset as a `c_ulong`, which has 32 bits on Windows.
fn pdfium_max_bytes() -> u64 {
    fn widen(n: impl Into<u64>) -> u64 {
        n.into()
    }
    widen(c_ulong::MAX)
}

/// Refuses a file of `len` bytes that PDFium couldn't address whole.
fn pdfium_can_read(len: u64) -> Result<(), Failure> {
    if len > pdfium_max_bytes() {
        return Err(Failure::new(
            FailKind::TooLarge,
            format!("{len} bytes, over PDFium's {}", pdfium_max_bytes()),
        ));
    }
    Ok(())
}

/// The source PDFium reads from. pdfium-render's read callback (it has no
/// way to open by path: `load_pdf_from_file` is this same reader path)
/// makes one `read` call per block PDFium asks for and takes any count
/// above zero as the whole block. So this reader fills the block or fails.
/// PDFium never asks past the length it was given, so a short block can
/// only mean the file changed while it was open.
struct WholeBlocks<R>(R);

impl<R: Read> Read for WholeBlocks<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_exact(buf)?;
        Ok(buf.len())
    }
}

impl<R: Seek> Seek for WholeBlocks<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

/// Opens `source` with PDFium, read block by block through
/// [`WholeBlocks`]. A file too large for PDFium to address is refused.
fn open<'p>(pdfium: &'p Pdfium, source: &Path) -> Result<PdfDocument<'p>, Failure> {
    let file = File::open(source).map_err(|e| read_failed(&e))?;
    pdfium_can_read(file.metadata().map_err(|e| read_failed(&e))?.len())?;
    pdfium
        .load_pdf_from_reader(WholeBlocks(file), None)
        .map_err(|e| match e {
            PdfiumError::PdfiumLibraryInternalError(
                PdfiumInternalError::PasswordError | PdfiumInternalError::SecurityError,
            ) => Failure::new(FailKind::Encrypted, "PDFium: it needs a password"),
            PdfiumError::IoError(e) => read_failed(&e),
            other => Failure::new(
                FailKind::Damaged,
                format!("PDFium can't open it: {other:?}"),
            ),
        })
}

/// Refuses a document PDFium opened that has a security handler at all.
/// Split and merge's PDFium fallbacks write new files without it, so an
/// owner password's restrictions would be gone (render and text don't
/// write PDFs and don't call this).
fn refuse_protected(document: &PdfDocument) -> Result<(), Failure> {
    match document.permissions().security_handler_revision() {
        Ok(PdfSecurityHandlerRevision::Unprotected) => Ok(()),
        _ => Err(Failure::new(
            FailKind::Encrypted,
            "PDFium: it has a security handler",
        )),
    }
}

/// A page count from PDFium, refusing an empty document.
fn page_count(document: &PdfDocument) -> Result<usize, Failure> {
    match usize::try_from(document.pages().len()) {
        Ok(0) | Err(_) => Err(Failure::new(FailKind::NoPages, "no pages")),
        Ok(n) => Ok(n),
    }
}

/// Creates `name` in the temp folder (never an existing file).
fn create(out_dir: &Path, name: &str) -> Result<File, Failure> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out_dir.join(name))
        .map_err(|e| write_failed(&e))
}

/// Writes one output through `fill`, at most `left` bytes, then syncs it.
/// Returns the bytes still allowed.
fn write_output(
    out_dir: &Path,
    name: &str,
    left: u64,
    fill: impl FnOnce(&mut Budget<BufWriter<File>>) -> Result<(), Failure>,
) -> Result<u64, Failure> {
    let file = create(out_dir, name)?;
    let mut budget = Budget::new(BufWriter::new(file), left);
    let filled = fill(&mut budget).and_then(|()| budget.flush().map_err(|e| write_failed(&e)));
    if budget.over {
        return Err(Failure::new(
            FailKind::TooMuchOutput,
            "over the output bound",
        ));
    }
    filled?;
    let left = budget.left();
    let file = budget
        .into_inner()
        .into_inner()
        .map_err(|e| write_failed(e.error()))?;
    file.sync_all().map_err(|e| write_failed(&e))?;
    Ok(left)
}

/// PDF → JPG or PNG: `page-{n}.{ext}` for every page.
fn render_pages<W: Write>(
    pdfium: &Pdfium,
    source: &Path,
    format: ImageFormat,
    out_dir: &Path,
    bounds: &Bounds,
    emit: &mut Emitter<W>,
) -> Result<Done, Failure> {
    let document = open(pdfium, source)?;
    let count = page_count(&document)?;
    if count > bounds.max_pages as usize {
        return Err(Failure::new(
            FailKind::TooManyPages,
            format!("{count} pages, over {}", bounds.max_pages),
        ));
    }
    emit.pages(count);
    let ext = names::image_ext(format);
    let mut left = bounds.max_output_bytes;
    let mut scaled_pages = Vec::new();
    for index in 0..count {
        let number = index + 1;
        let damaged =
            |what: &str| Failure::new(FailKind::Damaged, format!("page {number}: {what}"));
        let pdfium_index = i32::try_from(index).map_err(|_| damaged("index"))?;
        // The page (and PDFium's bitmap) is dropped before encoding.
        let (rgb, size) = {
            let page = document
                .pages()
                .get(pdfium_index)
                .map_err(|e| damaged(&format!("{e:?}")))?;
            let size = render::page_size(page.width().value, page.height().value, bounds)
                .ok_or_else(|| damaged("no usable size"))?;
            let rgb = render::draw(&page, size).map_err(|e| damaged(&format!("drawing: {e:?}")))?;
            (rgb, size)
        };
        if size.scaled {
            scaled_pages.push(u32::try_from(number).unwrap_or(u32::MAX));
        }
        let name = names::page(u32::try_from(number).unwrap_or(u32::MAX), ext);
        left = write_output(out_dir, &name, left, |out| {
            render::encode(&rgb, size, format, out).map_err(|e| match e {
                image::ImageError::IoError(e) => write_failed(&e),
                other => Failure::new(FailKind::Internal, format!("encoding: {other}")),
            })
        })?;
        emit.progress(number as f32 / count as f32);
    }
    Ok(Done {
        pages: u32::try_from(count).unwrap_or(u32::MAX),
        scaled_pages,
        ..Done::default()
    })
}

/// PDF → TXT or MD: `text.{txt|md}`.
fn extract_text<W: Write>(
    pdfium: &Pdfium,
    source: &Path,
    format: TextFormat,
    out_dir: &Path,
    bounds: &Bounds,
    emit: &mut Emitter<W>,
) -> Result<Done, Failure> {
    let document = open(pdfium, source)?;
    let count = page_count(&document)?;
    emit.pages(count);
    let too_much = || Failure::new(FailKind::TooMuchText, "over the text bound");
    let mut text = Text::new(format);
    for index in 0..count {
        let number = index + 1;
        let damaged =
            |what: String| Failure::new(FailKind::Damaged, format!("page {number}: {what}"));
        let pdfium_index = i32::try_from(index).map_err(|_| damaged("index".to_string()))?;
        let page = document
            .pages()
            .get(pdfium_index)
            .map_err(|e| damaged(format!("{e:?}")))?;
        let glyphs = page_glyphs(&page).map_err(|e| damaged(format!("text: {e:?}")))?;
        drop(page);
        text.add_page(&glyphs);
        if text.held_bytes() > bounds.max_text_bytes {
            return Err(too_much());
        }
        emit.progress(0.95 * number as f32 / count as f32);
    }
    if text.is_empty() {
        return Err(Failure::new(FailKind::NoText, "no page has text"));
    }
    let content = text.output();
    drop(text);
    if content.len() as u64 > bounds.max_text_bytes {
        return Err(too_much());
    }
    write_output(
        out_dir,
        names::text(format),
        bounds.max_output_bytes,
        |out| {
            out.write_all(content.as_bytes())
                .map_err(|e| write_failed(&e))
        },
    )?;
    emit.progress(1.0);
    Ok(Done {
        pages: u32::try_from(count).unwrap_or(u32::MAX),
        ..Done::default()
    })
}

/// How lopdf did with a file.
enum Loaded {
    /// Loaded, whole, not encrypted: the tree is walked.
    Whole(Box<Document>, Tree),
    /// lopdf couldn't read it (or lost part of it): PDFium may.
    Unreadable(String),
}

/// Loads `source` with lopdf under the decompression bound. An encrypted
/// file (any password, or an owner password only, whose restrictions a
/// rewrite would drop) is refused.
///
/// A file lopdf didn't read whole is left to PDFium: one whose page tree
/// doesn't match its `/Count`, one with objects its cross-reference table
/// lists that didn't load, and a hybrid-reference file (`/XRefStm`) with
/// any object missing that it uses. lopdf reads a hybrid file's extra
/// cross-reference stream only through the newest trailer, and only when
/// that trailer has a `/Prev`, so objects listed only there can be missing
/// with nothing saying so (their classic entries say "free").
fn load(source: &Path, bounds: &Bounds) -> Result<Loaded, Failure> {
    let options = LoadOptions {
        max_decompressed_size: Some(usize::try_from(bounds.max_decompressed).unwrap_or(usize::MAX)),
        ..LoadOptions::default()
    };
    let bytes = fs::read(source).map_err(|e| read_failed(&e))?;
    let hybrid = has_xref_stm(&bytes);
    let loaded = Document::load_mem_with_options(&bytes, options);
    drop(bytes);
    let document = match loaded {
        Ok(document) => document,
        Err(lopdf::Error::IO(e)) => return Err(read_failed(&e)),
        Err(
            lopdf::Error::Decryption(_)
            | lopdf::Error::InvalidPassword
            | lopdf::Error::UnsupportedSecurityHandler(_),
        ) => return Err(Failure::new(FailKind::Encrypted, "lopdf: encrypted")),
        Err(e) => return Ok(Loaded::Unreadable(format!("lopdf: {}", variant(&e)))),
    };
    if document.was_encrypted() || document.trailer.has(b"Encrypt") {
        return Err(Failure::new(FailKind::Encrypted, "lopdf: encrypted"));
    }
    let tree = match Tree::walk(&document) {
        Ok(tree) if tree.is_whole() => tree,
        Ok(_) => return Ok(Loaded::Unreadable("lopdf: pages missing".to_string())),
        Err(e) => return Ok(Loaded::Unreadable(format!("lopdf: {e:?}"))),
    };
    let missing = meta::missing_objects(&document);
    if missing.listed > 0 {
        return Ok(Loaded::Unreadable(format!(
            "lopdf: {} objects unreadable",
            missing.listed
        )));
    }
    if hybrid && missing.unlisted > 0 {
        return Ok(Loaded::Unreadable(format!(
            "lopdf: hybrid cross-reference, {} objects missing",
            missing.unlisted
        )));
    }
    Ok(Loaded::Whole(Box::new(document), tree))
}

/// True when `bytes` hold `/XRefStm`: a hybrid-reference file.
fn has_xref_stm(bytes: &[u8]) -> bool {
    const KEY: &[u8] = b"/XRefStm";
    bytes
        .iter()
        .enumerate()
        .any(|(at, &byte)| byte == b'/' && bytes[at..].starts_with(KEY))
}

/// The name of a library error's variant (`Parse`, `Xref`, …): its message
/// can quote bytes of the file.
fn variant(error: &impl std::fmt::Debug) -> String {
    let debug = format!("{error:?}");
    debug
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Saves `document` with lopdf as a full rewrite. A loaded trailer can
/// still hold `/XRefStm` (lopdf keeps it when there's no `/Prev`): its
/// offset means nothing in the new file, so it goes, with any `/Prev`.
fn save(document: &mut Document, mut out: &mut dyn Write) -> Result<(), Failure> {
    document.trailer.remove(b"XRefStm");
    document.trailer.remove(b"Prev");
    document.save_to(&mut out).map_err(|e| write_failed(&e))
}

/// Refuses a split of more than [`Bounds::max_split_pages`] pages, before
/// any file is written.
fn split_bound(count: usize, bounds: &Bounds) -> Result<(), Failure> {
    if count > bounds.max_split_pages as usize {
        return Err(Failure::new(
            FailKind::TooManyPagesToSplit,
            format!("{count} pages, over {}", bounds.max_split_pages),
        ));
    }
    Ok(())
}

/// Split PDF: `page-{n}.pdf` for every page, with lopdf, or with PDFium's
/// page import when lopdf can't read the file.
fn split<W: Write>(
    library: &Path,
    source: &Path,
    out_dir: &Path,
    bounds: &Bounds,
    emit: &mut Emitter<W>,
) -> Result<Done, Failure> {
    let mut left = bounds.max_output_bytes;
    if let Loaded::Whole(document, tree) = load(source, bounds)? {
        let count = tree.pages.len();
        split_bound(count, bounds)?;
        emit.pages(count);
        for index in 0..count {
            let mut output = Output::new();
            output.add(&document, &tree, &[index]);
            let mut page = output.finish();
            let name = names::page(u32::try_from(index + 1).unwrap_or(u32::MAX), "pdf");
            left = write_output(out_dir, &name, left, |out| save(&mut page, out))?;
            emit.progress((index + 1) as f32 / count as f32);
        }
        return Ok(Done {
            pages: u32::try_from(count).unwrap_or(u32::MAX),
            ..Done::default()
        });
    }
    let pdfium = bind(library)?;
    let document = open(&pdfium, source)?;
    refuse_protected(&document)?;
    let count = page_count(&document)?;
    split_bound(count, bounds)?;
    emit.pages(count);
    for index in 0..count {
        let pdfium_index =
            i32::try_from(index).map_err(|_| Failure::new(FailKind::Internal, "page index"))?;
        let mut page = pdfium
            .create_new_pdf()
            .map_err(|e| Failure::new(FailKind::Internal, format!("new PDF: {e:?}")))?;
        page.pages_mut()
            .copy_page_from_document(&document, pdfium_index, 0)
            .map_err(|e| Failure::new(FailKind::Damaged, format!("page import: {e:?}")))?;
        let name = names::page(u32::try_from(index + 1).unwrap_or(u32::MAX), "pdf");
        left = write_output(out_dir, &name, left, |out| save_pdfium(&page, out))?;
        emit.progress((index + 1) as f32 / count as f32);
    }
    Ok(Done {
        pages: u32::try_from(count).unwrap_or(u32::MAX),
        used_pdfium: true,
        ..Done::default()
    })
}

/// Saves a PDFium document into `out`.
fn save_pdfium(document: &PdfDocument, out: &mut Budget<BufWriter<File>>) -> Result<(), Failure> {
    document.save_to_writer(out).map_err(|e| match e {
        PdfiumError::IoError(e) => write_failed(&e),
        other => Failure::new(FailKind::Internal, format!("saving: {other:?}")),
    })
}

/// Merge PDFs: `merged.pdf` with every page of every source, in order.
fn merge<W: Write>(
    library: &Path,
    sources: &[std::path::PathBuf],
    out_dir: &Path,
    bounds: &Bounds,
    emit: &mut Emitter<W>,
) -> Result<Done, Failure> {
    let mut output = Output::new();
    let mut readable = true;
    for (i, source) in sources.iter().enumerate() {
        match load(source, bounds).map_err(|f| f.about(i))? {
            Loaded::Whole(document, tree) => {
                let all: Vec<usize> = (0..tree.pages.len()).collect();
                output.add(&document, &tree, &all);
                emit.pages(output.page_count());
                emit.progress(0.9 * (i + 1) as f32 / sources.len() as f32);
            }
            Loaded::Unreadable(_) => {
                readable = false;
                break;
            }
        }
    }
    if readable {
        let count = output.page_count();
        let mut merged = output.finish();
        write_output(out_dir, names::MERGED, bounds.max_output_bytes, |out| {
            save(&mut merged, out)
        })?;
        emit.progress(1.0);
        return Ok(Done {
            pages: u32::try_from(count).unwrap_or(u32::MAX),
            ..Done::default()
        });
    }
    drop(output);
    let pdfium = bind(library)?;
    let mut merged = pdfium
        .create_new_pdf()
        .map_err(|e| Failure::new(FailKind::Internal, format!("new PDF: {e:?}")))?;
    let mut count = 0;
    for (i, source) in sources.iter().enumerate() {
        let document = open(&pdfium, source).map_err(|f| f.about(i))?;
        refuse_protected(&document).map_err(|f| f.about(i))?;
        count += page_count(&document).map_err(|f| f.about(i))?;
        merged
            .pages_mut()
            .append(&document)
            .map_err(|e| Failure::new(FailKind::Damaged, format!("page import: {e:?}")).about(i))?;
        emit.pages(count);
        emit.progress(0.9 * (i + 1) as f32 / sources.len() as f32);
    }
    write_output(out_dir, names::MERGED, bounds.max_output_bytes, |out| {
        save_pdfium(&merged, out)
    })?;
    emit.progress(1.0);
    Ok(Done {
        pages: u32::try_from(count).unwrap_or(u32::MAX),
        used_pdfium: true,
        ..Done::default()
    })
}

/// Remove Metadata: `clean.pdf`, a full rewrite without the Info
/// dictionary and the XMP stream. lopdf only: a file it can't read whole
/// is damaged.
fn remove_metadata<W: Write>(
    source: &Path,
    out_dir: &Path,
    bounds: &Bounds,
    emit: &mut Emitter<W>,
) -> Result<Done, Failure> {
    let (mut document, tree) = match load(source, bounds)? {
        Loaded::Whole(document, tree) => (document, tree),
        Loaded::Unreadable(detail) => return Err(Failure::new(FailKind::Damaged, detail)),
    };
    let count = tree.pages.len();
    drop(tree);
    emit.pages(count);
    let removed = meta::strip(&mut document);
    emit.progress(0.5);
    write_output(out_dir, names::CLEAN, bounds.max_output_bytes, |out| {
        save(&mut document, out)
    })?;
    emit.progress(1.0);
    Ok(Done {
        pages: u32::try_from(count).unwrap_or(u32::MAX),
        removed,
        ..Done::default()
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;
    use std::path::PathBuf;

    use lopdf::Object;

    use super::super::copy::tests::{contains, fixture, save_classic, save_modern};
    use super::super::protocol::MetaKey;
    use super::*;
    use crate::output::unique_suffix;

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Dir {
            let path =
                std::env::temp_dir().join(format!("grape-pdf-worker-unit-{}", unique_suffix()));
            fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn bounds() -> Bounds {
        Bounds {
            dpi: 300.0,
            max_pages: 2000,
            max_split_pages: 100_000,
            max_page_pixels: 100_000_000,
            max_side: 65_535,
            max_output_bytes: 8 << 30,
            max_text_bytes: 256 << 20,
            max_decompressed: 512 << 20,
            max_heap_bytes: 4 << 30,
        }
    }

    fn request(dir: &Path, op: WorkerOp) -> Request {
        Request {
            // No PDFium here: these operations don't bind it.
            pdfium: dir.join("no-pdfium"),
            out_dir: dir.join("out"),
            op,
            bounds: bounds(),
        }
    }

    /// Runs `op` in-process, as the worker thread would: the result and
    /// the lines it printed.
    fn run_op(dir: &Path, op: WorkerOp) -> (Result<Done, Failure>, Vec<Message>) {
        fs::create_dir_all(dir.join("out")).unwrap();
        let mut emit = Emitter::new(Vec::new());
        let result = execute(&request(dir, op), &mut emit);
        let lines = String::from_utf8(emit.out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        (result, lines)
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn sniffing_wants_the_signature_near_the_start() {
        assert!(is_pdf(b"%PDF-1.7\n"));
        let mut late = vec![b' '; 1000];
        late.extend_from_slice(b"%PDF-1.4");
        assert!(is_pdf(&late));
        assert!(!is_pdf(b"%PDF"));
        assert!(!is_pdf(b"PK\x03\x04"));

        let dir = Dir::new();
        let mut far = vec![b' '; SNIFF_BYTES];
        far.extend_from_slice(b"%PDF-1.4");
        let far = write(&dir.0, "far.pdf", &far);
        assert_eq!(sniff(&far).unwrap_err().kind, FailKind::NotPdf);
        assert_eq!(
            sniff(&dir.0.join("missing.pdf")).unwrap_err().kind,
            FailKind::Read
        );
    }

    #[test]
    fn requests_are_checked() {
        let dir = Dir::new();
        fs::create_dir_all(dir.0.join("out")).unwrap();
        let good = request(
            &dir.0,
            WorkerOp::Split {
                source: dir.0.join("a.pdf"),
            },
        );
        let json = serde_json::to_vec(&good).unwrap();
        assert_eq!(read_request(json.as_slice()).unwrap(), good);

        let relative = Request {
            op: WorkerOp::Split {
                source: PathBuf::from("a.pdf"),
            },
            ..good.clone()
        };
        let json = serde_json::to_vec(&relative).unwrap();
        assert_eq!(
            read_request(json.as_slice()).unwrap_err().kind,
            FailKind::Internal
        );

        let no_sources = Request {
            op: WorkerOp::Merge {
                sources: Vec::new(),
            },
            ..good.clone()
        };
        let json = serde_json::to_vec(&no_sources).unwrap();
        assert_eq!(
            read_request(json.as_slice()).unwrap_err().kind,
            FailKind::Internal
        );

        let missing_out = Request {
            out_dir: dir.0.join("gone"),
            ..good.clone()
        };
        let json = serde_json::to_vec(&missing_out).unwrap();
        assert_eq!(
            read_request(json.as_slice()).unwrap_err().kind,
            FailKind::Write
        );

        // A bad request's detail never quotes it.
        let failure = read_request(&br#"{"pdfium":"/secret/name.dll"}"#[..]).unwrap_err();
        assert_eq!(failure.kind, FailKind::Internal);
        assert!(!failure.detail.contains("secret"), "{}", failure.detail);
    }

    #[test]
    fn split_writes_one_pdf_per_page() {
        let dir = Dir::new();
        let source = write(&dir.0, "a.pdf", &save_modern(&fixture("A")));
        let before = fs::read(&source).unwrap();
        let (result, lines) = run_op(
            &dir.0,
            WorkerOp::Split {
                source: source.clone(),
            },
        );
        let done = result.unwrap();
        assert_eq!(done.pages, 3);
        assert!(!done.used_pdfium);
        assert_eq!(
            listing(&dir.0.join("out")),
            ["page-1.pdf", "page-2.pdf", "page-3.pdf"]
        );
        for n in 1..=3 {
            let page = Document::load(dir.0.join("out").join(format!("page-{n}.pdf"))).unwrap();
            assert_eq!(page.get_pages().len(), 1);
            assert!(page
                .extract_text(&[1])
                .unwrap()
                .contains(&format!("A page {n}")));
        }
        assert_eq!(lines.first(), Some(&Message::Pages(3)));
        assert_eq!(lines.last(), Some(&Message::Progress(1.0)));
        assert_eq!(fs::read(&source).unwrap(), before);
    }

    #[test]
    fn merge_writes_every_page_in_order() {
        let dir = Dir::new();
        let a = write(&dir.0, "a.pdf", &save_classic(&fixture("A")));
        let b = write(&dir.0, "b.pdf", &save_modern(&fixture("B")));
        let (result, lines) = run_op(
            &dir.0,
            WorkerOp::Merge {
                sources: vec![b, a],
            },
        );
        assert_eq!(result.unwrap().pages, 6);
        let merged = Document::load(dir.0.join("out").join(names::MERGED)).unwrap();
        assert_eq!(merged.get_pages().len(), 6);
        assert!(merged.extract_text(&[1]).unwrap().contains("B page 1"));
        assert!(merged.extract_text(&[4]).unwrap().contains("A page 1"));
        assert!(lines.contains(&Message::Pages(3)) && lines.contains(&Message::Pages(6)));
    }

    #[test]
    fn remove_metadata_reports_what_it_removed() {
        let dir = Dir::new();
        let source = write(&dir.0, "a.pdf", &save_classic(&fixture("A")));
        let (result, _) = run_op(&dir.0, WorkerOp::RemoveMetadata { source });
        let done = result.unwrap();
        assert_eq!(
            done.removed,
            [
                MetaKey::Title,
                MetaKey::Author,
                MetaKey::Producer,
                MetaKey::Other,
                MetaKey::Xmp
            ]
        );
        let clean = fs::read(dir.0.join("out").join(names::CLEAN)).unwrap();
        assert!(!contains(&clean, "MARK-TITLE"));
        assert_eq!(Document::load_mem(&clean).unwrap().get_pages().len(), 3);
    }

    #[test]
    fn encrypted_files_are_refused() {
        use lopdf::{EncryptionState, EncryptionVersion, Permissions};
        for user_password in ["", "user"] {
            let dir = Dir::new();
            let mut doc = fixture("E");
            let id = Object::string_literal("0123456789abcdef");
            doc.trailer.set("ID", vec![id.clone(), id]);
            let state = EncryptionState::try_from(EncryptionVersion::V2 {
                document: &doc,
                owner_password: "owner",
                user_password,
                key_length: 128,
                permissions: Permissions::PRINTABLE,
            })
            .unwrap();
            doc.encrypt(&state).unwrap();
            let mut bytes = Vec::new();
            doc.save_to(&mut bytes).unwrap();
            let source = write(&dir.0, "e.pdf", &bytes);
            for op in [
                WorkerOp::Split {
                    source: source.clone(),
                },
                WorkerOp::RemoveMetadata {
                    source: source.clone(),
                },
                WorkerOp::Merge {
                    sources: vec![source.clone()],
                },
            ] {
                let (result, _) = run_op(&dir.0, op);
                assert_eq!(
                    result.unwrap_err().kind,
                    FailKind::Encrypted,
                    "{user_password:?}"
                );
            }
            assert!(listing(&dir.0.join("out")).is_empty());
        }
    }

    #[test]
    fn not_a_pdf_and_damaged_files_are_refused() {
        let dir = Dir::new();
        let text = write(&dir.0, "a.pdf", b"just text");
        let (result, _) = run_op(
            &dir.0,
            WorkerOp::RemoveMetadata {
                source: text.clone(),
            },
        );
        assert_eq!(result.unwrap_err().kind, FailKind::NotPdf);
        // In a merge, the failure says which source.
        let good = write(&dir.0, "b.pdf", &save_classic(&fixture("B")));
        let (result, _) = run_op(
            &dir.0,
            WorkerOp::Merge {
                sources: vec![good, text],
            },
        );
        assert_eq!(result.unwrap_err().input, Some(1));
        // A header and nothing else: lopdf can't read it, and Remove
        // Metadata has no fallback.
        let broken = write(&dir.0, "c.pdf", b"%PDF-1.7\n%garbage\n");
        let (result, _) = run_op(
            &dir.0,
            WorkerOp::RemoveMetadata {
                source: broken.clone(),
            },
        );
        assert_eq!(result.unwrap_err().kind, FailKind::Damaged);
        // Split falls back to PDFium, which isn't there in unit tests.
        let (result, _) = run_op(&dir.0, WorkerOp::Split { source: broken });
        assert_eq!(result.unwrap_err().kind, FailKind::Library);
    }

    #[test]
    fn the_output_bound_stops_a_job() {
        let dir = Dir::new();
        let source = write(&dir.0, "a.pdf", &save_classic(&fixture("A")));
        fs::create_dir_all(dir.0.join("out")).unwrap();
        let mut emit = Emitter::new(Vec::new());
        let mut request = request(&dir.0, WorkerOp::Split { source });
        request.bounds.max_output_bytes = 100;
        let result = execute(&request, &mut emit);
        assert_eq!(result.unwrap_err().kind, FailKind::TooMuchOutput);
    }

    #[test]
    fn split_refuses_more_pages_than_its_bound() {
        let dir = Dir::new();
        let source = write(&dir.0, "a.pdf", &save_classic(&fixture("A")));
        fs::create_dir_all(dir.0.join("out")).unwrap();
        let mut emit = Emitter::new(Vec::new());
        let mut request = request(&dir.0, WorkerOp::Split { source });
        request.bounds.max_split_pages = 2;
        let result = execute(&request, &mut emit);
        assert_eq!(result.unwrap_err().kind, FailKind::TooManyPagesToSplit);
        assert!(listing(&dir.0.join("out")).is_empty());
        // At the bound is fine.
        request.bounds.max_split_pages = 3;
        assert_eq!(execute(&request, &mut emit).unwrap().pages, 3);
    }

    /// A one-page hybrid-reference file (PDF 32000-1 §7.5.8.4). The font,
    /// object 5, sits in object stream 6, and only the cross-reference
    /// stream 7 lists it: the classic table says 5 is free, and 6 too
    /// unless `stream_listed`. With `older_section`, the trailer also has a
    /// `/Prev` to an older, empty classic section (Word's layout, which
    /// lopdf reads whole). Info (8) and XMP (9) carry markers.
    fn hybrid(stream_listed: bool, older_section: bool) -> Vec<u8> {
        fn object(pdf: &mut Vec<u8>, offsets: &mut [usize; 10], n: usize, body: &[u8]) {
            offsets[n] = pdf.len();
            pdf.extend_from_slice(format!("{n} 0 obj\n").as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let mut pdf = b"%PDF-1.5\n".to_vec();
        let mut offsets = [0usize; 10];
        let stream = |dict: &str, data: &[u8]| {
            let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            body.extend_from_slice(data);
            body.extend_from_slice(b"\nendstream");
            body
        };
        object(
            &mut pdf,
            &mut offsets,
            1,
            b"<< /Type /Catalog /Pages 2 0 R /Metadata 9 0 R >>",
        );
        object(
            &mut pdf,
            &mut offsets,
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        );
        object(
            &mut pdf,
            &mut offsets,
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>",
        );
        object(
            &mut pdf,
            &mut offsets,
            4,
            &stream("", b"BT /F1 24 Tf 72 700 Td (Hybrid page) Tj ET"),
        );
        let members = b"5 0 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";
        object(
            &mut pdf,
            &mut offsets,
            6,
            &stream("/Type /ObjStm /N 1 /First 4", members),
        );
        object(&mut pdf, &mut offsets, 8, b"<< /Title (MARK-TITLE) >>");
        object(
            &mut pdf,
            &mut offsets,
            9,
            &stream(
                "/Type /Metadata /Subtype /XML",
                b"<x:xmpmeta>MARK-XMP</x:xmpmeta>",
            ),
        );
        // Entries for 5, 6 and 7: W [1 4 2].
        let at7 = pdf.len();
        let entry = |kind: u8, field: usize, index: u16| {
            let mut bytes = vec![kind];
            bytes.extend_from_slice(&u32::try_from(field).unwrap().to_be_bytes());
            bytes.extend_from_slice(&index.to_be_bytes());
            bytes
        };
        let mut entries = entry(2, 6, 0);
        entries.extend(entry(1, offsets[6], 0));
        entries.extend(entry(1, at7, 0));
        object(
            &mut pdf,
            &mut offsets,
            7,
            &stream("/Type /XRef /Size 10 /W [1 4 2] /Index [5 3]", &entries),
        );
        let older = older_section.then(|| {
            let at = pdf.len();
            pdf.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 1 >>\n");
            at
        });
        let xref = pdf.len();
        pdf.extend_from_slice(b"xref\n0 10\n0000000000 65535 f \n");
        for (n, offset) in offsets.iter().enumerate().skip(1) {
            let free = n == 5 || (n == 6 && !stream_listed);
            let line = if free {
                "0000000000 00001 f \n".to_string()
            } else {
                format!("{offset:010} 00000 n \n")
            };
            pdf.extend_from_slice(line.as_bytes());
        }
        let prev = older.map(|at| format!(" /Prev {at}")).unwrap_or_default();
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 10 /Root 1 0 R /Info 8 0 R /XRefStm {at7}{prev} >>\n\
                 startxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        pdf
    }

    /// `bytes` with a classic incremental update that only replaces the
    /// Info dictionary: its trailer has `/Prev` but no `/XRefStm`.
    fn with_update(mut bytes: Vec<u8>) -> Vec<u8> {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let at = text.rfind("startxref").unwrap();
        let prev: usize = text[at + 9..]
            .trim()
            .trim_end_matches("%%EOF")
            .trim()
            .parse()
            .unwrap();
        let offset = bytes.len();
        bytes.extend_from_slice(b"8 0 obj\n<< /Title (MARK-NEWER) >>\nendobj\n");
        let xref = bytes.len();
        bytes.extend_from_slice(
            format!(
                "xref\n0 1\n0000000000 65535 f \n8 1\n{offset:010} 00000 n \n\
                 trailer\n<< /Size 10 /Root 1 0 R /Info 8 0 R /Prev {prev} >>\n\
                 startxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        bytes
    }

    #[test]
    fn a_hybrid_file_lopdf_reads_only_in_part_goes_to_pdfium() {
        for (what, bytes) in [
            ("hybrid", hybrid(false, false)),
            (
                "hybrid, then a classic update",
                with_update(hybrid(false, false)),
            ),
        ] {
            let dir = Dir::new();
            let source = write(&dir.0, "h.pdf", &bytes);
            let bounds = bounds();
            assert!(
                matches!(load(&source, &bounds).unwrap(), Loaded::Unreadable(_)),
                "{what}"
            );
            // Split falls back to PDFium (not there in unit tests); Remove
            // Metadata has no fallback. Nothing is written either way.
            let (result, _) = run_op(
                &dir.0,
                WorkerOp::Split {
                    source: source.clone(),
                },
            );
            assert_eq!(result.unwrap_err().kind, FailKind::Library, "{what}");
            let (result, _) = run_op(&dir.0, WorkerOp::RemoveMetadata { source });
            assert_eq!(result.unwrap_err().kind, FailKind::Damaged, "{what}");
            assert!(listing(&dir.0.join("out")).is_empty(), "{what}");
        }
    }

    #[test]
    fn hybrid_files_lopdf_reads_whole_still_work() {
        // Word's layout: lopdf follows the /XRefStm.
        let dir = Dir::new();
        let source = write(&dir.0, "w.pdf", &hybrid(false, true));
        let (result, _) = run_op(&dir.0, WorkerOp::Split { source });
        let done = result.unwrap();
        assert!(!done.used_pdfium);
        let page = fs::read(dir.0.join("out").join("page-1.pdf")).unwrap();
        assert!(contains(&page, "Helvetica"));

        // The object stream is in the classic table too: lopdf reads it,
        // and the stale /XRefStm isn't written again.
        let dir = Dir::new();
        let source = write(&dir.0, "a.pdf", &hybrid(true, false));
        let (result, _) = run_op(&dir.0, WorkerOp::RemoveMetadata { source });
        assert_eq!(result.unwrap().removed, [MetaKey::Title, MetaKey::Xmp]);
        let clean = fs::read(dir.0.join("out").join(names::CLEAN)).unwrap();
        assert!(!contains(&clean, "XRefStm") && !contains(&clean, "MARK-TITLE"));
        assert!(contains(&clean, "Helvetica"));
        let clean = Document::load_mem(&clean).unwrap();
        assert_eq!(clean.get_pages().len(), 1);
    }

    #[test]
    fn the_hybrid_marker_is_found_anywhere() {
        assert!(has_xref_stm(b"<< /Size 3 /XRefStm 120 >>"));
        assert!(has_xref_stm(b"/XRefStm"));
        assert!(!has_xref_stm(b"<< /XRef /Stm >>"));
        assert!(!has_xref_stm(b"/XRefSt"));
        assert!(!has_xref_stm(b""));
    }

    #[test]
    fn an_owner_password_file_lopdf_cannot_read_is_left_to_pdfium() {
        use lopdf::xref::XrefType;
        use lopdf::{EncryptionState, EncryptionVersion, Permissions};
        let mut doc = fixture("E");
        let id = Object::string_literal("0123456789abcdef");
        doc.trailer.set("ID", vec![id.clone(), id]);
        let state = EncryptionState::try_from(EncryptionVersion::V2 {
            document: &doc,
            owner_password: "owner",
            user_password: "",
            key_length: 128,
            permissions: Permissions::PRINTABLE,
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
        doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        // Every entry of the table 3 bytes off: lopdf loses the objects,
        // PDFium rebuilds the table. So the PDFium fallback must refuse the
        // file itself (checked with PDFium in tests/pdf.rs).
        let table = bytes.windows(6).rposition(|w| w == b"\nxref\n").unwrap();
        let mut out = bytes[..table].to_vec();
        let tail = std::str::from_utf8(&bytes[table..]).unwrap();
        for line in tail.split_inclusive('\n') {
            match line
                .strip_suffix(" 00000 n \r\n")
                .or(line.strip_suffix(" 00000 n \n"))
            {
                Some(offset) if offset.len() == 10 => {
                    let offset: usize = offset.parse().unwrap();
                    let end = &line[10..];
                    out.extend_from_slice(format!("{:010}{end}", offset + 3).as_bytes());
                }
                _ => out.extend_from_slice(line.as_bytes()),
            }
        }
        assert_ne!(out, bytes);
        let dir = Dir::new();
        let source = write(&dir.0, "o.pdf", &out);
        assert!(matches!(
            load(&source, &bounds()).unwrap(),
            Loaded::Unreadable(_)
        ));
    }

    #[test]
    fn pdfium_reads_whole_blocks_or_nothing() {
        /// Gives at most one byte per read.
        struct Trickle(io::Cursor<Vec<u8>>);
        impl Read for Trickle {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let end = buf.len().min(1);
                self.0.read(&mut buf[..end])
            }
        }
        impl Seek for Trickle {
            fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
                self.0.seek(pos)
            }
        }
        let mut reader = WholeBlocks(Trickle(io::Cursor::new(b"0123456789".to_vec())));
        assert_eq!(reader.seek(SeekFrom::End(0)).unwrap(), 10);
        reader.seek(SeekFrom::Start(2)).unwrap();
        let mut block = [0u8; 5];
        assert_eq!(reader.read(&mut block).unwrap(), 5);
        assert_eq!(&block, b"23456");
        // Past the end (the file shrank): an error, so PDFium's read fails.
        reader.seek(SeekFrom::Start(8)).unwrap();
        assert_eq!(
            reader.read(&mut block).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn files_pdfium_cannot_address_are_refused() {
        let max = pdfium_max_bytes();
        assert!(max >= u64::from(u32::MAX));
        if cfg!(windows) {
            assert_eq!(max, u64::from(u32::MAX));
        }
        assert!(pdfium_can_read(max).is_ok());
        if let Some(over) = max.checked_add(1) {
            assert_eq!(pdfium_can_read(over).unwrap_err().kind, FailKind::TooLarge);
        }
    }

    #[test]
    fn library_errors_are_named_by_variant_only() {
        assert_eq!(
            variant(&lopdf::Error::Syntax("MARK-CONTENT".to_string())),
            "Syntax"
        );
        assert_eq!(variant(&lopdf::Error::InvalidPassword), "InvalidPassword");
    }

    #[test]
    fn progress_only_moves_forward_in_steps() {
        let mut emit = Emitter::new(Vec::new());
        for fraction in [0.001, 0.005, 0.02, 0.01, 0.5, 0.505, 2.0, 1.0] {
            emit.progress(fraction);
        }
        let lines = String::from_utf8(emit.out).unwrap();
        assert_eq!(
            lines,
            "{\"progress\":0.02}\n{\"progress\":0.5}\n{\"progress\":1.0}\n"
        );
    }
}
