//! A subtitle file's bytes to text.
//!
//! - A BOM decides first: EF BB BF is UTF-8, FF FE UTF-16LE, FE FF
//!   UTF-16BE. Invalid sequences become U+FFFD; the BOM is not part of the
//!   text.
//! - Without a BOM, valid UTF-8 (which includes plain ASCII) is UTF-8, and
//!   anything else is Windows-1252, the encoding of most older subtitle
//!   files that aren't UTF-8. Every byte decodes to something there, so
//!   decoding never fails.
//! - Every U+0000 is removed, so a UTF-16 file that lost its BOM still
//!   reads as text when it is mostly ASCII.

use crate::CancelToken;

/// The most input decoded between cancel checks.
const CHECK_BYTES: usize = 1024 * 1024;

/// What Windows-1252 bytes 0x80 to 0x9F decode to, from the WHATWG
/// encoding standard's table. The five bytes the code page leaves
/// undefined decode to the C1 control of the same value, as browsers do.
const CP1252_80_9F: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
];

/// The encoding a subtitle file was read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// UTF-8 without a BOM, including plain ASCII.
    Utf8,
    /// UTF-8 after an EF BB BF byte order mark.
    Utf8Bom,
    /// UTF-16, little-endian, after an FF FE byte order mark.
    Utf16Le,
    /// UTF-16, big-endian, after an FE FF byte order mark.
    Utf16Be,
    /// No BOM and not valid UTF-8.
    Windows1252,
}

impl Encoding {
    /// The encoding's usual name, for the log.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 with BOM",
            Encoding::Utf16Le => "UTF-16LE",
            Encoding::Utf16Be => "UTF-16BE",
            Encoding::Windows1252 => "Windows-1252",
        }
    }
}

/// Decodes `bytes` as the module docs describe and says which encoding it
/// used. `None` only when `cancel` is set: it is checked before decoding
/// and at least every [`CHECK_BYTES`] of input after that.
pub(crate) fn decode(bytes: &[u8], cancel: &CancelToken) -> Option<(String, Encoding)> {
    if cancel.is_cancelled() {
        return None;
    }
    let (mut text, encoding) = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        (
            String::from_utf8_lossy(rest).into_owned(),
            Encoding::Utf8Bom,
        )
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (utf16(rest, u16::from_le_bytes, cancel)?, Encoding::Utf16Le)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (utf16(rest, u16::from_be_bytes, cancel)?, Encoding::Utf16Be)
    } else {
        match std::str::from_utf8(bytes) {
            Ok(text) => (text.to_owned(), Encoding::Utf8),
            Err(_) => (windows_1252(bytes, cancel)?, Encoding::Windows1252),
        }
    };
    text.retain(|c| c != '\0');
    Some((text, encoding))
}

/// Decodes UTF-16 whose code units `unit` reads from byte pairs. Unpaired
/// surrogates and an odd trailing byte become U+FFFD.
fn utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16, cancel: &CancelToken) -> Option<String> {
    let (pairs, odd_byte) = bytes.as_chunks::<2>();
    let units = pairs.iter().map(|&pair| unit(pair));
    // A char takes at most four bytes, so this many is at most
    // `CHECK_BYTES` of input.
    let check_chars = CHECK_BYTES / 4;
    let mut text = String::with_capacity(bytes.len());
    for (i, c) in char::decode_utf16(units).enumerate() {
        if i.is_multiple_of(check_chars) && cancel.is_cancelled() {
            return None;
        }
        text.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    if !odd_byte.is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    Some(text)
}

/// Decodes Windows-1252 as the WHATWG standard does: ASCII, then
/// [`CP1252_80_9F`], then 0xA0 to 0xFF as U+00A0 to U+00FF.
fn windows_1252(bytes: &[u8], cancel: &CancelToken) -> Option<String> {
    // Bytes from 0x80 take two or three bytes in UTF-8.
    let mut text = String::with_capacity(bytes.len() + bytes.len() / 2);
    for chunk in bytes.chunks(CHECK_BYTES) {
        if cancel.is_cancelled() {
            return None;
        }
        text.extend(chunk.iter().map(|&b| match b {
            0x80..=0x9F => CP1252_80_9F[usize::from(b - 0x80)],
            _ => char::from(b),
        }));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn decoded(bytes: &[u8]) -> (String, Encoding) {
        decode(bytes, &CancelToken::new()).unwrap()
    }

    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn utf16be(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_be_bytes).collect()
    }

    #[test]
    fn a_utf8_bom_is_detected_and_not_part_of_the_text() {
        let bytes = [b"\xEF\xBB\xBF".as_slice(), "1\nCafé".as_bytes()].concat();
        assert_eq!(decoded(&bytes), ("1\nCafé".into(), Encoding::Utf8Bom));
    }

    #[test]
    fn invalid_utf8_after_a_utf8_bom_becomes_replacement_characters() {
        assert_eq!(
            decoded(b"\xEF\xBB\xBFa\xFFb"),
            ("a\u{FFFD}b".into(), Encoding::Utf8Bom)
        );
    }

    #[test]
    fn utf16_boms_pick_the_byte_order() {
        let text = "1\r\nHello 漢字 😀";
        let le = [vec![0xFF, 0xFE], utf16le(text)].concat();
        let be = [vec![0xFE, 0xFF], utf16be(text)].concat();
        assert_eq!(decoded(&le), (text.into(), Encoding::Utf16Le));
        assert_eq!(decoded(&be), (text.into(), Encoding::Utf16Be));
    }

    #[test]
    fn unpaired_surrogates_and_an_odd_trailing_byte_become_replacement_characters() {
        // "a", a lone high surrogate, "b", a lone low surrogate, then half a unit.
        let le = [0xFF, 0xFE, b'a', 0, 0x3D, 0xD8, b'b', 0, 0x00, 0xDE, b'c'];
        assert_eq!(
            decoded(&le),
            ("a\u{FFFD}b\u{FFFD}\u{FFFD}".into(), Encoding::Utf16Le)
        );
        let be = [0xFE, 0xFF, 0, b'a', 0xD8, 0x3D, 0, b'b', b'c'];
        assert_eq!(
            decoded(&be),
            ("a\u{FFFD}b\u{FFFD}".into(), Encoding::Utf16Be)
        );
    }

    #[test]
    fn valid_utf8_without_a_bom_is_utf8() {
        assert_eq!(
            decoded(b"plain ascii"),
            ("plain ascii".into(), Encoding::Utf8)
        );
        assert_eq!(
            decoded("Grüße, 漢字".as_bytes()),
            ("Grüße, 漢字".into(), Encoding::Utf8)
        );
        assert_eq!(decoded(b""), (String::new(), Encoding::Utf8));
    }

    #[test]
    fn anything_else_without_a_bom_is_windows_1252() {
        assert_eq!(
            decoded(b"Caf\xE9 \x93quoted\x94 \x80 5"),
            ("Café “quoted” € 5".into(), Encoding::Windows1252)
        );
    }

    #[test]
    fn windows_1252_maps_0x80_to_0x9f_by_the_whatwg_table() {
        let bytes: Vec<u8> = (0x80..=0x9F).collect();
        // From the WHATWG index, with the five undefined bytes as C1 controls.
        let expected: String = [
            0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160,
            0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022,
            0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
        ]
        .into_iter()
        .map(|code| char::from_u32(code).unwrap())
        .collect();
        assert_eq!(decoded(&bytes), (expected, Encoding::Windows1252));
    }

    #[test]
    fn windows_1252_maps_ascii_as_is_and_0xa0_to_0xff_to_latin_1() {
        let bytes: Vec<u8> = (0x01..=0x7F).chain(0xA0..=0xFF).collect();
        let expected: String = bytes.iter().map(|&b| char::from(b)).collect();
        assert_eq!(decoded(&bytes), (expected, Encoding::Windows1252));
    }

    #[test]
    fn every_nul_is_removed() {
        // UTF-16LE without its BOM is valid UTF-8 full of NULs.
        assert_eq!(decoded(&utf16le("1\nHi")), ("1\nHi".into(), Encoding::Utf8));
        assert_eq!(
            decoded(b"\xEF\xBB\xBFa\0b"),
            ("ab".into(), Encoding::Utf8Bom)
        );
        let le = [vec![0xFF, 0xFE], utf16le("a\0b")].concat();
        assert_eq!(decoded(&le), ("ab".into(), Encoding::Utf16Le));
        assert_eq!(decoded(b"\xE9\0x"), ("éx".into(), Encoding::Windows1252));
    }

    #[test]
    fn encodings_have_their_usual_names() {
        let names = [
            Encoding::Utf8,
            Encoding::Utf8Bom,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Windows1252,
        ]
        .map(Encoding::name);
        assert_eq!(
            names,
            [
                "UTF-8",
                "UTF-8 with BOM",
                "UTF-16LE",
                "UTF-16BE",
                "Windows-1252"
            ]
        );
    }

    #[test]
    fn a_cancelled_token_returns_none_for_every_encoding() {
        let token = CancelToken::new();
        token.cancel();
        for bytes in [
            b"ascii".as_slice(),
            b"\xEF\xBB\xBFx",
            b"\xFF\xFEx\0",
            b"\xFE\xFF\0x",
            b"\xE9",
        ] {
            assert_eq!(decode(bytes, &token), None, "{bytes:?}");
        }
    }

    #[test]
    fn large_inputs_decode_across_cancel_checks() {
        // Past several check points in each loop.
        let text = "é".repeat(CHECK_BYTES);
        let le = [vec![0xFF, 0xFE], utf16le(&text)].concat();
        assert_eq!(decoded(&le), (text.clone(), Encoding::Utf16Le));
        let latin = vec![0xE9; 2 * CHECK_BYTES + 1];
        let (decoded_latin, encoding) = decoded(&latin);
        assert_eq!(encoding, Encoding::Windows1252);
        assert_eq!(decoded_latin.chars().count(), latin.len());
        assert!(decoded_latin.chars().all(|c| c == 'é'));
    }
}
