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
//!     Other beyond unless it's all zeros), `mime` item (XMP, or Other),
//!     `uri ` item (Other), `iptc` item (IPTC), and any other item that
//!     describes another (a `cdsc` from it) but a picture (Other: a region
//!     annotation, Exif under another type): its entries and references go
//!     (an `iref` left with none too), and its bytes are zeroed in `mdat`
//!     or `idat`;
//!   - the `udes` (title, description, tags), `crtt` and `mdft`
//!     properties, the non-empty ones reported: each box is removed from
//!     `ipco`, its associations from `ipma`, and every later index is
//!     renumbered, so no unknown box is left in `ipco`;
//!   - top-level XMP and C2PA `uuid` boxes, a Samsung motion photo's
//!     `mpvd` and other unknown boxes that hold no kept data: each becomes
//!     a zeroed `free` box of the same size, or is cut when it's past the
//!     last box Grape keeps; bytes that aren't a box are cut (Other,
//!     unless all zero: a Samsung `sefd` trailer, with the capture time
//!     and mobile country code); existing `free` and `skip` boxes are zeroed,
//!     unreported (old editors leave metadata there);
//!   - `meta`'s own `uuid` boxes (XMP, or Other: a Canon HIF's encoder
//!     version), and its `xml ` (XMP) and `bxml` (Other) boxes: left out
//!     of the rebuilt `meta`, unreported when they hold nothing past their
//!     user type, or their version and flags;
//!   - every `iloc` entry with no item (no `infe`), which no reader
//!     follows: what it points to is no item's (below);
//!   - every byte of `mdat` and `idat` that no kept item points to (still
//!     pictures reach their data only through `iloc`): zeroed, as the copy
//!     goes, and reported as Other when what no item at all points to
//!     isn't all zeros (an earlier editor's leftovers, a writer's
//!     signature).
//! - **Kept byte for byte:** every other item (the primary, a grid's
//!   tiles, thumbnails, alpha, depth, Portrait mattes, gain maps, `tmap`),
//!   every other property (`colr`, `irot`, `imir`, `clap`, `pixi`,
//!   `hvcC`, `ispe`, `auxC`, `pasp`, …), `ftyp`, `etyp`, `hdlr`, `pitm`,
//!   `dinf`, `grpl` (less the dropped ids), `idat` (all but kept data zeroed),
//!   each kept `iloc` entry and every other box in `meta`, copied as it
//!   was; `colr` holds the color.
//! - **Kept for the orientation** (D26, as JPEG's minimal Exif): the
//!   primary's Exif when its Orientation is 2 to 8, whether or not `irot`
//!   and `imir` turn the picture too, since readers that follow Exif don't
//!   look at them, and those that do ignore Exif. Its `infe`, `iloc` entry
//!   and `cdsc` stay byte for byte; its one extent is overwritten in place
//!   with its 4-byte header offset (and `Exif\0\0` where the source had
//!   it), a TIFF structure of that Orientation alone, then zeros. Its
//!   other fields are reported.
//! - **Refused** ([`LAYOUT`]): a dropped item whose data is in another
//!   item (construction method 2) or another file, has an extent of
//!   length 0 (to the end), overlaps kept data, or lies outside every
//!   `mdat` and `idat`; the primary's Exif kept for its orientation in
//!   several extents, or in too few bytes for it, or as dropped data may
//!   not be, or that a kept item refers to by anything but a `cdsc` (a
//!   picture made of it, data in it); an `iloc` entry with no item that a
//!   kept item refers to that way; kept data inside `meta`, or inside
//!   a box that's dropped or zeroed; a picture made of a dropped item
//!   (`dimg`), or whose data is in one; a primary item that is metadata.
//!   A sequence is refused too ([`SEQUENCE`]). Kept data that names
//!   another data reference counts as this file's, as libheif reads it:
//!   it's kept, overlapped by nothing dropped, and checked.
//! - **Checked:** [`Plan::check`] reads the output again before it's
//!   published, the Exif kept for the orientation holding nothing else:
//!   read byte for byte, its zeros past its first [`MAX_EXIF`] aren't
//!   other fields there.
//! - **Memory:** the source's `meta` (within
//!   [`MAX_META`](super::isobmff::boxes::MAX_META)) and its model, and the
//!   rebuilt `meta`, written straight into one buffer of the source's
//!   size, measured by `tests/heic_heap.rs`: 32.06 MiB for a 16 MiB `meta`
//!   of data, 73.25 MiB for one as large packed at every cap of the model,
//!   and 55.88 MiB for a 9.75 MiB one packed as densely as the model lets
//!   it (a model is several times the bytes it's read from, more so the
//!   denser they are, so the peak, not a ratio, is what's bounded). The
//!   kept ranges (16 bytes each) are held until the copy, which zeroes
//!   what they don't cover as it goes: 22.94 MiB for a 3.8 MiB `meta` of
//!   262,132 extents a byte apart. What's written is let go of before the
//!   check, which holds the output's `meta` and its model. Besides, one
//!   Exif payload's first [`MAX_EXIF`], and 64 KiB buffers.

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

/// The primary's Exif, kept when it turns the picture (D26), its one
/// extent overwritten in place.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Oriented {
    id: u32,
    /// 0: in the file; 1: in `idat`.
    method: u8,
    /// Its extent, in the file or in `idat`'s content.
    range: Range<u64>,
    /// What's written at its start, zeros after: its 4-byte header offset,
    /// `Exif\0\0` when the source had it there, and a TIFF structure with
    /// the Orientation alone.
    payload: Vec<u8>,
}

/// What an Exif item holds, walked.
#[derive(Debug, Default)]
struct Walked {
    fields: Vec<MetaField>,
    /// IFD0's Orientation, when it's 1 to 8.
    orientation: Option<u16>,
    /// `Exif\0\0` between its header offset and its TIFF structure, as
    /// Apple writes it.
    prefixed: bool,
    /// What follows its first [`MAX_EXIF`] after its header is other
    /// fields: a byte of it isn't zero, or it isn't read.
    rest: bool,
}

/// What a [`Survey`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// A source, to plan its rewrite.
    Source,
    /// The output Grape wrote from one, read again by [`Plan::check`].
    Output,
}

/// What Remove Metadata finds in a HEIF file ([module docs](self)).
struct Survey {
    pass: Pass,
    top: Top,
    /// The file-level `meta` box.
    meta_box: Header,
    meta: Meta,
    /// The items dropped, and the ids of `orphans`.
    dropped: HashSet<u32>,
    /// The ids of `iloc` entries with no item (no `infe`): no reader
    /// follows one, so it goes, and the bytes it points to are no item's.
    orphans: HashSet<u32>,
    /// The primary's Exif, kept for its orientation.
    oriented: Option<Oriented>,
    /// The Exif item walked to find it, reported already.
    walked: Option<u32>,
    /// What's left to read of Exif items: the file's size, at first.
    left: u64,
    /// For each of `ipco`'s properties, whether it's removed.
    removed: Vec<bool>,
    /// The places of `meta`'s boxes left out of it, in order.
    gone: Vec<usize>,
    /// Each top-level box's fate.
    fates: Vec<Fate>,
    /// Where the output ends: past it, boxes and bytes are cut.
    end: u64,
    /// Where kept data is in the file, and in `idat`'s content: merged,
    /// sorted. Every other byte of `mdat` and `idat` is zeroed.
    kept_file: Vec<Range<u64>>,
    kept_idat: Vec<Range<u64>>,
    /// Where dropped data is in the file and in `idat`'s content: merged,
    /// sorted.
    drop_file: Vec<Range<u64>>,
    drop_idat: Vec<Range<u64>>,
    /// Whether `idat` holds a byte to zero.
    idat_dirty: bool,
    found: Found,
}

impl Survey {
    /// Reads the HEIF `r` (`len` bytes) on `pass`, and finds what goes and
    /// what stays, or refuses it ([module docs](self)).
    fn new(r: &mut dyn In, len: u64, pass: Pass) -> Result<Survey, Error> {
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
        let mut dropped: HashSet<u32> = meta
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
        let items: HashSet<u32> = meta.items.iter().map(|item| item.id).collect();
        let orphans: HashSet<u32> = meta
            .locations
            .iter()
            .map(|l| l.id)
            .filter(|id| !items.contains(id))
            .collect();
        dropped.extend(&orphans);
        let mut s = Survey {
            pass,
            left: top.len,
            top,
            meta_box,
            dropped,
            orphans,
            oriented: None,
            walked: None,
            removed: meta
                .properties
                .iter()
                .map(|p| METADATA_PROPERTIES.contains(&&p.kind))
                .collect(),
            gone: Vec::new(),
            meta,
            fates: Vec::new(),
            end: 0,
            kept_file: Vec::new(),
            kept_idat: Vec::new(),
            drop_file: Vec::new(),
            drop_idat: Vec::new(),
            idat_dirty: false,
            found: Found::default(),
        };
        s.orient(r)?;
        s.place_data()?;
        s.check_pictures()?;
        s.judge_meta_boxes()?;
        s.judge_boxes(r)?;
        s.report_items(r)?;
        s.report_properties();
        s.report_gaps(r)?;
        Ok(s)
    }

    /// Keeps the primary's Exif when it turns the picture (D26): the
    /// first Exif item that describes the primary, walked (and reported:
    /// it says no more than its orientation after), with an Orientation of
    /// 2 to 8, whether or not `irot` or `imir` turn it too, so that readers
    /// that follow Exif, and those that follow `irot` and ignore Exif as
    /// the format says, see it as before. Its entries stay as they are,
    /// and its one extent gets [`Oriented::payload`]; refused when it has
    /// several, or too few bytes.
    fn orient(&mut self, r: &mut dyn In) -> Result<(), Error> {
        let m = &self.meta;
        let describe_primary: HashSet<u32> = m
            .refs
            .iter()
            .filter(|r| &r.kind == b"cdsc" && m.to(r).contains(&m.primary))
            .map(|r| r.from)
            .collect();
        let Some(id) = m
            .items
            .iter()
            .find(|i| i.role() == Role::Exif && describe_primary.contains(&i.id))
            .map(|i| i.id)
        else {
            return Ok(());
        };
        // Data in another item or file is refused with the dropped.
        let Some(loc) = m
            .locations
            .iter()
            .find(|l| l.id == id && l.method < 2 && l.data_ref == 0)
        else {
            return Ok(());
        };
        // Its extents, judged before a byte of it is read, as place_data
        // judges dropped data: one that runs to the end is refused, and
        // one past its file or idat is broken.
        let spans = m.spans(loc)?;
        let size = match loc.method {
            0 => self.top.len,
            _ => m.idat().map_or(0, |r| r.len() as u64),
        };
        for span in &spans {
            if span.len.is_none() {
                return Err(Error::Refused(LAYOUT.into()));
            }
            if span.end(size) > size {
                return Err(Error::Broken(format!("item {id}'s data runs past its end")));
            }
        }
        let mut left = self.left;
        // What follows its first MAX_EXIF is read in a source: other fields
        // unless it's all zeros. Not in an output, whose Exif kept for the
        // orientation holds zeros there, which the check reads byte for
        // byte.
        let walked = self.exif(r, loc, &mut left, self.pass == Pass::Source)?;
        self.left = left;
        self.walked = Some(id);
        self.found.fields.extend(walked.fields);
        let tiff = walked
            .orientation
            .and_then(|o| exif::minimal(Some(o), false));
        if walked.rest && (self.pass == Pass::Source || tiff.is_none()) {
            self.found.fields.insert(MetaField::Other);
        }
        let Some(tiff) = tiff else {
            return Ok(());
        };
        let mut payload = if walked.prefixed {
            [&6u32.to_be_bytes()[..], b"Exif\0\0"].concat()
        } else {
            0u32.to_be_bytes().to_vec()
        };
        payload.extend_from_slice(&tiff);
        let [span] = spans[..] else {
            return Err(Error::Refused(LAYOUT.into()));
        };
        let Some(len) = span.len.filter(|&len| len >= payload.len() as u64) else {
            return Err(Error::Refused(LAYOUT.into()));
        };
        self.oriented = Some(Oriented {
            id,
            method: loc.method,
            range: span.start..span.start + len,
            payload,
        });
        self.dropped.remove(&id);
        Ok(())
    }

    /// Where the oriented Exif's data is, by construction method: in the
    /// file (0) or in `idat`'s content (1); none, or its one extent.
    fn oriented_in(&self, method: u8) -> Vec<Range<u64>> {
        self.oriented
            .iter()
            .filter(|o| o.method == method)
            .map(|o| o.range.clone())
            .collect()
    }

    /// Whether `range` of the file lies in an `mdat`'s content.
    fn in_mdat(&self, range: &Range<u64>) -> bool {
        let boxes = &self.top.boxes;
        // The box it starts in: boxes are end to end.
        let at = boxes.partition_point(|b| b.end() <= range.start);
        boxes.get(at).is_some_and(|b| {
            &b.kind == b"mdat" && b.content().start <= range.start && range.end <= b.end()
        })
    }

    /// Finds where kept and dropped data is, and refuses what can't be
    /// rewritten safely. What an `iloc` entry with no item points to isn't
    /// any item's data: zeroed with the rest no kept item points to, and
    /// read by report_gaps.
    fn place_data(&mut self) -> Result<(), Error> {
        let len = self.top.len;
        let idat = self.meta.idat().map_or(0, |r| r.len() as u64);
        let mut kept_file = Vec::new();
        let mut kept_idat = Vec::new();
        let oriented = self.oriented.as_ref().map(|o| o.id);
        for loc in self
            .meta
            .locations
            .iter()
            .filter(|l| Some(l.id) != oriented && !self.orphans.contains(&l.id))
        {
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
        if self
            .drop_file
            .iter()
            .any(|range| !self.in_mdat(range) || overlaps(&kept_file, range))
        {
            return Err(Error::Refused(LAYOUT.into()));
        }
        if self.drop_idat.iter().any(|r| overlaps(&kept_idat, r)) {
            return Err(Error::Refused(LAYOUT.into()));
        }
        self.drop_file = merged(std::mem::take(&mut self.drop_file));
        self.drop_idat = merged(std::mem::take(&mut self.drop_idat));
        // The oriented Exif's extent is written over: in mdat or idat, as
        // dropped data is, and apart from all else.
        if let Some(o) = &self.oriented {
            let (size, kept, drops) = match o.method {
                0 => (len, &kept_file, &self.drop_file),
                _ => (idat, &kept_idat, &self.drop_idat),
            };
            if o.range.end > size {
                return Err(Error::Broken(format!(
                    "item {}'s data runs past its end",
                    o.id
                )));
            }
            if (o.method == 0 && !self.in_mdat(&o.range))
                || overlaps(kept, &o.range)
                || overlaps(drops, &o.range)
            {
                return Err(Error::Refused(LAYOUT.into()));
            }
        }
        self.kept_file = kept_file;
        self.kept_idat = kept_idat;
        Ok(())
    }

    /// Refuses a kept picture made of a dropped item (`dimg`), or whose
    /// data is in one (`iloc` references, for construction method 2), and
    /// the Exif kept for the orientation, or an `iloc` entry with no item,
    /// when a kept item refers to it by anything but a `cdsc`: a picture
    /// made of it, data in it, or what else might lean on bytes that are
    /// written over or zeroed.
    fn check_pictures(&self) -> Result<(), Error> {
        let oriented = self.oriented.as_ref().map(|o| o.id);
        let leant_on = |id: &u32| Some(*id) == oriented || self.orphans.contains(id);
        let leans = self.meta.refs.iter().any(|r| {
            let to = self.meta.to(r);
            !self.dropped.contains(&r.from)
                && ((matches!(&r.kind, b"dimg" | b"iloc")
                    && to.iter().any(|id| self.dropped.contains(id)))
                    || (&r.kind != b"cdsc" && to.iter().any(leant_on)))
        });
        if leans {
            return Err(Error::Refused(LAYOUT.into()));
        }
        Ok(())
    }

    /// Leaves out of `meta` the boxes in it that hold metadata, and
    /// reports the ones that hold anything past their user type, or their
    /// version and flags: each `uuid` (XMP, or other fields: a Canon HIF's
    /// encoder version), `xml ` (XMP) and `bxml` (other fields). Kept data
    /// in one is refused, though `meta` holds none (place_data), and
    /// construction method 1 points into `idat` only.
    fn judge_meta_boxes(&mut self) -> Result<(), Error> {
        let at = self.meta_box.content().start;
        for (i, child) in self.meta.children.iter().enumerate() {
            let content = child.content();
            // What it says, and where what it holds starts.
            let (field, head) = match &child.kind {
                b"uuid" => match self.meta.bytes[content.clone()].get(..16) {
                    Some(user) if user == XMP_UUID => (MetaField::Xmp, 16),
                    _ => (MetaField::Other, 16),
                },
                b"xml " => (MetaField::Xmp, 4),
                b"bxml" => (MetaField::Other, 4),
                _ => continue,
            };
            let range = at + child.range.start as u64..at + child.range.end as u64;
            if overlaps(&self.kept_file, &range) {
                return Err(Error::Refused(LAYOUT.into()));
            }
            if content.len() > head {
                self.found.fields.insert(field);
            }
            self.gone.push(i);
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
        let mut buf = vec![0; STEP];
        if self.top.tail < self.top.len && !zeros(r, self.top.tail..self.top.len, &mut buf)? {
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
        let mut left = self.left;
        let mut fields = Vec::new();
        for item in self
            .meta
            .items
            .iter()
            .filter(|i| self.dropped.contains(&i.id) && Some(i.id) != self.walked)
        {
            match item.role() {
                Role::Exif => {
                    if let Some(loc) = locations.get(&item.id) {
                        let walked = self.exif(r, loc, &mut left, true)?;
                        fields.extend(walked.fields);
                        if walked.rest {
                            fields.push(MetaField::Other);
                        }
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
    /// [`MAX_EXIF`] of its TIFF structure, and Other for a header offset
    /// past its end, or when what it reads would pass `left`. What follows
    /// that is read too when `rest` says so, [`STEP`] at a time, to see
    /// whether it's all zeros ([`Walked::rest`]).
    fn exif(
        &self,
        r: &mut dyn In,
        loc: &Location,
        left: &mut u64,
        rest: bool,
    ) -> Result<Walked, Error> {
        let mut walked = Walked::default();
        let pieces = self.pieces(loc)?;
        // Each piece lies in its file or idat; many that share bytes may
        // add up past any number all the same.
        let Some(total) = pieces
            .iter()
            .try_fold(0u64, |total, p| total.checked_add(p.end - p.start))
        else {
            return Err(Error::Broken(format!(
                "item {}'s data runs past any number",
                loc.id
            )));
        };
        if total == 0 {
            return Ok(walked);
        }
        let head = self.item_bytes(r, loc, &pieces, 0, total.min(10))?;
        let Some(offset) = head.first_chunk::<4>() else {
            walked.fields.push(MetaField::Other);
            return Ok(walked);
        };
        let skip = 4 + u64::from(u32::from_be_bytes(*offset));
        if skip > total {
            walked.fields.push(MetaField::Other);
            return Ok(walked);
        }
        walked.prefixed = skip == 10 && head[4..] == *b"Exif\0\0";
        let take = (total - skip).min(MAX_EXIF as u64);
        match left.checked_sub(4 + take) {
            Some(rest) => *left = rest,
            None => {
                walked.fields.push(MetaField::Other);
                return Ok(walked);
            }
        }
        let data = self.item_bytes(r, loc, &pieces, skip, take)?;
        let tiff = data.strip_prefix(b"Exif\0\0").unwrap_or(&data);
        let exif = exif::walk(tiff);
        walked.fields.extend(exif.fields);
        walked.orientation = exif.orientation;
        let more = total - skip - take;
        if more > 0 {
            walked.rest = match left.checked_sub(more) {
                Some(after) if rest => {
                    *left = after;
                    !self.zero_data(r, loc, &pieces, skip + take, more)?
                }
                _ => true,
            };
        }
        Ok(walked)
    }

    /// An Exif item's extents, in the file or in `idat`'s content.
    fn pieces(&self, loc: &Location) -> Result<Vec<Range<u64>>, Error> {
        // Each has a length, and lies within its space: orient checks the
        // primary's, and place_data the dropped ones.
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
        let mut out = Vec::new();
        for range in within(pieces, skip, take) {
            if loc.method == 0 {
                let mut buf = vec![0; (range.end - range.start) as usize];
                read_at(r, range.start, &mut buf)?;
                out.extend_from_slice(&buf);
            } else {
                out.extend_from_slice(self.in_idat(loc, range)?);
            }
        }
        Ok(out)
    }

    /// Whether `take` bytes of an item's data from `skip` on, its `pieces`
    /// end to end, are all zeros: read [`STEP`] at a time from the file, or
    /// looked at in `idat`.
    fn zero_data(
        &self,
        r: &mut dyn In,
        loc: &Location,
        pieces: &[Range<u64>],
        skip: u64,
        take: u64,
    ) -> Result<bool, Error> {
        let mut buf = vec![0; STEP];
        for range in within(pieces, skip, take) {
            let zero = if loc.method == 0 {
                zeros(r, range, &mut buf)?
            } else {
                self.in_idat(loc, range)?.iter().all(|&b| b == 0)
            };
            if !zero {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `range` of `idat`'s content, which holds data of the item at `loc`:
    /// within it (checked), but checked again here.
    fn in_idat(&self, loc: &Location, range: Range<u64>) -> Result<&[u8], Error> {
        let idat = self.meta.idat().unwrap_or(0..0);
        let at = |offset: u64| idat.start.checked_add(usize::try_from(offset).ok()?);
        match (at(range.start), at(range.end)) {
            (Some(start), Some(end)) if start <= end && end <= idat.end => {
                Ok(&self.meta.bytes[start..end])
            }
            _ => Err(Error::Broken(format!(
                "item {}'s data runs past its end",
                loc.id
            ))),
        }
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

    /// Reports the bytes of `mdat` and `idat` that no item points to:
    /// still pictures reach their data only through `iloc`, so they're
    /// zeroed, and when any isn't zero (what an earlier editor left of an
    /// item it dropped), they're reported as Other.
    fn report_gaps(&mut self, r: &mut dyn In) -> Result<(), Error> {
        let mut left_over = false;
        let mut buf = vec![0; STEP];
        let (in_file, in_idat) = (self.oriented_in(0), self.oriented_in(1));
        'mdats: for b in self.top.all(b"mdat") {
            for gap in gaps(&self.kept_file, b.content()) {
                for unread in gaps(&self.drop_file, gap).flat_map(|g| gaps(&in_file, g)) {
                    if !zeros(r, unread, &mut buf)? {
                        left_over = true;
                        break 'mdats;
                    }
                }
            }
        }
        if let Some(idat) = self.meta.idat() {
            let bytes = &self.meta.bytes[idat];
            let set = |r: Range<u64>| {
                bytes[r.start as usize..r.end as usize]
                    .iter()
                    .any(|&b| b != 0)
            };
            let all = 0..bytes.len() as u64;
            let unkept = || gaps(&self.kept_idat, all.clone()).flat_map(|g| gaps(&in_idat, g));
            self.idat_dirty = unkept().any(set);
            left_over |= unkept().flat_map(|g| gaps(&self.drop_idat, g)).any(set);
        }
        if left_over {
            self.found.fields.insert(MetaField::Other);
        }
        Ok(())
    }

    /// Whether `meta` loses an item, a property or a box.
    fn meta_changes(&self) -> bool {
        !self.dropped.is_empty() || self.removed.contains(&true) || !self.gone.is_empty()
    }

    /// Whether `meta` is written again: it changes, bytes in `idat` are
    /// zeroed, or the oriented Exif is in `idat`.
    fn rewrites_meta(&self) -> bool {
        self.meta_changes() || self.idat_dirty || !self.oriented_in(1).is_empty()
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

/// The parts of `within` that none of `covered` (sorted, apart) covers.
fn gaps(covered: &[Range<u64>], within: Range<u64>) -> impl Iterator<Item = Range<u64>> + '_ {
    let first = covered.partition_point(|c| c.end <= within.start);
    let mut rest = covered[first..].iter();
    let mut at = within.start;
    std::iter::from_fn(move || {
        while at < within.end {
            let next = rest.next().filter(|c| c.start < within.end);
            let to = next.map_or(within.end, |c| c.start.max(at));
            let gap = at..to;
            at = next.map_or(within.end, |c| c.end.max(at));
            if !gap.is_empty() {
                return Some(gap);
            }
        }
        None
    })
}

/// Where `take` bytes of an item's data from `skip` on lie in its `pieces`
/// (end to end, each in order): fewer when it ends first.
fn within(pieces: &[Range<u64>], skip: u64, take: u64) -> impl Iterator<Item = Range<u64>> + '_ {
    let end = skip.saturating_add(take);
    // Where each piece starts in the item's data.
    let mut at = 0u64;
    pieces.iter().filter_map(move |piece| {
        let len = piece.end - piece.start;
        let (from, to) = (skip.max(at), end.min(at.saturating_add(len)));
        let part = (from < to).then(|| piece.start + (from - at)..piece.start + (to - at));
        at = at.saturating_add(len);
        part
    })
}

/// Whether the file's bytes in `range` are all zero, read through `buf`.
fn zeros(r: &mut dyn In, range: Range<u64>, buf: &mut [u8]) -> Result<bool, Error> {
    let mut at = range.start;
    while at < range.end {
        let n = (range.end - at).min(buf.len() as u64) as usize;
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
            // Zeros where no kept item points.
            let from = out.len() + child.form.head() as usize;
            out.extend_from_slice(&m.bytes[child.range.clone()]);
            for gap in gaps(&s.kept_idat, 0..child.content().len() as u64) {
                out[from + gap.start as usize..from + gap.end as usize].fill(0);
            }
            if let Some(o) = s.oriented.as_ref().filter(|o| o.method == 1) {
                let at = from + o.range.start as usize;
                out[at..at + o.payload.len()].copy_from_slice(&o.payload);
            }
        } else if m.grpl == Some(i) {
            rebuild_grpl(s, child, out);
        } else if s.gone.binary_search(&i).is_err() {
            // But a box of metadata, which is left out.
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

/// `edits` (sorted), and zeros for each stretch of the `mdats` (contents,
/// in order) that none of `keep` (merged, sorted) covers, in the file's
/// order.
fn changes<'a>(
    edits: &'a [Edit],
    keep: &'a [Range<u64>],
    mdats: &'a [Range<u64>],
) -> impl Iterator<Item = Edit> + 'a {
    let mut zeros = mdats
        .iter()
        .flat_map(move |m| gaps(keep, m.clone()))
        .peekable();
    let mut edits = edits.iter().peekable();
    std::iter::from_fn(move || {
        let zero_first = match (edits.peek(), zeros.peek()) {
            (Some(e), Some(z)) => z.start < e.range().start,
            (e, z) => e.is_none() && z.is_some(),
        };
        if zero_first {
            zeros.next().map(Edit::Zero)
        } else {
            edits.next().cloned()
        }
    })
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
    /// What changes, sorted, apart; and every byte of `mdat` no kept item
    /// points to is zeroed.
    edits: Vec<Edit>,
    /// The bytes the edits write: `meta` and `free` headers.
    bytes: Vec<u8>,
    /// Where kept data is in the file: merged, sorted.
    keep: Vec<Range<u64>>,
    /// Where each `mdat`'s content is, in order.
    mdats: Vec<Range<u64>>,
    found: Found,
    primary: u32,
    /// Every item it keeps with an `iloc` entry, in `iloc`'s order.
    kept: Vec<Kept>,
    dropped: HashSet<u32>,
    /// The primary's Exif, kept for its orientation, and what its data
    /// starts with now.
    oriented: Option<(u32, Vec<u8>)>,
    /// Where the source's `idat` content is in the file.
    idat_at: Option<u64>,
}

impl Plan {
    /// Reads the HEIF `r` (`len` bytes) and plans its rewrite, or refuses
    /// it ([module docs](self)).
    pub(crate) fn new(r: &mut dyn In, len: u64) -> Result<Plan, Error> {
        let mut s = Survey::new(r, len, Pass::Source)?;
        let mut edits = Vec::new();
        // What the edits write, each in its place: the rebuilt meta and the
        // free box after it, which fit in the source's meta, a free header
        // for each box that goes, and the oriented Exif.
        let freed = s.fates.iter().filter(|&&f| f == Fate::Free).count();
        let room = if s.rewrites_meta() {
            s.meta_box.len as usize
        } else {
            0
        };
        let exif = s.oriented.as_ref().map_or(0, |o| o.payload.len());
        let mut bytes = Vec::with_capacity(room + 16 * freed + exif);
        let mut len = s.end;
        let mut padded = false;
        if s.rewrites_meta() {
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
        // The oriented Exif in mdat (in idat, the rebuilt meta has it).
        let mut keep = std::mem::take(&mut s.kept_file);
        if let Some(o) = s.oriented.as_ref().filter(|o| o.method == 0) {
            let start = bytes.len();
            bytes.extend_from_slice(&o.payload);
            edits.push(Edit::Write {
                at: o.range.start,
                bytes: start..bytes.len(),
            });
            edits.push(Edit::Zero(
                o.range.start + o.payload.len() as u64..o.range.end,
            ));
            keep.push(o.range.clone());
            keep = merged(keep);
        }
        edits.retain(|e| !e.range().is_empty());
        edits.sort_unstable_by_key(|e| e.range().start);
        // With every byte of mdat that no kept item points to, dropped data
        // and what earlier editors left, zeroed as it's copied.
        let mdats: Vec<_> = s.top.all(b"mdat").map(|b| b.content()).collect();
        let mut end = 0;
        for change in changes(&edits, &keep, &mdats) {
            let range = change.range();
            if range.start < end {
                // Drops are in mdat, apart from meta and the boxes freed.
                return Err(Error::Refused(LAYOUT.into()));
            }
            end = range.end;
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
        let oriented = s.oriented.take().map(|o| (o.id, o.payload));
        Ok(Plan {
            len,
            edits,
            bytes,
            keep,
            mdats,
            found: s.found,
            primary: m.primary,
            kept,
            idat_at,
            dropped: s.dropped,
            oriented,
        })
    }

    /// Writes the plan into `w`, copying from the source `r` it was made
    /// from, once; gives what went. What it writes is let go of as it
    /// returns, so the check never holds it beside the output's `meta`.
    pub(crate) fn write(&mut self, r: &mut dyn In, w: &mut dyn Out) -> Result<Found, Error> {
        let edits = std::mem::take(&mut self.edits);
        let bytes = std::mem::take(&mut self.bytes);
        let keep = std::mem::take(&mut self.keep);
        r.seek(SeekFrom::Start(0)).map_err(Error::Read)?;
        let mut buf = vec![0; STEP];
        let mut at = 0;
        for edit in changes(&edits, &keep, &self.mdats) {
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
                    w.write_all(&bytes[written]).map_err(Error::Write)?;
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
    /// data the same bytes but for the Exif kept for the orientation,
    /// which must hold the payload planned and zeros, and no reference
    /// from a dropped item. [`Error::Broken`] says what's wrong.
    pub(crate) fn check(&self, src: &mut dyn In, out: &mut dyn In, len: u64) -> Result<(), Error> {
        let wrong = |why: String| Err(Error::Broken(format!("the output {why}")));
        if len != self.len {
            return wrong(format!("is {len} bytes, not {}", self.len));
        }
        let s = Survey::new(out, len, Pass::Output)?;
        if !s.found.fields.is_empty() {
            return wrong(format!("still holds {:?}", s.found.fields));
        }
        if s.meta_changes() {
            return wrong("still holds metadata items, properties or boxes".into());
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
        }
        let mut a = vec![0; STEP];
        let mut b = vec![0; STEP];
        let idat_at = m
            .idat()
            .map(|r| s.meta_box.content().start + r.start as u64);
        // The primary's Exif, kept for its orientation: the same item, its
        // data the payload planned, then zeros.
        let oriented = s.oriented.as_ref().map(|o| (o.id, &o.payload[..]));
        if oriented != self.oriented.as_ref().map(|(id, p)| (*id, &p[..])) {
            return wrong("doesn't keep the orientation as planned".into());
        }
        if let Some(o) = &s.oriented {
            let from = match (o.method, idat_at) {
                (0, _) => 0,
                (_, Some(at)) => at,
                (_, None) => return wrong("lost its idat".into()),
            };
            let mut at = o.range.start;
            while at < o.range.end {
                let n = (o.range.end - at).min(STEP as u64) as usize;
                read_at(out, from + at, &mut b[..n])?;
                let i = (at - o.range.start) as usize;
                let planned = (i..i + n).map(|i| o.payload.get(i).copied().unwrap_or(0));
                if !b[..n].iter().copied().eq(planned) {
                    return wrong(format!("changed item {}'s orientation", o.id));
                }
                at += n as u64;
            }
        }
        // Every kept item's data, each byte once however many items share
        // it, in the file and in idat: what the output's survey found kept,
        // which is every item it has, and so every item kept.
        let mut places = vec![(0, 0, &s.kept_file)];
        if !s.kept_idat.is_empty() {
            let (Some(from), Some(to)) = (self.idat_at, idat_at) else {
                return wrong("lost its idat".into());
            };
            places.push((from, to, &s.kept_idat));
        }
        for (from, to, ranges) in places {
            for range in ranges.iter().cloned() {
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
        bx, described, full, iphone, layouts, meta_at, portrait, read, simple, uuid, Builder, Item,
        Place, C2PA, EXIF, GAIN, SECRET_XMP, THUMB, TMAP, XMP, XMP_UUID as XMP_USER_TYPE,
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

    /// `b` with its first Exif item turned, as a portrait photo's is:
    /// Orientation 6.
    fn turned(mut b: Builder) -> Builder {
        if let Some(item) = b.items.iter_mut().find(|i| &i.kind == b"Exif") {
            item.data = Item::exif(item.id, &portrait()).data;
        }
        b
    }

    /// The payload a portrait photo's Exif keeps: its header offset (6 and
    /// `Exif\0\0`, as Apple writes it, or 0) and a TIFF of Orientation 6.
    fn upright(prefixed: bool) -> Vec<u8> {
        let tiff = exif::minimal(Some(6), false).unwrap();
        match prefixed {
            true => [&[0, 0, 0, 6][..], b"Exif\0\0", &tiff].concat(),
            false => [&[0; 4][..], &tiff].concat(),
        }
    }

    #[test]
    fn a_portrait_photos_exif_keeps_its_orientation_alone_in_its_place() {
        for (name, b) in layouts() {
            let b = turned(b);
            let exif = b.items.iter().find(|i| &i.kind == b"Exif").unwrap();
            let (src, layout) = b.build();
            if exif.extents > 1 {
                // Its orientation can't be written in place.
                assert_eq!(refused(&src), LAYOUT, "{name}");
                continue;
            }
            let (out, found) = strip(&src);
            // Its other fields are reported as before.
            assert!(
                found.is_superset(&[Location, Camera, CreationDate].into()),
                "{name}"
            );
            let (m, s) = (meta(&out), meta(&src));
            // Its infe, iloc entry and cdsc stay as they were.
            let id = exif.id;
            let entry = |m: &Meta| m.bytes[m.location(id).unwrap().range.clone()].to_vec();
            let infe = |m: &Meta| m.bytes[m.item(id).unwrap().range.clone()].to_vec();
            assert_eq!((entry(&m), infe(&m)), (entry(&s), infe(&s)), "{name}");
            assert!(m
                .refs
                .iter()
                .any(|r| &r.kind == b"cdsc" && r.from == id && m.to(r) == [b.primary]));
            // Its data: the Orientation alone, then zeros.
            let data = read(&out, &m, id);
            let payload = upright(true);
            assert_eq!(data.len(), layout.data(id).len(), "{name}");
            assert_eq!(data[..payload.len()], payload[..], "{name}");
            assert!(data[payload.len()..].iter().all(|&b| b == 0), "{name}");
            let walked = exif::walk(&data[10..]);
            assert_eq!((walked.orientation, walked.fields.len()), (Some(6), 0));
            assert!(!has(&out, b"Secret"), "{name}");
            // A second pass copies it as it is.
            let (again, found) = strip(&out);
            assert_eq!(again, out, "{name}");
            assert!(found.is_empty(), "{name}");
        }
    }

    #[test]
    fn an_exif_with_its_tiff_right_after_its_offset_keeps_that_form() {
        // Samsung's: offset 0, the TIFF straight after it.
        let mut b = simple();
        b.items[1].data = [&[0; 4][..], &portrait()].concat();
        let (src, _) = b.build();
        let (out, found) = strip(&src);
        assert_eq!(found, [Location, Camera, CreationDate, Xmp].into());
        let data = read(&out, &meta(&out), EXIF);
        let payload = upright(false);
        assert_eq!(data[..payload.len()], payload[..]);
        assert!(data[payload.len()..].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_portrait_exif_that_cant_take_its_orientation_in_place_is_refused() {
        // Too small for the payload: a TIFF of 22 bytes, its Orientation
        // and no next IFD, after offset 0.
        let mut tiny = super::super::exif::tests::tiff(
            b'M',
            &[&[(0x0112, super::super::exif::tests::V::Short(6))]],
            None,
        );
        tiny.truncate(22);
        let mut b = simple();
        b.items[1].data = [&[0; 4][..], &tiny].concat();
        assert_eq!(refused(&b.build().0), LAYOUT);
        // In another item, in another file, or over kept data.
        let (src, layout) = turned(simple()).build();
        let exif = entry_at(&src, EXIF);
        let primary = layout.file_ranges(1)[0].start as u32;
        for (name, at, bytes) in [
            ("in another item", exif + 2, vec![0, 2]),
            ("in another file", exif + 4, vec![0, 1]),
            ("over kept data", exif + 8, primary.to_be_bytes().to_vec()),
        ] {
            let mut file = src.clone();
            file[at..at + bytes.len()].copy_from_slice(&bytes);
            assert_eq!(refused(&file), LAYOUT, "{name}");
        }
        // An Exif that describes another item, the thumbnail's, goes.
        let mut b = turned(simple());
        b.items.push(Item::picture(THUMB, 0xC0, 40));
        b.refs[0] = (*b"cdsc", EXIF, vec![THUMB]);
        let (out, _) = strip(&b.build().0);
        assert!(meta(&out).item(EXIF).is_none());
    }

    #[test]
    fn a_portrait_exifs_extents_are_judged_before_its_read() {
        // 64-bit offsets and lengths: iloc v1's id, construction method,
        // data reference and extent count, then each extent's offset and
        // length.
        let mut b = turned(simple());
        b.sizes = [8, 8, 0, 0];
        b.items[1].extents = 2;
        let (src, _) = b.build();
        let first = entry_at(&src, EXIF) + 8;
        let extent = |file: &mut [u8], at: usize, offset: u64, len: u64| {
            file[at..at + 8].copy_from_slice(&offset.to_be_bytes());
            file[at + 8..at + 16].copy_from_slice(&len.to_be_bytes());
        };
        // Two halves of every number: their sum is past any.
        let mut file = src.clone();
        extent(&mut file, first, 0, 1 << 63);
        extent(&mut file, first + 16, 0, 1 << 63);
        assert!(matches!(plan(&file), Err(Error::Broken(_))));
        // Far past the file, read from a file, which can't seek there: 5
        // bytes 10 before the end of every number, and the rest after.
        let mut file = src.clone();
        extent(&mut file, first, u64::MAX - 9, 5);
        let path = std::env::temp_dir().join(format!(
            "grape-heic-unit-{}.heic",
            crate::output::unique_suffix()
        ));
        std::fs::write(&path, &file).unwrap();
        let mut r = std::io::BufReader::new(std::fs::File::open(&path).unwrap());
        let planned = Plan::new(&mut r, file.len() as u64);
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(planned, Err(Error::Broken(_))), "{planned:?}");
        // One that runs to the end is refused, as dropped data is.
        let mut file = src.clone();
        extent(&mut file, first + 16, 1, 0);
        assert_eq!(refused(&file), LAYOUT);
    }

    #[test]
    fn a_portrait_exif_a_kept_item_leans_on_is_refused() {
        // Item 8, hidden, its data in the Exif (construction method 2: 20
        // bytes from its byte 10) by an iloc reference: a JPEG thumbnail of
        // the primary, and an item of another type.
        for kind in [*b"jpeg", *b"abcd"] {
            let mut b = turned(simple());
            let mut item = Item::new(8, kind, Place::Mdat, vec![0xB8; 20]);
            item.hidden = true;
            b.items.push(item);
            b.refs.push((*b"iloc", 8, vec![EXIF]));
            if &kind == b"jpeg" {
                b.refs.push((*b"thmb", 8, vec![1]));
            }
            let (mut file, _) = b.build();
            // iloc v1: id, construction method, data reference, extent
            // count, then the extent's offset and length.
            let at = entry_at(&file, 8);
            file[at + 2..at + 4].copy_from_slice(&[0, 2]);
            file[at + 8..at + 12].copy_from_slice(&10u32.to_be_bytes());
            file[at + 12..at + 16].copy_from_slice(&20u32.to_be_bytes());
            assert_eq!(refused(&file), LAYOUT, "{kind:?}");
        }
        // A grid made of it, and a picture that refers to it otherwise.
        for kind in [b"dimg", b"thmb", b"auxl", b"prem", b"base", b"abcd"] {
            let mut b = turned(simple());
            b.items.push(match kind {
                b"dimg" => Item::new(16, *b"grid", Place::Idat, vec![0, 0, 0, 0, 0, 64, 0, 48]),
                _ => Item::picture(16, 0xC6, 20),
            });
            b.refs.push((*kind, 16, vec![EXIF]));
            assert_eq!(refused(&b.build().0), LAYOUT, "{kind:?}");
        }
        // A cdsc to it is no reason: the Exif keeps its orientation.
        let mut b = turned(simple());
        b.items.push(Item::picture(16, 0xC6, 20));
        b.refs.push((*b"cdsc", 16, vec![EXIF]));
        let (out, _) = strip(&b.build().0);
        assert!(meta(&out).item(EXIF).is_some());
        assert!(meta(&out).refs.iter().any(|r| r.from == 16));
    }

    #[test]
    fn a_portrait_exif_past_its_first_mib_keeps_its_orientation() {
        // 1 MiB, 64 KiB and a byte after its header, in mdat and in idat:
        // what's past the first MiB is other fields unless it's all zeros,
        // its last byte 1 or 0; in the output it's zeros, which the check
        // reads byte for byte.
        let len = 10 + MAX_EXIF + STEP + 1;
        let photo: BTreeSet<_> = [Location, Camera, CreationDate, Xmp].into();
        for place in [Place::Mdat, Place::Idat] {
            for last in [1, 0] {
                let mut b = turned(simple());
                b.items[1].data.resize(len, 0);
                b.items[1].data[len - 1] = last;
                b.items[1].place = place;
                let (src, _) = b.build();
                let (out, found) = strip(&src);
                let other = match last {
                    0 => BTreeSet::new(),
                    _ => [Other].into(),
                };
                assert_eq!(found, &photo | &other, "{place:?} {last}");
                let data = read(&out, &meta(&out), EXIF);
                let payload = upright(true);
                assert_eq!(data.len(), len, "{place:?}");
                assert_eq!(data[..payload.len()], payload[..], "{place:?}");
                assert!(data[payload.len()..].iter().all(|&b| b == 0), "{place:?}");
                // A second pass copies it as it is, and finds nothing.
                let (again, found) = strip(&out);
                assert_eq!(again, out, "{place:?} {last}");
                assert!(found.is_empty(), "{place:?} {last}");
            }
        }
    }

    /// A file that cancels `cancel` once a read passes `at`.
    struct Tripwire {
        file: Cursor<Vec<u8>>,
        at: u64,
        cancel: crate::CancelToken,
    }

    impl std::io::Read for Tripwire {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.file.read(buf)?;
            if self.file.position() > self.at {
                self.cancel.cancel();
            }
            Ok(n)
        }
    }

    impl std::io::Seek for Tripwire {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.file.seek(pos)
        }
    }

    #[test]
    fn what_follows_an_exifs_first_mib_is_read_a_step_at_a_time_till_a_cancel() {
        // A portrait Exif with 4 MiB of zeros after its first MiB, cancelled
        // as the first 64 KiB of them are read: the next read stops.
        let mut b = turned(simple());
        b.items[1].data.resize(10 + MAX_EXIF + (4 << 20), 0);
        let (file, layout) = b.build();
        let rest = layout.file_ranges(EXIF)[0].start + 10 + MAX_EXIF as u64;
        let len = file.len() as u64;
        let cancel = crate::CancelToken::new();
        let tripwire = Tripwire {
            file: Cursor::new(file),
            at: rest + 1,
            cancel: cancel.clone(),
        };
        let mut r = std::io::BufReader::with_capacity(
            STEP,
            crate::cancel::Cancellable::new(tripwire, &cancel),
        );
        let planned = Plan::new(&mut r, len);
        assert!(matches!(planned, Err(Error::Read(_))), "{planned:?}");
    }

    #[test]
    fn the_check_catches_a_changed_orientation() {
        let (src, layout) = turned(simple()).build();
        let mut p = plan(&src).unwrap();
        let mut o = Cursor::new(Vec::new());
        p.write(&mut Cursor::new(&src), &mut o).unwrap();
        let out = o.into_inner();
        let check = |o: &[u8]| p.check(&mut Cursor::new(&src), &mut Cursor::new(o), o.len() as u64);
        check(&out).unwrap();
        let data = layout.file_ranges(EXIF)[0].clone();
        // A byte left after it, or its Orientation changed.
        for at in [data.end as usize - 1, data.start as usize + 10 + 8 + 2 + 9] {
            let mut o = out.clone();
            o[at] ^= 3;
            assert!(matches!(check(&o), Err(Error::Broken(_))), "{at}");
        }
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
        // Past 1 MiB, only the first is walked, and the rest is Other
        // unless it's all zeros (its last byte 1 or 0), read across its 20
        // extents: the primary's Exif, and one that describes nothing.
        let mut big = 6u32.to_be_bytes().to_vec();
        big.extend_from_slice(b"Exif\0\0");
        big.extend_from_slice(&tiff);
        big.resize(4 + 6 + MAX_EXIF + STEP + 1, 0);
        b.items[1].extents = 20;
        let walked: BTreeSet<_> = [Location, Camera, CreationDate].into();
        for refs in [1, 0] {
            b.refs.truncate(refs);
            for last in [1, 0] {
                if let Some(end) = big.last_mut() {
                    *end = last;
                }
                b.items[1].data = big.clone();
                let other = match last {
                    0 => BTreeSet::new(),
                    _ => [Other].into(),
                };
                let found = strip(&b.build().0).1;
                assert_eq!(found, &walked | &other, "{refs} {last}");
            }
        }
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
        // Fifty Exif items whose extents are all the first's 200 KB, or
        // its 1 MiB and 200 KB, read past its first MiB to see it's zeros.
        for size in [200_000, MAX_EXIF + 200_000] {
            let mut b = simple();
            b.items.truncate(2);
            b.refs.truncate(1);
            b.items[1].data.resize(size, 0);
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
            // The first is walked; past the file's size, the rest are
            // unread.
            assert_eq!(
                p.found.fields,
                [Location, Camera, CreationDate, Other].into(),
                "{size}"
            );
            let read = r.get_ref().read;
            assert!(read < 3 * len, "{size}: {read} of {len}");
            let (out, _) = strip(&file);
            assert!(out[shared.start as usize..shared.end as usize]
                .iter()
                .all(|&b| b == 0));
        }
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
    fn metas_own_uuid_xml_and_bxml_boxes_go() {
        // A Canon HIF's uuid box, which names its encoder.
        const CANON: [u8; 16] = [
            0x85, 0xC0, 0xB6, 0x87, 0x82, 0x0F, 0x11, 0xE0, 0x81, 0x11, 0xF4, 0xCE, 0x46, 0x2B,
            0x6A, 0x48,
        ];
        let canon = uuid(CANON, b"\0\0\0\x26CNCVSecretHEIF001/10.00.01/00.00.00");
        let xmp = uuid(XMP_USER_TYPE, SECRET_XMP.as_bytes());
        let xml = full(b"xml ", 0, 0, SECRET_XMP.as_bytes());
        let bxml = full(b"bxml", 0, 0, b"SecretBinaryXml");
        let file = |boxes: &[&Vec<u8>]| {
            let mut b = simple();
            b.items.truncate(1);
            b.refs.clear();
            b.extra_meta = boxes.iter().map(|b| b.to_vec()).collect();
            b.extra_meta.push(bx(b"abcd", b"kept"));
            b.build().0
        };
        let kinds = |f: &[u8]| meta(f).children.iter().map(|c| c.kind).collect::<Vec<_>>();
        let src = file(&[&canon, &xmp, &xml, &bxml]);
        let (out, found) = strip(&src);
        assert_eq!(found, [Xmp, Other].into());
        assert!(!has(&out, b"Secret") && !has(&out, b"CNCV"));
        // The rest of meta stays, a box Grape doesn't know among it.
        let mut left = kinds(&src);
        left.retain(|k| !matches!(k, b"uuid" | b"xml " | b"bxml"));
        assert_eq!(kinds(&out), left);
        assert!(has(&out, b"kept"));
        // A second pass finds nothing, and copies it as it is.
        let (again, found) = strip(&out);
        assert_eq!(again, out);
        assert!(found.is_empty());
        // Each alone says what it held, and one that holds nothing past
        // its user type, or its version and flags, nothing.
        let empty = [
            bx(b"uuid", b""),
            uuid(XMP_USER_TYPE, b""),
            uuid(CANON, b""),
            full(b"xml ", 0, 0, b""),
            full(b"bxml", 0, 0, b""),
        ];
        let held = [
            (&canon, vec![Other]),
            (&xmp, vec![Xmp]),
            (&xml, vec![Xmp]),
            (&bxml, vec![Other]),
        ];
        for (b, said) in held.into_iter().chain(empty.iter().map(|b| (b, vec![]))) {
            let src = file(&[b]);
            let (out, found) = strip(&src);
            assert_eq!(
                found,
                said.into_iter().collect::<BTreeSet<_>>(),
                "{:?}",
                &b[4..8]
            );
            assert!(out != src && !kinds(&out).contains(&b[4..8].try_into().unwrap()));
        }
        // Kept data in one is refused: the primary's extent pointed into
        // Canon's (whose content starts 24 bytes into it).
        let mut src = file(&[&canon]);
        let m = meta(&src);
        let at = meta_at(&src)
            + m.children
                .iter()
                .find(|c| &c.kind == b"uuid")
                .unwrap()
                .range
                .start
            + 24;
        let primary = entry_at(&src, 1) + 8;
        src[primary..primary + 4].copy_from_slice(&(at as u32).to_be_bytes());
        src[primary + 4..primary + 8].copy_from_slice(&8u32.to_be_bytes());
        assert_eq!(refused(&src), LAYOUT);
    }

    #[test]
    fn bytes_in_mdat_and_idat_no_item_points_to_are_zeroed() {
        // An earlier editor shortened the primary's extent: its last 100
        // bytes in mdat are no item's. Its first 200 stay.
        let primary = |tail: u8| {
            let mut b = simple();
            b.items[0].data = [vec![0xA1; 200], vec![tail; 100]].concat();
            let (mut file, layout) = b.build();
            let len = entry_at(&file, 1) + 12;
            file[len..len + 4].copy_from_slice(&200u32.to_be_bytes());
            (file, layout.file_ranges(1)[0].clone())
        };
        let photo: BTreeSet<_> = [Location, Camera, CreationDate, Xmp].into();
        let (file, data) = primary(0xA1);
        let (out, found) = strip(&file);
        assert_eq!(found, &photo | &[Other].into());
        let (a, b) = (data.start as usize, data.end as usize);
        assert_eq!(out[a..a + 200], file[a..a + 200]);
        assert!(out[a + 200..b].iter().all(|&b| b == 0));
        // A second pass finds nothing, and copies it as it is.
        let (again, found) = strip(&out);
        assert_eq!(again, out);
        assert!(found.is_empty());
        // A gap of zeros isn't reported.
        assert_eq!(strip(&primary(0).0).1, photo);
        // In idat: the tone map's last 3 bytes.
        let (mut file, _) = iphone().build();
        let len = entry_at(&file, TMAP) + 12;
        file[len..len + 4].copy_from_slice(&3u32.to_be_bytes());
        let (out, found) = strip(&file);
        assert!(found.contains(&Other));
        let m = meta(&out);
        let idat = meta_at(&out) + m.idat().unwrap().start;
        // The grid's 8 bytes, the tone map's first 3, then zeros.
        assert_eq!(
            out[idat..idat + 14],
            [0, 0, 0, 1, 0, 64, 0, 48, 0, 1, 2, 0, 0, 0]
        );
        // The check finds a byte left in a gap.
        let (file, _) = primary(0xA1);
        let mut p = plan(&file).unwrap();
        let mut o = Cursor::new(Vec::new());
        p.write(&mut Cursor::new(&file), &mut o).unwrap();
        let mut o = o.into_inner();
        o[b - 1] = 1;
        assert!(matches!(
            p.check(
                &mut Cursor::new(&file),
                &mut Cursor::new(&o),
                o.len() as u64
            ),
            Err(Error::Broken(_))
        ));
    }

    /// `file` with item `id`'s `infe` (v2) naming item `to` instead, so
    /// that `id`'s `iloc` entry has no item.
    fn renamed(mut file: Vec<u8>, id: u32, to: u16) -> Vec<u8> {
        // After its header, version and flags.
        let at = meta_at(&file) + meta(&file).item(id).unwrap().range.start + 12;
        file[at..at + 2].copy_from_slice(&to.to_be_bytes());
        file
    }

    #[test]
    fn an_iloc_entry_with_no_item_goes_and_its_bytes_are_no_items() {
        // The Exif's infe renamed 99: its iloc entry is left with no item,
        // its data in mdat or in idat, the camera's or zeros.
        for place in [Place::Mdat, Place::Idat] {
            for blank in [false, true] {
                let mut b = simple();
                b.items.truncate(2);
                b.refs.truncate(1);
                b.items[1].place = place;
                if blank {
                    b.items[1].data.fill(0);
                }
                let (src, layout) = b.build();
                let src = renamed(src, EXIF, 99);
                let (out, found) = strip(&src);
                let said = match blank {
                    true => BTreeSet::new(),
                    false => [Other].into(),
                };
                assert_eq!(found, said, "{place:?} {blank}");
                let m = meta(&out);
                assert!(m.location(EXIF).is_none() && m.item(99).is_none());
                assert!(m.iref.is_none() && !has(&out, b"Secret"));
                if place == Place::Mdat {
                    let r = layout.file_ranges(EXIF)[0].clone();
                    assert!(out[r.start as usize..r.end as usize]
                        .iter()
                        .all(|&b| b == 0));
                }
                assert_eq!(read(&out, &m, 1), layout.data(1));
                // A second pass finds nothing, and copies it as it is.
                let (again, found) = strip(&out);
                assert_eq!(again, out, "{place:?} {blank}");
                assert!(found.is_empty(), "{place:?} {blank}");
            }
        }
    }

    #[test]
    fn a_kept_item_that_leans_on_an_iloc_entry_with_no_item_is_refused() {
        // Item 8, its data in the Exif's (construction method 2: 20 bytes
        // from its byte 10) by an iloc reference, and the Exif's infe
        // renamed: the entry its data is in has no item.
        let mut b = simple();
        let mut item = Item::new(8, *b"jpeg", Place::Mdat, vec![0xB8; 20]);
        item.hidden = true;
        b.items.push(item);
        b.refs.push((*b"iloc", 8, vec![EXIF]));
        let (mut file, _) = b.build();
        // iloc v1: id, construction method, data reference, extent count,
        // then the extent's offset and length.
        let at = entry_at(&file, 8);
        file[at + 2..at + 4].copy_from_slice(&[0, 2]);
        file[at + 8..at + 12].copy_from_slice(&10u32.to_be_bytes());
        file[at + 12..at + 16].copy_from_slice(&20u32.to_be_bytes());
        assert_eq!(refused(&renamed(file, EXIF, 99)), LAYOUT);
        // A grid made of it, and a picture that refers to it otherwise.
        for kind in [b"dimg", b"thmb", b"auxl", b"prem", b"base", b"abcd"] {
            let mut b = simple();
            b.items.push(match kind {
                b"dimg" => Item::new(16, *b"grid", Place::Idat, vec![0, 0, 0, 0, 0, 64, 0, 48]),
                _ => Item::picture(16, 0xC6, 20),
            });
            b.refs.push((*kind, 16, vec![EXIF]));
            assert_eq!(refused(&renamed(b.build().0, EXIF, 99)), LAYOUT, "{kind:?}");
        }
        // A cdsc to it is no reason: the reference loses it.
        let mut b = simple();
        b.items.push(Item::picture(16, 0xC6, 20));
        b.refs.push((*b"cdsc", 16, vec![EXIF]));
        let (out, _) = strip(&renamed(b.build().0, EXIF, 99));
        let m = meta(&out);
        assert!(m.item(16).is_some() && m.location(EXIF).is_none());
        assert!(m.iref.is_none());
    }

    #[test]
    fn gaps_are_what_nothing_covers_within_a_range() {
        let covered = [2..4, 6..9, 12..20];
        let all = |within: Range<u64>| gaps(&covered, within).collect::<Vec<_>>();
        assert_eq!(all(0..30), [0..2, 4..6, 9..12, 20..30]);
        // Ranges that start before it or run past it.
        assert_eq!(all(3..13), [4..6, 9..12]);
        assert_eq!(all(7..8), []);
        assert_eq!(all(20..20), []);
        assert_eq!(gaps(&[], 5..9).next(), Some(5..9));
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
        // The bytes it had in mdat are no item's now, and go.
        assert_eq!(found, [Location, Camera, CreationDate, Xmp, Other].into());
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
