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
//! - Input is decoded at most [`CHECK_BYTES`] at a time, UTF-8 cut only
//!   where a character starts, with a cancel check before each piece.

use super::Cancelled;

/// The most input decoded between cancel checks.
const CHECK_BYTES: usize = 1024 * 1024;

/// What Windows-1252 bytes 0x80 to 0x9F decode to, from the WHATWG
/// encoding standard's table. The five bytes the code page leaves
/// undefined decode to the C1 control of the same value, as browsers do.
/// HTML reads the numeric references `&#128;` to `&#159;` the same way.
pub(super) const CP1252_80_9F: [char; 32] = [
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
/// used. `None` only when `is_cancelled` says so: it is asked before
/// decoding and at least every [`CHECK_BYTES`] of input after that.
pub(crate) fn decode(
    bytes: &[u8],
    is_cancelled: &mut dyn FnMut() -> bool,
) -> Option<(String, Encoding)> {
    let decoded = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        utf8_lossy(rest, is_cancelled).map(|text| (text, Encoding::Utf8Bom))
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        utf16(rest, u16::from_le_bytes, is_cancelled).map(|text| (text, Encoding::Utf16Le))
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        utf16(rest, u16::from_be_bytes, is_cancelled).map(|text| (text, Encoding::Utf16Be))
    } else {
        match utf8(bytes, is_cancelled) {
            Ok(Some(text)) => Ok((text, Encoding::Utf8)),
            Ok(None) => windows_1252(bytes, is_cancelled).map(|text| (text, Encoding::Windows1252)),
            Err(Cancelled) => Err(Cancelled),
        }
    };
    decoded.ok()
}

/// `bytes` in pieces of at most [`CHECK_BYTES`], each cut just before a
/// byte that starts a character (or at the end), so each decodes alone
/// as it would within the whole. Even empty input is one (empty) piece.
fn pieces(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = Some(bytes);
    std::iter::from_fn(move || {
        let left = rest?;
        if left.len() <= CHECK_BYTES {
            rest = None;
            return Some(left);
        }
        // A character has at most three continuation bytes (10xxxxxx), so
        // one of these four starts one, unless the input is invalid there.
        let end = (CHECK_BYTES - 3..=CHECK_BYTES)
            .rev()
            .find(|&at| left[at] & 0xC0 != 0x80)
            .unwrap_or(CHECK_BYTES);
        let (piece, tail) = left.split_at(end);
        rest = Some(tail);
        Some(piece)
    })
}

/// Appends `piece` to `text` without its NULs.
fn push_without_nuls(text: &mut String, piece: &str) {
    for part in piece.split('\0') {
        text.push_str(part);
    }
}

/// Decodes valid UTF-8; `Ok(None)` if `bytes` isn't.
fn utf8(bytes: &[u8], is_cancelled: &mut dyn FnMut() -> bool) -> Result<Option<String>, Cancelled> {
    let mut text = String::with_capacity(bytes.len());
    for piece in pieces(bytes) {
        if is_cancelled() {
            return Err(Cancelled);
        }
        let Ok(piece) = std::str::from_utf8(piece) else {
            return Ok(None);
        };
        push_without_nuls(&mut text, piece);
    }
    Ok(Some(text))
}

/// Decodes UTF-8, with each invalid sequence as U+FFFD.
fn utf8_lossy(bytes: &[u8], is_cancelled: &mut dyn FnMut() -> bool) -> Result<String, Cancelled> {
    let mut text = String::with_capacity(bytes.len());
    for piece in pieces(bytes) {
        if is_cancelled() {
            return Err(Cancelled);
        }
        push_without_nuls(&mut text, &String::from_utf8_lossy(piece));
    }
    Ok(text)
}

/// Decodes UTF-16 whose code units `unit` reads from byte pairs. Unpaired
/// surrogates and an odd trailing byte become U+FFFD.
fn utf16(
    bytes: &[u8],
    unit: fn([u8; 2]) -> u16,
    is_cancelled: &mut dyn FnMut() -> bool,
) -> Result<String, Cancelled> {
    let (pairs, odd_byte) = bytes.as_chunks::<2>();
    let mut chars = char::decode_utf16(pairs.iter().map(|&pair| unit(pair)));
    // A char takes at most four bytes, so this many is at most
    // `CHECK_BYTES` of input.
    let check_chars = CHECK_BYTES / 4;
    let mut text = String::with_capacity(bytes.len());
    loop {
        if is_cancelled() {
            return Err(Cancelled);
        }
        let mut decoded = 0;
        for c in chars.by_ref().take(check_chars) {
            decoded += 1;
            match c {
                Ok('\0') => {}
                Ok(c) => text.push(c),
                Err(_) => text.push(char::REPLACEMENT_CHARACTER),
            }
        }
        if decoded < check_chars {
            break;
        }
    }
    if !odd_byte.is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    Ok(text)
}

/// Decodes Windows-1252 as the WHATWG standard does: ASCII, then
/// [`CP1252_80_9F`], then 0xA0 to 0xFF as U+00A0 to U+00FF.
fn windows_1252(bytes: &[u8], is_cancelled: &mut dyn FnMut() -> bool) -> Result<String, Cancelled> {
    // Bytes from 0x80 take two or three bytes in UTF-8.
    let mut text = String::with_capacity(bytes.len() + bytes.len() / 2);
    for chunk in bytes.chunks(CHECK_BYTES) {
        if is_cancelled() {
            return Err(Cancelled);
        }
        text.extend(chunk.iter().filter(|&&b| b != 0).map(|&b| match b {
            0x80..=0x9F => CP1252_80_9F[usize::from(b - 0x80)],
            _ => char::from(b),
        }));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn decoded(bytes: &[u8]) -> (String, Encoding) {
        decode(bytes, &mut || false).unwrap()
    }

    /// A check that finds the job cancelled from its second call on.
    fn cancelled_after_first() -> impl FnMut() -> bool {
        let mut calls = 0;
        move || {
            calls += 1;
            calls > 1
        }
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
    fn a_cancelled_job_returns_none_for_every_encoding() {
        for bytes in [
            b"".as_slice(),
            b"ascii",
            b"\xEF\xBB\xBF",
            b"\xEF\xBB\xBFx",
            b"\xFF\xFE",
            b"\xFF\xFEx\0",
            b"\xFE\xFF\0x",
            b"\xE9",
        ] {
            assert_eq!(decode(bytes, &mut || true), None, "{bytes:?}");
        }
    }

    #[test]
    fn large_inputs_decode_across_cancel_checks() {
        // Fixed sizes of a few MiB, whatever CHECK_BYTES is. Pieces of
        // UTF-8 are cut inside 2-, 3- and 4-byte characters, and beside an
        // invalid sequence, and must decode as the whole input does.
        let mut invalid = vec![b'x'; (1 << 20) - 2];
        invalid.extend_from_slice(b"\xE6\xBC\xFFy\xE6");
        let texts = [
            "é".repeat(1 << 20),
            "漢".repeat(1 << 20),
            format!("a{}", "😀".repeat(1 << 19)),
        ];
        for text in &texts {
            assert_eq!(decoded(text.as_bytes()), (text.clone(), Encoding::Utf8));
            let bom = [b"\xEF\xBB\xBF".as_slice(), text.as_bytes()].concat();
            assert_eq!(decoded(&bom), (text.clone(), Encoding::Utf8Bom));
            let le = [vec![0xFF, 0xFE], utf16le(text)].concat();
            assert_eq!(decoded(&le), (text.clone(), Encoding::Utf16Le));
        }
        let bom = [b"\xEF\xBB\xBF".as_slice(), &invalid].concat();
        let lossy = String::from_utf8_lossy(&invalid).into_owned();
        assert_eq!(decoded(&bom), (lossy, Encoding::Utf8Bom));
        let latin = vec![0xE9; 3 << 20];
        let (decoded_latin, encoding) = decoded(&latin);
        assert_eq!(encoding, Encoding::Windows1252);
        assert_eq!(decoded_latin.chars().count(), latin.len());
        assert!(decoded_latin.chars().all(|c| c == 'é'));
    }

    #[test]
    fn every_decoder_checks_again_within_a_mebibyte() {
        // Just over 1 MiB of input: a second check must come within it.
        let n = (1 << 20) + 4;
        let ascii = vec![b'a'; n];
        assert_eq!(utf8(&ascii, &mut cancelled_after_first()), Err(Cancelled));
        assert_eq!(
            utf8_lossy(&ascii, &mut cancelled_after_first()),
            Err(Cancelled)
        );
        assert_eq!(
            windows_1252(&vec![0xE9; n], &mut cancelled_after_first()),
            Err(Cancelled)
        );
        let units = [b'a', 0].repeat(n / 2);
        assert_eq!(
            utf16(&units, u16::from_le_bytes, &mut cancelled_after_first()),
            Err(Cancelled)
        );
        // Not cancelled, each check is a call: none is skipped.
        let mut calls = 0;
        utf8(&ascii, &mut || {
            calls += 1;
            false
        })
        .unwrap();
        assert_eq!(calls, 2);
    }
}
