//! The request: one JSON value on the helper's stdin, then EOF.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The job the helper is asked to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    /// The protocol version the parent speaks: [`crate::PROTOCOL`].
    pub protocol: u32,
    /// The empty, private folder the parent made (absolute): where the pixel
    /// files and `out.heic` go, under [`crate::names`].
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
    /// Decode the primary image of `source` into `pixels.rgba`.
    Decode {
        /// The HEIC file (absolute), opened read-only.
        source: PathBuf,
    },
    /// Encode `input.rgba` into `out.heic`.
    Encode {
        /// 1..=100. Absent: the helper's own default (libheif 50, ImageIO
        /// 0.8). The two scales aren't the same perceptually.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
    },
}

/// The limits the helper holds. None of them may be 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// The largest HEIC source, in bytes.
    pub max_source_bytes: u64,
    /// The most pixels a decoded image may have.
    pub max_pixels: u64,
    /// The most pixels an image to encode may have (one HEVC picture).
    pub max_encode_pixels: u64,
    /// The longest side of an image to encode, in pixels.
    pub max_encode_side: u32,
    /// The largest ICC profile kept; a larger one is dropped.
    pub max_icc_bytes: u32,
    /// The decoder's total memory budget, in bytes (libheif's
    /// `max_total_memory`).
    pub max_decoder_memory: u64,
}

impl Limits {
    /// Grape's limits (D33).
    pub const DEFAULT: Limits = Limits {
        // 512 MiB: grape-engines' `image::MAX_SOURCE` (D16).
        max_source_bytes: 536_870_912,
        // 512 MiB of RGBA8: the image crate's default decode limit (D16).
        max_pixels: 134_217_728,
        // HEVC level 6.2's largest picture: Grape encodes one picture, not
        // a grid.
        max_encode_pixels: 35_651_584,
        max_encode_side: 16_888,
        // 255 × 65,519: grape-engines' private `image::MAX_ICC`.
        max_icc_bytes: 16_707_345,
        // 1 GiB: grape-engines' `image::WORKING_SET_LIMIT`.
        max_decoder_memory: 1_073_741_824,
    };
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
            protocol: 1,
            work_dir: PathBuf::from("/t/grape-heif-1"),
            op,
            limits: Limits::DEFAULT,
        }
    }

    fn line(op: &str) -> String {
        format!(r#"{{"protocol":1,"workDir":"/t/grape-heif-1","op":{op},{LIMITS}}}"#)
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
            &request(Op::Encode { quality: None }),
            &line(r#"{"kind":"encode"}"#),
        );
        round_trip(
            &request(Op::Encode { quality: Some(70) }),
            &line(r#"{"kind":"encode","quality":70}"#),
        );
        round_trip(&request(Op::Probe {}), &line(r#"{"kind":"probe"}"#));
    }

    #[test]
    fn the_first_spec_line_is_exact() {
        // heic-spec §3.2, verbatim.
        let json = r#"{"protocol":1,"workDir":"/t/grape-heif-1","op":{"kind":"decode","source":"/in/IMG_0001.HEIC"},"limits":{"maxSourceBytes":536870912,"maxPixels":134217728,"maxEncodePixels":35651584,"maxEncodeSide":16888,"maxIccBytes":16707345,"maxDecoderMemory":1073741824}}"#;
        let value = request(Op::Decode {
            source: PathBuf::from("/in/IMG_0001.HEIC"),
        });
        round_trip(&value, json);
    }

    #[test]
    fn unknown_fields_are_refused_at_every_level() {
        for op in [
            r#"{"kind":"probe","x":1}"#,
            r#"{"kind":"decode","source":"/a","x":1}"#,
            r#"{"kind":"encode","x":1}"#,
            r#"{"kind":"encode","quality":70,"x":1}"#,
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
    fn quality_must_fit_a_byte() {
        assert!(
            serde_json::from_str::<Request>(&line(r#"{"kind":"encode","quality":256}"#)).is_err()
        );
        assert!(
            serde_json::from_str::<Request>(&line(r#"{"kind":"encode","quality":-1}"#)).is_err()
        );
        // 0 and 101 parse; serve refuses them.
        assert!(serde_json::from_str::<Request>(&line(r#"{"kind":"encode","quality":0}"#)).is_ok());
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
}
