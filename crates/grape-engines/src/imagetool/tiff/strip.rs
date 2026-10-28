//! Remove Metadata on TIFF (D28): every page and SubIFD written back
//! compact ([`write`](super::write)), their image blocks byte for byte, in
//! the source's byte order and kind, with only the tags that describe the
//! picture.
//!
//! - **Kept:** the layout, color, resolution and page tags ([`kept`]), ICC
//!   and Orientation among them; Photoshop's resources, filtered
//!   ([`irb`](super::irb)): a block that IFDs share is filtered once, and
//!   held and written once; with a block past [`MAX_IRB`], past that much
//!   of them kept in all, or more of them read than the file holds
//!   (blocks that overlap), the file is too large.
//! - **Dropped and reported:** every other tag ([`field`]), and the Exif
//!   and GPS IFDs, walked as [`exif::classify`] does (one that pages share,
//!   once). Padding, and blank values of up to [`MAX_JUDGED`] bytes, go
//!   unsaid: a value that entries share is read once, and at most
//!   [`JUDGE_BUDGET`] in all, past which a value is reported unread.
//! - **Written back:** the DCF Adobe RGB Exif (ColorSpace uncalibrated,
//!   Interop `R03`), when that's how an IFD with no ICC profile said its
//!   color: each such IFD gets its own, even when they shared one.
//! - **Refused:** DNG and old-style JPEG (both tools); metadata inside
//!   JPEG data (an APP1, APP13 or COM in its JPEGTables, or in a block:
//!   up to its first scan when that's a sequential frame's only one, and
//!   through every scan otherwise), and JPEG data Grape can't check, whose
//!   segments or scans run past the first [`MAX_SCAN`] bytes read before
//!   that, or that's hierarchical; ImageJ and OME files, whose layout is
//!   in their description.
//! - **Checked:** [`Plan::check`] reads the output again before it's
//!   published.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::super::exif;
use super::super::strip::{read_at, read_exact, Error, Found, In, Out};
use super::irb;
use super::read::{read_value, seek, Block, Entry, Ifd, Layout, Meta, Order, Tiff, Value};
use super::write::{self, Data, Dir, Field, Image};
use crate::{CancelToken, MetaField};

/// The longest dropped value read to see whether it's blank, and how much
/// of a description is read to see whether it's ImageJ's or OME's.
const MAX_JUDGED: u64 = 64 * 1024;

/// The most bytes read in one file to see whether dropped values are
/// blank, as PNG's text has: past it, a dropped value is reported unread.
const JUDGE_BUDGET: u64 = 16 << 20;

/// The most values judged blank that the report remembers, so that one
/// that entries share is read once.
const MAX_BLANKS: usize = 4_096;

/// The most bytes of Photoshop's resources the plan holds, filtered, each
/// block once however many IFDs name it; and the longest block it reads.
const MAX_IRB: u64 = 16 << 20;

/// How much of each JPEG block (and of its tables) is read for segments.
const MAX_SCAN: u64 = 64 * 1024;

/// Why a file with metadata inside its JPEG data is refused.
const IN_DATA: &str = "has metadata inside its image data, which Grape can't remove yet.";

/// Why a file whose JPEG data runs on past what's read of it is refused.
const UNCHECKED: &str = "has JPEG data Grape can't check for metadata yet.";

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
    pages: Vec<Dir>,
    found: Found,
}

impl Plan {
    /// Walks the TIFF `r` (`len` bytes) and plans its rewrite, or refuses
    /// it ([module docs](self)); the walk refuses DNG and old-style JPEG.
    pub(crate) fn new(r: &mut dyn In, len: u64) -> Result<Plan, Error> {
        let tiff = Tiff::read(r, len)?;
        let order = tiff.layout.order;
        // What's read of descriptions and JPEG tables, each value once,
        // may come to what the file holds: only values that overlap come
        // to more.
        let mut left = len;
        // A description that IFDs share is read once.
        let mut descriptions = HashSet::new();
        for ifd in tiff.ifds() {
            if let Some(entry) = ifd.get(270).filter(|e| descriptions.insert(e.value)) {
                let text = look(r, entry, MAX_JUDGED, &mut left)?.map(|(text, _)| text);
                let text = text.unwrap_or_default();
                if text.starts_with(b"ImageJ=") || text.windows(4).any(|w| w == b"<OME") {
                    return Err(Error::Refused(DESCRIBED.into()));
                }
            }
        }
        let jpegs: Vec<&Ifd> = tiff
            .ifds()
            .into_iter()
            .filter(|ifd| ifd.uint(259, order) == Some(7))
            .collect();
        if !jpegs.is_empty() {
            let shared = tiff.shared();
            let mut scan = JpegScan {
                shared: &shared,
                scanned: vec![false; shared.len()],
                tables: HashSet::new(),
                left,
            };
            for ifd in jpegs {
                match scan.seen(r, ifd)? {
                    Seen::Clean => {}
                    Seen::Metadata => return Err(Error::Refused(IN_DATA.into())),
                    Seen::Unseen => return Err(Error::Refused(UNCHECKED.into())),
                }
            }
        }
        let Tiff { layout, pages } = tiff;
        let mut walk = Walk {
            r,
            order,
            found: Found::default(),
            reported: HashSet::new(),
            irbs: HashMap::new(),
            keep: true,
            irb_left: MAX_IRB,
            irb_read: len,
            blanks: HashSet::new(),
            judge_left: JUDGE_BUDGET,
        };
        let pages = pages
            .into_iter()
            .map(|page| walk.dir(page))
            .collect::<Result<_, _>>()?;
        Ok(Plan {
            layout,
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
        write::write(r, w, self.layout, &self.pages, cancel)?;
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
        // And the report finds nothing. The walk keeps none of the
        // resources it filters, a page's or its SubIFDs'. The output's are
        // the plan's, so they're within its cap already.
        let mut walk = Walk {
            r,
            order: out.layout.order,
            found: Found::default(),
            reported: HashSet::new(),
            irbs: HashMap::new(),
            keep: false,
            irb_left: u64::MAX,
            irb_read: u64::MAX,
            blanks: HashSet::new(),
            judge_left: JUDGE_BUDGET,
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
/// `None` when it can't be read. Only they are read ([`read_at`]), however
/// far from what was read before.
fn head(r: &mut dyn In, entry: &Entry, max: u64) -> Result<Option<(Vec<u8>, bool)>, Error> {
    match entry.value {
        Value::Inline { bytes, len } => Ok(Some((bytes[..usize::from(len)].to_vec(), true))),
        Value::At { offset, len } => {
            let mut bytes = vec![0; len.min(max) as usize];
            read_at(r, offset, &mut bytes)?;
            Ok(Some((bytes, len <= max)))
        }
        Value::Unread => Ok(None),
    }
}

/// The first `max` bytes of `entry`'s value, as [`head`] reads them,
/// taken from `left`: past it, the file is too large.
fn look(
    r: &mut dyn In,
    entry: &Entry,
    max: u64,
    left: &mut u64,
) -> Result<Option<(Vec<u8>, bool)>, Error> {
    if let Value::At { len, .. } = entry.value {
        *left = left.checked_sub(len.min(max)).ok_or_else(|| {
            Error::TooLarge(
                "descriptions or JPEG tables that overlap, more to read than it holds".into(),
            )
        })?;
    }
    head(r, entry, max)
}

/// The scan of JPEG data for metadata, which reads a block, or a
/// JPEGTables value, once however many times IFDs list it: one that held
/// metadata, or couldn't be seen through, ended the scan.
struct JpegScan<'s> {
    /// The blocks listed more than once, sorted ([`Tiff::shared`]).
    shared: &'s [Block],
    /// Which of them it has read.
    scanned: Vec<bool>,
    /// The JPEGTables values it has read, by where they are.
    tables: HashSet<Value>,
    /// What's left of the bytes it may read of JPEGTables values ([`look`]).
    left: u64,
}

impl JpegScan<'_> {
    /// What the JPEG data of `ifd` (compression 7) holds, as [`segments`]
    /// reads its JPEGTables and each of its blocks: the first that isn't
    /// clean.
    fn seen(&mut self, r: &mut dyn In, ifd: &Ifd) -> Result<Seen, Error> {
        if let Some(tables) = ifd.get(347).filter(|e| self.tables.insert(e.value)) {
            if let Some((tables, whole)) = look(r, tables, MAX_SCAN, &mut self.left)? {
                let seen = segments(&tables, whole);
                if seen != Seen::Clean {
                    return Ok(seen);
                }
            }
        }
        let mut buf = Vec::new();
        for block in ifd.blocks.iter().filter(|b| b.len > 0) {
            let shared = self.shared.binary_search(block).ok();
            if let Some(scanned) = shared.and_then(|n| self.scanned.get_mut(n)) {
                if std::mem::replace(scanned, true) {
                    continue;
                }
            }
            buf.resize(block.len.min(MAX_SCAN) as usize, 0);
            seek(r, block.offset)?;
            read_exact(r, &mut buf)?;
            let seen = segments(&buf, block.len <= MAX_SCAN);
            if seen != Seen::Clean {
                return Ok(seen);
            }
        }
        Ok(Seen::Clean)
    }
}

/// What JPEG data holds, as far as it's read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// No APP1, APP13 or COM segment, or not segments Grape knows, which
    /// are copied as they are.
    Clean,
    /// An APP1, APP13 or COM segment.
    Metadata,
    /// What Grape can't see through: segments or scans that run on past
    /// what was read, or a hierarchical JPEG (DHP), whose frames after the
    /// first it doesn't read.
    Unseen,
}

/// What the JPEG stream `jpeg` holds ([`Seen`]): when `jpeg` is only the
/// stream's first bytes (not `whole`) and they end too soon, what follows
/// can't be seen. Its segments are read up to its first scan when that
/// holds every component of a sequential frame, which then has no other
/// scan, so only its end follows; otherwise past every scan, each to the
/// marker after its coded data, to the end. Stray bytes before a marker
/// are read past, as libjpeg reads past them. A hierarchical JPEG isn't
/// read past its DHP. Data that doesn't start with SOI, or a scan with no
/// frame, isn't a stream Grape knows, and is clean: it's copied as it is.
fn segments(jpeg: &[u8], whole: bool) -> Seen {
    // Where what was read ends too soon.
    let cut = if whole { Seen::Clean } else { Seen::Unseen };
    let Some(mut rest) = jpeg.strip_prefix(&[0xFF, 0xD8]) else {
        return Seen::Clean;
    };
    // The frame's components, and whether it's sequential (each component
    // in one scan).
    let mut frame: Option<(u8, bool)> = None;
    loop {
        let Some((at, marker)) = next_marker(rest) else {
            return cut;
        };
        rest = &rest[at..];
        match marker {
            0xE1 | 0xED | 0xFE => return Seen::Metadata,
            // A hierarchical JPEG (DHP): its frames after the first,
            // which may have segments between them, aren't read.
            0xDE => return Seen::Unseen,
            0xD9 => return Seen::Clean,
            0x01 | 0xD0..=0xD7 => continue,
            _ => {}
        }
        let Some(len) = rest.get(..2) else {
            return cut;
        };
        let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
        let (Some(segment), Some(next)) = (rest.get(..len), rest.get(len..)) else {
            return cut;
        };
        rest = next;
        match marker {
            // A frame header: SOF0 to SOF15 but DHT, JPG and DAC.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let sequential = matches!(marker, 0xC0 | 0xC1 | 0xC3 | 0xC9 | 0xCB);
                frame = segment.get(7).map(|&n| (n, sequential));
            }
            0xDA => {
                let (Some((components, sequential)), Some(&n)) = (frame, segment.get(2)) else {
                    // A scan with no frame: not a stream Grape knows.
                    return Seen::Clean;
                };
                if sequential && n == components {
                    return Seen::Clean;
                }
                let Some(at) = coded_end(rest) else {
                    return cut;
                };
                rest = &rest[at..];
            }
            _ => {}
        }
    }
}

/// The next marker in `jpeg`, and where what follows it starts, as
/// libjpeg's `next_marker` and jpeg.rs find it: past stray bytes, which
/// libjpeg warns of ("extraneous bytes before marker") and reads past,
/// fill bytes (0xFF) and stuffed zeros (0xFF 0); `None` when `jpeg` ends
/// first.
fn next_marker(jpeg: &[u8]) -> Option<(usize, u8)> {
    let mut from = 0;
    loop {
        let at = from + jpeg.get(from..)?.iter().position(|&b| b == 0xFF)?;
        let fill = jpeg[at..].iter().take_while(|&&b| b == 0xFF).count();
        match *jpeg.get(at + fill)? {
            0 => from = at + fill + 1,
            marker => return Some((at + fill + 1, marker)),
        }
    }
}

/// Where the marker after a scan's coded data `coded` starts, its fill
/// bytes (0xFF) included: the first 0xFF that, after any fill bytes, isn't
/// a stuffed one (0xFF 0) or a restart marker, which coded data holds, as
/// jpeg.rs reads them; `None` when `coded` ends first.
fn coded_end(coded: &[u8]) -> Option<usize> {
    let mut from = 0;
    loop {
        let at = from + coded.get(from..)?.iter().position(|&b| b == 0xFF)?;
        let fill = coded[at..].iter().take_while(|&&b| b == 0xFF).count();
        match coded.get(at + fill)? {
            0 | 0xD0..=0xD7 => from = at + fill + 1,
            _ => return Some(at),
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
    /// IFDs share is read and filtered once, and its field shared, when
    /// it's kept.
    irbs: HashMap<(u64, u64), Option<Field>>,
    /// Whether the fields it makes are kept for the plan; the self-check
    /// only needs what the report says, so it makes none, and holds no
    /// filtered resources while it walks a page's SubIFDs.
    keep: bool,
    /// What's left of the bytes of Photoshop's resources it may keep,
    /// filtered, each block once ([`MAX_IRB`]).
    irb_left: u64,
    /// What's left of the bytes of Photoshop's resources it may read, each
    /// block once: what the file holds, which only blocks that overlap
    /// come to more than.
    irb_read: u64,
    /// The values it read and judged blank, by where they are and their
    /// tag and type (what blank means for them), at most [`MAX_BLANKS`].
    blanks: HashSet<(Value, u16, u16)>,
    /// What's left of the bytes it may read to judge values blank
    /// ([`JUDGE_BUDGET`]).
    judge_left: u64,
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

    /// Reports `found` for the dropped `entry`, unless it's reported
    /// already, or its value is blank: inline, or read whole, at most
    /// [`MAX_JUDGED`] bytes, while the budget lasts. A value judged blank
    /// already isn't read again. One that can't be read is reported.
    fn report(&mut self, found: MetaField, entry: &Entry) -> Result<(), Error> {
        let judged = (entry.value, entry.tag, entry.kind);
        if self.found.fields.contains(&found) || self.blanks.contains(&judged) {
            return Ok(());
        }
        let len = match entry.value {
            Value::At { len, .. } => len,
            Value::Inline { .. } | Value::Unread => 0,
        };
        let blank = if len > MAX_JUDGED.min(self.judge_left) {
            false
        } else {
            self.judge_left -= len;
            match head(self.r, entry, MAX_JUDGED)? {
                Some((value, true)) => exif::is_empty(entry.tag, entry.kind, &value),
                _ => false,
            }
        };
        if !blank {
            self.found.fields.insert(found);
        } else if len > 0 && self.blanks.len() < MAX_BLANKS {
            self.blanks.insert(judged);
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
    /// picture; `None` when none is left, or when the walk keeps no
    /// fields. Resources that an earlier IFD points at too are the field
    /// it got.
    fn irb(&mut self, entry: &Entry) -> Result<Option<Field>, Error> {
        let Value::At { offset, len } = entry.value else {
            return self.filter_irb(entry);
        };
        if let Some(field) = self.irbs.get(&(offset, len)) {
            return Ok(field.clone());
        }
        if len > MAX_IRB {
            return Err(Error::TooLarge(format!(
                "{len} bytes of Photoshop resources"
            )));
        }
        // Blocks that overlap count each time, however little they differ.
        self.irb_read = self.irb_read.checked_sub(len).ok_or_else(|| {
            Error::TooLarge("Photoshop resources that overlap, more to read than it holds".into())
        })?;
        let field = self.filter_irb(entry)?;
        let kept = field.as_ref().map_or(0, |f| f.count);
        self.irb_left = self.irb_left.checked_sub(kept).ok_or_else(|| {
            Error::TooLarge(format!(
                "over {MAX_IRB} bytes of Photoshop resources to keep"
            ))
        })?;
        self.irbs.insert((offset, len), field.clone());
        Ok(field)
    }

    /// Reads and filters the resources `entry` holds, for [`Walk::irb`]:
    /// the field, if the walk keeps it.
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
        if filtered.kept.is_empty() || !self.keep {
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

    /// A TIFF of `pages` pages, each with its own Photoshop resources,
    /// `irb`.
    fn own_irbs(pages: usize, irb: &[u8]) -> Vec<u8> {
        let pages = (0..pages)
            .map(|_| {
                let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
                page.tags.push((34377, V::Bytes(1, irb.to_vec())));
                page
            })
            .collect();
        Builder::new(b'I', false, pages).build()
    }

    #[test]
    fn photoshop_resources_kept_past_16_mib_in_all_are_too_large() {
        // Three pages, each with its own 6 MiB path: 18 MiB to keep.
        let path = res(2000, "", &[7; 6 << 20]);
        assert!(matches!(plan(&own_irbs(3, &path)), Err(Error::TooLarge(_))));
        // As much read, but a thumbnail, which goes: nothing to keep.
        let thumbnail = res(1036, "", &[7; 6 << 20]);
        let (out, found) = strip(&own_irbs(3, &thumbnail));
        assert_eq!(found, [Thumbnail].into());
        assert!(read(&out).pages.iter().all(|p| p.get(34377).is_none()));
        // 40 pages, each with its own resolution, a path and a 500 KB
        // thumbnail: 20 MiB read, under 1 MiB kept.
        let kept = [
            res(1005, "", &[0, 72, 0, 0, 0, 1, 0, 1]),
            res(2000, "", &[7; 22_000]),
        ]
        .concat();
        let irb = [kept.clone(), res(1036, "", &[7; 500_000])].concat();
        let (out, found) = strip(&own_irbs(40, &irb));
        assert_eq!(found, [Thumbnail].into());
        let back = read(&out);
        assert_eq!(back.pages.len(), 40);
        for page in &back.pages {
            assert_eq!(value(&out, page, 34377).unwrap(), kept);
        }
    }

    #[test]
    fn photoshop_resources_that_overlap_are_read_up_to_the_files_size() {
        // Three pages whose resources are one block of a 6 MiB thumbnail,
        // which goes, with padding after it: named the same way, it's
        // read once; each page naming 2 more bytes of it, it's read three
        // times, more than the file holds, though nothing is kept.
        let thumbnail = res(1036, "", &[7; 6 << 20]);
        for (step, fine) in [(0, true), (2, false)] {
            let pages = (0..3)
                .map(|n| {
                    let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
                    let irb = if n == 0 {
                        [thumbnail.clone(), vec![0; 4]].concat()
                    } else {
                        vec![0]
                    };
                    page.tags.push((34377, V::Bytes(1, irb)));
                    page
                })
                .collect();
            let mut file = Builder::new(b'I', false, pages).build();
            let tiff = read(&file);
            let Value::At { offset, .. } = tiff.pages[0].get(34377).unwrap().value else {
                panic!("inline resources")
            };
            for (n, page) in (0..).zip(&tiff.pages) {
                let len = thumbnail.len() as u64 + n * step;
                set_entry(&mut file, page.at, 34377, len, offset);
            }
            if fine {
                let (out, found) = strip(&file);
                assert_eq!(found, [Thumbnail].into());
                assert!(out.len() < file.len());
            } else {
                assert!(matches!(plan(&file), Err(Error::TooLarge(_))));
            }
        }
        // One block over 16 MiB is too large to read.
        let file = own_irbs(1, &res(2000, "", &[7; 16 << 20]));
        assert!(matches!(plan(&file), Err(Error::TooLarge(_))));
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
            // Not a JPEG stream: nothing to say.
            (jpeg_page(b"garbage\xFF\xE1".to_vec(), None), false),
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
        // A scan with no frame before it isn't a stream Grape knows.
        assert_eq!(
            segments(b"\xFF\xD8\xFF\xDA\0\x02\xFF\xE1", false),
            Seen::Clean
        );
    }

    /// A segment: `marker` and `body`, with its length.
    fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let len = (body.len() as u16 + 2).to_be_bytes();
        [&[0xFF, marker], &len[..], body].concat()
    }

    /// A frame header (SOF `marker`) of `n` components, 8 × 8.
    fn frame(marker: u8, n: u8) -> Vec<u8> {
        let mut body = vec![8, 0, 8, 0, 8, n];
        for c in 1..=n {
            body.extend([c, 0x11, 0]);
        }
        segment(marker, &body)
    }

    /// A scan of `n` components and its coded data, which holds a stuffed
    /// 0xFF and a restart marker.
    fn scan(n: u8) -> Vec<u8> {
        let mut body = vec![n];
        for c in 1..=n {
            body.extend([c, 0]);
        }
        body.extend([0, 63, 0]);
        [
            segment(0xDA, &body),
            b"\x12\xFF\x00\x34\xFF\xD0\x56".to_vec(),
        ]
        .concat()
    }

    #[test]
    fn metadata_between_a_jpegs_scans_is_refused() {
        let app1 = segment(0xE1, b"Exif\0\0MM\0*");
        let tables = segment(0xC4, &[0; 17]);
        let (soi, eoi) = (b"\xFF\xD8".to_vec(), b"\xFF\xD9".to_vec());
        // Progressive: scans until the end.
        let progressive =
            |between: &[u8]| [&soi, &frame(0xC2, 1), &scan(1), between, &scan(1), &eoi].concat();
        assert_eq!(segments(&progressive(&app1), true), Seen::Metadata);
        assert_eq!(segments(&progressive(&tables), true), Seen::Clean);
        // The first scan's coded data running past what was read.
        let cut = [soi.clone(), frame(0xC2, 1), scan(1)].concat();
        assert_eq!(segments(&cut, false), Seen::Unseen);
        assert_eq!(segments(&cut, true), Seen::Clean);
        // Sequential, its first scan of one of three components.
        let partial = [&soi[..], &frame(0xC0, 3), &scan(1), &app1, &scan(2), &eoi].concat();
        assert_eq!(segments(&partial, true), Seen::Metadata);
        // Sequential, its first scan of every component: only the end
        // follows, so nothing past it is read, however long the data.
        let only = [&soi[..], &frame(0xC0, 3), &scan(3), &app1, &eoi].concat();
        assert_eq!(segments(&only, true), Seen::Clean);
        assert_eq!(segments(&only, false), Seen::Clean);
        // Through the plan: refused, or kept as it is.
        let Err(Error::Refused(why)) = plan(&jpeg_page(progressive(&app1), None)) else {
            panic!("not refused")
        };
        assert_eq!(why, IN_DATA);
        let file = jpeg_page(progressive(&tables), None);
        let (out, found) = strip(&file);
        assert!(found.is_empty());
        let block = read(&out).pages[0].blocks[0];
        let range = block.offset as usize..(block.offset + block.len) as usize;
        assert_eq!(out[range], progressive(&tables));
    }

    #[test]
    fn fill_bytes_in_a_scan_dont_end_it() {
        let app1 = segment(0xE1, b"Exif\0\0MM\0*");
        let (soi, eoi) = (b"\xFF\xD8".to_vec(), b"\xFF\xD9".to_vec());
        // Coded data with fill bytes before a restart marker, and before
        // the marker after it.
        let filled = [scan(1), b"\x78\xFF\xFF\xD0\x9A\xFF\xFF".to_vec()].concat();
        let progressive =
            |between: &[u8]| [&soi[..], &frame(0xC2, 1), &filled, between, &scan(1), &eoi].concat();
        assert_eq!(segments(&progressive(&app1), true), Seen::Metadata);
        assert_eq!(segments(&progressive(b""), true), Seen::Clean);
        // Fill bytes to the end of what was read: what follows can't be
        // seen.
        let cut = [&soi[..], &frame(0xC2, 1), &scan(1), b"\xFF\xFF"].concat();
        assert_eq!(segments(&cut, false), Seen::Unseen);
        assert_eq!(segments(&cut, true), Seen::Clean);
    }

    #[test]
    fn stray_bytes_before_a_marker_are_read_past_as_libjpeg_does() {
        // A JFIF APP0, then 2 zero bytes, then an APP1: libjpeg warns of
        // "2 extraneous bytes before marker 0xe1" and reads the APP1.
        let app0 = segment(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        let app1 = segment(0xE1, b"Exif\0\0MM\0*");
        let stray = [&app0[..], b"\0\0", &app1].concat();
        let (soi, eoi) = (b"\xFF\xD8".to_vec(), b"\xFF\xD9".to_vec());
        assert_eq!(segments(&[&soi, &stray[..]].concat(), true), Seen::Metadata);
        // A stuffed zero (0xFF 0) among them is stray too; and between a
        // progressive frame's scans, after a segment.
        let stuffed = [&soi[..], b"\x12\xFF\x00\x34", &app1].concat();
        assert_eq!(segments(&stuffed, true), Seen::Metadata);
        let tables = segment(0xC4, &[0; 17]);
        let between = [&tables[..], b"\0\0", &app1].concat();
        let progressive = [
            &soi[..],
            &frame(0xC2, 1),
            &scan(1),
            &between,
            &scan(1),
            &eoi,
        ]
        .concat();
        assert_eq!(segments(&progressive, true), Seen::Metadata);
        // Stray bytes to the end of what was read: what follows can't be
        // seen.
        let cut = [&soi[..], &app0, b"\0\0\0"].concat();
        assert_eq!(segments(&cut, false), Seen::Unseen);
        assert_eq!(segments(&cut, true), Seen::Clean);
        // Through the plan, in a strip or in its tables: refused.
        for file in [
            jpeg_page(jpeg(&stray), None),
            jpeg_page(jpeg(b""), Some([&soi, &stray[..], &eoi].concat())),
        ] {
            let Err(Error::Refused(why)) = plan(&file) else {
                panic!("not refused")
            };
            assert_eq!(why, IN_DATA);
        }
        // Stray bytes past the 64 KiB read: unchecked.
        let far = [&app0[..], &[0; 70_000], &app1].concat();
        let Err(Error::Refused(why)) = plan(&jpeg_page(jpeg(&far), None)) else {
            panic!("not refused")
        };
        assert_eq!(why, UNCHECKED);
    }

    #[test]
    fn jpeg_data_whose_scans_pass_the_window_is_refused_as_unchecked() {
        // A progressive strip, and a sequential one whose first scan holds
        // one component of three, with no metadata, whose first scan's
        // coded data runs past the 64 KiB read.
        let long = [segment(0xDA, &[1, 1, 0, 0, 63, 0]), vec![0x55; 70_000]].concat();
        let (soi, eoi) = (b"\xFF\xD8".to_vec(), b"\xFF\xD9".to_vec());
        for frame in [frame(0xC2, 1), frame(0xC0, 3)] {
            let file = jpeg_page([&soi[..], &frame, &long, &scan(1), &eoi].concat(), None);
            let Err(Error::Refused(why)) = plan(&file) else {
                panic!("not refused")
            };
            assert_eq!(why, UNCHECKED);
        }
    }

    #[test]
    fn a_hierarchical_jpeg_is_refused_as_unchecked() {
        // DHP, then a first frame that's a sequential one of one scan, and
        // after it an APP1 and a differential frame.
        let stream = [
            b"\xFF\xD8".to_vec(),
            frame(0xDE, 1),
            frame(0xC0, 1),
            scan(1),
            segment(0xE1, b"Exif\0\0MM\0*"),
            frame(0xC5, 1),
            scan(1),
            b"\xFF\xD9".to_vec(),
        ]
        .concat();
        assert_eq!(segments(&stream, true), Seen::Unseen);
        let Err(Error::Refused(why)) = plan(&jpeg_page(stream, None)) else {
            panic!("not refused")
        };
        assert_eq!(why, UNCHECKED);
    }

    #[test]
    fn jpeg_data_whose_segments_pass_the_window_is_refused() {
        // An APP2 of 65,535 bytes: the first 64 KiB end inside it, before
        // the APP1 and the scan, which can't be seen.
        let app2 = [&b"\xFF\xE2\xFF\xFF"[..], &[7; 65_533]].concat();
        let app1 = b"\xFF\xE1\0\x0EExif\0\0MM\0*\0\0";
        let tables = [&b"\xFF\xD8"[..], &app2, b"\xFF\xD9"].concat();
        for file in [
            jpeg_page(jpeg(&[&app2[..], app1].concat()), None),
            jpeg_page(jpeg(&app2), None),
            // Tables as long.
            jpeg_page(jpeg(b""), Some(tables)),
        ] {
            let Err(Error::Refused(why)) = plan(&file) else {
                panic!("not refused")
            };
            assert_eq!(why, UNCHECKED);
        }
        // A stream that ends there, read whole, has nothing after it.
        assert_eq!(segments(b"\xFF\xD8\xFF\xE2\0\x10ab", true), Seen::Clean);
        assert_eq!(segments(b"\xFF\xD8\xFF\xE2\0\x10ab", false), Seen::Unseen);
        assert_eq!(segments(b"\xFF\xD8\xFF\xDA\0\x02", false), Seen::Clean);
    }

    /// A source that counts the bytes read from it.
    struct Tally<'f> {
        file: Cursor<&'f [u8]>,
        read: u64,
    }

    impl std::io::Read for Tally<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.file.read(buf)?;
            self.read += n as u64;
            Ok(n)
        }
    }

    impl std::io::Seek for Tally<'_> {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.file.seek(pos)
        }
    }

    #[test]
    fn what_ifds_share_or_the_report_has_said_is_read_once() {
        // 40 JPEG pages whose strip, JPEGTables and description are the
        // first page's, of over 64 KiB each, and whose Software tags are
        // 64 KiB each, all different.
        let strip = [jpeg(b""), vec![0; 64 * 1024]].concat();
        let tables = [&b"\xFF\xD8\xFF\xD9"[..], &[0; 64 * 1024]].concat();
        let description = b"A scan ".repeat(10_000);
        let pages = (0..40u8)
            .map(|n| {
                let mut page = gray(8, 8, 8, vec![B::Data(strip.clone())]);
                page.tags.extend([
                    (259, V::Short(vec![7])),
                    (270, V::Bytes(2, description.clone())),
                    (305, V::Bytes(2, vec![b'A' + n % 26; 64 * 1024])),
                    (347, V::Bytes(7, tables.clone())),
                ]);
                page.tags.sort_by_key(|t| t.0);
                page
            })
            .collect();
        let mut file = Builder::new(b'I', false, pages).build();
        let tiff = read(&file);
        let first = &tiff.pages[0];
        let at = |tag| match first.get(tag).unwrap().value {
            Value::At { offset, len } => (offset, len),
            _ => panic!("{tag} inline"),
        };
        let block = first.blocks[0];
        for page in &tiff.pages[1..] {
            set_entry(&mut file, page.at, 273, 1, block.offset);
            for tag in [270, 347] {
                let (offset, len) = at(tag);
                set_entry(&mut file, page.at, tag, len, offset);
            }
        }
        let mut r = std::io::BufReader::with_capacity(
            64 * 1024,
            Tally {
                file: Cursor::new(&file),
                read: 0,
            },
        );
        let plan = Plan::new(&mut r, file.len() as u64).unwrap();
        assert_eq!(plan.found.fields, [Comment, Software].into());
        // Each 64 KiB once, and the IFDs: not 40 times.
        let read = r.get_ref().read;
        assert!(read < 1 << 20, "{read} bytes read");
    }

    /// A TIFF of `pages` pages whose `tags` (ASCII) all point into one run
    /// of NULs, `len` bytes each: page `n`'s `step × n` bytes into it.
    fn blanks(pages: u32, tags: &[u16], len: u64, step: u64) -> Vec<u8> {
        let pages = (0..pages)
            .map(|n| {
                let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
                for &tag in tags {
                    let run = if n == 0 && tag == tags[0] {
                        vec![0; (len + step * u64::from(pages)) as usize]
                    } else {
                        vec![0]
                    };
                    page.tags.push((tag, V::Bytes(2, run)));
                }
                page.tags.sort_by_key(|t| t.0);
                page
            })
            .collect();
        let mut file = Builder::new(b'I', false, pages).build();
        let tiff = read(&file);
        let Value::At { offset, .. } = tiff.pages[0].get(tags[0]).unwrap().value else {
            panic!("an inline run")
        };
        for (n, page) in (0..).zip(&tiff.pages) {
            for &tag in tags {
                set_entry(&mut file, page.at, tag, len, offset + step * n);
            }
        }
        file
    }

    /// `file` planned through a reader that counts: what went, and the
    /// bytes read.
    fn tallied(file: &[u8]) -> (BTreeSet<MetaField>, u64) {
        let mut r = std::io::BufReader::with_capacity(
            64 * 1024,
            Tally {
                file: Cursor::new(file),
                read: 0,
            },
        );
        let plan = Plan::new(&mut r, file.len() as u64).unwrap();
        (plan.found.fields, r.get_ref().read)
    }

    #[test]
    fn a_blank_value_that_entries_share_is_read_once() {
        // 64 pages whose title, camera, software, date, author and
        // copyright tags are all one blank 64 KiB value.
        let tags = [269, 271, 272, 285, 305, 306, 315, 33432];
        let (found, read) = tallied(&blanks(64, &tags, 64 * 1024, 0));
        assert!(found.is_empty(), "{found:?}");
        // Once for each tag: not 512 times.
        assert!(read < 1 << 20, "{read} bytes read");
    }

    #[test]
    fn values_are_judged_blank_within_16_mib_a_file() {
        // 260 pages whose software tags are 64 KiB of one run of NULs,
        // each 2 bytes further on: each is read, until 16 MiB have been,
        // and the rest are reported unread.
        let (found, read) = tallied(&blanks(260, &[305], 64 * 1024, 2));
        assert_eq!(found, [Software].into());
        assert!(read < JUDGE_BUDGET + (1 << 20), "{read} bytes read");
    }

    #[test]
    fn values_judged_out_of_order_read_only_their_own_bytes() {
        // 16 pages of 4,088 Software tags, each 5 bytes of one run of NULs,
        // each a byte before the last: refilling a 64 KiB buffer for each
        // would read 4 GiB.
        let (pages, per) = (16u32, 4_088u32);
        let ifd = 2 + 12 * (7 + per) + 4;
        let strips = 8 + pages * ifd;
        let run = strips + pages;
        let mut file = b"II*\0\x08\0\0\0".to_vec();
        for n in 0..pages {
            let mut entries = vec![
                (256, 4, 1, 1),
                (257, 4, 1, 1),
                (258, 3, 1, 8),
                (262, 3, 1, 1),
                (273, 4, 1, strips + n),
                (278, 4, 1, 1),
                (279, 4, 1, 1),
            ];
            let last = run + pages * per - 1;
            entries.extend((0..per).map(|i| (305, 2, 5, last - (n * per + i))));
            file.extend((entries.len() as u16).to_le_bytes());
            for (tag, kind, count, value) in entries {
                file.extend(u16::to_le_bytes(tag));
                file.extend(u16::to_le_bytes(kind));
                file.extend(u32::to_le_bytes(count));
                file.extend(u32::to_le_bytes(value));
            }
            let next = if n + 1 < pages { 8 + (n + 1) * ifd } else { 0 };
            file.extend(next.to_le_bytes());
        }
        file.extend(vec![b'x'; pages as usize]);
        file.extend(vec![0; (pages * per + 4) as usize]);
        let (found, read) = tallied(&file);
        assert!(found.is_empty(), "{found:?}");
        assert!(
            read < 2 * file.len() as u64,
            "{read} bytes read of {}",
            file.len()
        );
    }

    #[test]
    fn descriptions_or_jpeg_tables_that_overlap_are_read_up_to_the_files_size() {
        // 40 JPEG pages whose descriptions, or JPEG tables, are 64 KiB of
        // one run of text, each 2 bytes further on: 2.5 MiB to read from
        // a file of under 100 KB.
        let run = b"A scan ".repeat(10_000);
        for (tag, kind) in [(270, 2), (347, 7)] {
            let pages = (0..40)
                .map(|n| {
                    let mut page = gray(8, 8, 8, vec![B::Data(jpeg(b""))]);
                    let value = if n == 0 { run.clone() } else { vec![0] };
                    page.tags
                        .extend([(259, V::Short(vec![7])), (tag, V::Bytes(kind, value))]);
                    page.tags.sort_by_key(|t| t.0);
                    page
                })
                .collect();
            let mut file = Builder::new(b'I', false, pages).build();
            let tiff = read(&file);
            let Value::At { offset, .. } = tiff.pages[0].get(tag).unwrap().value else {
                panic!("an inline {tag}")
            };
            for (n, page) in (0..).zip(&tiff.pages) {
                set_entry(&mut file, page.at, tag, 64 * 1024, offset + 2 * n);
            }
            assert!(file.len() < 100_000);
            assert!(matches!(plan(&file), Err(Error::TooLarge(_))), "{tag}");
        }
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
