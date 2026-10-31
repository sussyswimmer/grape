//! The RAR preflight: a walk of the archive's headers in Rust, before
//! bsdtar starts. It reads the headers only (the data is skipped by its
//! declared size) and gives the [`Plan`] bsdtar's output is held to, or
//! the reason Grape refuses the archive.
//!
//! RAR 4 follows the RAR 2.9 technote as libarchive reads it
//! (`archive_read_support_format_rar.c`); RAR 5 follows RARLAB's RAR 5.0
//! technote, with libarchive's checks (`…_rar5.c`). Where the two differ,
//! this copies libarchive's reading. The walk doesn't stop at the first
//! finding: it stops at the end block, at encrypted headers, at damage or
//! at the bounds, then reports the worst finding: encrypted, multi-volume,
//! solid RAR 4, dictionary, feature, damage, two RAR 4 files in a row with
//! one name, too many entries.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

use grape_core::Format;

use super::super::names;
use super::super::sniff::{RAR4_SIG, RAR5_SIG};
use super::super::stream::{read_prefix, Run};
use super::super::{Failure, Over};
use super::RarFailure;
use crate::cancel::Cancellable;
use crate::CancelToken;

/// The read buffer on the RAR, and the most read from it at once: every
/// read from the file checks the cancel token, so a 2 MiB RAR 5 header
/// is read in steps.
const BUF: usize = 64 * 1024;

/// How many headers are walked between cancel checks, for headers that
/// are already in the buffer.
const CHECK_EVERY: u64 = 64;

/// The largest RAR 5 dictionary libarchive reads.
const MAX_DICTIONARY: u64 = 64 << 20;

/// The largest RAR 5 header, its size field included (libarchive's
/// `MAX_HEADER_SIZE`).
const MAX_HEADER5: u64 = 2 << 20;

/// The longest RAR 5 name, in bytes (libarchive: 2048 with the NUL).
const MAX_NAME5: u64 = 2047;

/// How far into a `.rar` that starts like an executable a signature is
/// looked for. RARLAB puts the self-extracting module at most 1 MB before
/// the archive.
const SFX_SCAN: usize = 1 << 20;

/// What bsdtar must write for each RAR file entry, in order, and the sum
/// of the regular files' sizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// One per file entry (not per service header or comment).
    pub(crate) entries: Vec<Expect>,
    /// The saturating sum of the [`Expect::Data`] sizes.
    pub(crate) declared: u64,
}

/// What one RAR file entry becomes in bsdtar's tar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    /// A regular file with data: bsdtar must write a regular tar entry of
    /// exactly `size` bytes whose CRC32 is `crc` (when the RAR stores one).
    Data { size: u64, crc: Option<u32> },
    /// A RAR5 junction or file copy: bsdtar writes an empty folder or file.
    /// Grape leaves it out and counts it.
    Skip(Skipped),
    /// Everything else (folders, symlinks, hard links, devices): no data.
    Pass,
}

/// The RAR5 entries Grape leaves out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skipped {
    /// A Windows junction (redirection type 3), counted as a link.
    Junction,
    /// A file copy (redirection type 5), counted in `LeftOut::copies`.
    FileCopy,
}

/// Walks `file` (`len` bytes, at its start)'s headers, then seeks it back
/// to its start.
pub(crate) fn preflight(run: &Run<'_>, file: &mut File, len: u64) -> Result<Plan, Failure> {
    let (walked, findings, plan) = {
        let mut walker = Walker {
            run,
            reader: Reader {
                inner: BufReader::with_capacity(BUF, Cancellable::new(&mut *file, run.token())),
                cancel: run.token().clone(),
                pos: 0,
                len,
            },
            buf: Vec::new(),
            plan: Plan {
                entries: Vec::new(),
                declared: 0,
            },
            found: Findings::default(),
            headers: 0,
            last_name4: None,
        };
        let walked = walker.walk();
        (walked, walker.found, walker.plan)
    };
    if let Err(Stop::Fail(failure)) = walked {
        return Err(failure);
    }
    if let Some(failure) = findings.worst() {
        return Err(failure);
    }
    file.seek(SeekFrom::Start(0)).map_err(source_failed)?;
    Ok(plan)
}

/// Why a file that matched no signature at its start isn't read: a RAR of
/// a version after 5 (any `.zip`, `.tar` or `.rar` alike), a
/// self-extracting RAR (only for a `.rar`, after a scan of its first MiB),
/// or no archive at all. `head` is the file's first bytes.
pub(crate) fn foreign(file: &mut File, head: &[u8], from: Format) -> Failure {
    if let Some(rest) = head.strip_prefix(b"Rar!\x1A\x07") {
        // RAR4_SIG and RAR5_SIG were sniffed already; a cut one is no RAR.
        let newer = match rest {
            [0x01, next, ..] => *next != 0,
            [version, ..] => *version > 1,
            [] => false,
        };
        if newer {
            return Failure::Rar(RarFailure::Feature("RAR format version"));
        }
    }
    let executable = head.starts_with(b"MZ") || head.starts_with(b"\x7FELF");
    if from == Format::Rar && executable {
        return match has_signature(file) {
            Ok(true) => Failure::Rar(RarFailure::Sfx),
            Ok(false) => Failure::NotArchive,
            Err(e) => source_failed(e),
        };
    }
    Failure::NotArchive
}

/// True when a RAR signature starts in the first [`SFX_SCAN`] bytes.
fn has_signature(file: &mut File) -> io::Result<bool> {
    file.seek(SeekFrom::Start(0))?;
    let bytes = read_prefix(file, SFX_SCAN + RAR5_SIG.len() - 1)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(bytes
        .windows(RAR4_SIG.len())
        .take(SFX_SCAN)
        .enumerate()
        .any(|(at, window)| window == RAR4_SIG || bytes[at..].starts_with(RAR5_SIG)))
}

fn source_failed(e: io::Error) -> Failure {
    Failure::Read(format!("reading the source: {:?}", e.kind()))
}

/// The findings so far; each keeps the first reason it saw (for the log).
#[derive(Debug, Default)]
struct Findings {
    encrypted: bool,
    multi_volume: Option<&'static str>,
    solid4: Option<&'static str>,
    dictionary: Option<u64>,
    feature: Option<&'static str>,
    corrupt: Option<String>,
    /// Two RAR 4 files in a row with one name (the name, as shown).
    duplicate: Option<String>,
    too_many: bool,
}

impl Findings {
    /// The finding that is reported, if any.
    fn worst(self) -> Option<Failure> {
        if self.encrypted {
            Some(Failure::Encrypted)
        } else if let Some(why) = self.multi_volume {
            Some(Failure::Rar(RarFailure::MultiVolume(why)))
        } else if let Some(why) = self.solid4 {
            Some(Failure::Rar(RarFailure::Solid4(why)))
        } else if let Some(bytes) = self.dictionary {
            Some(Failure::Rar(RarFailure::Dictionary(bytes)))
        } else if let Some(what) = self.feature {
            Some(Failure::Rar(RarFailure::Feature(what)))
        } else if let Some(why) = self.corrupt {
            Some(Failure::Corrupt(why))
        } else if let Some(name) = self.duplicate {
            Some(Failure::Duplicate(name))
        } else if self.too_many {
            Some(Failure::TooBig(Over::Entries))
        } else {
            None
        }
    }
}

/// Why the walk ended early.
#[derive(Debug)]
enum Stop {
    /// A finding that ends the walk; it is in the findings already.
    Found,
    /// A cancel, or the source couldn't be read: reported as is.
    Fail(Failure),
}

/// The RAR, read forward through a buffer, its position counted. Every
/// read from the file fails once the job is cancelled.
struct Reader<'f> {
    inner: BufReader<Cancellable<&'f mut File>>,
    cancel: CancelToken,
    pos: u64,
    len: u64,
}

impl Reader<'_> {
    fn left(&self) -> u64 {
        self.len.saturating_sub(self.pos)
    }

    /// A failed read or seek: the cancel, or the source.
    fn failed(&self, e: io::Error) -> Stop {
        if self.cancel.is_cancelled() {
            Stop::Fail(Failure::Cancelled)
        } else {
            Stop::Fail(source_failed(e))
        }
    }

    /// Appends the next `n` bytes to `out`, at most [`BUF`] at a time.
    /// `Ok(false)` if the file ends first.
    fn read_into(&mut self, out: &mut Vec<u8>, n: u64) -> Result<bool, Stop> {
        if n > self.left() {
            return Ok(false);
        }
        let before = out.len();
        let mut left = n;
        while left > 0 {
            let step = left.min(BUF as u64);
            match (&mut self.inner).take(step).read_to_end(out) {
                Ok(got) if got as u64 == step => left -= step,
                // The file got shorter since its size was read.
                Ok(_) => {
                    out.truncate(before);
                    return Ok(false);
                }
                Err(e) => return Err(self.failed(e)),
            }
        }
        self.pos += n;
        Ok(true)
    }

    /// Skips the next `n` bytes. `Ok(false)` if the file ends first.
    fn skip(&mut self, n: u64) -> Result<bool, Stop> {
        if n > self.left() {
            return Ok(false);
        }
        let by = i64::try_from(n).map_err(|_| Stop::Fail(Failure::Corrupt("skip".into())))?;
        self.inner.seek_relative(by).map_err(|e| self.failed(e))?;
        self.pos += n;
        Ok(true)
    }
}

/// The walk's state.
struct Walker<'r, 'a, 'f> {
    run: &'r Run<'a>,
    reader: Reader<'f>,
    /// The current header's bytes (reused).
    buf: Vec<u8>,
    plan: Plan,
    found: Findings,
    /// Headers walked so far, of any kind.
    headers: u64,
    /// The last RAR 4 file's name, as libarchive compares it.
    last_name4: Option<Vec<u8>>,
}

impl Walker<'_, '_, '_> {
    fn walk(&mut self) -> Result<(), Stop> {
        let mut sig = Vec::with_capacity(RAR5_SIG.len());
        self.read(&mut sig, RAR4_SIG.len() as u64, "the signature")?;
        if sig == RAR4_SIG {
            return self.rar4();
        }
        self.read(&mut sig, 1, "the signature")?;
        if sig == RAR5_SIG {
            return self.rar5();
        }
        Err(self.corrupt("no RAR signature at the start"))
    }

    /// Records damage (the first reason only) and ends the walk.
    fn corrupt(&mut self, why: impl Into<String>) -> Stop {
        if self.found.corrupt.is_none() {
            self.found.corrupt = Some(why.into());
        }
        Stop::Found
    }

    fn multi_volume(&mut self, why: &'static str) {
        self.found.multi_volume.get_or_insert(why);
    }

    fn solid4(&mut self, why: &'static str) {
        self.found.solid4.get_or_insert(why);
    }

    fn feature(&mut self, what: &'static str) {
        self.found.feature.get_or_insert(what);
    }

    /// Reads `n` more bytes into `out`; a file that ends first is damaged.
    fn read(&mut self, out: &mut Vec<u8>, n: u64, what: &str) -> Result<(), Stop> {
        if self.reader.read_into(out, n)? {
            Ok(())
        } else {
            Err(self.corrupt(format!("cut inside {what}")))
        }
    }

    /// Skips `n` bytes of data; data past the end is damage.
    fn skip(&mut self, n: u64, what: &str) -> Result<(), Stop> {
        if self.reader.skip(n)? {
            Ok(())
        } else {
            Err(self.corrupt(format!("{what} runs past the end")))
        }
    }

    /// Counts one more header: the cancel check, the bound on headers, and
    /// the end of the file where a header should start.
    fn next_header(&mut self) -> Result<(), Stop> {
        self.headers += 1;
        if self.headers % CHECK_EVERY == 1 {
            self.run.check().map_err(Stop::Fail)?;
        }
        if self.headers > self.run.limits.max_entries.saturating_mul(4) {
            self.found.too_many = true;
            return Err(Stop::Found);
        }
        if self.reader.left() == 0 {
            return Err(self.corrupt("no end-of-archive block"));
        }
        Ok(())
    }

    /// Adds one file entry to the plan.
    fn push(&mut self, expect: Expect) -> Result<(), Stop> {
        if let Expect::Data { size, .. } = expect {
            self.plan.declared = self.plan.declared.saturating_add(size);
        }
        self.plan.entries.push(expect);
        if self.plan.entries.len() as u64 > self.run.limits.max_entries {
            self.found.too_many = true;
            return Err(Stop::Found);
        }
        Ok(())
    }

    // ---- RAR 4 -----------------------------------------------------------

    fn rar4(&mut self) -> Result<(), Stop> {
        loop {
            self.next_header()?;
            let mut block = std::mem::take(&mut self.buf);
            block.clear();
            let step = self.block4(&mut block);
            self.buf = block;
            if step? {
                return Ok(());
            }
        }
    }

    /// One RAR 4 block, into `block`. `Ok(true)` after the end block.
    fn block4(&mut self, block: &mut Vec<u8>) -> Result<bool, Stop> {
        self.read(block, 7, "a RAR 4 block")?;
        let crc = le16(block, 0);
        let kind = block[2];
        let flags = le16(block, 3);
        let size = u64::from(le16(block, 5));
        if size < 7 {
            return Err(self.corrupt("a RAR 4 block shorter than 7 bytes"));
        }
        match kind {
            // MARK: the signature again, as libarchive takes it.
            0x72 => {
                if block.as_slice() != RAR4_SIG {
                    return Err(self.corrupt("a RAR 4 mark block that isn't the signature"));
                }
            }
            0x73 => self.main4(block, crc, flags, size)?,
            0x74 | 0x7A => self.file4(block, kind, crc, flags, size)?,
            // COMM, AV, SUB, PROTECT, SIGN: skipped whole; libarchive
            // checks their CRC itself.
            0x75..=0x79 => self.skip4(block, flags, size)?,
            0x7B => {
                if flags & 0x0001 != 0 {
                    self.multi_volume("end block: the archive continues");
                }
                self.skip4(block, flags, size)?;
                return Ok(true);
            }
            _ => return Err(self.corrupt("an unknown RAR 4 block type")),
        }
        Ok(false)
    }

    /// Reads the rest of a `size`-byte header into `block` and checks its
    /// CRC (the low 16 bits of CRC32 over bytes 2 to `size`).
    fn header4(
        &mut self,
        block: &mut Vec<u8>,
        crc: u16,
        size: u64,
        what: &str,
    ) -> Result<(), Stop> {
        self.read(block, size - 7, what)?;
        if crc16(&block[2..]) != crc {
            return Err(self.corrupt(format!("{what}: header CRC")));
        }
        Ok(())
    }

    fn main4(&mut self, block: &mut Vec<u8>, crc: u16, flags: u16, size: u64) -> Result<(), Stop> {
        // RESERVED1 and RESERVED2, then ENCRYPTVER with MHD_ENCRYPTVER.
        let least = if flags & 0x0200 != 0 { 14 } else { 13 };
        if size < least {
            return Err(self.corrupt("a RAR 4 main header too short"));
        }
        self.header4(block, crc, size, "the RAR 4 main header")?;
        if flags & 0x0080 != 0 {
            // MHD_PASSWORD: every header after this one is encrypted.
            self.found.encrypted = true;
            return Err(Stop::Found);
        }
        if flags & 0x0001 != 0 {
            self.multi_volume("main header VOLUME flag");
        }
        if flags & 0x0008 != 0 {
            self.solid4("main header SOLID flag");
        }
        Ok(())
    }

    fn file4(
        &mut self,
        block: &mut Vec<u8>,
        kind: u8,
        crc: u16,
        flags: u16,
        size: u64,
    ) -> Result<(), Stop> {
        let large = flags & 0x0100 != 0;
        let fixed: u64 = if large { 40 } else { 32 };
        if size < fixed {
            return Err(self.corrupt("a RAR 4 file header too short"));
        }
        self.header4(block, crc, size, "a RAR 4 file header")?;
        let high = |at| {
            if large {
                u64::from(le32(block, at)) << 32
            } else {
                0
            }
        };
        let packed = u64::from(le32(block, 7)) | high(32);
        let unpacked = u64::from(le32(block, 11)) | high(36);
        if packed > i64::MAX as u64 || unpacked > i64::MAX as u64 {
            return Err(self.corrupt("a RAR 4 size over 2^63"));
        }
        if kind == 0x7A {
            // NEWSUB (comments, NTFS streams, ACLs): not an entry, and
            // its encryption doesn't matter (libarchive skips it).
            if flags & 0x0010 != 0 {
                self.solid4("service block SOLID flag");
            }
        } else {
            let host = block[15];
            let file_crc = le32(block, 16);
            let name_size = u64::from(le16(block, 26));
            let attr = le32(block, 28);
            if fixed + name_size > size {
                return Err(self.corrupt("a RAR 4 name past its header"));
            }
            if flags & 0x0004 != 0 {
                self.found.encrypted = true;
            }
            if flags & 0x0003 != 0 {
                self.multi_volume("entry split flag");
            }
            if flags & 0x0010 != 0 {
                self.solid4("entry SOLID flag");
            }
            let name = &block[fixed as usize..(fixed + name_size) as usize];
            self.repeated4(flags, name);
            let data = Expect::Data {
                size: unpacked,
                crc: Some(file_crc),
            };
            let expect = match host {
                // MS-DOS, OS/2, Win32: the directory attribute.
                0..=2 if attr & 0x10 != 0 => Expect::Pass,
                0..=2 => data,
                // Unix, Mac OS, BeOS: the mode's type bits.
                3..=5 if attr & 0o170_000 == 0o100_000 => data,
                3..=5 => Expect::Pass,
                _ => return Err(self.corrupt("an unknown RAR 4 host OS")),
            };
            self.push(expect)?;
        }
        self.skip(packed, "RAR 4 data")
    }

    /// libarchive takes a RAR 4 file named as the one before it for the
    /// next part of a split file, even without the split flag, and gives
    /// it no name: bsdtar skips it with a warning, exit 0. Without the
    /// flag, that's two items with one name. The names compare as
    /// libarchive reads them: `\` as `/`, or the UTF-16 form (Unicode flag
    /// and a NUL) as stored.
    fn repeated4(&mut self, flags: u16, name: &[u8]) {
        let slash = |b: &u8| if *b == b'\\' { b'/' } else { *b };
        let key: Vec<u8> = if flags & 0x0200 != 0 && name.contains(&0) {
            std::iter::once(1).chain(name.iter().copied()).collect()
        } else {
            std::iter::once(0).chain(name.iter().map(slash)).collect()
        };
        if flags & 0x0001 == 0 && self.last_name4.as_ref() == Some(&key) {
            // The plain name (the UTF-16 form keeps one before its NUL).
            let plain: Vec<u8> = name.iter().take_while(|b| **b != 0).map(slash).collect();
            self.found
                .duplicate
                .get_or_insert_with(|| names::shown(&names::decode(&plain)));
        }
        self.last_name4 = Some(key);
    }

    /// Skips a block that isn't read for meaning: its header, then
    /// ADD_SIZE bytes when `HEAD_FLAGS & 0x8000`.
    fn skip4(&mut self, block: &mut Vec<u8>, flags: u16, size: u64) -> Result<(), Stop> {
        let mut rest = size - 7;
        let mut add = 0;
        if flags & 0x8000 != 0 {
            if size < 11 {
                return Err(self.corrupt("a RAR 4 block too short for its ADD_SIZE"));
            }
            self.read(block, 4, "a RAR 4 block")?;
            add = u64::from(le32(block, 7));
            rest -= 4;
        }
        self.skip(rest + add, "a RAR 4 block")
    }

    // ---- RAR 5 -----------------------------------------------------------

    fn rar5(&mut self) -> Result<(), Stop> {
        loop {
            self.next_header()?;
            let mut block = std::mem::take(&mut self.buf);
            block.clear();
            let step = self.block5(&mut block);
            self.buf = block;
            if step? {
                return Ok(());
            }
        }
    }

    /// One RAR 5 block, its header into `block` (from the size field on).
    /// `Ok(true)` after the end of archive header.
    fn block5(&mut self, block: &mut Vec<u8>) -> Result<bool, Stop> {
        let mut crc = Vec::with_capacity(4);
        self.read(&mut crc, 4, "a RAR 5 header")?;
        let crc = le32(&crc, 0);
        // The header size: a vint of at most 3 bytes (technote).
        let mut size: u64 = 0;
        for byte in 0..3 {
            self.read(block, 1, "a RAR 5 header")?;
            let b = block[byte];
            size |= u64::from(b & 0x7F) << (7 * byte);
            if b & 0x80 == 0 {
                break;
            }
            if byte == 2 {
                return Err(self.corrupt("a RAR 5 header size over 3 bytes"));
            }
        }
        let size_len = block.len() as u64;
        if size < 2 || size + size_len > MAX_HEADER5 {
            return Err(self.corrupt("a RAR 5 header size out of range"));
        }
        self.read(block, size, "a RAR 5 header")?;
        if crc32fast::hash(block) != crc {
            return Err(self.corrupt("a RAR 5 header CRC"));
        }
        let header = &block[..];
        let mut at = size_len as usize;
        let parsed = (|| {
            let kind = vint(header, &mut at)?;
            let flags = vint(header, &mut at)?;
            let extra = if flags & 0x0001 != 0 {
                vint(header, &mut at)?
            } else {
                0
            };
            let data = if flags & 0x0002 != 0 {
                vint(header, &mut at)?
            } else {
                0
            };
            // The extra area is the header's last `extra` bytes.
            let extra_start = (header.len() as u64).checked_sub(extra)?;
            let extra_start = usize::try_from(extra_start).ok().filter(|&s| s >= at)?;
            Some((kind, flags, extra_start, data))
        })();
        let Some((kind, flags, extra_start, data)) = parsed else {
            return Err(self.corrupt("a RAR 5 header's sizes"));
        };
        if data > i64::MAX as u64 {
            return Err(self.corrupt("a RAR 5 data size over 2^63"));
        }
        let fields = &header[at..extra_start];
        let extra = &header[extra_start..];
        match kind {
            1 => self.main5(fields)?,
            2 | 3 => self.file5(kind, flags, fields, extra, data)?,
            4 => {
                // Archive encryption: every header after it is encrypted.
                self.found.encrypted = true;
                return Err(Stop::Found);
            }
            5 => {
                let mut at = 0;
                let Some(end_flags) = vint(fields, &mut at) else {
                    return Err(self.corrupt("the RAR 5 end header's flags"));
                };
                if end_flags & 0x0001 != 0 {
                    self.multi_volume("end header: not the last volume");
                }
                return Ok(true);
            }
            // Skip-if-unknown, else "Header type error" (libarchive).
            _ if flags & 0x0004 != 0 => {}
            _ => return Err(self.corrupt("an unknown RAR 5 header type")),
        }
        self.skip(data, "RAR 5 data")?;
        Ok(false)
    }

    fn main5(&mut self, fields: &[u8]) -> Result<(), Stop> {
        let mut at = 0;
        let Some(archive_flags) = vint(fields, &mut at) else {
            return Err(self.corrupt("the RAR 5 main header's flags"));
        };
        if archive_flags & 0x0001 != 0 {
            self.multi_volume("main header VOLUME flag");
        }
        if archive_flags & 0x0002 != 0 {
            self.multi_volume("main header volume number");
            if vint(fields, &mut at).is_none() {
                return Err(self.corrupt("the RAR 5 main header's volume number"));
            }
        }
        Ok(())
    }

    /// A file header (`kind` 2) or a service header (3), whose data area
    /// is `data` bytes; they share a layout. Only file headers are entries.
    fn file5(
        &mut self,
        kind: u64,
        flags: u64,
        fields: &[u8],
        extra: &[u8],
        data: u64,
    ) -> Result<(), Stop> {
        let file = kind == 2;
        if flags & 0x0018 != 0 {
            self.multi_volume("data split across volumes");
        }
        let mut at = 0;
        let Some((file_flags, unpacked, attr)) = (|| {
            Some((
                vint(fields, &mut at)?,
                vint(fields, &mut at)?,
                vint(fields, &mut at)?,
            ))
        })() else {
            return Err(self.corrupt("a RAR 5 file header's fields"));
        };
        let dir = file_flags & 0x0001 != 0;
        if file_flags & 0x0008 != 0 {
            self.feature("unknown unpacked size");
        }
        if file_flags & 0x0002 != 0 && take(fields, &mut at, 4).is_none() {
            return Err(self.corrupt("a RAR 5 file header's mtime"));
        }
        let crc = if file_flags & 0x0004 != 0 {
            match take(fields, &mut at, 4) {
                Some(bytes) => Some(le32(bytes, 0)),
                None => return Err(self.corrupt("a RAR 5 file header's CRC32")),
            }
        } else {
            None
        };
        let Some(compression) = vint(fields, &mut at) else {
            return Err(self.corrupt("a RAR 5 file header's compression"));
        };
        self.compression5(file, dir, compression);
        let Some((host, name_len)) = (|| Some((vint(fields, &mut at)?, vint(fields, &mut at)?)))()
        else {
            return Err(self.corrupt("a RAR 5 file header's fields"));
        };
        if host > 1 {
            return Err(self.corrupt("an unknown RAR 5 host OS"));
        }
        if name_len == 0 || name_len > MAX_NAME5 {
            return Err(self.corrupt("a RAR 5 name length out of range"));
        }
        if usize::try_from(name_len)
            .ok()
            .and_then(|n| take(fields, &mut at, n))
            .is_none()
        {
            return Err(self.corrupt("a RAR 5 name past its header"));
        }
        if unpacked > i64::MAX as u64 {
            return Err(self.corrupt("a RAR 5 unpacked size over 2^63"));
        }
        let redirect = self.extra5(file, extra)?;
        if !file {
            // CMT, QO, ACL, STM, RR: skipped by libarchive, not entries.
            return Ok(());
        }
        let stored = (compression >> 7) & 0x7 == 0;
        let expect = match redirect {
            // Unix symlink, Windows symlink, hard link.
            Some(1 | 2 | 4) => Expect::Pass,
            Some(3) => Expect::Skip(Skipped::Junction),
            Some(5) => Expect::Skip(Skipped::FileCopy),
            Some(_) => {
                // libarchive writes an unknown one as an empty file.
                self.feature("redirection type");
                Expect::Pass
            }
            None if dir => Expect::Pass,
            // Windows: the directory attribute; Unix: the mode's type bits.
            None if host == 0 && attr & 0x10 != 0 => Expect::Pass,
            None if host == 1 && attr & 0o170_000 != 0o100_000 => Expect::Pass,
            // Stored: the data area is the file. libarchive stops at its end
            // and bsdtar pads a short one with zeros, exit 0, so without a
            // CRC32 nothing after this would notice. Encrypted data is
            // padded, and is refused as encrypted anyway.
            None if stored && data != unpacked && !self.found.encrypted => {
                return Err(self.corrupt("a stored RAR 5 file whose data isn't its size"));
            }
            None => Expect::Data {
                size: unpacked,
                crc,
            },
        };
        self.push(expect)
    }

    /// The compression information's findings: the dictionary (the full
    /// 5-bit field, plus RAR 7's fraction; libarchive reads 4 bits) for
    /// every header but a folder's, and for a file, the version.
    fn compression5(&mut self, file: bool, dir: bool, info: u64) {
        let version = info & 0x3F;
        let method = (info >> 7) & 0x7;
        let n = (info >> 10) & 0x1F;
        let (fraction, v0_algorithm) = if version == 1 {
            ((info >> 15) & 0x1F, info & 0x10_0000 != 0)
        } else {
            (0, false)
        };
        let base = (128u64 << 10) << n;
        let dictionary = base.saturating_add((base / 32).saturating_mul(fraction));
        if !dir && dictionary > MAX_DICTIONARY {
            self.found.dictionary.get_or_insert(dictionary);
        }
        if file {
            if version >= 2 {
                self.feature("compression version");
            } else if version == 1 && method != 0 && !v0_algorithm {
                // libarchive has no RAR 7 decoder.
                self.feature("RAR 7 compression");
            }
        }
    }

    /// Walks a file or service header's extra records; returns a file
    /// header's redirection type, if it has one.
    fn extra5(&mut self, file: bool, extra: &[u8]) -> Result<Option<u64>, Stop> {
        let mut redirect = None;
        let mut at = 0;
        while at < extra.len() {
            let record = vint(extra, &mut at)
                .and_then(|size| usize::try_from(size).ok())
                .and_then(|size| take(extra, &mut at, size));
            let Some(record) = record else {
                return Err(self.corrupt("a RAR 5 extra record overruns its area"));
            };
            let mut inner = 0;
            let Some(kind) = vint(record, &mut inner) else {
                return Err(self.corrupt("a RAR 5 extra record's type"));
            };
            match kind {
                // Encryption: only a file's matters (libarchive skips
                // service data).
                0x01 if file => self.found.encrypted = true,
                0x05 if file => {
                    let parsed = (|| {
                        let kind = vint(record, &mut inner)?;
                        let _flags = vint(record, &mut inner)?;
                        let len = usize::try_from(vint(record, &mut inner)?).ok()?;
                        take(record, &mut inner, len)?;
                        Some(kind)
                    })();
                    match parsed {
                        Some(kind) => redirect = Some(kind),
                        None => return Err(self.corrupt("a RAR 5 redirection record")),
                    }
                }
                // Unknown records are skipped (technote).
                _ => {}
            }
        }
        Ok(redirect)
    }
}

/// A RAR 5 vint at `at` in `bytes`, moving `at` past it: 7 bits a byte,
/// low bits first, at most 10 bytes and 64 bits. `None` if it is cut or
/// too long.
fn vint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    for i in 0..10 {
        let b = *bytes.get(*at + i)?;
        let bits = u64::from(b & 0x7F);
        if i == 9 && bits > 1 {
            return None;
        }
        value |= bits << (7 * i);
        if b & 0x80 == 0 {
            *at += i + 1;
            return Some(value);
        }
    }
    None
}

/// The next `n` bytes at `at` in `bytes`, moving `at` past them.
fn take<'b>(bytes: &'b [u8], at: &mut usize, n: usize) -> Option<&'b [u8]> {
    let end = at.checked_add(n)?;
    let taken = bytes.get(*at..end)?;
    *at = end;
    Some(taken)
}

/// The low 16 bits of CRC32: a RAR 4 header's check.
fn crc16(bytes: &[u8]) -> u16 {
    (crc32fast::hash(bytes) & 0xFFFF) as u16
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn reads_vints_as_the_technote_writes_them() {
        let mut at = 0;
        assert_eq!(vint(&[0x05], &mut at), Some(5));
        assert_eq!(at, 1);
        let mut at = 0;
        assert_eq!(vint(&[0x80, 0x01], &mut at), Some(128));
        assert_eq!(at, 2);
        // Cut.
        assert_eq!(vint(&[0x80], &mut 0), None);
        // u64::MAX takes 10 bytes, the last holding one bit.
        let mut max = vec![0xFF; 9];
        max.push(0x01);
        let mut at = 0;
        assert_eq!(vint(&max, &mut at), Some(u64::MAX));
        assert_eq!(at, 10);
        // Over 64 bits, and over 10 bytes.
        let mut over = vec![0xFF; 9];
        over.push(0x02);
        assert_eq!(vint(&over, &mut 0), None);
        let mut long = vec![0x80; 10];
        long.push(0x00);
        assert_eq!(vint(&long, &mut 0), None);
    }
}
