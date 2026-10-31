//! The six file actions and their per-platform titles.
//!
//! Source of truth: `docs/TOOLS_AND_ACTIONS.md` → "File actions". Titles live
//! here, keyed by [`Platform`], so core needs no `cfg`.

use serde::{Deserialize, Serialize};

/// The platform a title is shown on. The app passes its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    /// Windows.
    Windows,
    /// macOS.
    MacOs,
}

/// A file action, in doc order. Serialized as the doc key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionKey {
    /// `share`: AirDrop on macOS, the share sheet on Windows.
    Share,
    /// `float`: the floating shelf.
    Float,
    /// `copy`: files to the clipboard.
    Copy,
    /// `move`: the only action allowed to move a source.
    Move,
    /// `copyPath`: absolute paths, one per line.
    CopyPath,
    /// `terminal`: open the folder in a terminal.
    Terminal,
}

impl ActionKey {
    /// All six actions, in doc order.
    pub const ALL: [ActionKey; 6] = [
        ActionKey::Share,
        ActionKey::Float,
        ActionKey::Copy,
        ActionKey::Move,
        ActionKey::CopyPath,
        ActionKey::Terminal,
    ];

    /// The doc key, e.g. `"copyPath"`.
    pub const fn key(self) -> &'static str {
        match self {
            ActionKey::Share => "share",
            ActionKey::Float => "float",
            ActionKey::Copy => "copy",
            ActionKey::Move => "move",
            ActionKey::CopyPath => "copyPath",
            ActionKey::Terminal => "terminal",
        }
    }

    /// The wheel title on `platform`. Share is "AIRDROP" on macOS and
    /// "SHARE" on Windows; the others are the same on both.
    pub const fn title(self, platform: Platform) -> &'static str {
        match (self, platform) {
            (ActionKey::Share, Platform::MacOs) => "AIRDROP",
            (ActionKey::Share, Platform::Windows) => "SHARE",
            (ActionKey::Float, _) => "FLOAT",
            (ActionKey::Copy, _) => "COPY",
            (ActionKey::Move, _) => "MOVE",
            (ActionKey::CopyPath, _) => "COPY PATH",
            (ActionKey::Terminal, _) => "TERMINAL",
        }
    }
}

/// The six file actions, in wheel order. Actions work on any selection.
pub fn actions() -> &'static [ActionKey; 6] {
    &ActionKey::ALL
}
