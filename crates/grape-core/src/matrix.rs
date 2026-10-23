//! The conversion matrix: which formats each input converts to, and the tier
//! of each pair.
//!
//! Source of truth: `docs/FORMAT_MATRIX.md` → "Conversion targets". The
//! `matrix!` table below mirrors the doc row by row and entry by entry, in the
//! doc's order. `Target@V2` / `Target@Later` mirror the doc's
//! `<sup>v2</sup>` / `<sup>later</sup>` marks; an untagged target is v1.
//! The generated `match` has no `_` arm, so every `Format` must have a row.

use crate::format::Format;
use crate::tier::{Features, Tier};

/// Expands one target entry to its tier: untagged means v1.
macro_rules! pair_tier {
    ($to:ident) => {
        Tier::V1
    };
    ($to:ident @ $tier:ident) => {
        Tier::$tier
    };
}

/// Builds `targets()` and `row_tiers()` from one table so they can't drift.
macro_rules! matrix {
    ($($from:ident => [$($to:ident $(@ $tier:ident)?),* $(,)?];)*) => {
        /// The conversion targets of `f`, in the doc's order, across all
        /// tiers. `html` (target-only) has none. Filter with
        /// [`targets_enabled`] before showing them.
        pub fn targets(f: Format) -> &'static [Format] {
            match f {
                $(Format::$from => &[$(Format::$to),*],)*
            }
        }

        /// The tier of each entry of `targets(f)`, index for index.
        fn row_tiers(f: Format) -> &'static [Tier] {
            match f {
                $(Format::$from => &[$(pair_tier!($to $(@ $tier)?)),*],)*
            }
        }
    };
}

matrix! {
    // Images
    Jpg  => [Png, Webp, Heic, Tiff, Avif, Bmp, Pdf];
    Png  => [Jpg, Webp, Heic, Tiff, Avif, Bmp, Pdf];
    Webp => [Jpg, Png, Heic, Tiff, Avif, Bmp, Pdf];
    Heic => [Jpg, Png, Webp, Tiff, Avif, Bmp, Pdf];
    Tiff => [Jpg, Png, Webp, Heic, Avif, Bmp, Pdf];
    Svg  => [Jpg, Png, Webp, Heic, Tiff, Avif, Bmp, Pdf];
    Avif => [Jpg, Png, Webp, Heic, Tiff, Bmp, Pdf];
    Bmp  => [Jpg, Png, Webp, Heic, Tiff, Avif, Pdf];
    // Audio
    Mp3  => [M4a, Wav, Flac, Ogg, Opus, Aiff, Wma];
    M4a  => [Mp3, Wav, Flac, Ogg, Opus, Aiff, Wma];
    Wav  => [Mp3, M4a, Flac, Ogg, Opus, Aiff, Wma];
    Flac => [Mp3, M4a, Wav, Ogg, Opus, Aiff, Wma];
    Ogg  => [Mp3, M4a, Wav, Flac, Opus, Aiff, Wma];
    Opus => [Mp3, M4a, Wav, Flac, Ogg, Aiff, Wma];
    Aiff => [Mp3, M4a, Wav, Flac, Ogg, Opus, Wma];
    Wma  => [Mp3, M4a, Wav, Flac, Ogg, Opus, Aiff];
    // Video
    Mp4  => [Mov, Mkv, Webm, Avi, Wmv, Gif, Mp3];
    Mov  => [Mp4, Mkv, Webm, Avi, Wmv, Gif, Mp3];
    Mkv  => [Mp4, Mov, Webm, Avi, Wmv, Gif, Mp3];
    Webm => [Mp4, Mov, Mkv, Avi, Wmv, Gif, Mp3];
    Avi  => [Mp4, Mov, Mkv, Webm, Wmv, Gif, Mp3];
    Wmv  => [Mp4, Mov, Mkv, Webm, Avi, Gif, Mp3];
    Gif  => [Mp4, Mov, Mkv, Webm, Avi, Wmv];
    // PDF
    Pdf  => [Docx@V2, Pages@Later, Pptx@V2, Xlsx@V2, Jpg, Png, Epub@V2, Txt, Md];
    // Slides
    Ppt  => [Pdf@V2, Jpg@V2, Png@V2];
    Pptx => [Pdf@V2, Jpg@V2, Png@V2];
    // Spreadsheets
    Xlsx => [Ods@V2, Csv@V2, Tsv@V2, Pdf@V2];
    Xls  => [Xlsx@V2, Ods@V2, Csv@V2, Tsv@V2, Pdf@V2];
    Ods  => [Xlsx@V2, Csv@V2, Tsv@V2, Pdf@V2];
    Csv  => [Xlsx@V2, Ods@V2, Tsv@V2, Pdf@V2];
    Tsv  => [Xlsx@V2, Ods@V2, Csv@V2, Pdf@V2];
    // Documents
    Docx => [Pages@Later, Pdf@V2, Txt@V2, Md@V2, Html@V2, Epub@V2, Png@V2, Jpg@V2];
    Pages => [Docx@Later, Pdf@Later, Txt@Later, Md@Later, Html@Later, Epub@Later, Png@Later, Jpg@Later];
    Md   => [Docx@V2, Pages@Later, Pdf@V2, Txt@V2, Html@V2, Epub@V2, Png@V2, Jpg@V2];
    Rtf  => [Docx@V2, Pages@Later, Pdf@V2, Txt@V2, Md@V2, Html@V2, Epub@V2, Png@V2, Jpg@V2];
    Rtfd => [Docx@V2, Pages@Later, Pdf@V2, Txt@V2, Md@V2, Html@V2, Epub@V2, Png@V2, Jpg@V2];
    TextClipping => [Docx@V2, Pages@Later, Pdf@V2, Txt@V2, Md@V2, Html@V2, Epub@V2, Png@V2, Jpg@V2];
    Epub => [Pdf@V2, Docx@V2, Pages@Later, Txt@V2, Md@V2, Html@V2];
    Txt  => [Docx@V2, Pages@Later, Pdf@V2, Md@V2, Html@V2, Epub@V2, Jpg@V2, Png@V2];
    // Subtitles
    Srt  => [Vtt, Txt];
    Vtt  => [Srt, Txt];
    // Archives
    Zip  => [Tar, Gz];
    Tar  => [Zip, Gz];
    Gz   => [Zip, Tar];
    Rar  => [Zip, Tar, Gz];
    // Target-only
    Html => [];
}

/// The tier of the pair `from → to`, or `None` when the matrix doesn't
/// offer it.
pub fn pair_tier(from: Format, to: Format) -> Option<Tier> {
    let index = targets(from).iter().position(|&t| t == to)?;
    row_tiers(from).get(index).copied()
}

/// The targets of `f` whose tier is enabled under `features`, in doc order.
pub fn targets_enabled(f: Format, features: Features) -> Vec<Format> {
    targets(f)
        .iter()
        .zip(row_tiers(f))
        .filter(|(_, tier)| tier.is_enabled(features))
        .map(|(&to, _)| to)
        .collect()
}

/// Every enabled `(from, to)` pair, in doc order. `pairs(Features::FULL)`
/// has 275 entries and `pairs(Features::V1_ONLY)` has 178.
pub fn pairs(features: Features) -> Vec<(Format, Format)> {
    Format::ALL
        .iter()
        .flat_map(|&from| {
            targets_enabled(from, features)
                .into_iter()
                .map(move |to| (from, to))
        })
        .collect()
}
