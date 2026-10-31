//! The helper's answers: JSON lines on stdout, one [`Message`] each.

use serde::{Deserialize, Serialize};

use crate::HeifFormat;

/// One line from the helper.
///
/// - `image` comes at most once, only for `decode`, and before any pixel is
///   written: before decoding with libheif, right after ImageIO's own
///   decode on the Mac ([`crate::Codec::decode`]).
/// - The last line is exactly one of `done` or `error`. The exit code is 0
///   after `done` and 1 after `error` (2 for bad usage, before any line).
/// - The parent skips a line longer than [`crate::MAX_LINE_BYTES`] or one
///   that isn't a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Message {
    /// Decode: the image's size, channels and ICC size.
    Image(ImageInfo),
    /// How far the job is, from 0 to 1.
    Progress(f32),
    /// The job is done; `pixels.rgba`, `out.heic` or `out.avif` is complete.
    Done(Done),
    /// The job failed.
    Error(Failure),
}

/// What a decode will produce, sent before any pixel is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageInfo {
    /// Width in pixels, upright (after the image's own transforms).
    pub width: u32,
    /// Height in pixels, upright.
    pub height: u32,
    /// 3 (RGB) or 4 (RGBA, straight alpha).
    pub channels: u8,
    /// The size of the ICC profile in `pixels.rgba`, 0 for none.
    pub icc_bytes: u32,
}

/// The `done` line. Zero values and `None` are left out when writing and
/// default when read: decode fills the size, encode `bytes`, probe `caps`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Done {
    /// Decode: the width written to `pixels.rgba`.
    #[serde(skip_serializing_if = "is_zero")]
    pub width: u32,
    /// Decode: the height written to `pixels.rgba`.
    #[serde(skip_serializing_if = "is_zero")]
    pub height: u32,
    /// Decode: the channels written to `pixels.rgba`.
    #[serde(skip_serializing_if = "is_zero")]
    pub channels: u8,
    /// Decode: the ICC profile's size in `pixels.rgba`.
    #[serde(skip_serializing_if = "is_zero")]
    pub icc_bytes: u32,
    /// Encode: the size of `out.heic` or `out.avif`.
    #[serde(skip_serializing_if = "is_zero")]
    pub bytes: u64,
    /// Probe: what this helper can do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caps: Option<Caps>,
}

fn is_zero<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// What a helper can do on this machine, per format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Caps {
    /// HEIC (encode is false on Macs without a HEVC encoder).
    pub heic: FormatCaps,
    /// AVIF.
    pub avif: FormatCaps,
    /// The codec and its versions, for the log.
    pub codec: String,
}

impl Caps {
    /// What it can do with `format`.
    pub fn of(&self, format: HeifFormat) -> FormatCaps {
        match format {
            HeifFormat::Heic => self.heic,
            HeifFormat::Avif => self.avif,
        }
    }
}

/// What a helper can do with one format. The default is nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormatCaps {
    /// It can decode the format.
    pub decode: bool,
    /// It can encode the format.
    pub encode: bool,
}

/// Why the helper failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Failure {
    /// The kind, which picks the user message.
    pub kind: FailKind,
    /// For the log: never a path, a name or file contents.
    #[serde(default)]
    pub detail: String,
}

impl Failure {
    /// A failure of `kind` with a log detail.
    pub fn new(kind: FailKind, detail: impl Into<String>) -> Failure {
        Failure {
            kind,
            detail: detail.into(),
        }
    }
}

/// The kinds of failure a helper reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailKind {
    /// No HEIF `ftyp` box, or the codec says the bytes aren't HEIF.
    NotHeif,
    /// HEIF with a codec this helper lacks (AVC, JPEG, VVC, uncompressed),
    /// or a codec or profile the decoder can't do.
    Unsupported,
    /// The codec failed on bytes that looked like HEIC or AVIF (truncated,
    /// corrupt).
    Damaged,
    /// No primary image, or no images.
    NoImage,
    /// Over a limit: the source size, the pixels, the codec's own security
    /// or memory limits, or the encode size.
    TooLarge,
    /// This machine can't encode that format.
    NoEncoder,
    /// The encoder failed.
    Encode,
    /// The source or `input.rgba` can't be opened or read.
    Read,
    /// `pixels.rgba` or the encode output can't be created or written.
    Write,
    /// The request's protocol isn't [`crate::PROTOCOL`].
    Protocol,
    /// A bad request, invalid paths, a panic or inconsistent codec output.
    Internal,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn round_trip(value: &Message, json: &str) {
        assert_eq!(serde_json::to_string(value).unwrap(), json);
        assert_eq!(&serde_json::from_str::<Message>(json).unwrap(), value);
    }

    #[test]
    fn the_spec_lines_round_trip() {
        round_trip(
            &Message::Image(ImageInfo {
                width: 3024,
                height: 4032,
                channels: 3,
                icc_bytes: 536,
            }),
            r#"{"image":{"width":3024,"height":4032,"channels":3,"iccBytes":536}}"#,
        );
        round_trip(&Message::Progress(0.5), r#"{"progress":0.5}"#);
        round_trip(
            &Message::Done(Done {
                width: 3024,
                height: 4032,
                channels: 3,
                icc_bytes: 536,
                ..Done::default()
            }),
            r#"{"done":{"width":3024,"height":4032,"channels":3,"iccBytes":536}}"#,
        );
        round_trip(
            &Message::Done(Done {
                bytes: 1_843_322,
                ..Done::default()
            }),
            r#"{"done":{"bytes":1843322}}"#,
        );
        let both = FormatCaps {
            decode: true,
            encode: true,
        };
        round_trip(
            &Message::Done(Done {
                caps: Some(Caps {
                    heic: FormatCaps {
                        decode: true,
                        encode: false,
                    },
                    avif: FormatCaps {
                        decode: true,
                        encode: true,
                    },
                    codec: "ImageIO".to_string(),
                }),
                ..Done::default()
            }),
            r#"{"done":{"caps":{"heic":{"decode":true,"encode":false},"avif":{"decode":true,"encode":true},"codec":"ImageIO"}}}"#,
        );
        // avif-spec's Windows example (D41).
        round_trip(
            &Message::Done(Done {
                caps: Some(Caps {
                    heic: both,
                    avif: both,
                    codec: "libheif 1.23.7 (libde265 1.1.3, x265 4.3, dav1d 1.5.4, aom 3.15.2)"
                        .to_string(),
                }),
                ..Done::default()
            }),
            r#"{"done":{"caps":{"heic":{"decode":true,"encode":true},"avif":{"decode":true,"encode":true},"codec":"libheif 1.23.7 (libde265 1.1.3, x265 4.3, dav1d 1.5.4, aom 3.15.2)"}}}"#,
        );
        // A helper without AVIF.
        round_trip(
            &Message::Done(Done {
                caps: Some(Caps {
                    heic: both,
                    avif: FormatCaps::default(),
                    codec: "libheif 1.23.7 (libde265 1.1.3, x265 4.3)".to_string(),
                }),
                ..Done::default()
            }),
            r#"{"done":{"caps":{"heic":{"decode":true,"encode":true},"avif":{"decode":false,"encode":false},"codec":"libheif 1.23.7 (libde265 1.1.3, x265 4.3)"}}}"#,
        );
        round_trip(
            &Message::Error(Failure::new(
                FailKind::TooLarge,
                "14000x14000 is over 134217728 pixels",
            )),
            r#"{"error":{"kind":"tooLarge","detail":"14000x14000 is over 134217728 pixels"}}"#,
        );
        round_trip(
            &Message::Error(Failure::new(
                FailKind::NoEncoder,
                "public.heic isn't an image destination type",
            )),
            r#"{"error":{"kind":"noEncoder","detail":"public.heic isn't an image destination type"}}"#,
        );
    }

    #[test]
    fn caps_answer_per_format() {
        let caps = Caps {
            heic: FormatCaps {
                decode: true,
                encode: false,
            },
            avif: FormatCaps {
                decode: false,
                encode: true,
            },
            codec: String::new(),
        };
        assert_eq!(caps.of(HeifFormat::Heic), caps.heic);
        assert_eq!(caps.of(HeifFormat::Avif), caps.avif);
        assert_eq!(
            FormatCaps::default(),
            FormatCaps {
                decode: false,
                encode: false
            }
        );
    }

    #[test]
    fn a_decode_without_icc_leaves_icc_bytes_out() {
        round_trip(
            &Message::Done(Done {
                width: 3,
                height: 5,
                channels: 4,
                ..Done::default()
            }),
            r#"{"done":{"width":3,"height":5,"channels":4}}"#,
        );
        round_trip(&Message::Done(Done::default()), r#"{"done":{}}"#);
    }

    #[test]
    fn every_kind_has_its_camel_case_name() {
        let names = [
            (FailKind::NotHeif, "notHeif"),
            (FailKind::Unsupported, "unsupported"),
            (FailKind::Damaged, "damaged"),
            (FailKind::NoImage, "noImage"),
            (FailKind::TooLarge, "tooLarge"),
            (FailKind::NoEncoder, "noEncoder"),
            (FailKind::Encode, "encode"),
            (FailKind::Read, "read"),
            (FailKind::Write, "write"),
            (FailKind::Protocol, "protocol"),
            (FailKind::Internal, "internal"),
        ];
        for (kind, name) in names {
            round_trip(
                &Message::Error(Failure::new(kind, "")),
                &format!(r#"{{"error":{{"kind":"{name}","detail":""}}}}"#),
            );
        }
    }

    #[test]
    fn a_missing_detail_reads_as_empty() {
        assert_eq!(
            serde_json::from_str::<Message>(r#"{"error":{"kind":"damaged"}}"#).unwrap(),
            Message::Error(Failure::new(FailKind::Damaged, ""))
        );
    }

    #[test]
    fn unknown_fields_and_messages_are_refused() {
        for line in [
            r#"{"image":{"width":1,"height":1,"channels":3,"iccBytes":0,"x":1}}"#,
            r#"{"image":{"width":1,"height":1,"channels":3}}"#,
            r#"{"done":{"x":1}}"#,
            r#"{"done":{"bytes":1,"path":"/a"}}"#,
            r#"{"done":{"caps":{"heic":{"decode":true,"encode":true},"avif":{"decode":true,"encode":true},"codec":"c","x":1}}}"#,
            r#"{"done":{"caps":{"heic":{"decode":true,"encode":true,"x":1},"avif":{"decode":true,"encode":true},"codec":"c"}}}"#,
            r#"{"done":{"caps":{"heic":{"decode":true,"encode":true},"avif":{"decode":true,"encode":true}}}}"#,
            r#"{"done":{"caps":{"heic":{"decode":true,"encode":true},"codec":"c"}}}"#,
            r#"{"done":{"caps":{"heic":{"decode":true},"avif":{"decode":true,"encode":true},"codec":"c"}}}"#,
            r#"{"done":{"caps":{"heic":{"decode":true,"encode":true},"avif":{"decode":true,"encode":true},"heif":{"decode":true,"encode":true},"codec":"c"}}}"#,
            // Protocol 1's caps.
            r#"{"done":{"caps":{"decode":true,"encode":true,"codec":"c"}}}"#,
            r#"{"done":{"caps":{"decode":true,"encode":true}}}"#,
            r#"{"error":{"kind":"read","detail":"","x":1}}"#,
            r#"{"error":{"kind":"nope","detail":""}}"#,
            r#"{"error":{"kind":"Read","detail":""}}"#,
            r#"{"error":{"detail":""}}"#,
            r#"{"hello":1}"#,
            r#"{"pages":12}"#,
            r#"{"Done":{}}"#,
            r#"{"done":{},"error":{"kind":"read"}}"#,
            r#"{"progress":0.5,"x":1}"#,
            r#"{"progress":"half"}"#,
            r#"{}"#,
            r#""done""#,
            r#"[]"#,
            "",
            "running 1 test",
            "test heif::tests::fake_helper ... ok",
        ] {
            assert!(
                serde_json::from_str::<Message>(line).is_err(),
                "accepted {line}"
            );
        }
    }

    #[test]
    fn channels_must_fit_a_byte() {
        assert!(serde_json::from_str::<Message>(
            r#"{"image":{"width":1,"height":1,"channels":256,"iccBytes":0}}"#
        )
        .is_err());
    }
}
