//! The RAR tests that run Grape's real bsdtar (feature `rar-it`, D27).
//! `GRAPE_TEST_BSDTAR` names the binary; the tests fail, not skip,
//! without it. libarchive's own test files ([`super::vectors`]) and every
//! writer case the preflight accepts go through it, converted to ZIP, TAR
//! and GZIP and extracted, and the outputs are read back.
//!
//! `cargo test -p grape-engines --features rar-it --lib archive::rar`
//!
//! On Windows each bsdtar of one test goes into a Job Object of its own as
//! the app makes it; under cargo that job is nested in cargo's, so it has
//! no UI limits. CI also runs this test binary outside cargo with
//! `GRAPE_TEST_JOB_UI=1`: then one test starts it again as a child that
//! breaks away from the runner's job, where each bsdtar's job must have
//! the UI limits.

#![allow(clippy::unwrap_used)]

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::Command;
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
use grape_platform::windows::job::JobGuard;

use grape_core::{Format, JobOutput, Operation, ToolKey};

use super::super::stream::Run;
use super::super::tests::{
    case as new_case, gunzip, tar_items, tree_items, zip_items, Case, Item, Node, TestDir,
};
use super::super::{convert_with, extract_with, ArchiveOutcome, Failure, LeftOut, Limits};
#[cfg(windows)]
use super::bsdtar::MAX_MEMORY;
use super::header::{preflight, Expect, Plan};
use super::vectors;
use super::writer::{self, File4, File5, A, UNIX_2020};
use super::{Launch, RarPaths};
#[cfg(windows)]
use crate::helper::{Contain, Contained};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError};

/// Grape's bsdtar, as `GRAPE_TEST_BSDTAR` names it.
fn bsdtar_path() -> PathBuf {
    let path = std::env::var_os("GRAPE_TEST_BSDTAR")
        .expect("GRAPE_TEST_BSDTAR must name Grape's bsdtar (see docs/TESTING.md)");
    let path = PathBuf::from(path);
    assert!(
        path.is_file(),
        "GRAPE_TEST_BSDTAR: {} isn't a file",
        path.display()
    );
    path
}

fn bsdtar() -> Launch {
    Launch::bsdtar(&RarPaths {
        bsdtar: bsdtar_path(),
    })
}

// ---- The vectors -------------------------------------------------------------

/// Decodes uuencoded `text`: the lines between `begin` and `end`. A line
/// shorter than its length byte says is padded with zeros.
fn uudecode(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    assert!(
        lines.next().is_some_and(|l| l.starts_with("begin ")),
        "no begin line"
    );
    for line in lines {
        if line == "end" {
            return out;
        }
        let bytes = line.as_bytes();
        let Some((&len, rest)) = bytes.split_first() else {
            continue;
        };
        let value = |c: u8| (c.wrapping_sub(b' ')) & 0x3F;
        let len = usize::from(value(len));
        let mut decoded = Vec::with_capacity(len + 2);
        for group in rest.chunks(4) {
            let mut quad = [0u8; 4];
            for (slot, &c) in quad.iter_mut().zip(group) {
                *slot = value(c);
            }
            decoded.push((quad[0] << 2) | (quad[1] >> 4));
            decoded.push((quad[1] << 4) | (quad[2] >> 2));
            decoded.push((quad[2] << 6) | quad[3]);
        }
        decoded.resize(len, 0);
        out.extend(decoded);
    }
    panic!("no end line");
}

/// SHA-256 (FIPS 180-4), for checking the vectors.
fn sha256(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = bytes.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend(((bytes.len() as u64) * 8).to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (slot, add) in h.iter_mut().zip(v) {
            *slot = slot.wrapping_add(add);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

/// The vectors, their `.uu` and decoded sizes, and the decoded SHA-256
/// (recorded when they were embedded, from the pinned tarball).
const VECTORS: [(&str, &str, usize, usize, &str); 8] = [
    (
        "RAR4_PLAIN",
        vectors::RAR4_PLAIN,
        505,
        336,
        "d421b86f6290aefad61b2a36737253b2b30fe27c156bd95abfc230f24fe0307e",
    ),
    (
        "RAR4_FILTER",
        vectors::RAR4_FILTER,
        129_976,
        94_301,
        "b058412adcd3329a868cb66561a224eb0acf22ad2d380ef5d9e17e432a47d6a8",
    ),
    (
        "RAR4_UNICODE",
        vectors::RAR4_UNICODE,
        967,
        666,
        "3dc3d2a8f3f6cbfeeb6f45b3bb09fa7b2bf4b52e10e80df29217fdeaa9dbda45",
    ),
    (
        "RAR5_COMPRESSED",
        vectors::RAR5_COMPRESSED,
        657,
        436,
        "68f9d7e2662cd5bc5607ba9c6bf1edbf121cf3b08011a7d8643091791750a7c8",
    ),
    (
        "RAR5_SOLID",
        vectors::RAR5_SOLID,
        1_496,
        1_050,
        "1149dda288494ab6c954d0e3f2a8831318aaf9c69e7da741729bb0c681f924b5",
    ),
    (
        "RAR5_UNICODE",
        vectors::RAR5_UNICODE,
        423,
        269,
        "062c77fb1d47efbd5a468609a9c283ecb0b4cae43b0f6778935c41a975cedee9",
    ),
    (
        "RAR4_ENCRYPTED",
        vectors::RAR4_ENCRYPTED,
        483,
        311,
        "c30dc6d10e59aaab1c863afa469d188c644a22d67d6e50ac7ca887e520f5fc8b",
    ),
    (
        "RAR5_PART01",
        vectors::RAR5_PART01,
        21_226,
        15_360,
        "2f3894dc4b8a1bba90830ef1f89f457b0295b6f9030c70febd57283721bc2ce3",
    ),
];

#[test]
fn the_vectors_decode_to_what_was_embedded() {
    assert_eq!(
        sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    for (name, text, uu, size, digest) in VECTORS {
        assert_eq!(text.len(), uu, "{name}");
        let bytes = uudecode(text);
        assert_eq!(bytes.len(), size, "{name}");
        assert_eq!(sha256(&bytes), digest, "{name}");
    }
}

/// What a vector's entry is, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Is {
    File(usize),
    Dir,
    Link(&'static str),
}

fn file(name: &'static str, size: usize) -> (&'static str, Is) {
    (name, Is::File(size))
}

fn dir(name: &'static str) -> (&'static str, Is) {
    (name, Is::Dir)
}

fn link(name: &'static str, target: &'static str) -> (&'static str, Is) {
    (name, Is::Link(target))
}

/// Items as names and kinds, sorted.
fn shape(items: &[Item]) -> Vec<(String, Is)> {
    let mut out: Vec<(String, Is)> = items
        .iter()
        .filter_map(|item| {
            let is = match &item.node {
                Node::File(data) => Is::File(data.len()),
                Node::Dir => Is::Dir,
                Node::Link(target) => Is::Link(Box::leak(target.clone().into_boxed_str())),
                Node::HardLink(_) | Node::Fifo => return None,
            };
            Some((item.name.clone(), is))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn want(entries: &[(&'static str, Is)]) -> Vec<(String, Is)> {
    let mut out: Vec<(String, Is)> = entries
        .iter()
        .map(|(name, is)| (name.to_string(), is.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The regular files' sizes and CRC32s, sorted.
fn file_crcs(items: &[Item]) -> Vec<(u64, u32)> {
    let mut out: Vec<(u64, u32)> = items
        .iter()
        .filter_map(|item| match &item.node {
            Node::File(data) => Some((data.len() as u64, crc32fast::hash(data))),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out
}

/// The preflight's plan of `bytes`.
fn plan(bytes: &[u8]) -> Result<Plan, Failure> {
    let dir = TestDir::new();
    let path = dir.join("x.rar");
    fs::write(&path, bytes).unwrap();
    let mut file = File::open(&path).unwrap();
    let cancel = CancelToken::new();
    let mut progress = |_: f32| {};
    let run = Run::new(
        &cancel,
        Limits::default(),
        bytes.len() as u64,
        &mut progress,
    );
    preflight(&run, &mut file, bytes.len() as u64)
}

/// The RAR headers' sizes and CRC32s of the regular files, sorted.
fn header_crcs(bytes: &[u8]) -> Vec<(u64, u32)> {
    let mut out: Vec<(u64, u32)> = plan(bytes)
        .unwrap()
        .entries
        .iter()
        .filter_map(|expect| match expect {
            Expect::Data {
                size,
                crc: Some(crc),
            } => Some((*size, *crc)),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out
}

fn run(case: &Case) -> Result<ArchiveOutcome, GrapeError> {
    case.run_rar(Limits::default(), None, Some(&bsdtar()))
}

/// What a converted archive holds.
fn converted(case: &Case, to: Format) -> Vec<Item> {
    let bytes = case.output_bytes();
    match to {
        Format::Zip => zip_items(&bytes),
        Format::Tar => tar_items(&bytes),
        Format::Gz => tar_items(&gunzip(&bytes)),
        other => panic!("{other:?}"),
    }
}

/// Converts `bytes` to ZIP, TAR and GZIP and extracts it; checks each
/// output's entries against `entries` (links left out when extracting)
/// and its files' CRC32s against the RAR's headers. Returns the
/// conversions' and the extraction's [`LeftOut`].
fn round_trip(name: &str, bytes: &[u8], entries: &[(&'static str, Is)]) -> (LeftOut, LeftOut) {
    let crcs = header_crcs(bytes);
    let mut converted_left = None;
    for to in [Format::Zip, Format::Tar, Format::Gz] {
        let case = new_case("x.rar", bytes, Operation::convert(to));
        let outcome = run(&case).unwrap_or_else(|e| panic!("{name} → {to:?}: {e:?}"));
        let items = converted(&case, to);
        assert_eq!(shape(&items), want(entries), "{name} → {to:?}");
        assert_eq!(file_crcs(&items), crcs, "{name} → {to:?}");
        converted_left = Some(outcome.left_out);
    }
    let case = new_case("x.rar", bytes, Operation::tool(ToolKey::ExtractArchive));
    let outcome = run(&case).unwrap_or_else(|e| panic!("{name} extracted: {e:?}"));
    let items = tree_items(&case.planned());
    let on_disk: Vec<(&'static str, Is)> = entries
        .iter()
        .filter(|(_, is)| !matches!(is, Is::Link(_)))
        .cloned()
        .collect();
    assert_eq!(shape(&items), want(&on_disk), "{name} extracted");
    assert_eq!(file_crcs(&items), crcs, "{name} extracted");
    (converted_left.unwrap_or_default(), outcome.left_out)
}

#[test]
fn libarchives_own_rar_files_convert_and_extract() {
    let decode = |name: &str| {
        let (_, text, _, _, _) = VECTORS.iter().find(|v| v.0 == name).unwrap();
        uudecode(text)
    };
    let (conv, ext) = round_trip(
        "RAR4_PLAIN",
        &decode("RAR4_PLAIN"),
        &[
            file("test.txt", 20),
            link("testlink", "test.txt"),
            file("testdir/test.txt", 20),
            dir("testdir"),
            dir("testemptydir"),
        ],
    );
    assert_eq!((conv.links, ext.links), (0, 1));
    round_trip(
        "RAR4_FILTER",
        &decode("RAR4_FILTER"),
        &[file("bsdcat.exe", 204_288)],
    );
    let (conv, ext) = round_trip(
        "RAR4_UNICODE",
        &decode("RAR4_UNICODE"),
        &[
            file("表だよ/新しいフォルダ/新規テキスト ドキュメント.txt", 0),
            file("表だよ/漢字長いファイル名long-filename-in-漢字.txt", 5),
            dir("表だよ/新しいフォルダ"),
            dir("表だよ"),
            link(
                "表だよ/ファイル",
                "漢字長いファイル名long-filename-in-漢字.txt",
            ),
            file("abcdefghijklmnopqrsテスト.txt", 16),
        ],
    );
    assert_eq!((conv.links, ext.links), (0, 1));
    round_trip(
        "RAR5_COMPRESSED",
        &decode("RAR5_COMPRESSED"),
        &[file("test.bin", 1_200)],
    );
    round_trip(
        "RAR5_SOLID",
        &decode("RAR5_SOLID"),
        &[
            file("test.bin", 1_200),
            file("test1.bin", 4_096),
            file("test2.bin", 4_096),
            file("test3.bin", 4_096),
            file("test4.bin", 4_096),
            file("test5.bin", 4_096),
            file("test6.bin", 4_096),
        ],
    );
    let (conv, ext) = round_trip(
        "RAR5_UNICODE",
        &decode("RAR5_UNICODE"),
        &[file("👋🌎.txt", 13), link("𝒮𝓎𝓂𝒷𝑜𝓁𝒾𝒸 𝐿𝒾𝓃𝓀.txt", "👋🌎.txt")],
    );
    // The hard link is left out either way; the symlink when extracting.
    assert_eq!((conv.links, ext.links), (1, 2));

    for (name, message) in [
        ("RAR4_ENCRYPTED", "x.rar is password-protected."),
        (
            "RAR5_PART01",
            "x.rar is one part of a multi-part RAR archive, which Grape can't read yet.",
        ),
    ] {
        let case = new_case("x.rar", &decode(name), Operation::convert(Format::Zip));
        let error = run(&case).unwrap_err();
        assert_eq!(error.user_message, message, "{name}");
        case.assert_no_output();
    }
}

/// What a writer case gives through the real bsdtar.
fn through(name: &str, to: Format) -> Result<(ArchiveOutcome, Vec<Item>), Failure> {
    let bytes = writer::case(name);
    plan(&bytes)?;
    let case = new_case("x.rar", &bytes, Operation::convert(to));
    match run(&case) {
        Ok(outcome) => {
            let items = converted(&case, to);
            Ok((outcome, items))
        }
        Err(error) => {
            case.assert_no_output();
            Err(Failure::Grape(error))
        }
    }
}

fn message(result: &Result<(ArchiveOutcome, Vec<Item>), Failure>) -> String {
    match result {
        Err(Failure::Grape(error)) => error.user_message.clone(),
        other => panic!("not refused: {other:?}"),
    }
}

fn detail(result: &Result<(ArchiveOutcome, Vec<Item>), Failure>) -> String {
    match result {
        Err(Failure::Grape(error)) => error.detail.clone(),
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn every_writer_case_the_preflight_accepts_runs_through_bsdtar() {
    let a_txt = vec![("a.txt".to_string(), Is::File(24))];
    let mut checked = Vec::new();
    for (name, bytes) in writer::cases() {
        if plan(&bytes).is_err() {
            continue;
        }
        checked.push(name);
        let result = through(name, Format::Zip);
        let names = |result: &Result<(ArchiveOutcome, Vec<Item>), Failure>| {
            shape(
                &result
                    .as_ref()
                    .unwrap_or_else(|e| panic!("{name}: {e:?}"))
                    .1,
            )
        };
        match name {
            "r4_stored" | "r5_stored" | "r4_dostime_tz" => assert_eq!(names(&result), a_txt),
            "r4_name_absolute" | "r4_name_backslash_dotdot" => assert!(
                message(&result).starts_with("x.rar has an item with a name Grape won't write"),
                "{name}: {}",
                message(&result)
            ),
            "r4_name_cp437_byte" | "r5_name_invalid_utf8" => assert_eq!(
                message(&result),
                "x.rar has item names in a text encoding Grape can't read yet.",
                "{name}"
            ),
            "r4_name_utf8_noflag" => {
                assert_eq!(names(&result), [("café.txt".to_string(), Is::File(24))]);
            }
            "r4_bad_data_crc" | "r5_bad_data_crc" => {
                assert_eq!(
                    message(&result),
                    "x.rar isn't a readable RAR file.",
                    "{name}"
                );
                assert!(
                    detail(&result).contains("fails its CRC32"),
                    "{name}: {}",
                    detail(&result)
                );
            }
            "r4_mode_zero" | "r4_short_data" | "r4_symlink_2mib" | "r5_solid_flag_first" => {
                assert_eq!(
                    message(&result),
                    "x.rar isn't a readable RAR file.",
                    "{name}"
                );
            }
            "r4_fifo" => {
                assert_eq!(result.as_ref().unwrap().0.left_out.specials, 1);
                assert!(names(&result).is_empty());
            }
            "r4_zip_last_member" | "r5_zip_last_member" => {
                let size = writer::tiny_zip().len();
                assert_eq!(
                    names(&result),
                    [
                        ("a.txt".to_string(), Is::File(24)),
                        ("x.zip".to_string(), Is::File(size))
                    ],
                    "{name}"
                );
            }
            "r5_junction" | "r5_junction_nodirflag" => {
                assert_eq!(result.as_ref().unwrap().0.left_out.links, 1, "{name}");
                assert!(!names(&result).iter().any(|(n, _)| n == "junc"), "{name}");
            }
            "r5_filecopy" => {
                assert_eq!(result.as_ref().unwrap().0.left_out.copies, 1);
                assert_eq!(names(&result), [("orig.txt".to_string(), Is::File(24))]);
            }
            "r5_unix_symlink" | "r5_win_symlink" => {
                let link = if name == "r5_unix_symlink" {
                    "ln"
                } else {
                    "wln"
                };
                assert_eq!(
                    names(&result),
                    [
                        ("a.txt".to_string(), Is::File(24)),
                        (link.to_string(), Is::Link("a.txt"))
                    ],
                    "{name}"
                );
            }
            "r5_hardlink" => {
                assert_eq!(result.as_ref().unwrap().0.left_out.links, 1);
                assert_eq!(names(&result), a_txt);
            }
            other => panic!("no expectation for {other}"),
        }
    }
    assert_eq!(checked.len(), 23, "{checked:?}");
}

#[test]
fn rar4_dos_times_are_read_as_utc_and_rar5_times_are_kept() {
    let (_, items) = through("r4_dostime_tz", Format::Tar).unwrap();
    assert_eq!(items[0].mtime, Some(1_591_012_800));
    let (_, items) = through("r5_stored", Format::Tar).unwrap();
    assert_eq!(items[0].mtime, Some(i64::from(UNIX_2020)));
}

#[test]
fn rar5_names_stay_nfc_on_every_os() {
    let nfc = "caf\u{e9}.txt";
    let bytes = writer::rar5(0, &[File5::new(nfc.as_bytes(), A)]);
    let case = new_case("x.rar", &bytes, Operation::convert(Format::Zip));
    run(&case).unwrap();
    assert_eq!(zip_items(&case.output_bytes())[0].name, nfc);
}

#[test]
fn a_rar_file_named_like_a_folder_is_refused() {
    // bsdtar writes each one as a regular tar entry holding its 24 bytes,
    // exit 0, nothing on stderr; the name check alone would make a folder
    // of it (or skip `./`) and drop the data.
    let host2 = |name: &[u8]| {
        let mut f = File4::new(name, A);
        f.host = 2;
        f.attr = 0x20;
        f
    };
    let cases = [
        (
            "RAR5 notes\\",
            writer::rar5(0, &[File5::new(b"notes\\", A)]),
        ),
        ("RAR5 ./", writer::rar5(0, &[File5::new(b"./", A)])),
        ("RAR4 a/", writer::rar4(0, &[File4::new(b"a/", A)])),
        ("RAR4 host 2 a\\", writer::rar4(0, &[host2(b"a\\")])),
    ];
    for (what, bytes) in cases {
        assert!(
            matches!(
                plan(&bytes).unwrap().entries[..],
                [Expect::Data { size: 24, .. }]
            ),
            "{what}"
        );
        for op in [
            Operation::convert(Format::Zip),
            Operation::tool(ToolKey::ExtractArchive),
        ] {
            let case = new_case("x.rar", &bytes, op);
            match run(&case) {
                Ok(outcome) => panic!("{what}: converted, {outcome:?}"),
                Err(error) => assert!(
                    error
                        .user_message
                        .starts_with("x.rar has an item with a name Grape won't write"),
                    "{what}: {error:?}"
                ),
            }
            case.assert_no_output();
        }
    }
}

#[test]
fn two_rar4_files_in_a_row_with_one_name_are_named_as_such() {
    // libarchive takes the second for the next part of a split file and
    // gives it no name; bsdtar skips it with a warning and exits 0.
    let bytes = writer::rar4(
        0,
        &[
            File4::new(b"a.txt", A),
            File4::new(b"a.txt", A),
            File4::new(b"c.txt", A),
        ],
    );
    let case = new_case("x.rar", &bytes, Operation::convert(Format::Zip));
    let error = run(&case).unwrap_err();
    assert_eq!(error.user_message, "x.rar has two items named a.txt.");
    case.assert_no_output();
}

#[test]
fn a_stored_rar5_file_shorter_than_its_size_is_refused() {
    // No CRC32 (as in a BLAKE2-only archive): bsdtar pads the 24 stored
    // bytes with zeros to the 4096 the header declares, exit 0.
    let mut f = File5::new(b"a.txt", A);
    f.file_flags = 0x0002;
    f.unp = Some(4096);
    let bytes = writer::rar5(0, &[f]);
    let case = new_case("x.rar", &bytes, Operation::convert(Format::Zip));
    match run(&case) {
        Ok(_) => panic!("converted: {:?}", shape(&converted(&case, Format::Zip))),
        Err(error) => assert_eq!(error.user_message, "x.rar isn't a readable RAR file."),
    }
    case.assert_no_output();
}

#[test]
fn junk_after_the_end_block_is_read_to_its_end() {
    let mut bytes = writer::rar4(0, &[File4::new(b"a.txt", A)]);
    bytes.extend((0..4 << 20).map(|i: u32| (i % 251) as u8));
    let case = new_case("x.rar", &bytes, Operation::convert(Format::Zip));
    let JobOutput::File(dest) = &case.job.output else {
        panic!("not a file output");
    };
    let ctx = EngineCtx::default();
    let mut seen = Vec::new();
    let outcome = convert_with(
        &case.job,
        &case.src,
        dest,
        Format::Rar,
        Format::Zip,
        &ctx,
        &mut |f: f32| seen.push(f),
        Limits::default(),
        Some(&bsdtar()),
    )
    .unwrap();
    assert_eq!(outcome.outcome.outputs, std::slice::from_ref(dest));
    assert_eq!(seen.last(), Some(&1.0));
    let before = seen[seen.len() - 2];
    assert!(before >= 0.98, "{seen:?}");
    case.source.assert_unchanged();
}

#[test]
fn extract_runs_only_with_bsdtar() {
    let bytes = writer::case("r5_stored");
    let case = new_case("x.rar", &bytes, Operation::tool(ToolKey::ExtractArchive));
    let JobOutput::Folder(dest) = &case.job.output else {
        panic!("not a folder output");
    };
    let ctx = EngineCtx::default();
    extract_with(
        &case.job,
        &case.src,
        dest,
        Format::Rar,
        &ctx,
        &mut |_: f32| {},
        Limits::default(),
        Some(&bsdtar()),
    )
    .unwrap();
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), A);
    let error = extract_with(
        &case.job,
        &case.src,
        &case.dir.join("other"),
        Format::Rar,
        &ctx,
        &mut |_: f32| {},
        Limits::default(),
        None,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotYetSupported);
}

/// True for `bsdtar 3.8.10-grape.N - libarchive 3.8.10`, then tokens of
/// the form `name/where`, then at most one space.
fn version_line_ok(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("bsdtar 3.8.10-grape.") else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let Some(rest) = rest[digits..].strip_prefix(" - libarchive 3.8.10") else {
        return false;
    };
    let rest = rest.strip_suffix(' ').unwrap_or(rest);
    if rest.is_empty() {
        return true;
    }
    let Some(rest) = rest.strip_prefix(' ') else {
        return false;
    };
    rest.split(' ').all(|token| {
        token
            .split_once('/')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains('/'))
    })
}

#[test]
fn the_version_line_carries_grapes_marker() {
    assert!(version_line_ok(
        "bsdtar 3.8.10-grape.1 - libarchive 3.8.10 libb2/bundled "
    ));
    assert!(version_line_ok(
        "bsdtar 3.8.10-grape.12 - libarchive 3.8.10 CommonCrypto/system libb2/bundled "
    ));
    assert!(!version_line_ok(
        "bsdtar 3.8.10 - libarchive 3.8.10 libb2/bundled "
    ));
    assert!(!version_line_ok(
        "bsdtar 3.8.10-grape.1 - libarchive 3.8.9 "
    ));
    let output = Command::new(bsdtar_path())
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let line = text.lines().next().unwrap_or_default();
    assert!(version_line_ok(line), "{line:?}");
}

#[test]
fn bsdtars_own_refusal_is_mapped_from_its_message() {
    // A stored entry whose data is shorter than its size passes the
    // preflight; bsdtar refuses it ("File CRC error", exit 1).
    let result = through("r4_short_data", Format::Tar);
    assert_eq!(message(&result), "x.rar isn't a readable RAR file.");
    assert!(
        detail(&result).contains("\"File CRC error\""),
        "{}",
        detail(&result)
    );
}

// ---- Through run_job, as the app runs them (#22) ----------------------------

#[test]
fn run_job_reaches_bsdtar_through_the_sidecars() {
    let ctx = EngineCtx {
        sidecars: crate::SidecarPaths {
            bsdtar: Some(bsdtar_path()),
            svg_worker: None,
            ..crate::SidecarPaths::default()
        },
        ..EngineCtx::default()
    };
    let bytes = writer::case("r5_filecopy");
    let copies = vec![crate::Note::LeftOut(LeftOut {
        copies: 1,
        ..LeftOut::default()
    })];
    for to in [Format::Zip, Format::Tar, Format::Gz] {
        let case = new_case("x.rar", &bytes, Operation::convert(to));
        assert!(crate::supports_with(
            &case.job.op,
            &[Some(Format::Rar)],
            &ctx.sidecars
        ));
        let outcome = crate::run_job(&case.job, &ctx, &mut |_| {}).unwrap();
        assert_eq!(outcome.notes, copies, "{to:?}");
        case.source.assert_unchanged();
    }
    let case = new_case("x.rar", &bytes, Operation::tool(ToolKey::ExtractArchive));
    let JobOutput::Folder(dest) = case.job.output.clone() else {
        panic!("not a folder output");
    };
    let outcome = crate::run_job(&case.job, &ctx, &mut |_| {}).unwrap();
    assert_eq!(outcome.outputs, vec![dest.clone()]);
    assert_eq!(fs::read(dest.join("orig.txt")).unwrap(), A);
    assert!(!dest.join("copy.txt").exists());
    assert_eq!(outcome.notes, copies);
    // The same job without bsdtar: refused before anything is read.
    fs::remove_dir_all(&dest).unwrap();
    let error = crate::run_job(&case.job, &EngineCtx::default(), &mut |_| {}).unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotYetSupported);
    assert!(!dest.exists());
    case.source.assert_unchanged();
}

// ---- In a Job Object (Windows) ----------------------------------------------

/// The hook the app's `contain.rs` sets: a Job Object of its own for each
/// bsdtar (`grape_platform::windows::job`). It counts what it did.
#[cfg(windows)]
#[derive(Debug, Default)]
struct Jobs {
    /// bsdtars put in a job.
    contained: AtomicUsize,
    /// Of those, the ones whose job has the UI limits.
    ui_limited: AtomicUsize,
    /// Calls that asked for bsdtar's cap.
    capped: AtomicUsize,
    /// The highest peak a job reported when its bsdtar's run ended.
    peak: Arc<AtomicU64>,
}

/// One bsdtar's job; its peak goes to [`Jobs::peak`] when it's dropped.
#[cfg(windows)]
struct Job(JobGuard, Arc<AtomicU64>);

#[cfg(windows)]
impl Contained for Job {
    fn peak_memory(&self) -> Option<u64> {
        self.0.peak_memory()
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        let peak = self.0.peak_memory().unwrap_or(0);
        self.1.fetch_max(peak, Ordering::SeqCst);
    }
}

#[cfg(windows)]
impl Contain for Jobs {
    fn contain(
        &self,
        child: &std::process::Child,
        max_memory: Option<u64>,
    ) -> Result<Box<dyn Contained>, String> {
        let guard = JobGuard::assign(child, max_memory).map_err(|e| e.to_string())?;
        self.contained.fetch_add(1, Ordering::SeqCst);
        if guard.has_ui_limits() {
            self.ui_limited.fetch_add(1, Ordering::SeqCst);
        }
        if max_memory == Some(MAX_MEMORY) {
            self.capped.fetch_add(1, Ordering::SeqCst);
        }
        Ok(Box::new(Job(guard, Arc::clone(&self.peak))))
    }
}

/// A RAR 5 and a RAR 4 converted to ZIP and extracted by the real bsdtar,
/// each bsdtar contained by `jobs`; the outputs are read back.
#[cfg(windows)]
fn convert_and_extract_in(jobs: &Arc<Jobs>) {
    let launch = Launch {
        contain: Some(Arc::clone(jobs) as Arc<dyn Contain>),
        ..bsdtar()
    };
    let a = vec![("a.txt".to_string(), Node::File(A.to_vec()))];
    for (name, bytes) in [
        ("x.rar", writer::rar5(0, &[File5::new(b"a.txt", A)])),
        ("y.rar", writer::rar4(0, &[File4::new(b"a.txt", A)])),
    ] {
        let case = new_case(name, &bytes, Operation::convert(Format::Zip));
        case.run_rar(Limits::default(), None, Some(&launch))
            .unwrap_or_else(|e| panic!("{name} → ZIP: {e:?}"));
        let items: Vec<(String, Node)> = zip_items(&case.output_bytes())
            .into_iter()
            .map(|item| (item.name, item.node))
            .collect();
        assert_eq!(items, a, "{name} → ZIP");
        let case = new_case(name, &bytes, Operation::tool(ToolKey::ExtractArchive));
        case.run_rar(Limits::default(), None, Some(&launch))
            .unwrap_or_else(|e| panic!("{name} extracted: {e:?}"));
        assert_eq!(fs::read(case.planned().join("a.txt")).unwrap(), A, "{name}");
        case.source.assert_unchanged();
    }
}

/// Each bsdtar in a Job Object of its own, as the app makes it: the
/// 1 GiB cap, one process, started with `DETACHED_PROCESS`. Under cargo
/// the job is nested in cargo's, so it has no UI limits; the child below
/// checks those.
#[cfg(windows)]
#[test]
fn bsdtar_runs_in_a_job_object_as_the_app_makes_it() {
    let jobs = Arc::new(Jobs::default());
    convert_and_extract_in(&jobs);
    let peak = jobs.peak.load(Ordering::SeqCst);
    println!(
        "bsdtar in its job: peak {} KiB (cap {} MiB)",
        peak >> 10,
        MAX_MEMORY >> 20
    );
    assert_eq!(jobs.contained.load(Ordering::SeqCst), 4);
    assert_eq!(jobs.capped.load(Ordering::SeqCst), 4);
    assert!(peak > 0, "no job reported a peak");
}

/// What tells this test binary it was started as the UI-limited child: the
/// file it writes once its checks pass, so its parent knows it ran (a name
/// that matches no test runs none and still exits 0).
#[cfg(windows)]
const UI_CHILD: &str = "GRAPE_TEST_JOB_UI_CHILD";

/// With `GRAPE_TEST_JOB_UI=1` (CI runs this binary outside cargo so),
/// starts this binary again, breaking away from the jobs this process is
/// in, to run [`ui_limited_bsdtar_child`]: its bsdtar is then in no other
/// job, as an installed Grape's is, so its own job must have the UI
/// limits. If this process's job allows no breakaway, it warns and the PC
/// checks it. Without the variable it does nothing.
#[cfg(windows)]
#[test]
fn bsdtar_runs_in_the_ui_limited_job_outside_cargo() {
    use std::os::windows::process::CommandExt;

    /// `CREATE_BREAKAWAY_FROM_JOB`.
    const BREAKAWAY: u32 = 0x0100_0000;

    if std::env::var_os("GRAPE_TEST_JOB_UI").is_none_or(|v| v != "1") {
        return;
    }
    let dir = TestDir::new();
    let ran = dir.join("ran");
    let started = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "archive::rar::it::ui_limited_bsdtar_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(UI_CHILD, &ran)
        .creation_flags(BREAKAWAY)
        .spawn();
    let mut child = match started {
        Ok(child) => child,
        Err(err) => {
            // On a line of its own, so GitHub shows it on the run.
            println!(
                "\n::warning::GRAPE_TEST_JOB_UI=1, but this process's job lets no child break away ({err}), so bsdtar's UI-limited job isn't tested here; the PC checks it (TESTING.md, RAR, D27)"
            );
            return;
        }
    };
    let status = child.wait().unwrap();
    assert!(status.success(), "the UI-limited child failed: {status}");
    assert!(ran.is_file(), "the UI-limited child ran no test");
}

/// The child: [`convert_and_extract_in`] with every bsdtar's job made as
/// the app makes it, which must have the UI limits. Only for its parent
/// above.
#[cfg(windows)]
#[test]
#[ignore = "the child of bsdtar_runs_in_the_ui_limited_job_outside_cargo"]
fn ui_limited_bsdtar_child() {
    let Some(ran) = std::env::var_os(UI_CHILD) else {
        return;
    };
    let jobs = Arc::new(Jobs::default());
    convert_and_extract_in(&jobs);
    let contained = jobs.contained.load(Ordering::SeqCst);
    let ui_limited = jobs.ui_limited.load(Ordering::SeqCst);
    println!("UI-limited child: {ui_limited} of {contained} bsdtar jobs had the UI limits");
    assert_eq!(contained, 4);
    assert_eq!(ui_limited, 4, "a bsdtar's job had no UI limits");
    fs::write(ran, b"").unwrap();
}
