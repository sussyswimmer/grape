//! Photoshop's image resources (34377) in a TIFF, for Remove Metadata
//! (D28): kept down to the ones that describe the picture, so product
//! photos keep their clipping paths.
//!
//! Each resource is a signature (`8BIM`, `MeSa`, `PHUT`, `AgHg` or
//! `DCSR`), a big-endian id, a Pascal name padded to even, a big-endian
//! size and its data padded to even.
//!
//! - **Kept:** resolution (1005), alpha channel names and ids (1006, 1045,
//!   1053), display info (1007, 1077), halftone (1013), transfer (1016),
//!   guides (1032), pixel aspect ratio (1064), paths (2000–2997), the
//!   clipping path's name (2999) and the origin path (3000).
//! - **Dropped and reported:** IPTC (1028), thumbnails (1033, 1036), the
//!   copyright flag (1034), the caption (1008), EXIF (1058, 1059, walked),
//!   XMP (1060), and everything else (URLs, slices, layer comps…) as
//!   Other. A resource whose data is blank goes unsaid.
//! - A block Grape can't read is dropped whole, as IPTC and Other.

use std::collections::BTreeSet;

use super::super::exif;
use crate::MetaField;

/// The most resources read from one block; one with more is dropped whole.
const MAX_RESOURCES: usize = 4_096;

/// What [`filter`] kept and found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Filtered {
    /// The resources kept, back to back, each padded to even; empty when
    /// none was.
    pub(super) kept: Vec<u8>,
    /// What the dropped ones held.
    pub(super) fields: BTreeSet<MetaField>,
}

/// Whether the resource `id` describes the picture, and stays.
fn kept(id: u16) -> bool {
    matches!(
        id,
        1005 | 1006 | 1007 | 1013 | 1016 | 1032 | 1045 | 1053 | 1064 | 1077 | 2000
            ..=2997 | 2999 | 3000
    )
}

/// What the dropped resource `id` holds, from its `data`; `None` for data
/// that's blank.
fn field(id: u16, data: &[u8]) -> Vec<MetaField> {
    use MetaField::*;
    if exif::is_empty(0, 7, data) {
        return Vec::new();
    }
    match id {
        1028 => vec![Iptc],
        1033 | 1036 => vec![Thumbnail],
        1034 => vec![Copyright],
        1008 => vec![Comment],
        1058 | 1059 => exif::walk(data).fields.into_iter().collect(),
        1060 => vec![Xmp],
        _ => vec![Other],
    }
}

/// Keeps the resources of `irb` that describe the picture ([module
/// docs](self)). They're walked twice, to size what's kept and then to
/// keep it, so it's held at its size and never grown.
pub(super) fn filter(irb: &[u8]) -> Filtered {
    let mut fields = BTreeSet::new();
    let mut size = 0;
    let read = each(irb, |id, whole, data| {
        if kept(id) {
            // Padded to even.
            size += whole.len() + data.len() % 2;
        } else {
            fields.extend(field(id, data));
        }
    });
    if !read {
        return Filtered {
            kept: Vec::new(),
            fields: [MetaField::Iptc, MetaField::Other].into(),
        };
    }
    let mut held = Vec::with_capacity(size);
    each(irb, |id, whole, data| {
        if kept(id) {
            held.extend_from_slice(whole);
            if data.len() % 2 == 1 {
                held.push(0);
            }
        }
    });
    Filtered { kept: held, fields }
}

/// Gives `f` each resource of `irb`: its id, its bytes and its data, both
/// without the padding; `false` when `irb` isn't resources Grape can read,
/// or holds more than [`MAX_RESOURCES`].
fn each(irb: &[u8], mut f: impl FnMut(u16, &[u8], &[u8])) -> bool {
    let mut rest = irb;
    let mut n = 0;
    // Trailing NULs pad the block.
    while !rest.iter().all(|&b| b == 0) {
        n += 1;
        let Some((id, end, size)) = resource(rest).filter(|_| n <= MAX_RESOURCES) else {
            return false;
        };
        let whole = &rest[..end];
        f(id, whole, &whole[end - size..]);
        rest = rest.get(end + size % 2..).unwrap_or_default();
    }
    true
}

/// The resource `irb` starts with: its id, where its data ends, and the
/// data's size; `None` when it isn't one.
fn resource(irb: &[u8]) -> Option<(u16, usize, usize)> {
    let signature = irb.get(..4)?;
    if ![b"8BIM", b"MeSa", b"PHUT", b"AgHg", b"DCSR"].contains(&signature.try_into().ok()?) {
        return None;
    }
    let id = u16::from_be_bytes([*irb.get(4)?, *irb.get(5)?]);
    // A Pascal name, its length byte included, padded to even.
    let name = 1 + usize::from(*irb.get(6)?);
    let at = 6 + name + name % 2;
    let size = irb.get(at..at + 4)?;
    let size = usize::try_from(u32::from_be_bytes([size[0], size[1], size[2], size[3]])).ok()?;
    let end = (at + 4).checked_add(size)?;
    (end <= irb.len()).then_some((id, end, size))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use MetaField::*;

    /// A resource: `8BIM`, `id`, the name `name`, and `data`, padded.
    pub(in super::super) fn res(id: u16, name: &str, data: &[u8]) -> Vec<u8> {
        let mut r = b"8BIM".to_vec();
        r.extend_from_slice(&id.to_be_bytes());
        r.push(name.len() as u8);
        r.extend_from_slice(name.as_bytes());
        if (1 + name.len()) % 2 == 1 {
            r.push(0);
        }
        r.extend_from_slice(&(data.len() as u32).to_be_bytes());
        r.extend_from_slice(data);
        if data.len() % 2 == 1 {
            r.push(0);
        }
        r
    }

    #[test]
    fn paths_and_the_clipping_path_stay_and_the_rest_is_said() {
        let path = res(2000, "Path 1", &[0; 26]);
        let clip = res(2999, "", b"\x06Path 1\0\0\0\0\0\0");
        let resolution = res(1005, "", &[0, 72, 0, 0, 0, 1, 0, 1]);
        let irb = [
            resolution.clone(),
            res(1028, "", b"\x1c\x02\x05\0\x05Grape"),
            path.clone(),
            res(1036, "", b"thumbnail"),
            res(1034, "", &[1]),
            res(1060, "", b"<x:xmpmeta/>"),
            res(1050, "slices", b"slices"),
            clip.clone(),
        ]
        .concat();
        let filtered = filter(&irb);
        assert_eq!(filtered.kept, [resolution, path, clip].concat());
        assert_eq!(
            filtered.fields,
            [Iptc, Thumbnail, Copyright, Xmp, Other].into()
        );
        // What's kept is kept again whole.
        let again = filter(&filtered.kept);
        assert_eq!(again.kept, filtered.kept);
        assert!(again.fields.is_empty());
    }

    #[test]
    fn exif_in_a_resource_is_walked_and_blank_data_goes_unsaid() {
        let exif = exif::tests::tiff(b'M', &[&[(0x010F, exif::tests::V::Ascii("Canon"))]], None);
        let irb = [
            res(1058, "", &exif),
            res(1034, "", &[0]),
            res(1008, "", b"  "),
        ]
        .concat();
        let filtered = filter(&irb);
        assert!(filtered.kept.is_empty());
        assert_eq!(filtered.fields, [Camera].into());
    }

    #[test]
    fn other_signatures_odd_sizes_and_trailing_nuls_are_read() {
        let mut irb = res(1006, "", b"Alpha 1");
        irb[..4].copy_from_slice(b"MeSa");
        let filtered = filter(&[irb.clone(), vec![0, 0]].concat());
        assert_eq!(filtered.kept, irb);
        // The last resource's padding may be missing; it's put back.
        let mut cut = res(1007, "", b"odd");
        cut.pop();
        assert_eq!(filter(&cut).kept, res(1007, "", b"odd"));
    }

    #[test]
    fn what_is_kept_is_held_at_its_size() {
        // An odd size, whose padding goes back, and enough resources that
        // a list grown as they're added would have room to spare.
        let odd = res(2000, "", &[7; 1001]);
        let many: Vec<u8> = (0..5).flat_map(|_| res(2001, "", &[1; 100])).collect();
        for irb in [odd.clone(), many, [odd, res(1028, "", b"x")].concat()] {
            let kept = filter(&irb).kept;
            assert!(!kept.is_empty());
            assert_eq!(kept.capacity(), kept.len());
        }
    }

    #[test]
    fn a_block_grape_cant_read_is_dropped_whole() {
        let good = res(2000, "", &[0; 26]);
        for broken in [
            [good.clone(), b"XXXX".to_vec()].concat(),
            good[..good.len() - 1].to_vec(),
            [good.clone(), b"8BIM\x03".to_vec()].concat(),
        ] {
            let filtered = filter(&broken);
            assert!(filtered.kept.is_empty());
            assert_eq!(filtered.fields, [Iptc, Other].into());
        }
        let many: Vec<u8> = (0..=MAX_RESOURCES)
            .flat_map(|_| res(2000, "", b""))
            .collect();
        assert_eq!(filter(&many).fields, [Iptc, Other].into());
        assert_eq!(filter(b""), Filtered::default());
    }
}
