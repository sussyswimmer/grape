//! Grape's pure model: formats, the conversion matrix, tools per format,
//! output naming, wheel geometry and hit testing, and job planning.
//!
//! No I/O, no OS calls, no Tauri. See `CLAUDE.md` in this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod format;
pub mod matrix;
pub mod tier;

pub use format::{Category, Format, INPUT_ONLY};
pub use matrix::{pair_tier, pairs, targets, targets_enabled};
pub use tier::{Features, Tier};
