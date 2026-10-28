//! Grape's own image tools (D26), in Rust with no sidecar: Remove Metadata
//! on JPEG, PNG, WebP and GIF by stripping the container, which keeps the
//! image data byte for byte, and Compress on JPEG and PNG.
//!
//! - [`strip`]: Remove Metadata's runner, and each container's stripper
//!   ([`jpeg`], [`png`], [`webp`], [`gif`]).
//! - [`exif`]: what an EXIF block holds, and the minimal EXIF written back.

pub(crate) mod exif;
mod gif;
mod jpeg;
mod png;
mod strip;
mod webp;

use std::path::Path;

use grape_core::{Format, Job, ToolKey};

use crate::{EngineCtx, GrapeError, JobOutcome};

/// True when this engine runs `tool` on a file named as `from`.
pub(crate) fn runs_tool(tool: ToolKey, from: Format) -> bool {
    match tool {
        ToolKey::RemoveMetadata => strip::strips(from),
        _ => false,
    }
}

/// Runs `tool` on the image `src` (named as `from`) into `dest`.
pub(crate) fn run(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    tool: ToolKey,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    match tool {
        ToolKey::RemoveMetadata => strip::run(job, src, dest, from, ctx, progress),
        _ => Err(crate::not_yet_supported(job, Some(from))),
    }
}
