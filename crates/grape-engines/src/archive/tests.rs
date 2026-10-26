//! End-to-end tests of the archive engine on real files in a temp folder.
//! Every fixture is built here with the crates (no binary blobs). Every
//! run checks that the source is byte-for-byte unchanged, that no
//! `.grape-` temp file or folder is left, and that progress never moves
//! backwards (and ends at 1.0 on success). Outputs are read back and
//! compared entry by entry: names, bytes, links, mtimes and modes.

#![allow(clippy::unwrap_used)]

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::hash::Hasher;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use flate2::{Compression, GzBuilder};
use grape_core::{plan, Format, Job, JobOutput, Operation, Settings, ToolKey};
use tar::{EntryType, Header};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, System, ZipArchive, ZipWriter};

use super::*;
use crate::output::unique_suffix;
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError};
use sink::Sink;
use source::Entry;

/// An even time (DOS times have 2-second steps): 2023-11-14 22:13:20 UTC.
pub(super) const T: i64 = 1_700_000_000;

// ---- Folders, sources and runs -------------------------------------------

/// A fresh folder under the system temp folder, removed when dropped.
pub(super) struct TestDir(PathBuf);

impl TestDir {
    pub(super) fn new() -> TestDir {
        let path = std::env::temp_dir().join(format!("grape-archive-unit-{}", unique_suffix()));
        fs::create_dir_all(&path).unwrap();
        TestDir(path)
    }

    pub(super) fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

pub(super) fn assert_no_leftovers(dir: &Path) {
    let left: Vec<String> = listing(dir)
        .into_iter()
        .filter(|name| name.starts_with(".grape-"))
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// A file's length and a hash of its bytes, plus its mtime.
pub(super) struct SourceState {
    path: PathBuf,
    digest: (usize, u64),
    modified: SystemTime,
}

impl SourceState {
    fn of(path: &Path) -> SourceState {
        let bytes = fs::read(path).unwrap();
        let mut hasher = DefaultHasher::new();
        hasher.write(&bytes);
        SourceState {
            path: path.to_path_buf(),
            digest: (bytes.len(), hasher.finish()),
            modified: fs::metadata(path).unwrap().modified().unwrap(),
        }
    }

    pub(super) fn assert_unchanged(&self) {
        let now = SourceState::of(&self.path);
        assert_eq!(now.digest, self.digest, "the source's bytes changed");
        assert_eq!(now.modified, self.modified, "the source's mtime changed");
    }
}

/// One source in its own folder, planned the way the app plans it.
pub(super) struct Case {
    pub(super) dir: TestDir,
    pub(super) src: PathBuf,
    pub(super) from: Format,
    pub(super) job: Job,
    pub(super) source: SourceState,
}

pub(super) fn case(name: &str, bytes: &[u8], op: Operation) -> Case {
    let dir = TestDir::new();
    let src = dir.join(name);
    fs::write(&src, bytes).unwrap();
    let settings = Settings::new(dir.join("fallback"));
    let selection = crate::build_selection(std::slice::from_ref(&src), &settings);
    let mut jobs = plan(&selection, &op, &settings).unwrap().jobs;
    assert_eq!(jobs.len(), 1);
    Case {
        source: SourceState::of(&src),
        from: Format::from_file_name(name).unwrap(),
        job: jobs.remove(0),
        src,
        dir,
    }
}

pub(super) fn convert_case(name: &str, bytes: &[u8], to: Format) -> Case {
    case(name, bytes, Operation::convert(to))
}

pub(super) fn extract_case(name: &str, bytes: &[u8]) -> Case {
    case(name, bytes, Operation::tool(ToolKey::ExtractArchive))
}

impl Case {
    pub(super) fn planned(&self) -> PathBuf {
        self.job.output.display_path().to_path_buf()
    }

    /// Runs the job; cancels it once progress reaches `cancel_at`.
    pub(super) fn run_with(
        &self,
        limits: Limits,
        cancel_at: Option<f32>,
    ) -> Result<ArchiveOutcome, GrapeError> {
        self.run_rar(limits, cancel_at, None)
    }

    /// [`Case::run_with`], with bsdtar started as `rar` says.
    pub(super) fn run_rar(
        &self,
        limits: Limits,
        cancel_at: Option<f32>,
        rar: Option<&Launch>,
    ) -> Result<ArchiveOutcome, GrapeError> {
        let cancel = CancelToken::new();
        let ctx = EngineCtx {
            cancel: cancel.clone(),
            ..EngineCtx::default()
        };
        let mut seen = Vec::new();
        let result = {
            let mut progress = |f: f32| {
                seen.push(f);
                if cancel_at.is_some_and(|at| f >= at) {
                    cancel.cancel();
                }
            };
            match (&self.job.output, self.job.op) {
                (JobOutput::File(dest), Operation::Convert { to }) => convert_with(
                    &self.job,
                    &self.src,
                    dest,
                    self.from,
                    to,
                    &ctx,
                    &mut progress,
                    limits,
                    rar,
                ),
                (JobOutput::Folder(dest), Operation::Tool { .. }) => extract_with(
                    &self.job,
                    &self.src,
                    dest,
                    self.from,
                    &ctx,
                    &mut progress,
                    limits,
                    rar,
                ),
                other => panic!("not an archive job: {other:?}"),
            }
        };
        assert!(
            seen.windows(2).all(|w| w[0] <= w[1]),
            "progress went back: {seen:?}"
        );
        assert!(seen.iter().all(|f| (0.0..=1.0).contains(f)), "{seen:?}");
        if result.is_ok() {
            assert_eq!(seen.last(), Some(&1.0));
        }
        self.source.assert_unchanged();
        assert_no_leftovers(&self.dir.0);
        result
    }

    pub(super) fn ok(&self) -> ArchiveOutcome {
        let outcome = self.run_with(Limits::default(), None).unwrap();
        assert_eq!(outcome.outcome.outputs, [self.planned()]);
        assert!(!outcome.outcome.renamed);
        outcome
    }

    pub(super) fn fails_with(&self, limits: Limits) -> GrapeError {
        let error = self.run_with(limits, None).unwrap_err();
        self.assert_no_output();
        error
    }

    pub(super) fn fails(&self) -> GrapeError {
        self.fails_with(Limits::default())
    }

    /// Nothing but the source is in the folder.
    pub(super) fn assert_no_output(&self) {
        let name = self.src.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(listing(&self.dir.0), [name]);
    }

    pub(super) fn output_bytes(&self) -> Vec<u8> {
        fs::read(self.planned()).unwrap()
    }
}

// ---- Fixtures --------------------------------------------------------------

/// Deterministic bytes that don't compress.
pub(super) fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A ZIP entry for [`zip_of`]. Entry `i` is dated `T + 7200 * i`.
enum Zx<'a> {
    File(&'a str, &'a [u8]),
    Exec(&'a str, &'a [u8]),
    Stored(&'a str, &'a [u8]),
    Dir(&'a str),
    Link(&'a str, &'a str),
}

fn zip_options(i: usize, mode: u32) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .system(System::Unix)
        .compression_method(CompressionMethod::DEFLATE)
        .last_modified_time(time::to_dos(T + 7200 * i as i64))
        .unix_permissions(mode)
}

fn zip_of(entries: &[Zx]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            Zx::File(name, data) => {
                zip.start_file(*name, zip_options(i, 0o644)).unwrap();
                zip.write_all(data).unwrap();
            }
            Zx::Exec(name, data) => {
                zip.start_file(*name, zip_options(i, 0o755)).unwrap();
                zip.write_all(data).unwrap();
            }
            Zx::Stored(name, data) => {
                let options = zip_options(i, 0o644).compression_method(CompressionMethod::Stored);
                zip.start_file(*name, options).unwrap();
                zip.write_all(data).unwrap();
            }
            Zx::Dir(name) => zip.add_directory(*name, zip_options(i, 0o755)).unwrap(),
            Zx::Link(name, target) => zip
                .add_symlink(*name, *target, zip_options(i, 0o755))
                .unwrap(),
        }
    }
    zip.finish().unwrap().into_inner()
}

/// A tar entry for [`tar_of`]. Entry `i` is dated `T + 1 + 3 * i` (odd,
/// so a ZIP keeps it only through the extended timestamp).
enum Tx<'a> {
    File(&'a str, &'a [u8]),
    Mode(&'a str, &'a [u8], u32),
    Dir(&'a str),
    Sym(&'a str, &'a str),
    Hard(&'a str, &'a str),
    Fifo(&'a str),
    /// A name written straight into the header, unchecked by the tar crate.
    Raw(&'a str, &'a [u8]),
}

fn tar_header(i: usize, kind: EntryType, size: u64, mode: u32) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(mode);
    header.set_mtime((T + 1 + 3 * i as i64) as u64);
    header
}

fn tar_of(entries: &[Tx]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            Tx::File(name, data) => {
                let mut h = tar_header(i, EntryType::Regular, data.len() as u64, 0o644);
                tar.append_data(&mut h, name, *data).unwrap();
            }
            Tx::Mode(name, data, mode) => {
                let mut h = tar_header(i, EntryType::Regular, data.len() as u64, *mode);
                tar.append_data(&mut h, name, *data).unwrap();
            }
            Tx::Dir(name) => {
                let mut h = tar_header(i, EntryType::Directory, 0, 0o755);
                tar.append_data(&mut h, name, std::io::empty()).unwrap();
            }
            Tx::Sym(name, target) => {
                let mut h = tar_header(i, EntryType::Symlink, 0, 0o755);
                tar.append_link(&mut h, name, target).unwrap();
            }
            Tx::Hard(name, target) => {
                let mut h = tar_header(i, EntryType::Link, 0, 0o644);
                tar.append_link(&mut h, name, target).unwrap();
            }
            Tx::Fifo(name) => {
                let mut h = tar_header(i, EntryType::Fifo, 0, 0o644);
                tar.append_data(&mut h, name, std::io::empty()).unwrap();
            }
            Tx::Raw(name, data) => {
                let mut h = Header::new_ustar();
                h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_mtime(T as u64);
                h.set_entry_type(EntryType::Regular);
                h.set_cksum();
                tar.append(&h, *data).unwrap();
            }
        }
    }
    tar.into_inner().unwrap()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn gzip_named(data: &[u8], name: &str, mtime: u32) -> Vec<u8> {
    let mut encoder = GzBuilder::new()
        .filename(name)
        .mtime(mtime)
        .write(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

pub(super) fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    MultiGzDecoder::new(bytes).read_to_end(&mut out).unwrap();
    out
}

/// Patches a one-entry ZIP's local header and central directory header.
fn patch_zip(bytes: &mut [u8], patch: impl Fn(&mut [u8], bool)) {
    let local = find(bytes, b"PK\x03\x04");
    let central = find(bytes, b"PK\x01\x02");
    assert_eq!((local.len(), central.len()), (1, 1));
    patch(&mut bytes[local[0]..], true);
    patch(&mut bytes[central[0]..], false);
}

fn find(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i)
        .collect()
}

/// One tar entry, header and data padded to whole blocks, with the name
/// bytes written straight into the header (at most 100).
pub(super) fn raw_entry(name: &[u8], kind: EntryType, data: &[u8]) -> Vec<u8> {
    let mut header = Header::new_ustar();
    header.as_old_mut().name[..name.len()].copy_from_slice(name);
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(T as u64);
    header.set_entry_type(kind);
    header.set_cksum();
    let mut out = header.as_bytes().to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out
}

/// One PAX record: `{len} {key}={value}\n`, its length counting itself.
pub(super) fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let base = key.len() + value.len() + 3;
    let mut len = base + 1;
    while len != base + len.to_string().len() {
        len = base + len.to_string().len();
    }
    let mut out = format!("{len} {key}=").into_bytes();
    out.extend_from_slice(value);
    out.push(b'\n');
    out
}

/// A tar of `entries` (from [`raw_entry`]) and its end marker.
pub(super) fn tar_raw_of(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out = entries.concat();
    out.extend([0u8; 1024]);
    out
}

// ---- Reading outputs back ------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Node {
    File(Vec<u8>),
    Dir,
    Link(String),
    HardLink(String),
    Fifo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub(super) name: String,
    pub(super) node: Node,
    pub(super) mtime: Option<i64>,
    pub(super) mode: Option<u32>,
}

fn sorted(mut items: Vec<Item>) -> Vec<Item> {
    items.sort_by(|a, b| a.name.cmp(&b.name));
    items
}

pub(super) fn zip_items(bytes: &[u8]) -> Vec<Item> {
    let mut zip = ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut items = Vec::new();
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).unwrap();
        let name = file.name().unwrap().trim_end_matches('/').to_string();
        let mode = file.unix_mode();
        let kind = mode.map(|m| m & 0o170_000);
        let mtime = source::zip_mtime(&file);
        let node = if file.is_dir() || kind == Some(0o040_000) {
            Node::Dir
        } else {
            let mut data = Vec::new();
            file.read_to_end(&mut data).unwrap();
            if kind == Some(0o120_000) {
                Node::Link(String::from_utf8(data).unwrap())
            } else {
                Node::File(data)
            }
        };
        items.push(Item {
            name,
            node,
            mtime,
            mode: mode.map(|m| m & 0o777),
        });
    }
    sorted(items)
}

pub(super) fn tar_items(bytes: &[u8]) -> Vec<Item> {
    let mut archive = tar::Archive::new(bytes);
    let mut items = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = String::from_utf8(entry.path_bytes().into_owned())
            .unwrap()
            .trim_end_matches('/')
            .to_string();
        let header = entry.header();
        let kind = header.entry_type();
        let mtime = header.mtime().ok().map(|t| t as i64);
        let mode = header.mode().ok().map(|m| m & 0o777);
        let link = || String::from_utf8(entry.link_name_bytes().unwrap().into_owned()).unwrap();
        let node = if kind.is_dir() {
            Node::Dir
        } else if kind.is_symlink() {
            Node::Link(link())
        } else if kind.is_hard_link() {
            Node::HardLink(link())
        } else if kind.is_fifo() {
            Node::Fifo
        } else {
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            Node::File(data)
        };
        items.push(Item {
            name,
            node,
            mtime,
            mode,
        });
    }
    sorted(items)
}

/// An extracted folder as items: never a link; modes on Unix only.
pub(super) fn tree_items(root: &Path) -> Vec<Item> {
    fn walk(root: &Path, dir: &Path, items: &mut Vec<Item>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            assert!(!meta.file_type().is_symlink(), "a link was extracted");
            let name = path
                .strip_prefix(root)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                Some(meta.permissions().mode() & 0o777)
            };
            #[cfg(not(unix))]
            let mode = None;
            let mtime = Some(time::from_system(meta.modified().unwrap()));
            if meta.is_dir() {
                items.push(Item {
                    name,
                    node: Node::Dir,
                    mtime,
                    mode,
                });
                walk(root, &path, items);
            } else {
                items.push(Item {
                    name,
                    node: Node::File(fs::read(&path).unwrap()),
                    mtime,
                    mode,
                });
            }
        }
    }
    let mut items = Vec::new();
    walk(root, root, &mut items);
    sorted(items)
}

/// The items an extraction of `items` writes: links, hard links, FIFOs
/// and macOS metadata left out; owner access added to modes on Unix;
/// folders implied by deeper items added.
fn extracted(items: &[Item]) -> (Vec<Item>, BTreeSet<String>) {
    let mut files = Vec::new();
    let mut dirs = BTreeSet::new();
    for item in items {
        let path: Vec<&str> = item.name.split('/').collect();
        if names::is_mac_metadata(&path.iter().map(|p| p.to_string()).collect::<Vec<_>>()) {
            continue;
        }
        match &item.node {
            Node::File(_) => {
                for depth in 1..path.len() {
                    dirs.insert(path[..depth].join("/"));
                }
                files.push(Item {
                    mode: if cfg!(unix) {
                        item.mode.map(|m| m & 0o755 | 0o600)
                    } else {
                        None
                    },
                    ..item.clone()
                });
            }
            Node::Dir => {
                for depth in 1..=path.len() {
                    dirs.insert(path[..depth].join("/"));
                }
            }
            Node::Link(_) | Node::HardLink(_) | Node::Fifo => {}
        }
    }
    (sorted(files), dirs)
}

fn assert_extracted(root: &Path, source: &[Item]) {
    let tree = tree_items(root);
    let (files, dirs) = extracted(source);
    let tree_files: Vec<Item> = tree
        .iter()
        .filter(|item| item.node != Node::Dir)
        .cloned()
        .collect();
    assert_eq!(tree_files, files);
    let tree_dirs: BTreeSet<String> = tree
        .iter()
        .filter(|item| item.node == Node::Dir)
        .map(|item| item.name.clone())
        .collect();
    assert_eq!(tree_dirs, dirs);
    // Nested folders named in the archive keep their mtime (top-level ones
    // are moved into the published folder at the end).
    for item in source
        .iter()
        .filter(|i| i.node == Node::Dir && i.name.contains('/'))
    {
        let on_disk = tree.iter().find(|t| t.name == item.name).unwrap();
        assert_eq!(on_disk.mtime, item.mtime, "{}", item.name);
    }
}

fn without(items: Vec<Item>, names: &[&str]) -> Vec<Item> {
    items
        .into_iter()
        .filter(|item| !names.contains(&item.name.as_str()))
        .collect()
}

// ---- The shapes of the API -------------------------------------------------

#[test]
fn supports_the_six_v1_pairs_and_extracts_three_formats() {
    let archives = [Format::Zip, Format::Tar, Format::Gz, Format::Rar];
    let mut pairs = Vec::new();
    for from in archives {
        for to in archives {
            if supports(from, to, false) {
                pairs.push((from, to));
            }
            // With bsdtar, RAR → ZIP, TAR, GZIP too, and nothing else.
            assert_eq!(
                supports(from, to, true),
                supports(from, to, false)
                    || (from == Format::Rar
                        && matches!(to, Format::Zip | Format::Tar | Format::Gz)),
                "{from:?} → {to:?}"
            );
        }
    }
    assert_eq!(
        pairs,
        [
            (Format::Zip, Format::Tar),
            (Format::Zip, Format::Gz),
            (Format::Tar, Format::Zip),
            (Format::Tar, Format::Gz),
            (Format::Gz, Format::Zip),
            (Format::Gz, Format::Tar),
        ]
    );
    assert!(!supports(Format::Png, Format::Zip, true));
    assert!(!supports(Format::Rar, Format::Rar, true));
    for has_bsdtar in [false, true] {
        assert!(
            extracts(Format::Zip, has_bsdtar)
                && extracts(Format::Tar, has_bsdtar)
                && extracts(Format::Gz, has_bsdtar)
        );
        assert!(!extracts(Format::Png, has_bsdtar));
    }
    assert!(!extracts(Format::Rar, false));
    assert!(extracts(Format::Rar, true));
    assert_eq!(
        Limits::default(),
        Limits {
            max_entries: 100_000,
            max_total: 64 << 30,
            ratio_floor: 1 << 30,
            max_ratio: 1000,
        }
    );
    assert_eq!(thousands(100_000), "100,000");
    assert_eq!(thousands(999), "999");
    assert_eq!(bytes(64 << 30), "64 GiB");
    assert_eq!(bytes(16 << 20), "16 MiB");
    assert_eq!(bytes(1500), "1,500 bytes");
}

// ---- Conversions -------------------------------------------------------------

fn sample_zip() -> Vec<u8> {
    zip_of(&[
        Zx::Dir("docs/"),
        Zx::File("docs/readme.txt", b"hello\n"),
        Zx::Exec("docs/run.sh", b"#!/bin/sh\necho hi\n"),
        Zx::Dir("docs/empty/"),
        Zx::Dir("docs/empty/deeper/"),
        Zx::File("ảnh chụp.jpg", &noise(20_000, 1)),
        Zx::Stored("docs/stored.bin", &noise(1_000, 2)),
        Zx::Link("docs/latest", "readme.txt"),
        Zx::File("Upper.TXT", b""),
    ])
}

#[test]
fn zip_to_tar_keeps_folders_bytes_links_modes_and_mtimes() {
    let source = sample_zip();
    let case = convert_case("bundle.zip", &source, Format::Tar);
    assert_eq!(case.planned(), case.dir.join("bundle.tar"));
    let outcome = case.ok();
    assert_eq!(outcome.left_out, LeftOut::default());
    assert_eq!(tar_items(&case.output_bytes()), zip_items(&source));
}

#[test]
fn zip_to_gz_writes_a_tarball() {
    let source = sample_zip();
    let case = convert_case("bundle.zip", &source, Format::Gz);
    assert_eq!(case.planned(), case.dir.join("bundle.tar.gz"));
    case.ok();
    assert_eq!(tar_items(&gunzip(&case.output_bytes())), zip_items(&source));
}

fn sample_tar() -> Vec<u8> {
    let long = format!("{}/{}.txt", "folder-".repeat(20), "file-".repeat(30));
    let long_link = format!("{}/link", "deep-".repeat(25));
    let long_target = format!("../{}", "target-".repeat(20));
    // Leaked so the fixture can borrow it; tests are short-lived.
    let long: &'static str = Box::leak(long.into_boxed_str());
    let long_link: &'static str = Box::leak(long_link.into_boxed_str());
    let long_target: &'static str = Box::leak(long_target.into_boxed_str());
    tar_of(&[
        Tx::Dir("src/"),
        Tx::File("src/main.rs", b"fn main() {}\n"),
        Tx::Mode("src/tool", b"\x7fELF", 0o755),
        Tx::Dir("src/empty/"),
        Tx::File(long, &noise(5_000, 3)),
        Tx::Sym("src/current", "main.rs"),
        Tx::Sym(long_link, long_target),
        Tx::Hard("src/again.rs", "src/main.rs"),
        Tx::Fifo("src/pipe"),
        Tx::File("latin.txt", b"x"),
    ])
}

#[test]
fn tar_to_zip_keeps_long_names_and_links_and_leaves_out_the_rest() {
    let source = sample_tar();
    let case = convert_case("code.tar", &source, Format::Zip);
    let outcome = case.ok();
    assert_eq!(
        outcome.left_out,
        LeftOut {
            links: 1,
            specials: 1,
            mac_metadata: 0,
            copies: 0,
        }
    );
    // Odd-second mtimes survive through the extended timestamp.
    let expected = without(tar_items(&source), &["src/again.rs", "src/pipe"]);
    assert_eq!(zip_items(&case.output_bytes()), expected);
}

#[test]
fn tar_to_gz_and_back_keep_the_bytes() {
    let source = sample_tar();
    let case = convert_case("code.tar", &source, Format::Gz);
    let outcome = case.ok();
    // Nothing is left out: the bytes are carried as they are.
    assert_eq!(outcome.left_out, LeftOut::default());
    let tarball = case.output_bytes();
    assert_eq!(gunzip(&tarball), source);

    let back = convert_case("code.tar.gz", &tarball, Format::Tar);
    assert_eq!(back.planned(), back.dir.join("code.tar"));
    back.ok();
    assert_eq!(back.output_bytes(), source);

    // .tgz, and a tarball with zero padding after its member.
    let mut padded = tarball.clone();
    padded.extend([0u8; 1024]);
    let tgz = convert_case("code.tgz", &padded, Format::Tar);
    tgz.ok();
    assert_eq!(tgz.output_bytes(), source);
}

#[test]
fn tarball_to_zip_repacks_the_entries() {
    let source = sample_tar();
    let case = convert_case("code.tar.gz", &gzip(&source), Format::Zip);
    assert_eq!(case.planned(), case.dir.join("code.zip"));
    case.ok();
    let expected = without(tar_items(&source), &["src/again.rs", "src/pipe"]);
    assert_eq!(zip_items(&case.output_bytes()), expected);
}

#[test]
fn conversions_round_trip() {
    // zip → tar → zip → tar.gz → zip: the same entries every time.
    let source = sample_zip();
    let expected = zip_items(&source);
    let to_tar = convert_case("a.zip", &source, Format::Tar);
    to_tar.ok();
    let to_zip = convert_case("a.tar", &to_tar.output_bytes(), Format::Zip);
    to_zip.ok();
    assert_eq!(zip_items(&to_zip.output_bytes()), expected);
    let to_gz = convert_case("a.zip", &to_zip.output_bytes(), Format::Gz);
    to_gz.ok();
    let again = convert_case("a.tar.gz", &to_gz.output_bytes(), Format::Zip);
    again.ok();
    assert_eq!(zip_items(&again.output_bytes()), expected);
}

#[test]
fn modes_lose_special_bits_and_group_write() {
    let source = tar_of(&[
        Tx::Mode("suid", b"x", 0o4777),
        Tx::Mode("plain", b"y", 0o664),
    ]);
    let case = convert_case("m.tar", &source, Format::Zip);
    case.ok();
    let modes: Vec<(String, Option<u32>)> = zip_items(&case.output_bytes())
        .into_iter()
        .map(|item| (item.name, item.mode))
        .collect();
    assert_eq!(
        modes,
        [
            ("plain".to_string(), Some(0o644)),
            ("suid".to_string(), Some(0o755))
        ]
    );
}

#[test]
fn plain_gz_becomes_one_entry_named_by_its_header_or_stem() {
    let data = noise(70_000, 4);
    // FNAME and MTIME from the header.
    let named = gzip_named(&data, "notes.txt", 1_600_000_001);
    let case = convert_case("notes.gz", &named, Format::Zip);
    assert_eq!(case.planned(), case.dir.join("notes.zip"));
    case.ok();
    assert_eq!(
        zip_items(&case.output_bytes()),
        [Item {
            name: "notes.txt".into(),
            node: Node::File(data.clone()),
            mtime: Some(1_600_000_001),
            mode: Some(0o644),
        }]
    );

    // No FNAME: the source's stem, and the source's own mtime.
    let case = convert_case("report.csv.gz", &gzip(&data), Format::Tar);
    assert_eq!(case.planned(), case.dir.join("report.csv.tar"));
    case.ok();
    let modified = time::from_system(fs::metadata(&case.src).unwrap().modified().unwrap());
    assert_eq!(
        tar_items(&case.output_bytes()),
        [Item {
            name: "report.csv".into(),
            node: Node::File(data.clone()),
            mtime: Some(modified),
            mode: Some(0o644),
        }]
    );

    // An FNAME that isn't one safe name falls back to the stem.
    for fname in ["../../evil", "dir/x", "CON", "trail."] {
        let case = convert_case("safe.bin.gz", &gzip_named(b"z", fname, 0), Format::Zip);
        case.ok();
        assert_eq!(
            zip_items(&case.output_bytes())[0].name,
            "safe.bin",
            "{fname}"
        );
    }

    // Several members, then zero padding: one file, all members in order.
    let mut multi = gzip(b"part one\n");
    multi.extend(gzip(b"part two\n"));
    multi.extend([0u8; 512]);
    let case = convert_case("log.txt.gz", &multi, Format::Tar);
    case.ok();
    let items = tar_items(&case.output_bytes());
    assert_eq!(items[0].node, Node::File(b"part one\npart two\n".to_vec()));
}

#[test]
fn the_real_container_decides_not_the_extension() {
    let source = sample_tar();
    let expected = without(tar_items(&source), &["src/again.rs", "src/pipe"]);
    // A .tar that is really gzip.
    let case = convert_case("really-gz.tar", &gzip(&source), Format::Zip);
    case.ok();
    assert_eq!(zip_items(&case.output_bytes()), expected);
    // A .zip that is really a tar: still gzipped byte for byte.
    let case = convert_case("really-tar.zip", &source, Format::Gz);
    case.ok();
    assert_eq!(gunzip(&case.output_bytes()), source);
    // A .gz that is really a ZIP.
    let zip = sample_zip();
    let case = convert_case("really-zip.gz", &zip, Format::Tar);
    case.ok();
    assert_eq!(tar_items(&case.output_bytes()), zip_items(&zip));
}

// ---- Extract Archive -------------------------------------------------------

#[test]
fn extract_zip_writes_the_tree_and_leaves_out_links_and_mac_metadata() {
    let mut source = zip_of(&[
        Zx::Dir("photos/"),
        Zx::Dir("photos/2024/"),
        Zx::File("photos/2024/ảnh chụp.jpg", &noise(30_000, 5)),
        Zx::Exec("photos/run.sh", b"#!/bin/sh\n"),
        Zx::Dir("photos/empty/"),
        Zx::File("u_.txt", b"cp437"),
        Zx::Link("photos/latest", "2024"),
        Zx::Dir("__MACOSX/"),
        Zx::File("__MACOSX/photos/._run.sh", b"apple"),
        Zx::File("photos/._run.sh", b"apple"),
    ]);
    // "u\x81.txt": not UTF-8, so the name is read as CP437 ("uü.txt").
    for at in find(&source, b"u_.txt") {
        source[at + 1] = 0x81;
    }
    let case = extract_case("photos.zip", &source);
    assert_eq!(case.planned(), case.dir.join("photos Extracted"));
    let outcome = case.ok();
    assert_eq!(
        outcome.left_out,
        LeftOut {
            links: 1,
            specials: 0,
            mac_metadata: 2,
            copies: 0,
        }
    );
    let mut items = zip_items(&source);
    assert!(items.iter().any(|item| item.name == "uü.txt"));
    items.retain(|item| item.name != "photos/latest");
    assert_extracted(&case.planned(), &items);

    // Again, into the next free name.
    let again = extract_case("photos.zip", &source);
    fs::create_dir(again.dir.join("photos Extracted")).unwrap();
    let planned = {
        let settings = Settings::new(again.dir.join("fallback"));
        let selection = crate::build_selection(std::slice::from_ref(&again.src), &settings);
        let op = Operation::tool(ToolKey::ExtractArchive);
        plan(&selection, &op, &settings).unwrap().jobs.remove(0)
    };
    assert_eq!(
        planned.output,
        JobOutput::Folder(again.dir.join("photos Extracted 2"))
    );
}

#[test]
fn extract_tar_and_tarballs() {
    let source = sample_tar();
    let mut expected = tar_items(&source);
    expected.retain(|item| matches!(item.node, Node::File(_) | Node::Dir));
    for (name, bytes) in [
        ("code.tar", source.clone()),
        ("code.tar.gz", gzip(&source)),
        ("code.tgz", gzip(&source)),
    ] {
        let case = extract_case(name, &bytes);
        assert_eq!(case.planned(), case.dir.join("code Extracted"));
        let outcome = case.ok();
        // Two symlinks and a hard link; one FIFO.
        assert_eq!(
            outcome.left_out,
            LeftOut {
                links: 3,
                specials: 1,
                mac_metadata: 0,
                copies: 0,
            },
            "{name}"
        );
        assert_extracted(&case.planned(), &expected);
    }
}

#[test]
fn extract_plain_gz_writes_one_file() {
    let mut multi = gzip_named(b"part one\n", "story.txt", 1_600_000_000);
    multi.extend(gzip(b"part two\n"));
    multi.extend([0u8; 100]);
    let case = extract_case("story.gz", &multi);
    assert_eq!(case.planned(), case.dir.join("story Extracted"));
    case.ok();
    let tree = tree_items(&case.planned());
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].name, "story.txt");
    assert_eq!(tree[0].node, Node::File(b"part one\npart two\n".to_vec()));
    assert_eq!(tree[0].mtime, Some(1_600_000_000));
}

#[test]
fn a_name_taken_meanwhile_is_replanned_without_replacing_anything() {
    let source = sample_zip();
    let case = extract_case("x.zip", &source);
    // An empty folder appears under the planned name after planning: a
    // rename would replace it, so Grape must not use one.
    let planned = case.planned();
    fs::create_dir(&planned).unwrap();
    let outcome = case.run_with(Limits::default(), None).unwrap();
    assert!(outcome.outcome.renamed);
    assert_eq!(outcome.outcome.outputs, [case.dir.join("x Extracted 2")]);
    assert!(listing(&planned).is_empty());
    assert_extracted(&outcome.outcome.outputs[0], &zip_items(&source));

    let convert = convert_case("x.zip", &source, Format::Tar);
    fs::write(convert.planned(), b"keep me").unwrap();
    let outcome = convert.run_with(Limits::default(), None).unwrap();
    assert!(outcome.outcome.renamed);
    assert_eq!(outcome.outcome.outputs, [convert.dir.join("x 2.tar")]);
    assert_eq!(fs::read(convert.planned()).unwrap(), b"keep me");
}

// ---- Refused archives ----------------------------------------------------------

/// Every way an archive with `name` can be used fails with an unsafe-name
/// error naming it, and writes nothing.
fn assert_refused(archive: &str, bytes: &[u8], shown: &str) {
    let targets: &[Format] = match Format::from_file_name(archive).unwrap() {
        Format::Zip => &[Format::Tar, Format::Gz],
        Format::Tar => &[Format::Zip, Format::Gz],
        _ => &[Format::Zip, Format::Tar],
    };
    let mut cases: Vec<Case> = targets
        .iter()
        .map(|to| convert_case(archive, bytes, *to))
        .collect();
    cases.push(extract_case(archive, bytes));
    for case in cases {
        let error = case.fails();
        assert_eq!(error.kind, ErrorKind::DecodeFailed, "{shown}");
        assert_eq!(
            error.user_message,
            format!("{archive} has an item with a name Grape won't write: {shown}."),
        );
        assert!(!error.detail.contains(shown), "a name reached the log");
    }
}

#[test]
fn zip_slip_and_windows_unsafe_names_refuse_the_zip() {
    for name in [
        "../x",
        "/etc/x",
        "C:\\x",
        "\\\\srv\\share\\x",
        "a/../../x",
        "a\\..\\..\\x",
        "\\\\?\\C:\\../b",
        "CON.txt",
        "x/NUL.tar.gz",
        "trail.",
        "a.txt:stream",
    ] {
        let bytes = zip_of(&[Zx::File("ok.txt", b"fine"), Zx::File(name, b"evil")]);
        assert_refused("evil.zip", &bytes, name);
    }
}

#[test]
fn traversal_in_a_tar_refuses_it() {
    for name in ["../../escape.txt", "/abs.txt", "a/../../x", "c:\\x", "PRN"] {
        let bytes = tar_of(&[Tx::File("ok.txt", b"fine"), Tx::Raw(name, b"hi")]);
        assert_refused("evil.tar", &bytes, name);
        assert_refused("evil.tar.gz", &gzip(&bytes), name);
    }
}

#[test]
fn two_items_with_one_name_refuse_the_archive() {
    let zip = zip_of(&[Zx::File("A.txt", b"1"), Zx::File("a.txt", b"2")]);
    let tar = tar_of(&[Tx::File("dir/X", b"1"), Tx::File("DIR/x", b"2")]);
    let file_then_folder = tar_of(&[Tx::File("a", b"1"), Tx::File("a/b", b"2")]);
    for (name, bytes, shown) in [
        ("dup.zip", zip, "a.txt"),
        ("dup.tar", tar.clone(), "DIR/x"),
        ("dup.tgz", gzip(&tar), "DIR/x"),
        ("clash.tar", file_then_folder, "a"),
    ] {
        let mut cases = vec![extract_case(name, &bytes)];
        if !name.ends_with(".zip") {
            // Including the conversion that keeps the bytes.
            cases.push(convert_case(name, &bytes, Format::Zip));
        }
        if name.ends_with(".tar") {
            cases.push(convert_case(name, &bytes, Format::Gz));
        }
        for case in cases {
            let error = case.fails();
            assert_eq!(
                error.user_message,
                format!("{name} has two items named {shown}.")
            );
        }
    }
}

#[test]
fn escaping_links_are_left_out_and_never_followed() {
    let zip = zip_of(&[
        Zx::File("a.txt", b"a"),
        Zx::Link("ok", "a.txt"),
        Zx::Link("evil", "../../etc/passwd"),
        Zx::Link("abs", "/etc/passwd"),
    ]);
    let case = convert_case("links.zip", &zip, Format::Tar);
    let outcome = case.ok();
    assert_eq!(outcome.left_out.links, 2);
    assert_eq!(
        tar_items(&case.output_bytes()),
        without(zip_items(&zip), &["abs", "evil"])
    );
    let case = extract_case("links.zip", &zip);
    assert_eq!(case.ok().left_out.links, 3);
    assert_eq!(listing(&case.planned()), ["a.txt"]);

    // The two-step attack: a link out, then a file "through" it.
    let tar = tar_of(&[
        Tx::Sym("sym", "../../outside"),
        Tx::File("sym/pwned.txt", b"x"),
    ]);
    let case = extract_case("two-step.tar", &tar);
    assert_eq!(case.ok().left_out.links, 1);
    let tree = tree_items(&case.planned());
    assert_eq!(
        tree.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        ["sym", "sym/pwned.txt"]
    );
    assert_eq!(listing(&case.dir.0), ["two-step Extracted", "two-step.tar"]);
}

#[test]
fn bombs_stop_at_the_bounds() {
    // "1 GiB of zeros": 1024 gzip members of 1 MiB each, about 1 MB on disk.
    let member = gzip(&vec![0u8; 1 << 20]);
    let bomb = member.repeat(1024);
    let small_total = Limits {
        max_total: 4 << 20,
        ..Limits::default()
    };
    for case in [
        extract_case("zeros.bin.gz", &bomb),
        convert_case("zeros.bin.gz", &bomb, Format::Zip),
    ] {
        let error = case.fails_with(small_total);
        assert_eq!(
            error.user_message,
            "zeros.bin.gz is larger unpacked than Grape will write (more than 4 MiB)."
        );
    }
    // The ratio applies past the floor.
    let ratio = Limits {
        max_total: u64::MAX,
        ratio_floor: 1 << 20,
        max_ratio: 10,
        ..Limits::default()
    };
    let error = extract_case("zeros.bin.gz", &bomb).fails_with(ratio);
    assert_eq!(
        error.user_message,
        "zeros.bin.gz is larger unpacked than Grape will write (more than 10 times its own size)."
    );

    // A ZIP is refused by its declared sizes, before anything is written.
    let zip = zip_of(&[Zx::File("zeros.bin", &vec![0u8; 4 << 20])]);
    let small = Limits {
        max_total: 1 << 20,
        ..Limits::default()
    };
    for case in [
        extract_case("zeros.zip", &zip),
        convert_case("zeros.zip", &zip, Format::Tar),
    ] {
        let error = case.fails_with(small);
        assert!(
            error.user_message.ends_with("(more than 1 MiB)."),
            "{error:?}"
        );
    }

    // A tarball is held while it streams.
    let tarball = gzip(&tar_of(&[Tx::File("zeros.bin", &vec![0u8; 4 << 20])]));
    for case in [
        extract_case("zeros.tgz", &tarball),
        convert_case("zeros.tgz", &tarball, Format::Zip),
        convert_case("zeros.tgz", &tarball, Format::Tar),
    ] {
        assert!(case
            .fails_with(small)
            .user_message
            .contains("more than 1 MiB"));
    }

    // Too many entries.
    let names: Vec<String> = (0..50).map(|i| format!("f{i}")).collect();
    let many_zip = zip_of(&names.iter().map(|n| Zx::File(n, b"")).collect::<Vec<_>>());
    let many_tar = tar_of(&names.iter().map(|n| Tx::File(n, b"")).collect::<Vec<_>>());
    let few = Limits {
        max_entries: 10,
        ..Limits::default()
    };
    for case in [
        extract_case("many.zip", &many_zip),
        extract_case("many.tar", &many_tar),
        convert_case("many.tar", &many_tar, Format::Gz),
    ] {
        assert!(case
            .fails_with(few)
            .user_message
            .ends_with("(more than 10 items)."));
    }
}

#[test]
fn encrypted_and_unsupported_zips_are_refused() {
    let mut encrypted = zip_of(&[Zx::File("secret.txt", b"secret")]);
    patch_zip(&mut encrypted, |header, local| {
        // General purpose flag bit 0: encrypted.
        header[if local { 6 } else { 8 }] |= 1;
    });
    for case in [
        extract_case("locked.zip", &encrypted),
        convert_case("locked.zip", &encrypted, Format::Tar),
    ] {
        assert_eq!(
            case.fails().user_message,
            "locked.zip is password-protected."
        );
    }

    let mut bzip2 = zip_of(&[Zx::File("data.txt", b"data")]);
    patch_zip(&mut bzip2, |header, local| {
        // Compression method 12: bzip2.
        header[if local { 8 } else { 10 }] = 12;
    });
    let error = extract_case("bz.zip", &bzip2).fails();
    assert_eq!(error.kind, ErrorKind::NotYetSupported);
    assert_eq!(
        error.user_message,
        "bz.zip uses bzip2 compression, which Grape can't read yet."
    );
}

#[test]
fn damaged_and_foreign_files_are_refused() {
    // Deflate data with a flipped byte.
    let mut zip = zip_of(&[Zx::File("data.bin", &noise(5_000, 6))]);
    zip[200] ^= 0xFF;
    let error = extract_case("broken.zip", &zip).fails();
    assert_eq!(error.user_message, "broken.zip isn't a readable ZIP file.");

    // A stored entry that claims more bytes than it has.
    let mut longer = zip_of(&[Zx::Stored("a.bin", &[7u8; 100])]);
    patch_zip(&mut longer, |header, local| {
        let at = if local { 22 } else { 24 };
        header[at..at + 4].copy_from_slice(&200u32.to_le_bytes());
    });
    // ... and one that claims fewer.
    let mut shorter = zip_of(&[Zx::Stored("a.bin", &[7u8; 100])]);
    patch_zip(&mut shorter, |header, local| {
        let at = if local { 22 } else { 24 };
        header[at..at + 4].copy_from_slice(&50u32.to_le_bytes());
    });
    for bytes in [longer, shorter] {
        let error = convert_case("liar.zip", &bytes, Format::Tar).fails();
        assert_eq!(error.user_message, "liar.zip isn't a readable ZIP file.");
    }

    // A gzip member with a bad checksum, and a truncated tar.
    let mut tarball = gzip(&sample_tar());
    let at = tarball.len() - 6;
    tarball[at] ^= 0xFF;
    for case in [
        extract_case("bad.tgz", &tarball),
        convert_case("bad.tgz", &tarball, Format::Tar),
    ] {
        assert_eq!(
            case.fails().user_message,
            "bad.tgz isn't a readable GZIP file."
        );
    }
    let tar = sample_tar();
    let error = extract_case("cut.tar", &tar[..2_000]).fails();
    assert_eq!(error.user_message, "cut.tar isn't a readable TAR file.");

    // No archive at all.
    let error = convert_case("text.zip", b"just some text", Format::Tar).fails();
    assert_eq!(error.kind, ErrorKind::DecodeFailed);
    assert_eq!(error.user_message, "text.zip doesn't look like a ZIP file.");
}

// ---- Hardening ---------------------------------------------------------------

#[test]
fn names_too_long_for_any_volume_refuse_the_archive() {
    let component = "x".repeat(256);
    let deep = format!("{}/x", vec!["d"; 2048].join("/"));
    for name in [component.as_str(), deep.as_str()] {
        let zip = zip_of(&[Zx::File("ok.txt", b"fine"), Zx::File(name, b"long")]);
        assert_refused("long.zip", &zip, &names::shown(name));
        let tar = tar_of(&[Tx::File("ok.txt", b"fine"), Tx::File(name, b"long")]);
        assert_refused("long.tar", &tar, &names::shown(name));
    }
}

#[test]
fn implied_folders_count_against_the_entry_bound() {
    // Six entries, but eighteen paths with the folders they imply.
    let names: Vec<String> = (0..6).map(|i| format!("d{i}/e{i}/f")).collect();
    let zip = zip_of(&names.iter().map(|n| Zx::File(n, b"")).collect::<Vec<_>>());
    let tar = tar_of(&names.iter().map(|n| Tx::File(n, b"")).collect::<Vec<_>>());
    let six = Limits {
        max_entries: 6,
        ..Limits::default()
    };
    for case in [
        extract_case("deep.zip", &zip),
        convert_case("deep.zip", &zip, Format::Tar),
        extract_case("deep.tar", &tar),
        convert_case("deep.tar", &tar, Format::Gz),
    ] {
        let error = case.fails_with(six);
        assert!(
            error.user_message.ends_with("(more than 6 items)."),
            "{error:?}"
        );
    }
    // Nine paths fit twice six.
    let shallow: Vec<String> = (0..6).map(|i| format!("d{}/f{i}", i % 3)).collect();
    let tar = tar_of(&shallow.iter().map(|n| Tx::File(n, b"")).collect::<Vec<_>>());
    extract_case("shallow.tar", &tar)
        .run_with(six, None)
        .unwrap();
}

#[test]
fn a_chain_of_links_cannot_climb_out() {
    // Followed by the OS, `d/evil` would land above the archive's root.
    let tar = tar_of(&[
        Tx::Dir("d/"),
        Tx::Sym("d/up", ".."),
        Tx::Sym("d/evil", "up/../.."),
        Tx::File("d/x", b"x"),
    ]);
    let expected = without(tar_items(&tar), &["d/evil"]);
    for (name, bytes) in [("chain.tar", tar.clone()), ("chain.tgz", gzip(&tar))] {
        let case = convert_case(name, &bytes, Format::Zip);
        assert_eq!(case.ok().left_out.links, 1, "{name}");
        assert_eq!(zip_items(&case.output_bytes()), expected, "{name}");
    }
    let zip = zip_of(&[
        Zx::Dir("d/"),
        Zx::Link("d/up", ".."),
        Zx::Link("d/evil", "up/../.."),
    ]);
    let case = convert_case("chain.zip", &zip, Format::Tar);
    assert_eq!(case.ok().left_out.links, 1);
    assert_eq!(
        tar_items(&case.output_bytes()),
        without(zip_items(&zip), &["d/evil"])
    );
}

#[test]
fn a_zip_naming_one_file_twice_is_refused_not_merged() {
    // The zip crate's writer refuses this, so the second name is patched.
    let mut zip = zip_of(&[Zx::File("a.txt", b"first"), Zx::File("b.txt", b"secnd")]);
    for at in find(&zip, b"b.txt") {
        zip[at] = b'a';
    }
    for case in [
        extract_case("dup.zip", &zip),
        convert_case("dup.zip", &zip, Format::Tar),
        convert_case("dup.zip", &zip, Format::Gz),
    ] {
        assert_eq!(
            case.fails().user_message,
            "dup.zip has two items named a.txt."
        );
    }

    // A folder twice is one folder.
    let mut zip = zip_of(&[
        Zx::Dir("dd1/"),
        Zx::File("dd1/x.txt", b"x"),
        Zx::Dir("dd2/"),
    ]);
    for at in find(&zip, b"dd2/") {
        zip[at + 2] = b'1';
    }
    let case = convert_case("twice.zip", &zip, Format::Tar);
    case.ok();
    let names: Vec<String> = tar_items(&case.output_bytes())
        .into_iter()
        .map(|item| item.name)
        .collect();
    assert_eq!(names, ["dd1", "dd1/x.txt"]);
    extract_case("twice.zip", &zip).ok();
}

#[test]
fn a_tar_naming_a_folder_twice_converts_and_extracts() {
    let tar = tar_of(&[
        Tx::Dir("d/"),
        Tx::File("d/x.txt", b"x"),
        Tx::Dir("d/"),
        Tx::Dir("D/"),
    ]);
    let case = convert_case("twice.tar", &tar, Format::Zip);
    case.ok();
    let names: Vec<String> = zip_items(&case.output_bytes())
        .into_iter()
        .map(|item| item.name)
        .collect();
    assert_eq!(names, ["d", "d/x.txt"]);
    let case = extract_case("twice.tar", &tar);
    case.ok();
    assert_eq!(listing(&case.planned()), ["d"]);
}

#[test]
fn a_kept_tar_must_name_each_entry_one_way() {
    let file = |name: &[u8]| raw_entry(name, EntryType::Regular, b"data");
    let pax =
        |records: &[Vec<u8>]| raw_entry(b"PaxHeaders/x", EntryType::XHeader, &records.concat());
    let two_paths = tar_raw_of(&[
        pax(&[
            pax_record("path", b"ok.txt"),
            pax_record("path", b"../../evil.txt"),
        ]),
        file(b"ok.txt"),
    ]);
    // The tar crate takes a GNU long name over the PAX path.
    let long_and_pax = tar_raw_of(&[
        pax(&[pax_record("path", b"../../evil.txt")]),
        raw_entry(b"././@LongLink", EntryType::GNULongName, b"ok.txt\0"),
        file(b"ok.txt"),
    ]);
    let global = tar_raw_of(&[
        raw_entry(
            b"pax_global_header",
            EntryType::XGlobalHeader,
            &pax_record("path", b"../../evil.txt"),
        ),
        file(b"ok.txt"),
    ]);
    for (name, bytes) in [
        ("two-paths.tar", &two_paths),
        ("long-and-pax.tar", &long_and_pax),
        ("global.tar", &global),
    ] {
        // Kept byte for byte, it would mean `../../evil.txt` to other tools.
        let error = convert_case(name, bytes, Format::Gz).fails();
        assert_eq!(
            error.user_message,
            format!("{name} has an item with a name Grape won't write: ../../evil.txt."),
        );
        let error = convert_case(&format!("{name}.gz"), &gzip(bytes), Format::Tar).fails();
        assert!(error.user_message.ends_with("../../evil.txt."), "{error:?}");
        // Re-packed or extracted, Grape writes its own reading: `ok.txt`.
        let case = convert_case(name, bytes, Format::Zip);
        case.ok();
        assert_eq!(zip_items(&case.output_bytes())[0].name, "ok.txt");
        let case = extract_case(name, bytes);
        case.ok();
        assert_eq!(listing(&case.planned()), ["ok.txt"]);
    }

    // One PAX path that names the entry (the usual long-name case) is kept.
    let long = "long-".repeat(40) + ".txt";
    let usual = tar_raw_of(&[
        pax(&[pax_record("path", long.as_bytes())]),
        file(b"long-long"),
    ]);
    let case = convert_case("usual.tar", &usual, Format::Gz);
    case.ok();
    assert_eq!(gunzip(&case.output_bytes()), usual);
    assert_eq!(tar_items(&usual)[0].name, long);

    // A PAX record the tar crate can't read could mean anything elsewhere.
    let broken = tar_raw_of(&[pax(&[b"garbage\n".to_vec()]), file(b"ok.txt")]);
    let error = convert_case("broken.tar", &broken, Format::Gz).fails();
    assert_eq!(error.user_message, "broken.tar isn't a readable TAR file.");
}

/// A GNU sparse tar of 2048 bytes holding a file of `real` bytes: holes,
/// then 512 bytes of data.
fn sparse_tar(real: u64) -> Vec<u8> {
    let mut header = Header::new_gnu();
    header.set_path("sparse.img").unwrap();
    header.set_entry_type(EntryType::GNUSparse);
    header.set_size(512);
    header.set_mode(0o644);
    header.set_mtime(T as u64);
    let gnu = header.as_gnu_mut().unwrap();
    gnu.sparse[0].set_offset(real - 512);
    gnu.sparse[0].set_length(512);
    gnu.set_real_size(real);
    header.set_cksum();
    let mut out = header.as_bytes().to_vec();
    out.extend(noise(512, 7));
    out.extend([0u8; 1024]);
    out
}

#[test]
fn sparse_holes_count_only_where_they_are_written() {
    let tar = sparse_tar(10 << 20);
    assert_eq!(tar.len(), 2048);
    let small = Limits {
        max_total: 5 << 20,
        ..Limits::default()
    };
    // Kept byte for byte, the tar is 2 KiB, whatever its holes.
    let case = convert_case("sparse.tar", &tar, Format::Gz);
    case.run_with(small, None).unwrap();
    assert_eq!(gunzip(&case.output_bytes()), tar);
    let case = convert_case("sparse.tar.gz", &gzip(&tar), Format::Tar);
    case.run_with(small, None).unwrap();
    assert_eq!(case.output_bytes(), tar);
    // Written out, it is 10 MiB.
    for case in [
        convert_case("sparse.tar", &tar, Format::Zip),
        extract_case("sparse.tar", &tar),
    ] {
        assert!(case
            .fails_with(small)
            .user_message
            .ends_with("(more than 5 MiB)."));
    }
    let case = extract_case("sparse.tar", &tar);
    case.ok();
    let image = fs::read(case.planned().join("sparse.img")).unwrap();
    assert_eq!(image.len(), 10 << 20);
    assert_eq!(image[(10 << 20) - 512..], noise(512, 7)[..]);
}

#[test]
fn a_zip_claiming_too_many_entries_is_refused_before_it_is_parsed() {
    // The end record says 50 entries; the directory has 3. Parsed, it would
    // be damaged; the count alone already refuses it.
    let mut zip = zip_of(&[
        Zx::File("a", b"1"),
        Zx::File("b", b"2"),
        Zx::File("c", b"3"),
    ]);
    let eocd = find(&zip, b"PK\x05\x06")[0];
    for at in [eocd + 8, eocd + 10] {
        zip[at..at + 2].copy_from_slice(&50u16.to_le_bytes());
    }
    let few = Limits {
        max_entries: 10,
        ..Limits::default()
    };
    let error = extract_case("lies.zip", &zip).fails_with(few);
    assert_eq!(
        error.user_message,
        "lies.zip is larger unpacked than Grape will write (more than 10 items)."
    );
    let error = extract_case("lies.zip", &zip).fails();
    assert_eq!(error.user_message, "lies.zip isn't a readable ZIP file.");

    // A ZIP64 end record claiming a million, in 202 bytes.
    let mut zip64 = b"PK\x03\x04".to_vec();
    zip64.extend([0u8; 100]);
    let record_at = zip64.len() as u64;
    zip64.extend(b"PK\x06\x06");
    zip64.extend(44u64.to_le_bytes());
    zip64.extend([45, 0, 45, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    zip64.extend(1_000_000u64.to_le_bytes());
    zip64.extend(1_000_000u64.to_le_bytes());
    zip64.extend(0u64.to_le_bytes());
    zip64.extend(0u64.to_le_bytes());
    zip64.extend(b"PK\x06\x07");
    zip64.extend(0u32.to_le_bytes());
    zip64.extend(record_at.to_le_bytes());
    zip64.extend(1u32.to_le_bytes());
    zip64.extend(b"PK\x05\x06");
    zip64.extend([0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
    zip64.extend([0xFF; 8]);
    zip64.extend([0, 0]);
    assert_eq!(zip64.len(), 202);
    let error = convert_case("million.zip", &zip64, Format::Tar).fails();
    assert_eq!(
        error.user_message,
        "million.zip is larger unpacked than Grape will write (more than 100,000 items)."
    );

    // No end record near the end, where the format puts it.
    let mut far = zip_of(&[Zx::File("a", b"1")]);
    far.extend(vec![0u8; 70_000]);
    let error = extract_case("far.zip", &far).fails();
    assert_eq!(error.user_message, "far.zip isn't a readable ZIP file.");
}

/// A sink that notes how much of the source was read as each file began.
#[derive(Default)]
struct Recorder {
    at: Vec<u64>,
}

impl Sink for Recorder {
    fn dir(&mut self, _run: &Run<'_>, _entry: &Entry) -> Result<(), Failure> {
        Ok(())
    }

    fn file(
        &mut self,
        run: &Run<'_>,
        _entry: &Entry,
        _size: Option<u64>,
        data: &mut dyn Read,
    ) -> Result<(), Failure> {
        self.at.push(run.consumed());
        std::io::copy(data, &mut std::io::sink())
            .map_err(|e| run.failure(&e, Side::Write, "test"))?;
        Ok(())
    }

    fn symlink(&mut self, _run: &Run<'_>, _entry: &Entry, _target: &str) -> Result<(), Failure> {
        Ok(())
    }
}

#[test]
fn zip_progress_follows_the_entries_written() {
    let names: Vec<String> = (0..2_000).map(|i| format!("f{i:04}.bin")).collect();
    let data: Vec<Vec<u8>> = (0..2_000).map(|i| noise(1_024, i)).collect();
    let entries: Vec<Zx> = names
        .iter()
        .zip(&data)
        .map(|(name, data)| Zx::Stored(name, data))
        .collect();
    let bytes = zip_of(&entries);
    let dir = TestDir::new();
    let src = dir.join("many.zip");
    fs::write(&src, &bytes).unwrap();
    let cancel = CancelToken::new();
    let mut progress = |_: f32| {};
    let len = bytes.len() as u64;
    let run = Run::new(&cancel, Limits::default(), len, &mut progress);
    let mut walk = Walk::new(Mode::Repack, &run.limits);
    let mut sink = Recorder::default();
    source::walk_zip(&run, File::open(&src).unwrap(), &src, &mut walk, &mut sink).unwrap();
    assert_eq!(sink.at.len(), 2_000);
    // Each byte is read about once: half way through the entries, about
    // half of the file (each entry used to read two 64 KiB buffers).
    let middle = sink.at[1_000];
    assert!(
        (len * 4 / 10..len * 6 / 10).contains(&middle),
        "{middle} of {len}"
    );
    assert!(run.consumed() <= len, "{} of {len}", run.consumed());
}

#[test]
fn the_mark_lands_on_the_published_file_however_it_was_placed() {
    let case = convert_case("web.zip", &sample_zip(), Format::Tar);
    let dest = case.planned();
    let names = Names::new(&case.src, &dest, Format::Zip, Format::Tar);
    let (temp, mut file) = TempFile::create(&case.dir.0, "tar").unwrap();
    file.write_all(b"archive").unwrap();
    drop(file);
    // Not on the temp file: what the copy fallback would leave behind. (Off
    // Windows the "stream" is a file of its own, which a hard link doesn't
    // carry either.)
    let mark = Mark::with(b"[ZoneTransfer]\r\nZoneId=3\r\n");
    let outcome = finalize_marked(
        &temp,
        &case.job,
        &case.src,
        &dest,
        &case.dir.0,
        &names,
        &mark,
    )
    .unwrap();
    drop(temp);
    assert_eq!(outcome.outputs, std::slice::from_ref(&dest));
    assert_eq!(fs::read(&dest).unwrap(), b"archive");
    assert_eq!(
        fs::read(motw::zone_stream(&dest)).unwrap(),
        b"[ZoneTransfer]\r\nZoneId=3\r\n"
    );
}

#[test]
fn a_plain_gz_of_a_dot_underscore_file_still_gives_it() {
    let data = b"a file, not macOS metadata";
    let named = gzip_named(data, "._notes.txt", 1_600_000_001);
    for to in [Format::Zip, Format::Tar] {
        let case = convert_case("notes.gz", &named, to);
        let outcome = case.ok();
        assert_eq!(outcome.left_out, LeftOut::default());
        let items = if to == Format::Zip {
            zip_items(&case.output_bytes())
        } else {
            tar_items(&case.output_bytes())
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "._notes.txt");
        assert_eq!(items[0].node, Node::File(data.to_vec()));
    }
    let case = extract_case("notes.gz", &named);
    assert_eq!(case.ok().left_out, LeftOut::default());
    assert_eq!(listing(&case.planned()), ["._notes.txt"]);
}

#[test]
fn windows_1252_tar_names_are_read_not_refused() {
    let tar = tar_raw_of(&[raw_entry(
        b"caf\xE9 \x93menu\x94.txt",
        EntryType::Regular,
        b"x",
    )]);
    let case = extract_case("old.tar", &tar);
    case.ok();
    assert_eq!(listing(&case.planned()), ["café “menu”.txt"]);
    let case = convert_case("old.tar", &tar, Format::Zip);
    case.ok();
    assert_eq!(zip_items(&case.output_bytes())[0].name, "café “menu”.txt");
}

#[test]
fn empty_tars_and_tarballs_are_archives_with_nothing_in_them() {
    // The tar crate writes 1024 zero bytes; GNU tar a 10,240-byte record.
    for len in [1_024, 10_240] {
        let tar = vec![0u8; len];
        let case = convert_case("empty.tar", &tar, Format::Zip);
        case.ok();
        assert!(zip_items(&case.output_bytes()).is_empty());
        let case = convert_case("empty.tar", &tar, Format::Gz);
        case.ok();
        assert_eq!(gunzip(&case.output_bytes()), tar);
        let case = extract_case("empty.tar", &tar);
        case.ok();
        assert!(listing(&case.planned()).is_empty());

        let tarball = gzip(&tar);
        let case = convert_case("empty.tar.gz", &tarball, Format::Tar);
        case.ok();
        assert_eq!(case.output_bytes(), tar);
        let case = convert_case("empty.tar.gz", &tarball, Format::Zip);
        case.ok();
        assert!(zip_items(&case.output_bytes()).is_empty());
        let case = extract_case("empty.tar.gz", &tarball);
        case.ok();
        assert!(listing(&case.planned()).is_empty());
    }
    // Zeros that can't be a tar: less than its end marker, or not whole blocks.
    for len in [512, 1_000] {
        let error = extract_case("zeros.tar", &vec![0u8; len]).fails();
        assert_eq!(
            error.user_message,
            "zeros.tar doesn't look like a TAR file."
        );
    }
    // More zeros than GNU's record, gzipped: a plain file (a disk image).
    let image = vec![0u8; 20_480];
    let case = extract_case("disk.img.gz", &gzip(&image));
    case.ok();
    assert_eq!(fs::read(case.planned().join("disk.img")).unwrap(), image);
}

#[cfg(target_os = "linux")]
#[test]
fn a_name_too_long_for_the_volume_is_named_as_the_entry() {
    // 200 UTF-16 units, which NTFS holds; 600 bytes, which ext4 doesn't.
    let name = format!("dir/{}", "漢".repeat(200));
    let tar = tar_of(&[Tx::File(&name, b"x")]);
    let error = extract_case("long.tar", &tar).fails();
    assert_eq!(
        error.user_message,
        format!(
            "long.tar has an item with a name Grape won't write: {}.",
            names::shown(&name)
        )
    );
    // Converting creates no file of that name.
    let case = convert_case("long.tar", &tar, Format::Zip);
    case.ok();
    assert_eq!(zip_items(&case.output_bytes())[0].name, name);
}

// ---- Cancel ----------------------------------------------------------------

#[test]
fn cancel_midway_leaves_nothing() {
    let big: Vec<Vec<u8>> = (0..4).map(|i| noise(300_000, 10 + i)).collect();
    let zip = zip_of(&[
        Zx::File("one.bin", &big[0]),
        Zx::File("two.bin", &big[1]),
        Zx::File("three.bin", &big[2]),
        Zx::File("four.bin", &big[3]),
    ]);
    let tar = tar_of(&[
        Tx::File("one.bin", &big[0]),
        Tx::File("two.bin", &big[1]),
        Tx::File("three.bin", &big[2]),
        Tx::File("four.bin", &big[3]),
    ]);
    let plain = gzip(&big.concat());
    let cases = [
        convert_case("c.zip", &zip, Format::Tar),
        convert_case("c.zip", &zip, Format::Gz),
        convert_case("c.tar", &tar, Format::Zip),
        convert_case("c.tar", &tar, Format::Gz),
        convert_case("c.tgz", &gzip(&tar), Format::Tar),
        convert_case("c.bin.gz", &plain, Format::Zip),
        extract_case("c.zip", &zip),
        extract_case("c.tar", &tar),
        extract_case("c.tgz", &gzip(&tar)),
        extract_case("c.bin.gz", &plain),
    ];
    for case in cases {
        let error = case.run_with(Limits::default(), Some(0.5)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled, "{}", case.src.display());
        case.assert_no_output();
    }
}

#[test]
fn a_cancelled_job_does_not_start() {
    let case = extract_case("x.zip", &sample_zip());
    let ctx = EngineCtx::default();
    ctx.cancel.cancel();
    let dest = case.planned();
    let error = extract(
        &case.job,
        &case.src,
        &dest,
        Format::Zip,
        &ctx,
        &mut |_: f32| {},
        None,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    case.assert_no_output();
    case.source.assert_unchanged();
}

// ---- Platform specifics ------------------------------------------------------

#[cfg(unix)]
#[test]
fn extracted_modes_keep_owner_access_and_drop_special_bits() {
    use std::os::unix::fs::PermissionsExt;
    let tar = tar_of(&[
        Tx::Mode("suid", b"x", 0o4777),
        Tx::Mode("locked", b"y", 0o000),
        Tx::Mode("run", b"z", 0o755),
    ]);
    let case = extract_case("modes.tar", &tar);
    case.ok();
    let mode = |name: &str| {
        fs::metadata(case.planned().join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("suid"), 0o755);
    assert_eq!(mode("locked"), 0o600);
    assert_eq!(mode("run"), 0o755);
}

#[cfg(windows)]
#[test]
fn mark_of_the_web_follows_converted_and_extracted_files() {
    let mark = b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.com/\r\n";
    let zip = sample_zip();
    let zip_without_links = zip_of(&[
        Zx::Dir("docs/"),
        Zx::File("docs/readme.txt", b"hello\n"),
        Zx::File("top.txt", b"top"),
    ]);
    for (name, bytes, op) in [
        ("web.zip", &zip, Operation::convert(Format::Tar)),
        (
            "web.zip",
            &zip_without_links,
            Operation::tool(ToolKey::ExtractArchive),
        ),
    ] {
        let dir = TestDir::new();
        let src = dir.join(name);
        fs::write(&src, bytes).unwrap();
        fs::write(motw::zone_stream(&src), mark).unwrap();
        let settings = Settings::new(dir.join("fallback"));
        let selection = crate::build_selection(std::slice::from_ref(&src), &settings);
        let job = plan(&selection, &op, &settings).unwrap().jobs.remove(0);
        let ctx = EngineCtx::default();
        match (&job.output, job.op) {
            (JobOutput::File(dest), Operation::Convert { to }) => {
                convert(
                    &job,
                    &src,
                    dest,
                    Format::Zip,
                    to,
                    &ctx,
                    &mut |_: f32| {},
                    None,
                )
                .unwrap();
                assert_eq!(fs::read(motw::zone_stream(dest)).unwrap(), mark);
            }
            (JobOutput::Folder(dest), _) => {
                extract(&job, &src, dest, Format::Zip, &ctx, &mut |_: f32| {}, None).unwrap();
                // Entry `i` of `zip_of` is dated `T + 7200 * i`.
                for (file, i) in [("docs/readme.txt", 1), ("top.txt", 2)] {
                    let path = dest.join(file);
                    assert_eq!(fs::read(motw::zone_stream(&path)).unwrap(), mark, "{file}");
                    // Writing the stream must not leave its own time on the file.
                    let modified = fs::metadata(&path).unwrap().modified().unwrap();
                    assert_eq!(time::from_system(modified), T + 7200 * i, "{file}");
                }
            }
            other => panic!("{other:?}"),
        }
        assert_no_leftovers(&dir.0);
    }
}

// ---- What run_job returns (#22) --------------------------------------------

#[test]
fn into_outcome_notes_only_what_was_left_out() {
    let wrote = JobOutcome {
        outputs: vec![PathBuf::from("x Extracted")],
        ..JobOutcome::default()
    };
    let nothing = ArchiveOutcome {
        outcome: wrote.clone(),
        left_out: LeftOut::default(),
    };
    assert_eq!(nothing.into_outcome(), wrote);
    // macOS metadata counts as a fact too; the app decides not to word it.
    let mac = LeftOut {
        mac_metadata: 2,
        ..LeftOut::default()
    };
    let outcome = ArchiveOutcome {
        outcome: wrote.clone(),
        left_out: mac,
    }
    .into_outcome();
    assert_eq!(outcome.outputs, wrote.outputs);
    assert_eq!(outcome.notes, vec![crate::Note::LeftOut(mac)]);
}
