//! Remove Metadata on TIFF (D28): every page and SubIFD written back
//! compact ([`write`](super::write)), their image blocks byte for byte, in
//! the source's byte order and kind, with only the tags that describe the
//! picture.
//!
//! - **Kept:** the layout, color, resolution and page tags ([`kept`]), ICC
//!   and Orientation among them; Photoshop's resources, filtered
//!   ([`irb`](super::irb)): a block that IFDs share is filtered once, and
//!   held and written once.
//! - **Dropped and reported:** every other tag ([`field`]), and the Exif
//!   and GPS IFDs, walked as [`exif::classify`] does (one that pages share,
//!   once). Padding, and values that are blank as far as they're read, go
//!   unsaid.
//! - **Written back:** the DCF Adobe RGB Exif (ColorSpace uncalibrated,
//!   Interop `R03`), when that's how an IFD with no ICC profile said its
//!   color: each such IFD gets its own, even when they shared one.
//! - **Refused:** DNG and old-style JPEG (both tools); metadata inside
//!   JPEG data (an APP1, APP13 or COM before a block's first scan, or in
//!   its JPEGTables, or segments that run past the first [`MAX_SCAN`]
//!   bytes read); ImageJ and OME files, whose layout is in their
//!   description.
//! - **Checked:** [`Plan::check`] reads the output again before it's
//!   published.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::super::exif;
use super::super::strip::{read_exact, Error, Found, In, Out};
use super::irb;
use super::read::{read_value, seek, Block, Entry, Ifd, Layout, Meta, Order, Tiff, Value};
use super::write::{self, Data, Dir, Field, Image};
use crate::{CancelToken, MetaField};

/// How much of a dropped value is read to see whether it's blank, or of a
/// description to see whether it's ImageJ's or OME's.
const MAX_JUDGED: u64 = 64 * 1024;

/// The most bytes of Photoshop's resources read.
const MAX_IRB: u64 = 16 << 20;

/// How much of each JPEG block (and of its tables) is read for segments.
const MAX_SCAN: u64 = 64 * 1024;

/// Why a file with metadata inside its JPEG data is refused.
const IN_DATA: &str = "has metadata inside its image data, which Grape can't remove yet.";

/// Why an ImageJ or OME file is refused.
const DESCRIBED: &str = "keeps its layout in its description, so Remove Metadata would break it.";

/// Whether `tag` is kept as it is: the picture's layout, color,
/// resolution and pages, with ICC and Orientation (D28's whitelist).
fn kept(tag: u16) -> bool {
    matches!(
        tag,
        // Layout.
        254..=259 | 262..=266 | 274 | 277 | 278 | 280 | 281 | 284 | 317 | 338..=342
        // Resolution and position; pages; fax and gray.
        | 282 | 283 | 286 | 287 | 296 | 297 | 290..=293
        // Color.
        | 301 | 318..=321 | 332..=334 | 336 | 529..=532 | 34675
        // The tile size, JPEG's tables.
        | 322 | 323 | 347
        // SGI, TIFF/EP's CFA, GDAL's no-data value.
        | 32995..=32998 | 33421 | 33422 | 42113
    )
}

/// Whether the writer makes `tag` afresh: the blocks and the SubIFDs.
fn rebuilt(tag: u16) -> bool {
    matches!(tag, 273 | 279 | 324 | 325 | 330)
}

/// What a dropped image-IFD tag holds, for the report; `None` for padding
/// and structure, which go unsaid.
fn field(tag: u16) -> Option<MetaField> {
    use MetaField::*;
    match tag {
        // DocumentName, PageName.
        269 | 285 => Some(Title),
        // GeoTIFF.
        33550 | 33922 | 34264 | 34735..=34737 => Some(Location),
        // Photoshop's layers.
        37724 => Some(Embedded),
        // FreeOffsets and FreeByteCounts, Microsoft's padding.
        288 | 289 | 59932 | 59933 => None,
        // ImageDescription, Make, Model, Software, DateTime, Artist,
        // Copyright, XMP, IPTC, the XP tags and TIFF/EP's camera tags;
        // Other for the rest.
        _ => exif::classify(exif::Ifd::Zero, tag),
    }
}

/// Remove Metadata's plan for one TIFF: what to write, and what goes.
#[derive(Debug)]
pub(crate) struct Plan {
    layout: Layout,
    /// The source's blocks listed more than once, sorted.
    shared: Vec<Block>,
    pages: Vec<Dir>,
    found: Found,
}

impl Plan {
    /// Walks the TIFF `r` (`len` bytes) and plans its rewrite, or refuses
    /// it ([module docs](self)); the walk refuses DNG and old-style JPEG.
    pub(crate) fn new(r: &mut dyn In, len: u64) -> Result<Plan, Error> {
        let tiff = Tiff::read(r, len)?;
        let order = tiff.layout.order;
        for ifd in tiff.ifds() {
            if let Some(entry) = ifd.get(270) {
                let text = head(r, entry, MAX_JUDGED)?.map(|(text, _)| text);
                let text = text.unwrap_or_default();
                if text.starts_with(b"ImageJ=") || text.windows(4).any(|w| w == b"<OME") {
                    return Err(Error::Refused(DESCRIBED.into()));
                }
            }
        }
        for ifd in tiff.ifds() {
            if ifd.uint(259, order) == Some(7) && jpeg_holds_metadata(r, ifd)? {
                return Err(Error::Refused(IN_DATA.into()));
            }
        }
        let shared = tiff.shared();
        let Tiff { layout, pages } = tiff;
        let mut walk = Walk {
            r,
            order,
            found: Found::default(),
            reported: HashSet::new(),
            irbs: HashMap::new(),
        };
        let pages = pages
            .into_iter()
            .map(|page| walk.dir(page))
            .collect::<Result<_, _>>()?;
        Ok(Plan {
            layout,
            shared,
            pages,
            found: walk.found,
        })
    }

    /// Writes the plan into `w`, copying from the source `r` it was made
    /// from; gives what went.
    pub(crate) fn write(
        &self,
        r: &mut dyn In,
        w: &mut dyn Out,
        cancel: &CancelToken,
    ) -> Result<Found, Error> {
        write::write(r, w, self.layout, &self.shared, &self.pages, cancel)?;
        Ok(self.found.clone())
    }

    /// Reads the output `r` (`len` bytes) again: it must have the same IFDs
    /// with the same block lengths, no tag but the kept ones (and the DCF
    /// Exif's), and nothing the report would name. [`Error::Broken`] says
    /// what's wrong.
    pub(crate) fn check(&self, r: &mut dyn In, len: u64) -> Result<(), Error> {
        let out = Tiff::read(r, len)?;
        let wrong = |why: String| Err(Error::Broken(format!("the output {why}")));
        if (out.layout.order, out.layout.big) != (self.layout.order, self.layout.big) {
            return wrong("has another byte order or kind".into());
        }
        if !same_shape(&self.pages, &out.pages) {
            return wrong("has other IFDs or blocks".into());
        }
        // Photoshop's resources that IFDs share are read once.
        let mut irbs = HashSet::new();
        for ifd in out.ifds() {
            let dcf = ifd.exif.as_ref().is_some_and(dcf_only);
            if let Some(entry) = ifd.entries.iter().find(|e| {
                !(kept(e.tag) || rebuilt(e.tag) || e.tag == 34377 || (e.tag == 34665 && dcf))
            }) {
                return wrong(format!("keeps tag {}", entry.tag));
            }
            if let Some(entry) = ifd.get(34377).filter(|e| irbs.insert(e.value)) {
                let irb = read_value(r, entry, MAX_IRB)?.unwrap_or_default();
                if irb::filter(&irb).kept != irb {
                    return wrong("keeps a Photoshop resource it drops".into());
                }
            }
        }
        // And the report finds nothing.
        let mut walk = Walk {
            r,
            order: out.layout.order,
            found: Found::default(),
            reported: HashSet::new(),
            irbs: HashMap::new(),
        };
        for page in out.pages {
            walk.dir(page)?;
        }
        if !walk.found.fields.is_empty() {
            return wrong(format!("still holds {:?}", walk.found.fields));
        }
        Ok(())
    }
}

/// Whether the IFDs `dirs` planned and `ifds` read are the same: the same
/// chains, each IFD with blocks of the same lengths in the same order.
fn same_shape(dirs: &[Dir], ifds: &[Ifd]) -> bool {
    dirs.len() == ifds.len()
        && dirs.iter().zip(ifds).all(|(dir, ifd)| {
            let Some(image) = &dir.image else {
                return false;
            };
            image.tiled == ifd.tiled
                && image
                    .blocks
                    .iter()
                    .map(|b| b.len)
                    .eq(ifd.blocks.iter().map(|b| b.len))
                && image.subs.len() == ifd.subs.len()
                && image
                    .subs
                    .iter()
                    .zip(&ifd.subs)
                    .all(|(a, b)| same_shape(a, b))
        })
}

/// Whether `exif` is the DCF Adobe RGB Exif Grape writes, and nothing
/// more.
fn dcf_only(exif: &Meta) -> bool {
    let tags = |meta: &Meta, allowed: &[u16]| {
        !meta.unread && meta.entries.iter().all(|e| allowed.contains(&e.tag))
    };
    tags(exif, &[0xA001, 0xA005]) && exif.interop.as_deref().is_some_and(|i| tags(i, &[1, 2]))
}

/// The Exif IFD that says a picture is Adobe RGB the DCF way: ColorSpace
/// uncalibrated, and an Interop IFD with `R03` (`exif::minimal`'s tags).
fn dcf(order: Order) -> Dir {
    // ASCII and UNDEFINED: a byte a value.
    let field = |tag, kind, value: &[u8]| Field {
        tag,
        kind,
        count: value.len() as u64,
        value: Data::Bytes(value.to_vec()),
    };
    let interop = Dir {
        fields: vec![field(1, 2, b"R03\0"), field(2, 7, b"0100")],
        image: None,
        children: Vec::new(),
    };
    Dir {
        fields: vec![Field {
            tag: 0xA001,
            kind: 3,
            count: 1,
            value: Data::Bytes(order.bytes(0xFFFF, 2)),
        }],
        image: None,
        children: vec![(0xA005, interop)],
    }
}

/// The first `max` bytes of `entry`'s value, and whether that's all of it;
/// `None` when it can't be read.
fn head(r: &mut dyn In, entry: &Entry, max: u64) -> Result<Option<(Vec<u8>, bool)>, Error> {
    match entry.value {
        Value::Inline { bytes, len } => Ok(Some((bytes[..usize::from(len)].to_vec(), true))),
        Value::At { offset, len } => {
            let mut bytes = vec![0; len.min(max) as usize];
            seek(r, offset)?;
            read_exact(r, &mut bytes)?;
            Ok(Some((bytes, len <= max)))
        }
        Value::Unread => Ok(None),
    }
}

/// Whether the JPEG data of `ifd` (compression 7) holds metadata, or may:
/// in its JPEGTables, or in a block before its first scan.
fn jpeg_holds_metadata(r: &mut dyn In, ifd: &Ifd) -> Result<bool, Error> {
    if let Some(tables) = ifd.get(347) {
        if let Some((tables, whole)) = head(r, tables, MAX_SCAN)? {
            if segments_hold_metadata(&tables, whole) {
                return Ok(true);
            }
        }
    }
    let mut buf = Vec::new();
    for block in ifd.blocks.iter().filter(|b| b.len > 0) {
        buf.resize(block.len.min(MAX_SCAN) as usize, 0);
        seek(r, block.offset)?;
        read_exact(r, &mut buf)?;
        if segments_hold_metadata(&buf, block.len <= MAX_SCAN) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the JPEG stream `jpeg` has an APP1, APP13 or COM segment before
/// its first scan, or may have: when `jpeg` is only the stream's first
/// bytes (not `whole`) and they end before its first scan, what follows
/// can't be seen. Anything that isn't a stream of segments says no: it's
/// copied as it is.
fn segments_hold_metadata(jpeg: &[u8], whole: bool) -> bool {
    let Some(mut rest) = jpeg.strip_prefix(&[0xFF, 0xD8]) else {
        return false;
    };
    loop {
        // A marker, after any fill bytes.
        let fill = rest.iter().take_while(|&&b| b == 0xFF).count();
        let Some(&marker) = rest.get(fill) else {
            return !whole;
        };
        if fill == 0 {
            return false;
        }
        rest = &rest[fill + 1..];
        match marker {
            0xE1 | 0xED | 0xFE => return true,
            0xDA | 0xD9 => return false,
            0x01 | 0xD0..=0xD7 => {}
            _ => {
                let Some(len) = rest.get(..2) else {
                    return !whole;
                };
                let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
                let Some(next) = rest.get(len..) else {
                    return !whole;
                };
                rest = next;
            }
        }
    }
}

/// The walk that plans each IFD and reports what goes.
struct Walk<'r> {
    r: &'r mut dyn In,
    order: Order,
    found: Found,
    /// The Exif, GPS and Interop IFDs reported, by offset: one that IFDs
    /// share is reported once.
    reported: HashSet<u64>,
    /// Photoshop's resources filtered, by where they are: a block that
    /// IFDs share is read and filtered once, and its field shared.
    irbs: HashMap<(u64, u64), Option<Field>>,
}

impl Walk<'_> {
    /// The image IFD `ifd` as it will be written; what its dropped tags,
    /// Exif and GPS IFDs held goes into the report.
    fn dir(&mut self, ifd: Ifd) -> Result<Dir, Error> {
        let mut fields = Vec::new();
        let mut tags = HashSet::new();
        for entry in &ifd.entries {
            let first = tags.insert(entry.tag);
            match entry.tag {
                // Made again by the writer, or walked below.
                tag if rebuilt(tag) => {}
                34665 | 34853 => {}
                34377 if first => fields.extend(self.irb(entry)?),
                tag if kept(tag) => {
                    if first {
                        let field = Field::copy(entry)
                            .ok_or_else(|| Error::Broken(format!("tag {tag} can't be read")))?;
                        fields.push(field);
                    }
                }
                tag => {
                    if let Some(found) = field(tag) {
                        self.report(found, entry)?;
                    }
                }
            }
        }
        let mut children = Vec::new();
        if let Some(exif) = &ifd.exif {
            self.meta(exif, exif::Ifd::Exif)?;
            if let Some(interop) = &exif.interop {
                self.meta(interop, exif::Ifd::Interop)?;
            }
            if ifd.get(34675).is_none() && self.adobe_rgb(exif)? {
                children.push((34665, dcf(self.order)));
            }
        }
        if let Some(gps) = &ifd.gps {
            self.meta(gps, exif::Ifd::Gps)?;
        }
        let subs = ifd
            .subs
            .into_iter()
            .map(|chain| chain.into_iter().map(|sub| self.dir(sub)).collect())
            .collect::<Result<_, _>>()?;
        Ok(Dir {
            fields,
            image: Some(Image {
                blocks: ifd.blocks,
                tiled: ifd.tiled,
                subs,
            }),
            children,
        })
    }

    /// Reports `found` for the dropped `entry`, unless its value is blank
    /// as far as it's read. One that can't be read is reported.
    fn report(&mut self, found: MetaField, entry: &Entry) -> Result<(), Error> {
        let blank = match head(self.r, entry, MAX_JUDGED)? {
            Some((value, true)) => exif::is_empty(entry.tag, entry.kind, &value),
            _ => false,
        };
        if !blank {
            self.found.fields.insert(found);
        }
        Ok(())
    }

    /// Reports what the Exif, GPS or Interop IFD `meta` holds, unless
    /// another IFD that shares it did.
    fn meta(&mut self, meta: &Meta, kind: exif::Ifd) -> Result<(), Error> {
        if meta.unread {
            self.found.fields.insert(MetaField::Other);
        }
        if !self.reported.insert(meta.at) {
            return Ok(());
        }
        for entry in &meta.entries {
            if let Some(found) = exif::classify(kind, entry.tag) {
                self.report(found, entry)?;
            }
        }
        Ok(())
    }

    /// Whether the Exif IFD `exif` says the picture is Adobe RGB the DCF
    /// way: ColorSpace uncalibrated, and Interop `R03`.
    fn adobe_rgb(&mut self, exif: &Meta) -> Result<bool, Error> {
        let uncalibrated = exif
            .entries
            .iter()
            .find(|e| e.tag == 0xA001)
            .is_some_and(|e| e.kind == 3 && e.uint(self.order) == Some(0xFFFF));
        let Some(index) = exif
            .interop
            .as_deref()
            .and_then(|i| i.entries.iter().find(|e| e.tag == 1))
        else {
            return Ok(false);
        };
        let r03 = head(self.r, index, 16)?.is_some_and(|(v, _)| v.starts_with(b"R03"));
        Ok(uncalibrated && r03)
    }

    /// Photoshop's resources, kept down to the ones that describe the
    /// picture; `None` when none is left. Resources that an earlier IFD
    /// points at too are the field it got.
    fn irb(&mut self, entry: &Entry) -> Result<Option<Field>, Error> {
        let Value::At { offset, len } = entry.value else {
            return self.filter_irb(entry);
        };
        if len > MAX_IRB {
            return Err(Error::TooLarge(format!(
                "{len} bytes of Photoshop resources"
            )));
        }
        if let Some(field) = self.irbs.get(&(offset, len)) {
            return Ok(field.clone());
        }
        let field = self.filter_irb(entry)?;
        self.irbs.insert((offset, len), field.clone());
        Ok(field)
    }

    /// Reads and filters the resources `entry` holds, for [`Walk::irb`].
    fn filter_irb(&mut self, entry: &Entry) -> Result<Option<Field>, Error> {
        let Some(irb) = read_value(self.r, entry, MAX_IRB)? else {
            // Resources that can't be read go whole, as a broken block does.
            self.found
                .fields
                .extend([MetaField::Iptc, MetaField::Other]);
            return Ok(None);
        };
        let filtered = irb::filter(&irb);
        self.found.fields.extend(filtered.fields);
        if filtered.kept.is_empty() {
            return Ok(None);
        }
        Ok(Some(Field {
            tag: 34377,
            kind: 7,
            count: filtered.kept.len() as u64,
            value: Data::Shared(Arc::new(filtered.kept)),
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeSet;
    use std::io::Cursor;

    use image::codecs::jpeg::JpegEncoder;
    use image::{GrayImage, ImageEncoder};

    use super::super::irb::tests::res;
    use super::super::read::Order;
    use super::super::tests::{gray, set_entry, shared_meta, Builder, Page, B, V};
    use super::*;
    use MetaField::*;

    fn plan(file: &[u8]) -> Result<Plan, Error> {
        Plan::new(&mut Cursor::new(file), file.len() as u64)
    }

    /// `file` stripped and checked: the output, and what went.
    fn strip(file: &[u8]) -> (Vec<u8>, BTreeSet<MetaField>) {
        let plan = plan(file).unwrap();
        let mut out = Cursor::new(Vec::new());
        let found = plan
            .write(&mut Cursor::new(file), &mut out, &CancelToken::new())
            .unwrap();
        let out = out.into_inner();
        plan.check(&mut Cursor::new(&out), out.len() as u64)
            .unwrap();
        (out, found.fields)
    }

    fn read(file: &[u8]) -> Tiff {
        Tiff::read(&mut Cursor::new(file), file.len() as u64).unwrap()
    }

    /// The value of `tag` in `ifd` of `file`.
    fn value(file: &[u8], ifd: &Ifd, tag: u16) -> Option<Vec<u8>> {
        let entry = ifd.get(tag)?;
        read_value(&mut Cursor::new(file), entry, 1 << 20).unwrap()
    }

    /// A Photoshop block: resolution, a path, a thumbnail and IPTC.
    fn photoshop() -> Vec<u8> {
        [
            res(1005, "", &[0, 72, 0, 0, 0, 1, 0, 1]),
            res(2000, "Path 1", &[7; 26]),
            res(1036, "", b"thumbnail of the secret"),
            res(1028, "", b"\x1c\x02\x05\0\x06secret"),
        ]
        .concat()
    }

    /// A scanner's or an editor's TIFF, full of metadata: two pages, a
    /// reduced-resolution SubIFD, Exif, Interop and GPS IFDs.
    fn tagged(order: u8, big: bool) -> Builder {
        let mut first = gray(4, 2, 1, vec![B::data(b"abcd"), B::data(b"efgh")]);
        first.tags.extend([
            (269, V::Ascii("secret document")),
            (270, V::Ascii("secret description")),
            (271, V::Ascii("secret make")),
            (272, V::Ascii("secret model")),
            (274, V::Short(vec![6])),
            (282, V::Rational(vec![(300, 1)])),
            (283, V::Rational(vec![(300, 1)])),
            (288, V::Long(vec![0])),
            (289, V::Long(vec![0])),
            (296, V::Short(vec![2])),
            (305, V::Ascii("secret software")),
            (306, V::Ascii("2024:05:01 10:20:30")),
            (315, V::Ascii("secret artist")),
            (700, V::Bytes(1, b"<x:xmpmeta>secret</x:xmpmeta>".to_vec())),
            (0x9C9B, V::Bytes(1, b"s\0e\0c\0r\0e\0t\0".to_vec())),
            (33432, V::Ascii("secret copyright")),
            // IPTC typed LONG, as Photoshop writes it.
            (
                33723,
                V::Bytes(4, b"\x1c\x02\x05\0\x06secret\0\0\0".to_vec()),
            ),
            (
                33550,
                V::Bytes(12, [0x3F, 0xF0, 0, 0, 0, 0, 0, 0].repeat(3)),
            ),
            (34377, V::Bytes(1, photoshop())),
            (34675, V::Bytes(7, b"an ICC profile".to_vec())),
            (34735, V::Short(vec![1, 1, 0, 0])),
            (37724, V::Bytes(7, b"secret layers".to_vec())),
            (50341, V::Bytes(7, b"PrintIM secret".to_vec())),
            (59932, V::Bytes(7, vec![0; 16])),
        ]);
        let mut reduced = gray(2, 1, 1, vec![B::data(b"ij")]);
        reduced
            .tags
            .extend([(254, V::Long(vec![1])), (271, V::Ascii("secret make"))]);
        first.subs = vec![vec![reduced]];
        first.exif = Some(vec![
            (0x9003, V::Ascii("2024:05:01 10:20:30")),
            (0x927C, V::Bytes(7, b"secret maker note".to_vec())),
            (0xA001, V::Short(vec![1])),
        ]);
        first.interop = Some(vec![(1, V::Ascii("R98"))]);
        first.gps = Some(vec![
            (0, V::Bytes(1, vec![2, 3, 0, 0])),
            (2, V::Rational(vec![(48, 1), (8, 1), (0, 1)])),
        ]);
        let mut second = Page {
            tags: vec![
                (256, V::Long(vec![32])),
                (257, V::Long(vec![16])),
                (258, V::Short(vec![8])),
                (262, V::Short(vec![1])),
                (285, V::Ascii("secret page")),
                // Blank: dropped, not said.
                (305, V::Ascii("        ")),
                (322, V::Short(vec![16])),
                (323, V::Short(vec![16])),
            ],
            blocks: vec![B::data(&[1; 256]), B::Empty],
            tiled: true,
            ..Page::default()
        };
        second.tags.sort_by_key(|t| t.0);
        Builder {
            reversed: true,
            tail: b"secret orphaned bytes".to_vec(),
            ..Builder::new(order, big, vec![first, second])
        }
    }

    #[test]
    fn every_page_and_subifd_stays_byte_for_byte_with_only_the_kept_tags() {
        for (order, big) in [(b'I', false), (b'M', false), (b'I', true), (b'M', true)] {
            let file = tagged(order, big).build();
            let (out, found) = strip(&file);
            let want: BTreeSet<MetaField> = [
                Location,
                Camera,
                Author,
                Title,
                Comment,
                Copyright,
                Software,
                CreationDate,
                ModDate,
                Xmp,
                Iptc,
                Thumbnail,
                Embedded,
                Other,
            ]
            .into();
            assert_eq!(found, want, "{} {big}", order as char);
            let (a, b) = (read(&file), read(&out));
            assert_eq!(
                (b.layout.order, b.layout.big),
                (a.layout.order, a.layout.big)
            );
            let (ia, ib) = (a.ifds(), b.ifds());
            assert_eq!(ia.len(), ib.len());
            for (x, y) in ia.iter().zip(&ib) {
                let bytes = |f: &[u8], ifd: &Ifd| -> Vec<Vec<u8>> {
                    let range = |b: &Block| b.offset as usize..(b.offset + b.len) as usize;
                    ifd.blocks.iter().map(|b| f[range(b)].to_vec()).collect()
                };
                assert_eq!(bytes(&file, x), bytes(&out, y));
                assert_eq!(x.tiled, y.tiled);
                for e in &y.entries {
                    assert!(kept(e.tag) || rebuilt(e.tag) || e.tag == 34377, "{}", e.tag);
                }
                assert!(y.exif.is_none() && y.gps.is_none());
            }
            let page = &b.pages[0];
            assert_eq!(page.uint(274, b.layout.order), Some(6));
            assert_eq!(value(&out, page, 34675).unwrap(), b"an ICC profile");
            assert_eq!(value(&out, page, 282), value(&file, &a.pages[0], 282));
            // Photoshop's block holds the resolution and the path.
            let irb = value(&out, page, 34377).unwrap();
            assert_eq!(
                irb,
                [&photoshop()[..12 + 8], &res(2000, "Path 1", &[7; 26])].concat()
            );
            assert_eq!(page.get(34377).unwrap().kind, 7);
            for secret in [
                &b"secret"[..],
                b"s\0e\0c",
                b"2024:05",
                b"PrintIM",
                b"\x3F\xF0",
            ] {
                assert!(
                    !out.windows(secret.len()).any(|w| w == secret),
                    "{secret:?} left"
                );
            }
        }
    }

    #[test]
    fn a_profile_every_page_points_at_is_kept_once() {
        // A BigTIFF of 120 pages whose ICC profiles are all the first
        // page's 64 KiB: the output holds it once, not 120 times.
        let profile: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8 + 1).collect();
        let pages = (0..120)
            .map(|n| {
                let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
                let icc = if n == 0 { profile.clone() } else { vec![0] };
                page.tags.push((34675, V::Bytes(7, icc)));
                page
            })
            .collect();
        let mut file = Builder::new(b'I', true, pages).build();
        let tiff = read(&file);
        let Value::At { offset, len } = tiff.pages[0].get(34675).unwrap().value else {
            panic!("an inline profile")
        };
        for page in &tiff.pages[1..] {
            set_entry(&mut file, page.at, 34675, len, offset);
        }
        let (out, found) = strip(&file);
        assert!(found.is_empty());
        assert!(out.len() < 2 * profile.len(), "{}", out.len());
        let back = read(&out);
        assert_eq!(back.pages.len(), 120);
        for page in &back.pages {
            assert_eq!(value(&out, page, 34675).unwrap(), profile);
        }
    }

    #[test]
    fn photoshop_resources_pages_share_are_filtered_and_written_once() {
        // Three pages whose resources are the first page's: a 64 KiB path
        // and IPTC, most of a 66 KB file.
        let path = res(2000, "Path 1", &[7; 64 * 1024]);
        let block = [path.clone(), res(1028, "", b"\x1c\x02\x05\0\x06secret")].concat();
        let pages = (0..3)
            .map(|n| {
                let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
                let irb = if n == 0 { block.clone() } else { vec![0] };
                page.tags.push((34377, V::Bytes(1, irb)));
                page
            })
            .collect();
        let mut file = Builder::new(b'M', false, pages).build();
        let tiff = read(&file);
        let Value::At { offset, len } = tiff.pages[0].get(34377).unwrap().value else {
            panic!("inline resources")
        };
        for page in &tiff.pages[1..] {
            set_entry(&mut file, page.at, 34377, len, offset);
        }
        let (out, found) = strip(&file);
        assert_eq!(found, [Iptc].into());
        assert!(out.len() < file.len(), "{} of {}", out.len(), file.len());
        let back = read(&out);
        for page in &back.pages {
            assert_eq!(value(&out, page, 34377).unwrap(), path);
        }
        // The plan holds them once.
        let plan = plan(&file).unwrap();
        let held: Vec<&Data> = plan
            .pages
            .iter()
            .flat_map(|dir| dir.fields.iter().filter(|f| f.tag == 34377))
            .map(|f| &f.value)
            .collect();
        let [Data::Shared(first), rest @ ..] = &held[..] else {
            panic!("{held:?}")
        };
        assert_eq!(rest.len(), 2);
        assert!(rest
            .iter()
            .all(|d| matches!(d, Data::Shared(b) if Arc::ptr_eq(b, first))));
    }

    #[test]
    fn a_file_with_no_metadata_says_nothing_went_and_is_written_compact() {
        let file = Builder {
            tail: vec![0; 100],
            ..Builder::new(b'I', false, vec![gray(2, 1, 1, vec![B::data(b"ab")])])
        }
        .build();
        let (out, found) = strip(&file);
        assert!(found.is_empty());
        assert!(out.len() < file.len());
    }

    /// A page with an Exif IFD saying ColorSpace `space` and Interop
    /// `index`, and an ICC profile if `icc`.
    fn signalled(space: u16, index: &'static str, icc: bool) -> Vec<u8> {
        let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
        if icc {
            page.tags.push((34675, V::Bytes(7, b"profile".to_vec())));
        }
        page.exif = Some(vec![
            (0x9003, V::Ascii("2024:05:01 10:20:30")),
            (0xA001, V::Short(vec![space])),
        ]);
        page.interop = Some(vec![
            (1, V::Ascii(index)),
            (2, V::Bytes(7, b"0100".to_vec())),
        ]);
        Builder::new(b'M', false, vec![page]).build()
    }

    #[test]
    fn the_dcf_adobe_rgb_exif_goes_back_only_when_the_color_needs_it() {
        let (out, found) = strip(&signalled(0xFFFF, "R03", false));
        assert_eq!(found, [CreationDate].into());
        let tiff = read(&out);
        let exif = tiff.pages[0].exif.as_ref().unwrap();
        assert!(dcf_only(exif));
        let space = exif.entries.iter().find(|e| e.tag == 0xA001).unwrap();
        assert_eq!(space.uint(Order::Big), Some(0xFFFF));
        let interop = exif.interop.as_ref().unwrap();
        let index = read_value(&mut Cursor::new(&out), &interop.entries[0], 8).unwrap();
        assert_eq!(index.unwrap(), b"R03\0");
        assert!(!out.windows(7).any(|w| w == b"2024:05"));
        // exif.rs reads the same signal from it.
        assert!(exif::walk(&out).dcf_adobe_rgb());
        // With a profile, or sRGB the DCF way, no Exif goes back.
        for file in [
            signalled(0xFFFF, "R03", true),
            signalled(0xFFFF, "R98", false),
            signalled(1, "R03", false),
        ] {
            let (out, _) = strip(&file);
            assert!(read(&out).pages[0].exif.is_none());
        }
    }

    #[test]
    fn pages_that_share_an_exif_and_a_gps_ifd_are_stripped_and_keep_their_color() {
        for (order, big) in [(b'I', false), (b'M', false), (b'I', true), (b'M', true)] {
            let (out, found) = strip(&shared_meta(order, big));
            assert_eq!(found, [CreationDate, Location].into());
            let tiff = read(&out);
            assert_eq!(tiff.pages.len(), 2);
            // Each page said Adobe RGB the DCF way, so each gets it back.
            for page in &tiff.pages {
                assert!(dcf_only(page.exif.as_ref().unwrap()));
                assert!(page.gps.is_none());
            }
            assert!(!out.windows(7).any(|w| w == b"2024:05"));
        }
    }

    #[test]
    fn dng_and_old_style_jpeg_are_refused() {
        for (tag, value, why) in [
            (
                50706,
                V::Bytes(1, vec![1, 4, 0, 0]),
                "is a camera raw (DNG) file",
            ),
            (
                259,
                V::Short(vec![6]),
                "uses an old kind of TIFF compression",
            ),
        ] {
            let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
            page.tags.push((tag, value));
            let Err(Error::Refused(said)) = plan(&Builder::new(b'I', false, vec![page]).build())
            else {
                panic!("{tag} not refused")
            };
            assert!(said.starts_with(why), "{said}");
        }
        // An old-style JPEG whose data is only at 513, with no strips.
        let mut page = gray(1, 1, 1, Vec::new());
        page.tags.extend([
            (259, V::Short(vec![6])),
            (513, V::Long(vec![0])),
            (514, V::Long(vec![0])),
        ]);
        let Err(Error::Refused(said)) = plan(&Builder::new(b'I', false, vec![page]).build()) else {
            panic!("not refused")
        };
        assert!(said.starts_with("uses an old kind"), "{said}");
    }

    #[test]
    fn imagej_and_ome_files_are_refused_and_other_descriptions_dropped() {
        for (text, refused) in [
            ("ImageJ=1.54f\nimages=3\nslices=3\n", true),
            (
                "<?xml version=\"1.0\"?><OME xmlns=\"http://www.openmicroscopy.org/\">",
                true,
            ),
            ("A scan of the secret letter", false),
        ] {
            let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
            page.tags.push((270, V::Ascii(text)));
            let file = Builder::new(b'I', false, vec![page]).build();
            match plan(&file) {
                Err(Error::Refused(why)) => {
                    assert!(refused, "{text}");
                    assert_eq!(why, DESCRIBED);
                }
                Ok(_) => assert!(!refused, "{text}"),
                Err(e) => panic!("{e:?}"),
            }
        }
    }

    /// A small gray JPEG, with `segments` after its SOI.
    fn jpeg(segments: &[u8]) -> Vec<u8> {
        let img = GrayImage::from_fn(8, 8, |x, y| image::Luma([(x * 30 + y) as u8]));
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 90)
            .write_image(img.as_raw(), 8, 8, image::ExtendedColorType::L8)
            .unwrap();
        [&out[..2], segments, &out[2..]].concat()
    }

    /// A page of JPEG data (compression 7): `strip`, and `tables` as its
    /// JPEGTables.
    fn jpeg_page(strip: Vec<u8>, tables: Option<Vec<u8>>) -> Vec<u8> {
        let mut page = gray(8, 8, 8, vec![B::Data(strip)]);
        page.tags.push((259, V::Short(vec![7])));
        if let Some(tables) = tables {
            page.tags.push((347, V::Bytes(7, tables)));
        }
        Builder::new(b'I', false, vec![page]).build()
    }

    #[test]
    fn metadata_inside_jpeg_data_is_refused_and_plain_jpeg_data_kept() {
        let app1 = b"\xFF\xE1\0\x0EExif\0\0MM\0*\0\0";
        let com = b"\xFF\xFE\0\x08secret";
        for (file, refused) in [
            (jpeg_page(jpeg(b""), None), false),
            (jpeg_page(jpeg(app1), None), true),
            (jpeg_page(jpeg(com), None), true),
            (jpeg_page(jpeg(b"\xFF\xED\0\x04ab"), None), true),
            // Fill bytes before a marker.
            (jpeg_page(jpeg(b"\xFF\xFF\xFF\xFE\0\x03x"), None), true),
            (
                jpeg_page(
                    jpeg(b""),
                    Some([&b"\xFF\xD8"[..], app1, b"\xFF\xD9"].concat()),
                ),
                true,
            ),
            (
                jpeg_page(jpeg(b""), Some(b"\xFF\xD8\xFF\xD9".to_vec())),
                false,
            ),
            // Not a stream of segments: nothing to say.
            (jpeg_page(b"\xFF\xD8garbage\xFF\xE1".to_vec(), None), false),
        ] {
            match plan(&file) {
                Err(Error::Refused(why)) => {
                    assert!(refused);
                    assert_eq!(why, IN_DATA);
                }
                Ok(_) => {
                    assert!(!refused);
                    let (out, _) = strip(&file);
                    let tiff = read(&out);
                    let block = tiff.pages[0].blocks[0];
                    let source = read(&file).pages[0].blocks[0];
                    let range = |b: Block| b.offset as usize..(b.offset + b.len) as usize;
                    assert_eq!(out[range(block)], file[range(source)]);
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        // APP1 after the first scan isn't looked for.
        assert!(!segments_hold_metadata(
            b"\xFF\xD8\xFF\xDA\0\x02\xFF\xE1",
            false
        ));
    }

    #[test]
    fn jpeg_data_whose_segments_pass_the_window_is_refused() {
        // An APP2 of 65,535 bytes: the first 64 KiB end inside it, before
        // the APP1 and the scan.
        let app2 = [&b"\xFF\xE2\xFF\xFF"[..], &[7; 65_533]].concat();
        let app1 = b"\xFF\xE1\0\x0EExif\0\0MM\0*\0\0";
        for segments in [[&app2[..], app1].concat(), app2.clone()] {
            let file = jpeg_page(jpeg(&segments), None);
            let Err(Error::Refused(why)) = plan(&file) else {
                panic!("not refused")
            };
            assert_eq!(why, IN_DATA);
        }
        // Tables as long.
        let tables = [&b"\xFF\xD8"[..], &app2, b"\xFF\xD9"].concat();
        let file = jpeg_page(jpeg(b""), Some(tables));
        assert!(matches!(plan(&file), Err(Error::Refused(_))));
        // A stream that ends there, read whole, has nothing after it.
        assert!(!segments_hold_metadata(b"\xFF\xD8\xFF\xE2\0\x10ab", true));
        assert!(segments_hold_metadata(b"\xFF\xD8\xFF\xE2\0\x10ab", false));
        assert!(!segments_hold_metadata(b"\xFF\xD8\xFF\xDA\0\x02", false));
    }

    #[test]
    fn photoshop_resources_that_cant_be_read_go_whole() {
        let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
        page.tags
            .push((34377, V::Bytes(1, b"8BIM\x07\xD0garbage".to_vec())));
        let (out, found) = strip(&Builder::new(b'I', false, vec![page]).build());
        assert_eq!(found, [Iptc, Other].into());
        assert!(read(&out).pages[0].get(34377).is_none());
    }

    #[test]
    fn the_check_catches_a_planted_leftover_and_other_ifds() {
        let file = tagged(b'I', false).build();
        let plan = plan(&file).unwrap();
        let mut out = Cursor::new(Vec::new());
        plan.write(&mut Cursor::new(&file), &mut out, &CancelToken::new())
            .unwrap();
        let out = out.into_inner();
        // ResolutionUnit's tag, made Software's.
        let tiff = read(&out);
        let page = &tiff.pages[0];
        let n = page.entries.iter().position(|e| e.tag == 296).unwrap();
        let at = page.at as usize + 2 + 12 * n;
        let mut planted = out.clone();
        planted[at..at + 2].copy_from_slice(&305u16.to_le_bytes());
        let Err(Error::Broken(why)) = plan.check(&mut Cursor::new(&planted), planted.len() as u64)
        else {
            panic!("not caught")
        };
        assert!(why.contains("keeps tag 305"), "{why}");
        // Another file's output.
        let other =
            strip(&Builder::new(b'I', false, vec![gray(1, 1, 1, vec![B::data(b"x")])]).build()).0;
        assert!(matches!(
            plan.check(&mut Cursor::new(&other), other.len() as u64),
            Err(Error::Broken(_))
        ));
    }

    #[test]
    fn a_truncated_tiff_is_never_planned() {
        // With no bytes after its last block, any cut cuts something.
        let file = Builder {
            tail: Vec::new(),
            ..tagged(b'M', true)
        }
        .build();
        for n in 0..file.len() {
            assert!(plan(&file[..n]).is_err(), "cut at {n}");
        }
    }
}
