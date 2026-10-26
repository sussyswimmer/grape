//! Subtitle conversions end to end: plan with `grape_core`, run with
//! `run_job`, check the output and that the source never changed.
#![allow(clippy::unwrap_used)]

mod common;

use std::fs;

use common::{assert_no_leftovers, names_in, plan_one, planned_file, SourceState, TestDir};
use grape_core::{plan, DirSnapshot, Format, Operation, Selection, SelectionItem, Settings};
use grape_engines::{run_job, EngineCtx, ErrorKind, GrapeError, JobOutcome};

const SRT: &str = "1\r\n\
00:00:01,000 --> 00:00:02,500\r\n\
Hello <i>there</i>\r\n\
\r\n\
2\r\n\
00:00:03,000 --> 00:00:04,000\r\n\
Caf\u{e9} & bar\r\n\
Line two\r\n";

const VTT: &str = "WEBVTT\n\
Kind: captions\n\
\n\
NOTE a comment\n\
\n\
00:01.000 --> 00:02.500 align:start\n\
<v Ana>Hello</v> <b.loud>world</b> &amp; you\n\
\n\
00:02.500 --> 00:04.000\n\
Hello\n";

fn convert(dir: &TestDir, name: &str, bytes: &[u8], to: Format) -> (Vec<u8>, JobOutcome) {
    let (src, job) = plan_one(dir, name, bytes, Operation::convert(to));
    let before = SourceState::of(&src);
    let outcome = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();
    assert_eq!(outcome.outputs.len(), 1);
    let out = fs::read(&outcome.outputs[0]).unwrap();
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
    (out, outcome)
}

fn fail(dir: &TestDir, name: &str, bytes: &[u8], to: Format) -> GrapeError {
    let (src, job) = plan_one(dir, name, bytes, Operation::convert(to));
    let before = SourceState::of(&src);
    let names_before = dir.names();
    let err = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap_err();
    assert_eq!(dir.names(), names_before, "nothing was written");
    before.assert_unchanged();
    err
}

#[test]
fn an_srt_converts_to_webvtt_next_to_the_source() {
    let dir = TestDir::new("srt-vtt");
    let (out, outcome) = convert(&dir, "movie.srt", SRT.as_bytes(), Format::Vtt);
    assert_eq!(outcome.outputs, [dir.join("movie.vtt")]);
    assert!(!outcome.renamed);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "WEBVTT\r\n\
\r\n\
1\r\n\
00:00:01.000 --> 00:00:02.500\r\n\
Hello <i>there</i>\r\n\
\r\n\
2\r\n\
00:00:03.000 --> 00:00:04.000\r\n\
Caf\u{e9} &amp; bar\r\n\
Line two\r\n"
    );
    assert_eq!(dir.names(), ["movie.srt", "movie.vtt"]);
}

#[test]
fn a_webvtt_converts_to_srt_without_a_bom_when_the_source_had_none() {
    let dir = TestDir::new("vtt-srt");
    let (out, outcome) = convert(&dir, "talk.vtt", VTT.as_bytes(), Format::Srt);
    assert_eq!(outcome.outputs, [dir.join("talk.srt")]);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "1\n\
00:00:01,000 --> 00:00:02,500\n\
Hello <b>world</b> & you\n\
\n\
2\n\
00:00:02,500 --> 00:00:04,000\n\
Hello\n"
    );
}

#[test]
fn both_formats_convert_to_plain_text() {
    let dir = TestDir::new("to-txt");
    let (out, outcome) = convert(&dir, "movie.srt", SRT.as_bytes(), Format::Txt);
    assert_eq!(outcome.outputs, [dir.join("movie.txt")]);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "Hello there\r\nCaf\u{e9} & bar\r\nLine two\r\n"
    );
    let (out, outcome) = convert(&dir, "talk.vtt", VTT.as_bytes(), Format::Txt);
    assert_eq!(outcome.outputs, [dir.join("talk.txt")]);
    // "Hello" repeats "Hello world & you" only in part, so both are kept.
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "Hello world & you\nHello\n"
    );
}

#[test]
fn a_repeated_line_is_written_once_in_text() {
    let dir = TestDir::new("txt-repeat");
    let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nsame line\n\n00:02.000 --> 00:03.000\nsame line\nnext line\n";
    let (out, _) = convert(&dir, "auto.vtt", vtt.as_bytes(), Format::Txt);
    assert_eq!(String::from_utf8(out).unwrap(), "same line\nnext line\n");
}

#[test]
fn round_trips_keep_text_and_timings() {
    let dir = TestDir::new("round-trip");
    let (vtt, _) = convert(&dir, "a.srt", SRT.as_bytes(), Format::Vtt);
    let back_dir = TestDir::new("round-trip-back");
    let (srt, _) = convert(&back_dir, "a.vtt", &vtt, Format::Srt);
    let srt = String::from_utf8(srt).unwrap();
    // The source was BOM-less UTF-8 VTT, so no BOM; the rest is the original.
    assert_eq!(srt, SRT);
}

#[test]
fn an_srt_gets_a_bom_unless_its_source_was_plain_utf8() {
    let dir = TestDir::new("bom");
    let with_bom = [b"\xEF\xBB\xBF".as_slice(), VTT.as_bytes()].concat();
    let (out, _) = convert(&dir, "bom.vtt", &with_bom, Format::Srt);
    assert!(out.starts_with(b"\xEF\xBB\xBF1\n"), "{out:?}");
    let (out, _) = convert(&dir, "plain.vtt", VTT.as_bytes(), Format::Srt);
    assert!(out.starts_with(b"1\n"), "{out:?}");
    // WebVTT and text never get one.
    let srt_bom = [b"\xEF\xBB\xBF".as_slice(), SRT.as_bytes()].concat();
    let (out, _) = convert(&dir, "bom.srt", &srt_bom, Format::Vtt);
    assert!(out.starts_with(b"WEBVTT"));
    let (out, _) = convert(&dir, "bom2.srt", &srt_bom, Format::Txt);
    assert!(out.starts_with(b"Hello"));
}

#[test]
fn an_srt_from_a_utf16_or_windows_1252_source_gets_a_bom() {
    let dir = TestDir::new("bom-encodings");
    let text = "WEBVTT\r\n\r\n00:01.000 --> 00:02.000\r\nCaf\u{e9}\r\n";
    let mut le = vec![0xFF, 0xFE];
    le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    let mut be = vec![0xFE, 0xFF];
    be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
    let ansi = b"WEBVTT\r\n\r\n00:01.000 --> 00:02.000\r\nCaf\xE9\r\n".to_vec();
    for (name, bytes) in [("le.vtt", le), ("be.vtt", be), ("ansi.vtt", ansi)] {
        let (out, _) = convert(&dir, name, &bytes, Format::Srt);
        assert!(out.starts_with(b"\xEF\xBB\xBF1\r\n"), "{name}: {out:?}");
        let text = String::from_utf8(out[3..].to_vec()).unwrap();
        assert!(text.contains("Caf\u{e9}\r\n"), "{name}: {text}");
    }
}

#[test]
fn utf16_and_windows_1252_sources_come_out_as_utf8() {
    let dir = TestDir::new("encodings");
    let text = "1\r\n00:00:01,000 --> 00:00:02,000\r\nCaf\u{e9} \u{2014} na\u{ef}ve\r\n";
    let mut utf16le = vec![0xFF, 0xFE];
    utf16le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    let mut utf16be = vec![0xFE, 0xFF];
    utf16be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
    // "Café — naïve" in Windows-1252: é = E9, — = 97, ï = EF.
    let mut cp1252 = b"1\r\n00:00:01,000 --> 00:00:02,000\r\nCaf\xE9 \x97 na\xEFve\r\n".to_vec();
    for (name, bytes) in [
        ("le.srt", &utf16le),
        ("be.srt", &utf16be),
        ("ansi.srt", &mut cp1252),
    ] {
        let (out, _) = convert(&dir, name, bytes, Format::Vtt);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("Caf\u{e9} \u{2014} na\u{ef}ve\r\n"),
            "{name}: {out}"
        );
    }
}

#[test]
fn the_content_decides_between_srt_and_webvtt() {
    let dir = TestDir::new("by-content");
    // A WebVTT file named .srt is read as WebVTT (its header is skipped).
    let (out, _) = convert(&dir, "mislabeled.srt", VTT.as_bytes(), Format::Txt);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "Hello world & you\nHello\n"
    );
}

#[test]
fn webvtt_named_references_are_decoded_once() {
    let dir = TestDir::new("references");
    let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n\
               He said &quot;hi&quot; &hellip; caf&eacute; &Eacute;t&eacute; &amp; more &copy\n";
    let (out, _) = convert(&dir, "a.vtt", vtt.as_bytes(), Format::Srt);
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.ends_with("\nHe said \"hi\" … café Été & more ©\n"),
        "{out}"
    );
    // A WebVTT file named .srt, to WebVTT: decoded, then only &, < and >
    // escaped again.
    let (out, _) = convert(&dir, "b.srt", vtt.as_bytes(), Format::Vtt);
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.ends_with("\nHe said \"hi\" … café Été &amp; more ©\n"),
        "{out}"
    );
}

#[test]
fn a_file_without_cues_is_not_readable_subtitles() {
    let dir = TestDir::new("no-cues");
    for (name, bytes) in [
        ("empty.srt", b"".as_slice()),
        (
            "notes.srt",
            b"just some notes\nwithout timings\n".as_slice(),
        ),
        ("image.vtt", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".as_slice()),
        ("header.vtt", b"WEBVTT\n\nNOTE nothing else\n".as_slice()),
    ] {
        let err = fail(&dir, name, bytes, Format::Txt);
        assert_eq!(err.kind, ErrorKind::DecodeFailed, "{name}");
        assert_eq!(err.user_message, format!("{name} has no subtitles in it."));
    }
}

#[test]
fn a_subtitle_file_over_the_size_limit_is_refused_before_reading() {
    let dir = TestDir::new("too-large");
    let (src, job) = plan_one(
        &dir,
        "huge.srt",
        SRT.as_bytes(),
        Operation::convert(Format::Vtt),
    );
    // Sparse where the file system allows it, so the test stays fast.
    let file = fs::OpenOptions::new().write(true).open(&src).unwrap();
    file.set_len(16 * 1024 * 1024 + 1).unwrap();
    drop(file);
    let before = SourceState::of(&src);
    let names_before = dir.names();

    let err = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap_err();

    assert_eq!(err.kind, ErrorKind::DecodeFailed);
    assert_eq!(err.user_message, "huge.srt is too large to convert.");
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}

#[test]
fn a_file_at_the_size_limit_is_read() {
    let dir = TestDir::new("at-limit");
    // Two cues, then lines of spaces, 1 KiB each, up to exactly 16 MiB.
    let mut bytes = SRT.as_bytes().to_vec();
    let fill = (bytes.len()..16 * 1024 * 1024).map(|i| if i % 1024 == 0 { b'\n' } else { b' ' });
    bytes.extend(fill.collect::<Vec<u8>>());
    assert_eq!(bytes.len(), 16 * 1024 * 1024);
    let (out, _) = convert(&dir, "big.srt", &bytes, Format::Txt);
    assert!(String::from_utf8(out).unwrap().starts_with("Hello there"));
}

#[test]
fn a_file_over_the_line_limit_is_refused_before_anything_is_written() {
    let dir = TestDir::new("too-many-lines");
    let bytes = vec![b'\n'; 1_000_001];
    let err = fail(&dir, "lines.srt", &bytes, Format::Vtt);
    assert_eq!(err.kind, ErrorKind::DecodeFailed);
    assert_eq!(err.user_message, "lines.srt is too large to convert.");
}

#[test]
fn a_file_with_a_line_over_64_kib_is_refused_before_anything_is_written() {
    let dir = TestDir::new("long-line");
    let mut bytes = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
    bytes.resize(bytes.len() + 64 * 1024 + 1, b'&');
    let err = fail(&dir, "long.srt", &bytes, Format::Vtt);
    assert_eq!(err.kind, ErrorKind::DecodeFailed);
    assert_eq!(err.user_message, "long.srt is too large to convert.");
    // A line of exactly 64 KiB converts.
    bytes.pop();
    let (out, _) = convert(&dir, "longest.srt", &bytes, Format::Vtt);
    let amps = "&amp;".repeat(64 * 1024);
    assert!(String::from_utf8(out)
        .unwrap()
        .ends_with(&format!("\n{amps}\n")));
    // Measured once read as UTF-8: 32 KiB and one byte of Windows-1252 "é"
    // are over 64 KiB as UTF-8.
    let mut ansi = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
    ansi.resize(ansi.len() + 32 * 1024 + 1, 0xE9);
    let err = fail(&dir, "ansi.srt", &ansi, Format::Vtt);
    assert_eq!(err.user_message, "ansi.srt is too large to convert.");
}

#[test]
fn a_name_taken_after_planning_gets_the_next_number() {
    let dir = TestDir::new("collision");
    let (src, job) = plan_one(
        &dir,
        "subs.srt",
        SRT.as_bytes(),
        Operation::convert(Format::Vtt),
    );
    let planned = planned_file(&job);
    assert_eq!(planned, dir.join("subs.vtt"));
    fs::write(&planned, b"not ours").unwrap();
    let before = SourceState::of(&src);

    let outcome = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();

    assert_eq!(
        outcome,
        JobOutcome {
            outputs: vec![dir.join("subs 2.vtt")],
            renamed: true
        }
    );
    assert_eq!(fs::read(&planned).unwrap(), b"not ours");
    assert!(fs::read(dir.join("subs 2.vtt"))
        .unwrap()
        .starts_with(b"WEBVTT"));
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn an_existing_output_is_never_replaced() {
    let dir = TestDir::new("existing");
    fs::write(dir.join("subs.txt"), b"keep me").unwrap();
    let (_, outcome) = convert(&dir, "subs.srt", SRT.as_bytes(), Format::Txt);
    assert_eq!(outcome.outputs, [dir.join("subs 2.txt")]);
    assert_eq!(fs::read(dir.join("subs.txt")).unwrap(), b"keep me");
}

#[test]
fn a_cancelled_job_writes_nothing() {
    let dir = TestDir::new("cancel");
    let (src, job) = plan_one(
        &dir,
        "subs.srt",
        SRT.as_bytes(),
        Operation::convert(Format::Vtt),
    );
    let before = SourceState::of(&src);
    let names_before = dir.names();
    let ctx = EngineCtx::default();
    ctx.cancel.cancel();

    let err = run_job(&job, &ctx, &mut |_| {}).unwrap_err();

    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(dir.names(), names_before);
    before.assert_unchanged();
}

#[test]
fn a_job_cancelled_while_writing_reports_it_and_leaves_nothing() {
    for (name, bytes, to) in [
        ("subs.srt", SRT, Format::Vtt),
        ("subs.srt", SRT, Format::Txt),
        ("talk.vtt", VTT, Format::Srt),
    ] {
        let dir = TestDir::new("cancel-writing");
        let (src, job) = plan_one(&dir, name, bytes.as_bytes(), Operation::convert(to));
        let before = SourceState::of(&src);
        let names_before = dir.names();
        let ctx = EngineCtx::default();
        let token = ctx.cancel.clone();
        // progress(0.5) comes once the text is parsed, just before the
        // temp file is created and written.
        let mut seen = Vec::new();
        let err = run_job(&job, &ctx, &mut |f| {
            seen.push(f);
            if f == 0.5 {
                token.cancel();
            }
        })
        .unwrap_err();
        assert_eq!(seen, [0.5], "{to:?}");
        assert_eq!(err.kind, ErrorKind::Cancelled, "{to:?}: {err:?}");
        assert_eq!(dir.names(), names_before, "{to:?}");
        before.assert_unchanged();
    }
}

#[test]
fn a_read_only_source_converts_and_stays_untouched() {
    let dir = TestDir::new("read-only-source");
    let (src, job) = plan_one(
        &dir,
        "movie.srt",
        SRT.as_bytes(),
        Operation::convert(Format::Vtt),
    );
    let writable = fs::metadata(&src).unwrap().permissions();
    let mut read_only = writable.clone();
    read_only.set_readonly(true);
    fs::set_permissions(&src, read_only).unwrap();
    // Restore write access before `TestDir` deletes the tree.
    struct Unlock(std::path::PathBuf, fs::Permissions);
    impl Drop for Unlock {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, self.1.clone());
        }
    }
    let _unlock = Unlock(src.clone(), writable);
    // Root (and CAP_DAC_OVERRIDE) ignores permission bits: check first.
    if fs::OpenOptions::new().append(true).open(&src).is_ok() {
        println!(
            "note: skipped, this user can open a read-only file for writing (running as root?)"
        );
        return;
    }
    let before = SourceState::of(&src);

    let outcome = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();

    assert_eq!(outcome.outputs, [dir.join("movie.vtt")]);
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn a_missing_source_is_a_read_error() {
    let dir = TestDir::new("missing");
    let (src, job) = plan_one(
        &dir,
        "gone.vtt",
        VTT.as_bytes(),
        Operation::convert(Format::Srt),
    );
    fs::remove_file(&src).unwrap();

    let err = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap_err();

    assert_eq!(err.kind, ErrorKind::ReadFailed);
    assert_eq!(err.user_message, "Grape couldn't read gone.vtt.");
    assert!(names_in(&dir.path).is_empty());
}

#[test]
fn progress_ends_at_one() {
    let dir = TestDir::new("progress");
    let (_, job) = plan_one(
        &dir,
        "subs.srt",
        SRT.as_bytes(),
        Operation::convert(Format::Vtt),
    );
    let mut seen = Vec::new();
    run_job(&job, &EngineCtx::default(), &mut |f| seen.push(f)).unwrap();
    assert_eq!(seen.last(), Some(&1.0));
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
}

#[test]
fn a_fallback_job_creates_the_fallback_folder() {
    let dir = TestDir::new("fallback");
    let shows = dir.join("shows");
    fs::create_dir(&shows).unwrap();
    let src = shows.join("ep1.srt");
    fs::write(&src, SRT).unwrap();
    let fallback = dir.join("Downloads").join("Grape");
    let settings = Settings::new(&fallback);
    // The source folder is read-only; the fallback doesn't exist yet.
    let sel = Selection::new(vec![SelectionItem::file(&src)])
        .with_dir(&shows, DirSnapshot::new(["ep1.srt"], false))
        .with_dir(&fallback, DirSnapshot::new(Vec::<String>::new(), true));
    let job = plan(&sel, &Operation::convert(Format::Vtt), &settings)
        .unwrap()
        .jobs
        .remove(0);
    assert!(job.used_fallback);
    let before = SourceState::of(&src);

    let outcome = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();

    assert_eq!(outcome.outputs, [fallback.join("ep1.vtt")]);
    assert_eq!(names_in(&shows), ["ep1.srt"]);
    assert_eq!(names_in(&fallback), ["ep1.vtt"]);
    before.assert_unchanged();
}
