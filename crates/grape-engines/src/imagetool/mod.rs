//! Grape's own image tools (D26), in Rust with no sidecar: Remove Metadata
//! on JPEG, PNG, WebP and GIF by stripping the container, which keeps the
//! image data byte for byte, and Compress on JPEG and PNG.
//!
//! - [`exif`]: what an EXIF block holds, and the minimal EXIF written back.

// The strippers that use it come next.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod exif;
