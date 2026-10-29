//! Remove Metadata on HEIC (D28): `meta` rebuilt in place without the
//! metadata, a `free` box where it shrank, so no offset changes (when
//! nothing follows it, the file ends sooner instead); nothing is decoded
//! and no offset is rewritten. It works on any HEIF the
//! [model](super::isobmff) reads, AVIF content named `.heic` too, and
//! needs no helper.
//!
//! - **Dropped and reported:**
//!   - every Exif item ([`exif::walk`] on its payload after its 4-byte
//!     header offset and an `Exif\0\0`; the first [`MAX_EXIF`] of it, and
//!     Other beyond), `mime` item (XMP, or Other), `uri ` item (Other),
//!     `iptc` item (IPTC), and any other item that describes another (a
//!     `cdsc` from it) but a picture (Other: a region annotation, Exif
//!     under another type): its entries and references go (an `iref` left
//!     with none too), and its bytes are zeroed in `mdat` or `idat`;
//!   - the `udes` (title, description, tags), `crtt` and `mdft`
//!     properties, the non-empty ones reported: each box is removed from
//!     `ipco`, its associations from `ipma`, and every later index is
//!     renumbered, so no unknown box is left in `ipco`;
//!   - top-level XMP and C2PA `uuid` boxes, a Samsung motion photo's
//!     `mpvd` and other unknown boxes that hold no kept data: each becomes
//!     a zeroed `free` box of the same size, or is cut when it's past the
//!     last box Grape keeps; bytes that aren't a box are cut (Other,
//!     unless all zero); existing `free` and `skip` boxes are zeroed,
//!     unreported (old editors leave metadata there).
//! - **Kept byte for byte:** every other item (the primary, a grid's
//!   tiles, thumbnails, alpha, depth, Portrait mattes, gain maps, `tmap`),
//!   every other property (`colr`, `irot`, `imir`, `clap`, `pixi`,
//!   `hvcC`, `ispe`, `auxC`, `pasp`, …), `ftyp`, `etyp`, `hdlr`, `pitm`,
//!   `dinf`, `grpl` (less the dropped ids), `idat` (dropped data zeroed)
//!   and each kept `iloc` entry, copied as it was. No minimal Exif goes
//!   back: `irot` and `imir` turn the picture, and `colr` holds its color.
//! - **Refused** ([`LAYOUT`]): a dropped item whose data is in another
//!   item (construction method 2) or another file, has an extent of
//!   length 0 (to the end), overlaps kept data, or lies outside every
//!   `mdat` and `idat`; kept data inside `meta`, or inside a box that's
//!   dropped or zeroed; a picture made of a dropped item (`dimg`), or
//!   whose data is in one; a primary item that is metadata. A sequence is
//!   refused too ([`SEQUENCE`]). Kept data that names another data
//!   reference counts as this file's, as libheif reads it: it's kept,
//!   overlapped by nothing dropped, and checked.
//! - **Checked:** [`Plan::check`] reads the output again before it's
//!   published.
//! - **Memory:** the source's `meta` (within
//!   [`MAX_META`](super::isobmff::boxes::MAX_META)) and its model, and the
//!   rebuilt `meta`, written straight into one buffer of the source's
//!   size: twice a `meta` of data, and 4.74 times one packed at every cap
//!   of the model, whose model is 2.4 times its size (`tests/heic_heap.rs`).
//!   What's written is let go of before the check, which holds the
//!   output's `meta` and its model. Besides, one Exif payload's first
//!   [`MAX_EXIF`], and 64 KiB buffers.

use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::ops::Range;

use super::exif::{self, MAX_EXIF};
use super::isobmff::boxes::{load, Child, Fields, Form, Header, Top, MAX_BOXES};
use super::isobmff::items::{file_meta, is_sequence, Location, Meta, Role};
use super::strip::{read_at, read_exact, Error, Found, In, Out};
use crate::MetaField;

/// Why an image sequence is refused.
pub(crate) const SEQUENCE: &str =
    "is an image sequence. Remove Metadata works on still HEIC photos for now.";

/// Why a layout Remove Metadata can't rewrite safely is refused.
pub(crate) const LAYOUT: &str = "is laid out in a way Remove Metadata can't handle yet.";

/// An XMP `uuid` box's user type.
const XMP_UUID: [u8; 16] = [
    0xBE, 0x7A, 0xCF, 0xCB, 0x97, 0xA9, 0x42, 0xE8, 0x9C, 0x71, 0x99, 0x94, 0x91, 0xE3, 0xAF, 0xAC,
];

/// A C2PA `uuid` box's user type.
const C2PA_UUID: [u8; 16] = [
    0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
];

/// The properties that are metadata.
const METADATA_PROPERTIES: [&[u8; 4]; 3] = [b"udes", b"crtt", b"mdft"];

/// How much is read or written at a time.
const STEP: usize = 64 * 1024;

/// What becomes of a top-level box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// Kept as it is: `ftyp`, `etyp`, `mdat`, `imda`, and unknown boxes
    /// that hold kept data. Dropped data in `mdat` is zeroed. The output
    /// ends with the last of these, or `meta`.
    Keep,
    /// The one `meta`, rebuilt.
    Meta,
    /// `free` or `skip`: its content zeroed.
    Zero,
    /// A zeroed `free` box of the same size.
    Free,
}

/// What Remove Metadata finds in a HEIF file ([module docs](self)).
struct Survey {
    top: Top,
    /// The file-level `meta` box.
    meta_box: Header,
    meta: Meta,
    /// The items dropped.
    dropped: HashSet<u32>,
    /// For each of `ipco`'s properties, whether it's removed.
    removed: Vec<bool>,
    /// Each top-level box's fate.
    fates: Vec<Fate>,
    /// Where the output ends: past it, boxes and bytes are cut.
    end: u64,
    /// Where kept data is in the file, and in `idat`'s content: merged,
    /// sorted.
    kept_file: Vec<Range<u64>>,
    /// Where dropped data is in the file and in `idat`'s content.
    drop_file: Vec<Range<u64>>,
    drop_idat: Vec<Range<u64>>,
    found: Found,
}

impl Survey {
    /// Reads the HEIF `r` (`len` bytes), and finds what goes and what
    /// stays, or refuses it ([module docs](self)).
    fn new(r: &mut dyn In, len: u64) -> Result<Survey, Error> {
        let top = Top::read(r, len)?;
        if top.boxes.first().map(|b| &b.kind) != Some(b"ftyp") {
            return Err(Error::Broken("no ftyp box first".into()));
        }
        if is_sequence(&top) {
            return Err(Error::Refused(SEQUENCE.into()));
        }
        let meta_box = file_meta(&top)?;
        let meta = Meta::parse(load(r, &meta_box)?)?;
        // Metadata by its type, and any other item that describes another
        // (a `cdsc` from it) but a picture: IPTC under another type, Exif
        // under a type that isn't Exif, a region annotation.
        let describes: HashSet<u32> = meta
            .refs
            .iter()
            .filter(|r| &r.kind == b"cdsc")
            .map(|r| r.from)
            .collect();
        let dropped: HashSet<u32> = meta
            .items
            .iter()
            .filter(|item| {
                let role = item.role();
                role.metadata() || (role != Role::Image && describes.contains(&item.id))
            })
            .map(|item| item.id)
            .collect();
        if dropped.contains(&meta.primary) {
            return Err(Error::Refused(LAYOUT.into()));
        }
        let mut s = Survey {
            top,
            meta_box,
            dropped,
            removed: meta
                .properties
                .iter()
                .map(|p| METADATA_PROPERTIES.contains(&&p.kind))
                .collect(),
            meta,
            fates: Vec::new(),
            end: 0,
            kept_file: Vec::new(),
            drop_file: Vec::new(),
            drop_idat: Vec::new(),
            found: Found::default(),
        };
        s.place_data()?;
        s.check_pictures()?;
        s.judge_boxes(r)?;
        s.report_items(r)?;
        s.report_properties();
        Ok(s)
    }

    /// Finds where kept and dropped data is, and refuses what can't be
    /// rewritten safely.
    fn place_data(&mut self) -> Result<(), Error> {
        let len = self.top.len;
        let idat = self.meta.idat().map_or(0, |r| r.len() as u64);
        let mut kept_file = Vec::new();
        let mut kept_idat = Vec::new();
        for loc in &self.meta.locations {
            let dropped = self.dropped.contains(&loc.id);
            if dropped && (loc.method == 2 || loc.data_ref != 0) {
                return Err(Error::Refused(LAYOUT.into()));
            }
            // Data in other items has no place of its own. Kept data that
            // names another data reference is read from this file all the
            // same, as libheif reads it (and a self-contained `url ` says).
            if loc.method == 2 {
                continue;
            }
            let size = if loc.method == 0 { len } else { idat };
            for span in self.meta.spans(loc)? {
                if dropped && span.len.is_none() {
                    return Err(Error::Refused(LAYOUT.into()));
                }
                let range = span.start..span.end(size);
                if range.end > size || range.start > range.end {
                    return Err(Error::Broken(format!(
                        "item {}'s data runs past its end",
                        loc.id
                    )));
                }
                if range.is_empty() {
                    continue;
                }
                match (dropped, loc.method) {
                    (true, 0) => self.drop_file.push(range),
                    (true, _) => self.drop_idat.push(range),
                    (false, 0) => kept_file.push(range),
                    (false, _) => kept_idat.push(range),
                }
            }
        }
        let kept_file = merged(kept_file);
        let kept_idat = merged(kept_idat);
        if kept_file.last().is_some_and(|r| r.end > self.top.tail) {
            return Err(Error::Broken(
                "an item's data runs past the file's boxes".into(),
            ));
        }
        let meta = self.meta_box.at..self.meta_box.end();
        if overlaps(&kept_file, &meta) {
            return Err(Error::Refused(LAYOUT.into()));
        }
        let boxes = &self.top.boxes;
        for range in &self.drop_file {
            // The box it starts in: boxes are end to end.
            let at = boxes.partition_point(|b| b.end() <= range.start);
            let in_mdat = boxes.get(at).is_some_and(|b| {
                &b.kind == b"mdat" && b.content().start <= range.start && range.end <= b.end()
            });
            if !in_mdat || overlaps(&kept_file, range) {
                return Err(Error::Refused(LAYOUT.into()));
            }
        }
        if self.drop_idat.iter().any(|r| overlaps(&kept_idat, r)) {
            return Err(Error::Refused(LAYOUT.into()));
        }
        self.kept_file = kept_file;
        self.drop_file = merged(std::mem::take(&mut self.drop_file));
        Ok(())
    }

    /// Refuses a kept picture made of a dropped item (`dimg`), or whose
    /// data is in one (`iloc` references, for construction method 2).
    fn check_pictures(&self) -> Result<(), Error> {
        let made_of_dropped = self.meta.refs.iter().any(|r| {
            matches!(&r.kind, b"dimg" | b"iloc")
                && !self.dropped.contains(&r.from)
                && self.meta.to(r).iter().any(|to| self.dropped.contains(to))
        });
        if made_of_dropped {
            return Err(Error::Refused(LAYOUT.into()));
        }
        Ok(())
    }

    /// Gives each top-level box its fate, works out where the output ends,
    /// and reports the boxes and bytes that go.
    fn judge_boxes(&mut self, r: &mut dyn In) -> Result<(), Error> {
        let mut end = 0;
        let mut fates = Vec::with_capacity(self.top.boxes.len());
        for b in &self.top.boxes {
            let holds = overlaps(&self.kept_file, &(b.at..b.end()));
            let fate = match &b.kind {
                b"meta" => Fate::Meta,
                b"ftyp" | b"etyp" | b"mdat" | b"imda" => Fate::Keep,
                b"free" | b"skip" => Fate::Zero,
                _ if holds && !metadata_box(r, b)? => Fate::Keep,
                _ => Fate::Free,
            };
            // Kept data in a box that's zeroed, or in an XMP, C2PA or
            // motion-photo box.
            if holds && fate != Fate::Keep {
                return Err(Error::Refused(LAYOUT.into()));
            }
            if matches!(fate, Fate::Keep | Fate::Meta) {
                end = b.end();
            }
            if fate == Fate::Free {
                if let Some(field) = dropped_box(r, b)? {
                    self.found.fields.insert(field);
                }
            }
            fates.push(fate);
        }
        if self.top.tail < self.top.len && !zeros(r, self.top.tail..self.top.len)? {
            self.found.fields.insert(MetaField::Other);
        }
        self.fates = fates;
        self.end = end;
        Ok(())
    }

    /// Reports the dropped items: Exif walked, XMP, other `mime` and
    /// `uri ` items. What's read of Exif items comes to the file's size at
    /// most, as when no two share bytes: past it, one is reported unread,
    /// as Other.
    fn report_items(&mut self, r: &mut dyn In) -> Result<(), Error> {
        let locations: HashMap<u32, &Location> =
            self.meta.locations.iter().map(|l| (l.id, l)).collect();
        let mut left = self.top.len;
        let mut fields = Vec::new();
        for item in self
            .meta
            .items
            .iter()
            .filter(|i| self.dropped.contains(&i.id))
        {
            match item.role() {
                Role::Exif => {
                    if let Some(loc) = locations.get(&item.id) {
                        fields.extend(self.exif(r, loc, &mut left)?);
                    }
                }
                Role::Xmp => fields.push(MetaField::Xmp),
                Role::Iptc => fields.push(MetaField::Iptc),
                _ => fields.push(MetaField::Other),
            }
        }
        self.found.fields.extend(fields);
        Ok(())
    }

    /// What the Exif item at `loc` holds: [`exif::walk`] on the first
    /// [`MAX_EXIF`] of its TIFF structure, and Other past that, or for a
    /// header offset past its end, or when what it reads would pass
    /// `left`.
    fn exif(
        &self,
        r: &mut dyn In,
        loc: &Location,
        left: &mut u64,
    ) -> Result<Vec<MetaField>, Error> {
        let pieces = self.pieces(loc)?;
        let total: u64 = pieces.iter().map(|p| p.end - p.start).sum();
        if total == 0 {
            return Ok(Vec::new());
        }
        let head = self.item_bytes(r, loc, &pieces, 0, 4)?;
        let Ok(head) = <[u8; 4]>::try_from(head.as_slice()) else {
            return Ok(vec![MetaField::Other]);
        };
        let skip = 4 + u64::from(u32::from_be_bytes(head));
        if skip > total {
            return Ok(vec![MetaField::Other]);
        }
        let take = (total - skip).min(MAX_EXIF as u64);
        match left.checked_sub(4 + take) {
            Some(rest) => *left = rest,
            None => return Ok(vec![MetaField::Other]),
        }
        let data = self.item_bytes(r, loc, &pieces, skip, take)?;
        let tiff = data.strip_prefix(b"Exif\0\0").unwrap_or(&data);
        let mut fields: Vec<MetaField> = exif::walk(tiff).fields.into_iter().collect();
        if total - skip > take {
            fields.push(MetaField::Other);
        }
        Ok(fields)
    }

    /// A dropped item's extents, in the file or in `idat`'s content.
    fn pieces(&self, loc: &Location) -> Result<Vec<Range<u64>>, Error> {
        // Dropped data has lengths (checked), and lies within its space.
        Ok(self
            .meta
            .spans(loc)?
            .into_iter()
            .map(|s| s.start..s.start + s.len.unwrap_or(0))
            .collect())
    }

    /// `take` bytes of an item's data from `skip` on, its `pieces` end to
    /// end; fewer when it ends first.
    fn item_bytes(
        &self,
        r: &mut dyn In,
        loc: &Location,
        pieces: &[Range<u64>],
        skip: u64,
        take: u64,
    ) -> Result<Vec<u8>, Error> {
        let idat = self.meta.idat().unwrap_or(0..0);
        let mut out = Vec::new();
        let mut at = 0u64;
        for piece in pieces {
            let len = piece.end - piece.start;
            let from = skip.max(at);
            let to = (skip + take).min(at + len);
            if from < to {
                let start = piece.start + (from - at);
                let n = (to - from) as usize;
                if loc.method == 0 {
                    let mut buf = vec![0; n];
                    read_at(r, start, &mut buf)?;
                    out.extend_from_slice(&buf);
                } else {
                    let s = idat.start + start as usize;
                    out.extend_from_slice(&self.meta.bytes[s..s + n]);
                }
            }
            at += len;
        }
        Ok(out)
    }

    /// Reports the removed properties' non-empty fields.
    fn report_properties(&mut self) {
        for (p, _) in self
            .meta
            .properties
            .iter()
            .zip(&self.removed)
            .filter(|(_, &removed)| removed)
        {
            let mut f = Fields::new(&self.meta.bytes, p.content());
            let full = f.full().is_ok();
            match &p.kind {
                b"udes" => {
                    // Its language, then name, description and tags; a
                    // string cut short reads as far as it goes.
                    f.text();
                    for field in [MetaField::Title, MetaField::Comment, MetaField::Keywords] {
                        if !f.text().is_empty() {
                            self.found.fields.insert(field);
                        }
                    }
                }
                kind => {
                    let field = if kind == b"crtt" {
                        MetaField::CreationDate
                    } else {
                        MetaField::ModDate
                    };
                    // A time that can't be read is still there.
                    if !full || f.uint(8).map_or(true, |t| t != 0) {
                        self.found.fields.insert(field);
                    }
                }
            }
        }
    }

    /// Whether anything changes in `meta`.
    fn meta_changes(&self) -> bool {
        !self.dropped.is_empty() || self.removed.contains(&true)
    }
}

/// A `uuid` box's user type; `None` for another box, or one too short.
fn user_type(r: &mut dyn In, b: &Header) -> Result<Option<[u8; 16]>, Error> {
    let content = b.content();
    if &b.kind != b"uuid" || content.end - content.start < 16 {
        return Ok(None);
    }
    let mut user = [0; 16];
    read_at(r, content.start, &mut user)?;
    Ok(Some(user))
}

/// Whether a top-level box is one Remove Metadata drops by its type: XMP,
/// C2PA, a motion photo's video.
fn metadata_box(r: &mut dyn In, b: &Header) -> Result<bool, Error> {
    Ok(&b.kind == b"mpvd" || user_type(r, b)?.is_some_and(|u| u == XMP_UUID || u == C2PA_UUID))
}

/// What a top-level box that goes held, by its type: `None` for an empty
/// one.
fn dropped_box(r: &mut dyn In, b: &Header) -> Result<Option<MetaField>, Error> {
    if b.content().is_empty() {
        return Ok(None);
    }
    Ok(Some(match (&b.kind, user_type(r, b)?) {
        (b"mpvd", _) => MetaField::Embedded,
        (_, Some(XMP_UUID)) => MetaField::Xmp,
        _ => MetaField::Other,
    }))
}

/// `ranges` sorted and merged where they touch or overlap, in place.
fn merged(mut ranges: Vec<Range<u64>>) -> Vec<Range<u64>> {
    ranges.sort_unstable_by_key(|r| (r.start, r.end));
    ranges.dedup_by(|next, last| {
        let touches = next.start <= last.end;
        if touches {
            last.end = last.end.max(next.end);
        }
        touches
    });
    ranges
}

/// Whether `range` overlaps any of `merged` (sorted, apart).
fn overlaps(merged: &[Range<u64>], range: &Range<u64>) -> bool {
    let i = merged.partition_point(|m| m.end <= range.start);
    merged.get(i).is_some_and(|m| m.start < range.end)
}

/// Whether the file's bytes in `range` are all zero.
fn zeros(r: &mut dyn In, range: Range<u64>) -> Result<bool, Error> {
    let mut buf = vec![0; STEP];
    let mut at = range.start;
    while at < range.end {
        let n = (range.end - at).min(STEP as u64) as usize;
        read_at(r, at, &mut buf[..n])?;
        if buf[..n].iter().any(|&b| b != 0) {
            return Ok(false);
        }
        at += n as u64;
    }
    Ok(true)
}

/// Starts a box of `kind` in `form` (an explicit size for one that ran to
/// the end) at the end of `out`, for [`end_box`] to give its size; gives
/// where it starts.
fn start_box(out: &mut Vec<u8>, kind: &[u8; 4], form: Form) -> usize {
    let at = out.len();
    out.extend_from_slice(&Header::write(kind, form, form.head()));
    at
}

/// Sets the size of the box [`start_box`] started at `at` in `form`: the
/// rest of `out`. A rebuilt box is never bigger than the source's, so a
/// 32-bit size still fits.
fn end_box(out: &mut [u8], at: usize, form: Form) {
    let len = out.len() - at;
    match form {
        Form::Large => out[at + 8..at + 16].copy_from_slice(&(len as u64).to_be_bytes()),
        Form::Small | Form::ToEnd => {
            let len = u32::try_from(len).unwrap_or(u32::MAX);
            out[at..at + 4].copy_from_slice(&len.to_be_bytes());
        }
    }
}

/// Writes an item id: 32 bits when `wide`, 16 otherwise (an id the source
/// wrote in 16 bits).
fn put_id(out: &mut Vec<u8>, id: u32, wide: bool) {
    if wide {
        out.extend_from_slice(&id.to_be_bytes());
    } else {
        out.extend_from_slice(&(id as u16).to_be_bytes());
    }
}

/// Writes `meta` rebuilt without what `s` drops at the end of `out`
/// ([module docs](self)), each box straight into it.
fn rebuild(s: &Survey, out: &mut Vec<u8>) {
    let m = &s.meta;
    let form = match s.meta_box.form {
        Form::Large => Form::Large,
        Form::Small | Form::ToEnd => Form::Small,
    };
    let at = start_box(out, b"meta", form);
    out.extend_from_slice(&m.bytes[..4]);
    for (i, child) in m.children.iter().enumerate() {
        if i == m.iinf.0 {
            rebuild_iinf(s, child, out);
        } else if i == m.iloc.0 {
            rebuild_iloc(s, child, out);
        } else if m.iref.is_some_and(|(at, _)| at == i) {
            rebuild_iref(s, child, out);
        } else if m.iprp.as_ref().is_some_and(|p| p.child == i) {
            rebuild_iprp(s, child, out);
        } else if m.idat == Some(i) {
            let from = out.len() + child.form.head() as usize;
            out.extend_from_slice(&m.bytes[child.range.clone()]);
            for range in &s.drop_idat {
                out[from + range.start as usize..from + range.end as usize].fill(0);
            }
        } else if m.grpl == Some(i) {
            rebuild_grpl(s, child, out);
        } else {
            out.extend_from_slice(&m.bytes[child.range.clone()]);
        }
    }
    end_box(out, at, form);
}

fn rebuild_iinf(s: &Survey, child: &Child, out: &mut Vec<u8>) {
    let m = &s.meta;
    let start = child.content().start;
    let kept = || m.items.iter().filter(|item| !s.dropped.contains(&item.id));
    let n = kept().count();
    let at = start_box(out, b"iinf", child.form);
    out.extend_from_slice(&m.bytes[start..start + 4]);
    if m.iinf.1 == 0 {
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
    for item in kept() {
        out.extend_from_slice(&m.bytes[item.range.clone()]);
    }
    end_box(out, at, child.form);
}

fn rebuild_iloc(s: &Survey, child: &Child, out: &mut Vec<u8>) {
    let m = &s.meta;
    let start = child.content().start;
    let kept = || {
        m.locations
            .iter()
            .filter(|loc| !s.dropped.contains(&loc.id))
    };
    let n = kept().count();
    let at = start_box(out, b"iloc", child.form);
    // Its version, flags and field sizes.
    out.extend_from_slice(&m.bytes[start..start + 6]);
    if m.iloc.1 < 2 {
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
    for loc in kept() {
        out.extend_from_slice(&m.bytes[loc.range.clone()]);
    }
    out.extend_from_slice(&m.bytes[m.iloc_end..child.range.end]);
    end_box(out, at, child.form);
}

/// `iref` without the dropped ids, and its boxes left with none; nothing
/// when no box is left.
fn rebuild_iref(s: &Survey, child: &Child, out: &mut Vec<u8>) {
    let m = &s.meta;
    let start = child.content().start;
    let wide = m.iref.is_some_and(|(_, v)| v == 1);
    let at = start_box(out, b"iref", child.form);
    out.extend_from_slice(&m.bytes[start..start + 4]);
    let mut any = false;
    for r in m.refs.iter().filter(|r| !s.dropped.contains(&r.from)) {
        let to = m.to(r);
        let kept = || to.iter().copied().filter(|id| !s.dropped.contains(id));
        let n = kept().count();
        if n == to.len() {
            out.extend_from_slice(&m.bytes[r.child.range.clone()]);
        } else if n > 0 {
            let b = start_box(out, &r.kind, r.child.form);
            put_id(out, r.from, wide);
            out.extend_from_slice(&(n as u16).to_be_bytes());
            for id in kept() {
                put_id(out, id, wide);
            }
            end_box(out, b, r.child.form);
        } else {
            continue;
        }
        any = true;
    }
    if any {
        end_box(out, at, child.form);
    } else {
        // An iref left with no references goes.
        out.truncate(at);
    }
}

fn rebuild_iprp(s: &Survey, child: &Child, out: &mut Vec<u8>) {
    let m = &s.meta;
    let Some(iprp) = &m.iprp else {
        out.extend_from_slice(&m.bytes[child.range.clone()]);
        return;
    };
    // Each index's new number: less the removed properties before it.
    let mut renumbered = vec![0u16; s.removed.len() + 1];
    let mut gone = 0;
    for (i, &removed) in s.removed.iter().enumerate() {
        if removed {
            gone += 1;
        }
        renumbered[i + 1] = (i + 1 - gone) as u16;
    }
    let at = start_box(out, b"iprp", child.form);
    // The ipma boxes, in iprp's order.
    let mut maps = m.maps.iter().peekable();
    for (i, kid) in iprp.children.iter().enumerate() {
        if iprp.ipco == Some(i) {
            let ipco = start_box(out, b"ipco", kid.form);
            for (p, _) in m.properties.iter().zip(&s.removed).filter(|(_, &r)| !r) {
                out.extend_from_slice(&m.bytes[p.range.clone()]);
            }
            end_box(out, ipco, kid.form);
        } else if let Some(map) = maps.next_if(|map| map.child == i) {
            let start = kid.content().start;
            let ipma = start_box(out, b"ipma", kid.form);
            out.extend_from_slice(&m.bytes[start..start + 4]);
            let entries = || map.entries.iter().filter(|e| !s.dropped.contains(&e.item));
            out.extend_from_slice(&(entries().count() as u32).to_be_bytes());
            for entry in entries() {
                let props = m.props(entry);
                let unchanged = props.iter().all(|a| {
                    let i = usize::from(a.index);
                    i == 0 || (!s.removed[i - 1] && renumbered[i] == a.index)
                });
                if unchanged {
                    out.extend_from_slice(&m.bytes[entry.range.clone()]);
                    continue;
                }
                let kept = || {
                    props
                        .iter()
                        .filter(|a| a.index == 0 || !s.removed[usize::from(a.index) - 1])
                };
                put_id(out, entry.item, map.version == 1);
                out.push(kept().count() as u8);
                for a in kept() {
                    let index = renumbered[usize::from(a.index)];
                    if map.wide() {
                        out.extend_from_slice(
                            &(index | u16::from(a.essential) << 15).to_be_bytes(),
                        );
                    } else {
                        out.push(index as u8 | u8::from(a.essential) << 7);
                    }
                }
            }
            out.extend_from_slice(&m.bytes[map.end..kid.range.end]);
            end_box(out, ipma, kid.form);
        } else {
            out.extend_from_slice(&m.bytes[kid.range.clone()]);
        }
    }
    end_box(out, at, child.form);
}

fn rebuild_grpl(s: &Survey, child: &Child, out: &mut Vec<u8>) {
    let m = &s.meta;
    let at = start_box(out, b"grpl", child.form);
    for g in &m.groups {
        let members = m.members(g);
        let kept = || members.iter().copied().filter(|id| !s.dropped.contains(id));
        let n = kept().count();
        if n == members.len() {
            out.extend_from_slice(&m.bytes[g.child.range.clone()]);
        } else if n > 0 {
            let start = g.child.content().start;
            let b = start_box(out, &g.child.kind, g.child.form);
            // Its version, flags and group id.
            out.extend_from_slice(&m.bytes[start..start + 8]);
            out.extend_from_slice(&(n as u32).to_be_bytes());
            for id in kept() {
                out.extend_from_slice(&id.to_be_bytes());
            }
            out.extend_from_slice(&m.bytes[g.list_end..g.child.range.end]);
            end_box(out, b, g.child.form);
        }
    }
    end_box(out, at, child.form);
}

/// What the output has instead of a source range.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    /// Zeros.
    Zero(Range<u64>),
    /// The plan's bytes in `bytes`, from `at`.
    Write { at: u64, bytes: Range<usize> },
}

impl Edit {
    fn range(&self) -> Range<u64> {
        match self {
            Edit::Zero(range) => range.clone(),
            Edit::Write { at, bytes } => *at..*at + bytes.len() as u64,
        }
    }
}

/// An item Remove Metadata keeps, for the check: where its `iloc` entry is
/// in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    id: u32,
    at: u64,
    len: usize,
}

/// Remove Metadata's plan for one HEIF file: what to write, and what goes.
#[derive(Debug)]
pub(crate) struct Plan {
    /// The output's size.
    len: u64,
    /// What changes, sorted, apart.
    edits: Vec<Edit>,
    /// The bytes the edits write: `meta` and `free` headers.
    bytes: Vec<u8>,
    found: Found,
    primary: u32,
    /// Every item it keeps with an `iloc` entry, in `iloc`'s order.
    kept: Vec<Kept>,
    dropped: HashSet<u32>,
    /// Where the source's `idat` content is in the file.
    idat_at: Option<u64>,
}

impl Plan {
    /// Reads the HEIF `r` (`len` bytes) and plans its rewrite, or refuses
    /// it ([module docs](self)).
    pub(crate) fn new(r: &mut dyn In, len: u64) -> Result<Plan, Error> {
        let s = Survey::new(r, len)?;
        let mut edits = Vec::new();
        // What the edits write, each in its place: the rebuilt meta and the
        // free box after it, which fit in the source's meta, and a free
        // header for each box that goes.
        let freed = s.fates.iter().filter(|&&f| f == Fate::Free).count();
        let room = if s.meta_changes() {
            s.meta_box.len as usize
        } else {
            0
        };
        let mut bytes = Vec::with_capacity(room + 16 * freed);
        let mut len = s.end;
        let mut padded = false;
        if s.meta_changes() {
            rebuild(&s, &mut bytes);
            // A meta that ends the output ends it sooner, with no free box
            // after it: Samsung's ftyp, mdat, meta.
            let last = s.meta_box.end() == s.end;
            let saved = s
                .meta_box
                .len
                .checked_sub(bytes.len() as u64)
                .filter(|&saved| last || saved == 0 || saved >= 8)
                .ok_or_else(|| Error::Refused(LAYOUT.into()))?;
            if last {
                len -= saved;
            } else if saved > 0 {
                bytes.extend_from_slice(&Header::write(b"free", Form::Small, saved));
                padded = true;
            }
            let at = s.meta_box.at;
            edits.push(Edit::Write {
                at,
                bytes: 0..bytes.len(),
            });
            if !last {
                let written = at + bytes.len() as u64;
                edits.push(Edit::Zero(written..s.meta_box.end()));
            }
        }
        // The output's boxes: those it keeps, and the free box after meta.
        // Over the cap, Grape couldn't read what it wrote.
        let boxes = s.top.boxes.iter().take_while(|b| b.at < s.end).count();
        if boxes + usize::from(padded) > MAX_BOXES {
            return Err(Error::TooLarge(format!(
                "it would write over {MAX_BOXES} top-level boxes"
            )));
        }
        for (b, fate) in s.top.boxes.iter().zip(&s.fates) {
            if b.at >= s.end {
                break;
            }
            let content = b.content();
            match fate {
                Fate::Zero => edits.push(Edit::Zero(content)),
                Fate::Free => {
                    let header = Header::write(b"free", b.form, b.len);
                    let start = bytes.len();
                    bytes.extend_from_slice(&header);
                    edits.push(Edit::Write {
                        at: b.at,
                        bytes: start..bytes.len(),
                    });
                    edits.push(Edit::Zero(b.at + header.len() as u64..b.end()));
                }
                Fate::Keep | Fate::Meta => {}
            }
        }
        for range in &s.drop_file {
            if range.start < s.end {
                edits.push(Edit::Zero(range.start..range.end.min(s.end)));
            }
        }
        edits.retain(|e| !e.range().is_empty());
        edits.sort_unstable_by_key(|e| e.range().start);
        if edits
            .windows(2)
            .any(|w| w[0].range().end > w[1].range().start)
        {
            // The drops are in mdat, apart from meta and the boxes freed.
            return Err(Error::Refused(LAYOUT.into()));
        }
        let m = &s.meta;
        let content = s.meta_box.content().start;
        let kept = m
            .locations
            .iter()
            .filter(|loc| !s.dropped.contains(&loc.id))
            .map(|loc| Kept {
                id: loc.id,
                at: content + loc.range.start as u64,
                len: loc.range.len(),
            })
            .collect();
        let idat_at = m.idat().map(|r| content + r.start as u64);
        Ok(Plan {
            len,
            edits,
            bytes,
            found: s.found,
            primary: m.primary,
            kept,
            idat_at,
            dropped: s.dropped,
        })
    }

    /// Writes the plan into `w`, copying from the source `r` it was made
    /// from, once; gives what went. What it writes is let go of as it
    /// returns, so the check never holds it beside the output's `meta`.
    pub(crate) fn write(&mut self, r: &mut dyn In, w: &mut dyn Out) -> Result<Found, Error> {
        let edits = std::mem::take(&mut self.edits);
        let bytes = std::mem::take(&mut self.bytes);
        r.seek(SeekFrom::Start(0)).map_err(Error::Read)?;
        let mut buf = vec![0; STEP];
        let mut at = 0;
        for edit in &edits {
            let range = edit.range();
            copy(r, w, range.start - at, &mut buf)?;
            skip(r, range.end - range.start, &mut buf)?;
            match edit {
                Edit::Zero(range) => {
                    buf.fill(0);
                    let mut left = range.end - range.start;
                    while left > 0 {
                        let n = left.min(STEP as u64) as usize;
                        w.write_all(&buf[..n]).map_err(Error::Write)?;
                        left -= n as u64;
                    }
                }
                Edit::Write { bytes: written, .. } => {
                    w.write_all(&bytes[written.clone()]).map_err(Error::Write)?;
                }
            }
            at = range.end;
        }
        copy(r, w, self.len - at, &mut buf)?;
        Ok(self.found.clone())
    }

    /// Reads the output `out` (`len` bytes) again against the source `src`
    /// it came from: it must be the size planned; the reporter must find
    /// nothing in it; it must have the same primary item, every kept
    /// `iloc` entry byte for byte (read again from the source) with its
    /// data the same bytes, and no reference from a dropped item.
    /// [`Error::Broken`] says what's wrong.
    pub(crate) fn check(&self, src: &mut dyn In, out: &mut dyn In, len: u64) -> Result<(), Error> {
        let wrong = |why: String| Err(Error::Broken(format!("the output {why}")));
        if len != self.len {
            return wrong(format!("is {len} bytes, not {}", self.len));
        }
        let s = Survey::new(out, len)?;
        if !s.found.fields.is_empty() {
            return wrong(format!("still holds {:?}", s.found.fields));
        }
        if s.meta_changes() {
            return wrong("still holds metadata items or properties".into());
        }
        let m = &s.meta;
        if m.primary != self.primary {
            return wrong(format!("has primary item {}", m.primary));
        }
        if let Some(r) = m.refs.iter().find(|r| self.dropped.contains(&r.from)) {
            return wrong(format!("has a reference from dropped item {}", r.from));
        }
        let locations: HashMap<u32, _> = m.locations.iter().map(|l| (l.id, l)).collect();
        if locations.len() != self.kept.len() {
            return wrong("has other items".into());
        }
        let idat = m.idat().map_or(0, |r| r.len() as u64);
        // Every kept item's data, each byte once however many items share
        // it, in the file and in idat.
        let mut file = Vec::new();
        let mut in_idat = Vec::new();
        let mut entry = Vec::new();
        for kept in &self.kept {
            let Some(loc) = locations.get(&kept.id) else {
                return wrong(format!("lost item {}", kept.id));
            };
            // The source's entry, read again.
            entry.resize(kept.len, 0);
            read_at(src, kept.at, &mut entry)?;
            if m.bytes[loc.range.clone()] != entry[..] {
                return wrong(format!("changed item {}'s location", kept.id));
            }
            if loc.method == 2 {
                continue;
            }
            for span in m.spans(loc)? {
                match loc.method {
                    0 => file.push(span.start..span.end(len)),
                    _ => in_idat.push(span.start..span.end(idat)),
                }
            }
        }
        let idat_at = m
            .idat()
            .map(|r| s.meta_box.content().start + r.start as u64);
        let mut places = vec![(0, 0, merged(file))];
        let in_idat = merged(in_idat);
        if !in_idat.is_empty() {
            let (Some(from), Some(to)) = (self.idat_at, idat_at) else {
                return wrong("lost its idat".into());
            };
            places.push((from, to, in_idat));
        }
        let mut a = vec![0; STEP];
        let mut b = vec![0; STEP];
        for (from, to, ranges) in places {
            for range in ranges {
                let mut at = range.start;
                while at < range.end {
                    let n = (range.end - at).min(STEP as u64) as usize;
                    read_at(src, from + at, &mut a[..n])?;
                    read_at(out, to + at, &mut b[..n])?;
                    if a[..n] != b[..n] {
                        return wrong(format!("changed the data at {}", to + at));
                    }
                    at += n as u64;
                }
            }
        }
        Ok(())
    }
}

/// Copies `n` bytes from `r` to `w`.
fn copy(r: &mut dyn In, w: &mut dyn Out, mut n: u64, buf: &mut [u8]) -> Result<(), Error> {
    while n > 0 {
        let k = n.min(buf.len() as u64) as usize;
        read_exact(r, &mut buf[..k])?;
        w.write_all(&buf[..k]).map_err(Error::Write)?;
        n -= k as u64;
    }
    Ok(())
}

/// Reads past `n` bytes of `r`.
fn skip(r: &mut dyn In, mut n: u64, buf: &mut [u8]) -> Result<(), Error> {
    while n > 0 {
        let k = n.min(buf.len() as u64) as usize;
        read_exact(r, &mut buf[..k])?;
        n -= k as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeSet;
    use std::io::Cursor;

    use super::super::isobmff::boxes::load;
    use super::super::isobmff::tests::{
        bx, described, iphone, layouts, meta_at, read, simple, uuid, Builder, Item, Place, C2PA,
        EXIF, GAIN, SECRET_XMP, XMP, XMP_UUID as XMP_USER_TYPE,
    };
    use super::*;
    use MetaField::*;

    fn plan(file: &[u8]) -> Result<Plan, Error> {
        Plan::new(&mut Cursor::new(file), file.len() as u64)
    }

    /// Strips `file`, checks the output against it, and gives the output
    /// and what went.
    fn strip(file: &[u8]) -> (Vec<u8>, BTreeSet<MetaField>) {
        let mut plan = plan(file).unwrap();
        let mut out = Cursor::new(Vec::new());
        let found = plan.write(&mut Cursor::new(file), &mut out).unwrap();
        let out = out.into_inner();
        plan.check(
            &mut Cursor::new(file),
            &mut Cursor::new(&out),
            out.len() as u64,
        )
        .unwrap();
        (out, found.fields)
    }

    /// The `meta` of `file`.
    fn meta(file: &[u8]) -> Meta {
        let top = Top::read(&mut Cursor::new(file), file.len() as u64).unwrap();
        let header = file_meta(&top).unwrap();
        Meta::parse(load(&mut Cursor::new(file), &header).unwrap()).unwrap()
    }

    fn refused(file: &[u8]) -> String {
        match plan(file) {
            Err(Error::Refused(why)) => why,
            other => panic!("{other:?}"),
        }
    }

    /// Where item `id`'s `iloc` entry is in `file`.
    fn entry_at(file: &[u8], id: u32) -> usize {
        meta_at(file) + meta(file).location(id).unwrap().range.start
    }

    /// Whether `needle` is anywhere in `hay`.
    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn every_layout_keeps_its_pictures_byte_for_byte_and_drops_its_metadata() {
        for (name, b) in layouts() {
            let (src, layout) = b.build();
            let (out, _) = strip(&src);
            // The same size, but for what's cut from the end, and what a
            // meta that ends the file saved.
            let cut = !b.after.is_empty() || !b.tail.is_empty() || b.meta_after_mdat;
            assert_eq!(out.len() == src.len(), !cut, "{name}");
            let m = meta(&out);
            for item in &b.items {
                let metadata = matches!(&item.kind, b"Exif" | b"mime" | b"uri ");
                assert_eq!(m.item(item.id).is_some(), !metadata, "{name}: {}", item.id);
                if metadata {
                    assert!(m.location(item.id).is_none(), "{name}");
                    if item.place == Place::Mdat {
                        for r in layout.file_ranges(item.id) {
                            assert!(out[r.start as usize..r.end as usize]
                                .iter()
                                .all(|&b| b == 0));
                        }
                    }
                } else if item.place != Place::Nowhere {
                    assert_eq!(read(&out, &m, item.id), layout.data(item.id), "{name}");
                }
            }
            assert!(m.refs.iter().all(|r| m.item(r.from).is_some()), "{name}");
            assert!(!has(&out, b"Secret"), "{name}");
            // Everything before meta is the same, and so is everything from
            // mdat on, but what's dropped.
            let at = meta_at(&src) - 8;
            assert_eq!(out[..at.min(28)], src[..at.min(28)], "{name}");
        }
    }

    #[test]
    fn the_report_says_exactly_what_went() {
        let report = |b: Builder| strip(&b.build().0).1;
        let photo: BTreeSet<_> = [Location, Camera, CreationDate, Xmp].into();
        assert_eq!(report(simple()), photo);
        assert_eq!(
            report(described()),
            [
                Location,
                Camera,
                Title,
                Comment,
                Keywords,
                CreationDate,
                ModDate,
                Xmp
            ]
            .into()
        );
        let mut b = simple();
        b.after = vec![uuid(C2PA, b"manifest")];
        assert_eq!(report(b), &photo | &[Other].into());
        let mut b = simple();
        b.items.truncate(1);
        b.refs.clear();
        b.between = vec![uuid(XMP_USER_TYPE, b"<x/>")];
        assert_eq!(report(b), [Xmp].into());
        let mut b = simple();
        b.after = vec![bx(b"mpvd", b"video")];
        assert_eq!(report(b), &photo | &[Embedded].into());
        // Trailing zeros aren't reported, other bytes are; an empty box
        // isn't either.
        let mut b = simple();
        b.tail = vec![0; 100];
        b.after = vec![bx(b"wide", b"")];
        assert_eq!(report(b), photo);
        let mut b = simple();
        b.tail = vec![0, 0, 1];
        assert_eq!(report(b), &photo | &[Other].into());
        // Blank properties aren't reported.
        let mut b = described();
        b.items.truncate(1);
        b.refs.clear();
        b.properties[2] = super::super::isobmff::tests::udes("", "", "");
        b.properties[4] = super::super::isobmff::tests::time(b"crtt", 0);
        assert_eq!(report(b), [ModDate].into());
        // A mime item that isn't XMP, a uri item.
        let mut b = simple();
        b.items[2] = Item::mime(XMP, "text/plain", b"note");
        let mut uri = Item::new(13, *b"uri ", Place::Mdat, b"payload".to_vec());
        uri.text = Some("urn:example");
        b.items.push(uri);
        assert_eq!(report(b), [Location, Camera, CreationDate, Other].into());
    }

    #[test]
    fn items_that_describe_another_go_whatever_their_type_but_pictures() {
        let mut b = simple();
        let item =
            |id, kind: &[u8; 4], data: &[u8]| Item::new(id, *kind, Place::Mdat, data.to_vec());
        b.items.extend([
            // IPTC: a city.
            item(12, b"iptc", b"\x1c\x02\x5a\x00\x0aSecretCity"),
            // An Exif payload under a type that isn't Exif.
            item(13, b"E\xEAif", b"\0\0\0\0MM\0\x2aSecretExif"),
            // A region annotation.
            item(14, b"rgan", b"SecretRegion"),
            // An item of another type that describes nothing, and a
            // picture that describes the primary: kept.
            item(15, b"abcd", b"kept"),
            Item::picture(16, 0xC6, 20),
        ]);
        for id in [12, 13, 14, 16] {
            b.refs.push((*b"cdsc", id, vec![1]));
        }
        let (src, layout) = b.build();
        let (out, found) = strip(&src);
        assert_eq!(
            found,
            [Location, Camera, CreationDate, Xmp, Iptc, Other].into()
        );
        let m = meta(&out);
        for id in [12, 13, 14] {
            assert!(m.item(id).is_none(), "{id}");
            for r in layout.file_ranges(id) {
                assert!(out[r.start as usize..r.end as usize]
                    .iter()
                    .all(|&b| b == 0));
            }
        }
        for id in [15, 16] {
            assert_eq!(read(&out, &m, id), layout.data(id), "{id}");
        }
        assert!(!has(&out, b"Secret"));
        let refs: Vec<_> = m.refs.iter().map(|r| (r.kind, r.from)).collect();
        assert_eq!(refs, [(*b"cdsc", 16)]);
    }

    #[test]
    fn an_exif_payload_is_walked_after_its_offset_and_capped() {
        let tiff = super::super::isobmff::tests::camera();
        let mut b = simple();
        b.items.truncate(2);
        b.refs.truncate(1);
        // Offset 0, the TIFF right after it.
        let mut payload = 0u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&tiff);
        b.items[1].data = payload;
        assert_eq!(
            strip(&b.build().0).1,
            [Location, Camera, CreationDate].into()
        );
        // An offset past its end.
        b.items[1].data = vec![0, 0, 1, 0, 1, 2];
        assert_eq!(strip(&b.build().0).1, [Other].into());
        // Past 1 MiB, only the first is walked, and the rest is Other.
        let mut big = 6u32.to_be_bytes().to_vec();
        big.extend_from_slice(b"Exif\0\0");
        big.extend_from_slice(&tiff);
        big.resize(4 + 6 + MAX_EXIF + 1, 0);
        b.items[1].data = big;
        b.items[1].extents = 3;
        assert_eq!(
            strip(&b.build().0).1,
            [Location, Camera, CreationDate, Other].into()
        );
    }

    /// A file that counts the bytes read from it.
    struct Tally {
        file: Cursor<Vec<u8>>,
        read: u64,
    }

    impl std::io::Read for Tally {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.file.read(buf)?;
            self.read += n as u64;
            Ok(n)
        }
    }

    impl std::io::Seek for Tally {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.file.seek(pos)
        }
    }

    #[test]
    fn exif_items_that_share_bytes_are_read_up_to_the_files_size() {
        // Fifty Exif items whose extents are all the first's 200 KB.
        let mut b = simple();
        b.items.truncate(2);
        b.refs.truncate(1);
        b.items[1].data.resize(200_000, 0);
        for id in 100..150 {
            b.items.push(Item::exif(id, b"x"));
        }
        let (mut file, layout) = b.build();
        let shared = layout.file_ranges(EXIF)[0].clone();
        for id in 100..150 {
            let at = entry_at(&file, id) + 8;
            file[at..at + 4].copy_from_slice(&(shared.start as u32).to_be_bytes());
            let len = (shared.end - shared.start) as u32;
            file[at + 4..at + 8].copy_from_slice(&len.to_be_bytes());
        }
        let len = file.len() as u64;
        let mut r = std::io::BufReader::with_capacity(
            64 * 1024,
            Tally {
                file: Cursor::new(file.clone()),
                read: 0,
            },
        );
        let p = Plan::new(&mut r, len).unwrap();
        // The first is walked; past the file's size, the rest are unread.
        assert_eq!(
            p.found.fields,
            [Location, Camera, CreationDate, Other].into()
        );
        assert!(r.get_ref().read < 3 * len, "{} of {len}", r.get_ref().read);
        let (out, _) = strip(&file);
        assert!(out[shared.start as usize..shared.end as usize]
            .iter()
            .all(|&b| b == 0));
    }

    #[test]
    fn udes_crtt_and_mdft_leave_ipco_and_later_indices_are_renumbered() {
        let (src, _) = described().build();
        let (out, _) = strip(&src);
        let m = meta(&out);
        let kinds: Vec<_> = m.properties.iter().map(|p| p.kind).collect();
        assert_eq!(kinds, [*b"hvcC", *b"ispe", *b"colr", *b"irot"]);
        let primary = m.maps[0].entries.iter().find(|e| e.item == 1).unwrap();
        let props: Vec<_> = m
            .props(primary)
            .iter()
            .map(|a| (a.index, a.essential))
            .collect();
        assert_eq!(props, [(1, true), (2, false), (3, false), (4, true)]);
        // The properties' own bytes are the source's.
        let src_meta = meta(&src);
        for (p, q) in m.properties_of(1).zip(
            src_meta
                .properties_of(1)
                .filter(|p| !METADATA_PROPERTIES.contains(&&p.kind)),
        ) {
            assert_eq!(m.bytes[p.range.clone()], src_meta.bytes[q.range.clone()]);
        }
        // With 15-bit indices too.
        let mut b = described();
        b.wide_indices = true;
        let (out, _) = strip(&b.build().0);
        let m = meta(&out);
        let props: Vec<_> = m
            .props(&m.maps[0].entries[0])
            .iter()
            .map(|a| a.index)
            .collect();
        assert_eq!(props, [1, 2, 3, 4]);
    }

    #[test]
    fn meta_shrinks_in_place_behind_a_free_box_of_what_it_saved() {
        for (name, b) in [("simple", simple()), ("iphone", iphone())] {
            let (src, _) = b.build();
            let (out, _) = strip(&src);
            let top = |f: &[u8]| Top::read(&mut Cursor::new(f), f.len() as u64).unwrap();
            let (before, after) = (top(&src), top(&out));
            let old = file_meta(&before).unwrap();
            let new = file_meta(&after).unwrap();
            assert_eq!(new.at, old.at, "{name}");
            let free = after.boxes.iter().find(|b| b.at == new.end()).unwrap();
            assert_eq!(&free.kind, b"free", "{name}");
            assert_eq!(free.end(), old.end(), "{name}");
            assert!(out[free.content().start as usize..free.end() as usize]
                .iter()
                .all(|&b| b == 0));
            // Every box after it is where it was.
            let rest = |t: &Top| -> Vec<(u64, [u8; 4])> {
                t.boxes
                    .iter()
                    .filter(|b| b.at >= old.end())
                    .map(|b| (b.at, b.kind))
                    .collect()
            };
            assert_eq!(rest(&after), rest(&before), "{name}");
        }
        // The forms: a largesize meta stays one; a size-0 meta gets a size.
        for (name, b) in layouts() {
            let (src, _) = b.build();
            let (out, _) = strip(&src);
            let t = Top::read(&mut Cursor::new(&out), out.len() as u64).unwrap();
            let form = file_meta(&t).unwrap().form;
            match b.meta_form {
                Form::Large => assert_eq!(form, Form::Large, "{name}"),
                _ => assert_eq!(form, Form::Small, "{name}"),
            }
        }
    }

    #[test]
    fn a_meta_that_ends_the_file_shrinks_with_no_free_box_after_it() {
        // ftyp, mdat, meta, as Samsung lays it out, and with a motion
        // photo's video after it, which is cut.
        for after in [vec![], vec![bx(b"mpvd", b"SecretClip")]] {
            let mut b = simple();
            b.meta_after_mdat = true;
            b.after = after;
            let (src, _) = b.build();
            let (out, _) = strip(&src);
            let t = Top::read(&mut Cursor::new(&out), out.len() as u64).unwrap();
            let kinds: Vec<_> = t.boxes.iter().map(|b| b.kind).collect();
            assert_eq!(kinds, [*b"ftyp", *b"mdat", *b"meta"]);
            assert_eq!(t.tail, out.len() as u64);
            // A second pass copies it byte for byte, and finds nothing.
            let (again, found) = strip(&out);
            assert_eq!(again, out);
            assert!(found.is_empty());
        }
    }

    #[test]
    fn an_output_holds_as_many_top_level_boxes_as_a_source_may_and_no_more() {
        // ftyp, meta, empty free boxes, mdat: `n` boxes in all, or with
        // meta last.
        let file = |n: usize, meta_last: bool| {
            let mut b = simple();
            b.between = vec![bx(b"free", b""); n - 3];
            b.meta_after_mdat = meta_last;
            b.build().0
        };
        let boxes = |f: &[u8]| {
            Top::read(&mut Cursor::new(f), f.len() as u64)
                .unwrap()
                .boxes
                .len()
        };
        // With the free box after meta, 4,096.
        let (out, _) = strip(&file(MAX_BOXES - 1, false));
        assert_eq!(boxes(&out), MAX_BOXES);
        // Which a source of 4,096 can't have: too large, as is one more.
        for n in [MAX_BOXES, MAX_BOXES + 1] {
            assert!(
                matches!(plan(&file(n, false)), Err(Error::TooLarge(_))),
                "{n}"
            );
        }
        // A meta that ends the file adds none.
        let (out, _) = strip(&file(MAX_BOXES, true));
        assert_eq!(boxes(&out), MAX_BOXES);
        assert!(matches!(
            plan(&file(MAX_BOXES + 1, true)),
            Err(Error::TooLarge(_))
        ));
    }

    #[test]
    fn boxes_that_go_become_free_boxes_or_are_cut_past_the_last_kept() {
        let mut b = simple();
        b.before = vec![uuid(XMP_USER_TYPE, SECRET_XMP.as_bytes())];
        b.between = vec![bx(b"free", b"OldSecret"), bx(b"abcd", b"Secret stuff")];
        b.after = vec![uuid(C2PA, b"SecretSigner"), bx(b"mpvd", b"SecretClip")];
        b.tail = b"SecretJunk".to_vec();
        let (src, _) = b.build();
        let (out, found) = strip(&src);
        assert!(found.is_superset(&[Xmp, Other, Embedded].into()));
        let t = Top::read(&mut Cursor::new(&out), out.len() as u64).unwrap();
        let kinds: Vec<_> = t.boxes.iter().map(|b| b.kind).collect();
        // ftyp, the XMP uuid freed, meta, its padding, the old free, abcd
        // freed, mdat; the rest cut.
        assert_eq!(
            kinds,
            [*b"ftyp", *b"free", *b"meta", *b"free", *b"free", *b"free", *b"mdat"]
        );
        let src_top = Top::read(&mut Cursor::new(&src), src.len() as u64).unwrap();
        let mdat = src_top.all(b"mdat").next().unwrap();
        assert_eq!(out.len() as u64, mdat.end());
        assert_eq!(t.tail, out.len() as u64);
    }

    #[test]
    fn a_file_with_nothing_to_drop_is_copied_byte_for_byte() {
        let mut b = simple();
        b.items.truncate(1);
        b.refs.clear();
        b.between = vec![bx(b"free", &[0; 10])];
        b.tail = vec![0; 3];
        let (src, _) = b.build();
        let p = plan(&src).unwrap();
        assert!(p.bytes.is_empty());
        let (out, found) = strip(&src[..src.len() - 3]);
        assert_eq!(out, src[..src.len() - 3]);
        assert!(found.is_empty());
        // Trailing zeros are cut, unsaid.
        let (out, found) = strip(&src);
        assert_eq!(out, src[..src.len() - 3]);
        assert!(found.is_empty());
    }

    #[test]
    fn an_iphone_photo_keeps_its_grid_thumbnail_gain_map_depth_and_tone_map() {
        let (src, layout) = iphone().build();
        let (out, found) = strip(&src);
        assert_eq!(out.len(), src.len());
        assert_eq!(found, [Location, Camera, CreationDate, Xmp].into());
        let m = meta(&out);
        assert_eq!(m.primary, 1);
        for id in [1, 2, 3, 20, GAIN, 31, 40] {
            assert_eq!(read(&out, &m, id), layout.data(id), "{id}");
        }
        assert!(m.item(EXIF).is_none() && m.item(XMP).is_none());
        // The gain map's XMP and the Exif leave iref; the rest stays.
        let refs: Vec<_> = m
            .refs
            .iter()
            .map(|r| (r.kind, r.from, m.to(r).to_vec()))
            .collect();
        assert_eq!(
            refs,
            [
                (*b"dimg", 1, vec![2, 3]),
                (*b"thmb", 20, vec![1]),
                (*b"auxl", GAIN, vec![1]),
                (*b"auxl", 31, vec![1]),
                (*b"dimg", 40, vec![1, GAIN]),
            ]
        );
        assert_eq!(
            m.aux(GAIN),
            Some(super::super::isobmff::items::Aux::GainMap)
        );
        assert_eq!(m.members(&m.groups[0]), [40, 1]);
    }

    #[test]
    fn dropped_ids_leave_the_groups() {
        let mut b = iphone();
        b.groups.push((*b"altr", 51, vec![EXIF, XMP]));
        b.groups.push((*b"ster", 52, vec![2, EXIF, 3]));
        let (out, _) = strip(&b.build().0);
        let m = meta(&out);
        let groups: Vec<_> = m
            .groups
            .iter()
            .map(|g| (g.id, m.members(g).to_vec()))
            .collect();
        assert_eq!(groups, [(50, vec![40, 1]), (52, vec![2, 3])]);
    }

    #[test]
    fn each_layout_it_cant_rewrite_safely_is_refused() {
        let (src, layout) = simple().build();
        let exif = entry_at(&src, EXIF);
        // iloc v1, fields of 4: id (2), method (2), data ref (2), extent
        // count (2), offset (4), length (4).
        let patched = |at: usize, bytes: &[u8]| {
            let mut f = src.clone();
            f[at..at + bytes.len()].copy_from_slice(bytes);
            f
        };
        let primary_at = layout.file_ranges(1)[0].start as u32;
        for (name, file) in [
            ("in another item", patched(exif + 2, &[0, 2])),
            ("in another file", patched(exif + 4, &[0, 1])),
            ("to the end", patched(exif + 12, &[0; 4])),
            (
                "over kept data",
                patched(exif + 8, &primary_at.to_be_bytes()),
            ),
            ("outside mdat", patched(exif + 8, &[0, 0, 0, 8])),
            (
                "kept data in meta",
                patched(entry_at(&src, 1) + 8, &(meta_at(&src) as u32).to_be_bytes()),
            ),
        ] {
            assert_eq!(refused(&file), LAYOUT, "{name}");
        }
        // The primary is metadata.
        let mut b = simple();
        b.primary = EXIF;
        assert_eq!(refused(&b.build().0), LAYOUT);
        // A picture made of a dropped item.
        let mut b = simple();
        b.refs.push((*b"dimg", 1, vec![EXIF]));
        assert_eq!(refused(&b.build().0), LAYOUT);
        // Kept data in a box that's zeroed or goes: the primary's extent
        // pointed into it (after ftyp's 24 bytes and its header).
        for kind in [b"free", b"skip", b"mpvd"] {
            let mut b = simple();
            b.before = vec![bx(kind, &[0xA1; 300])];
            let (mut file, _) = b.build();
            let at = entry_at(&file, 1) + 8;
            file[at..at + 4].copy_from_slice(&32u32.to_be_bytes());
            assert_eq!(refused(&file), LAYOUT, "{kind:?}");
        }
        // A sequence.
        let mut b = simple();
        b.after = vec![bx(b"moov", b"")];
        assert_eq!(refused(&b.build().0), SEQUENCE);
    }

    #[test]
    fn kept_data_that_names_another_data_reference_is_this_files_all_the_same() {
        // The primary at data reference 1: libheif reads it from this file
        // whatever the index says, as a self-contained `url ` means.
        let (src, layout) = simple().build();
        let mut file = src.clone();
        let primary = entry_at(&file, 1);
        file[primary + 4..primary + 6].copy_from_slice(&[0, 1]);
        let data = layout.file_ranges(1)[0].clone();
        let (out, _) = strip(&file);
        let (a, b) = (data.start as usize, data.end as usize);
        assert_eq!(out[a..b], file[a..b]);
        // The check reads it as kept data.
        let mut p = plan(&file).unwrap();
        let mut o = Cursor::new(Vec::new());
        p.write(&mut Cursor::new(&file), &mut o).unwrap();
        let mut o = o.into_inner();
        o[b - 1] ^= 1;
        assert!(matches!(
            p.check(
                &mut Cursor::new(&file),
                &mut Cursor::new(&o),
                o.len() as u64
            ),
            Err(Error::Broken(_))
        ));
        // An Exif over its last 40 bytes would zero them: refused.
        let exif = entry_at(&file, EXIF) + 8;
        file[exif..exif + 4].copy_from_slice(&(data.end as u32 - 40).to_be_bytes());
        assert_eq!(refused(&file), LAYOUT);
    }

    #[test]
    fn kept_data_in_an_unknown_box_keeps_the_box() {
        // The primary's extent pointed into a box of another type.
        let mut b = simple();
        b.before = vec![bx(b"abcd", &[0xA1; 300])];
        let (mut file, _) = b.build();
        let at = entry_at(&file, 1) + 8;
        // After ftyp's 24 bytes and abcd's header.
        file[at..at + 4].copy_from_slice(&32u32.to_be_bytes());
        let (out, found) = strip(&file);
        assert_eq!(found, [Location, Camera, CreationDate, Xmp].into());
        assert_eq!(out[24..24 + 308], file[24..24 + 308]);
        // An XMP uuid box holding kept data is refused.
        let mut b = simple();
        b.before = vec![uuid(XMP_USER_TYPE, &[0xA1; 300])];
        let (mut file, _) = b.build();
        let at = entry_at(&file, 1) + 8;
        file[at..at + 4].copy_from_slice(&48u32.to_be_bytes());
        assert_eq!(refused(&file), LAYOUT);
    }

    #[test]
    fn damaged_files_are_broken() {
        let (src, layout) = simple().build();
        // Kept data past the end, or in the bytes after the last box.
        let end = layout.file_ranges(1)[0].end as usize;
        assert!(matches!(plan(&src[..end - 1]), Err(Error::Broken(_))));
        let mut b = simple();
        b.tail = vec![1; 400];
        let (mut file, _) = b.build();
        let at = entry_at(&file, 1) + 8;
        let tail = (file.len() - 400) as u32;
        file[at..at + 4].copy_from_slice(&tail.to_be_bytes());
        assert!(matches!(plan(&file), Err(Error::Broken(_))));
        // No ftyp first.
        let mut file = src.clone();
        file[4..8].copy_from_slice(b"free");
        assert!(matches!(plan(&file), Err(Error::Broken(_))));
    }

    #[test]
    fn a_file_cut_anywhere_is_refused_or_stripped_and_checked_never_a_panic() {
        let mut b = simple();
        b.after = vec![uuid(C2PA, b"manifest")];
        b.meta_after_mdat = true;
        let (src, _) = b.build();
        for n in 0..src.len() {
            let file = &src[..n];
            if let Ok(mut p) = plan(file) {
                let mut out = Cursor::new(Vec::new());
                p.write(&mut Cursor::new(file), &mut out).unwrap();
                let out = out.into_inner();
                p.check(
                    &mut Cursor::new(file),
                    &mut Cursor::new(&out),
                    out.len() as u64,
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn the_check_catches_a_planted_leftover() {
        let (src, layout) = simple().build();
        let mut p = plan(&src).unwrap();
        let mut out = Cursor::new(Vec::new());
        p.write(&mut Cursor::new(&src), &mut out).unwrap();
        let out = out.into_inner();
        let check = |o: &[u8]| p.check(&mut Cursor::new(&src), &mut Cursor::new(o), o.len() as u64);
        check(&out).unwrap();
        // The source itself: same size, still its metadata.
        assert!(matches!(check(&src), Err(Error::Broken(_))));
        // A byte of the picture changed.
        let mut o = out.clone();
        o[layout.file_ranges(1)[0].start as usize] ^= 1;
        assert!(matches!(check(&o), Err(Error::Broken(_))));
        // Another primary: the second picture.
        let mut b = simple();
        b.items.push(Item::picture(2, 0xA2, 10));
        let (src2, _) = b.build();
        let mut p2 = plan(&src2).unwrap();
        let mut out2 = Cursor::new(Vec::new());
        p2.write(&mut Cursor::new(&src2), &mut out2).unwrap();
        let mut o = out2.into_inner();
        let m = meta(&o);
        let pitm = meta_at(&o)
            + m.children
                .iter()
                .find(|c| &c.kind == b"pitm")
                .unwrap()
                .content()
                .start;
        o[pitm + 4..pitm + 6].copy_from_slice(&[0, 2]);
        assert!(matches!(
            p2.check(
                &mut Cursor::new(&src2),
                &mut Cursor::new(&o),
                o.len() as u64
            ),
            Err(Error::Broken(_))
        ));
        // A size other than planned.
        let mut o = out.clone();
        o.push(0);
        assert!(matches!(check(&o), Err(Error::Broken(_))));
    }

    #[test]
    fn many_ipma_boxes_take_a_time_in_proportion() {
        // The best of three runs with `n` empty ipma boxes.
        let time = |n: usize| {
            let mut b = described();
            b.more_ipma = n;
            let (src, _) = b.build();
            (0..3)
                .map(|_| {
                    let started = std::time::Instant::now();
                    let (out, _) = strip(&src);
                    assert_eq!(meta(&out).maps.len(), n + 1);
                    started.elapsed()
                })
                .min()
                .unwrap()
        };
        // Eight times the boxes: 64 times the time if each looked through
        // them all.
        let (few, many) = (time(8_191), time(65_534));
        assert!(many < few * 16, "{few:?}, then {many:?}");
    }

    #[test]
    fn wide_ids_and_wide_fields_are_rebuilt_as_they_were() {
        let mut b = super::super::isobmff::tests::wide();
        // A thumbnail whose reference loses the Exif's id.
        b.items.push(Item::picture(70_001, 0xC0, 40));
        b.refs.push((*b"thmb", 70_001, vec![70_000, 70_002]));
        let (src, layout) = b.build();
        let (out, found) = strip(&src);
        assert_eq!(found, [Location, Camera, CreationDate, Xmp].into());
        let m = meta(&out);
        assert_eq!((m.iinf.1, m.iloc.1, m.iref.unwrap().1), (1, 2, 1));
        assert_eq!(read(&out, &m, 70_000), layout.data(70_000));
        let refs: Vec<_> = m.refs.iter().map(|r| (r.kind, r.from, m.to(r))).collect();
        assert_eq!(refs, [(*b"thmb", 70_001, &[70_000][..])]);
    }

    #[test]
    fn an_iref_left_with_no_references_goes() {
        for b in [simple(), super::super::isobmff::tests::wide()] {
            let (src, _) = b.build();
            let (out, _) = strip(&src);
            assert!(meta(&out).iref.is_none());
            assert!(!has(&out, b"iref"));
        }
    }
}
