//! Grape's own image tools (D26, D28), in Rust with no sidecar: Remove
//! Metadata on JPEG, PNG, WebP, GIF and TIFF by stripping the container,
//! which keeps the image data byte for byte, and Compress on JPEG and PNG.
//!
//! - [`compress`] and [`png_compress`]: Compress on JPEG and PNG.
//! - [`scale`]: scaling a picture down for Compress, and what that holds.
//! - [`strip`]: Remove Metadata's runner, and each container's stripper
//!   ([`jpeg`], [`png`], [`webp`], [`gif`], and [`tiff`]'s rewrite).
//! - [`exif`]: what an EXIF block holds, and the minimal EXIF written back.
//! - [`isobmff`]: a HEIF file's boxes and items, for the HEIC tools (D28).
//!
//! The content decides which code runs, not the name ([`Kind`]): a PNG
//! named `.jpg` is stripped and compressed as a PNG, and keeps its name.

mod compress;
pub(crate) mod exif;
mod gif;
// The HEIC tools that use it come next.
#[cfg_attr(not(test), allow(dead_code))]
mod isobmff;
mod jpeg;
mod png;
mod png_compress;
mod scale;
mod strip;
mod tiff;
mod webp;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use grape_core::{Format, Job, ToolKey, ToolParams};
use grape_heif_proto::SNIFF_BYTES;

use crate::{EngineCtx, GrapeError, JobOutcome};

/// True when this engine runs `tool` on a file named as `from`. `_heif`
/// says whether this install has the HEIC helper (D33), as
/// `archive::extracts` is told about bsdtar: no tool here needs it yet;
/// Compress on HEIC will (D28).
pub(crate) fn runs_tool(tool: ToolKey, from: Format, _heif: bool) -> bool {
    match tool {
        ToolKey::RemoveMetadata => strip::strips(from),
        ToolKey::Compress => compress::compresses(from),
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
            // JPEG, or "isn't a readable …".
            _ => compress::run(job, src, dest, from, params.compress, ctx, progress),
        },
        _ => Err(crate::not_yet_supported(job, Some(from))),
    }
}

/// The image containers the tools read, by a file's first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Jpeg,
    Png,
    Webp,
    Gif,
    Tiff,
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
            None
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
