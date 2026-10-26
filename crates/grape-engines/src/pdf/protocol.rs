//! What the parent and the PDF worker say to each other.
//!
//! The parent writes one [`Request`] as a JSON line to the worker's stdin
//! and closes it. The worker answers with JSON lines on stdout, one
//! [`Message`] each: `{"pages":12}` once it knows the page count,
//! `{"progress":0.42}` as it goes, then exactly one of
//! `{"done":{…}}` (exit code 0) or `{"error":{"kind":…,"detail":…}}` (exit
//! code 1). No message ever holds a path, a file name or file contents.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The most bytes of one request; more is refused.
pub(crate) const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;

/// The longest line the parent reads from the worker. A longer one is not
/// a message and is skipped.
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;

/// The job the worker is asked to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    /// The PDFium library to bind, by absolute path. Bound only by the
    /// operations that need it.
    pub(crate) pdfium: PathBuf,
    /// The empty temp folder the parent created: the only place the worker
    /// writes, always with new names.
    pub(crate) out_dir: PathBuf,
    /// What to do.
    pub(crate) op: WorkerOp,
    /// The bounds to hold.
    pub(crate) bounds: Bounds,
}

/// One operation, with its sources (absolute paths, opened read-only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum WorkerOp {
    /// Every page to an image: `page-{n}.{jpg|png}` (n from 1, no padding).
    Render {
        /// The PDF.
        source: PathBuf,
        /// The image format.
        format: ImageFormat,
    },
    /// The text of every page: `text.{txt|md}`.
    Text {
        /// The PDF.
        source: PathBuf,
        /// Plain text or Markdown.
        format: TextFormat,
    },
    /// One PDF per page: `page-{n}.pdf`.
    Split {
        /// The PDF.
        source: PathBuf,
    },
    /// All pages of all sources, in this order: `merged.pdf`.
    Merge {
        /// The PDFs.
        sources: Vec<PathBuf>,
    },
    /// The document without its Info dictionary and XMP stream:
    /// `clean.pdf`.
    RemoveMetadata {
        /// The PDF.
        source: PathBuf,
    },
}

/// The output names the worker uses inside the temp folder. The parent
/// looks only for these.
pub(crate) mod names {
    use super::{ImageFormat, TextFormat};

    /// Page `n` (from 1) of a render or a split.
    pub(crate) fn page(n: u32, ext: &str) -> String {
        format!("page-{n}.{ext}")
    }

    /// A render's image extension.
    pub(crate) fn image_ext(format: ImageFormat) -> &'static str {
        match format {
            ImageFormat::Jpg => "jpg",
            ImageFormat::Png => "png",
        }
    }

    /// The text output.
    pub(crate) fn text(format: TextFormat) -> &'static str {
        match format {
            TextFormat::Txt => "text.txt",
            TextFormat::Md => "text.md",
        }
    }

    /// The merged PDF.
    pub(crate) const MERGED: &str = "merged.pdf";

    /// The PDF without metadata.
    pub(crate) const CLEAN: &str = "clean.pdf";
}

/// A render's image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ImageFormat {
    /// JPEG, quality 90, no alpha (pages are drawn on white).
    Jpg,
    /// PNG, default compression, RGB on white.
    Png,
}

/// A text extraction's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TextFormat {
    /// Plain text: pages joined with a blank line.
    Txt,
    /// Markdown: paragraphs and headings by font size.
    Md,
}

/// The bounds the worker holds (see `run::Limits` for the defaults).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Bounds {
    /// The resolution pages are rendered at.
    pub(crate) dpi: f32,
    /// The most pages a render exports. The parent's timeout also counts
    /// at most this many pages, whatever the worker reports.
    pub(crate) max_pages: u32,
    /// The most pages Split PDF makes files of.
    pub(crate) max_split_pages: u32,
    /// The most pixels one rendered page may have; a larger page is
    /// rendered at a lower resolution (never higher).
    pub(crate) max_page_pixels: u64,
    /// The most pixels on either side of a rendered page (JPEG's limit).
    pub(crate) max_side: u32,
    /// The most bytes a job writes in all.
    pub(crate) max_output_bytes: u64,
    /// The most bytes of text a TXT or MD export holds and writes.
    pub(crate) max_text_bytes: u64,
    /// The most bytes any single stream may decompress to while lopdf
    /// loads a file (object and cross-reference streams).
    pub(crate) max_decompressed: u64,
    /// The most bytes the worker's own heap may hold (see `heap`); past
    /// it the worker exits with `heap::OVER_HEAP_EXIT`.
    pub(crate) max_heap_bytes: u64,
}

/// One line from the worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum Message {
    /// The page count (all inputs together for a merge), once known. The
    /// parent's timeout grows with it.
    Pages(u32),
    /// How far along the job is, 0.0..=1.0.
    Progress(f32),
    /// The job is done; its outputs are in the temp folder.
    Done(Done),
    /// The job failed; whatever is in the temp folder is to be thrown away.
    Error(Failure),
}

/// What a finished job produced, besides its files.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Done {
    /// Pages written (render, split) or in the output (text, merge,
    /// metadata).
    pub(crate) pages: u32,
    /// Render: the pages (from 1) drawn below the requested resolution
    /// because they were over the pixel bounds.
    pub(crate) scaled_pages: Vec<u32>,
    /// Remove Metadata: what was there and is now gone.
    pub(crate) removed: Vec<MetaKey>,
    /// Split and merge: lopdf couldn't read a file, so PDFium's page import
    /// did the job.
    pub(crate) used_pdfium: bool,
}

/// A document-level metadata entry Remove Metadata found and removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum MetaKey {
    /// Info `/Title`.
    Title,
    /// Info `/Author`.
    Author,
    /// Info `/Subject`.
    Subject,
    /// Info `/Keywords`.
    Keywords,
    /// Info `/Creator` (the app that made the document).
    Creator,
    /// Info `/Producer` (the tool that wrote the PDF).
    Producer,
    /// Info `/CreationDate`.
    CreationDate,
    /// Info `/ModDate`.
    ModDate,
    /// Any other Info entry.
    Other,
    /// The catalog's XMP `/Metadata` stream.
    #[serde(rename = "XMP")]
    Xmp,
}

/// Why the worker failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Failure {
    /// The kind, which picks the user message.
    pub(crate) kind: FailKind,
    /// For the log: never a path, a name or file contents.
    #[serde(default)]
    pub(crate) detail: String,
    /// Merge: which source (from 0) it was about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input: Option<usize>,
}

impl Failure {
    /// A failure about the (only or first) source.
    pub(crate) fn new(kind: FailKind, detail: impl Into<String>) -> Failure {
        Failure {
            kind,
            detail: detail.into(),
            input: None,
        }
    }

    /// The same failure, about merge source `input`.
    pub(crate) fn about(mut self, input: usize) -> Failure {
        self.input = Some(input);
        self
    }
}

/// The kinds of failure the worker reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FailKind {
    /// No `%PDF-` in the first 1024 bytes.
    NotPdf,
    /// Neither library could open it.
    Damaged,
    /// It needs a password (or uses encryption Grape won't rewrite).
    Encrypted,
    /// It has no pages.
    NoPages,
    /// No page has any text (likely a scan).
    NoText,
    /// More pages than [`Bounds::max_pages`].
    TooManyPages,
    /// Split PDF: more pages than [`Bounds::max_split_pages`].
    TooManyPagesToSplit,
    /// The output would pass [`Bounds::max_output_bytes`].
    TooMuchOutput,
    /// The text would pass [`Bounds::max_text_bytes`].
    TooMuchText,
    /// The file is larger than PDFium can address (4 GiB on Windows).
    TooLarge,
    /// A source couldn't be read.
    Read,
    /// The temp folder couldn't be written.
    Write,
    /// PDFium couldn't be loaded.
    Library,
    /// A bad request, a panic or another fault in the worker.
    Internal,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        value: &T,
        json: &str,
    ) {
        assert_eq!(serde_json::to_string(value).unwrap(), json);
        assert_eq!(&serde_json::from_str::<T>(json).unwrap(), value);
    }

    fn bounds() -> Bounds {
        Bounds {
            dpi: 300.0,
            max_pages: 2000,
            max_split_pages: 100_000,
            max_page_pixels: 100_000_000,
            max_side: 65_535,
            max_output_bytes: 8 << 30,
            max_text_bytes: 256 << 20,
            max_decompressed: 512 << 20,
            max_heap_bytes: 4 << 30,
        }
    }

    #[test]
    fn requests_round_trip() {
        let request = Request {
            pdfium: PathBuf::from("/lib/libpdfium.so"),
            out_dir: PathBuf::from("/out/.grape-tmp-1"),
            op: WorkerOp::Render {
                source: PathBuf::from("/in/a.pdf"),
                format: ImageFormat::Png,
            },
            bounds: bounds(),
        };
        round_trip(
            &request,
            concat!(
                r#"{"pdfium":"/lib/libpdfium.so","outDir":"/out/.grape-tmp-1","#,
                r#""op":{"kind":"render","source":"/in/a.pdf","format":"png"},"#,
                r#""bounds":{"dpi":300.0,"maxPages":2000,"maxSplitPages":100000,"#,
                r#""maxPagePixels":100000000,"maxSide":65535,"maxOutputBytes":8589934592,"#,
                r#""maxTextBytes":268435456,"maxDecompressed":536870912,"#,
                r#""maxHeapBytes":4294967296}}"#
            ),
        );
        for (op, json) in [
            (
                WorkerOp::Text {
                    source: PathBuf::from("/a.pdf"),
                    format: TextFormat::Md,
                },
                r#"{"kind":"text","source":"/a.pdf","format":"md"}"#,
            ),
            (
                WorkerOp::Split {
                    source: PathBuf::from("/a.pdf"),
                },
                r#"{"kind":"split","source":"/a.pdf"}"#,
            ),
            (
                WorkerOp::Merge {
                    sources: vec![PathBuf::from("/b.pdf"), PathBuf::from("/a.pdf")],
                },
                r#"{"kind":"merge","sources":["/b.pdf","/a.pdf"]}"#,
            ),
            (
                WorkerOp::RemoveMetadata {
                    source: PathBuf::from("/a.pdf"),
                },
                r#"{"kind":"removeMetadata","source":"/a.pdf"}"#,
            ),
        ] {
            round_trip(&op, json);
        }
    }

    #[test]
    fn messages_round_trip() {
        round_trip(&Message::Pages(12), r#"{"pages":12}"#);
        round_trip(&Message::Progress(0.5), r#"{"progress":0.5}"#);
        round_trip(
            &Message::Done(Done {
                pages: 3,
                scaled_pages: vec![2],
                removed: vec![MetaKey::Title, MetaKey::Xmp],
                used_pdfium: false,
            }),
            r#"{"done":{"pages":3,"scaledPages":[2],"removed":["Title","XMP"],"usedPdfium":false}}"#,
        );
        round_trip(
            &Message::Error(Failure::new(FailKind::Encrypted, "user password").about(1)),
            r#"{"error":{"kind":"encrypted","detail":"user password","input":1}}"#,
        );
        round_trip(
            &Message::Error(Failure::new(FailKind::NoText, "")),
            r#"{"error":{"kind":"noText","detail":""}}"#,
        );
        for (kind, name) in [
            (FailKind::TooManyPagesToSplit, "tooManyPagesToSplit"),
            (FailKind::TooMuchText, "tooMuchText"),
            (FailKind::TooLarge, "tooLarge"),
        ] {
            round_trip(
                &Message::Error(Failure::new(kind, "")),
                &format!(r#"{{"error":{{"kind":"{name}","detail":""}}}}"#),
            );
        }
        // Missing fields take their defaults.
        assert_eq!(
            serde_json::from_str::<Message>(r#"{"done":{}}"#).unwrap(),
            Message::Done(Done::default())
        );
    }

    #[test]
    fn unknown_messages_and_fields_are_refused() {
        for line in [
            r#"{"hello":1}"#,
            r#"{"done":{"pages":1,"extra":true}}"#,
            r#"{"progress":0.1,"pages":2}"#,
            r#"{"error":{"kind":"meltdown"}}"#,
            "running 1 test",
            "",
        ] {
            assert!(serde_json::from_str::<Message>(line).is_err(), "{line}");
        }
        let extra = r#"{"pdfium":"/p","outDir":"/o","op":{"kind":"split","source":"/a"},"bounds":{"dpi":300.0,"maxPages":1,"maxSplitPages":1,"maxPagePixels":1,"maxSide":1,"maxOutputBytes":1,"maxTextBytes":1,"maxDecompressed":1,"maxHeapBytes":1},"more":1}"#;
        assert!(serde_json::from_str::<Request>(extra).is_err());
    }

    #[test]
    fn output_names_are_fixed() {
        assert_eq!(names::page(7, "png"), "page-7.png");
        assert_eq!(names::image_ext(ImageFormat::Jpg), "jpg");
        assert_eq!(names::text(TextFormat::Md), "text.md");
        assert_eq!(names::MERGED, "merged.pdf");
        assert_eq!(names::CLEAN, "clean.pdf");
    }
}
