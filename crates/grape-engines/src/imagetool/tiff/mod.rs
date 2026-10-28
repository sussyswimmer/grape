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
