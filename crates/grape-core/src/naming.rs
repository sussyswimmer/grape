//! Output naming: stems, suffixes, extensions and collision-safe names.
//!
//! Source of truth: `docs/FORMAT_MATRIX.md` → "Output naming". The tools
//! table is [`tool_naming`], one match arm per tool.
//!
//! Names here are file names, never paths. "Does this name exist?" comes in
//! as a `taken` set (names on disk plus names already planned in the batch).

use std::collections::HashSet;

use crate::format::{Category, Format};
use crate::operation::{Operation, ToolParams};
use crate::tools::ToolKey;

/// Split a file name into `(stem, extension)`, without the dot.
///
/// - `.tar.gz` is stripped as one unit, any ASCII case: `a.tar.gz` gives
///   `("a", Some("tar.gz"))`. `.tgz` is a plain extension.
/// - Only the last other extension is stripped: `my.report.pdf` gives
///   `("my.report", Some("pdf"))`.
/// - A leading-dot name (`.env`), a trailing dot (`notes.`) and a name
///   without a dot have no extension: the whole name is the stem.
pub fn split_name(name: &str) -> (&str, Option<&str>) {
    crate::format::split_name(name)
}

/// The stem of a file name: [`split_name`]'s first half.
pub fn stem(name: &str) -> &str {
    split_name(name).0
}

/// The suffix column of the tools naming table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Suffix {
    /// A fixed word, e.g. `"Compressed"`.
    Text(&'static str),
    /// `Speed {N}x`, from [`ToolParams::speed`].
    Speed,
    /// `Mono` or `Stereo`, from [`ToolParams::channels`].
    Channels,
}

/// The "Output ext" column of the tools naming table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputExt {
    /// The source's literal extension (for combine tools, the first file's).
    Same,
    /// The source's literal extension, except a `wav` source writes `flac`.
    SameWavToFlac,
    /// Always this format's extension.
    Fixed(Format),
    /// A folder: no extension.
    Folder,
}

/// One row of the tools naming table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolNaming {
    /// The suffix after the stem.
    pub suffix: Suffix,
    /// The output extension rule.
    pub ext: OutputExt,
}

const fn n(suffix: Suffix, ext: OutputExt) -> ToolNaming {
    ToolNaming { suffix, ext }
}

/// `FORMAT_MATRIX.md` → Output naming → Tools: suffix and extension per tool.
/// Combine tools (createPDF, mergePDF, joinVideos, createCollage) are named
/// from the first file's stem.
#[rustfmt::skip]
pub const fn tool_naming(tool: ToolKey) -> ToolNaming {
    use OutputExt::{Fixed, Folder, Same, SameWavToFlac};
    use Suffix::Text;
    match tool {
        ToolKey::Compress =>         n(Text("Compressed"),  SameWavToFlac),
        ToolKey::RemoveMetadata =>   n(Text("No Metadata"), Same),
        ToolKey::EditImage =>        n(Text("Edited"),      Same),
        ToolKey::AnnotateImage =>    n(Text("Annotated"),   Fixed(Format::Png)),
        ToolKey::FrameImage =>       n(Text("Background"),  Same),
        ToolKey::CropImage =>        n(Text("Cropped"),     Same),
        ToolKey::CropVideo =>        n(Text("Cropped"),     Same),
        ToolKey::RedactImage =>      n(Text("Redacted"),    Same),
        ToolKey::RedactVideo =>      n(Text("Redacted"),    Same),
        ToolKey::RedactAudio =>      n(Text("Bleeped"),     Same),
        ToolKey::NormalizeAudio =>   n(Text("Normalized"),  Same),
        ToolKey::MuteVideo =>        n(Text("Muted"),       Same),
        ToolKey::TrimVideo =>        n(Text("Trimmed"),     Same),
        ToolKey::TrimAudio =>        n(Text("Trimmed"),     Same),
        ToolKey::ChangeVideoSpeed => n(Suffix::Speed,       Same),
        ToolKey::VideoSnapshots =>   n(Text("Snapshot"),    Fixed(Format::Png)),
        ToolKey::SplitVideo =>       n(Text("Clips"),       Folder),
        ToolKey::SplitPdf =>         n(Text("Pages"),       Folder),
        ToolKey::OrganizePdf =>      n(Text("Organized"),   Fixed(Format::Pdf)),
        ToolKey::ExtractArchive =>   n(Text("Extracted"),   Folder),
        ToolKey::AudioToVideo =>     n(Text("Visualizer"),  Fixed(Format::Mp4)),
        ToolKey::AudioChannels =>    n(Suffix::Channels,    Same),
        ToolKey::CreatePdf =>        n(Text("Combined"),    Fixed(Format::Pdf)),
        ToolKey::MergePdf =>         n(Text("Merged"),      Fixed(Format::Pdf)),
        ToolKey::JoinVideos =>       n(Text("Joined"),      Same),
        ToolKey::CreateCollage =>    n(Text("Collage"),     Fixed(Format::Png)),
    }
}

/// The suffix text for `tool` with `params`, e.g. `"Speed 1.5x"`.
pub fn tool_suffix(tool: ToolKey, params: &ToolParams) -> String {
    match tool_naming(tool).suffix {
        Suffix::Text(text) => text.to_string(),
        Suffix::Speed => format!("Speed {}x", params.speed.label()),
        Suffix::Channels => params.channels.label().to_string(),
    }
}

/// Whether an operation writes one file or one folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputKind {
    /// A single file.
    File,
    /// A folder (splitVideo, splitPDF, extractArchive).
    Folder,
}

/// Whether `op` writes a file or a folder. Multi-page conversions (see
/// [`produces_pages`]) are files here; the plan reserves a folder as well.
pub fn output_kind(op: &Operation) -> OutputKind {
    match op {
        Operation::Tool { tool, .. } if tool_naming(*tool).ext == OutputExt::Folder => {
            OutputKind::Folder
        }
        _ => OutputKind::File,
    }
}

/// True when converting `from` to `to` exports pages: any document to `jpg`
/// or `png` (`FORMAT_MATRIX.md`: "Multiple pages go into a new folder").
pub fn produces_pages(from: Format, to: Format) -> bool {
    from.category() == Category::Document && matches!(to, Format::Jpg | Format::Png)
}

/// The output name for `op` on `source` (a file name, not a path), unique
/// against `taken` (case-insensitively).
///
/// - Conversions: `{stem}.{ext}`, where `gz` writes `tar.gz`.
/// - Tools: `{stem} {Suffix}.{ext}`; folders have no extension; "same" keeps
///   the source's literal extension (`Photo.JPG` → `Photo Compressed.JPG`).
/// - Combine tools: pass the first file's name.
/// - Collisions: `{base} 2.{ext}`, `{base} 3.{ext}`, …
///
/// The source's format comes from its extension. The planner passes the
/// detected format instead, so compress's `wav → flac` follows the content
/// when the extension lies.
pub fn output_name(source: &str, op: &Operation, taken: &HashSet<String>) -> String {
    let format = Format::from_file_name(source);
    output_name_folded(source, format, op, &fold_set(taken))
}

/// The folder name for a multi-page export of `source`: a unique `{stem}`.
pub fn pages_folder_name(source: &str, taken: &HashSet<String>) -> String {
    unique_name(stem(source), None, &fold_set(taken))
}

/// The file name of page `page` (1-based) of `page_count` inside a pages
/// folder: `{stem} {n}.{ext}`, with `n` zero-padded to the width of
/// `page_count` (`"report 03.png"` for page 3 of 12).
pub fn page_file_name(stem: &str, page: usize, page_count: usize, ext: &str) -> String {
    let width = digits(page_count.max(page));
    format!("{stem} {page:0width$}.{ext}")
}

fn digits(mut n: usize) -> usize {
    let mut width = 1;
    while n >= 10 {
        n /= 10;
        width += 1;
    }
    width
}

/// The collision key: names that differ only in case are the same name.
///
/// Lowercasing first sends `ẞ` (U+1E9E) to `ß`, which the uppercase step
/// then expands to `SS`, so `STRAẞE`, `straße` and `STRASSE` share a key.
/// Starting with uppercase would leave `ẞ` as `ß` and miss that pair. The
/// key can over-match (`straße` vs `strasse`), which is the safe direction:
/// it can only add a ` 2`, never plan over an existing file.
///
/// Unicode normalization is not folded: an NFC and an NFD spelling of the
/// same name get different keys. Engines must therefore never replace an
/// existing file when they move an output to its final name.
pub(crate) fn fold(name: &str) -> String {
    name.to_lowercase().to_uppercase().to_lowercase()
}

pub(crate) fn fold_set(taken: &HashSet<String>) -> HashSet<String> {
    taken.iter().map(|s| fold(s)).collect()
}

/// [`output_name`] for a source of format `format` (`None` if unknown),
/// against an already folded `taken` set.
pub(crate) fn output_name_folded(
    source: &str,
    format: Option<Format>,
    op: &Operation,
    folded: &HashSet<String>,
) -> String {
    let (base, ext) = output_base(source, format, op);
    unique_name(&base, ext, folded)
}

/// The name before collision handling: `(base, extension)`.
fn output_base<'a>(
    source: &'a str,
    format: Option<Format>,
    op: &Operation,
) -> (String, Option<&'a str>) {
    let (stem, src_ext) = split_name(source);
    match op {
        Operation::Convert { to } => (stem.to_string(), Some(to.output_ext())),
        Operation::Tool { tool, params } => {
            let base = format!("{stem} {}", tool_suffix(*tool, params));
            let ext = match tool_naming(*tool).ext {
                OutputExt::Same => src_ext,
                OutputExt::SameWavToFlac => {
                    if format == Some(Format::Wav) {
                        Some(Format::Flac.output_ext())
                    } else {
                        src_ext
                    }
                }
                OutputExt::Fixed(f) => Some(f.output_ext()),
                OutputExt::Folder => None,
            };
            (base, ext)
        }
    }
}

/// `{base}.{ext}`, or `{base} 2.{ext}`, `{base} 3.{ext}`, … until the folded
/// name is not in `folded`.
pub(crate) fn unique_name(base: &str, ext: Option<&str>, folded: &HashSet<String>) -> String {
    let with_ext = |b: &str| match ext {
        Some(ext) => format!("{b}.{ext}"),
        None => b.to_string(),
    };
    let first = with_ext(base);
    if !folded.contains(&fold(&first)) {
        return first;
    }
    let mut counter: u64 = 2;
    loop {
        let candidate = with_ext(&format!("{base} {counter}"));
        if !folded.contains(&fold(&candidate)) {
            return candidate;
        }
        counter += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::fold;

    /// Every code point shares its key with its own lowercase and uppercase
    /// forms, so no case pair can slip past the collision check.
    #[test]
    fn fold_never_splits_a_case_pair() {
        let mut misses = Vec::new();
        for c in (0..=0x10_FFFF_u32).filter_map(char::from_u32) {
            let key = fold(&c.to_string());
            let lower: String = c.to_lowercase().collect();
            let upper: String = c.to_uppercase().collect();
            if fold(&lower) != key || fold(&upper) != key {
                misses.push(format!("U+{:04X}", u32::from(c)));
            }
        }
        assert!(
            misses.is_empty(),
            "case pairs with different keys: {misses:?}"
        );
    }

    #[test]
    fn fold_joins_capital_and_small_sharp_s() {
        assert_eq!(fold("STRAẞE.PNG"), fold("straße.png"));
        assert_eq!(fold("STRAẞE.PNG"), fold("STRASSE.PNG"));
    }
}
