//! Roadmap tiers and the feature switches that gate them.
//!
//! The full matrix and tool tables stay in the data; callers filter at the
//! edge with a [`Features`] value (`grape-core/CLAUDE.md` → feature gating).

use serde::{Deserialize, Serialize};

/// The roadmap tier of a conversion pair or tool (`FORMAT_MATRIX.md` and
/// `TOOLS_AND_ACTIONS.md` → Tier column). This is Grape's choice, not
/// observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tier {
    /// Ships in v1.
    V1,
    /// Ships in v2 (documents, spreadsheets, slides, editor tools).
    V2,
    /// Later than v2 (Apple Pages, see DECISIONS O2).
    Later,
}

impl Tier {
    /// True when items of this tier are visible under `features`.
    /// V1 is always enabled; V2 and Later each have their own switch.
    pub const fn is_enabled(self, features: Features) -> bool {
        match self {
            Tier::V1 => true,
            Tier::V2 => features.v2,
            Tier::Later => features.later,
        }
    }
}

/// Which tiers beyond v1 are switched on. Passed to every `*_enabled` lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Features {
    /// Show v2 items (`features.v2` in Settings).
    pub v2: bool,
    /// Show later-tier items.
    pub later: bool,
}

impl Features {
    /// Only v1 items: what ships first. This is the default.
    pub const V1_ONLY: Features = Features {
        v2: false,
        later: false,
    };

    /// Every tier: full parity with the observed matrix (275 pairs).
    pub const FULL: Features = Features {
        v2: true,
        later: true,
    };
}

impl Default for Features {
    fn default() -> Self {
        Features::V1_ONLY
    }
}
