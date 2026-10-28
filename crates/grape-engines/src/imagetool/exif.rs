//! EXIF, pure (D26): what an EXIF block holds, for Remove Metadata's
//! report, and the minimal EXIF it writes back when the picture needs one.
//!
//! [`walk`] reads a TIFF structure (the bytes after a JPEG's `Exif\0\0`,
//! a PNG `eXIf` chunk, a WebP `EXIF` chunk) in either byte order: IFD0,
//! the Exif IFD, the GPS IFD, the Interop IFD and IFD1. It never trusts an
//! offset: each is bounds-checked, an IFD is read once (a loop ends
//! there), at most [`MAX_IFDS`] IFDs and [`MAX_ENTRIES`] entries each, and
//! a MakerNote is never followed. What it can't read is reported as
//! [`MetaField::Other`]. It keeps no value but the three the minimal EXIF
//! needs: the orientation, the color space and the Interop index.

use std::collections::BTreeSet;

use crate::MetaField;

/// The most IFDs [`walk`] reads: IFD0, Exif, GPS, Interop, IFD1 and a few
/// more a broken file may chain.
pub(crate) const MAX_IFDS: usize = 8;

/// The most entries [`walk`] reads in one IFD; an IFD claiming more is
/// broken.
pub(crate) const MAX_ENTRIES: u16 = 1024;

/// The most bytes of an EXIF block a stripper reads: of a bigger one, only
/// this much is walked (its first IFD is at the start), and the rest is
/// reported as Other.
pub(crate) const MAX_EXIF: usize = 1 << 20;

/// What [`walk`] found in an EXIF block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Exif {
    /// The kinds of metadata it holds with a value (D26's table), sorted.
    pub(crate) fields: BTreeSet<MetaField>,
    /// IFD0's Orientation (0x0112), when it's 1 to 8.
    pub(crate) orientation: Option<u16>,
    /// The Exif IFD's ColorSpace (0xA001): 1 sRGB, 0xFFFF uncalibrated.
    pub(crate) color_space: Option<u16>,
    /// The Interop IFD's index (0x0001) is `R03`: Adobe RGB, the DCF way.
    pub(crate) interop_r03: bool,
}

impl Exif {
    /// Whether the picture is Adobe RGB the DCF way (ColorSpace
    /// uncalibrated and Interop `R03`), which only these tags say when
    /// there's no ICC profile.
    pub(crate) fn dcf_adobe_rgb(&self) -> bool {
        self.color_space == Some(0xFFFF) && self.interop_r03
    }
}

/// Which IFD an entry is in, for [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ifd {
    Zero,
    Exif,
    Gps,
    Interop,
    One,
}

/// Reads the TIFF structure `tiff`. A header it can't read gives
/// [`MetaField::Other`] alone; a broken IFD adds it.
pub(crate) fn walk(tiff: &[u8]) -> Exif {
    let mut exif = Exif::default();
    let Some((order, first)) = header(tiff) else {
        if !tiff.is_empty() {
            exif.fields.insert(MetaField::Other);
        }
        return exif;
    };
    let reader = Reader { tiff, order };
    // The value bytes looked at so far: at most the block's own size, as
    // when no two values share bytes. A crafted block can point a thousand
    // entries at one long run of NULs; past it, values count as there.
    let mut looked = 0usize;
    let mut queue = vec![(first, Ifd::Zero)];
    let mut visited = BTreeSet::new();
    while let Some((offset, kind)) = queue.pop() {
        if offset == 0 {
            continue;
        }
        if visited.len() >= MAX_IFDS || !visited.insert(offset) {
            exif.fields.insert(MetaField::Other);
            continue;
        }
        let Some(ifd) = reader.ifd(offset) else {
            exif.fields.insert(MetaField::Other);
            continue;
        };
        for entry in &ifd.entries {
            let value = reader.value(entry);
            match (kind, entry.tag) {
                (Ifd::Zero | Ifd::Exif, 0x8769) => queue.push((reader.pointer(entry), Ifd::Exif)),
                (Ifd::Zero, 0x8825) => queue.push((reader.pointer(entry), Ifd::Gps)),
                (Ifd::Zero | Ifd::Exif, 0xA005) => {
                    queue.push((reader.pointer(entry), Ifd::Interop));
                }
                (Ifd::Zero, 0x0112) => {
                    exif.orientation = reader.short(entry).filter(|o| (1..=8).contains(o));
                }
                (Ifd::Exif, 0xA001) => exif.color_space = reader.short(entry),
                (Ifd::Interop, 0x0001) => {
                    exif.interop_r03 = value.is_some_and(|v| v.starts_with(b"R03"));
                }
                _ => {}
            }
            let Some(field) = classify(kind, entry.tag) else {
                continue;
            };
            if exif.fields.contains(&field) {
                continue;
            }
            // A value it can't read is still there: it's reported.
            let empty = value.is_some_and(|v| {
                looked = looked.saturating_add(v.len());
                looked <= tiff.len() && is_empty(entry.tag, entry.kind, v)
            });
            if !empty {
                exif.fields.insert(field);
            }
        }
        if kind == Ifd::Zero {
            queue.push((ifd.next, Ifd::One));
        }
    }
    exif
}

/// The byte order and IFD0's offset of a TIFF header.
fn header(tiff: &[u8]) -> Option<(Order, u32)> {
    let order = match tiff.get(..4)? {
        [b'I', b'I', 42, 0] => Order::Little,
        [b'M', b'M', 0, 42] => Order::Big,
        _ => return None,
    };
    let first = order.u32(tiff.get(4..8)?);
    Some((order, first))
}

/// A TIFF byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Little,
    Big,
}

impl Order {
    fn u16(self, b: &[u8]) -> u16 {
        let b = [b[0], b[1]];
        match self {
            Order::Little => u16::from_le_bytes(b),
            Order::Big => u16::from_be_bytes(b),
        }
    }

    fn u32(self, b: &[u8]) -> u32 {
        let b = [b[0], b[1], b[2], b[3]];
        match self {
            Order::Little => u32::from_le_bytes(b),
            Order::Big => u32::from_be_bytes(b),
        }
    }
}

/// One IFD entry.
struct Entry {
    tag: u16,
    /// The TIFF field type (1 BYTE, 2 ASCII, 3 SHORT, …).
    kind: u16,
    count: u32,
    /// The 4 bytes after the count: the value itself, or its offset.
    field: [u8; 4],
}

/// One IFD: its entries and the next IFD's offset.
struct IfdData {
    entries: Vec<Entry>,
    next: u32,
}

struct Reader<'a> {
    tiff: &'a [u8],
    order: Order,
}

impl Reader<'_> {
    /// The IFD at `offset`, or `None` if it runs past the end or claims
    /// more than [`MAX_ENTRIES`] entries.
    fn ifd(&self, offset: u32) -> Option<IfdData> {
        let start = usize::try_from(offset).ok()?;
        let count = self.order.u16(self.tiff.get(start..start.checked_add(2)?)?);
        if count > MAX_ENTRIES {
            return None;
        }
        let body = start + 2;
        let end = body + usize::from(count) * 12;
        let bytes = self.tiff.get(body..end)?;
        let entries = bytes
            .as_chunks::<12>()
            .0
            .iter()
            .map(|e| Entry {
                tag: self.order.u16(&e[0..2]),
                kind: self.order.u16(&e[2..4]),
                count: self.order.u32(&e[4..8]),
                field: [e[8], e[9], e[10], e[11]],
            })
            .collect();
        // A missing next-IFD offset ends the chain.
        let next = self.tiff.get(end..end + 4).map_or(0, |b| self.order.u32(b));
        Some(IfdData { entries, next })
    }

    /// An entry's value bytes, or `None` when they run past the end or
    /// its type is unknown.
    fn value<'e>(&'e self, entry: &'e Entry) -> Option<&'e [u8]> {
        let size = usize::try_from(entry.count)
            .ok()?
            .checked_mul(type_size(entry.kind)?)?;
        if size <= 4 {
            return Some(&entry.field[..size]);
        }
        let start = usize::try_from(self.order.u32(&entry.field)).ok()?;
        self.tiff.get(start..start.checked_add(size)?)
    }

    /// An entry's offset, for a sub-IFD pointer.
    fn pointer(&self, entry: &Entry) -> u32 {
        self.order.u32(&entry.field)
    }

    /// An entry's one SHORT value.
    fn short(&self, entry: &Entry) -> Option<u16> {
        (entry.kind == 3 && entry.count == 1).then(|| self.order.u16(&entry.field))
    }
}

/// The size of one value of TIFF field type `kind`.
pub(super) fn type_size(kind: u16) -> Option<usize> {
    match kind {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

/// Whether a value says nothing: no bytes, or only NULs, and for text
/// (BYTE, ASCII, UNDEFINED) only NULs and spaces. UserComment's 8-byte
/// character code doesn't count.
pub(super) fn is_empty(tag: u16, kind: u16, value: &[u8]) -> bool {
    let value = if tag == 0x9286 {
        value.get(8..).unwrap_or_default()
    } else {
        value
    };
    let text = matches!(kind, 1 | 2 | 7);
    value.iter().all(|&b| b == 0 || (text && b == b' '))
}

/// The kind of metadata a tag in IFD `ifd` is (D26's table), or `None`
/// for a tag that only describes the picture or the file's structure,
/// which isn't reported.
pub(super) fn classify(ifd: Ifd, tag: u16) -> Option<MetaField> {
    use MetaField::*;
    if STRUCTURAL.contains(&tag) {
        return None;
    }
    Some(match (ifd, tag) {
        // The thumbnail's own IFD describes only the thumbnail.
        (Ifd::One, _) => Thumbnail,
        (Ifd::Interop, _) => return None,
        // A GPS IFD holding only its version says nothing.
        (Ifd::Gps, 0x0000) => return None,
        (Ifd::Gps, _) => Location,
        (_, 0x010F | 0x0110) => Camera,
        (_, 0x0131) => Software,
        (_, 0x0132 | 0x9010 | 0x9290) => ModDate,
        (_, 0x013B | 0x9C9D | 0xA430) => Author,
        (_, 0x8298) => Copyright,
        (_, 0x010E | 0x9C9C | 0x9286) => Comment,
        (_, 0x9C9B) => Title,
        (_, 0x9C9E) => Keywords,
        (_, 0x9C9F) => Subject,
        (_, 0x02BC) => Xmp,
        (_, 0x83BB | 0x8649) => Iptc,
        (_, 0x9003 | 0x9004 | 0x9011 | 0x9012 | 0x9291 | 0x9292) => CreationDate,
        (_, 0x927C | 0xA431..=0xA435) => Camera,
        _ => Other,
    })
}

/// Tags that describe the picture or the structure, in any IFD: never
/// reported. Orientation, resolution, the IFD pointers, the Exif and
/// Flashpix versions, the color space and pixel size, and TIFF's baseline
/// image tags (an EXIF in a PNG or WebP sometimes carries them).
const STRUCTURAL: [u16; 24] = [
    0x0100, 0x0101, 0x0102, 0x0103, 0x0106, 0x0112, 0x0115, 0x011A, 0x011B, 0x011C, 0x0128, 0x0211,
    0x0212, 0x0213, 0x0214, 0x8769, 0x8825, 0xA005, 0x9000, 0x9101, 0xA000, 0xA001, 0xA002, 0xA003,
];

/// Sets the Exif IFD's PixelXDimension and PixelYDimension (0xA002,
/// 0xA003) of `tiff` to `width` and `height`, in place, after Compress
/// scaled the picture. A tag that isn't there, or a SHORT that can't hold
/// the value, is left as it is.
pub(crate) fn patch_dimensions(tiff: &mut [u8], width: u32, height: u32) {
    let Some((order, first)) = header(tiff) else {
        return;
    };
    let reader = Reader { tiff, order };
    let Some(exif_ifd) = reader.ifd(first).and_then(|ifd| {
        ifd.entries
            .iter()
            .find(|e| e.tag == 0x8769)
            .map(|e| reader.pointer(e))
    }) else {
        return;
    };
    let Ok(start) = usize::try_from(exif_ifd) else {
        return;
    };
    let Some(count) = tiff.get(start..start + 2).map(|b| order.u16(b)) else {
        return;
    };
    for n in 0..usize::from(count.min(MAX_ENTRIES)) {
        let at = start + 2 + n * 12;
        let Some(entry) = tiff.get(at..at + 12) else {
            return;
        };
        let (tag, kind, count) = (
            order.u16(&entry[0..2]),
            order.u16(&entry[2..4]),
            order.u32(&entry[4..8]),
        );
        let value = match tag {
            0xA002 => width,
            0xA003 => height,
            _ => continue,
        };
        let field = &mut tiff[at + 8..at + 12];
        match (kind, count, order) {
            (3, 1, _) => {
                let Ok(v) = u16::try_from(value) else {
                    continue;
                };
                let b = match order {
                    Order::Little => v.to_le_bytes(),
                    Order::Big => v.to_be_bytes(),
                };
                field[..2].copy_from_slice(&b);
            }
            (4, 1, Order::Little) => field.copy_from_slice(&value.to_le_bytes()),
            (4, 1, Order::Big) => field.copy_from_slice(&value.to_be_bytes()),
            _ => {}
        }
    }
}

/// The EXIF Remove Metadata writes back (D26, U2), as a big-endian TIFF:
/// the Orientation when it isn't 1, and the DCF Adobe RGB tags
/// (ColorSpace uncalibrated, Interop `R03`) when `adobe_rgb`. `None` when
/// neither is needed.
pub(crate) fn minimal(orientation: Option<u16>, adobe_rgb: bool) -> Option<Vec<u8>> {
    let orientation = orientation.filter(|&o| o != 1);
    if orientation.is_none() && !adobe_rgb {
        return None;
    }
    let mut ifd0 = Vec::new();
    if let Some(o) = orientation {
        ifd0.push((0x0112, 3, 1, u32::from(o) << 16));
    }
    let ifd0_len = ifd_len(ifd0.len() + usize::from(adobe_rgb));
    let exif_at = 8 + ifd0_len;
    let interop_at = exif_at + ifd_len(2);
    if adobe_rgb {
        ifd0.push((0x8769, 4, 1, offset(exif_at)));
    }
    let mut out = b"MM\0\x2a\0\0\0\x08".to_vec();
    write_ifd(&mut out, &ifd0);
    if adobe_rgb {
        write_ifd(
            &mut out,
            &[
                (0xA001, 3, 1, 0xFFFF_0000),
                (0xA005, 4, 1, offset(interop_at)),
            ],
        );
        write_ifd(
            &mut out,
            &[
                (0x0001, 2, 4, u32::from_be_bytes(*b"R03\0")),
                (0x0002, 7, 4, u32::from_be_bytes(*b"0100")),
            ],
        );
    }
    Some(out)
}

/// The size of an IFD of `entries` entries.
fn ifd_len(entries: usize) -> usize {
    2 + entries * 12 + 4
}

/// A small offset as a TIFF LONG.
fn offset(at: usize) -> u32 {
    u32::try_from(at).unwrap_or(u32::MAX)
}

/// Writes an IFD of (tag, type, count, 4-byte field as a big-endian
/// u32) entries, already in tag order, with no next IFD.
fn write_ifd(out: &mut Vec<u8>, entries: &[(u16, u16, u32, u32)]) {
    let count = u16::try_from(entries.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    for &(tag, kind, count, field) in entries {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&kind.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&field.to_be_bytes());
    }
    out.extend_from_slice(&0u32.to_be_bytes());
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use MetaField::*;

    #[test]
    fn values_that_share_bytes_are_looked_at_once_in_all() {
        // `entries` Make entries, all pointing at one run of `len` NULs.
        let shared = |entries: u16, len: u32| {
            let at = 8 + 2 + 12 * u32::from(entries) + 4;
            let mut t = b"II*\0\x08\0\0\0".to_vec();
            t.extend_from_slice(&entries.to_le_bytes());
            for _ in 0..entries {
                t.extend_from_slice(&0x010Fu16.to_le_bytes());
                t.extend_from_slice(&2u16.to_le_bytes());
                t.extend_from_slice(&len.to_le_bytes());
                t.extend_from_slice(&at.to_le_bytes());
            }
            t.extend_from_slice(&0u32.to_le_bytes());
            t.resize(t.len() + len as usize, 0);
            t
        };
        // A walk that read each of 1,000 would read 10 MB: past the
        // block's own size, a value counts as there.
        assert_eq!(walk(&shared(1000, 10_000)).fields, [Camera].into());
        // Values that share no bytes always fit, and are read: empty.
        assert!(walk(&shared(1, 10_000)).fields.is_empty());
    }

    /// A value for [`tiff`]: inline (≤4 bytes) or stored after the IFDs.
    pub(crate) enum V<'a> {
        Short(u16),
        Long(u32),
        Ascii(&'a str),
        Bytes(u16, &'a [u8]),
        /// A pointer to IFD number n (0-based, in the order given).
        Ifd(usize),
    }

    /// A TIFF in `order` (b'I' or b'M') with these IFDs of (tag, value),
    /// IFD0 first; IFD0's next pointer goes to IFD `next` if given.
    pub(crate) fn tiff(order: u8, ifds: &[&[(u16, V)]], next: Option<usize>) -> Vec<u8> {
        let big = order == b'M';
        let u16b = |v: u16| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let u32b = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        // IFDs are laid out one after the other from offset 8, then data.
        let mut at = Vec::new();
        let mut pos = 8usize;
        for ifd in ifds {
            at.push(pos);
            pos += ifd_len(ifd.len());
        }
        let mut data = Vec::new();
        let mut out = Vec::new();
        out.extend_from_slice(if big { b"MM" } else { b"II" });
        out.extend_from_slice(&u16b(42));
        out.extend_from_slice(&u32b(8));
        for (n, ifd) in ifds.iter().enumerate() {
            out.extend_from_slice(&u16b(u16::try_from(ifd.len()).unwrap()));
            for (tag, value) in ifd.iter() {
                out.extend_from_slice(&u16b(*tag));
                let (kind, count, bytes): (u16, u32, Vec<u8>) = match value {
                    V::Short(v) => (3, 1, [u16b(*v).as_slice(), &[0, 0]].concat()),
                    V::Long(v) => (4, 1, u32b(*v).to_vec()),
                    V::Ifd(i) => (4, 1, u32b(u32::try_from(at[*i]).unwrap()).to_vec()),
                    V::Ascii(s) => {
                        let mut b = s.as_bytes().to_vec();
                        b.push(0);
                        (2, u32::try_from(b.len()).unwrap(), b)
                    }
                    V::Bytes(kind, b) => (*kind, u32::try_from(b.len()).unwrap(), b.to_vec()),
                };
                out.extend_from_slice(&u16b(kind));
                out.extend_from_slice(&u32b(count));
                if bytes.len() <= 4 {
                    let mut field = bytes.clone();
                    field.resize(4, 0);
                    out.extend_from_slice(&field);
                } else {
                    out.extend_from_slice(&u32b(u32::try_from(pos + data.len()).unwrap()));
                    data.extend_from_slice(&bytes);
                }
            }
            let link = if n == 0 { next.map_or(0, |i| at[i]) } else { 0 };
            out.extend_from_slice(&u32b(u32::try_from(link).unwrap()));
        }
        out.extend_from_slice(&data);
        out
    }

    /// A camera's EXIF: make, model, date, orientation 6, an Exif IFD with
    /// the original date, exposure and a MakerNote, GPS with a position, an
    /// Interop IFD and an IFD1 thumbnail.
    fn camera(order: u8) -> Vec<u8> {
        tiff(
            order,
            &[
                &[
                    (0x010F, V::Ascii("Canon")),
                    (0x0110, V::Ascii("EOS R5")),
                    (0x0112, V::Short(6)),
                    (0x011A, V::Bytes(5, &[0, 0, 0, 72, 0, 0, 0, 1])),
                    (0x0132, V::Ascii("2024:05:01 10:20:30")),
                    (0x8769, V::Ifd(1)),
                    (0x8825, V::Ifd(2)),
                ],
                &[
                    (0x829A, V::Bytes(5, &[0, 0, 0, 1, 0, 0, 0, 125])),
                    (0x9003, V::Ascii("2024:05:01 10:20:30")),
                    (0x927C, V::Bytes(7, b"makernote bytes")),
                    (0xA001, V::Short(1)),
                    (0xA005, V::Ifd(3)),
                ],
                &[
                    (0x0000, V::Bytes(1, &[2, 2, 0, 0])),
                    (0x0002, V::Bytes(5, &[0, 0, 0, 48, 0, 0, 0, 1])),
                ],
                &[(0x0001, V::Ascii("R98")), (0x0002, V::Bytes(7, b"0100"))],
                &[
                    (0x0103, V::Short(6)),
                    (0x0201, V::Long(4000)),
                    (0x0202, V::Long(1200)),
                ],
            ],
            Some(4),
        )
    }

    #[test]
    fn a_cameras_exif_reports_its_kinds_in_both_byte_orders() {
        for order in *b"IM" {
            let exif = walk(&camera(order));
            let want: BTreeSet<MetaField> =
                [Location, Camera, CreationDate, ModDate, Thumbnail, Other].into();
            assert_eq!(exif.fields, want, "{}", order as char);
            assert_eq!(exif.orientation, Some(6));
            assert_eq!(exif.color_space, Some(1));
            assert!(!exif.interop_r03 && !exif.dcf_adobe_rgb());
        }
    }

    #[test]
    fn ifd0_tags_land_in_their_fields() {
        for (tag, value, field) in [
            (0x0131, V::Ascii("Photos 9.0"), Some(Software)),
            (0x013B, V::Ascii("Zoë"), Some(Author)),
            (0x8298, V::Ascii("(c) Zoë"), Some(Copyright)),
            (0x010E, V::Ascii("Beach"), Some(Comment)),
            (0x9C9B, V::Bytes(1, &[b'T', 0, b'i', 0]), Some(Title)),
            (0x9C9C, V::Bytes(1, &[b'C', 0, 0, 0]), Some(Comment)),
            (0x9C9D, V::Bytes(1, &[b'A', 0, 0, 0]), Some(Author)),
            (0x9C9E, V::Bytes(1, &[b'K', 0, 0, 0]), Some(Keywords)),
            (0x9C9F, V::Bytes(1, &[b'S', 0, 0, 0]), Some(Subject)),
            (0x02BC, V::Bytes(1, b"<x:xmpmeta/>"), Some(Xmp)),
            (0x83BB, V::Bytes(7, b"\x1c\x02\x05ab"), Some(Iptc)),
            (0x8649, V::Bytes(7, b"8BIM\x04\x04"), Some(Iptc)),
            (0x4746, V::Short(5), Some(Other)),
            (0x0112, V::Short(3), None),
            (0x0128, V::Short(2), None),
            (0x0100, V::Long(4000), None),
            // Empty and blank values aren't reported.
            (0x0131, V::Ascii(""), None),
            (0x013B, V::Ascii("    "), None),
            (0x9C9B, V::Bytes(1, &[0, 0, 0x20, 0, 0, 0]), None),
        ] {
            let t = tiff(b'M', &[&[(tag, value)]], None);
            let want: BTreeSet<MetaField> = field.into_iter().collect();
            assert_eq!(walk(&t).fields, want, "{tag:#06x}");
        }
    }

    #[test]
    fn exif_ifd_tags_land_in_their_fields() {
        for (tag, value, field) in [
            (0x9004, V::Ascii("2024:05:01 10:20:30"), Some(CreationDate)),
            (0x9011, V::Ascii("+02:00"), Some(CreationDate)),
            (0x9010, V::Ascii("+02:00"), Some(ModDate)),
            (0xA431, V::Ascii("123456"), Some(Camera)),
            (0xA434, V::Ascii("RF 24-70"), Some(Camera)),
            (0xA430, V::Ascii("Zoë"), Some(Author)),
            (0x9286, V::Bytes(7, b"ASCII\0\0\0hello"), Some(Comment)),
            (0x9286, V::Bytes(7, b"ASCII\0\0\0     "), None),
            (0x9000, V::Bytes(7, b"0232"), None),
            (0xA002, V::Long(4000), None),
            (0x8827, V::Short(100), Some(Other)),
        ] {
            let t = tiff(b'I', &[&[(0x8769, V::Ifd(1))], &[(tag, value)]], None);
            let want: BTreeSet<MetaField> = field.into_iter().collect();
            assert_eq!(walk(&t).fields, want, "{tag:#06x}");
        }
    }

    #[test]
    fn a_gps_ifd_with_only_its_version_says_nothing() {
        let t = tiff(
            b'M',
            &[
                &[(0x8825, V::Ifd(1))],
                &[(0x0000, V::Bytes(1, &[2, 3, 0, 0]))],
            ],
            None,
        );
        assert!(walk(&t).fields.is_empty());
    }

    #[test]
    fn broken_exif_is_other_and_never_panics() {
        assert_eq!(walk(b"").fields, BTreeSet::new());
        for broken in [
            &b"Exif\0\0"[..],
            b"II*",
            b"II*\0\xff\xff\xff\xff",
            b"MM\0*\0\0\0\x08\x7f\xff",
            b"XX*\0\x08\0\0\0",
        ] {
            assert_eq!(walk(broken).fields, [Other].into(), "{broken:?}");
        }
        // An IFD that points back at itself, and IFD0's next at IFD0.
        let looped = tiff(
            b'I',
            &[&[(0x8769, V::Ifd(0)), (0x010F, V::Ascii("Nikon"))]],
            Some(0),
        );
        assert_eq!(walk(&looped).fields, [Camera, Other].into());
        // Over the entry cap.
        let mut big = b"II*\0\x08\0\0\0".to_vec();
        big.extend_from_slice(&(MAX_ENTRIES + 1).to_le_bytes());
        big.resize(big.len() + usize::from(MAX_ENTRIES + 1) * 12 + 4, 0);
        assert_eq!(walk(&big).fields, [Other].into());
        // A value past the end is reported by its tag; an unknown type too.
        let mut cut = tiff(b'I', &[&[(0x010F, V::Ascii("A long camera make"))]], None);
        cut.truncate(cut.len() - 5);
        assert_eq!(walk(&cut).fields, [Camera].into());
        let odd = tiff(b'I', &[&[(0x8298, V::Bytes(99, b"x"))]], None);
        assert_eq!(walk(&odd).fields, [Copyright].into());
        // A chain of more IFDs than the cap.
        let mut chain: Vec<&[(u16, V)]> = Vec::new();
        let links: Vec<[(u16, V); 1]> = (1..12).map(|i| [(0x8769, V::Ifd(i))]).collect();
        for link in &links {
            chain.push(link);
        }
        let last = [(0x010F, V::Ascii("Nikon"))];
        chain.push(&last);
        let deep = tiff(b'I', &chain, None);
        assert!(walk(&deep).fields.contains(&Other));
        // Every prefix of a real one.
        let real = camera(b'M');
        for n in 0..real.len() {
            let _ = walk(&real[..n]);
        }
    }

    #[test]
    fn the_dcf_adobe_rgb_signal_is_read() {
        let t = tiff(
            b'I',
            &[
                &[(0x8769, V::Ifd(1))],
                &[(0xA001, V::Short(0xFFFF)), (0xA005, V::Ifd(2))],
                &[(0x0001, V::Ascii("R03"))],
            ],
            None,
        );
        let exif = walk(&t);
        assert!(exif.dcf_adobe_rgb());
        assert!(exif.fields.is_empty(), "{:?}", exif.fields);
    }

    #[test]
    fn compress_patches_the_pixel_dimensions_in_place() {
        for order in *b"IM" {
            let mut t = tiff(
                order,
                &[
                    &[(0x8769, V::Ifd(1))],
                    &[(0xA002, V::Long(4000)), (0xA003, V::Short(3000))],
                ],
                None,
            );
            let len = t.len();
            patch_dimensions(&mut t, 2048, 1536);
            assert_eq!(t.len(), len);
            let r = Reader {
                tiff: &t,
                order: header(&t).unwrap().0,
            };
            let ifd = r.ifd(r.pointer(&r.ifd(8).unwrap().entries[0])).unwrap();
            assert_eq!(r.order.u32(&ifd.entries[0].field), 2048);
            assert_eq!(r.short(&ifd.entries[1]), Some(1536));
        }
        // No Exif IFD, or garbage: nothing happens.
        let mut plain = tiff(b'I', &[&[(0x010F, V::Ascii("x"))]], None);
        let before = plain.clone();
        patch_dimensions(&mut plain, 1, 1);
        assert_eq!(plain, before);
        let mut junk = b"II*\0\xff\xff\xff\xff".to_vec();
        patch_dimensions(&mut junk, 1, 1);
    }

    #[test]
    fn the_minimal_exif_holds_only_what_the_picture_needs() {
        assert_eq!(minimal(None, false), None);
        assert_eq!(minimal(Some(1), false), None);
        for o in 2..=8 {
            let t = minimal(Some(o), false).unwrap();
            assert_eq!(t.len(), 26);
            let exif = walk(&t);
            assert_eq!(exif.orientation, Some(o));
            assert!(exif.fields.is_empty());
        }
        let t = minimal(Some(6), true).unwrap();
        let exif = walk(&t);
        assert_eq!(exif.orientation, Some(6));
        assert!(exif.dcf_adobe_rgb());
        assert!(exif.fields.is_empty(), "{:?}", exif.fields);
        let exif = walk(&minimal(None, true).unwrap());
        assert_eq!(exif.orientation, None);
        assert!(exif.dcf_adobe_rgb());
    }
}
