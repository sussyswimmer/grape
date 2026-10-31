//! ISOBMFF, for the HEIC tools (D28).
//!
//! - [`boxes`]: a file's top-level boxes, by seeking, and the boxes in a
//!   box held in memory.
//! - [`items`]: the one file-level `meta` and its items: where their data
//!   is, how they refer to each other, their properties.
//! - [`scan`]: what Compress on HEIC reads of a file before its helper
//!   starts, and what it refuses then.

pub(crate) mod boxes;
pub(crate) mod items;
pub(crate) mod scan;

#[cfg(test)]
pub(crate) mod tests;
