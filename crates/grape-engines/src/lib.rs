//! Executes `grape_core` job plans: images, the FFmpeg sidecar, PDF,
//! archives and subtitles (v2: documents).
//!
//! This is a skeleton. Lane B builds the engines in milestone M3; the rules
//! (never touch the source, no network, sidecars via `SidecarPaths`,
//! cancellation and progress) are in `CLAUDE.md` in this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
