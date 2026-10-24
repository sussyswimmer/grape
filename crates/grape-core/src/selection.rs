//! What is being dragged, and what each wheel shows for it.
//!
//! Sources of truth: `docs/INTERACTION_SPEC.md` §5 (empty states) and §7
//! (multiple files), `docs/FORMAT_MATRIX.md` and `docs/TOOLS_AND_ACTIONS.md`.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::actions::{actions, ActionKey, Platform};
use crate::format::Format;
use crate::matrix::targets_enabled;
use crate::operation::{Operation, ToolParams};
use crate::tier::Features;
use crate::tools::{tools_enabled, ToolKey};

/// Whether a selected item is a file (with its format, if known) or a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    /// A file. `format` is `None` when the extension is unknown.
    File {
        /// The detected format.
        format: Option<Format>,
    },
    /// A folder.
    Folder,
}

/// One dragged item.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SelectionItem {
    /// The item's absolute path.
    pub path: PathBuf,
    /// File or folder.
    pub kind: ItemKind,
}

impl SelectionItem {
    /// A file, with its format detected from the file name
    /// ([`Format::from_file_name`]).
    pub fn file(path: impl Into<PathBuf>) -> SelectionItem {
        let path = path.into();
        let format = path
            .file_name()
            .and_then(|name| Format::from_file_name(&name.to_string_lossy()));
        SelectionItem {
            path,
            kind: ItemKind::File { format },
        }
    }

    /// A file whose format the caller already knows (e.g. sniffed from magic
    /// bytes when the extension is missing or lies).
    pub fn file_with_format(path: impl Into<PathBuf>, format: Option<Format>) -> SelectionItem {
        SelectionItem {
            path: path.into(),
            kind: ItemKind::File { format },
        }
    }

    /// A folder.
    pub fn folder(path: impl Into<PathBuf>) -> SelectionItem {
        SelectionItem {
            path: path.into(),
            kind: ItemKind::Folder,
        }
    }

    /// The file's format; `None` for folders and unknown files.
    pub fn format(&self) -> Option<Format> {
        match self.kind {
            ItemKind::File { format } => format,
            ItemKind::Folder => None,
        }
    }
}

/// What the app knows about one directory when planning: the names in it
/// and whether Grape may write there. Core never reads the disk itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DirSnapshot {
    /// Every file and folder name in the directory.
    pub names: HashSet<String>,
    /// Whether outputs may be written there.
    pub writable: bool,
}

impl DirSnapshot {
    /// A snapshot from a list of names.
    pub fn new<I, S>(names: I, writable: bool) -> DirSnapshot
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        DirSnapshot {
            names: names.into_iter().map(Into::into).collect(),
            writable,
        }
    }
}

/// The dragged items plus a snapshot of every directory a plan may write
/// to: each source's parent, the configured output folder and the fallback
/// folder. Keys must be the same `PathBuf`s the planner derives (each
/// item's `path.parent()`, `Settings` paths).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Selection {
    /// The items, in drag order.
    pub items: Vec<SelectionItem>,
    /// Directory snapshots, keyed by path.
    pub dirs: BTreeMap<PathBuf, DirSnapshot>,
}

impl Selection {
    /// A selection with no directory snapshots yet.
    pub fn new(items: Vec<SelectionItem>) -> Selection {
        Selection {
            items,
            dirs: BTreeMap::new(),
        }
    }

    /// Adds (or replaces) the snapshot for `dir`.
    pub fn with_dir(mut self, dir: impl Into<PathBuf>, snapshot: DirSnapshot) -> Selection {
        self.dirs.insert(dir.into(), snapshot);
        self
    }
}

/// Which wheel is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WheelMode {
    /// The conversions wheel (Shift by default).
    Conversions,
    /// The tools wheel (Ctrl on Windows, Command on macOS).
    Tools,
    /// The file actions wheel (Alt on Windows, Control on macOS).
    Actions,
}

/// One wheel segment's item. Serialized as
/// `{"kind":"convert","key":"png"}`, `{"kind":"tool","key":"mergePDF"}` or
/// `{"kind":"action","key":"share"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "key", rename_all = "camelCase")]
pub enum WheelEntry {
    /// Convert to this format.
    Convert(Format),
    /// Run this tool.
    Tool(ToolKey),
    /// Perform this file action.
    Action(ActionKey),
}

impl WheelEntry {
    /// The segment title on `platform`.
    pub const fn title(self, platform: Platform) -> &'static str {
        match self {
            WheelEntry::Convert(f) => f.display_name(),
            WheelEntry::Tool(t) => t.title(),
            WheelEntry::Action(a) => a.title(platform),
        }
    }

    /// The operation to plan when dropped here, with `params` for tools.
    /// `None` for file actions, which are not planned.
    pub const fn operation(self, params: ToolParams) -> Option<Operation> {
        match self {
            WheelEntry::Convert(to) => Some(Operation::Convert { to }),
            WheelEntry::Tool(tool) => Some(Operation::Tool { tool, params }),
            WheelEntry::Action(_) => None,
        }
    }
}

/// Which empty-state message to show (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EmptyState {
    /// "No conversions for this format …", naming the actions key.
    NoConversions,
    /// "No editing tools for this format …", naming the conversions and
    /// actions keys.
    NoTools,
}

/// What a wheel shows: segments, or an empty-state message. Serialized as
/// `{"kind":"entries","value":[…]}` or `{"kind":"empty","value":"noTools"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum WheelContent {
    /// At least one segment, in wheel order.
    Entries(Vec<WheelEntry>),
    /// No segments; show this message instead.
    Empty(EmptyState),
}

impl WheelContent {
    /// The segments; empty for an empty state.
    pub fn entries(&self) -> &[WheelEntry] {
        match self {
            WheelContent::Entries(entries) => entries,
            WheelContent::Empty(_) => &[],
        }
    }

    /// True when `entry` is one of the segments.
    pub fn contains(&self, entry: WheelEntry) -> bool {
        self.entries().contains(&entry)
    }
}

/// What the `mode` wheel shows for `items` under `features`.
///
/// - **Actions** always has all six (they work on any selection).
/// - **Conversions** is the intersection of each distinct format's enabled
///   targets, in the first item's order (§7).
/// - **Tools** is the intersection of each distinct format's enabled tools
///   for this selection size, in the first item's order. A tool must be
///   enabled for every format, so in v1 jpg + pdf drops compress
///   (compress on pdf is v2).
/// - A folder, a file of unknown format or an empty selection makes
///   Conversions and Tools empty, as does an empty intersection.
pub fn wheel_content(items: &[SelectionItem], mode: WheelMode, features: Features) -> WheelContent {
    match mode {
        WheelMode::Actions => {
            WheelContent::Entries(actions().iter().map(|&a| WheelEntry::Action(a)).collect())
        }
        WheelMode::Conversions => {
            let entries = distinct_formats(items)
                .map(|formats| intersect(formats.iter().map(|&f| targets_enabled(f, features))))
                .unwrap_or_default();
            non_empty(
                entries.into_iter().map(WheelEntry::Convert).collect(),
                EmptyState::NoConversions,
            )
        }
        WheelMode::Tools => {
            let n = items.len();
            let entries = distinct_formats(items)
                .map(|formats| intersect(formats.iter().map(|&f| tools_enabled(f, n, features))))
                .unwrap_or_default();
            non_empty(
                entries.into_iter().map(WheelEntry::Tool).collect(),
                EmptyState::NoTools,
            )
        }
    }
}

/// The distinct formats in first-seen order, or `None` if the selection is
/// empty or holds a folder or an unknown file.
fn distinct_formats(items: &[SelectionItem]) -> Option<Vec<Format>> {
    if items.is_empty() {
        return None;
    }
    let mut formats = Vec::new();
    for item in items {
        let format = item.format()?;
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    Some(formats)
}

/// The items of the first list that are in every other list, in the first
/// list's order.
fn intersect<T: PartialEq>(mut lists: impl Iterator<Item = Vec<T>>) -> Vec<T> {
    let Some(mut acc) = lists.next() else {
        return Vec::new();
    };
    for list in lists {
        acc.retain(|item| list.contains(item));
    }
    acc
}

fn non_empty(entries: Vec<WheelEntry>, empty: EmptyState) -> WheelContent {
    if entries.is_empty() {
        WheelContent::Empty(empty)
    } else {
        WheelContent::Entries(entries)
    }
}
