//! TIFF for the image tools (D28).
//!
//! - [`read`]: a TIFF's model, every page and SubIFD, read by seeking.

pub(crate) mod read;

#[cfg(test)]
pub(crate) mod tests;
