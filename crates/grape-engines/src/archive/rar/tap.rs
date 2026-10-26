//! [`RarTap`]: holds bsdtar's tar to the RAR's own headers. bsdtar writes
//! exactly one tar entry per RAR file entry, in order (service headers
//! and comments give none, and it adds no folders), so the walk steps
//! through the preflight's [`Expect`] list in step with the tar: kind,
//! size and CRC32 must match one to one. That catches a damaged stored
//! entry (bsdtar doesn't check a stored entry's CRC32) and an entry
//! bsdtar dropped. Names and link targets are made NFC on the way: bsdtar
//! on macOS returns NFD. A file whose name would read as a folder (`a/`,
//! `notes\`, `./`) is refused, so its data is never dropped.

use unicode_normalization::{is_nfc, UnicodeNormalization};

use super::super::names;
use super::super::source::{Raw, RawKind, Tap, TarFacts};
use super::super::Failure;
use super::header::{Expect, Skipped};
use super::RarFailure;

/// Walks the preflight's entries in step with bsdtar's.
pub(crate) struct RarTap<'p> {
    plan: &'p [Expect],
    /// The entry being read.
    next: usize,
    /// The current regular file's CRC32 so far.
    hasher: crc32fast::Hasher,
    /// The current entry's data bytes so far.
    seen: u64,
}

impl<'p> RarTap<'p> {
    /// A tap over the preflight's `plan`.
    pub(crate) fn new(plan: &'p [Expect]) -> RarTap<'p> {
        RarTap {
            plan,
            next: 0,
            hasher: crc32fast::Hasher::new(),
            seen: 0,
        }
    }

    /// After the tar: bsdtar wrote every entry the RAR has.
    pub(crate) fn finish(&self) -> Result<(), Failure> {
        if self.next == self.plan.len() {
            Ok(())
        } else {
            Err(Failure::Corrupt(
                "bsdtar wrote fewer entries than the archive has".into(),
            ))
        }
    }
}

/// `text` in NFC.
fn nfc(text: &mut String) {
    if !is_nfc(text) {
        *text = text.nfc().collect();
    }
}

impl Tap for RarTap<'_> {
    const ACTIVE: bool = true;

    fn entry(&mut self, facts: &TarFacts, raw: &mut Raw) -> Result<(), Failure> {
        let i = self.next;
        let Some(expect) = self.plan.get(i) else {
            return Err(Failure::Corrupt(
                "bsdtar wrote more entries than the archive has".into(),
            ));
        };
        if facts.binary_name {
            return Err(Failure::Rar(RarFailure::Names("hdrcharset=BINARY")));
        }
        let mismatch = || Failure::Corrupt(format!("entry {i} doesn't match its header"));
        match expect {
            Expect::Data { size, .. } => {
                if !(facts.regular && facts.size == *size) {
                    return Err(mismatch());
                }
                // A file named `notes\` (legal on Unix), `a/` or `./`: the
                // name check would read the trailing separator as a folder
                // and drop the data, or skip `./` as the archive's root.
                if names::parse(&raw.name).is_some_and(|p| p.trailing_slash || p.path.is_empty()) {
                    return Err(Failure::UnsafeName(names::shown(&raw.name)));
                }
            }
            Expect::Skip(skipped) => {
                if facts.size != 0 {
                    return Err(mismatch());
                }
                raw.kind = match skipped {
                    // A folder link: left out and counted as a link.
                    Skipped::Junction => RawKind::Symlink(None),
                    Skipped::FileCopy => RawKind::FileCopy,
                };
            }
            Expect::Pass => {
                if facts.regular && facts.size > 0 {
                    return Err(mismatch());
                }
            }
        }
        nfc(&mut raw.name);
        if let RawKind::Symlink(Some(target)) = &mut raw.kind {
            nfc(target);
        }
        self.hasher = crc32fast::Hasher::new();
        self.seen = 0;
        Ok(())
    }

    fn data(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.seen = self.seen.saturating_add(bytes.len() as u64);
    }

    fn done(&mut self) -> Result<(), Failure> {
        let i = self.next;
        if let Some(Expect::Data { size, crc }) = self.plan.get(i) {
            if self.seen != *size {
                return Err(Failure::Corrupt(format!(
                    "entry {i} doesn't match its header"
                )));
            }
            let hasher = std::mem::replace(&mut self.hasher, crc32fast::Hasher::new());
            if crc.is_some_and(|crc| hasher.finalize() != crc) {
                return Err(Failure::Corrupt(format!("entry {i} fails its CRC32")));
            }
        }
        self.next += 1;
        Ok(())
    }
}
