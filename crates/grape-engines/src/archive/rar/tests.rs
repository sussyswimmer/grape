//! The RAR path without a real bsdtar: the preflight on the writer's
//! archives, the tap and the walk on hand-made tars, and the runner and
//! the engine against a fake bsdtar (this test binary, run as
//! `fake_bsdtar`). Every engine-level run checks, besides D27's rules
//! (the source unchanged, no `.grape-` leftovers, progress monotone and
//! ending at 1.0), that bsdtar's private folder is gone.

#![allow(clippy::unwrap_used)]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use grape_core::{Format, Operation, ToolKey};
use tar::EntryType;

use super::super::sink::Sink;
use super::super::source::{walk_tar_with, Entry, Mode, Walk};
use super::super::stream::{Metered, Run, Side};
use super::super::tests::{
    case as new_case, gunzip, pax_record, raw_entry, tar_items, tar_raw_of, tree_items, zip_items,
    Case, Node, TestDir,
};
use super::super::{open_source, ArchiveOutcome, Failure, LeftOut, Limits, Over};
use super::bsdtar::{self, Launch, Running, Seen, ARGS, STALL};
use super::header::{preflight, Expect, Plan, Skipped};
use super::tap::RarTap;
use super::writer::{self, *};
use super::*;
use crate::{CancelToken, ErrorKind, GrapeError};

// ---- The preflight ---------------------------------------------------------

/// The preflight on `bytes` under `limits`; on success, the file is back at
/// its start.
fn pre_with(bytes: &[u8], limits: Limits, cancel: &CancelToken) -> Result<Plan, Failure> {
    let dir = TestDir::new();
    let path = dir.join("x.rar");
    fs::write(&path, bytes).unwrap();
    let mut file = File::open(&path).unwrap();
    let mut progress = |_: f32| {};
    let run = Run::new(cancel, limits, bytes.len() as u64, &mut progress);
    let result = preflight(&run, &mut file, bytes.len() as u64);
    if result.is_ok() {
        assert_eq!(file.stream_position().unwrap(), 0);
    }
    result
}

fn pre(bytes: &[u8]) -> Result<Plan, Failure> {
    pre_with(bytes, Limits::default(), &CancelToken::new())
}

fn data(size: usize, bytes: &[u8]) -> Expect {
    Expect::Data {
        size: size as u64,
        crc: Some(crc32fast::hash(bytes)),
    }
}

/// The entries of an archive the preflight accepts.
fn plan_of(bytes: &[u8]) -> Vec<Expect> {
    match pre(bytes) {
        Ok(plan) => plan.entries,
        Err(failure) => panic!("refused: {failure}"),
    }
}

/// Re-checksums a RAR 4 block at `at` after a patch to its header.
fn rechecksum4(bytes: &mut [u8], at: usize) {
    let size = usize::from(u16::from_le_bytes([bytes[at + 5], bytes[at + 6]]));
    let crc = (crc32fast::hash(&bytes[at + 2..at + size]) & 0xFFFF) as u16;
    bytes[at..at + 2].copy_from_slice(&crc.to_le_bytes());
}

/// Where the first file block starts in a [`rar4`] archive.
const FILE4_AT: usize = 7 + 13;

/// Where the first file header starts in a [`rar5`] archive.
fn file5_at() -> usize {
    SIG5.len() + main5(0).len()
}

fn is_multi(result: &Result<Plan, Failure>) -> bool {
    matches!(result, Err(Failure::Rar(RarFailure::MultiVolume(_))))
}

fn is_corrupt(result: &Result<Plan, Failure>) -> bool {
    matches!(result, Err(Failure::Corrupt(_)))
}

#[test]
fn preflight_reads_a_stored_rar4() {
    let plan = pre(&rar4(0, &[File4::new(b"a.txt", A)])).unwrap();
    assert_eq!(plan.entries, [data(24, A)]);
    assert_eq!(plan.declared, 24);
}

#[test]
fn preflight_refuses_rar4_flags() {
    let a = || File4::new(b"a.txt", A);
    let with = |flags: u16| {
        let mut f = a();
        f.flags = flags;
        f
    };
    assert!(is_multi(&pre(&rar4(0x0001, &[a()]))));
    assert!(matches!(
        pre(&rar4(0x0008, &[a()])),
        Err(Failure::Rar(RarFailure::Solid4(_)))
    ));
    assert!(matches!(
        pre(&rar4(0x0080, &[a()])),
        Err(Failure::Encrypted)
    ));
    // A zero-length encrypted entry, which bsdtar writes as an empty file.
    let mut empty = File4::new(b"locked", b"");
    empty.flags = 0x0004;
    assert!(matches!(
        pre(&rar4(0, &[empty, a()])),
        Err(Failure::Encrypted)
    ));
    assert!(is_multi(&pre(&rar4(0, &[with(0x0001)]))));
    assert!(is_multi(&pre(&rar4(0, &[with(0x0002)]))));
    assert!(matches!(
        pre(&rar4(0, &[with(0x0010)])),
        Err(Failure::Rar(RarFailure::Solid4(_)))
    ));
    // An end block that says the archive goes on.
    let mut more = SIG4.to_vec();
    more.extend(main4(0));
    more.extend(file4(&a()));
    more.extend(end4(0x4001));
    assert!(is_multi(&pre(&more)));
}

#[test]
fn preflight_names_two_rar4_files_in_a_row_with_one_name() {
    // libarchive compares each RAR 4 file's name with the one before, `\`
    // read as `/`, and takes a match for the next part of a split file:
    // bsdtar gets no name, skips it with a warning and exits 0.
    let named = |name: &[u8]| File4::new(name, A);
    let twice = |a: &[u8], b: &[u8]| pre(&rar4(0, &[named(a), named(b), named(b"c.txt")]));
    assert!(matches!(
        twice(b"a.txt", b"a.txt"),
        Err(Failure::Duplicate(name)) if name == "a.txt"
    ));
    assert!(matches!(
        twice(b"d\\a.txt", b"d/a.txt"),
        Err(Failure::Duplicate(name)) if name == "d/a.txt"
    ));
    // Another case, or another file between: bsdtar writes both, and the
    // name check refuses them as it does for any archive.
    assert!(twice(b"A.txt", b"a.txt").is_ok());
    let plan = pre(&rar4(
        0,
        &[named(b"a.txt"), named(b"b.txt"), named(b"a.txt")],
    ))
    .unwrap();
    assert_eq!(plan.entries.len(), 3);
    // A real next part is a split entry: multi-volume.
    let mut part = named(b"a.txt");
    part.flags = 0x0001;
    assert!(is_multi(&pre(&rar4(0, &[named(b"a.txt"), part]))));
    // Encryption still outranks it.
    let mut locked = named(b"a.txt");
    locked.flags = 0x0004;
    assert!(matches!(
        pre(&rar4(0, &[named(b"a.txt"), locked])),
        Err(Failure::Encrypted)
    ));
}

#[test]
fn preflight_ranks_its_findings() {
    // Solid and encrypted: the password message wins.
    let mut locked = File4::new(b"a.txt", A);
    locked.flags = 0x0004;
    assert!(matches!(
        pre(&rar4(0x0008, &[locked])),
        Err(Failure::Encrypted)
    ));
    // A volume whose file header is damaged: still a volume.
    let mut bytes = rar4(0x0001, &[File4::new(b"a.txt", A)]);
    bytes[FILE4_AT] ^= 0xFF;
    assert!(is_multi(&pre(&bytes)));
    // RAR 5: a dictionary, then damage.
    let mut big = File5::new(b"a.txt", A);
    big.comp = 10 << 10;
    let mut bytes = rar5(0, &[big]);
    let end = bytes.len() - end5(0).len();
    bytes.truncate(end);
    assert!(matches!(
        pre(&bytes),
        Err(Failure::Rar(RarFailure::Dictionary(_)))
    ));
}

#[test]
fn preflight_refuses_damaged_rar4() {
    let good = rar4(0, &[File4::new(b"a.txt", A)]);
    let main_crc = {
        let mut bytes = good.clone();
        bytes[7] ^= 0xFF;
        bytes
    };
    let file_crc = {
        let mut bytes = good.clone();
        bytes[FILE4_AT + 1] ^= 0xFF;
        bytes
    };
    let after_main = |block: &[u8]| {
        let mut bytes = SIG4.to_vec();
        bytes.extend(main4(0));
        bytes.extend(block);
        bytes.extend(file4(&File4::new(b"a.txt", A)));
        bytes.extend(end4(0x4000));
        bytes
    };
    let size6 = {
        let mut block = block4(0x75, 0, &[]);
        block[5] = 6;
        block
    };
    let file31 = block4(0x74, 0x8000, &[0; 24]);
    let name_past = {
        let mut bytes = good.clone();
        bytes[FILE4_AT + 26] = 200;
        rechecksum4(&mut bytes, FILE4_AT);
        bytes
    };
    let pack_past = {
        let mut f = File4::new(b"a.txt", A);
        f.pack = Some(1000);
        rar4(0, &[f])
    };
    let kind70 = block4(0x70, 0, &[]);
    let bad_mark = block4(0x72, 0, &[]);
    for (what, bytes) in [
        ("main CRC", main_crc),
        ("file CRC", file_crc),
        ("HEAD_SIZE 6", after_main(&size6)),
        ("file HEAD_SIZE 31", after_main(&file31)),
        ("name past HEAD_SIZE", name_past),
        ("PACK_SIZE past the end", pack_past),
        ("type 0x70", after_main(&kind70)),
        ("a mark that isn't the signature", after_main(&bad_mark)),
        ("no end block", writer::case("r4_no_end_block")),
        (
            "cut inside a header",
            writer::case("r4_cut_at_100_inside_2nd_header"),
        ),
        ("cut inside data", writer::case("r4_cut_at_70_inside_data")),
        ("only the signature", SIG4.to_vec()),
    ] {
        assert!(is_corrupt(&pre(&bytes)), "{what}: {:?}", pre(&bytes));
    }
    // A mid-file mark that is the signature is skipped.
    assert_eq!(plan_of(&after_main(SIG4)), [data(24, A)]);
}

#[test]
fn preflight_classifies_rar4_entries_as_libarchive_does() {
    let entry = |host: u8, attr: u32| {
        let mut f = File4::new(b"x", b"");
        f.host = host;
        f.attr = attr;
        pre(&rar4(0, &[f]))
    };
    let empty = data(0, b"");
    assert_eq!(entry(2, 0x10).unwrap().entries, [Expect::Pass]);
    assert_eq!(entry(2, 0x20).unwrap().entries, [empty]);
    assert_eq!(entry(3, 0o100_644).unwrap().entries, [empty]);
    assert_eq!(entry(3, 0o120_777).unwrap().entries, [Expect::Pass]);
    assert_eq!(entry(3, 0o010_644).unwrap().entries, [Expect::Pass]);
    assert_eq!(entry(3, 0).unwrap().entries, [Expect::Pass]);
    assert!(is_corrupt(&entry(6, 0o100_644)));
}

#[test]
fn preflight_declares_large_rar4_sizes_without_reading_them() {
    // FHD_LARGE: 5 GiB unpacked, 24 bytes stored.
    let mut f = File4::new(b"big.bin", A);
    f.unp = Some(5 << 30);
    let bytes = rar4(0, &[f]);
    let plan = pre(&bytes).unwrap();
    assert_eq!(plan.declared, 5 << 30);
    let cancel = CancelToken::new();
    let mut progress = |_: f32| {};
    let small = Limits {
        max_total: 1 << 30,
        ..Limits::default()
    };
    let run = Run::new(&cancel, small, bytes.len() as u64, &mut progress);
    assert!(matches!(
        run.precheck(plan.declared),
        Err(Failure::TooBig(Over::Total))
    ));
}

#[test]
fn preflight_reads_a_stored_rar5_and_refuses_its_flags() {
    let a = || File5::new(b"a.txt", A);
    let plan = pre(&rar5(0, &[a()])).unwrap();
    assert_eq!(plan.entries, [data(24, A)]);
    assert_eq!(plan.declared, 24);
    assert!(is_multi(&pre(&rar5(0x0001, &[a()]))));
    assert!(is_multi(&pre(&rar5(0x0002, &[a()]))));
    for flags in [0x0008, 0x0010] {
        let mut f = a();
        f.header_flags = flags;
        assert!(is_multi(&pre(&rar5(0, &[f]))), "{flags:#x}");
    }
    let mut more = SIG5.to_vec();
    more.extend(main5(0));
    more.extend(file5(&a()));
    more.extend(end5(0x0001));
    assert!(is_multi(&pre(&more)));
    // Solid RAR 5 is read by libarchive.
    assert!(pre(&rar5(0x0004, &[a()])).is_ok());
}

#[test]
fn preflight_holds_a_stored_rar5_file_to_its_size() {
    // Stored, with no CRC32 (BLAKE2 only, or no hash): libarchive stops at
    // the end of the data area and bsdtar pads the file with zeros to its
    // size, exit 0, so nothing after the preflight would notice.
    let stored = |unp: u64, comp: u64, file_flags: u64| {
        let mut f = File5::new(b"a.txt", A);
        f.unp = Some(unp);
        f.comp = comp;
        f.file_flags = file_flags;
        rar5(0, &[f])
    };
    for comp in [0, 1] {
        // Version 0 and 1 alike: method 0 is stored.
        assert!(is_corrupt(&pre(&stored(4096, comp, 0x0002))), "{comp}");
        assert!(is_corrupt(&pre(&stored(10, comp, 0x0002))), "{comp}");
    }
    // With a CRC32 too: the data area is the file.
    assert!(is_corrupt(&pre(&stored(4096, 0, 0x0002 | 0x0004))));
    // A compressed file's data is any size.
    let plan = pre(&stored(4096, 3 << 7, 0x0002)).unwrap();
    assert_eq!(
        plan.entries,
        [Expect::Data {
            size: 4096,
            crc: None
        }]
    );
    // A link or a file copy has no data of its own: r5_hardlink and
    // r5_filecopy in `the_writer_writes_what_the_preflight_reads`.
    // Encrypted data is padded, and encryption is what's reported.
    let mut locked = File5::new(b"locked", A);
    locked.unp = Some(20);
    locked.extra = record5(1, &[0; 33]);
    assert!(matches!(pre(&rar5(0, &[locked])), Err(Failure::Encrypted)));
}

#[test]
fn preflight_refuses_rar5_encryption_but_not_in_a_service_header() {
    let mut crypt = SIG5.to_vec();
    crypt.extend(block5(4, 0, &[0; 4], &[], None));
    crypt.extend(main5(0));
    assert!(matches!(pre(&crypt), Err(Failure::Encrypted)));
    for data in [A, b""] {
        let mut f = File5::new(b"locked", data);
        f.extra = record5(1, &[0; 33]);
        assert!(matches!(pre(&rar5(0, &[f])), Err(Failure::Encrypted)));
    }
    let mut comment = File5::new(b"CMT", b"a comment");
    comment.kind = 3;
    comment.extra = record5(1, &[0; 33]);
    let plan = pre(&rar5(0, &[comment, File5::new(b"a.txt", A)])).unwrap();
    // A service header is no entry.
    assert_eq!(plan.entries, [data(24, A)]);
}

#[test]
fn preflight_reads_the_whole_rar5_dictionary_field() {
    let with = |comp: u64, dir: bool| {
        let mut f = if dir {
            let mut f = File5::new(b"d", b"");
            f.file_flags = 0x0001 | 0x0002;
            f
        } else {
            File5::new(b"a.txt", A)
        };
        f.comp = comp;
        pre(&rar5(0, &[f]))
    };
    let dictionary =
        |r: Result<Plan, Failure>| matches!(r, Err(Failure::Rar(RarFailure::Dictionary(_))));
    let feature = |r: Result<Plan, Failure>| matches!(r, Err(Failure::Rar(RarFailure::Feature(_))));
    assert!(with(9 << 10, false).is_ok());
    assert!(dictionary(with(10 << 10, false)));
    // libarchive reads 4 bits: N = 16 would be 128 KiB to it.
    assert!(dictionary(with(16 << 10, false)));
    assert!(with(20 << 10, true).is_ok());
    // RAR 7's version 1: the fraction bits.
    let v1 = |n: u64, f: u64, method: u64, v0: bool| {
        1 | (method << 7) | (n << 10) | (f << 15) | if v0 { 0x10_0000 } else { 0 }
    };
    assert!(with(v1(8, 31, 0, false), false).is_ok());
    assert!(dictionary(with(v1(9, 1, 0, false), false)));
    assert!(feature(with(v1(8, 0, 3, false), false)));
    assert!(with(v1(8, 0, 3, true), false).is_ok());
    assert!(feature(with(2, false)));
    let mut unknown = File5::new(b"a.txt", A);
    unknown.file_flags |= 0x0008;
    assert!(feature(pre(&rar5(0, &[unknown]))));
}

#[test]
fn preflight_classifies_rar5_redirections() {
    let link = |kind: u64, dir_flag: bool| {
        let mut f = File5::new(b"link", b"");
        f.file_flags = 0x0002 | if dir_flag { 0x0001 } else { 0 };
        f.extra = redirect5(kind, b"a.txt", dir_flag);
        pre(&rar5(0, &[f]))
    };
    for kind in [1, 2, 4] {
        assert_eq!(link(kind, false).unwrap().entries, [Expect::Pass], "{kind}");
    }
    let junction = [Expect::Skip(Skipped::Junction)];
    assert_eq!(link(3, true).unwrap().entries, junction);
    assert_eq!(link(3, false).unwrap().entries, junction);
    assert_eq!(
        link(5, false).unwrap().entries,
        [Expect::Skip(Skipped::FileCopy)]
    );
    for kind in [0, 6] {
        assert!(
            matches!(link(kind, false), Err(Failure::Rar(RarFailure::Feature(_)))),
            "{kind}"
        );
    }
    // No CRC32: accepted unchecked (a BLAKE2-only entry).
    let mut f = File5::new(b"a.txt", A);
    f.file_flags = 0x0002;
    assert_eq!(
        plan_of(&rar5(0, &[f])),
        [Expect::Data {
            size: 24,
            crc: None
        }]
    );
    // Windows host: the directory attribute; Unix: the type bits.
    let mut dir = File5::new(b"d", b"");
    dir.host = 0;
    dir.attr = 0x10;
    let mut file = File5::new(b"f", A);
    file.host = 0;
    file.attr = 0x20;
    let mut fifo = File5::new(b"p", b"");
    fifo.attr = 0o010_644;
    assert_eq!(
        plan_of(&rar5(0, &[dir, file, fifo])),
        [Expect::Pass, data(24, A), Expect::Pass]
    );
}

#[test]
fn preflight_skips_unknown_rar5_blocks_only_when_told() {
    let with = |flags: u64| {
        let mut bytes = SIG5.to_vec();
        bytes.extend(main5(0));
        bytes.extend(block5(9, flags, b"whatever", &[], Some(3)));
        bytes.extend(b"xyz");
        bytes.extend(file5(&File5::new(b"a.txt", A)));
        bytes.extend(end5(0));
        pre(&bytes)
    };
    assert_eq!(with(0x0004).unwrap().entries, [data(24, A)]);
    assert!(is_corrupt(&with(0)));
}

#[test]
fn preflight_refuses_damaged_rar5() {
    let at = file5_at();
    let size4 = {
        let mut bytes = SIG5.to_vec();
        bytes.extend(main5(0));
        bytes.extend([0, 0, 0, 0, 0x80, 0x80, 0x80, 0x01]);
        bytes.extend(end5(0));
        bytes
    };
    let vint11 = {
        let mut fields = vec![0x80; 10];
        fields.push(0);
        let mut bytes = SIG5.to_vec();
        bytes.extend(main5(0));
        bytes.extend(block5(2, 0, &fields, &[], Some(0)));
        bytes.extend(end5(0));
        bytes
    };
    let overrun = {
        let mut f = File5::new(b"a.txt", A);
        f.extra = vec![50, 1, 0];
        rar5(0, &[f])
    };
    let unnamed = rar5(0, &[File5::new(b"", A)]);
    let host2 = {
        let mut f = File5::new(b"a.txt", A);
        f.host = 2;
        rar5(0, &[f])
    };
    let header_crc = {
        let mut bytes = rar5(0, &[File5::new(b"a.txt", A)]);
        bytes[at] ^= 0xFF;
        bytes
    };
    for (what, bytes) in [
        ("header size vint of 4 bytes", size4),
        ("an 11-byte vint", vint11),
        ("an extra record overrunning", overrun),
        ("name length 0", unnamed),
        ("host 2", host2),
        ("header CRC", header_crc),
        ("no end header", writer::case("r5_no_end_header")),
        (
            "cut inside an entry",
            writer::case("r5_cut_at_140_inside_2nd_entry"),
        ),
    ] {
        assert!(is_corrupt(&pre(&bytes)), "{what}: {:?}", pre(&bytes));
    }
}

/// The failure [`open_source`] gives `bytes` named `name`.
fn opened(name: &str, bytes: &[u8]) -> Failure {
    let dir = TestDir::new();
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    let from = Format::from_file_name(name).unwrap();
    match open_source(&path, from) {
        Ok((_, _, container)) => panic!("{name} opened as {container:?}"),
        Err(failure) => failure,
    }
}

#[test]
fn foreign_files_are_named_for_what_they_are() {
    let mut mz = b"MZ".to_vec();
    mz.resize(0x10000, 0);
    mz.extend(rar4(0, &[File4::new(b"a.txt", A)]));
    assert!(matches!(
        opened("x.rar", &mz),
        Failure::Rar(RarFailure::Sfx)
    ));
    assert!(matches!(opened("x.zip", &mz), Failure::NotArchive));
    let mut elf = b"\x7FELF".to_vec();
    elf.resize(0x2000, 7);
    elf.extend(rar5(0, &[File5::new(b"a.txt", A)]));
    assert!(matches!(
        opened("x.rar", &elf),
        Failure::Rar(RarFailure::Sfx)
    ));
    let mut no_sig = b"MZ".to_vec();
    no_sig.resize(0x20000, 0);
    assert!(matches!(opened("x.rar", &no_sig), Failure::NotArchive));
    // A signature past the first MiB isn't looked for.
    let mut far = b"MZ".to_vec();
    far.resize((1 << 20) + 1, 0);
    far.extend(rar4(0, &[File4::new(b"a.txt", A)]));
    assert!(matches!(opened("x.rar", &far), Failure::NotArchive));
    assert!(matches!(
        opened("x.rar", &writer::case("r4_junk_prefix")),
        Failure::NotArchive
    ));
    for future in [
        &b"Rar!\x1A\x07\x02\x00rest"[..],
        b"Rar!\x1A\x07\x01\x05rest",
    ] {
        assert!(matches!(
            opened("x.tar", future),
            Failure::Rar(RarFailure::Feature(_))
        ));
    }
    // RAR bytes under any name are a RAR.
    let dir = TestDir::new();
    let path = dir.join("x.zip");
    fs::write(&path, rar5(0, &[File5::new(b"a.txt", A)])).unwrap();
    let (_, _, container) = open_source(&path, Format::Zip).unwrap();
    assert_eq!(container, super::super::sniff::Container::Rar);
}

#[test]
fn preflight_holds_the_entry_and_header_bounds() {
    let three = Limits {
        max_entries: 3,
        ..Limits::default()
    };
    let cancel = CancelToken::new();
    let files: Vec<File4> = (0..4)
        .map(|i| File4::new(format!("{i}.txt").as_bytes(), A))
        .collect();
    assert!(matches!(
        pre_with(&rar4(0, &files), three, &cancel),
        Err(Failure::TooBig(Over::Entries))
    ));
    assert!(pre_with(&rar4(0, &files[..3]), three, &cancel).is_ok());
    // Service headers aren't entries, but the walk is bounded too.
    let services: Vec<File5> = (0..13)
        .map(|_| {
            let mut f = File5::new(b"CMT", b"x");
            f.kind = 3;
            f
        })
        .collect();
    assert!(matches!(
        pre_with(&rar5(0, &services), three, &cancel),
        Err(Failure::TooBig(Over::Entries))
    ));
    assert!(pre_with(&rar5(0, &services[..9]), three, &cancel).is_ok());
}

#[test]
fn preflight_stops_when_cancelled() {
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(
        pre_with(
            &rar4(0, &[File4::new(b"a.txt", A)]),
            Limits::default(),
            &cancel
        ),
        Err(Failure::Cancelled)
    ));
}

/// Writes `bytes` to `out`; how many it took before an error.
#[cfg(unix)]
fn write_counted(out: &mut File, bytes: &[u8]) -> (u64, bool) {
    let mut at = 0;
    while at < bytes.len() {
        match out.write(&bytes[at..]) {
            Ok(0) | Err(_) => return (at as u64, false),
            Ok(n) => at += n,
        }
    }
    (at as u64, true)
}

#[cfg(unix)]
#[test]
fn preflight_sees_a_cancel_within_a_buffer_of_reading() {
    // A RAR 5 header may be 2 MiB, and the per-header check runs every 64
    // headers. The RAR comes through a FIFO, so the test knows how much
    // the preflight read after the cancel.
    let dir = TestDir::new();
    let fifo = dir.join("x.rar");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let service = |fill: u8| {
        let mut f = File5::new(b"CMT", b"");
        f.kind = 3;
        f.extra = record5(0x7F, &vec![fill; 1 << 20]);
        file5(&f)
    };
    let cancel = CancelToken::new();
    let writer = {
        let cancel = cancel.clone();
        let fifo = fifo.clone();
        thread::spawn(move || {
            let mut out = File::options().write(true).open(&fifo).unwrap();
            let mut head = SIG5.to_vec();
            head.extend(main5(0));
            head.extend(service(1));
            out.write_all(&head).unwrap();
            cancel.cancel();
            // Eight more headers, 8 MiB, then the end; it stops once the
            // preflight lets go of the FIFO.
            let mut after = 0;
            for fill in 2..10 {
                let (n, all) = write_counted(&mut out, &service(fill));
                after += n;
                if !all {
                    return after;
                }
            }
            after + write_counted(&mut out, &end5(0)).0
        })
    };
    let mut file = File::open(&fifo).unwrap();
    let mut progress = |_: f32| {};
    let run = Run::new(&cancel, Limits::default(), 1 << 40, &mut progress);
    let result = preflight(&run, &mut file, 1 << 40);
    drop(file);
    let after = writer.join().unwrap();
    assert!(matches!(result, Err(Failure::Cancelled)), "{result:?}");
    // At most what the pipe holds, plus a buffer or two.
    assert!(after < 512 << 10, "{after} bytes taken after the cancel");
}

/// What the preflight makes of each of the writer's cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    Entries(&'static [Expect]),
    Encrypted,
    Multi,
    Solid,
    Dictionary,
    Feature,
    Corrupt,
    /// Not a RAR at offset 0: `open_source` decides.
    Foreign,
}

const D24: Expect = Expect::Data {
    size: 24,
    crc: Some(0xE3BF_522A),
};

#[test]
fn the_writer_writes_what_the_preflight_reads() {
    assert_eq!(crc32fast::hash(A), 0xE3BF_522A);
    let zip_crc = crc32fast::hash(&tiny_zip());
    let zip = Expect::Data {
        size: tiny_zip().len() as u64,
        crc: Some(zip_crc),
    };
    let zip_entries: &'static [Expect] = Box::leak(Box::new([D24, zip]));
    let want = |name: &str| -> Want {
        match name {
            "r4_stored"
            | "r4_name_cp437_byte"
            | "r4_name_utf8_noflag"
            | "r4_name_absolute"
            | "r4_name_backslash_dotdot"
            | "r4_dostime_tz"
            | "r5_stored"
            | "r5_solid_flag_first" => Want::Entries(&[D24]),
            "r4_bad_data_crc" | "r5_bad_data_crc" => Want::Entries(&[
                Expect::Data {
                    size: 24,
                    crc: Some(0x1234_5678),
                },
                D24,
            ]),
            "r4_zip_last_member" | "r5_zip_last_member" => Want::Entries(zip_entries),
            "r4_short_data" => Want::Entries(&[D24]),
            "r4_fifo" | "r4_mode_zero" | "r4_symlink_2mib" => Want::Entries(&[Expect::Pass]),
            "r5_junction" => Want::Entries(&[Expect::Pass, Expect::Skip(Skipped::Junction)]),
            "r5_junction_nodirflag" => Want::Entries(&[Expect::Skip(Skipped::Junction)]),
            "r5_filecopy" => Want::Entries(&[D24, Expect::Skip(Skipped::FileCopy)]),
            "r5_unix_symlink" | "r5_win_symlink" | "r5_hardlink" => {
                Want::Entries(&[D24, Expect::Pass])
            }
            "r5_name_invalid_utf8" => Want::Entries(&[D24, D24]),
            "r4_solid_synthetic" => Want::Solid,
            "r4_zero_len_password_entry" | "r5_zero_len_encrypted" | "r5_encrypted_stored" => {
                Want::Encrypted
            }
            "r4_volume_flag" | "r5_volume_flag" => Want::Multi,
            "r5_dict_128mib" | "r5_dict_n16_bit14" => Want::Dictionary,
            "r5_unknown_size" => Want::Feature,
            "r4_sfx_mz_prefix" | "r4_junk_prefix" => Want::Foreign,
            "r4_no_end_block"
            | "r5_no_end_header"
            | "r4_cut_at_70_inside_data"
            | "r4_cut_at_100_inside_2nd_header"
            | "r5_cut_at_140_inside_2nd_entry" => Want::Corrupt,
            other => panic!("no expectation for {other}"),
        }
    };
    let cases = writer::cases();
    assert_eq!(cases.len(), 39);
    for (name, bytes) in cases {
        let got = match want(name) {
            Want::Foreign => {
                assert!(
                    !bytes.starts_with(SIG4) && !bytes.starts_with(SIG5),
                    "{name}"
                );
                continue;
            }
            _ => match pre(&bytes) {
                Ok(plan) => Want::Entries(Box::leak(plan.entries.into_boxed_slice())),
                Err(Failure::Encrypted) => Want::Encrypted,
                Err(Failure::Rar(RarFailure::MultiVolume(_))) => Want::Multi,
                Err(Failure::Rar(RarFailure::Solid4(_))) => Want::Solid,
                Err(Failure::Rar(RarFailure::Dictionary(_))) => Want::Dictionary,
                Err(Failure::Rar(RarFailure::Feature(_))) => Want::Feature,
                Err(Failure::Corrupt(_)) => Want::Corrupt,
                Err(other) => panic!("{name}: {other:?}"),
            },
        };
        assert_eq!(got, want(name), "{name}");
    }
}

// ---- The tap and the walk ------------------------------------------------------

/// Records what reaches it.
#[derive(Default)]
struct Recorder {
    items: Vec<(String, String)>,
}

impl Sink for Recorder {
    fn dir(&mut self, _run: &Run<'_>, entry: &Entry) -> Result<(), Failure> {
        self.items.push((entry.name(), "dir".into()));
        Ok(())
    }

    fn file(
        &mut self,
        _run: &Run<'_>,
        entry: &Entry,
        _size: Option<u64>,
        data: &mut dyn Read,
    ) -> Result<(), Failure> {
        let mut bytes = Vec::new();
        data.read_to_end(&mut bytes)
            .map_err(|e| Failure::Corrupt(format!("{:?}", e.kind())))?;
        self.items
            .push((entry.name(), String::from_utf8_lossy(&bytes).into_owned()));
        Ok(())
    }

    fn symlink(&mut self, _run: &Run<'_>, entry: &Entry, target: &str) -> Result<(), Failure> {
        self.items.push((entry.name(), format!("-> {target}")));
        Ok(())
    }
}

/// `entries` as bsdtar writes them: the end marker, then zeros to a
/// multiple of 10,240 bytes.
fn padded(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut tar = tar_raw_of(entries);
    tar.resize(tar.len().div_ceil(10_240) * 10_240, 0);
    tar
}

fn file_entry(name: &[u8], data: &[u8]) -> Vec<u8> {
    raw_entry(name, EntryType::Regular, data)
}

/// Walks `tar` through a [`RarTap`] over `plan` as the RAR path does.
fn tap_walk(tar: &[u8], plan: &[Expect], mode: Mode) -> (Result<(), Failure>, Recorder, LeftOut) {
    let cancel = CancelToken::new();
    let mut progress = |_: f32| {};
    let run = Run::new(&cancel, Limits::default(), tar.len() as u64, &mut progress);
    let mut walk = Walk::new(mode, &run.limits);
    let mut sink = Recorder::default();
    let mut tap = RarTap::new(plan);
    let result = walk_tar_with(
        &run,
        Metered::new(&run, tar),
        &mut walk,
        Some(&mut sink),
        &mut tap,
    )
    .and_then(|rest| drain_zeros(&run, rest, || tar.len() as u64))
    .and_then(|()| tap.finish());
    (result, sink, walk.left_out)
}

fn corrupt_with(result: &Result<(), Failure>, text: &str) -> bool {
    matches!(result, Err(Failure::Corrupt(detail)) if detail.contains(text))
}

#[test]
fn the_tap_holds_bsdtar_to_the_headers() {
    let a = file_entry(b"a.txt", A);
    let tar = padded(std::slice::from_ref(&a));
    let (ok, sink, _) = tap_walk(&tar, &[D24], Mode::Repack);
    ok.unwrap();
    assert_eq!(
        sink.items,
        [("a.txt".to_string(), String::from_utf8_lossy(A).into_owned())]
    );

    let two = padded(&[a.clone(), file_entry(b"b.txt", A)]);
    assert!(corrupt_with(
        &tap_walk(&two, &[D24], Mode::Repack).0,
        "more entries"
    ));
    assert!(corrupt_with(
        &tap_walk(&tar, &[D24, D24], Mode::Repack).0,
        "fewer entries"
    ));
    let longer = Expect::Data {
        size: 25,
        crc: None,
    };
    assert!(corrupt_with(
        &tap_walk(&tar, &[longer], Mode::Repack).0,
        "doesn't match"
    ));
    let wrong = Expect::Data {
        size: 24,
        crc: Some(0x1234_5678),
    };
    assert!(corrupt_with(
        &tap_walk(&tar, &[wrong], Mode::Repack).0,
        "fails its CRC32"
    ));
    // A drained entry is checked too: macOS metadata, left out.
    let drained = padded(&[file_entry(b"__MACOSX/._a.txt", A)]);
    assert!(corrupt_with(
        &tap_walk(&drained, &[wrong], Mode::Extract).0,
        "fails its CRC32"
    ));
    let (ok, _, left_out) = tap_walk(&drained, &[D24], Mode::Extract);
    ok.unwrap();
    assert_eq!(left_out.mac_metadata, 1);
    // Data met by a folder; Pass met by data; Skip with data.
    let folder = padded(&[raw_entry(b"d/", EntryType::Directory, b"")]);
    let empty = Expect::Data {
        size: 0,
        crc: Some(0),
    };
    assert!(corrupt_with(
        &tap_walk(&folder, &[empty], Mode::Repack).0,
        "doesn't match"
    ));
    assert!(tap_walk(&folder, &[Expect::Pass], Mode::Repack).0.is_ok());
    assert!(corrupt_with(
        &tap_walk(&tar, &[Expect::Pass], Mode::Repack).0,
        "doesn't match"
    ));
    let junction = Expect::Skip(Skipped::Junction);
    assert!(corrupt_with(
        &tap_walk(&tar, &[junction], Mode::Repack).0,
        "doesn't match"
    ));
    // Without a CRC32, only the size is held.
    let unchecked = Expect::Data {
        size: 24,
        crc: None,
    };
    assert!(tap_walk(&tar, &[unchecked], Mode::Repack).0.is_ok());
}

#[test]
fn a_rar_file_named_like_a_folder_is_refused_not_made_one() {
    // A RAR file named `notes\` (a legal Unix name), `a/` (RAR4) or `./`:
    // bsdtar writes a regular tar entry with its data and exits 0, and the
    // name check alone would read the trailing separator as a folder and
    // drop the data, or skip `./` as the archive's root.
    for name in [&b"notes\\"[..], b"a/", b"a\\", b"./", b"d/./"] {
        let tar = padded(&[file_entry(name, A)]);
        for mode in [Mode::Repack, Mode::Extract] {
            let (result, sink, left_out) = tap_walk(&tar, &[D24], mode);
            assert!(
                matches!(result, Err(Failure::UnsafeName(_))),
                "{name:?} {mode:?}: {result:?}"
            );
            assert!(sink.items.is_empty(), "{name:?} {mode:?}");
            assert_eq!(left_out, LeftOut::default());
        }
    }
    // An empty file is no folder either.
    let empty = Expect::Data {
        size: 0,
        crc: Some(0),
    };
    let tar = padded(&[file_entry(b"e/", b"")]);
    assert!(matches!(
        tap_walk(&tar, &[empty], Mode::Repack).0,
        Err(Failure::UnsafeName(_))
    ));
    // A RAR folder still comes through as a folder, slash or not.
    for (name, kind) in [
        (&b"d/"[..], EntryType::Directory),
        (b"d", EntryType::Directory),
    ] {
        let tar = padded(&[raw_entry(name, kind, b"")]);
        let (ok, sink, _) = tap_walk(&tar, &[Expect::Pass], Mode::Repack);
        ok.unwrap();
        assert_eq!(sink.items, [("d".to_string(), "dir".to_string())]);
    }
}

#[test]
fn the_tap_refuses_binary_names() {
    let tar = padded(&[
        raw_entry(
            b"PaxHeaders/a.txt",
            EntryType::XHeader,
            &pax_record("hdrcharset", b"BINARY"),
        ),
        file_entry(b"a.txt", A),
    ]);
    assert!(matches!(
        tap_walk(&tar, &[D24], Mode::Repack).0,
        Err(Failure::Rar(RarFailure::Names(_)))
    ));
}

#[test]
fn the_tap_makes_names_nfc() {
    let nfd = "cafe\u{301}.txt";
    let nfc = "caf\u{e9}.txt";
    let tar = padded(&[file_entry(nfd.as_bytes(), A)]);
    let (ok, sink, _) = tap_walk(&tar, &[D24], Mode::Repack);
    ok.unwrap();
    assert_eq!(sink.items[0].0, nfc);
    // A link's target too.
    let mut link = tar::Header::new_ustar();
    link.set_path("ln").unwrap();
    link.set_link_name(nfd).unwrap();
    link.set_entry_type(EntryType::Symlink);
    link.set_size(0);
    link.set_cksum();
    let tar = padded(&[file_entry(nfd.as_bytes(), A), link.as_bytes().to_vec()]);
    let (ok, sink, _) = tap_walk(&tar, &[D24, Expect::Pass], Mode::Repack);
    ok.unwrap();
    assert_eq!(sink.items[1], ("ln".to_string(), format!("-> {nfc}")));
    // One name in NFD and NFC is two items with one name.
    let tar = padded(&[file_entry(nfd.as_bytes(), A), file_entry(nfc.as_bytes(), A)]);
    assert!(matches!(
        tap_walk(&tar, &[D24, D24], Mode::Repack).0,
        Err(Failure::Duplicate(_))
    ));
}

#[test]
fn junctions_and_file_copies_are_left_out_and_counted() {
    let tar = padded(&[
        raw_entry(b"junc/", EntryType::Directory, b""),
        file_entry(b"a.txt", A),
        file_entry(b"copy.txt", b""),
    ]);
    let plan = [
        Expect::Skip(Skipped::Junction),
        D24,
        Expect::Skip(Skipped::FileCopy),
    ];
    for mode in [Mode::Repack, Mode::Extract] {
        let (ok, sink, left_out) = tap_walk(&tar, &plan, mode);
        ok.unwrap();
        assert_eq!(sink.items.len(), 1, "{mode:?}");
        assert_eq!(sink.items[0].0, "a.txt");
        assert_eq!((left_out.links, left_out.copies), (1, 1), "{mode:?}");
    }
}

#[test]
fn only_zeros_may_follow_the_end_marker() {
    let a = file_entry(b"a.txt", A);
    let ok = |tar: &[u8]| tap_walk(tar, &[D24], Mode::Repack).0;
    ok(&padded(std::slice::from_ref(&a))).unwrap();
    // One zero block, then the end.
    let mut one = a.clone();
    one.extend([0u8; 512]);
    assert!(corrupt_with(&ok(&one), "no tar end marker"));
    // No zero block.
    assert!(corrupt_with(&ok(&a), "no tar end marker"));
    // A byte after the marker.
    let mut junk = padded(std::slice::from_ref(&a));
    *junk.last_mut().unwrap() = 1;
    assert!(corrupt_with(&ok(&junk), "data after the end marker"));
    // Not whole blocks.
    let mut partial = tar_raw_of(std::slice::from_ref(&a));
    partial.extend([0u8; 100]);
    assert!(corrupt_with(&ok(&partial), "partial block"));
}

#[test]
fn plain_tars_are_walked_as_before() {
    // `walk_tar` is `walk_tar_with` and `NoTap`: a hdrcharset=BINARY name
    // is no reason to refuse a plain tar, and nothing is made NFC.
    let nfd = "cafe\u{301}.txt";
    let tar = padded(&[
        raw_entry(
            b"PaxHeaders/x",
            EntryType::XHeader,
            &pax_record("hdrcharset", b"BINARY"),
        ),
        file_entry(nfd.as_bytes(), A),
    ]);
    let cancel = CancelToken::new();
    let mut progress = |_: f32| {};
    let run = Run::new(&cancel, Limits::default(), tar.len() as u64, &mut progress);
    let mut walk = Walk::new(Mode::Repack, &run.limits);
    let mut sink = Recorder::default();
    super::super::source::walk_tar(&run, &tar[..], &mut walk, Some(&mut sink)).unwrap();
    assert_eq!(sink.items[0].0, nfd);
}

// ---- The fake bsdtar ---------------------------------------------------------------

/// The environment variable that turns the ignored test below into a fake
/// bsdtar when the test binary runs itself.
const FAKE: &str = "GRAPE_BSDTAR_FAKE";

/// What the fake writes first: libtest prints its banner on stdout before
/// any test body runs.
const MARK: &[u8] = b"GRAPE-FAKE-BSDTAR\n";

/// `entries` cut after `n` bytes.
fn cut(entries: &[Vec<u8>], n: usize) -> Vec<u8> {
    entries.concat()[..n].to_vec()
}

/// Not a test: the bsdtar the tests start. The test binary runs only this
/// "test" (`--exact … --ignored`) with [`FAKE`] set to a mode. It always
/// ends with `exit`, so libtest's trailer never prints.
#[test]
#[ignore = "a fake bsdtar, run by the tests below"]
fn fake_bsdtar() {
    let Ok(mode) = std::env::var(FAKE) else {
        return;
    };
    let mut out = io::stdout().lock();
    if mode != "no-preamble" {
        out.write_all(MARK).unwrap();
        out.flush().unwrap();
    }
    let input = || {
        let mut bytes = Vec::new();
        io::stdin().read_to_end(&mut bytes).unwrap();
        bytes
    };
    let a = || file_entry(b"a.txt", A);
    let mut say = |bytes: &[u8]| {
        out.write_all(bytes).unwrap();
        out.flush().unwrap();
    };
    let complain = |text: &str| io::stderr().write_all(text.as_bytes()).unwrap();
    let sleep = || thread::sleep(Duration::from_secs(60));
    let code = match mode.as_str() {
        "echo" => {
            say(&input());
            0
        }
        "env" => {
            let mut lines: Vec<String> = std::env::vars_os()
                .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
                .collect();
            lines.sort();
            let cwd = std::env::current_dir().unwrap();
            lines.push(format!("cwd={}", cwd.display()));
            lines.push(format!("entries={}", fs::read_dir(&cwd).unwrap().count()));
            say(lines.join("\n").as_bytes());
            0
        }
        "big-stderr" => {
            io::stderr().write_all(&vec![b'e'; 1 << 20]).unwrap();
            0
        }
        "exit-early" => 1,
        "crash" => std::process::abort(),
        "exit-3" => 3,
        "stall" => {
            let mut some = vec![0u8; 64 * 1024];
            io::stdin().read_exact(&mut some).unwrap();
            sleep();
            0
        }
        "slow" => {
            // Echoes the whole input 1 KiB at a time, 50 ms apart: never
            // stalled, however slow the machine. The time cap only stops a
            // fake that's left running, so it's far above the ~1.2 s a
            // 24 KiB input takes (a slow CI Mac once needed more than 1.5 s).
            let start = Instant::now();
            let mut stdin = io::stdin().lock();
            let mut buf = [0u8; 1024];
            while start.elapsed() < Duration::from_secs(30) {
                let n = stdin.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                say(&buf[..n]);
                thread::sleep(Duration::from_millis(50));
            }
            0
        }
        "sleep" => {
            input();
            say(&cut(&[a()], 700));
            sleep();
            0
        }
        "quiet" => {
            // Takes the whole RAR, then writes nothing: the job waits in
            // a read of stdout, where only the watchdog can reach it.
            input();
            sleep();
            0
        }
        "no-preamble" => {
            input();
            say(b"just some data");
            0
        }
        named => {
            input();
            match named {
                "tar-a" => say(&padded(&[a()])),
                "tar-a-badcrc" => say(&padded(&[file_entry(b"a.txt", &[b'x'; 24])])),
                "tar-a-noend" => say(&a()),
                "tar-a-junk" => {
                    let mut tar = padded(&[a()]);
                    *tar.last_mut().unwrap() = 1;
                    say(&tar);
                }
                "tar-a-warn" => {
                    say(&padded(&[a()]));
                    complain("bsdtar: x: Can't translate pathname 'x' to UTF-8\n");
                }
                "tar-a-exit1-crc" => {
                    say(&cut(&[a()], 522));
                    complain(
                        "bsdtar: File CRC error: Invalid or incomplete multibyte or wide character\n",
                    );
                    std::process::exit(1);
                }
                "tar-a-binary" => say(&padded(&[
                    raw_entry(
                        b"PaxHeaders/a.txt",
                        EntryType::XHeader,
                        &pax_record("hdrcharset", b"BINARY"),
                    ),
                    a(),
                ])),
                "tar-two" => say(&padded(&[a(), file_entry(b"b.txt", A)])),
                "tar-unsafe" => {
                    say(&file_entry(b"../x", A));
                    sleep();
                }
                "tar-badheader-sleep" => {
                    let mut entry = a();
                    entry[0] = b'b';
                    say(&entry);
                    sleep();
                }
                "tar-nfd" => say(&padded(&[file_entry("cafe\u{301}.txt".as_bytes(), A)])),
                "tar-junction" => say(&padded(&[
                    raw_entry(b"junc/", EntryType::Directory, b""),
                    a(),
                ])),
                "tar-filecopy" => say(&padded(&[
                    file_entry(b"orig.txt", A),
                    file_entry(b"copy.txt", b""),
                ])),
                other => panic!("unknown fake mode {other}"),
            }
            0
        }
    };
    drop(out);
    std::process::exit(code);
}

/// A launch of this test binary as a fake bsdtar in `mode`.
fn fake(mode: &str) -> Launch {
    Launch {
        program: std::env::current_exe().unwrap(),
        args: [
            "--exact",
            "archive::rar::tests::fake_bsdtar",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ]
        .iter()
        .map(OsString::from)
        .collect(),
        envs: vec![(OsString::from(FAKE), OsString::from(mode))],
        stall: STALL,
        preamble: Some(MARK),
    }
}

/// [`fake`] with a short stall time.
fn fake_stalling(mode: &str) -> Launch {
    Launch {
        stall: Duration::from_millis(300),
        ..fake(mode)
    }
}

/// One run of the runner, as the job runs it.
struct Ran {
    result: Result<(), Failure>,
    out: Vec<u8>,
    seen: Seen,
    progress: Vec<f32>,
    work: Option<PathBuf>,
    took: Duration,
    pid: Option<u32>,
    /// What the child's folder held while it ran ([`fs::canonicalize`]d).
    work_real: Option<PathBuf>,
    /// When another thread cancelled the job ([`run_fake_with`]).
    cancelled_at: Option<Instant>,
}

/// Spawns `launch` on `source`, reads its stdout to the end (cancelling
/// at the first progress if `cancel_early`), then ends the run.
fn run_fake(launch: &Launch, source: &[u8], cancel_early: bool) -> Ran {
    run_fake_with(launch, source, cancel_early, None)
}

/// [`run_fake`], and when `cancel_after` is given, another thread cancels
/// the job that long after the spawn, whatever the job is doing then.
fn run_fake_with(
    launch: &Launch,
    source: &[u8],
    cancel_early: bool,
    cancel_after: Option<Duration>,
) -> Ran {
    let dir = TestDir::new();
    let path = dir.join("src.rar");
    fs::write(&path, source).unwrap();
    let file = File::open(&path).unwrap();
    let cancel = CancelToken::new();
    let mut seen_progress = Vec::new();
    let start = Instant::now();
    bsdtar::take_work_dir();
    let ran = {
        let mut progress = |f: f32| {
            seen_progress.push(f);
            if cancel_early {
                cancel.cancel();
            }
        };
        let run = Run::new(
            &cancel,
            Limits::default(),
            source.len() as u64,
            &mut progress,
        );
        match Running::spawn(launch, file, cancel.clone()) {
            Err(failure) => (Err(failure), Vec::new(), Seen::default(), None, None, None),
            Ok(mut child) => {
                let pid = child.id();
                let canceller = cancel_after.map(|after| {
                    let cancel = cancel.clone();
                    thread::spawn(move || {
                        thread::sleep(after);
                        cancel.cancel();
                        Instant::now()
                    })
                });
                let mut out = Vec::new();
                let read = {
                    let mut stdout = child.stdout(&run);
                    stdout
                        .read_to_end(&mut out)
                        .map(drop)
                        .map_err(|e| run.failure(&e, Side::Read, "stdout"))
                };
                let work_real = bsdtar::take_work_dir().and_then(|w| fs::canonicalize(w).ok());
                let (result, seen) = child.end(read, &cancel);
                let cancelled_at = canceller.map(|t| t.join().unwrap());
                (result, out, seen, Some(pid), work_real, cancelled_at)
            }
        }
    };
    let (result, out, seen, pid, work_real, cancelled_at) = ran;
    let took = start.elapsed();
    Ran {
        result,
        out,
        seen,
        progress: seen_progress,
        work: work_real.clone(),
        took,
        pid,
        work_real,
        cancelled_at,
    }
}

impl Ran {
    /// The folder is gone and the child reaped.
    fn assert_clean(&self) {
        if let Some(work) = &self.work {
            assert!(!work.exists(), "{} is still there", work.display());
        }
        #[cfg(target_os = "linux")]
        if let Some(pid) = self.pid {
            // Reaped: not even a zombie is left.
            let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            assert!(!status.contains("State:\tZ"), "{status}");
        }
    }
}

#[test]
fn launch_is_bsdtar_as_grape_runs_it() {
    let launch = Launch::bsdtar(&RarPaths {
        bsdtar: PathBuf::from("/opt/grape/bsdtar"),
    });
    assert_eq!(launch.program, Path::new("/opt/grape/bsdtar"));
    assert_eq!(launch.args, ARGS.map(OsString::from));
    assert_eq!(ARGS, ["-c", "-P", "-f", "-", "--format", "pax", "@-"]);
    assert_eq!(launch.stall, Duration::from_secs(60));
    assert!(launch.envs.is_empty());
    assert_eq!(launch.preamble, None);
}

#[test]
fn the_pump_writes_the_source_exactly() {
    let source = super::super::tests::noise(3 << 20, 41);
    let ran = run_fake(&fake("echo"), &source, false);
    ran.result.as_ref().unwrap();
    assert!(ran.out == source, "the echo differs");
    assert_eq!(ran.seen.pumped, source.len() as u64);
    // The preamble isn't counted.
    assert_eq!(ran.seen.out, source.len() as u64);
    assert!(!ran.progress.is_empty());
    assert!(
        ran.progress.windows(2).all(|w| w[0] <= w[1]),
        "{:?}",
        ran.progress
    );
    ran.assert_clean();
}

#[test]
fn bsdtar_runs_in_an_empty_private_folder_with_only_grapes_variables() {
    let ran = run_fake(&fake("env"), b"", false);
    ran.result.as_ref().unwrap();
    let text = String::from_utf8(ran.out.clone()).unwrap();
    let mut vars = Vec::new();
    let mut cwd = None;
    let mut entries = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("cwd=") {
            cwd = Some(PathBuf::from(path));
        } else if let Some(n) = line.strip_prefix("entries=") {
            entries = Some(n.to_string());
        } else {
            let (key, value) = line.split_once('=').unwrap();
            if !key.starts_with("__CF_") {
                vars.push((key.to_string(), value.to_string()));
            }
        }
    }
    assert_eq!(entries.as_deref(), Some("0"));
    let work = ran.work_real.clone().unwrap();
    assert_eq!(fs::canonicalize(&work).ok(), None, "the folder is gone");
    // cwd is the private folder, and PATH names only it.
    let cwd = cwd.unwrap();
    let path = vars
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        .map(|(_, v)| PathBuf::from(v))
        .unwrap();
    let real = |p: &Path| {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>()
    };
    assert_eq!(real(&work).last(), real(&cwd).last());
    assert_eq!(real(&work).last(), real(&path).last());
    let mut want = vec![
        ("GRAPE_BSDTAR_FAKE".to_string(), "env".to_string()),
        ("TAR_READER_OPTIONS".into(), "rar:hdrcharset=UTF-8".into()),
        ("TZ".into(), "UTC0".into()),
    ];
    if cfg!(target_os = "macos") {
        want.push(("LC_ALL".into(), "en_US.UTF-8".into()));
    } else if cfg!(unix) {
        want.push(("LC_ALL".into(), "C.UTF-8".into()));
    }
    if cfg!(windows) {
        if let Some(root) = std::env::var_os("SystemRoot") {
            want.push(("SystemRoot".into(), root.to_string_lossy().into_owned()));
        }
    }
    let mut got: Vec<(String, String)> = vars
        .into_iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("PATH"))
        .collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
    ran.assert_clean();
}

#[test]
fn a_flood_on_stderr_never_blocks_bsdtar_and_refuses_the_job() {
    let ran = run_fake(&fake("big-stderr"), b"", false);
    assert_eq!(ran.seen.stderr.total, 1 << 20);
    assert_eq!(ran.seen.stderr.head.len(), 8 * 1024);
    assert!(
        matches!(ran.result, Err(Failure::Corrupt(_))),
        "{:?}",
        ran.result
    );
    ran.assert_clean();
}

#[test]
fn a_bsdtar_that_exits_early_ends_the_pump() {
    let source = vec![7u8; 20 << 20];
    let ran = run_fake(&fake("exit-early"), &source, false);
    // Exit 1 with nothing on stderr: unreadable, not a read failure.
    assert!(
        matches!(ran.result, Err(Failure::Corrupt(_))),
        "{:?}",
        ran.result
    );
    assert!(ran.took < Duration::from_secs(2), "{:?}", ran.took);
    ran.assert_clean();
}

#[test]
fn a_crash_or_an_odd_exit_is_the_helper_stopping() {
    for mode in ["crash", "exit-3"] {
        let ran = run_fake(&fake(mode), b"x", false);
        assert!(
            matches!(ran.result, Err(Failure::Rar(RarFailure::Stopped(_)))),
            "{mode}: {:?}",
            ran.result
        );
        ran.assert_clean();
    }
}

#[test]
fn a_stalled_bsdtar_is_killed() {
    let source = vec![1u8; 8 << 20];
    let ran = run_fake(&fake_stalling("stall"), &source, false);
    assert!(
        matches!(ran.result, Err(Failure::Rar(RarFailure::Stalled))),
        "{:?}",
        ran.result
    );
    assert!(ran.took < Duration::from_secs(2), "{:?}", ran.took);
    ran.assert_clean();
}

#[test]
fn a_slow_but_steady_bsdtar_is_not_a_stall() {
    let source = super::super::tests::noise(24 * 1024, 5);
    let ran = run_fake(&fake_stalling("slow"), &source, false);
    ran.result.as_ref().unwrap();
    assert!(ran.out == source);
    assert!(ran.took >= Duration::from_millis(900), "{:?}", ran.took);
    ran.assert_clean();
}

#[test]
fn cancel_kills_bsdtar_quickly() {
    let source = vec![3u8; 1 << 20];
    let ran = run_fake(&fake("sleep"), &source, true);
    assert!(
        matches!(ran.result, Err(Failure::Cancelled)),
        "{:?}",
        ran.result
    );
    assert!(ran.took < Duration::from_secs(2), "{:?}", ran.took);
    ran.assert_clean();
}

#[test]
fn cancel_reaches_a_job_waiting_on_a_quiet_bsdtar() {
    // The job is blocked in a read of stdout, so only the watchdog can
    // kill bsdtar; without it, the read would wait for the stall time.
    let launch = Launch {
        stall: Duration::from_secs(5),
        ..fake("quiet")
    };
    let source = vec![3u8; 1 << 20];
    let ran = run_fake_with(&launch, &source, false, Some(Duration::from_millis(200)));
    assert!(
        matches!(ran.result, Err(Failure::Cancelled)),
        "{:?}",
        ran.result
    );
    let since = ran.cancelled_at.unwrap().elapsed();
    assert!(since < Duration::from_secs(1), "{since:?} after the cancel");
    assert!(ran.out.is_empty());
    ran.assert_clean();
}

#[test]
fn output_without_the_preamble_is_refused() {
    let ran = run_fake(&fake("no-preamble"), b"x", false);
    assert!(
        matches!(ran.result, Err(Failure::Corrupt(_))),
        "{:?}",
        ran.result
    );
    ran.assert_clean();
}

#[test]
fn a_missing_bsdtar_does_not_start_and_leaves_no_folder() {
    let dir = TestDir::new();
    let launch = Launch {
        program: dir.join("no-such-bsdtar"),
        ..fake("echo")
    };
    bsdtar::take_work_dir();
    let ran = run_fake(&launch, b"x", false);
    assert!(
        matches!(
            ran.result,
            Err(Failure::Rar(RarFailure::NoStart(io::ErrorKind::NotFound)))
        ),
        "{:?}",
        ran.result
    );
    let work = bsdtar::take_work_dir();
    assert!(work.as_deref().is_none_or(|w| !w.exists()));
}

#[test]
fn stderr_is_classified_by_libarchives_own_text() {
    let class =
        |text: &str, code: i32| bsdtar::classify(text.as_bytes(), text.len() as u64, Some(code));
    assert!(matches!(
        class(
            "bsdtar: Error reading archive -: RAR encryption support unavailable\n",
            1
        ),
        Failure::Encrypted
    ));
    assert!(matches!(
        class(
            "bsdtar: Reading encrypted data is not currently supported: Illegal byte sequence",
            1
        ),
        Failure::Encrypted
    ));
    assert!(matches!(
        class("bsdtar: RAR solid archive support unavailable", 1),
        Failure::Rar(RarFailure::Solid4(_))
    ));
    assert!(matches!(
        class("bsdtar: Declared dictionary size is not supported", 1),
        Failure::Rar(RarFailure::Dictionary(_))
    ));
    assert!(matches!(
        class(
            "bsdtar: Pathname cannot be converted from UTF-8 to current locale",
            1
        ),
        Failure::Rar(RarFailure::Names(_))
    ));
    assert!(matches!(
        class(
            "bsdtar: Archive entry has empty or unreadable filename ... skipping",
            0
        ),
        Failure::Rar(RarFailure::Names(_))
    ));
    assert!(matches!(
        class(
            "bsdtar: A character-set conversion not fully supported on this platform",
            1
        ),
        Failure::Rar(RarFailure::Stopped(_))
    ));
    let crc = "bsdtar: secret-name.txt: File CRC error: Invalid or incomplete multibyte";
    match class(crc, 1) {
        Failure::Corrupt(detail) => {
            assert_eq!(
                detail,
                format!(
                    "bsdtar exit 1: \"File CRC error\" ({} bytes on stderr)",
                    crc.len()
                )
            );
        }
        other => panic!("{other:?}"),
    }
    match class("bsdtar: something new about secret-name.txt", 1) {
        Failure::Corrupt(detail) => assert!(!detail.contains("secret"), "{detail}"),
        other => panic!("{other:?}"),
    }
}

// ---- The engine, with the fake ------------------------------------------------------

/// A RAR case run against the fake in `mode`, as the job runs it; checks
/// that bsdtar's folder is gone afterwards.
fn run_case(
    case: &Case,
    launch: &Launch,
    cancel_at: Option<f32>,
) -> Result<ArchiveOutcome, GrapeError> {
    bsdtar::take_work_dir();
    let result = case.run_rar(Limits::default(), cancel_at, Some(launch));
    if let Some(work) = bsdtar::take_work_dir() {
        assert!(!work.exists(), "{} is still there", work.display());
    }
    result
}

fn ok_case(case: &Case, launch: &Launch) -> ArchiveOutcome {
    let outcome = run_case(case, launch, None).unwrap();
    assert_eq!(outcome.outcome.outputs, [case.planned()]);
    outcome
}

fn failing_case(case: &Case, launch: &Launch) -> GrapeError {
    let error = run_case(case, launch, None).unwrap_err();
    case.assert_no_output();
    error
}

fn a4() -> Vec<u8> {
    rar4(0, &[File4::new(b"a.txt", A)])
}

fn a5() -> Vec<u8> {
    rar5(0, &[File5::new(b"a.txt", A)])
}

/// The names and file bytes of a converted archive.
fn listed(case: &Case, to: Format) -> Vec<(String, Node)> {
    let bytes = case.output_bytes();
    let items = match to {
        Format::Zip => zip_items(&bytes),
        Format::Tar => tar_items(&bytes),
        Format::Gz => tar_items(&gunzip(&bytes)),
        other => panic!("{other:?}"),
    };
    items
        .into_iter()
        .map(|item| (item.name, item.node))
        .collect()
}

fn extracted_tree(case: &Case) -> Vec<(String, Node)> {
    tree_items(&case.planned())
        .into_iter()
        .map(|item| (item.name, item.node))
        .collect()
}

#[test]
fn rar_converts_and_extracts_through_bsdtar() {
    let launch = fake("tar-a");
    let want = [("a.txt".to_string(), Node::File(A.to_vec()))];
    for (name, bytes) in [("x.rar", a4()), ("y.rar", a5())] {
        for to in [Format::Zip, Format::Tar, Format::Gz] {
            let case = new_case(name, &bytes, Operation::convert(to));
            let outcome = ok_case(&case, &launch);
            assert_eq!(outcome.left_out, LeftOut::default());
            assert_eq!(listed(&case, to), want, "{name} → {to:?}");
        }
        let case = new_case(name, &bytes, Operation::tool(ToolKey::ExtractArchive));
        ok_case(&case, &launch);
        assert_eq!(extracted_tree(&case), want, "{name} extracted");
    }
}

#[test]
fn bsdtar_output_that_breaks_a_rule_is_refused() {
    for mode in ["tar-a-badcrc", "tar-a-noend", "tar-a-junk", "tar-two"] {
        let case = new_case("x.rar", &a4(), Operation::convert(Format::Zip));
        let error = failing_case(&case, &fake(mode));
        assert_eq!(error.kind, ErrorKind::DecodeFailed, "{mode}");
        assert_eq!(
            error.user_message, "x.rar isn't a readable RAR file.",
            "{mode}"
        );
    }
}

#[test]
fn bsdtar_warnings_refuse_the_job_without_logging_them() {
    let case = new_case("x.rar", &a4(), Operation::convert(Format::Zip));
    let error = failing_case(&case, &fake("tar-a-warn"));
    assert_eq!(
        error.user_message,
        "x.rar has item names in a text encoding Grape can't read yet."
    );
    assert_eq!(error.kind, ErrorKind::NotYetSupported);
    assert!(!error.detail.contains("translate"), "{}", error.detail);

    let error = failing_case(&case, &fake("tar-a-binary"));
    assert_eq!(
        error.user_message,
        "x.rar has item names in a text encoding Grape can't read yet."
    );

    let error = failing_case(&case, &fake("tar-a-exit1-crc"));
    assert_eq!(error.user_message, "x.rar isn't a readable RAR file.");
    assert!(
        error.detail.contains("\"File CRC error\""),
        "{}",
        error.detail
    );
    assert!(!error.detail.contains("multibyte"), "{}", error.detail);
}

#[test]
fn grapes_own_refusal_stops_bsdtar_at_once() {
    let mut up = File4::new(b"../x", A);
    up.host = 3;
    let case = new_case("x.rar", &rar4(0, &[up]), Operation::convert(Format::Zip));
    let start = Instant::now();
    let error = failing_case(&case, &fake("tar-unsafe"));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert!(
        error
            .user_message
            .starts_with("x.rar has an item with a name Grape won't write: "),
        "{}",
        error.user_message
    );
}

#[test]
fn a_broken_stream_from_a_bsdtar_grape_killed_is_damage_not_a_crash() {
    let case = new_case("x.rar", &a4(), Operation::convert(Format::Zip));
    let start = Instant::now();
    let error = failing_case(&case, &fake("tar-badheader-sleep"));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(error.user_message, "x.rar isn't a readable RAR file.");
}

#[test]
fn rar_names_come_out_nfc() {
    let case = new_case(
        "x.rar",
        &rar5(0, &[File5::new("caf\u{e9}.txt".as_bytes(), A)]),
        Operation::convert(Format::Zip),
    );
    ok_case(&case, &fake("tar-nfd"));
    assert_eq!(listed(&case, Format::Zip)[0].0, "caf\u{e9}.txt");
}

#[test]
fn junctions_and_file_copies_are_counted_not_written() {
    let mut junction = File5::new(b"junc", b"");
    junction.host = 0;
    junction.attr = 0x400 | 0x10;
    junction.file_flags = 0x0001 | 0x0002;
    junction.extra = redirect5(3, b"C:\\Windows", true);
    let source = rar5(0, &[junction, File5::new(b"a.txt", A)]);
    let case = new_case("x.rar", &source, Operation::convert(Format::Zip));
    let outcome = ok_case(&case, &fake("tar-junction"));
    assert_eq!(outcome.left_out.links, 1);
    assert_eq!(
        listed(&case, Format::Zip),
        [("a.txt".to_string(), Node::File(A.to_vec()))]
    );

    let mut copy = File5::new(b"copy.txt", b"");
    copy.file_flags = 0x0002;
    copy.unp = Some(24);
    copy.extra = redirect5(5, b"orig.txt", false);
    let source = rar5(0, &[File5::new(b"orig.txt", A), copy]);
    let case = new_case("x.rar", &source, Operation::tool(ToolKey::ExtractArchive));
    let outcome = ok_case(&case, &fake("tar-filecopy"));
    assert_eq!(outcome.left_out.copies, 1);
    assert_eq!(
        extracted_tree(&case),
        [("orig.txt".to_string(), Node::File(A.to_vec()))]
    );
}

#[test]
fn the_preflight_refuses_before_any_bsdtar_starts() {
    let dir = TestDir::new();
    let missing = Launch {
        program: dir.join("no-such-bsdtar"),
        ..fake("tar-a")
    };
    let case = new_case(
        "x.rar",
        &rar4(0x0080, &[File4::new(b"a.txt", A)]),
        Operation::convert(Format::Zip),
    );
    bsdtar::take_work_dir();
    let error = failing_case(&case, &missing);
    assert_eq!(error.user_message, "x.rar is password-protected.");
    assert_eq!(bsdtar::take_work_dir(), None);

    // The preflight's other refusals, worded.
    for (bytes, message) in [
        (
            rar4(0x0001, &[File4::new(b"a.txt", A)]),
            "x.rar is one part of a multi-part RAR archive, which Grape can't read yet.",
        ),
        (
            rar4(0x0008, &[File4::new(b"a.txt", A)]),
            "x.rar uses RAR 4's solid compression, which Grape can't read yet.",
        ),
        (
            writer::case("r5_dict_128mib"),
            "x.rar needs a RAR dictionary over 64 MiB, which Grape can't read yet.",
        ),
        (
            writer::case("r5_unknown_size"),
            "x.rar uses a RAR feature Grape can't read yet.",
        ),
        (
            writer::case("r4_no_end_block"),
            "x.rar isn't a readable RAR file.",
        ),
        (
            writer::case("r4_sfx_mz_prefix"),
            "x.rar is a self-extracting archive, which Grape can't read yet.",
        ),
        (b"not a rar".to_vec(), "x.rar doesn't look like a RAR file."),
    ] {
        let case = new_case("x.rar", &bytes, Operation::convert(Format::Zip));
        let error = failing_case(&case, &missing);
        assert_eq!(error.user_message, message);
    }
}

#[test]
fn without_bsdtar_a_rar_is_not_run() {
    // By its name: not in this build.
    let case = new_case("x.rar", &a4(), Operation::convert(Format::Zip));
    let error = case.fails();
    assert_eq!(error.kind, ErrorKind::NotYetSupported);
    assert_eq!(error.user_message, "RAR to ZIP isn't in this build yet.");
    // By its bytes, under another name.
    let case = super::super::tests::convert_case("x.zip", &a4(), Format::Tar);
    let error = case.fails();
    assert_eq!(error.kind, ErrorKind::NotYetSupported);
    assert_eq!(
        error.user_message,
        "x.zip is really a RAR file, which this build can't read yet."
    );
    // And with bsdtar, a `.zip` whose bytes are RAR goes through it.
    let case = super::super::tests::convert_case("x.zip", &a4(), Format::Tar);
    ok_case(&case, &fake("tar-a"));
}

#[test]
fn cancel_midway_leaves_nothing() {
    let mut big = File4::new(b"a.txt", A);
    big.data = vec![9u8; 4 << 20];
    let source = rar4(0, &[big]);
    for op in [
        Operation::convert(Format::Zip),
        Operation::tool(ToolKey::ExtractArchive),
    ] {
        let case = new_case("x.rar", &source, op);
        let error = run_case(&case, &fake("tar-a"), Some(0.0)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled);
        case.assert_no_output();
    }
}

#[test]
fn rar_failures_are_worded_and_never_name_a_file_in_the_log() {
    let names = |failure: RarFailure| failure.into_error("x.rar", "detail".into());
    let error = names(RarFailure::Stalled);
    assert_eq!(error.user_message, "x.rar took too long, so Grape stopped.");
    assert_eq!(error.kind, ErrorKind::DecodeFailed);
    let error = names(RarFailure::Stopped("exit Some(3)".into()));
    assert_eq!(
        error.user_message,
        "Grape's RAR helper stopped unexpectedly."
    );
    let error = names(RarFailure::NoStart(io::ErrorKind::NotFound));
    assert_eq!(error.user_message, "Grape's RAR helper couldn't start.");
    assert_eq!(error.kind, ErrorKind::NotYetSupported);
    assert_eq!(
        Failure::Rar(RarFailure::Dictionary(128 << 20)).to_string(),
        "RAR dictionary 134217728 bytes over 64 MiB"
    );
    assert_eq!(Failure::Encrypted.to_string(), "an encrypted entry");
    assert!(Failure::Rar(RarFailure::Names("x")).is_policy());
    assert!(!Failure::Rar(RarFailure::Stalled).is_policy());
    assert!(!Failure::Corrupt("x".into()).is_policy());
}
