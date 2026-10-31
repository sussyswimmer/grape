//! ISOBMFF, for the HEIC tools (D28).
//!
//! - [`boxes`]: a file's top-level boxes, by seeking, and the boxes in a
//!   box held in memory.
//! - [`items`]: the one file-level `meta` and its items: where their data
//!   is, how they refer to each other, their properties.

pub(crate) mod boxes;
pub(crate) mod items;

#[cfg(test)]
pub(crate) mod tests;
