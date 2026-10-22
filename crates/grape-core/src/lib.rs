//! Grape's pure model: formats, the conversion matrix, tools per format,
//! output naming, wheel geometry and hit testing, and job planning.
//!
//! No I/O, no OS calls, no Tauri. See `CLAUDE.md` in this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod actions;
pub mod format;
pub mod matrix;
pub mod tier;
pub mod tools;

pub use actions::{actions, ActionKey, Platform};
pub use format::{Category, Format, INPUT_ONLY};
pub use matrix::{pair_tier, pairs, targets, targets_enabled};
pub use tier::{Features, Tier};
pub use tools::{
    tool_row, tool_tier, tools, tools_enabled, ToolInfo, ToolKey, ToolRow, TIER_OVERRIDES,
    TOOL_ROWS,
};
