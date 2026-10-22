//! Grape's pure model: formats, the conversion matrix, tools per format,
//! output naming, wheel geometry and hit testing, and job planning.
//!
//! No I/O, no OS calls, no Tauri. See `CLAUDE.md` in this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod actions;
pub mod format;
pub mod matrix;
pub mod naming;
pub mod operation;
pub mod tier;
pub mod tools;

pub use actions::{actions, ActionKey, Platform};
pub use format::{Category, Format, INPUT_ONLY};
pub use matrix::{pair_tier, pairs, targets, targets_enabled};
pub use naming::{
    output_kind, output_name, page_file_name, pages_folder_name, produces_pages, split_name,
    tool_naming, tool_suffix, OutputExt, OutputKind, Suffix, ToolNaming,
};
pub use operation::{Channels, CompressPreset, Operation, Speed, ToolParams};
pub use tier::{Features, Tier};
pub use tools::{
    tool_row, tool_tier, tools, tools_enabled, ToolInfo, ToolKey, ToolRow, TIER_OVERRIDES,
    TOOL_ROWS,
};
