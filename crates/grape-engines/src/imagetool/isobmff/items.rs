//! A HEIF file's items (D28): the one file-level `meta`, held in memory
//! ([`MAX_META`](super::boxes::MAX_META) at most), and what its boxes say.
//!
//! - **Parsed:** `hdlr` (it must name `pict`), `pitm`, `iinf` (`infe`
//!   v2 and v3; a v0 or v1 entry, which has a MIME type and no item type,
//!   is read as a `mime` item), `iloc` (v0, v1 and v2; offset, length,
//!   base offset and index fields of 0, 4 or 8 bytes; several extents;
//!   construction methods 0, 1 and 2), `iref` (v0 and v1), `iprp` → `ipco`
//!   and `ipma` (v0 and v1; flag 1 for 15-bit property indices), `idat`,
//!   `grpl` and `dinf` (its place only). Every other box is kept as it is.
//! - **In place:** every box, entry and property keeps its place in
//!   `meta`'s bytes, so a tool writes back what it keeps byte for byte.
//! - **Roles** ([`Role`]): a picture (`hvc1`, `av01`, `grid`, `iden`,
//!   `iovl`, `tmap`, `jpeg`, …), Exif, XMP (a `mime` item of
//!   `application/rdf+xml`), another `mime` item, a `uri ` item, IPTC, or
//!   something else. Of the pictures, an auxiliary image's kind comes from
//!   its `auxC` property ([`Aux`]), and a thumbnail has an outgoing `thmb`.
//! - **Broken** ([`Error::Broken`], "isn't a readable HEIC image"): no
//!   file-level `meta`, or two; one without `hdlr`, `pitm`, `iinf` or
//!   `iloc`, or with two of a box it may hold once; an item id twice in
//!   `iinf` or in `iloc`; a primary item that isn't there; a version or a
//!   field size the format doesn't have; a property index past `ipco`'s
//!   end; anything that runs past its box.
//! - **Refused by both tools:** a `moov` ([`is_sequence`]): an image
//!   sequence or a burst.
//! - **Caps:** [`MAX_ITEMS`] items, [`MAX_EXTENTS`] extents, [`MAX_REFS`]
//!   item ids in `iref` (in at most [`MAX_ITEMS`] boxes) and in `grpl`,
//!   [`MAX_PROPERTIES`] properties and [`MAX_ASSOCIATIONS`] associations.
//!   Over one, the file is too large ([`Error::TooLarge`]), never cut
//!   short.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use super::super::strip::Error;
use super::boxes::{children, Child, Fields, Header, Top, MAX_CHILDREN};

/// The most items in a file, and the most entries in `iloc`, in `ipma`
/// (in all) and the most boxes in `iref`.
pub(crate) const MAX_ITEMS: usize = 65_536;

/// The most extents in `iloc`, in all.
pub(crate) const MAX_EXTENTS: usize = 262_144;

/// The most item ids `iref`'s boxes list, in all, and the most entity ids
/// `grpl`'s groups do.
pub(crate) const MAX_REFS: usize = 1 << 20;

/// The most properties in `ipco`.
pub(crate) const MAX_PROPERTIES: usize = 65_536;

/// The most property associations in `ipma`, in all.
pub(crate) const MAX_ASSOCIATIONS: usize = 1 << 20;

/// The item types that are pictures.
const PICTURES: [&[u8; 4]; 14] = [
    b"hvc1", b"hvt1", b"lhv1", b"av01", b"avc1", b"grid", b"iden", b"iovl", b"tmap", b"jpeg",
    b"j2k1", b"unci", b"vvc1", b"mski",
];

/// XMP's MIME type.
const XMP_TYPE: &[u8] = b"application/rdf+xml";

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// A picture: the primary, a tile, a thumbnail, an auxiliary image, a
    /// tone map, ….
    Image,
    Exif,
    /// A `mime` item of `application/rdf+xml`.
    Xmp,
    /// Any other `mime` item.
    Mime,
    /// A `uri ` item.
    Uri,
    /// An `iptc` item: IPTC-IIM.
    Iptc,
    /// An item of another type.
    Other,
}

impl Role {
    /// Whether it's metadata by its type: Exif, XMP, another `mime` item,
    /// a `uri ` item or IPTC.
    pub(crate) fn metadata(self) -> bool {
        matches!(
            self,
            Role::Exif | Role::Xmp | Role::Mime | Role::Uri | Role::Iptc
        )
    }
}

/// What an auxiliary image is, by its `auxC` URN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Aux {
    Alpha,
    Depth,
    /// Apple's Portrait mattes.
    Matte,
    /// Apple's HDR gain map.
    GainMap,
    Other,
}

/// An item, from its `infe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) id: u32,
    /// Its item type.
    pub(crate) kind: [u8; 4],
    /// `infe`'s flag 1: not meant to be shown on its own.
    pub(crate) hidden: bool,
    /// A `mime` item of `application/rdf+xml`.
    pub(crate) xmp: bool,
    /// Its `infe` box, in `meta`'s bytes.
    pub(crate) range: Range<usize>,
}

impl Item {
    pub(crate) fn role(&self) -> Role {
        match &self.kind {
            b"Exif" => Role::Exif,
            b"mime" if self.xmp => Role::Xmp,
            b"mime" => Role::Mime,
            b"uri " => Role::Uri,
            b"iptc" => Role::Iptc,
            kind if PICTURES.contains(&kind) => Role::Image,
            _ => Role::Other,
        }
    }
}

/// Where an item's data is: its `iloc` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Location {
    pub(crate) id: u32,
    /// 0: offsets in the file; 1: in `idat`; 2: in other items.
    pub(crate) method: u8,
    /// 0: this file.
    pub(crate) data_ref: u16,
    pub(crate) base: u64,
    /// Its extents, in [`Meta::extents`].
    pub(crate) extents: Range<usize>,
    /// The entry, in `meta`'s bytes.
    pub(crate) range: Range<usize>,
}

/// An extent as `iloc` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    pub(crate) offset: u64,
    /// 0: to the end of the file, `idat` or item.
    pub(crate) len: u64,
}

/// An extent with its base offset added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) start: u64,
    /// `None`: to the end of the file, `idat` or item.
    pub(crate) len: Option<u64>,
}

impl Span {
    /// Where it ends, in a space of `size` bytes.
    pub(crate) fn end(self, size: u64) -> u64 {
        self.len.map_or(size, |len| self.start + len)
    }
}

/// A reference box in `iref`: `from` refers to `to` (`cdsc`, `dimg`,
/// `thmb`, `auxl`, `prem`, `base`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ref {
    pub(crate) kind: [u8; 4],
    pub(crate) from: u32,
    /// The ids it refers to, in [`Meta::targets`].
    pub(crate) to: Range<usize>,
    /// The box, in `meta`'s bytes.
    pub(crate) child: Child,
}

/// One `ipma` box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Map {
    /// Its place among `iprp`'s boxes.
    pub(crate) child: usize,
    pub(crate) version: u8,
    pub(crate) flags: u32,
    pub(crate) entries: Vec<MapEntry>,
    /// Where its entries end, in `meta`'s bytes (anything after them is
    /// kept as it is).
    pub(crate) end: usize,
}

impl Map {
    /// Whether its property indices are 15 bits.
    pub(crate) fn wide(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// An item's properties in an `ipma`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MapEntry {
    pub(crate) item: u32,
    /// In [`Meta::associations`].
    pub(crate) props: Range<usize>,
    /// The entry, in `meta`'s bytes.
    pub(crate) range: Range<usize>,
}

/// A property association: a 1-based index into `ipco` (0: none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Association {
    pub(crate) essential: bool,
    pub(crate) index: u16,
}

/// `iprp`: its boxes, `ipco` among them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Iprp {
    /// Its place among `meta`'s boxes.
    pub(crate) child: usize,
    pub(crate) children: Vec<Child>,
    /// `ipco`'s place among them.
    pub(crate) ipco: Option<usize>,
}

/// An entity group in `grpl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Group {
    /// The box, in `meta`'s bytes.
    pub(crate) child: Child,
    pub(crate) id: u32,
    /// Its entity ids, in [`Meta::entities`].
    pub(crate) entities: Range<usize>,
    /// Where its entity list ends, in `meta`'s bytes (anything after it is
    /// kept as it is).
    pub(crate) list_end: usize,
}

/// A HEIF file's `meta` ([module docs](self)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Meta {
    /// The box's content: its version and flags, then its boxes.
    pub(crate) bytes: Vec<u8>,
    /// Its boxes, in order.
    pub(crate) children: Vec<Child>,
    pub(crate) primary: u32,
    /// The items, in `iinf`'s order.
    pub(crate) items: Vec<Item>,
    /// `iinf`'s place among the boxes, and its version.
    pub(crate) iinf: (usize, u8),
    /// `iloc`'s place among the boxes, and its version.
    pub(crate) iloc: (usize, u8),
    /// `iloc`'s entries, in order.
    pub(crate) locations: Vec<Location>,
    pub(crate) extents: Vec<Extent>,
    /// Where `iloc`'s entries end, in the bytes.
    pub(crate) iloc_end: usize,
    /// `iref`'s place among the boxes, and its version.
    pub(crate) iref: Option<(usize, u8)>,
    pub(crate) refs: Vec<Ref>,
    pub(crate) targets: Vec<u32>,
    pub(crate) iprp: Option<Iprp>,
    /// `ipco`'s properties, in order (index 1 first).
    pub(crate) properties: Vec<Child>,
    /// The `ipma` boxes, in order.
    pub(crate) maps: Vec<Map>,
    pub(crate) associations: Vec<Association>,
    /// `idat`'s place among the boxes.
    pub(crate) idat: Option<usize>,
    /// `grpl`'s place among the boxes.
    pub(crate) grpl: Option<usize>,
    pub(crate) groups: Vec<Group>,
    pub(crate) entities: Vec<u32>,
}

/// The file's one file-level `meta`: none, or two, is broken.
pub(crate) fn file_meta(top: &Top) -> Result<Header, Error> {
    let mut metas = top.all(b"meta");
    match (metas.next(), metas.next()) {
        (Some(meta), None) => Ok(*meta),
        (None, _) => Err(Error::Broken("no meta box".into())),
        (Some(_), Some(_)) => Err(Error::Broken("two meta boxes".into())),
    }
}

/// Whether the file is an image sequence or a burst (it has a `moov`),
/// which both tools refuse.
pub(crate) fn is_sequence(top: &Top) -> bool {
    top.all(b"moov").next().is_some()
}

/// The boxes `meta` holds at most once.
const ONCE: [&[u8; 4]; 9] = [
    b"hdlr", b"pitm", b"iinf", b"iloc", b"iref", b"iprp", b"idat", b"grpl", b"dinf",
];

impl Meta {
    /// Reads the content of a file-level `meta` ([module docs](self)).
    pub(crate) fn parse(bytes: Vec<u8>) -> Result<Meta, Error> {
        Fields::new(&bytes, 0..bytes.len()).full()?;
        let children = children(&bytes, 4..bytes.len(), MAX_CHILDREN)?;
        let mut once = HashMap::new();
        for (i, child) in children.iter().enumerate() {
            if ONCE.contains(&&child.kind) && once.insert(child.kind, i).is_some() {
                return Err(Error::Broken(format!("two {} boxes", name(&child.kind))));
            }
        }
        let need = |kind: &[u8; 4]| {
            once.get(kind)
                .copied()
                .ok_or_else(|| Error::Broken(format!("no {} box", name(kind))))
        };
        let (hdlr, pitm, iinf, iloc) = (
            need(b"hdlr")?,
            need(b"pitm")?,
            need(b"iinf")?,
            need(b"iloc")?,
        );
        let mut meta = Meta {
            bytes: Vec::new(),
            children: Vec::new(),
            primary: 0,
            items: Vec::new(),
            iinf: (iinf, 0),
            iloc: (iloc, 0),
            locations: Vec::new(),
            extents: Vec::new(),
            iloc_end: 0,
            iref: None,
            refs: Vec::new(),
            targets: Vec::new(),
            iprp: None,
            properties: Vec::new(),
            maps: Vec::new(),
            associations: Vec::new(),
            idat: once.get(b"idat").copied(),
            grpl: None,
            groups: Vec::new(),
            entities: Vec::new(),
        };
        let content = |i: usize| children[i].content();
        handler(&bytes, content(hdlr))?;
        meta.primary = primary(&bytes, content(pitm))?;
        meta.iinf.1 = meta.read_iinf(&bytes, content(iinf))?;
        meta.iloc.1 = meta.read_iloc(&bytes, content(iloc))?;
        if let Some(&i) = once.get(b"iref") {
            meta.iref = Some((i, meta.read_iref(&bytes, content(i))?));
        }
        if let Some(&i) = once.get(b"iprp") {
            meta.read_iprp(&bytes, i, content(i))?;
        }
        if let Some(&i) = once.get(b"grpl") {
            meta.grpl = Some(i);
            meta.read_grpl(&bytes, content(i))?;
        }
        if meta.item(meta.primary).is_none() {
            return Err(Error::Broken(format!(
                "the primary item {} isn't there",
                meta.primary
            )));
        }
        meta.bytes = bytes;
        meta.children = children;
        Ok(meta)
    }

    fn read_iinf(&mut self, bytes: &[u8], range: Range<usize>) -> Result<u8, Error> {
        let mut f = Fields::new(bytes, range.clone());
        let (version, _) = f.full()?;
        let count = if version == 0 {
            u64::from(f.u16()?)
        } else {
            u64::from(f.u32()?)
        };
        let entries = children(bytes, f.at..range.end, MAX_ITEMS)?;
        if entries.len() as u64 != count {
            return Err(Error::Broken(format!(
                "iinf says {count} items and holds {}",
                entries.len()
            )));
        }
        let mut ids = HashSet::new();
        for infe in entries {
            if &infe.kind != b"infe" {
                return Err(Error::Broken(format!("a {} box in iinf", name(&infe.kind))));
            }
            let item = item(bytes, &infe)?;
            if !ids.insert(item.id) {
                return Err(Error::Broken(format!("item {} twice in iinf", item.id)));
            }
            self.items.push(item);
        }
        Ok(version)
    }

    fn read_iloc(&mut self, bytes: &[u8], range: Range<usize>) -> Result<u8, Error> {
        let mut f = Fields::new(bytes, range);
        let (version, _) = f.full()?;
        if version > 2 {
            return Err(Error::Broken(format!("iloc version {version}")));
        }
        let (a, b) = (f.u8()?, f.u8()?);
        let index = if version == 0 { 0 } else { b & 15 };
        let [offset, length, base, index] = [a >> 4, a & 15, b >> 4, index].map(usize::from);
        if let Some(bad) = [offset, length, base, index]
            .into_iter()
            .find(|s| !matches!(s, 0 | 4 | 8))
        {
            return Err(Error::Broken(format!("an iloc field of {bad} bytes")));
        }
        let count = if version < 2 {
            u32::from(f.u16()?)
        } else {
            f.u32()?
        };
        let mut ids = HashSet::new();
        for _ in 0..count {
            if self.locations.len() == MAX_ITEMS {
                return Err(Error::TooLarge(format!("over {MAX_ITEMS} item locations")));
            }
            let start = f.at;
            let id = f.id(version == 2)?;
            let method = if version == 0 {
                0
            } else {
                (f.u16()? & 0xF) as u8
            };
            if method > 2 {
                return Err(Error::Broken(format!("construction method {method}")));
            }
            let data_ref = f.u16()?;
            let base = f.uint(base)?;
            let n = f.u16()?;
            let first = self.extents.len();
            for _ in 0..n {
                if self.extents.len() == MAX_EXTENTS {
                    return Err(Error::TooLarge(format!("over {MAX_EXTENTS} extents")));
                }
                f.uint(index)?;
                let offset = f.uint(offset)?;
                let len = f.uint(length)?;
                self.extents.push(Extent { offset, len });
            }
            if !ids.insert(id) {
                return Err(Error::Broken(format!("item {id} twice in iloc")));
            }
            self.locations.push(Location {
                id,
                method,
                data_ref,
                base,
                extents: first..self.extents.len(),
                range: start..f.at,
            });
        }
        self.iloc_end = f.at;
        Ok(version)
    }

    fn read_iref(&mut self, bytes: &[u8], range: Range<usize>) -> Result<u8, Error> {
        let mut f = Fields::new(bytes, range.clone());
        let (version, _) = f.full()?;
        if version > 1 {
            return Err(Error::Broken(format!("iref version {version}")));
        }
        for child in children(bytes, f.at..range.end, MAX_ITEMS)? {
            let mut f = Fields::new(bytes, child.content());
            let from = f.id(version == 1)?;
            let count = f.u16()?;
            let first = self.targets.len();
            for _ in 0..count {
                if self.targets.len() == MAX_REFS {
                    return Err(Error::TooLarge(format!("over {MAX_REFS} references")));
                }
                self.targets.push(f.id(version == 1)?);
            }
            self.refs.push(Ref {
                kind: child.kind,
                from,
                to: first..self.targets.len(),
                child,
            });
        }
        Ok(version)
    }

    fn read_iprp(&mut self, bytes: &[u8], at: usize, range: Range<usize>) -> Result<(), Error> {
        let kids = children(bytes, range, MAX_CHILDREN)?;
        let mut ipco = None;
        for (i, child) in kids.iter().enumerate() {
            if &child.kind == b"ipco" {
                if ipco.replace(i).is_some() {
                    return Err(Error::Broken("two ipco boxes".into()));
                }
                self.properties = children(bytes, child.content(), MAX_PROPERTIES)?;
            }
        }
        let mut entries = 0;
        for (i, child) in kids.iter().enumerate() {
            if &child.kind != b"ipma" {
                continue;
            }
            let mut f = Fields::new(bytes, child.content());
            let (version, flags) = f.full()?;
            if version > 1 {
                return Err(Error::Broken(format!("ipma version {version}")));
            }
            let mut map = Map {
                child: i,
                version,
                flags,
                entries: Vec::new(),
                end: 0,
            };
            for _ in 0..f.u32()? {
                entries += 1;
                if entries > MAX_ITEMS {
                    return Err(Error::TooLarge(format!("over {MAX_ITEMS} ipma entries")));
                }
                let start = f.at;
                let item = f.id(version == 1)?;
                let first = self.associations.len();
                for _ in 0..f.u8()? {
                    if self.associations.len() == MAX_ASSOCIATIONS {
                        return Err(Error::TooLarge(format!(
                            "over {MAX_ASSOCIATIONS} property associations"
                        )));
                    }
                    let (essential, index) = if map.wide() {
                        let v = f.u16()?;
                        (v & 0x8000 != 0, v & 0x7FFF)
                    } else {
                        let v = f.u8()?;
                        (v & 0x80 != 0, u16::from(v & 0x7F))
                    };
                    if usize::from(index) > self.properties.len() {
                        return Err(Error::Broken(format!("property {index} isn't there")));
                    }
                    self.associations.push(Association { essential, index });
                }
                map.entries.push(MapEntry {
                    item,
                    props: first..self.associations.len(),
                    range: start..f.at,
                });
            }
            map.end = f.at;
            self.maps.push(map);
        }
        self.iprp = Some(Iprp {
            child: at,
            children: kids,
            ipco,
        });
        Ok(())
    }

    fn read_grpl(&mut self, bytes: &[u8], range: Range<usize>) -> Result<(), Error> {
        for child in children(bytes, range, MAX_CHILDREN)? {
            let mut f = Fields::new(bytes, child.content());
            f.full()?;
            let id = f.u32()?;
            let first = self.entities.len();
            for _ in 0..f.u32()? {
                if self.entities.len() == MAX_REFS {
                    return Err(Error::TooLarge(format!("over {MAX_REFS} group entities")));
                }
                self.entities.push(f.u32()?);
            }
            self.groups.push(Group {
                id,
                entities: first..self.entities.len(),
                list_end: f.at,
                child,
            });
        }
        Ok(())
    }

    /// Item `id`.
    pub(crate) fn item(&self, id: u32) -> Option<&Item> {
        self.items.iter().find(|item| item.id == id)
    }

    /// Item `id`'s `iloc` entry (the tools look entries up by a map).
    #[cfg(test)]
    pub(crate) fn location(&self, id: u32) -> Option<&Location> {
        self.locations.iter().find(|loc| loc.id == id)
    }

    /// `loc`'s extents with its base offset added; one past any number is
    /// broken.
    pub(crate) fn spans(&self, loc: &Location) -> Result<Vec<Span>, Error> {
        self.extents[loc.extents.clone()]
            .iter()
            .map(|e| {
                let start = loc.base.checked_add(e.offset);
                let len = (e.len > 0).then_some(e.len);
                match start {
                    Some(start) if start.checked_add(e.len).is_some() => Ok(Span { start, len }),
                    _ => Err(Error::Broken(format!(
                        "item {}'s extent past any number",
                        loc.id
                    ))),
                }
            })
            .collect()
    }

    /// The ids `r` refers to.
    pub(crate) fn to(&self, r: &Ref) -> &[u32] {
        &self.targets[r.to.clone()]
    }

    /// The entity ids `g` groups.
    pub(crate) fn members(&self, g: &Group) -> &[u32] {
        &self.entities[g.entities.clone()]
    }

    /// `entry`'s associations.
    pub(crate) fn props(&self, entry: &MapEntry) -> &[Association] {
        &self.associations[entry.props.clone()]
    }

    /// `idat`'s content, in the bytes.
    pub(crate) fn idat(&self) -> Option<Range<usize>> {
        self.idat.map(|i| self.children[i].content())
    }

    /// The properties item `id` has, in its associations' order.
    pub(crate) fn properties_of(&self, id: u32) -> impl Iterator<Item = &Child> + '_ {
        self.maps
            .iter()
            .flat_map(|map| &map.entries)
            .filter(move |entry| entry.item == id)
            .flat_map(|entry| self.props(entry))
            .filter_map(|a| self.properties.get(usize::from(a.index).checked_sub(1)?))
    }

    /// What auxiliary image item `id` is, by its `auxC` property; `None`
    /// when it has none.
    // Compress on HEIC reads it (D28).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn aux(&self, id: u32) -> Option<Aux> {
        let auxc = self.properties_of(id).find(|p| &p.kind == b"auxC")?;
        let mut f = Fields::new(&self.bytes, auxc.content());
        f.full().ok()?;
        let urn = f.text();
        Some(match urn {
            b"urn:mpeg:hevc:2015:auxid:1" | b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha" => {
                Aux::Alpha
            }
            b"urn:mpeg:hevc:2015:auxid:2" | b"urn:mpeg:mpegB:cicp:systems:auxiliary:depth" => {
                Aux::Depth
            }
            b"urn:com:apple:photo:2020:aux:hdrgainmap" => Aux::GainMap,
            _ if urn.starts_with(b"urn:com:apple:photo:2018:aux:")
                || (urn.starts_with(b"urn:com:apple:photo:") && urn.ends_with(b"matte")) =>
            {
                Aux::Matte
            }
            _ => Aux::Other,
        })
    }

    /// Whether item `id` is a thumbnail: it has an outgoing `thmb`.
    // Compress on HEIC reads it (D28).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_thumbnail(&self, id: u32) -> bool {
        self.refs.iter().any(|r| &r.kind == b"thmb" && r.from == id)
    }
}

/// Checks that `hdlr` names pictures.
fn handler(bytes: &[u8], range: Range<usize>) -> Result<(), Error> {
    let mut f = Fields::new(bytes, range);
    f.full()?;
    f.u32()?;
    match &f.kind()? {
        b"pict" => Ok(()),
        other => Err(Error::Broken(format!("a meta box for {}", name(other)))),
    }
}

/// The primary item's id, from `pitm`.
fn primary(bytes: &[u8], range: Range<usize>) -> Result<u32, Error> {
    let mut f = Fields::new(bytes, range);
    match f.full()?.0 {
        0 => f.id(false),
        1 => f.id(true),
        v => Err(Error::Broken(format!("pitm version {v}"))),
    }
}

/// The item an `infe` box describes.
fn item(bytes: &[u8], infe: &Child) -> Result<Item, Error> {
    let mut f = Fields::new(bytes, infe.content());
    let (version, flags) = f.full()?;
    let (id, kind, xmp) = match version {
        // An item with a MIME type and no item type.
        0 | 1 => {
            let id = f.id(false)?;
            f.u16()?;
            f.text();
            (id, *b"mime", is_xmp(f.text()))
        }
        2 | 3 => {
            let id = f.id(version == 3)?;
            f.u16()?;
            let kind = f.kind()?;
            f.text();
            let xmp = &kind == b"mime" && is_xmp(f.text());
            (id, kind, xmp)
        }
        v => return Err(Error::Broken(format!("infe version {v}"))),
    };
    Ok(Item {
        id,
        kind,
        hidden: flags & 1 != 0,
        xmp,
        range: infe.range.clone(),
    })
}

/// Whether a MIME type is XMP's.
fn is_xmp(content_type: &[u8]) -> bool {
    content_type.eq_ignore_ascii_case(XMP_TYPE)
}

/// A box type, for a message.
pub(crate) fn name(kind: &[u8; 4]) -> String {
    String::from_utf8_lossy(kind).into_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use super::super::tests::{
        self as build, bx, described, full, iphone, layouts, meta_at, read, simple, wide, Place,
        DEPTH, EXIF, GAIN, THUMB, TMAP, XMP,
    };
    use super::*;

    /// The `meta` of the HEIF `file`, parsed.
    fn meta(file: &[u8]) -> Result<Meta, Error> {
        let top = Top::read(&mut Cursor::new(file), file.len() as u64)?;
        let header = file_meta(&top)?;
        let bytes = super::super::boxes::load(&mut Cursor::new(file), &header)?;
        Meta::parse(bytes)
    }

    #[test]
    fn an_iphone_like_file_reads_with_every_role() {
        let (file, layout) = iphone().build();
        let m = meta(&file).unwrap();
        assert_eq!(m.primary, 1);
        let role = |id| m.item(id).unwrap().role();
        assert_eq!(role(1), Role::Image);
        assert_eq!(role(2), Role::Image);
        assert_eq!(role(EXIF), Role::Exif);
        assert_eq!(role(XMP), Role::Xmp);
        assert_eq!(role(TMAP), Role::Image);
        let iptc = Item {
            id: 1,
            kind: *b"iptc",
            hidden: true,
            xmp: false,
            range: 0..0,
        };
        assert!(iptc.role() == Role::Iptc && iptc.role().metadata());
        assert!(role(EXIF).metadata() && role(XMP).metadata());
        assert!(!role(GAIN).metadata() && !role(1).metadata());
        assert!(m.item(2).unwrap().hidden);
        assert!(m.is_thumbnail(THUMB));
        assert!(!m.is_thumbnail(1));
        assert_eq!(m.aux(GAIN), Some(Aux::GainMap));
        assert_eq!(m.aux(DEPTH), Some(Aux::Depth));
        assert_eq!(m.aux(1), None);
        // The grid's descriptor is in idat; the rest in mdat.
        let grid = m.location(1).unwrap();
        assert_eq!(grid.method, 1);
        assert_eq!(read(&file, &m, 1), layout.data(1));
        for id in [2, 3, THUMB, EXIF, XMP, GAIN, DEPTH] {
            assert_eq!(read(&file, &m, id), layout.data(id), "{id}");
        }
        // The gain map's XMP describes it.
        let cdsc: Vec<_> = m.refs.iter().filter(|r| &r.kind == b"cdsc").collect();
        assert!(cdsc.iter().any(|r| r.from == XMP && m.to(r) == [GAIN]));
        assert_eq!(m.groups.len(), 1);
        assert_eq!(m.members(&m.groups[0]), [TMAP, 1]);
    }

    #[test]
    fn every_layout_reads_its_items_where_the_builder_put_them() {
        for (name, b) in layouts() {
            let (file, layout) = b.build();
            let m = meta(&file).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            for item in &b.items {
                assert_eq!(m.item(item.id).unwrap().kind, item.kind, "{name}");
                if item.place != Place::Nowhere {
                    assert_eq!(read(&file, &m, item.id), layout.data(item.id), "{name}");
                }
            }
            assert_eq!(m.primary, b.primary, "{name}");
        }
    }

    #[test]
    fn wide_ids_and_wide_fields_read() {
        let (file, layout) = wide().build();
        let m = meta(&file).unwrap();
        assert_eq!(m.iinf.1, 1);
        assert_eq!(m.iloc.1, 2);
        assert_eq!(m.iref.unwrap().1, 1);
        assert_eq!(m.primary, 70_000);
        assert_eq!(m.maps[0].version, 1);
        assert!(m.maps[0].wide());
        // 64-bit base offsets, each where its item's data starts.
        let exif = m.location(70_002).unwrap();
        let start = layout.file_ranges(70_002)[0].start;
        assert_eq!(exif.base, start);
        assert_eq!(
            m.spans(exif).unwrap(),
            [Span {
                start,
                len: Some(layout.data(70_002).len() as u64)
            }]
        );
        assert_eq!(read(&file, &m, 70_003), layout.data(70_003));
    }

    #[test]
    fn properties_come_by_their_associations() {
        let (file, _) = described().build();
        let m = meta(&file).unwrap();
        let kinds: Vec<_> = m.properties_of(1).map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            [*b"hvcC", *b"ispe", *b"udes", *b"colr", *b"crtt", *b"mdft", *b"irot"]
        );
        assert!(m.props(&m.maps[0].entries[0])[0].essential);
    }

    #[test]
    fn a_sequence_and_a_meta_too_many_or_missing_are_told() {
        let (file, _) = simple().build();
        let top = |f: &[u8]| Top::read(&mut Cursor::new(f), f.len() as u64).unwrap();
        assert!(!is_sequence(&top(&file)));
        let mut seq = file.clone();
        seq.extend_from_slice(&bx(b"moov", b""));
        assert!(is_sequence(&top(&seq)));
        let mut two = file.clone();
        two.extend_from_slice(&bx(b"meta", &[0; 4]));
        assert!(matches!(file_meta(&top(&two)), Err(Error::Broken(_))));
        let none = bx(b"ftyp", b"heic\0\0\0\0mif1");
        assert!(matches!(file_meta(&top(&none)), Err(Error::Broken(_))));
    }

    #[test]
    fn a_meta_missing_a_box_or_holding_one_twice_is_broken() {
        for kind in [b"hdlr", b"pitm", b"iinf", b"iloc"] {
            let mut b = simple();
            b.leave_out = vec![*kind];
            let (file, _) = b.build();
            assert!(matches!(meta(&file), Err(Error::Broken(_))), "{kind:?}");
        }
        let mut b = simple();
        b.extra_meta = vec![full(b"pitm", 0, 0, &[0, 1])];
        assert!(matches!(meta(&b.build().0), Err(Error::Broken(_))));
        // Not pictures.
        let mut b = simple();
        b.handler = *b"vide";
        assert!(matches!(meta(&b.build().0), Err(Error::Broken(_))));
        // The primary isn't an item.
        let mut b = simple();
        b.primary = 9;
        assert!(matches!(meta(&b.build().0), Err(Error::Broken(_))));
    }

    #[test]
    fn an_item_twice_is_broken() {
        let mut b = simple();
        let again = b.items[0].clone();
        b.items.push(again);
        assert!(matches!(meta(&b.build().0), Err(Error::Broken(_))));
        // In iloc only.
        let mut b = simple();
        b.iloc_twice = Some(1);
        assert!(matches!(meta(&b.build().0), Err(Error::Broken(_))));
    }

    #[test]
    fn versions_and_sizes_the_format_doesnt_have_are_broken() {
        let mut b = simple();
        b.sizes = [3, 4, 0, 0];
        assert!(matches!(meta(&b.build().0), Err(Error::Broken(_))));
        let (file, _) = simple().build();
        let m = meta(&file).unwrap();
        let at = meta_at(&file);
        for (child, version) in [(m.iloc.0, 3), (m.iref.unwrap().0, 2)] {
            let mut f = file.clone();
            f[at + m.children[child].content().start] = version;
            assert!(matches!(meta(&f), Err(Error::Broken(_))), "{child}");
        }
        // A property index past ipco.
        let mut b = simple();
        b.items[0].props.push((40, false));
        assert!(matches!(meta(&b.build().0), Err(Error::Broken(_))));
    }

    #[test]
    fn every_cap_is_refused_never_cut_short() {
        // Items: one too many.
        let mut b = simple();
        b.items.truncate(1);
        for id in 2..=MAX_ITEMS as u32 + 1 {
            b.items
                .push(build::Item::new(id, *b"hvt1", Place::Nowhere, Vec::new()));
        }
        b.wide = true;
        assert!(matches!(meta(&b.build().0), Err(Error::TooLarge(_))));
        b.items.pop();
        assert_eq!(meta(&b.build().0).unwrap().items.len(), MAX_ITEMS);
        // Extents: an item has at most 65,535, and the primary has one.
        let mut b = simple();
        b.items.truncate(1);
        let mut left = MAX_EXTENTS;
        for id in 2.. {
            let n = left.min(65_535);
            let mut item = build::Item::new(id, *b"hvt1", Place::Mdat, vec![1; n]);
            item.extents = n;
            b.items.push(item);
            left -= n;
            if left == 0 {
                break;
            }
        }
        assert!(matches!(meta(&b.build().0), Err(Error::TooLarge(_))));
        b.items[1].extents -= 1;
        assert_eq!(meta(&b.build().0).unwrap().extents.len(), MAX_EXTENTS);
        // References.
        let mut b = simple();
        b.refs = (0..MAX_REFS / 65_535 + 1)
            .map(|_| (*b"dimg", 1, vec![1; 65_535]))
            .collect();
        assert!(matches!(meta(&b.build().0), Err(Error::TooLarge(_))));
        // Properties and associations.
        let mut b = simple();
        b.properties = vec![bx(b"free", b""); MAX_PROPERTIES + 1];
        assert!(matches!(meta(&b.build().0), Err(Error::TooLarge(_))));
        let mut b = simple();
        b.wide_indices = true;
        b.items[0].props = vec![(1, false); 255];
        for id in 100..100 + (MAX_ASSOCIATIONS / 255) as u32 {
            let mut item = build::Item::new(id, *b"hvt1", Place::Nowhere, Vec::new());
            item.props = vec![(1, false); 255];
            b.items.push(item);
        }
        b.wide = true;
        let r = meta(&b.build().0);
        assert!(matches!(r, Err(Error::TooLarge(_))), "{r:?}");
    }

    #[test]
    fn a_meta_cut_at_every_byte_is_broken_and_never_panics() {
        let (file, _) = iphone().build();
        let m = meta(&file).unwrap();
        let bytes = &m.bytes;
        // Cut between its boxes, it may hold what it needs still.
        let ends: Vec<usize> = m.children.iter().map(|c| c.range.end).collect();
        for n in 0..bytes.len() {
            let cut = Meta::parse(bytes[..n].to_vec());
            assert!(cut.is_err() || ends.contains(&n), "{n}");
        }
    }

    #[test]
    fn junk_never_panics() {
        let (file, _) = iphone().build();
        let m = meta(&file).unwrap();
        // Every byte of meta set to a few values in turn.
        for at in 0..m.bytes.len() {
            for v in [0, 1, 0x7F, 0xFF] {
                let mut bytes = m.bytes.clone();
                bytes[at] = v;
                let _ = Meta::parse(bytes);
            }
        }
    }
}
