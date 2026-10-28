//! What Grape and its SVG worker say to each other (D39): the worker's own
//! protocol, version [`PROTOCOL`], separate from the image helper's
//! (`grape-heif-proto`) and the PDF worker's.
//!
//! - The parent writes one [`Request`] as JSON to the worker's stdin and
//!   closes it.
//! - The worker answers with JSON lines on stdout, one [`Message`] each:
//!   `image` once, after parsing and sizing and before the pixmap is
//!   allocated; `progress`; then exactly one of `done` (exit code 0) or
//!   `error` (exit code 1). Exit code 2 is bad usage, and
//!   [`crate::pdf::heap::OVER_HEAP_EXIT`] (86) the heap bound.
//! - The pixels go through `pixels.rgba` ([`PIXELS`]) in the request's
//!   work folder, written new with grape-heif-proto's `PixelWriter`: 3
//!   channels when every pixel is opaque, otherwise 4 with straight alpha,
//!   and no ICC profile (resvg draws sRGB).
//! - No message holds a path, a file name or file contents, and both sides
//!   refuse unknown fields. There is no internally tagged enum here, so
//!   serde's unit-variant gap (grape-heif-proto's `Probe {}`) can't arise:
//!   [`Mode`] and [`FailKind`] are plain strings.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The protocol version. Any change to the wire types bumps it; the worker
/// is Grape's own executable, so both sides always ship together.
pub(crate) const PROTOCOL: u32 = 1;

/// The most bytes of one request; a longer one is refused.
pub(crate) const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// The longest line the parent reads from the worker. A longer one is not a
/// message and is skipped.
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;

/// The worker's pixel file in the work folder, created new by the worker.
pub(crate) const PIXELS: &str = grape_heif_proto::names::PIXELS;

const MIB: u64 = 1024 * 1024;

/// One job for the worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    /// [`PROTOCOL`].
    pub(crate) protocol: u32,
    /// The SVG, an absolute path, opened read-only.
    pub(crate) source: PathBuf,
    /// The private work folder, an absolute path: the worker writes only
    /// [`PIXELS`] there.
    pub(crate) work_dir: PathBuf,
    /// How the canvas is sized.
    pub(crate) mode: Mode,
    /// What the worker holds to.
    pub(crate) limits: Limits,
}

/// How the canvas is sized (`super::size`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Mode {
    /// JPG, PNG, WebP, TIFF, BMP, HEIC and AVIF: the SVG's size, twice
    /// when it's under 256 px.
    Raster,
    /// A PDF page: 300 DPI.
    Page,
}

/// What the worker holds a job to. None may be 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Limits {
    /// The largest source file; a larger one is refused unread.
    pub(crate) max_source_bytes: u64,
    /// The most bytes of SVG text, after a gunzip.
    pub(crate) max_text_bytes: u64,
    /// The most XML nodes roxmltree parses.
    pub(crate) max_nodes: u32,
    /// The canvas's longest side, in pixels.
    pub(crate) max_side: u32,
    /// The canvas's pixels.
    pub(crate) max_pixels: u64,
    /// The most embedded (`data:`) SVGs the job admits, nested ones too.
    pub(crate) max_nested_svgs: u32,
    /// The most embedded (`data:`) bytes the job admits, decoded.
    pub(crate) max_data_bytes: u64,
    /// The longest side of an embedded raster image, from its header.
    pub(crate) max_embedded_side: u32,
    /// The pixels of an embedded raster image, from its header.
    pub(crate) max_embedded_pixels: u64,
    /// The worker's heap bound, armed once it has its request.
    pub(crate) max_heap_bytes: u64,
}

impl Limits {
    /// D39's limits, with a raster canvas's caps (16,384 a side, 64 MP).
    pub(crate) const DEFAULT: Limits = Limits {
        max_source_bytes: 64 * MIB,
        max_text_bytes: 64 * MIB,
        max_nodes: 2_000_000,
        max_side: 16_384,
        max_pixels: 64 * MIB,
        max_nested_svgs: 64,
        max_data_bytes: 128 * MIB,
        max_embedded_side: 16_384,
        max_embedded_pixels: 64 * MIB,
        max_heap_bytes: 2048 * MIB,
    };

    /// True when a limit is 0.
    pub(crate) fn has_zero(&self) -> bool {
        [
            self.max_source_bytes,
            self.max_text_bytes,
            u64::from(self.max_nodes),
            u64::from(self.max_side),
            self.max_pixels,
            u64::from(self.max_nested_svgs),
            self.max_data_bytes,
            u64::from(self.max_embedded_side),
            self.max_embedded_pixels,
            self.max_heap_bytes,
        ]
        .contains(&0)
    }
}

/// One line from the worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum Message {
    /// The canvas, before it is allocated.
    Image(ImageInfo),
    /// How far the job is, from 0 to 1.
    Progress(f32),
    /// The job is done; [`PIXELS`] is complete.
    Done(Done),
    /// The job failed.
    Error(Failure),
}

/// The canvas the worker will draw, and the SVG's own size.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ImageInfo {
    /// The canvas's width in pixels.
    pub(crate) width: u32,
    /// The canvas's height in pixels.
    pub(crate) height: u32,
    /// The SVG's width in CSS pixels (96 per inch).
    pub(crate) css_width: f32,
    /// The SVG's height in CSS pixels.
    pub(crate) css_height: f32,
    /// True when the caps made the canvas smaller than the size rule's.
    pub(crate) scaled: bool,
}

/// The `done` line: what [`PIXELS`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Done {
    /// The width written, the `image` line's.
    pub(crate) width: u32,
    /// The height written.
    pub(crate) height: u32,
    /// 3 when every pixel is opaque, otherwise 4.
    pub(crate) channels: u8,
    /// How many distinct references were left out: external ones, and
    /// embedded images refused by type or budget.
    pub(crate) left_out: u32,
    /// The `image` line's `scaled`.
    pub(crate) scaled: bool,
}

/// Why the worker failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Failure {
    /// The kind, which picks the user message.
    pub(crate) kind: FailKind,
    /// For the log: never a path, a name or file contents.
    #[serde(default)]
    pub(crate) detail: String,
}

impl Failure {
    /// A failure of `kind` with a log detail.
    pub(crate) fn new(kind: FailKind, detail: impl Into<String>) -> Failure {
        Failure {
            kind,
            detail: detail.into(),
        }
    }
}

/// The kinds of failure the worker reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FailKind {
    /// The source can't be opened or read.
    Read,
    /// Not an SVG Grape reads: bad gzip, not UTF-8 (UTF-16 included), bad
    /// XML, no `<svg>` root, an invalid size.
    NotSvg,
    /// Over a limit: the file, its text, its nodes, usvg's own limits, or
    /// a canvas that can't be allocated.
    TooLarge,
    /// [`PIXELS`] can't be created or written.
    Write,
    /// The request's protocol isn't [`PROTOCOL`].
    Protocol,
    /// A bad request, invalid paths or a panic.
    Internal,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn round_trip<T>(value: &T, json: &str)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        assert_eq!(serde_json::to_string(value).unwrap(), json);
        assert_eq!(&serde_json::from_str::<T>(json).unwrap(), value);
    }

    const LIMITS: &str = r#""limits":{"maxSourceBytes":67108864,"maxTextBytes":67108864,"maxNodes":2000000,"maxSide":16384,"maxPixels":67108864,"maxNestedSvgs":64,"maxDataBytes":134217728,"maxEmbeddedSide":16384,"maxEmbeddedPixels":67108864,"maxHeapBytes":2147483648}"#;

    fn request(mode: Mode) -> Request {
        Request {
            protocol: 1,
            source: PathBuf::from("/in/logo.svg"),
            work_dir: PathBuf::from("/t/grape-svg-123-456-0"),
            mode,
            limits: Limits::DEFAULT,
        }
    }

    #[test]
    fn the_spec_request_round_trips() {
        round_trip(
            &request(Mode::Raster),
            &format!(
                r#"{{"protocol":1,"source":"/in/logo.svg","workDir":"/t/grape-svg-123-456-0","mode":"raster",{LIMITS}}}"#
            ),
        );
        round_trip(
            &request(Mode::Page),
            &format!(
                r#"{{"protocol":1,"source":"/in/logo.svg","workDir":"/t/grape-svg-123-456-0","mode":"page",{LIMITS}}}"#
            ),
        );
        assert!(!Limits::DEFAULT.has_zero());
        for i in 0..10 {
            let mut limits = Limits::DEFAULT;
            match i {
                0 => limits.max_source_bytes = 0,
                1 => limits.max_text_bytes = 0,
                2 => limits.max_nodes = 0,
                3 => limits.max_side = 0,
                4 => limits.max_pixels = 0,
                5 => limits.max_nested_svgs = 0,
                6 => limits.max_data_bytes = 0,
                7 => limits.max_embedded_side = 0,
                8 => limits.max_embedded_pixels = 0,
                _ => limits.max_heap_bytes = 0,
            }
            assert!(limits.has_zero(), "{i}");
        }
    }

    #[test]
    fn the_spec_lines_round_trip() {
        round_trip(
            &Message::Image(ImageInfo {
                width: 48,
                height: 48,
                css_width: 24.0,
                css_height: 24.0,
                scaled: false,
            }),
            r#"{"image":{"width":48,"height":48,"cssWidth":24.0,"cssHeight":24.0,"scaled":false}}"#,
        );
        round_trip(&Message::Progress(0.5), r#"{"progress":0.5}"#);
        round_trip(
            &Message::Done(Done {
                width: 48,
                height: 48,
                channels: 4,
                left_out: 1,
                scaled: false,
            }),
            r#"{"done":{"width":48,"height":48,"channels":4,"leftOut":1,"scaled":false}}"#,
        );
        round_trip(
            &Message::Error(Failure::new(
                FailKind::NotSvg,
                "XML: unexpected end of stream at 3:1",
            )),
            r#"{"error":{"kind":"notSvg","detail":"XML: unexpected end of stream at 3:1"}}"#,
        );
    }

    #[test]
    fn every_failure_kind_has_its_name() {
        for (kind, name) in [
            (FailKind::Read, "read"),
            (FailKind::NotSvg, "notSvg"),
            (FailKind::TooLarge, "tooLarge"),
            (FailKind::Write, "write"),
            (FailKind::Protocol, "protocol"),
            (FailKind::Internal, "internal"),
        ] {
            round_trip(
                &Message::Error(Failure::new(kind, "d")),
                &format!(r#"{{"error":{{"kind":"{name}","detail":"d"}}}}"#),
            );
        }
        // The detail may be left out.
        assert_eq!(
            serde_json::from_str::<Message>(r#"{"error":{"kind":"read"}}"#).unwrap(),
            Message::Error(Failure::new(FailKind::Read, ""))
        );
    }

    #[test]
    fn unknown_fields_and_messages_are_refused_at_every_level() {
        let good = format!(
            r#"{{"protocol":1,"source":"/a.svg","workDir":"/w","mode":"raster",{LIMITS}}}"#
        );
        assert!(serde_json::from_str::<Request>(&good).is_ok());
        for bad in [
            good.replacen('{', r#"{"x":1,"#, 1),
            good.replace(
                r#""maxHeapBytes":2147483648"#,
                r#""maxHeapBytes":2147483648,"x":1"#,
            ),
            good.replace(r#""mode":"raster""#, r#""mode":"vector""#),
            good.replace(r#""mode":"raster""#, r#""mode":{"raster":{}}"#),
            // Every field is required.
            good.replace(r#""mode":"raster","#, ""),
            good.replace(r#""maxNodes":2000000,"#, ""),
        ] {
            assert!(serde_json::from_str::<Request>(&bad).is_err(), "{bad}");
        }
        for bad in [
            r#"{"image":{"width":1,"height":1,"cssWidth":1.0,"cssHeight":1.0,"scaled":false,"x":1}}"#,
            r#"{"image":{"width":1,"height":1,"cssWidth":1.0,"cssHeight":1.0}}"#,
            r#"{"done":{"width":1,"height":1,"channels":3,"leftOut":0,"scaled":false,"x":1}}"#,
            r#"{"done":{"width":1,"height":1,"channels":3,"scaled":false}}"#,
            r#"{"error":{"kind":"read","detail":"d","x":1}}"#,
            r#"{"error":{"kind":"crash","detail":"d"}}"#,
            r#"{"progress":0.5,"x":1}"#,
            r#"{"progress":"half"}"#,
            r#"{"probe":{}}"#,
            r#""progress""#,
            r#"{}"#,
        ] {
            assert!(serde_json::from_str::<Message>(bad).is_err(), "{bad}");
        }
    }
}
