//! The SVG worker (D39): one job per process, started as
//! `<exe> --grape-svg-worker` by the parent.
//!
//! It reads one [`Request`] from stdin, arms its heap bound
//! ([`crate::pdf::heap::arm`], with `maxHeapBytes`), does the job on a
//! thread with a 64 MiB stack inside `catch_unwind`, prints [`Message`]s on
//! stdout and exits 0 after `done` or 1 after `error` (or
//! [`crate::pdf::heap::OVER_HEAP_EXIT`] past the heap bound). It opens the
//! source read-only and writes only `pixels.rgba`, new, into the request's
//! work folder.
//!
//! It never logs: usvg's and resvg's `log` warnings go nowhere, because no
//! logger is installed in this process. No line it prints holds a path, a
//! name or file contents; XML errors are reported by kind and position,
//! never with the names or text they quote. A panic is caught and reported
//! as `internal`; a stack overflow or an abort just ends the process, and
//! the parent reports that.
//!
//! The steps:
//! 1. Read the source, at most `maxSourceBytes` (a larger file is refused
//!    unread, from its length).
//! 2. Gunzip it when it starts `1F 8B`, at most `maxTextBytes` out.
//! 3. Decode UTF-8, a BOM allowed; anything else (UTF-16 too) isn't read.
//! 4. Parse the XML with roxmltree: DTDs allowed (Illustrator's and
//!    Inkscape's files carry the SVG 1.1 DOCTYPE), no entity resolver (so
//!    no `SYSTEM` entity or external DTD is ever fetched), at most
//!    `maxNodes` nodes, which embedded SVGs share ([`Budget`]). Entity
//!    amplification is left to the heap bound.
//! 5. Load the system's fonts, only when the document can draw text: an
//!    element named `text` in the SVG namespace or in none, or an embedded
//!    SVG (which may hold text).
//! 6. Build usvg's tree: no resources folder, 96 DPI, `en`, no injected
//!    style sheet, and the data-only resolver ([`Budget`]).
//! 7. Size the canvas ([`size::canvas`]) and say so (`image`), then
//!    allocate it, draw it on transparent, demultiply it, and write it to
//!    `pixels.rgba` (3 channels when every pixel is opaque) and sync it.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::thread;

use flate2::read::GzDecoder;
use grape_heif_proto::{PixelHeader, PixelWriter};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{self, fontdb, roxmltree};
use serde::Deserialize;

use super::protocol::{
    Done, FailKind, Failure, ImageInfo, Limits, Message, Request, MAX_REQUEST_BYTES, PIXELS,
    PROTOCOL,
};
use super::resolve::Budget;
use super::size;
use crate::pdf::heap;

/// The job thread's name; its panics are silent ([`silence_job_panics`]).
const JOB_THREAD: &str = "grape-svg-job";

/// The job thread's stack: trees at usvg's 1,024-level depth limit need
/// more than 1 MiB, the main thread's on Windows.
const STACK_BYTES: usize = 64 * 1024 * 1024;

/// How often the Unix watchdog looks for a new parent.
#[cfg(unix)]
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// The `pixels.rgba` writer's buffer.
const WRITE_BUFFER: usize = 1 << 20;

/// The SVG namespace.
const SVG_NS: &str = "http://www.w3.org/2000/svg";

/// A gzip stream's first bytes.
const GZIP: [u8; 2] = [0x1f, 0x8b];

/// UTF-8's byte order mark.
const UTF8_BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

const MIB: u64 = 1024 * 1024;

/// The worker's entry point: `Grape --grape-svg-worker` and the
/// `grape-svg-worker` test binary both call this and exit with its code.
/// It reads its request from stdin and answers on stdout (D39). Every
/// panic is silent: its message could name what the job was working on.
pub fn worker_main() -> ExitCode {
    panic::set_hook(Box::new(|_| {}));
    watch_parent();
    if serve(io::stdin().lock(), io::stdout()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// [`worker_main`] over any streams: reads one request from `input`, runs
/// it and writes the lines to `output`. True when it ended with `done`.
pub(crate) fn serve(input: impl Read, output: impl Write + Send) -> bool {
    silence_job_panics();
    let lines = Lines {
        out: Mutex::new(output),
    };
    let result = read_request(input).and_then(|request| {
        // The output is set up (its buffer allocated) before the heap
        // bound is armed, so an exit at the bound never finds it half
        // made.
        lines.ready();
        heap::arm(request.limits.max_heap_bytes);
        run(&request, &lines)
    });
    lines.finish(result)
}

/// Makes panics on [`JOB_THREAD`] silent, once per process, and leaves the
/// rest to the hook that was there (so a test binary still reports its own
/// failures). [`worker_main`] silences every panic first.
fn silence_job_panics() {
    static SILENCE: Once = Once::new();
    SILENCE.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if thread::current().name() != Some(JOB_THREAD) {
                previous(info);
            }
        }));
    });
}

/// Ends the process once its parent changes: a worker whose Grape died is
/// reparented, and on a Mac there's no Job Object to end it. Started once
/// per process.
#[cfg(unix)]
fn watch_parent() {
    static WATCH: Once = Once::new();
    WATCH.call_once(|| {
        let parent = std::os::unix::process::parent_id();
        // Without the thread the worker still ends with its job or when
        // the parent kills it.
        let _ = thread::Builder::new()
            .name("grape-svg-watchdog".to_string())
            .spawn(move || loop {
                thread::sleep(WATCH_EVERY);
                if std::os::unix::process::parent_id() != parent {
                    std::process::exit(1);
                }
            });
    });
}

#[cfg(not(unix))]
fn watch_parent() {}

/// The output, one JSON line per message, shared by the job thread and the
/// last line.
struct Lines<W> {
    out: Mutex<W>,
}

impl<W: Write> Lines<W> {
    fn send(&self, message: &Message) -> io::Result<()> {
        let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        out.write_all(&line)?;
        out.flush()
    }

    /// Progress: a parent that has gone just doesn't hear it.
    fn progress(&self, fraction: f32) {
        let _ = self.send(&Message::Progress(fraction));
    }

    /// Flushes nothing, but makes the writer set itself up.
    fn ready(&self) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = out.flush();
    }

    /// Writes the last line; true when it is `done` and got out.
    fn finish(self, result: Result<Done, Failure>) -> bool {
        match result {
            Ok(done) => self.send(&Message::Done(done)).is_ok(),
            Err(failure) => {
                // The parent may be gone; then there's no one to tell.
                let _ = self.send(&Message::Error(failure));
                false
            }
        }
    }
}

/// Reads, parses and checks the request.
fn read_request(input: impl Read) -> Result<Request, Failure> {
    let mut bytes = Vec::new();
    input
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| internal(format!("reading the request: {:?}", e.kind())))?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(internal("the request is too long"));
    }
    // The version first, so another version's request is refused as that,
    // not as unknown fields.
    #[derive(Deserialize)]
    struct Version {
        protocol: u32,
    }
    if let Ok(Version { protocol }) = serde_json::from_slice::<Version>(&bytes) {
        if protocol != PROTOCOL {
            return Err(Failure::new(
                FailKind::Protocol,
                format!("protocol {protocol}, but this worker speaks {PROTOCOL}"),
            ));
        }
    }
    // serde_json's messages can quote the input: report the position only.
    let request: Request = serde_json::from_slice(&bytes).map_err(|e| {
        internal(format!(
            "bad request ({:?} at {}:{})",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;
    let work_dir = &request.work_dir;
    if !work_dir.is_absolute() || !work_dir.is_dir() {
        return Err(internal(
            "the work folder isn't an absolute path to a folder",
        ));
    }
    if !request.source.is_absolute() || !request.source.is_file() {
        return Err(internal("the source isn't an absolute path to a file"));
    }
    if request.limits.has_zero() {
        return Err(internal("a limit is 0"));
    }
    Ok(request)
}

/// Runs the job on its own thread, catching a panic.
fn run<W: Write + Send>(request: &Request, lines: &Lines<W>) -> Result<Done, Failure> {
    let panicked = || internal("the job panicked");
    thread::scope(|scope| {
        let job = thread::Builder::new()
            .name(JOB_THREAD.to_string())
            .stack_size(STACK_BYTES)
            .spawn_scoped(scope, || {
                panic::catch_unwind(AssertUnwindSafe(|| execute(request, lines)))
                    .unwrap_or_else(|_| Err(panicked()))
            });
        match job {
            Ok(handle) => handle.join().unwrap_or_else(|_| Err(panicked())),
            Err(e) => Err(internal(format!("starting the job thread: {:?}", e.kind()))),
        }
    })
}

/// The job, step by step (the module docs).
fn execute<W: Write>(request: &Request, lines: &Lines<W>) -> Result<Done, Failure> {
    let limits = &request.limits;
    let bytes = read_source(&request.source, limits)?;
    let bytes = gunzip(bytes, limits)?;
    let text = decode_text(&bytes)?;
    let doc = parse_xml(text, limits)?;
    lines.progress(0.2);

    let mut fonts = fontdb::Database::new();
    if needs_fonts(&doc) {
        fonts.load_system_fonts();
    }
    lines.progress(0.3);

    let budget = Budget::new(limits);
    // Embedded SVGs get what the file's own XML leaves of the node limit.
    budget.count_nodes(&doc);
    let tree = {
        let options = options(&budget, fonts);
        usvg::Tree::from_xmltree(&doc, &options).map_err(tree_failure)?
    };
    // The tree owns what it needs: the text and the XML go now.
    drop(doc);
    drop(bytes);
    let left_out = budget.left_out();

    let css = tree.size();
    let canvas = size::canvas(
        request.mode,
        f64::from(css.width()),
        f64::from(css.height()),
        limits.max_side,
        limits.max_pixels,
    );
    let info = ImageInfo {
        width: canvas.width,
        height: canvas.height,
        css_width: css.width(),
        css_height: css.height(),
        scaled: canvas.scaled,
    };
    // Before any pixel exists: the parent checks the size first.
    lines
        .send(&Message::Image(info))
        .map_err(|e| internal(format!("sending the image line: {:?}", e.kind())))?;

    let mut pixmap = Pixmap::new(canvas.width, canvas.height).ok_or_else(|| {
        Failure::new(
            FailKind::TooLarge,
            format!(
                "a {}x{} canvas can't be allocated",
                canvas.width, canvas.height
            ),
        )
    })?;
    let (sx, sy) = (
        canvas.width as f32 / css.width(),
        canvas.height as f32 / css.height(),
    );
    // A size so small that its scale isn't a number draws nothing.
    if sx.is_finite() && sy.is_finite() {
        resvg::render(&tree, Transform::from_scale(sx, sy), &mut pixmap.as_mut());
    }
    drop(tree);
    lines.progress(0.9);

    let rgba = pixmap.take_demultiplied();
    let channels = if rgba.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255) {
        3
    } else {
        4
    };
    write_pixels(
        &request.work_dir,
        canvas.width,
        canvas.height,
        &rgba,
        channels,
    )
    .map_err(|e| {
        Failure::new(
            FailKind::Write,
            format!("writing the pixels: {:?}", e.kind()),
        )
    })?;
    Ok(Done {
        width: canvas.width,
        height: canvas.height,
        channels,
        left_out,
        scaled: canvas.scaled,
    })
}

/// The source's bytes, read-only, at most `maxSourceBytes`.
fn read_source(source: &Path, limits: &Limits) -> Result<Vec<u8>, Failure> {
    let read_failed = |e: io::Error| {
        Failure::new(
            FailKind::Read,
            format!("reading the source: {:?}", e.kind()),
        )
    };
    let file = File::open(source).map_err(read_failed)?;
    let len = file.metadata().map_err(read_failed)?.len();
    let too_large = || {
        Failure::new(
            FailKind::TooLarge,
            format!(
                "the file is over the {} MiB limit",
                limits.max_source_bytes / MIB
            ),
        )
    };
    if len > limits.max_source_bytes {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    // It may have grown since its length was read.
    file.take(limits.max_source_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(read_failed)?;
    if bytes.len() as u64 > limits.max_source_bytes {
        return Err(too_large());
    }
    Ok(bytes)
}

/// `bytes`, inflated if they are gzip: at most `maxTextBytes` either way.
fn gunzip(bytes: Vec<u8>, limits: &Limits) -> Result<Vec<u8>, Failure> {
    let max = limits.max_text_bytes;
    let too_large = || {
        Failure::new(
            FailKind::TooLarge,
            format!("the SVG text is over the {} MiB limit", max / MIB),
        )
    };
    if !bytes.starts_with(&GZIP) {
        if bytes.len() as u64 > max {
            return Err(too_large());
        }
        return Ok(bytes);
    }
    let mut text = Vec::new();
    GzDecoder::new(&bytes[..])
        .take(max.saturating_add(1))
        .read_to_end(&mut text)
        .map_err(|e| Failure::new(FailKind::NotSvg, format!("gzip: {:?}", e.kind())))?;
    if text.len() as u64 > max {
        return Err(too_large());
    }
    Ok(text)
}

/// The SVG's text: UTF-8, a byte order mark allowed. An embedded SVG's
/// too ([`Budget`]).
pub(super) fn decode_text(bytes: &[u8]) -> Result<&str, Failure> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        return Err(Failure::new(FailKind::NotSvg, "UTF-16 text isn't read"));
    }
    let body = bytes.strip_prefix(&UTF8_BOM).unwrap_or(bytes);
    std::str::from_utf8(body).map_err(|e| {
        Failure::new(
            FailKind::NotSvg,
            format!("not UTF-8 after byte {}", e.valid_up_to()),
        )
    })
}

/// The XML, parsed with DTDs allowed, no entity resolver and at most
/// `maxNodes` nodes.
fn parse_xml<'a>(text: &'a str, limits: &Limits) -> Result<roxmltree::Document<'a>, Failure> {
    parse_svg_xml(text, limits.max_nodes).map_err(|e| xml_failure(&e))
}

/// `text` parsed as every SVG in a job is, the file's own and embedded
/// ones ([`Budget`]): DTDs allowed, no entity resolver (no `SYSTEM` entity
/// or external DTD is ever fetched), at most `nodes_limit` nodes.
pub(super) fn parse_svg_xml(
    text: &str,
    nodes_limit: u32,
) -> Result<roxmltree::Document<'_>, roxmltree::Error> {
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        nodes_limit,
        entity_resolver: None,
    };
    roxmltree::Document::parse_with_options(text, options)
}

/// True when the document can draw text: an element named `text` in the
/// SVG namespace or in none (usvg takes a missing `xmlns`), or an href
/// that embeds an SVG, which may hold text.
pub(crate) fn needs_fonts(doc: &roxmltree::Document) -> bool {
    doc.descendants()
        .filter(|node| node.is_element())
        .any(|node| {
            let name = node.tag_name();
            (name.name() == "text" && matches!(name.namespace(), None | Some(SVG_NS)))
                || node
                    .attributes()
                    .any(|attribute| attribute.name() == "href" && embeds_svg(attribute.value()))
        })
}

/// True for a `data:` URL whose MIME type is `image/svg+xml`, written any
/// way the data URL parser takes it.
fn embeds_svg(href: &str) -> bool {
    let href = href.trim_start_matches(|c: char| c.is_ascii_whitespace() || c.is_ascii_control());
    let Some(rest) = href
        .get(..5)
        .filter(|scheme| scheme.eq_ignore_ascii_case("data:"))
        .map(|_| &href[5..])
    else {
        return false;
    };
    let mime = rest.split([',', ';']).next().unwrap_or_default().trim();
    mime.eq_ignore_ascii_case("image/svg+xml")
}

/// usvg's options: no resources folder, 96 DPI, a 100 × 100 default size
/// (usvg's), `en`, no injected style sheet, `fonts`, and `budget`'s
/// data-only resolver.
fn options(budget: &Budget, fonts: fontdb::Database) -> usvg::Options<'_> {
    usvg::Options {
        resources_dir: None,
        dpi: 96.0,
        languages: vec!["en".to_string()],
        style_sheet: None,
        image_href_resolver: budget.resolver(),
        fontdb: Arc::new(fonts),
        ..usvg::Options::default()
    }
}

/// usvg's error in the worker's words.
pub(super) fn tree_failure(e: usvg::Error) -> Failure {
    match e {
        usvg::Error::ParsingFailed(e) => xml_failure(&e),
        usvg::Error::ElementsLimitReached => {
            Failure::new(FailKind::TooLarge, "SVG: over usvg's 1,000,000 elements")
        }
        usvg::Error::InvalidSize => {
            Failure::new(FailKind::NotSvg, "SVG: its width or height isn't valid")
        }
        usvg::Error::NotAnUtf8Str
        | usvg::Error::SvgzFeatureNotEnabled
        | usvg::Error::MalformedGZip => Failure::new(FailKind::NotSvg, "SVG: not readable text"),
    }
}

/// roxmltree's error by its kind and position, never with the names or
/// text it quotes. Over a limit is `tooLarge`; the rest `notSvg`.
pub(super) fn xml_failure(e: &roxmltree::Error) -> Failure {
    use roxmltree::Error as E;
    let what = match e {
        E::NodesLimitReached => {
            return Failure::new(
                FailKind::TooLarge,
                "XML: too many nodes or too deeply nested",
            )
        }
        E::AttributesLimitReached => {
            return Failure::new(FailKind::TooLarge, "XML: too many attributes")
        }
        E::NamespacesLimitReached => {
            return Failure::new(FailKind::TooLarge, "XML: too many namespaces")
        }
        E::InvalidXmlPrefixUri(_) => "the xml prefix has the wrong URI",
        E::UnexpectedXmlUri(_) => "the xml namespace URI on another prefix",
        E::UnexpectedXmlnsUri(_) => "the xmlns URI declared",
        E::InvalidElementNamePrefix(_) => "an xmlns element prefix",
        E::DuplicatedNamespace(..) => "a namespace defined twice",
        E::UnknownNamespace(..) => "an unknown namespace prefix",
        E::UnexpectedCloseTag(..) => "an unexpected close tag",
        E::UnexpectedEntityCloseTag(_) => "an entity that starts with a close tag",
        E::UnknownEntityReference(..) => "an unknown entity reference",
        E::MalformedEntityReference(_) => "a malformed entity reference",
        E::EntityReferenceLoop(_) => "an entity reference loop",
        E::InvalidAttributeValue(_) => "a '<' in an attribute value",
        E::DuplicatedAttribute(..) => "an attribute defined twice",
        E::NoRootNode => "no root element",
        E::UnclosedRootNode => "the root element is never closed",
        E::UnexpectedDeclaration(_) => "an XML declaration out of place",
        E::DtdDetected => "a DTD",
        E::InvalidName(_) => "an invalid name",
        E::NonXmlChar(..) => "a character XML doesn't allow",
        E::InvalidChar(..) | E::InvalidChar2(..) => "an unexpected character",
        E::InvalidString(..) => "an unexpected string",
        E::InvalidExternalID(_) => "an invalid external ID",
        E::EntityResolver(..) => "an external entity",
        E::InvalidComment(_) => "an invalid comment",
        E::InvalidCharacterData(_) => "invalid character data",
        E::UnknownToken(_) => "an unknown token",
        E::UnexpectedEndOfStream => "an unexpected end of the text",
    };
    let pos = e.pos();
    Failure::new(
        FailKind::NotSvg,
        format!("XML: {what} at {}:{}", pos.row, pos.col),
    )
}

/// Writes `rgba` (straight alpha, `width` × `height`) to a new
/// `pixels.rgba` in `work_dir` as `channels` (3: alpha dropped, or 4),
/// row by row, then syncs it.
fn write_pixels(
    work_dir: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
    channels: u8,
) -> io::Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(work_dir.join(PIXELS))?;
    let out = BufWriter::with_capacity(WRITE_BUFFER, file);
    let header = PixelHeader {
        width,
        height,
        channels,
        icc_len: 0,
    };
    let mut writer = PixelWriter::begin(out, &header, None)?;
    let row_len = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut rgb_row = Vec::new();
    for row in rgba.chunks_exact(row_len) {
        if channels == 4 {
            writer.row(row)?;
        } else {
            rgb_row.clear();
            for pixel in row.as_chunks::<4>().0 {
                rgb_row.extend_from_slice(&pixel[..3]);
            }
            writer.row(&rgb_row)?;
        }
    }
    let out = writer.finish()?;
    let file = out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()
}

fn internal(detail: impl Into<String>) -> Failure {
    Failure::new(FailKind::Internal, detail)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use grape_heif_proto::{read_pixels, PixelLimits};

    use super::super::protocol::Mode;
    use super::*;
    use crate::output::unique_suffix;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            let path = std::env::temp_dir().join(format!("grape-svg-unit-{}", unique_suffix()));
            fs::create_dir_all(path.join("work")).unwrap();
            TestDir(path)
        }

        fn work(&self) -> PathBuf {
            self.0.join("work")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut out = GzEncoder::new(Vec::new(), Compression::fast());
        out.write_all(bytes).unwrap();
        out.finish().unwrap()
    }

    fn failure(result: Result<impl std::fmt::Debug, Failure>) -> Failure {
        result.unwrap_err()
    }

    /// Runs `serve` in this process on `bytes` saved as the source, and
    /// returns its lines and whether it said `done`.
    fn serve_on(dir: &TestDir, bytes: &[u8], limits: Limits) -> (bool, Vec<Message>) {
        let src = dir.0.join(format!("in-{}.svg", unique_suffix()));
        fs::write(&src, bytes).unwrap();
        let request = Request {
            protocol: PROTOCOL,
            source: src,
            work_dir: dir.work(),
            mode: Mode::Raster,
            limits,
        };
        serve_request(&serde_json::to_vec(&request).unwrap())
    }

    fn serve_request(request: &[u8]) -> (bool, Vec<Message>) {
        let mut out = Vec::new();
        let done = serve(Cursor::new(request.to_vec()), &mut out);
        let lines = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        (done, lines)
    }

    fn last_failure(lines: &[Message]) -> Failure {
        match lines.last() {
            Some(Message::Error(failure)) => failure.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_small_svg_is_drawn_twice_its_size_into_the_pixel_file() {
        let dir = TestDir::new();
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
            <rect width="24" height="12" fill="#ff0000"/></svg>"##;
        let (done, lines) = serve_on(&dir, svg, Limits::DEFAULT);
        assert!(done, "{lines:?}");
        assert_eq!(
            lines,
            [
                Message::Progress(0.2),
                Message::Progress(0.3),
                Message::Image(ImageInfo {
                    width: 48,
                    height: 48,
                    css_width: 24.0,
                    css_height: 24.0,
                    scaled: false,
                }),
                Message::Progress(0.9),
                Message::Done(Done {
                    width: 48,
                    height: 48,
                    channels: 4,
                    left_out: 0,
                    scaled: false,
                }),
            ]
        );
        let path = dir.work().join(PIXELS);
        let len = fs::metadata(&path).unwrap().len();
        let limits = PixelLimits {
            max_pixels: 48 * 48,
            max_side: 48,
            max_icc_bytes: 0,
        };
        let pixels = read_pixels(File::open(&path).unwrap(), len, &limits).unwrap();
        assert_eq!((pixels.width, pixels.height, pixels.channels), (48, 48, 4));
        // The top half red, the bottom transparent.
        assert_eq!(&pixels.data[..4], &[255, 0, 0, 255]);
        let last = pixels.data.len() - 4;
        assert_eq!(&pixels.data[last..], &[0, 0, 0, 0]);
    }

    #[test]
    fn an_opaque_drawing_is_written_as_rgb() {
        let dir = TestDir::new();
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="200">
            <rect width="300" height="200" fill="rgb(0,128,255)"/></svg>"#;
        let (done, lines) = serve_on(&dir, svg, Limits::DEFAULT);
        assert!(done, "{lines:?}");
        match lines.last() {
            Some(Message::Done(done)) => {
                assert_eq!((done.width, done.height, done.channels), (300, 200, 3));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_source_is_held_to_its_limit_unread() {
        let limits = Limits {
            max_source_bytes: 100,
            ..Limits::DEFAULT
        };
        let dir = TestDir::new();
        let src = dir.0.join("big.svg");
        fs::write(&src, vec![b' '; 101]).unwrap();
        assert_eq!(failure(read_source(&src, &limits)).kind, FailKind::TooLarge);
        fs::write(&src, vec![b' '; 100]).unwrap();
        assert_eq!(read_source(&src, &limits).unwrap().len(), 100);
        let gone = dir.0.join("gone.svg");
        assert_eq!(failure(read_source(&gone, &limits)).kind, FailKind::Read);
    }

    #[test]
    fn gzip_is_inflated_within_its_bound() {
        let limits = Limits {
            max_text_bytes: 1000,
            ..Limits::DEFAULT
        };
        assert_eq!(gunzip(gzip(&[b'a'; 1000]), &limits).unwrap().len(), 1000);
        // One byte more is too large; so is plain text over the bound.
        assert_eq!(
            failure(gunzip(gzip(&[b'a'; 1001]), &limits)).kind,
            FailKind::TooLarge
        );
        assert_eq!(
            failure(gunzip(vec![b'a'; 1001], &limits)).kind,
            FailKind::TooLarge
        );
        // A bad stream isn't an SVG.
        let mut bad = gzip(b"<svg/>");
        let at = bad.len() - 12;
        bad[at] ^= 0xff;
        assert_eq!(failure(gunzip(bad, &limits)).kind, FailKind::NotSvg);
        assert_eq!(
            failure(gunzip(vec![0x1f, 0x8b, 1, 2], &limits)).kind,
            FailKind::NotSvg
        );
    }

    #[test]
    fn sixty_four_mib_and_one_byte_of_gzip_is_too_large() {
        let dir = TestDir::new();
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        let zeros = vec![b' '; 1 << 20];
        for _ in 0..64 {
            gz.write_all(&zeros).unwrap();
        }
        gz.write_all(b" ").unwrap();
        let (done, lines) = serve_on(&dir, &gz.finish().unwrap(), Limits::DEFAULT);
        assert!(!done);
        assert_eq!(last_failure(&lines).kind, FailKind::TooLarge);
        assert!(!dir.work().join(PIXELS).exists());
    }

    #[test]
    fn only_utf8_text_is_read() {
        assert_eq!(decode_text(b"<svg/>").unwrap(), "<svg/>");
        assert_eq!(decode_text(b"\xef\xbb\xbf<svg/>").unwrap(), "<svg/>");
        for bad in [
            &b"\xff\xfe<\0s\0v\0g\0/\0>\0"[..],
            &b"\xfe\xff\0<\0s\0v\0g\0/\0>"[..],
            &b"<svg>\xff</svg>"[..],
        ] {
            assert_eq!(failure(decode_text(bad)).kind, FailKind::NotSvg);
        }
    }

    #[test]
    fn the_standard_doctype_parses_and_external_entities_are_never_read() {
        let doctype = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#;
        assert!(parse_xml(doctype, &Limits::DEFAULT).is_ok());
        // An Illustrator-style internal entity is expanded.
        let entity = r#"<!DOCTYPE svg [<!ENTITY ns_svg "http://www.w3.org/2000/svg">]>
<svg xmlns="&ns_svg;" width="10" height="10"/>"#;
        assert!(parse_xml(entity, &Limits::DEFAULT).is_ok());
        // An external one is never fetched: the reference is unknown.
        let dir = TestDir::new();
        let secret = dir.0.join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        let xxe = format!(
            r#"<!DOCTYPE svg [<!ENTITY x SYSTEM "file://{}">]>
<svg xmlns="http://www.w3.org/2000/svg"><text>&x;</text></svg>"#,
            secret.display()
        );
        let failed = failure(parse_xml(&xxe, &Limits::DEFAULT));
        assert_eq!(failed.kind, FailKind::NotSvg);
        assert!(
            failed
                .detail
                .starts_with("XML: an unknown entity reference at 2:"),
            "{}",
            failed.detail
        );
        assert!(!failed.detail.contains("secret"), "{}", failed.detail);
    }

    #[test]
    fn xml_errors_say_where_but_never_what() {
        let failed = failure(parse_xml(
            "<svg xmlns='http://www.w3.org/2000/svg'><g></private-name></svg>",
            &Limits::DEFAULT,
        ));
        assert_eq!(failed.kind, FailKind::NotSvg);
        assert!(failed
            .detail
            .starts_with("XML: an unexpected close tag at 1:"));
        assert!(!failed.detail.contains("private"), "{}", failed.detail);
        let failed = failure(parse_xml("<svg", &Limits::DEFAULT));
        assert_eq!(failed.kind, FailKind::NotSvg);
    }

    /// An SVG of `nodes` XML nodes, roxmltree's count: the document, the
    /// `<svg>` element and `nodes - 2` children.
    fn nodes(nodes: usize) -> String {
        let mut text = String::from("<svg xmlns='http://www.w3.org/2000/svg'>");
        text.push_str(&"<g/>".repeat(nodes - 2));
        text.push_str("</svg>");
        text
    }

    #[test]
    fn two_million_and_one_nodes_is_too_large() {
        assert_eq!(Limits::DEFAULT.max_nodes, 2_000_000);
        assert_eq!(
            failure(parse_xml(&nodes(2_000_001), &Limits::DEFAULT)).kind,
            FailKind::TooLarge
        );
        // The bound is exact.
        let limits = Limits {
            max_nodes: 10,
            ..Limits::DEFAULT
        };
        assert!(parse_xml(&nodes(10), &limits).is_ok());
        assert_eq!(
            failure(parse_xml(&nodes(11), &limits)).kind,
            FailKind::TooLarge
        );
    }

    #[test]
    fn text_decides_whether_fonts_load() {
        let needs = |text: &str| needs_fonts(&parse_xml(text, &Limits::DEFAULT).unwrap());
        assert!(needs(
            "<svg xmlns='http://www.w3.org/2000/svg'><text>Hi</text></svg>"
        ));
        assert!(needs(
            "<s:svg xmlns:s='http://www.w3.org/2000/svg'><s:g><s:text>Hi</s:text></s:g></s:svg>"
        ));
        // No namespace at all: usvg draws it.
        assert!(needs("<svg><text>Hi</text></svg>"));
        // `text` in another namespace isn't SVG's.
        assert!(!needs(
            "<svg xmlns='http://www.w3.org/2000/svg' xmlns:o='urn:other'><o:text>Hi</o:text></svg>"
        ));
        assert!(!needs(
            "<svg xmlns='http://www.w3.org/2000/svg'><rect/><textPath/></svg>"
        ));
        // A nested SVG may hold text, however its data URL is written.
        for href in [
            "data:image/svg+xml;base64,PHN2Zy8+",
            "data:image/svg+xml,%3Csvg/%3E",
            "  DATA:Image/SVG+XML ;utf8,%3Csvg/%3E",
        ] {
            let text = format!(
                "<svg xmlns='http://www.w3.org/2000/svg' xmlns:xlink='http://www.w3.org/1999/xlink'><image xlink:href='{href}'/></svg>"
            );
            assert!(needs(&text), "{href}");
            assert!(needs(&text.replace("xlink:href", "href")), "{href}");
        }
        for href in ["data:image/png;base64,AAAA", "logo.svg", "data:,svg"] {
            let text =
                format!("<svg xmlns='http://www.w3.org/2000/svg'><image href='{href}'/></svg>");
            assert!(!needs(&text), "{href}");
        }
    }

    #[test]
    fn requests_are_checked() {
        let dir = TestDir::new();
        let src = dir.0.join("a.svg");
        fs::write(&src, "<svg/>").unwrap();
        let good = Request {
            protocol: PROTOCOL,
            source: src.clone(),
            work_dir: dir.work(),
            mode: Mode::Raster,
            limits: Limits::DEFAULT,
        };
        let failed = |request: &Request| {
            let (done, lines) = serve_request(&serde_json::to_vec(request).unwrap());
            assert!(!done);
            assert_eq!(lines.len(), 1, "{lines:?}");
            last_failure(&lines)
        };
        // Another version is refused as that, even with fields this one
        // doesn't know.
        let other = r#"{"protocol":2,"source":"/a.svg","workDir":"/w","mode":"raster","new":1}"#;
        let (done, lines) = serve_request(other.as_bytes());
        assert!(!done);
        assert_eq!(last_failure(&lines).kind, FailKind::Protocol);
        for request in [
            Request {
                source: PathBuf::from("a.svg"),
                ..good.clone()
            },
            Request {
                source: dir.work(),
                ..good.clone()
            },
            Request {
                work_dir: src.clone(),
                ..good.clone()
            },
            Request {
                work_dir: PathBuf::from("work"),
                ..good.clone()
            },
            Request {
                limits: Limits {
                    max_nodes: 0,
                    ..Limits::DEFAULT
                },
                ..good.clone()
            },
        ] {
            assert_eq!(failed(&request).kind, FailKind::Internal, "{request:?}");
        }
        // Bad JSON is reported by position, never quoted.
        let (done, lines) = serve_request(b"{\"protocol\":1,\"secret-name\":");
        assert!(!done);
        let failure = last_failure(&lines);
        assert_eq!(failure.kind, FailKind::Internal);
        assert!(!failure.detail.contains("secret"), "{}", failure.detail);
        let (done, lines) = serve_request(&vec![b' '; 70 * 1024]);
        assert!(!done);
        assert_eq!(last_failure(&lines).detail, "the request is too long");
    }

    #[test]
    fn usvg_errors_are_not_svg_or_too_large() {
        let dir = TestDir::new();
        for (svg, kind) in [
            (
                &br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="10"/>"#[..],
                FailKind::NotSvg,
            ),
            (&b"<html><body/></html>"[..], FailKind::NotSvg),
            (&b"not xml at all"[..], FailKind::NotSvg),
        ] {
            let (done, lines) = serve_on(&dir, svg, Limits::DEFAULT);
            assert!(!done);
            assert_eq!(last_failure(&lines).kind, kind, "{lines:?}");
        }
        // usvg's own depth limit: more than 1,024 nested groups.
        let mut deep = String::from("<svg xmlns='http://www.w3.org/2000/svg'>");
        deep.push_str(&"<g>".repeat(1100));
        deep.push_str(&"</g>".repeat(1100));
        deep.push_str("</svg>");
        let (done, lines) = serve_on(&dir, deep.as_bytes(), Limits::DEFAULT);
        assert!(!done);
        assert_eq!(last_failure(&lines).kind, FailKind::TooLarge);
        assert!(!dir.work().join(PIXELS).exists());
    }

    #[test]
    fn an_embedded_svgs_links_are_counted_in_done() {
        let dir = TestDir::new();
        // As design tools nest an export: percent-encoded, linking a file.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="300">
            <image href="data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='50'%20height='50'%3E%3Cimage%20href='/no/such/photo.png'%20width='50'%20height='50'/%3E%3C/svg%3E" width="50" height="50"/></svg>"#;
        let (done, lines) = serve_on(&dir, svg, Limits::DEFAULT);
        assert!(done, "{lines:?}");
        match lines.last() {
            Some(Message::Done(done)) => assert_eq!(done.left_out, 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_embedded_svg_past_the_node_limit_is_left_out() {
        // The file is 3 nodes (the document, `svg`, `image`), so a limit of
        // 8 leaves 5 for the embedded SVG: 2 plus its groups.
        let with_groups = |groups: usize| {
            format!(
                "<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'><image href=\"data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='10'%20height='10'%3E{}%3C/svg%3E\" width='10' height='10'/></svg>",
                "%3Cg/%3E".repeat(groups)
            )
        };
        let limits = Limits {
            max_nodes: 8,
            ..Limits::DEFAULT
        };
        let dir = TestDir::new();
        for (groups, left_out) in [(3, 0), (4, 1)] {
            let (done, lines) = serve_on(&dir, with_groups(groups).as_bytes(), limits);
            assert!(done, "{groups}: {lines:?}");
            match lines.last() {
                Some(Message::Done(done)) => assert_eq!(done.left_out, left_out, "{groups}"),
                other => panic!("{other:?}"),
            }
            fs::remove_file(dir.work().join(PIXELS)).unwrap();
        }
    }

    #[test]
    fn refused_references_are_counted_in_done() {
        let dir = TestDir::new();
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="300">
            <image href="/no/such/a.png" width="10" height="10"/>
            <image href="/no/such/a.png" width="10" height="10"/>
            <image href="b.png" width="10" height="10"/></svg>"#;
        let (done, lines) = serve_on(&dir, svg, Limits::DEFAULT);
        assert!(done, "{lines:?}");
        match lines.last() {
            Some(Message::Done(done)) => assert_eq!(done.left_out, 2),
            other => panic!("{other:?}"),
        }
    }
}
