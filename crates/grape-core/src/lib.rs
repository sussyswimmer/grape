//! Grape's pure model: formats, the conversion matrix, tools per format,
//! output naming, wheel geometry and hit testing, and job planning.
//!
//! No I/O, no OS calls, no Tauri. See `CLAUDE.md` in this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod actions;
pub mod format;
pub mod geometry;
pub mod matrix;
pub mod naming;
pub mod operation;
pub mod plan;
pub mod selection;
pub mod tier;
pub mod tools;

pub use actions::{actions, ActionKey, Platform};
pub use format::{Category, Format, INPUT_ONLY};
pub use geometry::{
    clamp_anchor, hit_test, wheel_layout, Geometry, Point, Rect, Segment, GEOMETRY,
};
pub use matrix::{pair_tier, pairs, targets, targets_enabled};
pub use naming::{
    output_kind, output_name, page_file_name, pages_folder_name, produces_pages, split_name,
    tool_naming, tool_suffix, OutputExt, OutputKind, Suffix, ToolNaming,
};
pub use operation::{Channels, CompressPreset, Operation, Speed, ToolParams};
pub use plan::{plan, Job, JobOutput, JobPlan, OutputLocation, PlanError, Settings};
pub use selection::{
    wheel_content, DirSnapshot, EmptyState, ItemKind, Selection, SelectionItem, WheelContent,
    WheelEntry, WheelMode,
};
pub use tier::{Features, Tier};
pub use tools::{
    tool_row, tool_tier, tools, tools_enabled, ToolInfo, ToolKey, ToolRow, TIER_OVERRIDES,
    TOOL_ROWS,
};
