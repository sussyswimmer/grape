//! The archive engine through `run_job` (DECISIONS D27): ZIP, TAR and
//! GZIP among each other and Extract Archive need no sidecar; RAR needs
//! `SidecarPaths::bsdtar` (its real runs are in `src/archive/rar/it.rs`).
//! What an archive job leaves out comes back as a `Note`. Every case
//! checks that the source is unchanged and nothing temporary is left.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::PathBuf;

use common::{assert_no_leftovers, plan_one, planned_file, SourceState, TestDir};
use grape_core::{Format, JobOutput, Operation, ToolKey};
use grape_engines::{
    run_job, supports, supports_with, EngineCtx, ErrorKind, LeftOut, Note, SidecarPaths,
};

/// A tar holding `a.txt` and a symlink `ln` to it.
fn tar_with_a_link() -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut file = tar::Header::new_gnu();
    file.set_entry_type(tar::EntryType::Regular);
    file.set_size(5);
    file.set_mode(0o644);
    file.set_mtime(1_600_000_000);
    tar.append_data(&mut file, "a.txt", &b"hello"[..]).unwrap();
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    link.set_mode(0o755);
    link.set_mtime(1_600_000_000);
    tar.append_link(&mut link, "ln", "a.txt").unwrap();
    tar.into_inner().unwrap()
}

/// A tar holding `a.txt` and a symlink `evil` that points out of the
/// archive, to `../../outside`.
fn tar_with_an_escaping_link() -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut file = tar::Header::new_gnu();
    file.set_entry_type(tar::EntryType::Regular);
    file.set_size(5);
    file.set_mode(0o644);
    file.set_mtime(1_600_000_000);
    tar.append_data(&mut file, "a.txt", &b"hello"[..]).unwrap();
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    link.set_mode(0o755);
    link.set_mtime(1_600_000_000);
    tar.append_link(&mut link, "evil", "../../outside").unwrap();
    tar.into_inner().unwrap()
}

fn zip_of_one() -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("a.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"hello").unwrap();
    zip.finish().unwrap().into_inner()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(data).unwrap();
    gz.finish().unwrap()
}

#[test]
fn the_six_archive_pairs_run_without_sidecars() {
    let tar = tar_with_a_link();
    let sources: [(&str, Vec<u8>, [Format; 2]); 3] = [
        ("a.zip", zip_of_one(), [Format::Tar, Format::Gz]),
        ("a.tar", tar.clone(), [Format::Zip, Format::Gz]),
        ("a.tar.gz", gzip(&tar), [Format::Zip, Format::Tar]),
    ];
    for (name, bytes, targets) in sources {
        for to in targets {
            let dir = TestDir::new("archive-pairs");
            let (src, job) = plan_one(&dir, name, &bytes, Operation::convert(to));
            let before = SourceState::of(&src);
            let from = Format::from_file_name(name);
            assert!(supports(&job.op, from), "{name} -> {to:?}");
            let outcome = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();
            assert_eq!(
                outcome.outputs,
                vec![planned_file(&job)],
                "{name} -> {to:?}"
            );
            // The link stays inside the archive, so it's carried over.
            assert_eq!(outcome.notes, Vec::new(), "{name} -> {to:?}");
            before.assert_unchanged();
            assert_no_leftovers(&dir.path);
        }
    }
}

#[test]
fn a_conversion_counts_what_it_left_out() {
    let dir = TestDir::new("archive-left-out");
    let (src, job) = plan_one(
        &dir,
        "a.tar",
        &tar_with_an_escaping_link(),
        Operation::convert(Format::Zip),
    );
    let before = SourceState::of(&src);
    let outcome = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();
    let zip_path = planned_file(&job);
    assert_eq!(outcome.outputs, vec![zip_path.clone()]);
    assert_eq!(
        outcome.notes,
        vec![Note::LeftOut(LeftOut {
            links: 1,
            ..LeftOut::default()
        })]
    );
    // The repacked zip holds the file and not the link.
    let mut zip = zip::ZipArchive::new(fs::File::open(&zip_path).unwrap()).unwrap();
    assert_eq!(zip.len(), 1);
    let mut entry = zip.by_index(0).unwrap();
    assert_eq!(entry.name().unwrap(), "a.txt");
    let mut text = String::new();
    entry.read_to_string(&mut text).unwrap();
    assert_eq!(text, "hello");
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn extract_archive_runs_and_counts_what_it_left_out() {
    let dir = TestDir::new("archive-extract");
    let (src, job) = plan_one(
        &dir,
        "a.tar",
        &tar_with_a_link(),
        Operation::tool(ToolKey::ExtractArchive),
    );
    let before = SourceState::of(&src);
    let JobOutput::Folder(folder) = job.output.clone() else {
        panic!("Extract Archive plans a folder");
    };
    let outcome = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap();
    assert_eq!(outcome.outputs, vec![folder.clone()]);
    assert_eq!(fs::read(folder.join("a.txt")).unwrap(), b"hello");
    assert!(!folder.join("ln").exists());
    assert_eq!(
        outcome.notes,
        vec![Note::LeftOut(LeftOut {
            links: 1,
            ..LeftOut::default()
        })]
    );
    before.assert_unchanged();
    assert_no_leftovers(&dir.path);
}

#[test]
fn rar_runs_only_with_bsdtar() {
    let rar = SidecarPaths {
        bsdtar: Some(PathBuf::from("bsdtar")),
        svg_worker: None,
        ..SidecarPaths::default()
    };
    for to in [Format::Zip, Format::Tar, Format::Gz] {
        let op = Operation::convert(to);
        assert!(!supports(&op, Some(Format::Rar)), "{to:?}");
        assert!(supports_with(&op, &[Some(Format::Rar)], &rar), "{to:?}");
    }
    let extract = Operation::tool(ToolKey::ExtractArchive);
    let mixed = [Some(Format::Zip), Some(Format::Rar)];
    assert!(!supports_with(&extract, &mixed, &SidecarPaths::default()));
    assert!(supports_with(&extract, &mixed, &rar));

    // Without bsdtar, run_job refuses before reading anything.
    let dir = TestDir::new("archive-rar");
    let (src, job) = plan_one(
        &dir,
        "x.rar",
        b"Rar!\x1a\x07\x01\x00",
        Operation::convert(Format::Zip),
    );
    let before = SourceState::of(&src);
    let names = dir.names();
    let error = run_job(&job, &EngineCtx::default(), &mut |_| {}).unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotYetSupported);
    assert_eq!(error.user_message, "RAR to ZIP isn't in this build yet.");
    assert_eq!(dir.names(), names);
    before.assert_unchanged();
}
