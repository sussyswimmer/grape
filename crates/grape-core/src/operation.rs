//! What the user asked for when they dropped on a segment.

use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};

use crate::format::Format;
use crate::tools::ToolKey;

/// A conversion or a tool run. File actions are not operations: they are
/// executed by the platform layer, not planned.
///
/// Serialized as `{"kind":"convert","to":"png"}` or
/// `{"kind":"tool","tool":"compress","params":{…}}` (`params` may be
/// omitted and then takes its defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Operation {
    /// Convert each file to `to`.
    Convert {
        /// The target format.
        to: Format,
    },
    /// Run `tool` with `params`.
    Tool {
        /// The tool.
        tool: ToolKey,
        /// Panel choices. Each tool reads only the fields it needs.
        #[serde(default)]
        params: ToolParams,
    },
}

impl Operation {
    /// A conversion to `to`.
    pub const fn convert(to: Format) -> Operation {
        Operation::Convert { to }
    }

    /// A tool run with default parameters.
    pub const fn tool(tool: ToolKey) -> Operation {
        Operation::Tool {
            tool,
            params: ToolParams::DEFAULT,
        }
    }
}

/// The options a tool's panel can set. A struct with defaults rather than a
/// per-tool enum, so there are no invalid tool/params combinations; each tool
/// reads only its own field (`TOOLS_AND_ACTIONS.md` → v1 panels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ToolParams {
    /// Compress quality preset. Default: Balanced.
    pub compress: CompressPreset,
    /// Audio Channels target. Default: Mono.
    pub channels: Channels,
    /// Change Speed factor. Default: 2x.
    pub speed: Speed,
    /// Normalize target loudness in LUFS. Default: −16.
    pub target_lufs: i8,
}

impl ToolParams {
    /// The defaults: Balanced, Mono, 2x, −16 LUFS.
    pub const DEFAULT: ToolParams = ToolParams {
        compress: CompressPreset::Balanced,
        channels: Channels::Mono,
        speed: Speed::DEFAULT,
        target_lufs: -16,
    };

    /// The quietest Normalize target, in LUFS.
    pub const LUFS_MIN: i8 = -24;
    /// The loudest Normalize target, in LUFS.
    pub const LUFS_MAX: i8 = -10;
    /// The Normalize panel's step, in LU.
    pub const LUFS_STEP: u8 = 1;

    /// True when `lufs` is a Normalize target Grape runs: in
    /// [`ToolParams::LUFS_MIN`]..=[`ToolParams::LUFS_MAX`] and on the
    /// [`ToolParams::LUFS_STEP`] grid from the minimum
    /// (`TOOLS_AND_ACTIONS.md` → "[Grape] v1 panels"). The engine checks it
    /// again before it runs.
    pub const fn lufs_ok(lufs: i8) -> bool {
        lufs >= Self::LUFS_MIN
            && lufs <= Self::LUFS_MAX
            && lufs
                .abs_diff(Self::LUFS_MIN)
                .is_multiple_of(Self::LUFS_STEP)
    }
}

impl Default for ToolParams {
    fn default() -> Self {
        ToolParams::DEFAULT
    }
}

/// Compress presets: Small / Balanced / High.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompressPreset {
    /// Smallest file, lowest quality.
    Small,
    /// The default trade-off.
    #[default]
    Balanced,
    /// Highest quality, largest file.
    High,
}

/// Audio Channels target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Channels {
    /// One channel.
    #[default]
    Mono,
    /// Two channels.
    Stereo,
}

impl Channels {
    /// The naming suffix: "Mono" or "Stereo".
    pub const fn label(self) -> &'static str {
        match self {
            Channels::Mono => "Mono",
            Channels::Stereo => "Stereo",
        }
    }
}

/// A playback speed factor, stored as whole hundredths so it is exact,
/// hashable and formats predictably (`150` is 1.5x). Never zero.
/// Serialized as the hundredths integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Speed(NonZeroU16);

impl Speed {
    /// 2x, the default.
    pub const DEFAULT: Speed = match NonZeroU16::new(200) {
        Some(h) => Speed(h),
        None => Speed(NonZeroU16::MIN),
    };

    /// A speed of `hundredths / 100`. `None` for zero.
    pub const fn from_hundredths(hundredths: u16) -> Option<Speed> {
        match NonZeroU16::new(hundredths) {
            Some(h) => Some(Speed(h)),
            None => None,
        }
    }

    /// The speed in hundredths (`150` for 1.5x).
    pub const fn hundredths(self) -> u16 {
        self.0.get()
    }

    /// The number used in `Speed {N}x`: up to two decimals with no
    /// trailing zeros (`"2"`, `"1.5"`, `"0.25"`).
    pub fn label(self) -> String {
        let h = self.hundredths();
        let (whole, frac) = (h / 100, h % 100);
        if frac == 0 {
            format!("{whole}")
        } else if frac % 10 == 0 {
            format!("{whole}.{}", frac / 10)
        } else {
            format!("{whole}.{frac:02}")
        }
    }
}

impl Default for Speed {
    fn default() -> Self {
        Speed::DEFAULT
    }
}
