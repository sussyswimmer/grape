//! TIFF for the image tools (D28).
//!
//! - [`read`]: a TIFF's model, every page and SubIFD, read by seeking.
//! - [`write`]: a TIFF written back compact from the IFDs a tool chose.
//! - [`strip`]: Remove Metadata on TIFF, with [`irb`] for Photoshop's
//!   resources.

pub(crate) mod irb;
pub(crate) mod read;
pub(crate) mod strip;
pub(crate) mod write;

#[cfg(test)]
pub(crate) mod tests;

/// A TIFF starts with its byte order, then 42, or 43 for a BigTIFF.
pub(super) fn sniff(head: &[u8]) -> bool {
    matches!(
        head.get(..4),
        Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+")
    )
}
