//! The two HEIF formats the helper reads and writes.

use serde::{Deserialize, Serialize};

/// HEIC or AVIF: what `encode` writes, what `decode` was handed (by
/// [`sniff`](crate::sniff)), and what [`Caps`](crate::Caps) answers for.
/// Not named `Format`, so it never clashes with grape-core's in the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HeifFormat {
    /// HEIF with HEVC (`"heic"`).
    Heic,
    /// HEIF with AV1 (`"avif"`).
    Avif,
}

impl HeifFormat {
    /// Both, HEIC first.
    pub const ALL: [HeifFormat; 2] = [HeifFormat::Heic, HeifFormat::Avif];

    /// Its name in messages and details: "HEIC" or "AVIF".
    pub const fn name(self) -> &'static str {
        match self {
            HeifFormat::Heic => "HEIC",
            HeifFormat::Avif => "AVIF",
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn formats_are_lower_case_on_the_wire() {
        for (format, json, name) in [
            (HeifFormat::Heic, r#""heic""#, "HEIC"),
            (HeifFormat::Avif, r#""avif""#, "AVIF"),
        ] {
            assert_eq!(serde_json::to_string(&format).unwrap(), json);
            assert_eq!(serde_json::from_str::<HeifFormat>(json).unwrap(), format);
            assert_eq!(format.name(), name);
        }
        for json in [
            r#""HEIC""#,
            r#""Avif""#,
            r#""heif""#,
            r#""png""#,
            "0",
            r#"{}"#,
        ] {
            assert!(serde_json::from_str::<HeifFormat>(json).is_err(), "{json}");
        }
        assert_eq!(HeifFormat::ALL, [HeifFormat::Heic, HeifFormat::Avif]);
    }
}
