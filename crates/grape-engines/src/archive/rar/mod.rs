//! RAR input (DECISIONS D27): RAR 4 and RAR 5 → ZIP, TAR or GZIP, and
//! Extract Archive, through Grape's own bsdtar sidecar.
//!
//! 1. [`header::preflight`] walks the RAR's headers in Rust, skipping the
//!    data, and refuses what bsdtar would get wrong or Grape won't read
//!    (encryption, multi-part sets, solid RAR 4, big dictionaries, damage,
//!    a missing end block) before anything starts.
//! 2. [`bsdtar::Running`] starts `bsdtar -c -P -f - --format pax @-` in an
//!    empty private folder with a cleared environment, pumps the RAR into
//!    its stdin from a thread, keeps the first 8 KiB of its stderr, and
//!    kills it on cancel or after a stall.
//! 3. Its stdout, a pax tar, goes through the same walk, name check,
//!    bounds and writers as any tar, always re-packed, never kept. The
//!    [`tap::RarTap`] holds every entry to the preflight's headers, one to
//!    one by kind, size and CRC32, and makes names NFC.
//! 4. [`drain_zeros`] reads the rest of stdout: only zeros after the end
//!    marker. Then [`bsdtar::Running::finish`] judges: the output counts
//!    only with exit 0, nothing on stderr, and every check passed.
//!
//! No `Display` or detail here holds a name, a path or bsdtar's stderr.

pub(crate) mod bsdtar;
pub(crate) mod header;
pub(crate) mod tap;
#[cfg(test)]
pub(crate) mod writer;

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "rar-it"))]
mod it;
#[cfg(all(test, feature = "rar-it"))]
mod vectors;

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use super::sink::Sink;
use super::sniff::is_zeros;
use super::source::{walk_tar_with, Walk};
use super::stream::{Metered, Pump, Run, Side};
use super::Failure;
use crate::{ErrorKind, GrapeError};

pub(crate) use bsdtar::Launch;

/// Where bsdtar is. The dispatch (#22) fills it from `SidecarPaths::bsdtar`
/// when that is `Some`; `archive` takes it this way, as `pdf` takes
/// `PdfPaths`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RarPaths {
    /// The bundled bsdtar executable (absolute).
    pub(crate) bsdtar: PathBuf,
}

/// Why a RAR job failed, beyond the failures every archive shares.
/// `Display` (the log) never holds a name, a path or bsdtar's stderr.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RarFailure {
    /// One part of a multi-part set (why, for the log).
    #[error("multi-volume RAR: {0}")]
    MultiVolume(&'static str),
    /// RAR 4's solid compression, which libarchive reads only to the first
    /// file.
    #[error("solid RAR 4: {0}")]
    Solid4(&'static str),
    /// A RAR 5 dictionary over 64 MiB (its size, 0 if bsdtar said so).
    #[error("RAR dictionary {0} bytes over 64 MiB")]
    Dictionary(u64),
    /// Something libarchive can't read (what).
    #[error("RAR feature: {0}")]
    Feature(&'static str),
    /// A self-extracting archive: the signature isn't at offset 0.
    #[error("self-extracting RAR")]
    Sfx,
    /// Names bsdtar couldn't encode (why).
    #[error("RAR names: {0}")]
    Names(&'static str),
    /// bsdtar took no input and wrote no output for the stall time.
    #[error("bsdtar made no progress for the stall time")]
    Stalled,
    /// bsdtar crashed, exited abnormally, or failed for a reason of
    /// Grape's (the detail says which, never what it printed).
    #[error("bsdtar stopped: {0}")]
    Stopped(String),
    /// bsdtar couldn't be started.
    #[error("starting bsdtar: {0:?}")]
    NoStart(io::ErrorKind),
    /// The file is a RAR, but this install has no bsdtar.
    #[error("a RAR file, but no bsdtar")]
    NoHelper,
}

impl RarFailure {
    /// The error in Grape's words; `source` is the source's file name and
    /// `detail` the log text.
    pub(crate) fn into_error(self, source: &str, detail: String) -> GrapeError {
        let (kind, message) = match self {
            RarFailure::MultiVolume(_) => (
                ErrorKind::NotYetSupported,
                format!(
                    "{source} is one part of a multi-part RAR archive, which Grape can't read yet."
                ),
            ),
            RarFailure::Solid4(_) => (
                ErrorKind::NotYetSupported,
                format!("{source} uses RAR 4's solid compression, which Grape can't read yet."),
            ),
            RarFailure::Dictionary(_) => (
                ErrorKind::NotYetSupported,
                format!("{source} needs a RAR dictionary over 64 MiB, which Grape can't read yet."),
            ),
            RarFailure::Feature(_) => (
                ErrorKind::NotYetSupported,
                format!("{source} uses a RAR feature Grape can't read yet."),
            ),
            RarFailure::Sfx => (
                ErrorKind::NotYetSupported,
                format!("{source} is a self-extracting archive, which Grape can't read yet."),
            ),
            RarFailure::Names(_) => (
                ErrorKind::NotYetSupported,
                format!("{source} has item names in a text encoding Grape can't read yet."),
            ),
            RarFailure::Stalled => (
                ErrorKind::DecodeFailed,
                format!("{source} took too long, so Grape stopped."),
            ),
            RarFailure::Stopped(_) => (
                ErrorKind::DecodeFailed,
                "Grape's RAR helper stopped unexpectedly.".to_string(),
            ),
            RarFailure::NoStart(_) => (
                ErrorKind::NotYetSupported,
                "Grape's RAR helper couldn't start.".to_string(),
            ),
            RarFailure::NoHelper => (
                ErrorKind::NotYetSupported,
                format!("{source} is really a RAR file, which this build can't read yet."),
            ),
        };
        GrapeError::new(kind, message, detail)
    }
}

/// Reads the RAR `file` (`len` bytes, at its start) through bsdtar into
/// `sink`, every entry checked by `walk`.
pub(crate) fn walk(
    run: &Run<'_>,
    mut file: File,
    len: u64,
    launch: &Launch,
    walk: &mut Walk,
    sink: &mut dyn Sink,
) -> Result<(), Failure> {
    let plan = header::preflight(run, &mut file, len)?;
    // The tap holds bsdtar to exactly these sizes, so the sum is a bound.
    run.precheck(plan.declared)?;
    run.check()?;
    let mut child = bsdtar::Running::spawn(launch, file, run.token().clone())?;
    let mut tap = tap::RarTap::new(&plan.entries);
    let stdout = child.stdout(run);
    let result = (|| {
        let stream = Metered::new(run, stdout);
        let rest = walk_tar_with(run, stream, walk, Some(sink), &mut tap)?;
        drain_zeros(run, rest, || child.stdout_total())?;
        tap.finish()
    })();
    let result = child.finish(result, run.token());
    if result.is_ok() {
        run.set_consumed(len);
    }
    result
}

/// Reads what follows the tar's first end block to the end of the stream,
/// charged as data: at least one more zero block (the end marker is two),
/// then only zeros (bsdtar pads to 10,240 bytes on a pipe). `total` gives
/// the stream's length once it is read, a whole number of blocks.
pub(crate) fn drain_zeros<R: Read>(
    run: &Run<'_>,
    mut rest: R,
    total: impl FnOnce() -> u64,
) -> Result<(), Failure> {
    let mut pump = Pump::new(run, &mut rest);
    let mut buf = [0u8; 8 * 1024];
    let mut zeros: u64 = 0;
    loop {
        let n = match pump.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(run.failure(&e, Side::Read, "after the tar")),
        };
        if !is_zeros(&buf[..n]) {
            return Err(Failure::Corrupt("data after the end marker".into()));
        }
        zeros = zeros.saturating_add(n as u64);
    }
    if zeros < 512 {
        return Err(Failure::Corrupt("no tar end marker".into()));
    }
    if !total().is_multiple_of(512) {
        return Err(Failure::Corrupt(
            "a partial block after the end marker".into(),
        ));
    }
    Ok(())
}
