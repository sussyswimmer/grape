//! Telling HEIC and AVIF from other HEIF and from everything else by the
//! `ftyp` box. Shared by the parent, `grape-heif.exe` and the Mac worker, so
//! all three agree.

use crate::HeifFormat;

/// What the first bytes of a file say it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniff {
    /// HEIF with HEVC (HEIC), or HEIF that names no codec at all.
    Heic,
    /// HEIF with AV1 (AVIF).
    Avif,
    /// HEIF with another codec (AVC, JPEG, J2K, VVC, uncompressed).
    OtherHeif,
    /// Not HEIF at all (MP4 and MOV included).
    NotHeif,
}

impl Sniff {
    /// The format a helper decodes it as: `Heic` and `Avif` only.
    pub fn format(self) -> Option<HeifFormat> {
        match self {
            Sniff::Heic => Some(HeifFormat::Heic),
            Sniff::Avif => Some(HeifFormat::Avif),
            Sniff::OtherHeif | Sniff::NotHeif => None,
        }
    }
}

/// The HEVC brands.
const HEVC: [&[u8; 4]; 8] = [
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs",
];

/// The AV1 brands: AVIF's image and sequence brands, the intra-only one and
/// MIAF's AV1 Baseline and Advanced profiles.
const AV1: [&[u8; 4]; 5] = [b"avif", b"avis", b"avio", b"MA1B", b"MA1A"];

/// The major brands that make a file AVIF whatever follows.
const AV1_MAJOR: [&[u8; 4]; 2] = [b"avif", b"avis"];

/// Structural brands: with one of these as the major brand, a compatible
/// codec brand decides (HEVC first, then AV1), and no codec brand at all
/// makes the file HEIC, for the decoder to decide.
const STRUCTURAL: [&[u8; 4]; 4] = [b"mif1", b"msf1", b"miaf", b"MiHE"];

/// The brands of HEIF's other codecs.
const OTHER_CODEC: [&[u8; 4]; 8] = [
    b"avci", b"avcs", b"jpeg", b"jpgs", b"j2ki", b"j2is", b"vvic", b"vvis",
];

/// The other HEIF brands: a file with one of these that isn't HEIC or AVIF
/// by the rules above is [`Sniff::OtherHeif`].
const OTHER_HEIF: [&[u8; 4]; 14] = [
    b"mif1", b"msf1", b"miaf", b"avif", b"avis", b"avci", b"avcs", b"jpeg", b"jpgs", b"j2ki",
    b"j2is", b"vvic", b"vvis", b"unif",
];

/// Reads the `ftyp` box at the start of `head`, the first
/// [`SNIFF_BYTES`](crate::SNIFF_BYTES) bytes of a file (or fewer, for a
/// shorter file).
///
/// - The box starts at offset 0: a u32 BE size, then `ftyp`. Size 1 means a
///   u64 BE size at 8..16 and the brands start 8 bytes later; size 0 means
///   "to the end", read as `head.len()`. The box must be at least 16 bytes
///   (its header, the major brand and the minor version).
/// - The rules, in order (D41):
///   1. An HEVC major brand: [`Sniff::Heic`].
///   2. A major `avif` or `avis`: [`Sniff::Avif`].
///   3. A structural major brand (`mif1`, `msf1`, `miaf`, `MiHE`) with a
///      compatible HEVC brand: [`Sniff::Heic`], so HEVC wins a tie.
///   4. A structural major brand with a compatible AV1 brand (`avif`,
///      `avis`, `avio`, `MA1B`, `MA1A`) and no HEVC one: [`Sniff::Avif`].
///   5. A structural major brand with no codec brand at all:
///      [`Sniff::Heic`]. libheif writes a grid that way when it can't find
///      its first tile's brand, so the decoder decides what's inside.
///   6. Otherwise, the major or a compatible brand is a HEIF brand:
///      [`Sniff::OtherHeif`]. So a non-structural major brand such as
///      `isom` with a compatible `avif` (or `heic`) is never decoded.
/// - [`Sniff::NotHeif`]: everything else.
pub fn sniff(head: &[u8]) -> Sniff {
    let Some((major, compatible)) = brands(head) else {
        return Sniff::NotHeif;
    };
    let is = |set: &[&[u8; 4]], brand: &[u8; 4]| set.contains(&brand);
    let any_compatible = |set: &[&[u8; 4]]| compatible.iter().any(|b| is(set, b));
    let structural = is(&STRUCTURAL, major);
    if is(&HEVC, major) {
        Sniff::Heic
    } else if is(&AV1_MAJOR, major) {
        Sniff::Avif
    } else if structural && any_compatible(&HEVC) {
        Sniff::Heic
    } else if structural && any_compatible(&AV1) {
        Sniff::Avif
    } else if structural && !any_compatible(&OTHER_CODEC) {
        Sniff::Heic
    } else if is(&OTHER_HEIF, major) || any_compatible(&OTHER_HEIF) {
        Sniff::OtherHeif
    } else {
        Sniff::NotHeif
    }
}

/// The major brand and the compatible brands (whole brands only, up to the
/// box's end or `head`'s), or `None` when `head` doesn't start with an
/// `ftyp` box.
fn brands(head: &[u8]) -> Option<(&[u8; 4], &[[u8; 4]])> {
    if head.len() < 8 || &head[4..8] != b"ftyp" {
        return None;
    }
    let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    let (box_len, header_len): (u64, usize) = match size {
        0 => (head.len() as u64, 8),
        1 => {
            let large: [u8; 8] = head.get(8..16)?.try_into().ok()?;
            (u64::from_be_bytes(large), 16)
        }
        n => (u64::from(n), 8),
    };
    // The header, the major brand and the minor version.
    if box_len < header_len as u64 + 8 {
        return None;
    }
    let major = head.get(header_len..header_len + 4)?.try_into().ok()?;
    let end = usize::try_from(box_len).map_or(head.len(), |len| len.min(head.len()));
    let compatible = head.get(header_len + 8..end).unwrap_or_default();
    Some((major, compatible.as_chunks::<4>().0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `ftyp` box with a 32-bit size.
    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let size = 16 + 4 * compatible.len() as u32;
        let mut bytes = size.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(major);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        for brand in compatible {
            bytes.extend_from_slice(*brand);
        }
        bytes
    }

    /// The same box followed by the start of another (`meta`), as in a file.
    fn file(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let mut bytes = ftyp(major, compatible);
        bytes.extend_from_slice(&[0, 0, 0, 33]);
        bytes.extend_from_slice(b"meta");
        bytes.resize(bytes.len() + 25, 0);
        bytes
    }

    #[test]
    fn every_hevc_brand_is_heic_as_the_major_brand() {
        for brand in HEVC {
            assert_eq!(sniff(&file(brand, &[])), Sniff::Heic, "{brand:?}");
            assert_eq!(sniff(&file(brand, &[b"mif1"])), Sniff::Heic, "{brand:?}");
        }
    }

    #[test]
    fn a_structural_major_brand_needs_a_compatible_hevc_brand() {
        for major in STRUCTURAL {
            for hevc in HEVC {
                assert_eq!(
                    sniff(&file(major, &[b"mif1", hevc])),
                    Sniff::Heic,
                    "{major:?} {hevc:?}"
                );
            }
        }
        // An iPhone photo.
        assert_eq!(sniff(&file(b"heic", &[b"mif1", b"heic"])), Sniff::Heic);
        assert_eq!(sniff(&file(b"mif1", &[b"heic"])), Sniff::Heic);
        assert_eq!(sniff(&file(b"msf1", &[b"hevc"])), Sniff::Heic);
        assert_eq!(sniff(&file(b"MiHE", &[b"miaf", b"heix"])), Sniff::Heic);
    }

    #[test]
    fn structural_brands_alone_go_to_the_decoder() {
        // libheif 1.23 writes a grid of HEVC tiles as mif1 + [mif1, miaf]
        // when it can't find its first tile's brand.
        assert_eq!(sniff(&file(b"mif1", &[b"mif1", b"miaf"])), Sniff::Heic);
        assert_eq!(sniff(&file(b"mif1", &[b"mif1"])), Sniff::Heic);
        assert_eq!(sniff(&file(b"mif1", &[])), Sniff::Heic);
        assert_eq!(sniff(&file(b"miaf", &[b"mif1", b"unif"])), Sniff::Heic);
        assert_eq!(sniff(&file(b"msf1", &[b"isom"])), Sniff::Heic);
    }

    #[test]
    fn avif_is_told_apart() {
        // An AVIF photo, and libavif's and libheif's usual brands.
        assert_eq!(sniff(&file(b"avif", &[b"mif1", b"avif"])), Sniff::Avif);
        assert_eq!(
            sniff(&file(b"avif", &[b"avif", b"mif1", b"miaf", b"MA1B"])),
            Sniff::Avif
        );
        assert_eq!(sniff(&file(b"avis", &[b"avis", b"msf1"])), Sniff::Avif);
        assert_eq!(sniff(&file(b"avif", &[])), Sniff::Avif);
        // A major avif decides, even with HEVC only as a compatible brand.
        assert_eq!(sniff(&file(b"avif", &[b"heic"])), Sniff::Avif);
        // Rule 4: a structural major brand with an AV1 brand and no HEVC one.
        assert_eq!(sniff(&file(b"mif1", &[b"avif", b"miaf"])), Sniff::Avif);
        for major in STRUCTURAL {
            for av1 in AV1 {
                assert_eq!(
                    sniff(&file(major, &[b"mif1", av1])),
                    Sniff::Avif,
                    "{major:?} {av1:?}"
                );
            }
        }
        // An AV1 brand wins over another codec's under a structural one.
        assert_eq!(sniff(&file(b"mif1", &[b"avci", b"avif"])), Sniff::Avif);
        // Rule 3 first: HEVC wins a tie.
        assert_eq!(sniff(&file(b"mif1", &[b"avif", b"heic"])), Sniff::Heic);
        assert_eq!(sniff(&file(b"miaf", &[b"MA1A", b"hevc"])), Sniff::Heic);
        // A non-structural major brand with a compatible avif: not decoded,
        // as `isom` with a compatible `heic` isn't.
        assert_eq!(sniff(&file(b"isom", &[b"avif"])), Sniff::OtherHeif);
        assert_eq!(sniff(&file(b"isom", &[b"avis", b"mif1"])), Sniff::OtherHeif);
        // The other AV1 brands as the major one aren't AVIF's own.
        assert_eq!(sniff(&file(b"avio", &[b"mif1"])), Sniff::OtherHeif);
        assert_eq!(sniff(&file(b"MA1B", &[])), Sniff::NotHeif);
        assert_eq!(sniff(&file(b"isom", &[b"avio", b"MA1B"])), Sniff::NotHeif);
    }

    #[test]
    fn the_formats_are_heic_and_avif_only() {
        assert_eq!(Sniff::Heic.format(), Some(HeifFormat::Heic));
        assert_eq!(Sniff::Avif.format(), Some(HeifFormat::Avif));
        assert_eq!(Sniff::OtherHeif.format(), None);
        assert_eq!(Sniff::NotHeif.format(), None);
    }

    #[test]
    fn other_heif_is_told_apart() {
        for brand in OTHER_CODEC {
            assert_eq!(sniff(&file(brand, &[])), Sniff::OtherHeif, "{brand:?}");
            assert_eq!(
                sniff(&file(b"mif1", &[b"mif1", brand, b"miaf"])),
                Sniff::OtherHeif,
                "{brand:?}"
            );
        }
        for brand in OTHER_HEIF {
            assert_eq!(
                sniff(&file(b"isom", &[brand])),
                Sniff::OtherHeif,
                "{brand:?}"
            );
        }
        // MiHE alone names no codec, so the decoder decides.
        assert_eq!(sniff(&file(b"MiHE", &[])), Sniff::Heic);
    }

    #[test]
    fn mp4_mov_and_other_files_are_not_heif() {
        assert_eq!(
            sniff(&file(b"isom", &[b"isom", b"iso2", b"mp41"])),
            Sniff::NotHeif
        );
        assert_eq!(sniff(&file(b"qt  ", &[b"qt  "])), Sniff::NotHeif);
        assert_eq!(sniff(&file(b"mp42", &[b"mp42", b"isom"])), Sniff::NotHeif);
        assert_eq!(sniff(&file(b"isom", &[b"heic"])), Sniff::NotHeif);
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"), Sniff::NotHeif);
        assert_eq!(sniff(b"\xff\xd8\xff\xe0\0\x10JFIF\0"), Sniff::NotHeif);
        assert_eq!(sniff(b""), Sniff::NotHeif);
        // `ftyp` not at offset 4.
        let mut late = vec![0];
        late.extend_from_slice(&file(b"heic", &[]));
        assert_eq!(sniff(&late), Sniff::NotHeif);
        // Another box first.
        let mut wide = vec![0, 0, 0, 8];
        wide.extend_from_slice(b"wide");
        wide.extend_from_slice(&file(b"heic", &[]));
        assert_eq!(sniff(&wide), Sniff::NotHeif);
    }

    #[test]
    fn size_zero_runs_to_the_end_of_head() {
        let mut bytes = ftyp(b"mif1", &[b"miaf", b"heic"]);
        bytes[..4].copy_from_slice(&[0, 0, 0, 0]);
        assert_eq!(sniff(&bytes), Sniff::Heic);
        // A trailing partial brand is ignored.
        bytes.extend_from_slice(b"av");
        assert_eq!(sniff(&bytes), Sniff::Heic);
        let mut short = ftyp(b"heic", &[]);
        short[..4].copy_from_slice(&[0, 0, 0, 0]);
        assert_eq!(sniff(&short), Sniff::Heic);
        // Size 0 with fewer than 16 bytes: no minor version.
        assert_eq!(sniff(&short[..15]), Sniff::NotHeif);
    }

    #[test]
    fn size_one_reads_a_64_bit_size() {
        let mut bytes = vec![0, 0, 0, 1];
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(&32u64.to_be_bytes());
        bytes.extend_from_slice(b"mif1");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(b"miafheic");
        bytes.extend_from_slice(b"avif"); // past the box's end
                                          // Read at 8, the major brand would be the size's high bytes and
                                          // `mif1` only a compatible brand: OtherHeif.
        assert_eq!(sniff(&bytes), Sniff::Heic);
        // Too small for its header, the major brand and the minor version.
        let mut small = bytes.clone();
        small[8..16].copy_from_slice(&23u64.to_be_bytes());
        assert_eq!(sniff(&small), Sniff::NotHeif);
        // The 64-bit size cut off.
        assert_eq!(sniff(&bytes[..12]), Sniff::NotHeif);
        // A huge 64-bit size runs to the end of head.
        let mut huge = bytes.clone();
        huge[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
        huge[16..20].copy_from_slice(b"mif1");
        huge[28..32].copy_from_slice(b"avif");
        huge[32..36].copy_from_slice(b"hevc");
        assert_eq!(sniff(&huge), Sniff::Heic);
    }

    #[test]
    fn compatible_brands_stop_at_the_box_end() {
        let mut bytes = ftyp(b"mif1", &[b"avif"]);
        // A following box whose type happens to read as a brand: inside the
        // box, `heic` would win.
        bytes.extend_from_slice(b"heic");
        assert_eq!(sniff(&bytes), Sniff::Avif);
        let mut bytes = ftyp(b"mif1", &[b"avci"]);
        bytes.extend_from_slice(b"avif");
        assert_eq!(sniff(&bytes), Sniff::OtherHeif);
    }

    #[test]
    fn small_and_truncated_boxes() {
        // Declared smaller than 16.
        for size in [8u32, 12, 15] {
            let mut bytes = ftyp(b"heic", &[]);
            bytes[..4].copy_from_slice(&size.to_be_bytes());
            assert_eq!(sniff(&bytes), Sniff::NotHeif, "size {size}");
        }
        let full = file(b"mif1", &[b"avif", b"heic"]);
        // Cut inside the type or the major brand.
        for len in 0..12 {
            assert_eq!(sniff(&full[..len]), Sniff::NotHeif, "len {len}");
        }
        // The major brand is there but the compatible brands are cut: what
        // is there decides. A structural brand alone goes to the decoder.
        assert_eq!(sniff(&full[..12]), Sniff::Heic);
        assert_eq!(sniff(&full[..16]), Sniff::Heic);
        assert_eq!(sniff(&full[..20]), Sniff::Avif);
        // `heic` cut to `hei`.
        assert_eq!(sniff(&full[..23]), Sniff::Avif);
        assert_eq!(sniff(&full[..24]), Sniff::Heic);
        let heic = file(b"heic", &[]);
        assert_eq!(sniff(&heic[..12]), Sniff::Heic);
    }

    #[test]
    fn the_fixtures_avci_brand_file_is_other_heif() {
        // grape-heif-fixtures' `avci-brand.heic` (D41): 32 bytes, major
        // `avci`, compatible `mif1 avci`, then zeros.
        let mut bytes = ftyp(b"avci", &[b"mif1", b"avci"]);
        bytes.resize(32, 0);
        assert_eq!(sniff(&bytes), Sniff::OtherHeif);
        // The `avif-brand.heic` it replaces is AVIF now.
        let mut bytes = ftyp(b"avif", &[b"mif1", b"avif"]);
        bytes.resize(32, 0);
        assert_eq!(sniff(&bytes), Sniff::Avif);
    }
}
