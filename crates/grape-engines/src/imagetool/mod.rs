//! Grape's own image tools (D26), in Rust with no sidecar: Remove Metadata
//! on JPEG, PNG, WebP and GIF by stripping the container, which keeps the
//! image data byte for byte, and Compress on JPEG and PNG.
//!
//! - [`compress`] and [`png_compress`]: Compress on JPEG and PNG.
//! - [`strip`]: Remove Metadata's runner, and each container's stripper
//!   ([`jpeg`], [`png`], [`webp`], [`gif`]).
//! - [`exif`]: what an EXIF block holds, and the minimal EXIF written back.

mod compress;
pub(crate) mod exif;
mod gif;
mod jpeg;
mod png;
mod png_compress;
mod strip;
mod webp;

use std::path::Path;

use grape_core::{Format, Job, ToolKey, ToolParams};

use crate::{EngineCtx, GrapeError, JobOutcome};

/// True when this engine runs `tool` on a file named as `from`.
pub(crate) fn runs_tool(tool: ToolKey, from: Format) -> bool {
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
        // The content decides: a PNG named .jpg is compressed as a PNG.
        ToolKey::Compress if is_png(src) => png_compress::run(job, src, dest, from, ctx, progress),
        ToolKey::Compress => compress::run(job, src, dest, from, params.compress, ctx, progress),
        _ => Err(crate::not_yet_supported(job, Some(from))),
    }
}

/// Whether `src` starts as a PNG does (an unreadable file isn't: the JPEG
/// path then says it can't be read).
fn is_png(src: &Path) -> bool {
    use std::io::Read;
    let mut head = [0; 8];
    std::fs::File::open(src)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| &head == b"\x89PNG\r\n\x1a\n")
}
