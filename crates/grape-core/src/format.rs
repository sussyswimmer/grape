//! The format registry: every format Grape knows, its key, display name,
//! category and output extension.
//!
//! Source of truth: `docs/FORMAT_MATRIX.md` → "Rules" and "Category of an
//! input". Variants are declared in the doc's table order (images, audio,
//! video, PDF, slides, spreadsheets, documents, subtitles, archives), with the
//! one target-only format, `html`, last.

use serde::{Deserialize, Serialize};

/// The category of a format. Drives the icon, the job runner and the engine.
///
/// Source: `FORMAT_MATRIX.md` → "Category of an input".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    /// `jpg png webp heic tiff svg avif bmp`
    Image,
    /// `mp3 m4a wav flac ogg opus aiff wma`
    Audio,
    /// `mp4 mov mkv webm avi wmv gif`
    Video,
    /// `pdf ppt pptx docx pages md rtf rtfd textClipping txt html epub`
    Document,
    /// `xlsx xls ods csv tsv`
    Spreadsheet,
    /// `zip tar gz rar`
    Archive,
    /// `srt vtt`
    Subtitle,
}

/// A file format. The 45 input formats of `FORMAT_MATRIX.md` plus the
/// target-only `html`, in the doc's table order.
///
/// Serialized as the doc key (`"jpg"`, `"textClipping"`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    // Images
    /// JPEG image (`jpg`, alias `jpeg`).
    Jpg,
    /// PNG image.
    Png,
    /// WebP image.
    Webp,
    /// HEIC/HEIF image (`heic`, aliases `heif` and `hif`).
    Heic,
    /// TIFF image (`tiff`, alias `tif`).
    Tiff,
    /// SVG vector image. Input-only within images.
    Svg,
    /// AVIF image.
    Avif,
    /// BMP image.
    Bmp,
    // Audio
    /// MP3 audio.
    Mp3,
    /// M4A (AAC) audio.
    M4a,
    /// WAV audio.
    Wav,
    /// FLAC audio.
    Flac,
    /// Ogg Vorbis audio.
    Ogg,
    /// Opus audio.
    Opus,
    /// AIFF audio (`aiff`, alias `aif`).
    Aiff,
    /// WMA audio.
    Wma,
    // Video
    /// MP4 video.
    Mp4,
    /// QuickTime video.
    Mov,
    /// Matroska video.
    Mkv,
    /// WebM video.
    Webm,
    /// AVI video.
    Avi,
    /// WMV video.
    Wmv,
    /// Animated GIF. Categorized as video.
    Gif,
    // PDF
    /// PDF document.
    Pdf,
    // Slides
    /// Legacy PowerPoint. Input-only.
    Ppt,
    /// PowerPoint.
    Pptx,
    // Spreadsheets
    /// Excel workbook.
    Xlsx,
    /// Legacy Excel workbook. Input-only.
    Xls,
    /// OpenDocument spreadsheet.
    Ods,
    /// Comma-separated values.
    Csv,
    /// Tab-separated values.
    Tsv,
    // Documents
    /// Word document.
    Docx,
    /// Apple Pages document.
    Pages,
    /// Markdown (`md`, alias `markdown`).
    Md,
    /// Rich Text Format. Input-only.
    Rtf,
    /// Rich Text Format bundle. Input-only.
    Rtfd,
    /// macOS text clipping. Input-only.
    TextClipping,
    /// EPUB e-book.
    Epub,
    /// Plain text.
    Txt,
    // Subtitles
    /// SubRip subtitles.
    Srt,
    /// WebVTT subtitles.
    Vtt,
    // Archives
    /// ZIP archive.
    Zip,
    /// Tar archive.
    Tar,
    /// Gzip-compressed tar (`gz`, `tgz`, `tar.gz`). Displays as GZIP and is
    /// written as `.tar.gz`.
    Gz,
    /// RAR archive. Input-only.
    Rar,
    // Target-only
    /// HTML. The only target-only format (alias `htm`).
    Html,
}

/// Rule 2 of `FORMAT_MATRIX.md`: input-only formats are never offered as
/// targets. This is the doc's list verbatim. Note that `svg` is also never a
/// target ("`svg` is input-only within images") but is not in the rule's list.
pub const INPUT_ONLY: [Format; 6] = [
    Format::Rar,
    Format::Ppt,
    Format::Xls,
    Format::Rtf,
    Format::Rtfd,
    Format::TextClipping,
];

impl Format {
    /// Every format, in doc order. The first 45 are inputs; `Html` is last.
    pub const ALL: [Format; 46] = [
        Format::Jpg,
        Format::Png,
        Format::Webp,
        Format::Heic,
        Format::Tiff,
        Format::Svg,
        Format::Avif,
        Format::Bmp,
        Format::Mp3,
        Format::M4a,
        Format::Wav,
        Format::Flac,
        Format::Ogg,
        Format::Opus,
        Format::Aiff,
        Format::Wma,
        Format::Mp4,
        Format::Mov,
        Format::Mkv,
        Format::Webm,
        Format::Avi,
        Format::Wmv,
        Format::Gif,
        Format::Pdf,
        Format::Ppt,
        Format::Pptx,
        Format::Xlsx,
        Format::Xls,
        Format::Ods,
        Format::Csv,
        Format::Tsv,
        Format::Docx,
        Format::Pages,
        Format::Md,
        Format::Rtf,
        Format::Rtfd,
        Format::TextClipping,
        Format::Epub,
        Format::Txt,
        Format::Srt,
        Format::Vtt,
        Format::Zip,
        Format::Tar,
        Format::Gz,
        Format::Rar,
        Format::Html,
    ];

    /// The doc key, which is also the lowercase extension (except
    /// `textClipping`, whose key keeps its camel case).
    pub const fn key(self) -> &'static str {
        match self {
            Format::Jpg => "jpg",
            Format::Png => "png",
            Format::Webp => "webp",
            Format::Heic => "heic",
            Format::Tiff => "tiff",
            Format::Svg => "svg",
            Format::Avif => "avif",
            Format::Bmp => "bmp",
            Format::Mp3 => "mp3",
            Format::M4a => "m4a",
            Format::Wav => "wav",
            Format::Flac => "flac",
            Format::Ogg => "ogg",
            Format::Opus => "opus",
            Format::Aiff => "aiff",
            Format::Wma => "wma",
            Format::Mp4 => "mp4",
            Format::Mov => "mov",
            Format::Mkv => "mkv",
            Format::Webm => "webm",
            Format::Avi => "avi",
            Format::Wmv => "wmv",
            Format::Gif => "gif",
            Format::Pdf => "pdf",
            Format::Ppt => "ppt",
            Format::Pptx => "pptx",
            Format::Xlsx => "xlsx",
            Format::Xls => "xls",
            Format::Ods => "ods",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Docx => "docx",
            Format::Pages => "pages",
            Format::Md => "md",
            Format::Rtf => "rtf",
            Format::Rtfd => "rtfd",
            Format::TextClipping => "textClipping",
            Format::Epub => "epub",
            Format::Txt => "txt",
            Format::Srt => "srt",
            Format::Vtt => "vtt",
            Format::Zip => "zip",
            Format::Tar => "tar",
            Format::Gz => "gz",
            Format::Rar => "rar",
            Format::Html => "html",
        }
    }

    /// The wheel label: the uppercase extension, except `gz` is "GZIP"
    /// (`FORMAT_MATRIX.md` rule 4).
    pub const fn display_name(self) -> &'static str {
        match self {
            Format::Jpg => "JPG",
            Format::Png => "PNG",
            Format::Webp => "WEBP",
            Format::Heic => "HEIC",
            Format::Tiff => "TIFF",
            Format::Svg => "SVG",
            Format::Avif => "AVIF",
            Format::Bmp => "BMP",
            Format::Mp3 => "MP3",
            Format::M4a => "M4A",
            Format::Wav => "WAV",
            Format::Flac => "FLAC",
            Format::Ogg => "OGG",
            Format::Opus => "OPUS",
            Format::Aiff => "AIFF",
            Format::Wma => "WMA",
            Format::Mp4 => "MP4",
            Format::Mov => "MOV",
            Format::Mkv => "MKV",
            Format::Webm => "WEBM",
            Format::Avi => "AVI",
            Format::Wmv => "WMV",
            Format::Gif => "GIF",
            Format::Pdf => "PDF",
            Format::Ppt => "PPT",
            Format::Pptx => "PPTX",
            Format::Xlsx => "XLSX",
            Format::Xls => "XLS",
            Format::Ods => "ODS",
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
            Format::Docx => "DOCX",
            Format::Pages => "PAGES",
            Format::Md => "MD",
            Format::Rtf => "RTF",
            Format::Rtfd => "RTFD",
            Format::TextClipping => "TEXTCLIPPING",
            Format::Epub => "EPUB",
            Format::Txt => "TXT",
            Format::Srt => "SRT",
            Format::Vtt => "VTT",
            Format::Zip => "ZIP",
            Format::Tar => "TAR",
            Format::Gz => "GZIP",
            Format::Rar => "RAR",
            Format::Html => "HTML",
        }
    }

    /// The format's category (`FORMAT_MATRIX.md` → "Category of an input").
    pub const fn category(self) -> Category {
        match self {
            Format::Jpg
            | Format::Png
            | Format::Webp
            | Format::Heic
            | Format::Tiff
            | Format::Svg
            | Format::Avif
            | Format::Bmp => Category::Image,
            Format::Mp3
            | Format::M4a
            | Format::Wav
            | Format::Flac
            | Format::Ogg
            | Format::Opus
            | Format::Aiff
            | Format::Wma => Category::Audio,
            Format::Mp4
            | Format::Mov
            | Format::Mkv
            | Format::Webm
            | Format::Avi
            | Format::Wmv
            | Format::Gif => Category::Video,
            Format::Pdf
            | Format::Ppt
            | Format::Pptx
            | Format::Docx
            | Format::Pages
            | Format::Md
            | Format::Rtf
            | Format::Rtfd
            | Format::TextClipping
            | Format::Txt
            | Format::Html
            | Format::Epub => Category::Document,
            Format::Xlsx | Format::Xls | Format::Ods | Format::Csv | Format::Tsv => {
                Category::Spreadsheet
            }
            Format::Zip | Format::Tar | Format::Gz | Format::Rar => Category::Archive,
            Format::Srt | Format::Vtt => Category::Subtitle,
        }
    }

    /// The extension written when converting *to* this format, without the
    /// leading dot. `gz` writes `tar.gz` (`FORMAT_MATRIX.md` rule 5); every
    /// other format writes its key.
    pub const fn output_ext(self) -> &'static str {
        match self {
            Format::Gz => "tar.gz",
            other => other.key(),
        }
    }

    /// True when this format appears in rule 2's input-only list
    /// ([`INPUT_ONLY`]).
    pub fn is_input_only(self) -> bool {
        INPUT_ONLY.contains(&self)
    }

    /// True when the format can be dropped as a source, i.e. it has a row in
    /// the conversion matrix. Every format except `html`.
    pub fn is_input(self) -> bool {
        !crate::matrix::targets(self).is_empty()
    }

    /// Resolve a file extension (with or without a leading dot, any ASCII
    /// case) to a format. Handles the aliases `jpeg tif heif hif aif htm
    /// markdown tgz`
    /// and the compound `tar.gz`. Unknown extensions give `None`.
    pub fn from_extension(ext: &str) -> Option<Format> {
        let ext = ext.strip_prefix('.').unwrap_or(ext);
        let lower = ext.to_ascii_lowercase();
        let format = match lower.as_str() {
            "jpg" | "jpeg" => Format::Jpg,
            "png" => Format::Png,
            "webp" => Format::Webp,
            "heic" | "heif" | "hif" => Format::Heic,
            "tiff" | "tif" => Format::Tiff,
            "svg" => Format::Svg,
            "avif" => Format::Avif,
            "bmp" => Format::Bmp,
            "mp3" => Format::Mp3,
            "m4a" => Format::M4a,
            "wav" => Format::Wav,
            "flac" => Format::Flac,
            "ogg" => Format::Ogg,
            "opus" => Format::Opus,
            "aiff" | "aif" => Format::Aiff,
            "wma" => Format::Wma,
            "mp4" => Format::Mp4,
            "mov" => Format::Mov,
            "mkv" => Format::Mkv,
            "webm" => Format::Webm,
            "avi" => Format::Avi,
            "wmv" => Format::Wmv,
            "gif" => Format::Gif,
            "pdf" => Format::Pdf,
            "ppt" => Format::Ppt,
            "pptx" => Format::Pptx,
            "xlsx" => Format::Xlsx,
            "xls" => Format::Xls,
            "ods" => Format::Ods,
            "csv" => Format::Csv,
            "tsv" => Format::Tsv,
            "docx" => Format::Docx,
            "pages" => Format::Pages,
            "md" | "markdown" => Format::Md,
            "rtf" => Format::Rtf,
            "rtfd" => Format::Rtfd,
            "textclipping" => Format::TextClipping,
            "epub" => Format::Epub,
            "txt" => Format::Txt,
            "srt" => Format::Srt,
            "vtt" => Format::Vtt,
            "zip" => Format::Zip,
            "tar" => Format::Tar,
            "gz" | "tgz" | "tar.gz" => Format::Gz,
            "rar" => Format::Rar,
            "html" | "htm" => Format::Html,
            _ => return None,
        };
        Some(format)
    }

    /// Resolve a file name (not a path) to a format by its extension, using
    /// the same split as [`crate::naming::split_name`], so `x.tar.gz` and
    /// `x.tgz` are `gz`. Names with no extension give `None`.
    pub fn from_file_name(name: &str) -> Option<Format> {
        let (_, ext) = split_name(name);
        ext.and_then(Format::from_extension)
    }
}

/// Compound extensions stripped as one unit (`FORMAT_MATRIX.md` → Output
/// naming: "`.tar.gz` / `.tgz` are stripped as one unit").
const COMPOUND_EXTS: [&str; 1] = [".tar.gz"];

/// Split a file name into `(stem, extension)`. Shared by format detection
/// and output naming so both always agree. See [`crate::naming::split_name`].
pub(crate) fn split_name(name: &str) -> (&str, Option<&str>) {
    for compound in COMPOUND_EXTS {
        let Some(cut) = name.len().checked_sub(compound.len()) else {
            continue;
        };
        if cut == 0 {
            continue;
        }
        if let (Some(stem), Some(tail)) = (name.get(..cut), name.get(cut..)) {
            if tail.eq_ignore_ascii_case(compound) {
                return (stem, tail.get(1..));
            }
        }
    }
    match name.rfind('.') {
        // A leading dot (`.env`) or a trailing dot (`notes.`) means "no
        // extension": the whole name is the stem.
        Some(0) | None => (name, None),
        Some(dot) if dot + 1 == name.len() => (name, None),
        Some(dot) => (&name[..dot], Some(&name[dot + 1..])),
    }
}
