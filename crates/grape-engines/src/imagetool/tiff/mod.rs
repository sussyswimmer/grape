//! TIFF for the image tools (D28).
//!
//! - [`read`]: a TIFF's model, every page and SubIFD, read by seeking.
//! - [`write`]: a TIFF written back compact from the IFDs a tool chose.

pub(crate) mod read;
pub(crate) mod write;

#[cfg(test)]
pub(crate) mod tests;
