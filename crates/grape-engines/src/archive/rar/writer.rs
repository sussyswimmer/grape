//! A writer of small stored (method 0) RAR 4 and RAR 5 archives, for the
//! tests. RAR 4 follows the RAR 2.9 technote, RAR 5 RARLAB's RAR 5.0
//! technote. Grape never writes RAR; this only builds test inputs, every
//! one of them in [`cases`] (the prototype's `mkrar.py`, in Rust).

use std::io::{Cursor, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

pub(crate) const SIG4: &[u8] = b"Rar!\x1a\x07\x00";
pub(crate) const SIG5: &[u8] = b"Rar!\x1a\x07\x01\x00";

/// 2020-01-02 03:04:06 as a DOS date and time.
pub(crate) const DOS_2020: u32 = (40 << 25) | (1 << 21) | (2 << 16) | (3 << 11) | (4 << 5) | 3;

/// 2020-01-02 03:04:06 UTC, the RAR 5 writer's mtime.
pub(crate) const UNIX_2020: u32 = 1_577_934_246;

/// The data of most cases' `a.txt` (24 bytes).
pub(crate) const A: &[u8] = b"hello from a stored RAR\n";

// ---- RAR 4 -------------------------------------------------------------------

/// A RAR 4 block: HEAD_CRC (the low 16 bits of CRC32 over the rest),
/// HEAD_TYPE, HEAD_FLAGS, HEAD_SIZE (7 + `body`), then `body`.
pub(crate) fn block4(kind: u8, flags: u16, body: &[u8]) -> Vec<u8> {
    let size = u16::try_from(7 + body.len()).expect("a RAR 4 header fits 64 KiB");
    let mut rest = vec![kind];
    rest.extend(flags.to_le_bytes());
    rest.extend(size.to_le_bytes());
    rest.extend(body);
    let crc = (crc32fast::hash(&rest) & 0xFFFF) as u16;
    let mut out = crc.to_le_bytes().to_vec();
    out.extend(rest);
    out
}

/// A main header: RESERVED1 and RESERVED2, zero.
pub(crate) fn main4(flags: u16) -> Vec<u8> {
    block4(0x73, flags, &[0; 6])
}

/// One RAR 4 file entry; every field the test sets.
#[derive(Debug, Clone)]
pub(crate) struct File4 {
    pub(crate) name: Vec<u8>,
    pub(crate) data: Vec<u8>,
    pub(crate) flags: u16,
    pub(crate) host: u8,
    pub(crate) attr: u32,
    /// The data's CRC32 when `None`.
    pub(crate) crc: Option<u32>,
    /// The data's length when `None`.
    pub(crate) unp: Option<u64>,
    /// The data's length when `None`.
    pub(crate) pack: Option<u64>,
    pub(crate) dostime: u32,
    pub(crate) method: u8,
}

impl File4 {
    /// A stored Unix file, mode 0644, dated [`DOS_2020`].
    pub(crate) fn new(name: &[u8], data: &[u8]) -> File4 {
        File4 {
            name: name.to_vec(),
            data: data.to_vec(),
            flags: 0,
            host: 3,
            attr: 0o100_644,
            crc: None,
            unp: None,
            pack: None,
            dostime: DOS_2020,
            method: 0x30,
        }
    }
}

/// A FILE block (`flags | 0x8000`, and FHD_LARGE with the high size fields
/// when a size is over 32 bits), then its data.
pub(crate) fn file4(f: &File4) -> Vec<u8> {
    let len = f.data.len() as u64;
    let pack = f.pack.unwrap_or(len);
    let unp = f.unp.unwrap_or(len);
    let large = pack > u64::from(u32::MAX) || unp > u64::from(u32::MAX);
    let mut flags = f.flags | 0x8000;
    if large {
        flags |= 0x0100;
    }
    let name_len = u16::try_from(f.name.len()).expect("a short name");
    let mut body = Vec::new();
    body.extend((pack as u32).to_le_bytes());
    body.extend((unp as u32).to_le_bytes());
    body.push(f.host);
    body.extend(
        f.crc
            .unwrap_or_else(|| crc32fast::hash(&f.data))
            .to_le_bytes(),
    );
    body.extend(f.dostime.to_le_bytes());
    body.push(29); // UNP_VER: RAR 2.9
    body.push(f.method);
    body.extend(name_len.to_le_bytes());
    body.extend(f.attr.to_le_bytes());
    if large {
        body.extend(((pack >> 32) as u32).to_le_bytes());
        body.extend(((unp >> 32) as u32).to_le_bytes());
    }
    body.extend(&f.name);
    let mut out = block4(0x74, flags, &body);
    out.extend(&f.data);
    out
}

/// An end block (`0x4000` as WinRAR writes it; `0x0001`: more volumes).
pub(crate) fn end4(flags: u16) -> Vec<u8> {
    block4(0x7B, flags, &[])
}

/// A whole RAR 4: signature, main header, `files`, end block.
pub(crate) fn rar4(main_flags: u16, files: &[File4]) -> Vec<u8> {
    let mut out = SIG4.to_vec();
    out.extend(main4(main_flags));
    for f in files {
        out.extend(file4(f));
    }
    out.extend(end4(0x4000));
    out
}

// ---- RAR 5 -------------------------------------------------------------------

/// A RAR 5 vint: 7 bits a byte, low first, the high bit for "more".
pub(crate) fn vint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let low = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(low);
            return out;
        }
        out.push(low | 0x80);
    }
}

/// A RAR 5 block: CRC32, header size, type, flags (`0x0001` and `0x0002`
/// added for an extra area and a data area), the sizes, `fields`, then
/// `extra` at the header's end.
pub(crate) fn block5(
    kind: u64,
    flags: u64,
    fields: &[u8],
    extra: &[u8],
    data_len: Option<u64>,
) -> Vec<u8> {
    let mut flags = flags;
    if !extra.is_empty() {
        flags |= 0x0001;
    }
    if data_len.is_some() {
        flags |= 0x0002;
    }
    let mut body = vint(kind);
    body.extend(vint(flags));
    if !extra.is_empty() {
        body.extend(vint(extra.len() as u64));
    }
    if let Some(len) = data_len {
        body.extend(vint(len));
    }
    body.extend(fields);
    body.extend(extra);
    let mut header = vint(body.len() as u64);
    header.extend(body);
    let mut out = crc32fast::hash(&header).to_le_bytes().to_vec();
    out.extend(header);
    out
}

/// An extra record: its size (from the type on), its type, `data`.
pub(crate) fn record5(kind: u64, data: &[u8]) -> Vec<u8> {
    let mut inner = vint(kind);
    inner.extend(data);
    let mut out = vint(inner.len() as u64);
    out.extend(inner);
    out
}

/// A redirection record: type, flags (`dir`: the target is a folder),
/// the target.
pub(crate) fn redirect5(kind: u64, target: &[u8], dir: bool) -> Vec<u8> {
    let mut data = vint(kind);
    data.extend(vint(u64::from(dir)));
    data.extend(vint(target.len() as u64));
    data.extend(target);
    record5(5, &data)
}

/// One RAR 5 file (or service) header; every field the test sets.
#[derive(Debug, Clone)]
pub(crate) struct File5 {
    pub(crate) name: Vec<u8>,
    pub(crate) data: Vec<u8>,
    /// 2 file, 3 service.
    pub(crate) kind: u64,
    pub(crate) header_flags: u64,
    /// `0x2 | 0x4` (mtime, CRC32) by default.
    pub(crate) file_flags: u64,
    /// The data's length when `None`.
    pub(crate) unp: Option<u64>,
    pub(crate) attr: u64,
    pub(crate) host: u64,
    /// The data's CRC32 when `None`.
    pub(crate) crc: Option<u32>,
    /// Compression information: 0 is version 0, stored, 128 KiB.
    pub(crate) comp: u64,
    pub(crate) extra: Vec<u8>,
}

impl File5 {
    /// A stored Unix file, mode 0644, with mtime and CRC32.
    pub(crate) fn new(name: &[u8], data: &[u8]) -> File5 {
        File5 {
            name: name.to_vec(),
            data: data.to_vec(),
            kind: 2,
            header_flags: 0,
            file_flags: 0x0002 | 0x0004,
            unp: None,
            attr: 0o100_644,
            host: 1,
            crc: None,
            comp: 0,
            extra: Vec::new(),
        }
    }
}

/// A file or service header, then its data.
pub(crate) fn file5(f: &File5) -> Vec<u8> {
    let mut fields = vint(f.file_flags);
    fields.extend(vint(f.unp.unwrap_or(f.data.len() as u64)));
    fields.extend(vint(f.attr));
    if f.file_flags & 0x0002 != 0 {
        fields.extend(UNIX_2020.to_le_bytes());
    }
    if f.file_flags & 0x0004 != 0 {
        fields.extend(
            f.crc
                .unwrap_or_else(|| crc32fast::hash(&f.data))
                .to_le_bytes(),
        );
    }
    fields.extend(vint(f.comp));
    fields.extend(vint(f.host));
    fields.extend(vint(f.name.len() as u64));
    fields.extend(&f.name);
    let mut out = block5(
        f.kind,
        f.header_flags,
        &fields,
        &f.extra,
        Some(f.data.len() as u64),
    );
    out.extend(&f.data);
    out
}

/// A main header (with a volume number when `flags & 0x0002`).
pub(crate) fn main5(flags: u64) -> Vec<u8> {
    let mut fields = vint(flags);
    if flags & 0x0002 != 0 {
        fields.extend(vint(1));
    }
    block5(1, 0, &fields, &[], None)
}

/// An end of archive header (`0x0001`: more volumes).
pub(crate) fn end5(flags: u64) -> Vec<u8> {
    block5(5, 0, &vint(flags), &[], None)
}

/// A whole RAR 5: signature, main header, `files`, end header.
pub(crate) fn rar5(main_flags: u64, files: &[File5]) -> Vec<u8> {
    let mut out = SIG5.to_vec();
    out.extend(main5(main_flags));
    for f in files {
        out.extend(file5(f));
    }
    out.extend(end5(0));
    out
}

// ---- The cases -----------------------------------------------------------------

/// A stored ZIP holding `inner-from-zip.txt`.
pub(crate) fn tiny_zip() -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(DateTime::from_date_and_time(2020, 1, 2, 3, 4, 6).expect("a date"));
    zip.start_file("inner-from-zip.txt", options)
        .expect("a zip entry");
    zip.write_all(b"this came from the ZIP, not the RAR\n")
        .expect("zip data");
    zip.finish().expect("a zip").into_inner()
}

fn f4(name: &[u8], data: &[u8], edit: impl FnOnce(&mut File4)) -> File4 {
    let mut f = File4::new(name, data);
    edit(&mut f);
    f
}

fn f5(name: &[u8], data: &[u8], edit: impl FnOnce(&mut File5)) -> File5 {
    let mut f = File5::new(name, data);
    edit(&mut f);
    f
}

/// A DOS date and time.
pub(crate) fn dos(year: u32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> u32 {
    ((year - 1980) << 25)
        | (month << 21)
        | (day << 16)
        | (hour << 11)
        | (minute << 5)
        | (second / 2)
}

/// Every synthetic case, by the prototype's names: stored RAR 4 and RAR 5
/// archives, odd ones, and archives cut short.
pub(crate) fn cases() -> Vec<(&'static str, Vec<u8>)> {
    let a4 = || File4::new(b"a.txt", A);
    let a5 = || File5::new(b"a.txt", A);
    let mut cases: Vec<(&'static str, Vec<u8>)> = vec![
        // RAR 4
        ("r4_stored", rar4(0, &[a4()])),
        (
            "r4_solid_synthetic",
            rar4(0x0008, &[a4(), f4(b"b.txt", A, |f| f.flags = 0x0010)]),
        ),
        (
            "r4_zero_len_password_entry",
            rar4(
                0,
                &[f4(b"empty-locked.txt", b"", |f| f.flags = 0x0004), a4()],
            ),
        ),
        ("r4_volume_flag", rar4(0x0001, &[a4()])),
        (
            "r4_zip_last_member",
            rar4(0, &[a4(), File4::new(b"x.zip", &tiny_zip())]),
        ),
        // 0x82 is "é" in CP437.
        (
            "r4_name_cp437_byte",
            rar4(0, &[File4::new(b"caf\x82.txt", A)]),
        ),
        (
            "r4_name_utf8_noflag",
            rar4(0, &[File4::new("café.txt".as_bytes(), A)]),
        ),
        (
            "r4_name_absolute",
            rar4(0, &[File4::new(b"/tmp/abs.txt", A)]),
        ),
        (
            "r4_name_backslash_dotdot",
            rar4(
                0,
                &[f4(b"..\\up.txt", A, |f| {
                    f.host = 2;
                    f.attr = 0x20;
                })],
            ),
        ),
        (
            "r4_bad_data_crc",
            rar4(
                0,
                &[
                    f4(b"a.txt", A, |f| f.crc = Some(0x1234_5678)),
                    File4::new(b"b.txt", A),
                ],
            ),
        ),
        (
            "r4_short_data",
            rar4(
                0,
                &[f4(b"a.txt", &A[..5], |f| {
                    f.unp = Some(A.len() as u64);
                    f.crc = Some(crc32fast::hash(A));
                })],
            ),
        ),
        (
            "r4_fifo",
            rar4(0, &[f4(b"fifo", b"", |f| f.attr = 0o010_644)]),
        ),
        ("r4_mode_zero", rar4(0, &[f4(b"notype", A, |f| f.attr = 0)])),
        (
            "r4_symlink_2mib",
            rar4(
                0,
                &[f4(b"biglink", &vec![b'a'; 2 << 20], |f| f.attr = 0o120_777)],
            ),
        ),
        (
            "r4_dostime_tz",
            rar4(
                0,
                &[f4(b"a.txt", A, |f| f.dostime = dos(2020, 6, 1, 12, 0, 0))],
            ),
        ),
        ("r4_sfx_mz_prefix", {
            let mut out = b"MZ".to_vec();
            out.resize(0x10000, 0);
            out.extend(rar4(0, &[a4()]));
            out
        }),
        ("r4_junk_prefix", {
            let mut out = b"JUNK".repeat(4);
            out.extend(rar4(0, &[a4()]));
            out
        }),
        // RAR 5
        ("r5_stored", rar5(0, &[a5()])),
        (
            "r5_junction",
            rar5(
                0,
                &[
                    f5(b"target", b"", |f| {
                        f.attr = 0o40_755;
                        f.file_flags = 0x0001 | 0x0002;
                    }),
                    f5(b"junc", b"", |f| {
                        f.host = 0;
                        f.attr = 0x400 | 0x10;
                        f.file_flags = 0x0001 | 0x0002;
                        f.extra = redirect5(3, b"C:\\Windows", true);
                    }),
                ],
            ),
        ),
        (
            "r5_filecopy",
            rar5(
                0,
                &[
                    File5::new(b"orig.txt", A),
                    f5(b"copy.txt", b"", |f| {
                        f.file_flags = 0x0002;
                        f.unp = Some(A.len() as u64);
                        f.extra = redirect5(5, b"orig.txt", false);
                    }),
                ],
            ),
        ),
        (
            "r5_unix_symlink",
            rar5(
                0,
                &[
                    a5(),
                    f5(b"ln", b"", |f| {
                        f.attr = 0o120_777;
                        f.file_flags = 0x0002;
                        f.extra = redirect5(1, b"a.txt", false);
                    }),
                ],
            ),
        ),
        (
            "r5_win_symlink",
            rar5(
                0,
                &[
                    a5(),
                    f5(b"wln", b"", |f| {
                        f.host = 0;
                        f.attr = 0x400 | 0x20;
                        f.file_flags = 0x0002;
                        f.extra = redirect5(2, b"a.txt", false);
                    }),
                ],
            ),
        ),
        (
            "r5_hardlink",
            rar5(
                0,
                &[
                    a5(),
                    f5(b"hl.txt", b"", |f| {
                        f.file_flags = 0x0002;
                        f.unp = Some(A.len() as u64);
                        f.extra = redirect5(4, b"a.txt", false);
                    }),
                ],
            ),
        ),
        (
            "r5_dict_128mib",
            rar5(0, &[f5(b"a.txt", A, |f| f.comp = 10 << 10)]),
        ),
        (
            "r5_dict_n16_bit14",
            rar5(0, &[f5(b"a.txt", A, |f| f.comp = 16 << 10)]),
        ),
        (
            "r5_unknown_size",
            rar5(
                0,
                &[f5(b"a.txt", A, |f| f.file_flags = 0x0002 | 0x0004 | 0x0008)],
            ),
        ),
        (
            "r5_name_invalid_utf8",
            rar5(0, &[File5::new(b"bad\xff.txt", A), File5::new(b"b.txt", A)]),
        ),
        ("r5_volume_flag", rar5(0x0001, &[a5()])),
        (
            "r5_solid_flag_first",
            rar5(0x0004, &[f5(b"a.txt", A, |f| f.comp = 0x40)]),
        ),
        (
            "r5_zip_last_member",
            rar5(0, &[a5(), File5::new(b"x.zip", &tiny_zip())]),
        ),
        (
            "r5_bad_data_crc",
            rar5(
                0,
                &[
                    f5(b"a.txt", A, |f| f.crc = Some(0x1234_5678)),
                    File5::new(b"b.txt", A),
                ],
            ),
        ),
        (
            "r5_zero_len_encrypted",
            rar5(
                0,
                &[
                    f5(b"locked-empty.txt", b"", |f| f.extra = record5(1, &[0; 33])),
                    a5(),
                ],
            ),
        ),
        (
            "r5_encrypted_stored",
            rar5(
                0,
                &[f5(b"locked.txt", A, |f| f.extra = record5(1, &[0; 33]))],
            ),
        ),
        (
            "r5_junction_nodirflag",
            rar5(
                0,
                &[f5(b"junc", b"", |f| {
                    f.host = 0;
                    f.attr = 0x400;
                    f.file_flags = 0x0002;
                    f.extra = redirect5(3, b"C:\\Windows", true);
                })],
            ),
        ),
    ];
    // Cut or end-less archives: prefixes of the cases above.
    let find = |cases: &[(&str, Vec<u8>)], name: &str| -> Vec<u8> {
        cases
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, bytes)| bytes.clone())
            .expect("a case")
    };
    let r4 = find(&cases, "r4_stored");
    let r5 = find(&cases, "r5_stored");
    let r4_zip = find(&cases, "r4_zip_last_member");
    let r5_zip = find(&cases, "r5_zip_last_member");
    cases.extend([
        ("r4_no_end_block", r4[..r4.len() - 7].to_vec()),
        ("r5_no_end_header", r5[..r5.len() - 8].to_vec()),
        ("r4_cut_at_70_inside_data", r4[..70].to_vec()),
        ("r4_cut_at_100_inside_2nd_header", r4_zip[..100].to_vec()),
        ("r5_cut_at_140_inside_2nd_entry", r5_zip[..140].to_vec()),
    ]);
    cases
}

/// The case named `name`.
pub(crate) fn case(name: &str) -> Vec<u8> {
    cases()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, bytes)| bytes)
        .unwrap_or_else(|| panic!("no case {name}"))
}
