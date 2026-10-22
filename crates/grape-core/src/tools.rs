//! The 26 tools, their flags and tiers, and which tools each format gets.
//!
//! Sources of truth:
//! - `docs/TOOLS_AND_ACTIONS.md` → "Tools": one [`ToolKey::info`] match arm
//!   per table row.
//! - `docs/FORMAT_MATRIX.md` → "Tools per input": [`TOOL_ROWS`], one entry per
//!   table row ("everything else" is the absence of a row).

use serde::{Deserialize, Serialize};

use crate::format::Format;
use crate::tier::{Features, Tier};

/// A tool key, in `TOOLS_AND_ACTIONS.md` table order. Serialized as the doc
/// key (`"compress"`, `"splitPDF"`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolKey {
    /// `compress`
    Compress,
    /// `removeMetadata`
    RemoveMetadata,
    /// `editImage`
    EditImage,
    /// `annotateImage`
    AnnotateImage,
    /// `frameImage`
    FrameImage,
    /// `cropImage`
    CropImage,
    /// `redactImage`
    RedactImage,
    /// `normalizeAudio`
    NormalizeAudio,
    /// `trimAudio`
    TrimAudio,
    /// `audioChannels`
    AudioChannels,
    /// `audioToVideo`
    AudioToVideo,
    /// `redactAudio`
    RedactAudio,
    /// `muteVideo`
    MuteVideo,
    /// `trimVideo`
    TrimVideo,
    /// `cropVideo`
    CropVideo,
    /// `changeVideoSpeed`
    ChangeVideoSpeed,
    /// `videoSnapshots`
    VideoSnapshots,
    /// `splitVideo`
    SplitVideo,
    /// `joinVideos`
    JoinVideos,
    /// `redactVideo`
    RedactVideo,
    /// `splitPDF`
    #[serde(rename = "splitPDF")]
    SplitPdf,
    /// `organizePDF`
    #[serde(rename = "organizePDF")]
    OrganizePdf,
    /// `mergePDF`
    #[serde(rename = "mergePDF")]
    MergePdf,
    /// `createPDF`
    #[serde(rename = "createPDF")]
    CreatePdf,
    /// `createCollage`
    CreateCollage,
    /// `extractArchive`
    ExtractArchive,
}

/// One row of the `TOOLS_AND_ACTIONS.md` "Tools" table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    /// The doc key, e.g. `"splitPDF"`.
    pub key: &'static str,
    /// The wheel title, e.g. `"MERGE PDF"`.
    pub title: &'static str,
    /// The human name, e.g. `"Merge PDFs"`.
    pub name: &'static str,
    /// Opens an options/editor panel before running.
    pub panel: bool,
    /// Turns many inputs into one output.
    pub combine: bool,
    /// Allowed when more than one file is selected.
    pub multi_ok: bool,
    /// Base tier. Per-format exceptions are in [`tool_tier`].
    pub tier: Tier,
}

/// Shorthand for one table row in [`ToolKey::info`].
const fn row(
    key: &'static str,
    title: &'static str,
    name: &'static str,
    panel: bool,
    combine: bool,
    multi_ok: bool,
    tier: Tier,
) -> ToolInfo {
    ToolInfo {
        key,
        title,
        name,
        panel,
        combine,
        multi_ok,
        tier,
    }
}

const Y: bool = true;
const N: bool = false;

impl ToolKey {
    /// All 26 tools, in doc order.
    pub const ALL: [ToolKey; 26] = [
        ToolKey::Compress,
        ToolKey::RemoveMetadata,
        ToolKey::EditImage,
        ToolKey::AnnotateImage,
        ToolKey::FrameImage,
        ToolKey::CropImage,
        ToolKey::RedactImage,
        ToolKey::NormalizeAudio,
        ToolKey::TrimAudio,
        ToolKey::AudioChannels,
        ToolKey::AudioToVideo,
        ToolKey::RedactAudio,
        ToolKey::MuteVideo,
        ToolKey::TrimVideo,
        ToolKey::CropVideo,
        ToolKey::ChangeVideoSpeed,
        ToolKey::VideoSnapshots,
        ToolKey::SplitVideo,
        ToolKey::JoinVideos,
        ToolKey::RedactVideo,
        ToolKey::SplitPdf,
        ToolKey::OrganizePdf,
        ToolKey::MergePdf,
        ToolKey::CreatePdf,
        ToolKey::CreateCollage,
        ToolKey::ExtractArchive,
    ];

    /// The tool's table row. Columns: key, wheel title, name, panel,
    /// combine, multi-OK, base tier. Compress is v2 on pdf: see
    /// [`TIER_OVERRIDES`].
    #[rustfmt::skip]
    pub const fn info(self) -> ToolInfo {
        use Tier::{V1, V2};
        match self {
            ToolKey::Compress =>         row("compress",         "COMPRESS",   "Compress",         Y, N, Y, V1),
            ToolKey::RemoveMetadata =>   row("removeMetadata",   "METADATA",   "Remove Metadata",  Y, N, Y, V1),
            ToolKey::EditImage =>        row("editImage",        "EDIT",       "Edit Photo",       Y, N, N, V2),
            ToolKey::AnnotateImage =>    row("annotateImage",    "ANNOTATE",   "Annotate Photo",   Y, N, N, V2),
            ToolKey::FrameImage =>       row("frameImage",       "ADD BG",     "Add Background",   Y, N, N, V2),
            ToolKey::CropImage =>        row("cropImage",        "CROP",       "Crop Image",       Y, N, Y, V2),
            ToolKey::RedactImage =>      row("redactImage",      "REDACT",     "Redact Photo",     Y, N, N, V2),
            ToolKey::NormalizeAudio =>   row("normalizeAudio",   "NORMALIZE",  "Normalize Volume", Y, N, Y, V1),
            ToolKey::TrimAudio =>        row("trimAudio",        "TRIM",       "Trim Audio",       Y, N, N, V2),
            ToolKey::AudioChannels =>    row("audioChannels",    "CHANNELS",   "Audio Channels",   Y, N, N, V1),
            ToolKey::AudioToVideo =>     row("audioToVideo",     "VISUALIZE",  "Audio Visualizer", Y, N, N, V2),
            ToolKey::RedactAudio =>      row("redactAudio",      "BLEEP",      "Bleep Audio",      Y, N, N, V2),
            ToolKey::MuteVideo =>        row("muteVideo",        "MUTE",       "Remove Audio",     N, N, Y, V1),
            ToolKey::TrimVideo =>        row("trimVideo",        "TRIM",       "Trim Video",       Y, N, N, V2),
            ToolKey::CropVideo =>        row("cropVideo",        "CROP",       "Crop Video",       Y, N, N, V2),
            ToolKey::ChangeVideoSpeed => row("changeVideoSpeed", "SPEED",      "Change Speed",     Y, N, N, V2),
            ToolKey::VideoSnapshots =>   row("videoSnapshots",   "SNAPSHOT",   "Video Snapshots",  Y, N, N, V2),
            ToolKey::SplitVideo =>       row("splitVideo",       "SPLIT",      "Split Video",      Y, N, N, V2),
            ToolKey::JoinVideos =>       row("joinVideos",       "JOIN",       "Join Videos",      Y, Y, Y, V2),
            ToolKey::RedactVideo =>      row("redactVideo",      "REDACT",     "Redact Video",     Y, N, N, V2),
            ToolKey::SplitPdf =>         row("splitPDF",         "SPLIT",      "Split PDF",        N, N, Y, V1),
            ToolKey::OrganizePdf =>      row("organizePDF",      "ORGANIZE",   "Organize PDF",     Y, N, N, V2),
            ToolKey::MergePdf =>         row("mergePDF",         "MERGE PDF",  "Merge PDFs",       Y, Y, Y, V1),
            ToolKey::CreatePdf =>        row("createPDF",        "CREATE PDF", "Create PDF",       N, Y, Y, V1),
            ToolKey::CreateCollage =>    row("createCollage",    "COLLAGE",    "Create Collage",   Y, Y, Y, V2),
            ToolKey::ExtractArchive =>   row("extractArchive",   "EXTRACT",    "Extract Archive",  N, N, Y, V1),
        }
    }

    /// The doc key, e.g. `"mergePDF"`.
    pub const fn key(self) -> &'static str {
        self.info().key
    }

    /// The wheel title, e.g. `"MERGE PDF"`.
    pub const fn title(self) -> &'static str {
        self.info().title
    }

    /// The human name, e.g. `"Merge PDFs"`.
    pub const fn name(self) -> &'static str {
        self.info().name
    }

    /// True when the tool opens a panel before running.
    pub const fn opens_panel(self) -> bool {
        self.info().panel
    }

    /// True when the tool turns many inputs into one output.
    pub const fn is_combine(self) -> bool {
        self.info().combine
    }

    /// True when the tool is in the multi-allowed set.
    pub const fn multi_ok(self) -> bool {
        self.info().multi_ok
    }

    /// The base tier, before per-format overrides.
    pub const fn base_tier(self) -> Tier {
        self.info().tier
    }

    /// Resolve a doc key (`"splitPDF"`) to a tool.
    pub fn from_key(key: &str) -> Option<ToolKey> {
        ToolKey::ALL.iter().copied().find(|t| t.key() == key)
    }
}

/// One row of `FORMAT_MATRIX.md` → "Tools per input".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolRow {
    /// The `Input(s)` cell.
    pub formats: &'static [Format],
    /// The `Single file` cell, in doc order.
    pub single: &'static [ToolKey],
    /// The `Multi-selection` cell, in doc order.
    pub multi: &'static [ToolKey],
}

/// `FORMAT_MATRIX.md` → "Tools per input", row by row. Formats in no row
/// (the doc's "everything else") get no tools.
pub const TOOL_ROWS: [ToolRow; 7] = {
    use Format::*;
    use ToolKey::*;
    [
        ToolRow {
            formats: &[Jpg, Png, Webp, Heic, Tiff],
            single: &[
                Compress,
                RemoveMetadata,
                EditImage,
                AnnotateImage,
                FrameImage,
                CropImage,
                RedactImage,
            ],
            multi: &[
                Compress,
                RemoveMetadata,
                CropImage,
                CreatePdf,
                CreateCollage,
            ],
        },
        ToolRow {
            formats: &[Svg],
            single: &[],
            multi: &[CreatePdf, CreateCollage],
        },
        ToolRow {
            formats: &[Mp3, M4a, Wav, Flac],
            single: &[
                Compress,
                RemoveMetadata,
                NormalizeAudio,
                AudioToVideo,
                TrimAudio,
                AudioChannels,
                RedactAudio,
            ],
            multi: &[Compress, RemoveMetadata, NormalizeAudio],
        },
        ToolRow {
            formats: &[Mp4, Mov, Mkv],
            single: &[
                Compress,
                RemoveMetadata,
                MuteVideo,
                TrimVideo,
                CropVideo,
                ChangeVideoSpeed,
                VideoSnapshots,
                SplitVideo,
                RedactVideo,
            ],
            multi: &[Compress, RemoveMetadata, MuteVideo, JoinVideos],
        },
        ToolRow {
            formats: &[Gif],
            single: &[RemoveMetadata],
            multi: &[RemoveMetadata],
        },
        ToolRow {
            formats: &[Pdf],
            single: &[Compress, RemoveMetadata, SplitPdf, OrganizePdf],
            multi: &[Compress, RemoveMetadata, SplitPdf, MergePdf],
        },
        ToolRow {
            formats: &[Zip, Tar, Gz, Rar],
            single: &[ExtractArchive],
            multi: &[ExtractArchive],
        },
    ]
};

/// Per-format tier exceptions: `(tool, format, tier)`. From the Tier column
/// "v1 (img/audio/video), v2 (pdf)".
pub const TIER_OVERRIDES: [(ToolKey, Format, Tier); 1] =
    [(ToolKey::Compress, Format::Pdf, Tier::V2)];

/// The tools-per-input row for `f`, if it has one.
pub fn tool_row(f: Format) -> Option<&'static ToolRow> {
    TOOL_ROWS.iter().find(|r| r.formats.contains(&f))
}

/// The tools offered for `f` when `selection_len` files are selected,
/// across all tiers, in doc order.
///
/// - `0` gives an empty list.
/// - `1` gives the row's single-file list.
/// - `2+` gives the row's multi list, keeping only multi-allowed tools
///   (`FORMAT_MATRIX.md` → "How multi is computed"). The doc's multi lists
///   are already filtered, so this never removes anything; it keeps the
///   rule explicit.
pub fn tools(f: Format, selection_len: usize) -> Vec<ToolKey> {
    let Some(row) = tool_row(f) else {
        return Vec::new();
    };
    match selection_len {
        0 => Vec::new(),
        1 => row.single.to_vec(),
        _ => row.multi.iter().copied().filter(|t| t.multi_ok()).collect(),
    }
}

/// The tier of `tool` when applied to `f`: the base tier unless
/// [`TIER_OVERRIDES`] says otherwise.
pub fn tool_tier(tool: ToolKey, f: Format) -> Tier {
    TIER_OVERRIDES
        .iter()
        .find(|&&(t, format, _)| t == tool && format == f)
        .map_or(tool.base_tier(), |&(_, _, tier)| tier)
}

/// [`tools`] filtered to the tiers enabled under `features`.
pub fn tools_enabled(f: Format, selection_len: usize, features: Features) -> Vec<ToolKey> {
    tools(f, selection_len)
        .into_iter()
        .filter(|&t| tool_tier(t, f).is_enabled(features))
        .collect()
}
