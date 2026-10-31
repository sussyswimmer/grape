//! The request: one JSON value on the helper's stdin, then EOF.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::HeifFormat;

/// The job the helper is asked to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    /// The protocol version the parent speaks: [`crate::PROTOCOL`].
    pub protocol: u32,
    /// The empty, private folder the parent made (absolute): where the pixel
    /// files and the encode output go, under [`crate::names`].
    pub work_dir: PathBuf,
    /// What to do.
    pub op: Op,
    /// The limits to hold.
    pub limits: Limits,
}

/// One operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Op {
    /// Report what this helper can do: `done {caps}`.
    // A struct variant, not a unit one: serde reads an internally tagged
    // unit variant without checking for unknown keys.
    Probe {},
    /// Decode the primary image of `source` into `pixels.rgba`. The helper
    /// sniffs the format itself, as the parent did ([`crate::sniff`]).
    Decode {
        /// The HEIC or AVIF file (absolute), opened read-only.
        source: PathBuf,
    },
    /// Encode `input.rgba` into `out.heic` or `out.avif`
    /// ([`crate::names::output`]).
    Encode {
        /// What to write. Required: protocol 1 had no such field.
        format: HeifFormat,
        /// 1..=100. Absent: the helper's own default for the format. The
        /// codecs' scales aren't the same perceptually.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
    },
}

/// The limits the helper holds. None of them may be 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// The largest HEIC or AVIF source, in bytes.
    pub max_source_bytes: u64,
    /// The most pixels a decoded image may have.
    pub max_pixels: u64,
    /// The most pixels an image to encode may have (one HEVC level 6.2 or
    /// AV1 level 6 picture: both are 35,651,584).
    pub max_encode_pixels: u64,
    /// The longest side of an image to encode, in pixels: per format, see
    /// [`Limits::for_encode`].
    pub max_encode_side: u32,
    /// The largest ICC profile kept; a larger one is dropped.
    pub max_icc_bytes: u32,
    /// The decoder's total memory budget, in bytes (libheif's
    /// `max_total_memory`).
    pub max_decoder_memory: u64,
}

impl Limits {
    /// Grape's limits (D33), with HEIC's side cap.
    pub const DEFAULT: Limits = Limits {
        // 512 MiB: grape-engines' `image::MAX_SOURCE` (D16).
        max_source_bytes: 536_870_912,
        // 512 MiB of RGBA8: the image crate's default decode limit (D16).
        max_pixels: 134_217_728,
        // HEVC level 6.2's largest picture: Grape encodes one picture, not
        // a grid.
        max_encode_pixels: 35_651_584,
        // HEVC 6.2's: √(8 × 35,651,584).
        max_encode_side: 16_888,
        // 255 × 65,519: grape-engines' private `image::MAX_ICC`.
        max_icc_bytes: 16_707_345,
        // 1 GiB: grape-engines' `image::WORKING_SET_LIMIT`.
        max_decoder_memory: 1_073_741_824,
    };

    /// [`Limits::DEFAULT`] for an encode to `format` (D41): HEIC keeps
    /// 16,888 a side, AVIF gets AV1 level 6's MaxHSize, 16,384. Both keep
    /// 35,651,584 pixels, AV1 level 6's MaxPicSize as well (AV1 spec Annex
    /// A.3). There's no separate height cap at level 6's MaxVSize (8,704):
    /// a taller picture gets `seq_level_idx` 31, which every decoder Grape
    /// uses reads.
    pub const fn for_encode(format: HeifFormat) -> Limits {
        let max_encode_side = match format {
            HeifFormat::Heic => Limits::DEFAULT.max_encode_side,
            HeifFormat::Avif => 16_384,
        };
        Limits {
            max_encode_side,
            ..Limits::DEFAULT
        }
    }
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const LIMITS: &str = r#""limits":{"maxSourceBytes":536870912,"maxPixels":134217728,"maxEncodePixels":35651584,"maxEncodeSide":16888,"maxIccBytes":16707345,"maxDecoderMemory":1073741824}"#;

    fn request(op: Op) -> Request {
        Request {
            protocol: 2,
            work_dir: PathBuf::from("/t/grape-heif-1"),
            op,
            limits: Limits::DEFAULT,
        }
    }

    fn line(op: &str) -> String {
        format!(r#"{{"protocol":2,"workDir":"/t/grape-heif-1","op":{op},{LIMITS}}}"#)
    }

    fn round_trip(value: &Request, json: &str) {
        assert_eq!(serde_json::to_string(value).unwrap(), json);
        assert_eq!(&serde_json::from_str::<Request>(json).unwrap(), value);
    }

    #[test]
    fn the_spec_requests_round_trip() {
        round_trip(
            &request(Op::Decode {
                source: PathBuf::from("/in/IMG_0001.HEIC"),
            }),
            &line(r#"{"kind":"decode","source":"/in/IMG_0001.HEIC"}"#),
        );
        round_trip(
            &request(Op::Decode {
                source: PathBuf::from("/in/photo.avif"),
            }),
            &line(r#"{"kind":"decode","source":"/in/photo.avif"}"#),
        );
        round_trip(
            &request(Op::Encode {
                format: HeifFormat::Avif,
                quality: None,
            }),
            &line(r#"{"kind":"encode","format":"avif"}"#),
        );
        round_trip(
            &request(Op::Encode {
                format: HeifFormat::Heic,
                quality: None,
            }),
            &line(r#"{"kind":"encode","format":"heic"}"#),
        );
        round_trip(
            &request(Op::Encode {
                format: HeifFormat::Heic,
                quality: Some(70),
            }),
            &line(r#"{"kind":"encode","format":"heic","quality":70}"#),
        );
        round_trip(&request(Op::Probe {}), &line(r#"{"kind":"probe"}"#));
    }

    #[test]
    fn the_spec_lines_are_exact() {
        // heic-spec §3.2's first line at protocol 2, and avif-spec's (D41)
        // three, verbatim with their limits written out.
        let limits = r#""limits":{"maxSourceBytes":536870912,"maxPixels":134217728,"maxEncodePixels":35651584,"maxEncodeSide":16888,"maxIccBytes":16707345,"maxDecoderMemory":1073741824}"#;
        for (json, op) in [
            (
                format!(
                    r#"{{"protocol":2,"workDir":"/t/grape-heif-1","op":{{"kind":"decode","source":"/in/IMG_0001.HEIC"}},{limits}}}"#
                ),
                Op::Decode {
                    source: PathBuf::from("/in/IMG_0001.HEIC"),
                },
            ),
            (
                format!(
                    r#"{{"protocol":2,"workDir":"/t/grape-heif-1","op":{{"kind":"encode","format":"avif"}},{limits}}}"#
                ),
                Op::Encode {
                    format: HeifFormat::Avif,
                    quality: None,
                },
            ),
            (
                format!(
                    r#"{{"protocol":2,"workDir":"/t/grape-heif-1","op":{{"kind":"encode","format":"heic","quality":70}},{limits}}}"#
                ),
                Op::Encode {
                    format: HeifFormat::Heic,
                    quality: Some(70),
                },
            ),
            (
                format!(
                    r#"{{"protocol":2,"workDir":"/t/grape-heif-1","op":{{"kind":"decode","source":"/in/photo.avif"}},{limits}}}"#
                ),
                Op::Decode {
                    source: PathBuf::from("/in/photo.avif"),
                },
            ),
        ] {
            round_trip(&request(op), &json);
        }
    }

    #[test]
    fn unknown_fields_are_refused_at_every_level() {
        for op in [
            r#"{"kind":"probe","x":1}"#,
            r#"{"kind":"decode","source":"/a","x":1}"#,
            r#"{"kind":"decode","source":"/a","format":"avif"}"#,
            r#"{"kind":"encode","format":"heic","x":1}"#,
            r#"{"kind":"encode","format":"avif","quality":70,"x":1}"#,
            r#"{"kind":"resize"}"#,
            r#"{"kind":"Probe"}"#,
            r#"{"source":"/a"}"#,
            r#"{"kind":"decode"}"#,
            r#""probe""#,
        ] {
            assert!(
                serde_json::from_str::<Request>(&line(op)).is_err(),
                "accepted op {op}"
            );
        }
        let extra_top = line(r#"{"kind":"probe"}"#).replacen('{', r#"{"x":1,"#, 1);
        assert!(serde_json::from_str::<Request>(&extra_top).is_err());
        let extra_limit = line(r#"{"kind":"probe"}"#).replace(
            r#""maxDecoderMemory":1073741824"#,
            r#""maxDecoderMemory":1073741824,"maxThreads":4"#,
        );
        assert!(serde_json::from_str::<Request>(&extra_limit).is_err());
        let missing_limit = line(r#"{"kind":"probe"}"#).replace(r#","maxIccBytes":16707345"#, "");
        assert!(serde_json::from_str::<Request>(&missing_limit).is_err());
        let snake = line(r#"{"kind":"probe"}"#).replace("workDir", "work_dir");
        assert!(serde_json::from_str::<Request>(&snake).is_err());
    }

    #[test]
    fn an_encode_must_name_a_format_it_knows() {
        // Protocol 1's encodes had no format: refused, not taken as HEIC.
        for op in [
            r#"{"kind":"encode"}"#,
            r#"{"kind":"encode","quality":70}"#,
            r#"{"kind":"encode","format":null}"#,
            r#"{"kind":"encode","format":"AVIF"}"#,
            r#"{"kind":"encode","format":"heif"}"#,
            r#"{"kind":"encode","format":"png"}"#,
            r#"{"kind":"encode","format":1}"#,
        ] {
            assert!(
                serde_json::from_str::<Request>(&line(op)).is_err(),
                "accepted op {op}"
            );
        }
    }

    #[test]
    fn quality_must_fit_a_byte() {
        let encode = |quality: &str| {
            line(&format!(
                r#"{{"kind":"encode","format":"heic","quality":{quality}}}"#
            ))
        };
        assert!(serde_json::from_str::<Request>(&encode("256")).is_err());
        assert!(serde_json::from_str::<Request>(&encode("-1")).is_err());
        // 0 and 101 parse; serve refuses them.
        assert!(serde_json::from_str::<Request>(&encode("0")).is_ok());
    }

    #[test]
    fn the_default_limits_are_the_spec_values() {
        assert_eq!(Limits::default(), Limits::DEFAULT);
        assert_eq!(Limits::DEFAULT.max_source_bytes, 512 << 20);
        assert_eq!(Limits::DEFAULT.max_pixels, (512 << 20) / 4);
        assert_eq!(Limits::DEFAULT.max_encode_pixels, 35_651_584);
        assert_eq!(Limits::DEFAULT.max_encode_side, 16_888);
        assert_eq!(Limits::DEFAULT.max_icc_bytes, 255 * 65_519);
        assert_eq!(Limits::DEFAULT.max_decoder_memory, 1 << 30);
    }

    #[test]
    fn each_format_has_its_encode_caps() {
        assert_eq!(Limits::for_encode(HeifFormat::Heic), Limits::DEFAULT);
        let avif = Limits::for_encode(HeifFormat::Avif);
        assert_eq!(avif.max_encode_side, 16_384);
        assert_eq!(
            avif,
            Limits {
                max_encode_side: 16_384,
                ..Limits::DEFAULT
            }
        );
        // AV1 level 6's MaxPicSize is HEVC 6.2's MaxLumaPs.
        assert_eq!(avif.max_encode_pixels, 35_651_584);
    }
}
