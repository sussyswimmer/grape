//! Grape's own image tools (D26, D28), in Rust with no sidecar: Remove
//! Metadata on JPEG, PNG, WebP, GIF, TIFF and HEIC by stripping the
//! container, which keeps the image data byte for byte, and Compress on
//! JPEG, PNG, TIFF and WebP.
//!
//! - [`compress`] and [`png_compress`]: Compress on JPEG and PNG; on TIFF,
//!   [`tiff`]'s, which deflates its strips and tiles again; on WebP,
//!   [`webp_compress`], which encodes it again with libwebp.
//! - [`scale`]: scaling a picture down for Compress, and what that holds.
//! - [`strip`]: Remove Metadata's runner, and each container's stripper
//!   ([`jpeg`], [`png`], [`webp`], [`gif`], [`tiff`]'s rewrite, and
//!   [`heic`]'s `meta` rebuilt in place).
//! - [`exif`]: what an EXIF block holds, and the minimal EXIF written back.
//! - [`isobmff`]: a HEIF file's boxes and items, for the HEIC tools.
//!
//! The content decides which code runs, not the name ([`Kind`]): a PNG
//! named `.jpg` is stripped and compressed as a PNG, and keeps its name.

mod compress;
pub(crate) mod exif;
mod gif;
mod heic;
mod isobmff;
mod jpeg;
mod png;
mod png_compress;
mod scale;
mod strip;
mod tiff;
mod webp;
mod webp_compress;

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use grape_core::{Format, Job, ToolKey, ToolParams};
use grape_heif_proto::{Sniff, SNIFF_BYTES};

use crate::cancel::Cancellable;
use crate::publish::Names;
use crate::{CancelToken, EngineCtx, GrapeError, JobOutcome};

pub(crate) use webp::Reading as WebpReading;

/// True when this engine runs `tool` on a file named as `from`. `_heif`
/// says whether this install has the HEIC helper (D33), as
/// `archive::extracts` is told about bsdtar: no tool here needs it yet;
/// Compress on HEIC will (D28).
pub(crate) fn runs_tool(tool: ToolKey, from: Format, _heif: bool) -> bool {
    match tool {
        ToolKey::RemoveMetadata => strip::strips(from),
        // By name; the content decides which compressor runs ([`run`]).
        ToolKey::Compress => matches!(
            from,
            Format::Jpg | Format::Png | Format::Tiff | Format::Webp
        ),
        _ => false,
    }
}

/// Runs `tool` on the image `src` (named as `from`) into `dest`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    tool: ToolKey,
    params: &ToolParams,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    match tool {
        ToolKey::RemoveMetadata => strip::run(job, src, dest, from, ctx, progress),
        ToolKey::Compress => match sniff(src) {
            Some(Kind::Png) => png_compress::run(job, src, dest, from, ctx, progress),
            Some(Kind::Tiff) => tiff::compress::run(job, src, dest, from, ctx, progress),
            Some(Kind::Webp) => {
                webp_compress::run(job, src, dest, from, params.compress, ctx, progress)
            }
            // JPEG, or "isn't a readable …".
            _ => compress::run(job, src, dest, from, params.compress, ctx, progress),
        },
        _ => Err(crate::not_yet_supported(job, Some(from))),
    }
}

/// How a decode must read the WebP `file` (`src`, named in `names`) to
/// show what libwebp shows, as Compress reads it too ([`webp::reading`]),
/// or why it can't: "… isn't a readable WEBP image." for what Compress's
/// scan refuses as broken. Reads chunk headers only; the file is left
/// wherever the walk stops.
pub(crate) fn webp_reading(
    file: &File,
    src: &Path,
    names: &Names,
    cancel: &CancelToken,
) -> Result<WebpReading, GrapeError> {
    webp::reading(&mut BufReader::new(Cancellable::new(file, cancel))).map_err(|e| match e {
        strip::Error::Broken(why) | strip::Error::Refused(why) => {
            names.decode_failed(format!("decode {}: {why}", src.display()))
        }
        strip::Error::TooLarge(why) => names.too_large(format!("decode {}: {why}", src.display())),
        strip::Error::Read(e) | strip::Error::Write(e) => {
            names.read_failed(format!("read {}: {e}", src.display()))
        }
    })
}

/// The image containers the tools read, by a file's first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Jpeg,
    Png,
    Webp,
    Gif,
    Tiff,
    /// HEIF by its `ftyp` (D33, D41's sniff): HEIC, AVIF, or HEIF with
    /// another codec.
    Heif(Sniff),
}

impl Kind {
    /// The container `head` (a file's first bytes) starts.
    fn of(head: &[u8]) -> Option<Kind> {
        if jpeg::sniff(head) {
            Some(Kind::Jpeg)
        } else if png::sniff(head) {
            Some(Kind::Png)
        } else if webp::sniff(head) {
            Some(Kind::Webp)
        } else if gif::sniff(head) {
            Some(Kind::Gif)
        } else if tiff::sniff(head) {
            Some(Kind::Tiff)
        } else {
            match grape_heif_proto::sniff(head.get(..SNIFF_BYTES).unwrap_or(head)) {
                Sniff::NotHeif => None,
                heif => Some(Kind::Heif(heif)),
            }
        }
    }
}

/// The container `src` starts, from its first [`SNIFF_BYTES`] bytes (what
/// HEIF's sniff needs too); `None` for a file that can't be read, or that
/// no tool here reads.
fn sniff(src: &Path) -> Option<Kind> {
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    File::open(src)
        .and_then(|f| f.take(SNIFF_BYTES as u64).read_to_end(&mut head))
        .ok()?;
    Kind::of(&head)
}
