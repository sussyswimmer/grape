//! The PDF engine end to end, with a real PDFium (feature `pdf-it`,
//! DECISIONS D29). `GRAPE_TEST_PDFIUM` must name the PDFium library
//! (`pdfium.dll`, `libpdfium.dylib`, `libpdfium.so`): without it these
//! tests fail rather than skip. The worker is this package's
//! `grape-pdf-worker` binary. Every PDF is generated here with lopdf;
//! every test checks that the sources are unchanged and that no temp file
//! or folder is left.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{assert_no_leftovers, names_in, plan_one, SourceState, TestDir};
use grape_core::{plan, Format, Job, JobOutput, Operation, Settings, ToolKey};
use grape_engines::pdf::it::{self, Options, Outcome};
use grape_engines::{build_selection, EngineCtx, ErrorKind, GrapeError};
use lopdf::content::{Content, Operation as Op};
use lopdf::{dictionary, Document, Object, Stream};

const WORKER: &str = env!("CARGO_BIN_EXE_grape-pdf-worker");

fn pdfium() -> PathBuf {
    let path = std::env::var_os("GRAPE_TEST_PDFIUM")
        .map(PathBuf::from)
        .expect("set GRAPE_TEST_PDFIUM to the PDFium library to run the pdf-it tests");
    assert!(
        path.is_file(),
        "GRAPE_TEST_PDFIUM is not a file: {}",
        path.display()
    );
    path
}

/// A line of text: `size` points at (`x`, `y`).
struct Line<'a> {
    text: &'a str,
    size: i64,
    x: i64,
    y: i64,
}

fn line(text: &str, size: i64, y: i64) -> Line<'_> {
    Line {
        text,
        size,
        x: 72,
        y,
    }
}

/// One page of `width` × `height` points holding `lines` in Helvetica.
struct Page<'a> {
    width: i64,
    height: i64,
    rotate: i64,
    lines: Vec<Line<'a>>,
}

fn letter(lines: Vec<Line<'_>>) -> Page<'_> {
    Page {
        width: 612,
        height: 792,
        rotate: 0,
        lines,
    }
}

/// A PDF with these pages, saved with a classic cross-reference table.
fn pdf(pages: &[Page]) -> Document {
    let mut doc = Document::with_version("1.7");
    let root = doc.new_object_id();
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources = doc.add_object(dictionary! {"Font" => dictionary! {"F1" => font}});
    let mut kids = Vec::new();
    for page in pages {
        let mut operations = Vec::new();
        for line in &page.lines {
            operations.extend([
                Op::new("BT", vec![]),
                Op::new("Tf", vec!["F1".into(), line.size.into()]),
                Op::new("Td", vec![line.x.into(), line.y.into()]),
                Op::new("Tj", vec![Object::string_literal(line.text)]),
                Op::new("ET", vec![]),
            ]);
        }
        let content = Content { operations }.encode().unwrap();
        let content = doc.add_object(Stream::new(dictionary! {}, content));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => root, "Contents" => content,
            "MediaBox" => vec![0.into(), 0.into(), page.width.into(), page.height.into()],
            "Rotate" => page.rotate,
        });
        kids.push(Object::Reference(page_id));
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        root,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => kids, "Count" => count, "Resources" => resources,
        }),
    );
    let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => root});
    let info = doc.add_object(dictionary! {
        "Title" => Object::string_literal("MARK-TITLE"),
        "Author" => Object::string_literal("MARK-AUTHOR"),
    });
    doc.trailer.set("Root", catalog);
    doc.trailer.set("Info", info);
    doc
}

fn bytes(doc: &Document) -> Vec<u8> {
    let mut doc = doc.clone();
    doc.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

/// `n` letter pages saying "Page {i}".
fn numbered(n: usize) -> Vec<u8> {
    let texts: Vec<String> = (1..=n).map(|i| format!("Page {i}")).collect();
    let pages: Vec<Page> = texts
        .iter()
        .map(|text| letter(vec![line(text, 24, 700)]))
        .collect();
    bytes(&pdf(&pages))
}

/// Runs `job` with Grape's bounds; also returns the progress seen.
fn run(job: &Job, options: Options) -> (Result<Outcome, GrapeError>, Vec<f32>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let mut progress = move |fraction: f32| log.lock().unwrap().push(fraction);
    let result = it::run(
        job,
        &EngineCtx::default(),
        &mut progress,
        Path::new(WORKER),
        &pdfium(),
        options,
    );
    let seen = seen.lock().unwrap().clone();
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
    (result, seen)
}

fn image_size(path: &Path) -> (u32, u32) {
    let image = image::open(path).unwrap();
    (image.width(), image.height())
}

#[test]
fn one_page_renders_to_a_file_at_300_dpi() {
    for (to, ext) in [(Format::Jpg, "jpg"), (Format::Png, "png")] {
        let dir = TestDir::new("pdf-render-one");
        let (src, job) = plan_one(&dir, "doc.pdf", &numbered(1), Operation::convert(to));
        let source = SourceState::of(&src);
        let (result, progress) = run(&job, Options::default());
        let outcome = result.unwrap();
        let file = dir.join(&format!("doc.{ext}"));
        assert_eq!(outcome.outputs, std::slice::from_ref(&file));
        assert_eq!(outcome.pages, 1);
        assert!(outcome.scaled_pages.is_empty());
        assert_eq!(image_size(&file), (2550, 3300));
        assert_eq!(progress.last(), Some(&1.0));
        let mut expected = vec!["doc.pdf".to_string(), format!("doc.{ext}")];
        expected.sort();
        assert_eq!(names_in(&dir.path), expected);
        source.assert_unchanged();
        assert_no_leftovers(&dir.path);
    }
}

#[test]
fn several_pages_render_into_a_folder_and_rotation_is_honoured() {
    let dir = TestDir::new("pdf-render-many");
    let mut pages = vec![
        letter(vec![line("One", 24, 700)]),
        letter(vec![line("Two", 24, 700)]),
        letter(vec![line("Three", 24, 700)]),
    ];
    pages[1].rotate = 90;
    pages[2].width = 300;
    pages[2].height = 144;
    let (src, job) = plan_one(
        &dir,
        "doc.pdf",
        &bytes(&pdf(&pages)),
        Operation::convert(Format::Png),
    );
    let source = SourceState::of(&src);
    let (result, _) = run(&job, Options::default());
    let outcome = result.unwrap();
    let folder = dir.join("doc");
    assert_eq!(outcome.outputs, std::slice::from_ref(&folder));
    assert_eq!(outcome.pages, 3);
    assert_eq!(names_in(&folder), ["doc 1.png", "doc 2.png", "doc 3.png"]);
    assert_eq!(image_size(&folder.join("doc 1.png")), (2550, 3300));
    assert_eq!(image_size(&folder.join("doc 2.png")), (3300, 2550));
    assert_eq!(image_size(&folder.join("doc 3.png")), (1250, 600));
    // The page is white where there is no text.
    let page = image::open(folder.join("doc 1.png")).unwrap().to_rgb8();
    assert_eq!(page.get_pixel(5, 5).0, [255, 255, 255]);
    source.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_huge_page_is_drawn_smaller() {
    let dir = TestDir::new("pdf-render-huge");
    let (src, job) = plan_one(
        &dir,
        "doc.pdf",
        &numbered(1),
        Operation::convert(Format::Jpg),
    );
    let source = SourceState::of(&src);
    let options = Options {
        max_page_pixels: 1_000_000,
        ..Options::default()
    };
    let (result, _) = run(&job, options);
    let outcome = result.unwrap();
    assert_eq!(outcome.scaled_pages, [1]);
    let (w, h) = image_size(&dir.join("doc.jpg"));
    assert!(u64::from(w) * u64::from(h) <= 1_000_000, "{w}×{h}");
    assert!(w < 2550 && h < 3300);
    // The aspect ratio is kept.
    assert!((f64::from(w) / f64::from(h) - 612.0 / 792.0).abs() < 0.01);
    source.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn too_many_pages_are_refused() {
    let dir = TestDir::new("pdf-render-many-pages");
    let (src, job) = plan_one(
        &dir,
        "doc.pdf",
        &numbered(3),
        Operation::convert(Format::Png),
    );
    let source = SourceState::of(&src);
    let options = Options {
        max_pages: 2,
        ..Options::default()
    };
    let (result, _) = run(&job, options);
    let error = result.unwrap_err();
    assert_eq!(
        error.user_message,
        "doc.pdf has more than 2 pages, more than Grape exports as images."
    );
    assert_eq!(names_in(&dir.path), ["doc.pdf"]);
    source.assert_unchanged();
}

#[test]
fn text_comes_out_as_txt_and_md() {
    let pages = [
        letter(vec![
            line("Annual Report", 28, 720),
            line("Introduction", 18, 680),
            line("This is the first line of body text,", 11, 650),
            line("and this is the second line of it.", 11, 636),
            line("A new paragraph starts after a gap.", 11, 600),
        ]),
        letter(vec![line(
            "Second page body text, more words here.",
            11,
            700,
        )]),
    ];
    let source_bytes = bytes(&pdf(&pages));
    for to in [Format::Txt, Format::Md] {
        let dir = TestDir::new("pdf-text");
        let (src, job) = plan_one(&dir, "doc.pdf", &source_bytes, Operation::convert(to));
        let source = SourceState::of(&src);
        let (result, _) = run(&job, Options::default());
        let outcome = result.unwrap();
        let file = dir.join(&format!("doc.{}", to.output_ext()));
        assert_eq!(outcome.outputs, std::slice::from_ref(&file));
        let text = fs::read_to_string(&file).unwrap();
        assert!(!text.starts_with('\u{feff}'));
        if to == Format::Txt {
            assert!(text.contains("Annual Report"), "{text:?}");
            assert!(
                text.contains("first line of body text,\nand this is"),
                "{text:?}"
            );
            assert!(
                text.contains("after a gap.\n\nSecond page body text"),
                "{text:?}"
            );
        } else {
            assert!(
                text.starts_with("# Annual Report\n\n## Introduction\n\n"),
                "{text:?}"
            );
            assert!(
                text.contains("first line of body text, and this is the second line of it.\n\n"),
                "{text:?}"
            );
            assert!(
                text.contains("\n\nA new paragraph starts after a gap.\n\n"),
                "{text:?}"
            );
            assert!(
                text.ends_with("Second page body text, more words here.\n"),
                "{text:?}"
            );
        }
        source.assert_unchanged();
        assert_no_leftovers(&dir.path);
    }
}

#[test]
fn a_pdf_without_text_is_refused() {
    let dir = TestDir::new("pdf-no-text");
    let (src, job) = plan_one(
        &dir,
        "scan.pdf",
        &bytes(&pdf(&[letter(vec![])])),
        Operation::convert(Format::Txt),
    );
    let source = SourceState::of(&src);
    let (result, _) = run(&job, Options::default());
    assert_eq!(
        result.unwrap_err().user_message,
        "scan.pdf has no text to extract — it may be a scan."
    );
    assert_eq!(names_in(&dir.path), ["scan.pdf"]);
    source.assert_unchanged();
}

/// `doc` encrypted (RC4 128) with these passwords.
fn encrypted(doc: &Document, user_password: &str) -> Vec<u8> {
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    let mut doc = doc.clone();
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
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

#[test]
fn a_password_protected_pdf_is_refused_and_an_owner_only_one_renders() {
    let doc = pdf(&[letter(vec![line("Secret", 24, 700)])]);
    let dir = TestDir::new("pdf-encrypted");
    let (src, job) = plan_one(
        &dir,
        "locked.pdf",
        &encrypted(&doc, "user"),
        Operation::convert(Format::Png),
    );
    let source = SourceState::of(&src);
    let (result, _) = run(&job, Options::default());
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::DecodeFailed);
    assert_eq!(error.user_message, "locked.pdf is password-protected.");
    assert_eq!(names_in(&dir.path), ["locked.pdf"]);
    source.assert_unchanged();

    let dir = TestDir::new("pdf-owner-only");
    let (src, job) = plan_one(
        &dir,
        "open.pdf",
        &encrypted(&doc, ""),
        Operation::convert(Format::Png),
    );
    let source = SourceState::of(&src);
    let (result, _) = run(&job, Options::default());
    assert_eq!(result.unwrap().outputs, [dir.join("open.png")]);
    source.assert_unchanged();
    assert_no_leftovers(&dir.path);

    // The lopdf tools refuse both: a rewrite would drop the restrictions.
    let dir = TestDir::new("pdf-owner-only-split");
    let (src, job) = plan_one(
        &dir,
        "open.pdf",
        &encrypted(&doc, ""),
        Operation::tool(ToolKey::SplitPdf),
    );
    let source = SourceState::of(&src);
    let (result, _) = run(&job, Options::default());
    assert_eq!(
        result.unwrap_err().user_message,
        "open.pdf is password-protected."
    );
    source.assert_unchanged();
    assert_eq!(names_in(&dir.path), ["open.pdf"]);
}

/// Two letter pages saved with a cross-reference stream and a startxref
/// that points nowhere: lopdf can't recover that, PDFium rebuilds it.
fn unreadable_for_lopdf() -> Vec<u8> {
    let mut doc = pdf(&[
        letter(vec![line("First", 24, 700)]),
        letter(vec![line("Second", 24, 700)]),
    ]);
    let mut saved = Vec::new();
    doc.save_with_options(
        &mut saved,
        lopdf::SaveOptions {
            use_object_streams: false,
            use_xref_streams: true,
            ..Default::default()
        },
    )
    .unwrap();
    let at = saved.windows(9).rposition(|w| w == b"startxref").unwrap();
    saved.truncate(at);
    saved.extend_from_slice(b"startxref\n12\n%%EOF\n");
    assert!(
        Document::load_mem(&saved).is_err()
            || Document::load_mem(&saved).unwrap().get_pages().is_empty()
    );
    saved
}

#[test]
fn split_falls_back_to_pdfium_when_lopdf_cannot_read_the_file() {
    let saved = unreadable_for_lopdf();
    let dir = TestDir::new("pdf-split-fallback");
    let (src, job) = plan_one(&dir, "doc.pdf", &saved, Operation::tool(ToolKey::SplitPdf));
    let source = SourceState::of(&src);
    let (result, _) = run(&job, Options::default());
    let outcome = result.unwrap();
    assert!(outcome.used_pdfium);
    let folder = dir.join("doc Pages");
    assert_eq!(outcome.outputs, std::slice::from_ref(&folder));
    assert_eq!(names_in(&folder), ["doc 1.pdf", "doc 2.pdf"]);
    for name in ["doc 1.pdf", "doc 2.pdf"] {
        let page = Document::load(folder.join(name)).unwrap();
        assert_eq!(page.get_pages().len(), 1);
    }
    source.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn split_holds_its_page_bound_in_the_pdfium_fallback_too() {
    let dir = TestDir::new("pdf-split-fallback-bound");
    let (src, job) = plan_one(
        &dir,
        "doc.pdf",
        &unreadable_for_lopdf(),
        Operation::tool(ToolKey::SplitPdf),
    );
    let source = SourceState::of(&src);
    let options = Options {
        max_split_pages: 1,
        ..Options::default()
    };
    let (result, _) = run(&job, options);
    assert_eq!(
        result.unwrap_err().user_message,
        "doc.pdf has more than 1 pages, more than Grape splits."
    );
    assert_eq!(names_in(&dir.path), ["doc.pdf"]);
    source.assert_unchanged();
}

/// `doc` with only an owner password, saved with a classic table whose
/// every entry is 3 bytes off: lopdf loses its objects (so split and merge
/// fall back to PDFium), PDFium rebuilds the table and opens it.
fn owner_only_with_bad_offsets(doc: &Document) -> Vec<u8> {
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    let mut doc = doc.clone();
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
    let bytes = bytes(&doc);
    let table = bytes.windows(6).rposition(|w| w == b"\nxref\n").unwrap();
    let mut out = bytes[..table].to_vec();
    let tail = std::str::from_utf8(&bytes[table..]).unwrap();
    for entry in tail.split_inclusive('\n') {
        match entry.get(..10).filter(|_| entry.ends_with(" 00000 n \n")) {
            Some(offset) => {
                let offset: usize = offset.parse().unwrap();
                out.extend_from_slice(format!("{:010}{}", offset + 3, &entry[10..]).as_bytes());
            }
            None => out.extend_from_slice(entry.as_bytes()),
        }
    }
    assert_ne!(out, bytes);
    out
}

#[test]
fn the_pdfium_fallbacks_refuse_an_owner_password_file() {
    let doc = pdf(&[letter(vec![line("Secret", 24, 700)])]);
    let locked = owner_only_with_bad_offsets(&doc);

    // PDFium opens it (it only has an owner password)...
    let dir = TestDir::new("pdf-owner-fallback-text");
    let (_, job) = plan_one(&dir, "o.pdf", &locked, Operation::convert(Format::Txt));
    let (result, _) = run(&job, Options::default());
    let text = fs::read_to_string(&result.unwrap().outputs[0]).unwrap();
    assert_eq!(text, "Secret\n");

    // ...but split and merge would write it again without its
    // restrictions.
    let dir = TestDir::new("pdf-owner-fallback-split");
    let (src, job) = plan_one(&dir, "o.pdf", &locked, Operation::tool(ToolKey::SplitPdf));
    let source = SourceState::of(&src);
    let (result, _) = run(&job, Options::default());
    assert_eq!(
        result.unwrap_err().user_message,
        "o.pdf is password-protected."
    );
    assert_eq!(names_in(&dir.path), ["o.pdf"]);
    source.assert_unchanged();

    let dir = TestDir::new("pdf-owner-fallback-merge");
    let a = dir.join("a.pdf");
    let o = dir.join("o.pdf");
    fs::write(&a, numbered(1)).unwrap();
    fs::write(&o, &locked).unwrap();
    let sources = [SourceState::of(&a), SourceState::of(&o)];
    let settings = Settings::new(dir.join("fallback"));
    let selection = build_selection(&[a.clone(), o.clone()], &settings);
    let mut jobs = plan(&selection, &Operation::tool(ToolKey::MergePdf), &settings)
        .unwrap()
        .jobs;
    let (result, _) = run(&jobs.remove(0), Options::default());
    assert_eq!(
        result.unwrap_err().user_message,
        "o.pdf is password-protected."
    );
    assert_eq!(names_in(&dir.path), ["a.pdf", "o.pdf"]);
    for source in &sources {
        source.assert_unchanged();
    }
}

/// A one-page PDF of 612 × 792 points whose content is `content`, with
/// Helvetica as `/F1`.
fn raw_page(content: &[u8]) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let root = doc.new_object_id();
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let content = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => root, "Contents" => content,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => dictionary! {"Font" => dictionary! {"F1" => font}},
    });
    doc.objects.insert(
        root,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
        }),
    );
    let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => root});
    doc.trailer.set("Root", catalog);
    bytes(&doc)
}

#[test]
fn turned_text_keeps_its_size_and_stays_whole_in_md() {
    // A title, two body lines, an arXiv-style stamp turned 90° through the
    // text matrix, and a line upside down.
    let content = b"BT /F1 20 Tf 72 720 Td (Title Here) Tj ET \
        BT /F1 10 Tf 72 690 Td (Body text line one.) Tj ET \
        BT /F1 10 Tf 72 678 Td (Body text line two.) Tj ET \
        BT /F1 20 Tf 0 1 -1 0 30 200 Tm (arXiv:2401.00001v1 [cs.LG] 1 Jan 2024) Tj ET \
        BT /F1 10 Tf -1 0 0 -1 400 400 Tm (Upside down) Tj ET";
    let dir = TestDir::new("pdf-turned-text");
    let (src, job) = plan_one(
        &dir,
        "doc.pdf",
        &raw_page(content),
        Operation::convert(Format::Md),
    );
    let source = SourceState::of(&src);
    let (result, _) = run(&job, Options::default());
    let md = fs::read_to_string(&result.unwrap().outputs[0]).unwrap();
    assert_eq!(
        md,
        "# Title Here\n\nBody text line one. Body text line two.\n\n\
         arXiv:2401.00001v1 \\[cs.LG\\] 1 Jan 2024\n\nUpside down\n"
    );
    source.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

/// A PDF of a few hundred bytes whose object stream lists `entries`
/// objects, all at offset 0, where an array of `zeros` zeros starts: lopdf
/// parses the array once per entry.
fn object_stream_bomb(entries: usize, zeros: usize) -> Vec<u8> {
    use std::io::Write;
    let header: String = (0..entries).map(|i| format!("{} 0 ", 100 + i)).collect();
    let mut plain = header.clone().into_bytes();
    plain.push(b'[');
    plain.extend(std::iter::repeat_n(*b"0 ", zeros).flatten());
    plain.push(b']');
    let mut packed = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    packed.write_all(&plain).unwrap();
    let packed = packed.finish().unwrap();
    let mut stream = format!(
        "<< /Type /ObjStm /N {entries} /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
        header.len(),
        packed.len()
    )
    .into_bytes();
    stream.extend_from_slice(&packed);
    stream.extend_from_slice(b"\nendstream");
    let objects: [&[u8]; 4] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /MediaBox [0 0 612 792] /Parent 2 0 R >>",
        &stream,
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (n, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", n + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    pdf
}

#[test]
fn an_object_stream_bomb_stops_at_the_heap_bound() {
    // 8 entries over a million zeros: about 900 MiB in lopdf, from a file
    // of a few KiB. The bound is set to 256 MiB here (4 GiB in Grape).
    let bomb = object_stream_bomb(8, 1_000_000);
    assert!(bomb.len() < 64 * 1024, "{}", bomb.len());
    let options = Options {
        max_heap_bytes: 256 << 20,
        ..Options::default()
    };
    for tool in [ToolKey::SplitPdf, ToolKey::RemoveMetadata] {
        let dir = TestDir::new("pdf-bomb");
        let (src, job) = plan_one(&dir, "doc.pdf", &bomb, Operation::tool(tool));
        let source = SourceState::of(&src);
        let started = Instant::now();
        let (result, _) = run(&job, options);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::DecodeFailed, "{tool:?}");
        assert_eq!(
            error.user_message, "doc.pdf needs too much memory, so Grape stopped.",
            "{tool:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(60), "{tool:?}");
        assert_eq!(names_in(&dir.path), ["doc.pdf"], "{tool:?}");
        source.assert_unchanged();
    }
    // One entry fits under the same bound.
    let dir = TestDir::new("pdf-bomb-one");
    let (_, job) = plan_one(
        &dir,
        "doc.pdf",
        &object_stream_bomb(1, 1_000_000),
        Operation::tool(ToolKey::RemoveMetadata),
    );
    assert!(run(&job, options).0.is_ok());
    assert_no_leftovers(&dir.path);
}

#[test]
fn split_merge_and_metadata_outputs_open_in_pdfium() {
    let dir = TestDir::new("pdf-tools");
    let a = dir.join("a.pdf");
    let b = dir.join("b.pdf");
    fs::write(&a, numbered(2)).unwrap();
    fs::write(&b, numbered(3)).unwrap();
    let sources = [SourceState::of(&a), SourceState::of(&b)];
    let settings = Settings::new(dir.join("fallback"));
    let selection = build_selection(&[b.clone(), a.clone()], &settings);
    let mut jobs = plan(&selection, &Operation::tool(ToolKey::MergePdf), &settings)
        .unwrap()
        .jobs;
    let (result, _) = run(&jobs.remove(0), Options::default());
    let merged = result.unwrap();
    assert_eq!(merged.outputs, [dir.join("b Merged.pdf")]);
    assert_eq!(merged.pages, 5);
    assert!(!merged.used_pdfium);

    // The merged file renders in PDFium: five pages, the first from b.
    let (_, job) = plan_one(
        &dir,
        "b Merged.pdf",
        &fs::read(dir.join("b Merged.pdf")).unwrap(),
        Operation::convert(Format::Txt),
    );
    let (result, _) = run(&job, Options::default());
    let text = fs::read_to_string(&result.unwrap().outputs[0]).unwrap();
    assert_eq!(text, "Page 1\n\nPage 2\n\nPage 3\n\nPage 1\n\nPage 2\n");

    let (_, job) = plan_one(
        &dir,
        "c.pdf",
        &numbered(1),
        Operation::tool(ToolKey::RemoveMetadata),
    );
    let (result, _) = run(&job, Options::default());
    let clean = result.unwrap();
    assert_eq!(clean.removed, ["Title", "Author"]);
    let output = &clean.outputs[0];
    assert_eq!(output, &dir.join("c No Metadata.pdf"));
    let (_, job) = plan_one(
        &dir,
        "c2.pdf",
        &fs::read(output).unwrap(),
        Operation::convert(Format::Png),
    );
    assert!(run(&job, Options::default()).0.is_ok());

    for source in &sources {
        source.assert_unchanged();
    }
    assert_no_leftovers(&dir.path);
}

#[test]
fn cancel_kills_the_worker_within_two_seconds() {
    let dir = TestDir::new("pdf-cancel");
    let (src, job) = plan_one(
        &dir,
        "doc.pdf",
        &numbered(60),
        Operation::convert(Format::Png),
    );
    let source = SourceState::of(&src);
    let ctx = EngineCtx::default();
    let token = ctx.cancel.clone();
    let cancelled_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let mark = Arc::clone(&cancelled_at);
    let mut progress = move |_: f32| {
        let mut at = mark.lock().unwrap();
        if at.is_none() {
            token.cancel();
            *at = Some(Instant::now());
        }
    };
    let result = it::run(
        &job,
        &ctx,
        &mut progress,
        Path::new(WORKER),
        &pdfium(),
        Options::default(),
    );
    let returned = Instant::now();
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    let at = cancelled_at
        .lock()
        .unwrap()
        .expect("no progress before the end");
    assert!(
        returned - at < Duration::from_secs(2),
        "{:?}",
        returned - at
    );
    assert_eq!(names_in(&dir.path), ["doc.pdf"]);
    source.assert_unchanged();
}

#[test]
fn the_folder_output_never_replaces_an_existing_folder() {
    let dir = TestDir::new("pdf-render-taken");
    let (src, job) = plan_one(
        &dir,
        "doc.pdf",
        &numbered(2),
        Operation::convert(Format::Jpg),
    );
    let JobOutput::PagesFileOrFolder { folder, .. } = &job.output else {
        panic!("expected pages, got {:?}", job.output);
    };
    let source = SourceState::of(&src);
    // Made after planning.
    fs::create_dir(folder).unwrap();
    fs::write(folder.join("keep.txt"), b"mine").unwrap();
    let (result, _) = run(&job, Options::default());
    let outcome = result.unwrap();
    assert!(outcome.renamed);
    assert_eq!(outcome.outputs, [dir.join("doc 2")]);
    assert_eq!(names_in(folder), ["keep.txt"]);
    assert_eq!(names_in(&dir.join("doc 2")), ["doc 1.jpg", "doc 2.jpg"]);
    source.assert_unchanged();
    assert_no_leftovers(&dir.path);
}
