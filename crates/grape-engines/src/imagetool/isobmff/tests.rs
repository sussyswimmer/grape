//! A HEIF builder for the tests (D28): `ftyp`, a `meta` with the items,
//! their properties, references and groups, and an `mdat`, in the layouts
//! real writers use and the odd ones the tools must still read: `meta`
//! before or after `mdat`; `iloc` v0, v1 or v2 with fields of 0, 4 or 8
//! bytes and base offsets; 16 or 32-bit item ids; data in `mdat` or
//! `idat`, in one extent or several; `meta`'s header in any form; boxes
//! after it all, and bytes that aren't a box.
//!
//! Each item's data goes where its [`Place`] says, items in order, with no
//! gaps; [`Layout`] says where it went.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::io::Cursor;
use std::ops::Range;

use super::super::exif::tests::{tiff, V};
use super::boxes::{Form, Header, Top};
use super::items::Meta;

/// The ids the layouts give their metadata and pictures.
pub(crate) const EXIF: u32 = 10;
pub(crate) const XMP: u32 = 11;
pub(crate) const THUMB: u32 = 20;
pub(crate) const GAIN: u32 = 30;
pub(crate) const DEPTH: u32 = 31;
pub(crate) const TMAP: u32 = 40;

/// The C2PA `uuid` box's user type.
pub(crate) const C2PA: [u8; 16] = [
    0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
];

/// The XMP `uuid` box's user type.
pub(crate) const XMP_UUID: [u8; 16] = [
    0xBE, 0x7A, 0xCF, 0xCB, 0x97, 0xA9, 0x42, 0xE8, 0x9C, 0x71, 0x99, 0x94, 0x91, 0xE3, 0xAF, 0xAC,
];

/// A box of `kind` holding `content`.
pub(crate) fn bx(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
    let mut out = Header::write(kind, Form::Small, 8 + content.len() as u64);
    out.extend_from_slice(content);
    out
}

/// A full box: `version`, `flags`, then `content`.
pub(crate) fn full(kind: &[u8; 4], version: u8, flags: u32, content: &[u8]) -> Vec<u8> {
    let mut c = (u32::from(version) << 24 | flags).to_be_bytes().to_vec();
    c.extend_from_slice(content);
    bx(kind, &c)
}

/// A `uuid` box of `user_type` holding `content`.
pub(crate) fn uuid(user_type: [u8; 16], content: &[u8]) -> Vec<u8> {
    bx(b"uuid", &[&user_type[..], content].concat())
}

/// Where an item's data goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Mdat,
    Idat,
    /// It has no `iloc` entry.
    Nowhere,
}

/// An item to build.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    pub(crate) id: u32,
    pub(crate) kind: [u8; 4],
    pub(crate) hidden: bool,
    /// A `mime` item's content type, a `uri ` item's URI type.
    pub(crate) text: Option<&'static str>,
    pub(crate) data: Vec<u8>,
    pub(crate) place: Place,
    /// Its data in this many extents (the last takes what's left).
    pub(crate) extents: usize,
    /// Its properties: 1-based indices into the builder's, and whether
    /// each is essential.
    pub(crate) props: Vec<(u16, bool)>,
}

impl Item {
    pub(crate) fn new(id: u32, kind: [u8; 4], place: Place, data: Vec<u8>) -> Item {
        Item {
            id,
            kind,
            hidden: false,
            text: None,
            data,
            place,
            extents: 1,
            props: Vec::new(),
        }
    }

    /// A picture whose data is `fill` repeated `len` times.
    pub(crate) fn picture(id: u32, fill: u8, len: usize) -> Item {
        Item::new(id, *b"hvc1", Place::Mdat, vec![fill; len])
    }

    /// An Exif item: its 4-byte header offset (6), `Exif\0\0`, then `tiff`.
    pub(crate) fn exif(id: u32, tiff: &[u8]) -> Item {
        let mut data = 6u32.to_be_bytes().to_vec();
        data.extend_from_slice(b"Exif\0\0");
        data.extend_from_slice(tiff);
        let mut item = Item::new(id, *b"Exif", Place::Mdat, data);
        item.hidden = true;
        item
    }

    /// A `mime` item of `content_type`.
    pub(crate) fn mime(id: u32, content_type: &'static str, data: &[u8]) -> Item {
        let mut item = Item::new(id, *b"mime", Place::Mdat, data.to_vec());
        item.text = Some(content_type);
        item.hidden = true;
        item
    }

    /// An XMP item.
    pub(crate) fn xmp(id: u32, text: &str) -> Item {
        Item::mime(id, "application/rdf+xml", text.as_bytes())
    }
}

/// A camera's Exif as a big-endian TIFF: make, model, a date, a comment
/// and a GPS position.
pub(crate) fn camera() -> Vec<u8> {
    tiff(
        b'M',
        &[
            &[
                (0x010F, V::Ascii("SecretMake")),
                (0x0110, V::Ascii("SecretModel")),
                (0x0112, V::Short(6)),
                (0x8769, V::Ifd(1)),
                (0x8825, V::Ifd(2)),
            ],
            &[(0x9003, V::Ascii("2024:05:01 10:20:30"))],
            &[
                (0x0001, V::Ascii("N")),
                (
                    0x0002,
                    V::Bytes(
                        5,
                        &[
                            0, 0, 0, 48, 0, 0, 0, 1, 0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1,
                        ],
                    ),
                ),
            ],
        ],
        None,
    )
}

/// An XMP packet with a secret.
pub(crate) const SECRET_XMP: &str = "<x:xmpmeta xmlns:x='adobe:ns:meta/'>SecretPlace</x:xmpmeta>";

/// A property box: `hvcC`, `ispe` and such hold what the tests need only.
pub(crate) fn prop(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
    bx(kind, content)
}

/// An `ispe` of `w` × `h`.
pub(crate) fn ispe(w: u32, h: u32) -> Vec<u8> {
    full(b"ispe", 0, 0, &[w.to_be_bytes(), h.to_be_bytes()].concat())
}

/// An `auxC` of `urn`.
pub(crate) fn auxc(urn: &str) -> Vec<u8> {
    full(b"auxC", 0, 0, &[urn.as_bytes(), b"\0"].concat())
}

/// A `udes`: language, name, description, tags.
pub(crate) fn udes(name: &str, description: &str, tags: &str) -> Vec<u8> {
    let mut c = b"en\0".to_vec();
    for s in [name, description, tags] {
        c.extend_from_slice(s.as_bytes());
        c.push(0);
    }
    full(b"udes", 0, 0, &c)
}

/// A `crtt` or `mdft` of `time`.
pub(crate) fn time(kind: &[u8; 4], time: u64) -> Vec<u8> {
    full(kind, 0, 0, &time.to_be_bytes())
}

/// An item's data, where it went, and its extents there.
type Placed = (Vec<u8>, Place, Vec<Range<u64>>);

/// Where the builder put each item's data.
#[derive(Debug, Clone, Default)]
pub(crate) struct Layout {
    /// Each placed item's data and where it is: file offsets for `mdat`,
    /// offsets in `idat`'s content for `idat`.
    pub(crate) items: HashMap<u32, Placed>,
}

impl Layout {
    /// Item `id`'s data.
    pub(crate) fn data(&self, id: u32) -> Vec<u8> {
        self.items[&id].0.clone()
    }

    /// Where item `id`'s data is in the file, for an `mdat` item.
    pub(crate) fn file_ranges(&self, id: u32) -> Vec<Range<u64>> {
        let (_, place, ranges) = &self.items[&id];
        assert_eq!(*place, Place::Mdat, "{id}");
        ranges.clone()
    }
}

/// A HEIF file to build.
#[derive(Debug, Clone)]
pub(crate) struct Builder {
    /// `ftyp`'s content.
    pub(crate) brands: Vec<u8>,
    /// `hdlr`'s handler type.
    pub(crate) handler: [u8; 4],
    pub(crate) items: Vec<Item>,
    pub(crate) primary: u32,
    /// `ipco`'s boxes.
    pub(crate) properties: Vec<Vec<u8>>,
    /// (type, from, to).
    pub(crate) refs: Vec<([u8; 4], u32, Vec<u32>)>,
    /// `grpl`'s groups: (type, group id, entity ids).
    pub(crate) groups: Vec<([u8; 4], u32, Vec<u32>)>,
    pub(crate) iloc_version: u8,
    /// `iloc`'s offset, length, base offset and index sizes. With a base
    /// offset, each item's is where its data starts and its extents count
    /// from there.
    pub(crate) sizes: [u8; 4],
    /// 32-bit item ids: `infe` v3, `iref` v1, `ipma` v1, `pitm` v1, and
    /// `iloc` v2.
    pub(crate) wide: bool,
    /// 15-bit property indices (`ipma` flag 1).
    pub(crate) wide_indices: bool,
    pub(crate) meta_after_mdat: bool,
    /// `iloc` as `meta`'s first box.
    pub(crate) iloc_first: bool,
    pub(crate) meta_form: Form,
    pub(crate) mdat_form: Form,
    /// More boxes at the end of `meta`.
    pub(crate) extra_meta: Vec<Vec<u8>>,
    /// `meta`'s boxes left out.
    pub(crate) leave_out: Vec<[u8; 4]>,
    /// An item whose `iloc` entry is written twice.
    pub(crate) iloc_twice: Option<u32>,
    /// A `dinf` with one `url ` of this file.
    pub(crate) dinf: bool,
    /// Top-level boxes after `ftyp`, before `meta` and `mdat`.
    pub(crate) before: Vec<Vec<u8>>,
    /// Top-level boxes between `meta` and `mdat`, whichever is first.
    pub(crate) between: Vec<Vec<u8>>,
    /// Top-level boxes at the end.
    pub(crate) after: Vec<Vec<u8>>,
    /// Bytes after everything that aren't a box.
    pub(crate) tail: Vec<u8>,
}

impl Builder {
    pub(crate) fn new(items: Vec<Item>, primary: u32) -> Builder {
        Builder {
            brands: b"heic\0\0\0\0mif1heic".to_vec(),
            handler: *b"pict",
            items,
            primary,
            properties: Vec::new(),
            refs: Vec::new(),
            groups: Vec::new(),
            iloc_version: 1,
            sizes: [4, 4, 0, 0],
            wide: false,
            wide_indices: false,
            meta_after_mdat: false,
            iloc_first: false,
            meta_form: Form::Small,
            mdat_form: Form::Small,
            extra_meta: Vec::new(),
            leave_out: Vec::new(),
            iloc_twice: None,
            dinf: true,
            before: Vec::new(),
            between: Vec::new(),
            after: Vec::new(),
            tail: Vec::new(),
        }
    }

    /// Builds the file, and says where each item's data went.
    pub(crate) fn build(&self) -> (Vec<u8>, Layout) {
        // Where the data goes: offsets in mdat's content and in idat's.
        let mut mdat = Vec::new();
        let mut idat = Vec::new();
        let mut spans: Vec<(u32, Place, Vec<Range<u64>>)> = Vec::new();
        for item in &self.items {
            let space = match item.place {
                Place::Mdat => &mut mdat,
                Place::Idat => &mut idat,
                Place::Nowhere => continue,
            };
            let start = space.len() as u64;
            space.extend_from_slice(&item.data);
            let n = item.extents.max(1) as u64;
            let len = item.data.len() as u64;
            let step = len / n;
            let ranges = (0..n)
                .map(|i| {
                    let a = start + i * step;
                    let b = if i + 1 == n { start + len } else { a + step };
                    a..b
                })
                .collect();
            spans.push((item.id, item.place, ranges));
        }
        let ftyp = bx(b"ftyp", &self.brands);
        let before: Vec<u8> = self.before.concat();
        let between: Vec<u8> = self.between.concat();
        let mdat_box = |content: &[u8]| {
            let mut out = Header::write(
                b"mdat",
                self.mdat_form,
                self.mdat_form.head() + content.len() as u64,
            );
            if self.mdat_form == Form::ToEnd {
                out[..4].copy_from_slice(&[0; 4]);
            }
            out.extend_from_slice(content);
            out
        };
        let mdat_head = self.mdat_form.head();
        // meta's size doesn't depend on where mdat is: every field has its
        // size.
        let meta_len = self.meta(&spans, &idat, 0).len() as u64;
        let mdat_at = (ftyp.len() + before.len()) as u64
            + if self.meta_after_mdat {
                0
            } else {
                meta_len + between.len() as u64
            };
        let content_at = mdat_at + mdat_head;
        let meta = self.meta(&spans, &idat, content_at);
        assert_eq!(meta.len() as u64, meta_len);
        let mut file = ftyp;
        file.extend_from_slice(&before);
        if self.meta_after_mdat {
            file.extend_from_slice(&mdat_box(&mdat));
            file.extend_from_slice(&between);
            file.extend_from_slice(&meta);
        } else {
            file.extend_from_slice(&meta);
            file.extend_from_slice(&between);
            file.extend_from_slice(&mdat_box(&mdat));
        }
        for b in &self.after {
            file.extend_from_slice(b);
        }
        file.extend_from_slice(&self.tail);
        let mut layout = Layout::default();
        for (id, place, ranges) in spans {
            let shift = if place == Place::Mdat { content_at } else { 0 };
            let ranges = ranges
                .into_iter()
                .map(|r| r.start + shift..r.end + shift)
                .collect();
            let data = self.items.iter().find(|i| i.id == id).unwrap().data.clone();
            layout.items.insert(id, (data, place, ranges));
        }
        (file, layout)
    }

    /// The `meta` box, with `mdat`'s content at `content_at`.
    fn meta(
        &self,
        spans: &[(u32, Place, Vec<Range<u64>>)],
        idat: &[u8],
        content_at: u64,
    ) -> Vec<u8> {
        let wide = self.wide;
        let id = |v: u32| -> Vec<u8> {
            if wide {
                v.to_be_bytes().to_vec()
            } else {
                u16::try_from(v).unwrap().to_be_bytes().to_vec()
            }
        };
        let mut boxes: Vec<([u8; 4], Vec<u8>)> = Vec::new();
        let mut hdlr = vec![0; 4];
        hdlr.extend_from_slice(&self.handler);
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.push(0);
        boxes.push((*b"hdlr", full(b"hdlr", 0, 0, &hdlr)));
        if self.dinf {
            let url = full(b"url ", 0, 1, b"");
            let dref = full(b"dref", 0, 0, &[&1u32.to_be_bytes()[..], &url].concat());
            boxes.push((*b"dinf", bx(b"dinf", &dref)));
        }
        boxes.push((
            *b"pitm",
            full(b"pitm", u8::from(wide), 0, &id(self.primary)),
        ));
        boxes.push((*b"iloc", self.iloc(spans, content_at)));
        let mut iinf = Vec::new();
        let count = self.items.len();
        let iinf_version = u8::from(wide || count > 0xFFFF);
        if iinf_version == 0 {
            iinf.extend_from_slice(&u16::try_from(count).unwrap().to_be_bytes());
        } else {
            iinf.extend_from_slice(&u32::try_from(count).unwrap().to_be_bytes());
        }
        for item in &self.items {
            let mut c = id(item.id);
            c.extend_from_slice(&[0, 0]);
            c.extend_from_slice(&item.kind);
            c.push(0);
            if let Some(text) = item.text {
                c.extend_from_slice(text.as_bytes());
                c.push(0);
            }
            let version = if wide { 3 } else { 2 };
            iinf.extend_from_slice(&full(b"infe", version, u32::from(item.hidden), &c));
        }
        boxes.push((*b"iinf", full(b"iinf", iinf_version, 0, &iinf)));
        if !self.refs.is_empty() {
            let mut iref = Vec::new();
            for (kind, from, to) in &self.refs {
                let mut c = id(*from);
                c.extend_from_slice(&u16::try_from(to.len()).unwrap().to_be_bytes());
                for &t in to {
                    c.extend_from_slice(&id(t));
                }
                iref.extend_from_slice(&bx(kind, &c));
            }
            boxes.push((*b"iref", full(b"iref", u8::from(wide), 0, &iref)));
        }
        let mut ipma = Vec::new();
        let with_props: Vec<&Item> = self.items.iter().filter(|i| !i.props.is_empty()).collect();
        ipma.extend_from_slice(&u32::try_from(with_props.len()).unwrap().to_be_bytes());
        for item in with_props {
            ipma.extend_from_slice(&id(item.id));
            ipma.push(u8::try_from(item.props.len()).unwrap());
            for &(index, essential) in &item.props {
                if self.wide_indices {
                    ipma.extend_from_slice(&(index | u16::from(essential) << 15).to_be_bytes());
                } else {
                    ipma.push(u8::try_from(index).unwrap() | u8::from(essential) << 7);
                }
            }
        }
        let ipco = bx(b"ipco", &self.properties.concat());
        let ipma = full(b"ipma", u8::from(wide), u32::from(self.wide_indices), &ipma);
        boxes.push((*b"iprp", bx(b"iprp", &[ipco, ipma].concat())));
        if !idat.is_empty() {
            boxes.push((*b"idat", bx(b"idat", idat)));
        }
        if !self.groups.is_empty() {
            let mut grpl = Vec::new();
            for (kind, group, ids) in &self.groups {
                let mut c = group.to_be_bytes().to_vec();
                c.extend_from_slice(&u32::try_from(ids.len()).unwrap().to_be_bytes());
                for &e in ids {
                    c.extend_from_slice(&e.to_be_bytes());
                }
                grpl.extend_from_slice(&full(kind, 0, 0, &c));
            }
            boxes.push((*b"grpl", bx(b"grpl", &grpl)));
        }
        if self.iloc_first {
            let at = boxes.iter().position(|(k, _)| k == b"iloc").unwrap();
            let iloc = boxes.remove(at);
            boxes.insert(0, iloc);
        }
        boxes.retain(|(k, _)| !self.leave_out.contains(k));
        let mut content = vec![0; 4];
        for (_, b) in boxes {
            content.extend_from_slice(&b);
        }
        for b in &self.extra_meta {
            content.extend_from_slice(b);
        }
        let len = self.meta_form.head() + content.len() as u64;
        let mut out = Header::write(b"meta", self.meta_form, len);
        if self.meta_form == Form::ToEnd {
            out[..4].copy_from_slice(&[0; 4]);
        }
        out.extend_from_slice(&content);
        out
    }

    /// The `iloc` box, with `mdat`'s content at `content_at`.
    fn iloc(&self, spans: &[(u32, Place, Vec<Range<u64>>)], content_at: u64) -> Vec<u8> {
        let version = if self.wide { 2 } else { self.iloc_version };
        let [offset, length, base, index] = self.sizes.map(usize::from);
        let uint = |out: &mut Vec<u8>, v: u64, size: usize| {
            out.extend_from_slice(&v.to_be_bytes()[8 - size..]);
        };
        let mut c = vec![
            self.sizes[0] << 4 | self.sizes[1],
            self.sizes[2] << 4 | self.sizes[3],
        ];
        let mut entries: Vec<&(u32, Place, Vec<Range<u64>>)> = spans.iter().collect();
        if let Some(twice) = self.iloc_twice {
            entries.push(spans.iter().find(|s| s.0 == twice).unwrap());
        }
        if version < 2 {
            c.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_be_bytes());
        } else {
            c.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_be_bytes());
        }
        for (id, place, ranges) in entries {
            if version < 2 {
                c.extend_from_slice(&u16::try_from(*id).unwrap().to_be_bytes());
            } else {
                c.extend_from_slice(&id.to_be_bytes());
            }
            let shift = if *place == Place::Mdat { content_at } else { 0 };
            if version > 0 {
                let method: u16 = if *place == Place::Idat { 1 } else { 0 };
                c.extend_from_slice(&method.to_be_bytes());
            } else {
                assert_eq!(*place, Place::Mdat, "iloc v0 has no idat");
            }
            c.extend_from_slice(&[0, 0]);
            let first = ranges.first().map_or(0, |r| r.start) + shift;
            let base_value = if base > 0 { first } else { 0 };
            uint(&mut c, base_value, base);
            c.extend_from_slice(&u16::try_from(ranges.len()).unwrap().to_be_bytes());
            for r in ranges {
                if version > 0 {
                    uint(&mut c, 0, index);
                }
                let at = r.start + shift - base_value;
                assert!(
                    offset > 0 || at == 0,
                    "no offset field, and an extent past the base"
                );
                uint(&mut c, at, offset);
                uint(&mut c, r.end - r.start, length);
            }
        }
        full(b"iloc", version, 0, &c)
    }
}

/// A primary picture (id 1, 300 bytes of 0xA1), its Exif and its XMP.
pub(crate) fn simple() -> Builder {
    let mut primary = Item::picture(1, 0xA1, 300);
    primary.props = vec![(1, true), (2, false)];
    let mut b = Builder::new(
        vec![
            primary,
            Item::exif(EXIF, &camera()),
            Item::xmp(XMP, SECRET_XMP),
        ],
        1,
    );
    b.properties = vec![prop(b"hvcC", &[1; 23]), ispe(64, 48)];
    b.refs = vec![(*b"cdsc", EXIF, vec![1]), (*b"cdsc", XMP, vec![1])];
    b
}

/// An iPhone's layout: a grid (its descriptor in `idat`) of two hidden
/// tiles, a thumbnail, the Exif, a gain map with its own XMP, a depth
/// map, and a tone map grouped with the grid.
pub(crate) fn iphone() -> Builder {
    let mut grid = Item::new(1, *b"grid", Place::Idat, vec![0, 0, 0, 1, 0, 64, 0, 48]);
    grid.props = vec![(3, false), (4, true)];
    let mut tiles = Vec::new();
    for (id, fill) in [(2, 0xB2), (3, 0xB3)] {
        let mut tile = Item::picture(id, fill, 500);
        tile.hidden = true;
        tile.props = vec![(1, true), (2, false)];
        tiles.push(tile);
    }
    let mut thumb = Item::picture(THUMB, 0xC0, 120);
    thumb.props = vec![(1, true), (2, false)];
    let mut gain = Item::picture(GAIN, 0xD0, 200);
    gain.hidden = true;
    gain.props = vec![(1, true), (2, false), (5, true)];
    let mut depth = Item::picture(DEPTH, 0xD1, 100);
    depth.hidden = true;
    depth.props = vec![(1, true), (2, false), (6, true)];
    let mut tmap = Item::new(TMAP, *b"tmap", Place::Idat, vec![0, 1, 2, 3, 4, 5]);
    tmap.props = vec![(3, false), (4, true)];
    let mut items = vec![grid];
    items.extend(tiles);
    items.extend([
        thumb,
        Item::exif(EXIF, &camera()),
        gain,
        Item::xmp(XMP, "<x:xmpmeta>HDRGainMapSecret</x:xmpmeta>"),
        depth,
        tmap,
    ]);
    let mut b = Builder::new(items, 1);
    b.properties = vec![
        prop(b"hvcC", &[1; 23]),
        ispe(32, 48),
        prop(b"colr", b"prof\0\0\0\x10icc-profile-data"),
        prop(b"irot", &[1]),
        auxc("urn:com:apple:photo:2020:aux:hdrgainmap"),
        auxc("urn:mpeg:hevc:2015:auxid:2"),
    ];
    b.refs = vec![
        (*b"dimg", 1, vec![2, 3]),
        (*b"thmb", THUMB, vec![1]),
        (*b"cdsc", EXIF, vec![1]),
        (*b"auxl", GAIN, vec![1]),
        (*b"cdsc", XMP, vec![GAIN]),
        (*b"auxl", DEPTH, vec![1]),
        (*b"dimg", TMAP, vec![1, GAIN]),
    ];
    b.groups = vec![(*b"altr", 50, vec![TMAP, 1])];
    b
}

/// 32-bit ids (`infe` v3, `iref` v1, `iloc` v2, `ipma` v1) and 64-bit
/// offsets, lengths and base offsets.
pub(crate) fn wide() -> Builder {
    let mut primary = Item::picture(70_000, 0xA1, 300);
    primary.props = vec![(1, true), (2, false)];
    let mut b = Builder::new(
        vec![
            primary,
            Item::exif(70_002, &camera()),
            Item::xmp(70_003, SECRET_XMP),
        ],
        70_000,
    );
    b.wide = true;
    b.wide_indices = true;
    b.sizes = [8, 8, 8, 0];
    b.properties = vec![prop(b"hvcC", &[1; 23]), ispe(64, 48)];
    b.refs = vec![
        (*b"cdsc", 70_002, vec![70_000]),
        (*b"cdsc", 70_003, vec![70_000]),
    ];
    b
}

/// A primary with a `udes`, a `crtt` and an `mdft` among its properties,
/// and properties after them.
pub(crate) fn described() -> Builder {
    let mut b = simple();
    b.properties = vec![
        prop(b"hvcC", &[1; 23]),
        ispe(64, 48),
        udes("SecretTitle", "SecretDescription", "SecretTag"),
        prop(b"colr", b"nclx\0\x01\0\x0d\0\x06\x80"),
        time(b"crtt", 3_800_000_000_000_000),
        time(b"mdft", 3_800_000_100_000_000),
        prop(b"irot", &[3]),
    ];
    b.items[0].props = vec![
        (1, true),
        (2, false),
        (3, false),
        (4, false),
        (5, false),
        (6, false),
        (7, true),
    ];
    b
}

/// Every layout the tools must read, by name.
pub(crate) fn layouts() -> Vec<(&'static str, Builder)> {
    let mut all = vec![("simple", simple()), ("iphone", iphone()), ("wide", wide())];
    let mut b = simple();
    b.meta_after_mdat = true;
    all.push(("meta after mdat", b));
    let mut b = simple();
    b.sizes = [8, 8, 8, 0];
    all.push(("64-bit fields and base offsets", b));
    let mut b = simple();
    b.sizes = [0, 4, 4, 0];
    all.push(("base offsets only", b));
    let mut b = simple();
    b.items[1].place = Place::Idat;
    all.push(("Exif in idat", b));
    let mut b = simple();
    b.items.push(Item::exif(12, &camera()));
    b.refs.push((*b"cdsc", 12, vec![1]));
    all.push(("two Exif items", b));
    let mut b = simple();
    b.items.rotate_left(1);
    b.items.rotate_left(1);
    all.push(("Exif last", b));
    let mut b = simple();
    b.iloc_first = true;
    all.push(("iloc first", b));
    let mut b = simple();
    b.meta_form = Form::Large;
    all.push(("largesize meta", b));
    let mut b = simple();
    b.meta_form = Form::ToEnd;
    b.meta_after_mdat = true;
    all.push(("size-0 meta", b));
    let mut b = simple();
    b.iloc_version = 0;
    all.push(("iloc v0", b));
    let mut b = simple();
    b.items[0].extents = 3;
    b.items[1].extents = 2;
    b.sizes = [4, 4, 0, 4];
    all.push(("several extents with indices", b));
    let mut b = simple();
    b.mdat_form = Form::Large;
    all.push(("largesize mdat", b));
    all.push(("described", described()));
    let mut b = simple();
    b.after = vec![
        uuid(XMP_UUID, SECRET_XMP.as_bytes()),
        uuid(C2PA, b"c2pa manifest SecretSigner"),
    ];
    all.push(("uuid boxes", b));
    let mut b = simple();
    b.after = vec![bx(b"mpvd", b"motion video SecretClip")];
    all.push(("trailing mpvd", b));
    let mut b = simple();
    b.tail = vec![0; 37];
    all.push(("trailing zeros", b));
    let mut b = simple();
    b.tail = b"\x01SecretJunk".to_vec();
    all.push(("trailing junk", b));
    all
}

/// Where the content of `file`'s `meta` starts.
pub(crate) fn meta_at(file: &[u8]) -> usize {
    let top = Top::read(&mut Cursor::new(file), file.len() as u64).unwrap();
    let meta = top.all(b"meta").next().unwrap();
    meta.content().start as usize
}

/// Item `id`'s data in `file`, read through `m` (its `meta`): its extents
/// end to end, from the file or `idat`.
pub(crate) fn read(file: &[u8], m: &Meta, id: u32) -> Vec<u8> {
    let loc = m.location(id).unwrap();
    let idat = m.idat();
    let mut out = Vec::new();
    for span in m.spans(loc).unwrap() {
        let (space, at): (&[u8], usize) = match loc.method {
            0 => (file, 0),
            1 => (&m.bytes, idat.clone().unwrap().start),
            _ => panic!("method {}", loc.method),
        };
        let size = match loc.method {
            0 => file.len() as u64,
            _ => idat.clone().unwrap().len() as u64,
        };
        let start = at + span.start as usize;
        let end = at + span.end(size) as usize;
        out.extend_from_slice(&space[start..end]);
    }
    out
}
