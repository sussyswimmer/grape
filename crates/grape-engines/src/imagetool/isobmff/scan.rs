//! Compress on HEIC before anything starts (D28): what Grape must know of
//! a HEIC before its helper decodes it, read from the file's boxes and its
//! `meta` alone ([`Scan::read`], [`Scan::load`]: pure Rust, nothing
//! decoded), and what it refuses then. [`check`] runs it all, in this
//! order, the helper last:
//!
//! 1. **The content, by its `ftyp`** (D33, D41's sniff): HEIC goes on.
//!    AVIF, whatever its name, is refused ([`AVIF`]): AVIF has no tools
//!    (D41), and Compress writes HEIC. HEIF that names another codec gets
//!    D33's "… is a kind of HEIF image Grape can't read yet."
//!    ([`heif::other_heif`]), as a conversion does. What isn't HEIF never
//!    comes here (the dispatch goes by the content's kind), and is "…
//!    isn't a readable HEIC image." if it does.
//! 2. **A sequence or a burst** (a `moov`): [`SEQUENCE`].
//! 3. **`meta`**, read by the [model](super::items): a broken one, a
//!    primary item that isn't a picture, or one made of nothing or of
//!    items that aren't there, is "… isn't a readable HEIC image.".
//!    Collected ([`Scan`]):
//!    - the primary item, and the coded items its picture is made of
//!      ([`Scan::coded`]): itself, or what it's derived from by `dimg`, as
//!      far as that goes (a `grid`'s tiles, an `iden`'s or `iovl`'s inputs,
//!      a `tmap`'s base image, the first of its two);
//!    - their codec ([`Scan::codec`]) and bit depths: each coded item's
//!      `hvcC` (luma and chroma, bytes 17 and 18 of the record, low 3 bits,
//!      plus 8) and the `pixi` of the primary and of each coded item;
//!    - the primary's `ispe`, `irot` and `imir`, and whether an `irot` or
//!      `imir` turns it or a picture it's made of ([`Scan::turns`]); its
//!      `colr` (`nclx`, and whether there's an ICC profile), `pasp` and
//!      `clap`, or its first coded item's when it has none;
//!    - the other pictures ([`Scan::others`]): each picture that isn't
//!      hidden, isn't the primary, isn't a thumbnail (a `thmb` from it),
//!      an auxiliary image (an `auxl` from it: alpha, depth, a gain map, a
//!      matte), a `tmap`, a picture another is made of (a `dimg` to it:
//!      a grid's tiles, which MIAF hides and some writers don't), or in an
//!      `altr` group with the primary. A stereo pair (`ster`), Apple's
//!      spatial photos, a burst or a collection stored as items have one;
//!    - the metadata items that describe the primary ([`Carried`]: each
//!      item that isn't a picture with a `cdsc` to the primary, as Remove
//!      Metadata drops them: Exif, XMP and other `mime` items, `uri `
//!      items, IPTC, and any other type), their `infe` fields and where
//!      their data is. Those that describe another picture (a gain map's
//!      or a depth map's XMP) go with it.
//! 4. **Refusals** ([`Scan::refusal`]), each `NotYetSupported`, in this
//!    order: a picture coded in AV1 ([`AVIF`]: libheif writes AV1 grids
//!    with no codec brand, which sniff as HEIC) or in another codec than
//!    HEVC ([`CODEC`]: AVC, JPEG, JPEG 2000, VVC, uncompressed, which a
//!    conversion may read); more than 8 bits ([`DEEP`]); another picture
//!    ([`PICTURES`]); non-square pixels ([`PIXELS`]); a `colr` `nclx` that
//!    isn't sRGB-like ([`Nclx::srgb_like`]) with no ICC profile, where the
//!    helper hands back no profile for one ([`WIDE`]; [`Bounds`]: libheif
//!    on the PC; ImageIO hands back its color space's, so on a Mac the
//!    pipeline decides after the decode); and, at High, an upright `ispe`
//!    over one HEVC picture ([`HIGH`], with [`Encoder::max_picture`]'s
//!    bounds; `clap` only crops, so this is conservative).
//! 5. **The metadata items' data** ([`Scan::load`]), read whole from the
//!    file or `idat` (data that names another data reference is this
//!    file's, as libheif reads it), [`MAX_CARRIED`] at most in all (over
//!    it, "… is too large to convert."). An item with no data (no `iloc`
//!    entry) holds nothing, and isn't carried. One whose data is in other
//!    items (construction method 2) or that's protected (`ipro`) is
//!    [`LAYOUT`]. The first carried Exif's Orientation is read
//!    ([`Scan::orientation`]): IFD0's, 1 to 8, as Remove Metadata reads
//!    it.
//! 6. **The helper can't write HEIC** ([`Encoder::check_encode`], asked
//!    once per process for each helper): D33's words, before anything is
//!    decoded.
//!
//! **Orientation:** the helper decodes the primary upright by its `irot`
//! and `imir` ([`Scan::turns`]), as libheif does; the format turns no
//! picture by its Exif, and libheif doesn't. A Samsung portrait (its
//! `irot` blanked into a `free` property) and an iOS 11 photo turn only by
//! their Exif's Orientation, so their pixels come back as stored, as JPEG
//! Compress keeps a JPEG's, and the Exif they carry must still say how to
//! turn them; when the helper turns the picture, the Exif must say 1.
//! [`Scan::orientation`] and [`Scan::turns`] are what the graft decides by.
//!
//! **Memory:** `meta` (within [`MAX_META`](super::boxes::MAX_META)) and its
//! model, an index of the items' properties and references, and `idat`'s
//! content, until [`Scan::load`] has read the carried data, [`MAX_CARRIED`]
//! at most, which it holds instead.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufReader, Read};
use std::ops::Range;
use std::path::Path;

use grape_core::CompressPreset;
use grape_heif_proto::{self as proto, Sniff};

use super::super::exif::{self, MAX_EXIF};
use super::super::strip::{self, read_at, Error, In};
use super::boxes::{children, load, Child, Fields, Top};
use super::items::{file_meta, is_sequence, name, Item, Location, Meta, Ref, Role};
use crate::cancel::Cancellable;
use crate::heif::{self, Encoder};
use crate::publish::{self, Names};
use crate::{CancelToken, GrapeError};

/// Why an image sequence or a burst is refused.
pub(crate) const SEQUENCE: &str =
    "is an image sequence. Compress works on still HEIC photos for now.";

/// Why AVIF content is refused, whatever its name.
pub(crate) const AVIF: &str = "is an AVIF image. Compress works on HEIC photos for now.";

/// Why a picture of more than 8 bits is refused.
pub(crate) const DEEP: &str = "is a 10-bit (HDR) photo, which Compress can't keep yet.";

/// Why a file with another picture is refused.
pub(crate) const PICTURES: &str = "holds more than one picture, which Compress can't keep yet.";

/// Why non-square pixels are refused.
pub(crate) const PIXELS: &str = "has non-square pixels, which Compress can't keep yet.";

/// Why a picture over one HEVC picture is refused at High.
pub(crate) const HIGH: &str = "is too large to compress at High yet. Try Balanced.";

/// Why a wide color space with no ICC profile is refused: encoded without
/// one, its colors would shift.
pub(crate) const WIDE: &str = "uses a wide color space that Compress can't keep yet.";

/// Why a picture coded in another codec than HEVC or AV1 is refused: HEIF
/// whose `ftyp` names no codec (rule 5 of D41's sniff) but whose picture
/// is AVC, JPEG, JPEG 2000, VVC, uncompressed (`unci`), …, which a
/// conversion may read, but Compress, which writes HEIC, doesn't change.
pub(crate) const CODEC: &str = "is a kind of HEIF image Compress can't work on yet.";

/// Why metadata Compress can't read to carry is refused.
pub(crate) const LAYOUT: &str = "is laid out in a way Compress can't handle yet.";

/// The most bytes of metadata items Compress carries, in all.
pub(crate) const MAX_CARRIED: u64 = 16 << 20;

/// The item types made from other pictures (`dimg`).
const DERIVED: [&[u8; 4]; 4] = [b"grid", b"iden", b"iovl", b"tmap"];

/// What codes the pictures the primary is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    /// `hvc1`: HEIC.
    Hevc,
    /// `av01` (and the rest `hvc1`): AVIF.
    Av1,
    /// The first coded item of another type (`avc1`, `jpeg`, `j2k1`,
    /// `vvc1`, `unci`, `hvt1`, …).
    Other([u8; 4]),
}

/// A `colr` of type `nclx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Nclx {
    pub(crate) primaries: u16,
    pub(crate) transfer: u16,
    pub(crate) matrix: u16,
    pub(crate) full_range: bool,
}

impl Nclx {
    /// Whether it's sRGB as far as Compress is concerned: SDR primaries
    /// close to sRGB's, BT.709 (1), unspecified (2) or BT.601's (5, 625
    /// lines; 6, 525 lines), and an SDR transfer, BT.709 (1), unspecified
    /// (2), BT.601 (6) or sRGB (13). Wide primaries (P3's 12, BT.2020's 9,
    /// …) and HDR transfers (PQ's 16, HLG's 18) aren't.
    pub(crate) fn srgb_like(&self) -> bool {
        matches!(self.primaries, 1 | 2 | 5 | 6) && matches!(self.transfer, 1 | 2 | 6 | 13)
    }
}

/// What the helper takes, which [`Scan::refusal`] holds a file to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bounds {
    /// The largest picture it encodes, in pixels and on a side
    /// ([`Encoder::max_picture`]).
    pub(crate) picture: (u64, u32),
    /// Whether it hands back an ICC profile for a picture whose color is an
    /// `nclx` alone: ImageIO hands back its color space's (macOS), libheif
    /// none (the PC, D33).
    pub(crate) nclx_profile: bool,
}

impl Bounds {
    /// `encoder`'s, on this OS.
    pub(crate) fn of(encoder: &Encoder) -> Bounds {
        Bounds {
            picture: encoder.max_picture(),
            nclx_profile: cfg!(target_os = "macos"),
        }
    }
}

/// Why [`Scan::refusal`] refuses a file, in its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Coded in AV1: [`AVIF`].
    Avif,
    /// Coded in another codec than HEVC or AV1: [`CODEC`].
    Codec([u8; 4]),
    /// [`DEEP`].
    Deep,
    /// [`PICTURES`].
    Pictures,
    /// [`PIXELS`].
    Pixels,
    /// [`WIDE`].
    Wide,
    /// [`HIGH`].
    High,
}

impl Refusal {
    /// The end of a sentence that starts with the file's name, for
    /// [`strip::refused`] (`NotYetSupported`).
    pub(crate) fn why(self) -> &'static str {
        match self {
            Refusal::Avif => AVIF,
            Refusal::Codec(_) => CODEC,
            Refusal::Deep => DEEP,
            Refusal::Pictures => PICTURES,
            Refusal::Pixels => PIXELS,
            Refusal::Wide => WIDE,
            Refusal::High => HIGH,
        }
    }
}

/// A metadata item that describes the primary, to carry into the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Carried {
    pub(crate) id: u32,
    /// Its item type: `Exif`, `mime` (a v0 or v1 `infe` too), `uri `,
    /// `iptc`, or another.
    pub(crate) kind: [u8; 4],
    pub(crate) role: Role,
    /// `infe`'s flag 1.
    pub(crate) hidden: bool,
    /// Its `item_name`.
    pub(crate) name: Vec<u8>,
    /// A `mime` item's `content_type`, a `uri ` item's `item_uri_type`;
    /// empty for another.
    pub(crate) content_type: Vec<u8>,
    /// A `mime` item's `content_encoding`; empty for none.
    pub(crate) encoding: Vec<u8>,
    /// Its data, once [`Scan::load`] has read it.
    pub(crate) data: Vec<u8>,
    /// Its `infe`'s `item_protection_index` (0: none).
    protection: u16,
    /// Its `iloc` entry's construction method.
    method: u8,
    /// Where its data is, in the file (method 0) or in `idat`'s content
    /// (method 1), its extents in order.
    pieces: Vec<Range<u64>>,
}

/// What Compress on HEIC knows of a source before its helper starts
/// ([module docs](self)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) primary: u32,
    /// The primary's item type (`hvc1`, `grid`, `iden`, …).
    pub(crate) kind: [u8; 4],
    /// The coded items its picture is made of, in the order found.
    pub(crate) coded: Vec<u32>,
    pub(crate) codec: Codec,
    /// The most bits for luma and for chroma that the coded items'
    /// `hvcC` records say.
    pub(crate) hvcc: Option<(u8, u8)>,
    /// The most bits per channel that a `pixi` of the primary or a coded
    /// item says.
    pub(crate) pixi: Option<u8>,
    /// The primary's `ispe`: its width and height as stored.
    pub(crate) ispe: Option<(u32, u32)>,
    /// The primary's `irot`: quarter turns anticlockwise, 0 to 3.
    pub(crate) irot: u8,
    /// The primary's `imir`: its axis (0 vertical, 1 horizontal).
    pub(crate) imir: Option<u8>,
    /// Whether an `irot` or `imir` turns the primary or a picture it's
    /// made of ([`Scan::turns`]).
    turned: bool,
    /// `pasp`'s spacings, horizontal and vertical.
    pub(crate) pasp: Option<(u32, u32)>,
    /// Whether there's a `clap`, which crops the picture.
    pub(crate) clap: bool,
    /// The `colr` of type `nclx`.
    pub(crate) nclx: Option<Nclx>,
    /// Whether there's a `colr` with an ICC profile (`prof` or `rICC`).
    pub(crate) icc: bool,
    /// The other pictures ([module docs](self)), in `iinf`'s order.
    pub(crate) others: Vec<u32>,
    /// The metadata items that describe the primary, in `iinf`'s order.
    pub(crate) carried: Vec<Carried>,
    /// The first carried Exif's IFD0 Orientation, 1 to 8, once
    /// [`Scan::load`] has read it.
    pub(crate) orientation: Option<u16>,
    /// `idat`'s content, for data there.
    idat: Vec<u8>,
}

/// The properties and references of each item, looked up by id.
struct Index<'a> {
    meta: &'a Meta,
    items: HashMap<u32, &'a Item>,
    /// Each item's properties, in its associations' order.
    props: HashMap<u32, Vec<&'a Child>>,
    /// The references from each item.
    refs: HashMap<u32, Vec<&'a Ref>>,
}

impl<'a> Index<'a> {
    fn new(m: &'a Meta) -> Index<'a> {
        let items = m.items.iter().map(|item| (item.id, item)).collect();
        let mut props: HashMap<u32, Vec<&Child>> = HashMap::new();
        for entry in m.maps.iter().flat_map(|map| &map.entries) {
            let list = props.entry(entry.item).or_default();
            // Index 0 is no property; the model checked the others.
            list.extend(
                m.props(entry)
                    .iter()
                    .filter_map(|a| m.properties.get(usize::from(a.index).checked_sub(1)?)),
            );
        }
        let mut refs: HashMap<u32, Vec<&Ref>> = HashMap::new();
        for r in &m.refs {
            refs.entry(r.from).or_default().push(r);
        }
        Index {
            meta: m,
            items,
            props,
            refs,
        }
    }

    /// Item `id`'s first property of type `kind`.
    fn prop(&self, id: u32, kind: &[u8; 4]) -> Option<&'a Child> {
        self.all(id, kind).next()
    }

    /// Item `id`'s properties of type `kind`.
    fn all<'b>(&'b self, id: u32, kind: &'b [u8; 4]) -> impl Iterator<Item = &'a Child> + 'b {
        self.props
            .get(&id)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |p| &p.kind == kind)
    }

    /// The items `id` refers to by `kind`, in order.
    fn to<'b>(&'b self, id: u32, kind: &'b [u8; 4]) -> impl Iterator<Item = u32> + 'b {
        self.refs
            .get(&id)
            .into_iter()
            .flatten()
            .filter(move |r| &r.kind == kind)
            .flat_map(|r| self.meta.to(r).iter().copied())
    }

    /// Whether `id` refers to anything by `kind`.
    fn refers(&self, id: u32, kind: &[u8; 4]) -> bool {
        self.to(id, kind).next().is_some()
    }
}

impl Scan {
    /// Reads the HEIF `r` (`len` bytes) for Compress: its boxes, `meta`
    /// and what that says ([module docs](self), steps 2 and 3). The
    /// carried items' data is read by [`Scan::load`].
    pub(crate) fn read(r: &mut dyn In, len: u64) -> Result<Scan, Error> {
        let top = Top::read(r, len)?;
        if top.boxes.first().map(|b| &b.kind) != Some(b"ftyp") {
            return Err(Error::Broken("no ftyp box first".into()));
        }
        if is_sequence(&top) {
            return Err(Error::Refused(SEQUENCE.into()));
        }
        let m = Meta::parse(load(r, &file_meta(&top)?)?)?;
        let ix = Index::new(&m);
        let primary = m.primary;
        // The model checked that it's there.
        let kind = ix.items.get(&primary).map_or(*b"\0\0\0\0", |i| i.kind);
        if !ix
            .items
            .get(&primary)
            .is_some_and(|i| i.role() == Role::Image)
        {
            return Err(Error::Broken(format!(
                "the primary item is a {} item, not a picture",
                name(&kind)
            )));
        }
        let (coded, made_of) = coded(&ix, primary)?;
        let codec =
            coded
                .iter()
                .filter_map(|id| ix.items.get(id))
                .fold(Codec::Hevc, |codec, item| match (codec, &item.kind) {
                    (Codec::Other(_), _) | (_, b"hvc1") => codec,
                    (_, b"av01") => Codec::Av1,
                    (_, other) => Codec::Other(*other),
                });
        let mut hvcc = None;
        let mut pixi = pixi_bits(&m, ix.prop(primary, b"pixi"));
        for &id in &coded {
            if let Some(depth) = ix.prop(id, b"hvcC").and_then(|p| hvcc_bits(&m, p)) {
                let (luma, chroma) = hvcc.unwrap_or(depth);
                hvcc = Some((luma.max(depth.0), chroma.max(depth.1)));
            }
            pixi = match (pixi, pixi_bits(&m, ix.prop(id, b"pixi"))) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
        }
        let first = coded.first().copied();
        // The primary's, or its first coded item's.
        let either = |kind: &[u8; 4]| -> Vec<&Child> {
            let own: Vec<&Child> = ix.all(primary, kind).collect();
            match first {
                Some(id) if own.is_empty() => ix.all(id, kind).collect(),
                _ => own,
            }
        };
        let colr = either(b"colr");
        let fields = |p: &Child| Fields::new(&m.bytes, p.content());
        let nclx = colr.iter().find_map(|p| {
            let mut f = fields(p);
            (&f.kind().ok()? == b"nclx").then_some(())?;
            Some(Nclx {
                primaries: f.u16().ok()?,
                transfer: f.u16().ok()?,
                matrix: f.u16().ok()?,
                full_range: f.u8().ok()? & 0x80 != 0,
            })
        });
        let icc = colr.iter().any(|p| {
            fields(p)
                .kind()
                .is_ok_and(|k| &k == b"prof" || &k == b"rICC")
        });
        let ispe = ix.prop(primary, b"ispe").and_then(|p| {
            let mut f = fields(p);
            f.full().ok()?;
            Some((f.u32().ok()?, f.u32().ok()?))
        });
        let irot = ix
            .prop(primary, b"irot")
            .and_then(|p| fields(p).u8().ok())
            .map_or(0, |b| b & 3);
        let imir = ix
            .prop(primary, b"imir")
            .and_then(|p| fields(p).u8().ok())
            .map(|b| b & 1);
        let pasp = either(b"pasp").first().and_then(|p| {
            let mut f = fields(p);
            Some((f.u32().ok()?, f.u32().ok()?))
        });
        let clap = !either(b"clap").is_empty();
        // An `irot` of 0 turns nothing; libheif applies every picture's
        // `irot` and `imir` on the way.
        let turns = |p: &Child| fields(p).u8().is_ok_and(|b| b & 3 != 0);
        let turned = made_of
            .iter()
            .any(|&id| ix.all(id, b"irot").any(turns) || ix.prop(id, b"imir").is_some());
        let others = others(&m, &ix);
        let carried = carried(&m, len)?;
        let idat = m.idat().map(|r| m.bytes[r].to_vec()).unwrap_or_default();
        Ok(Scan {
            primary,
            kind,
            coded,
            codec,
            hvcc,
            pixi,
            ispe,
            irot,
            imir,
            turned,
            pasp,
            clap,
            nclx,
            icc,
            others,
            carried,
            orientation: None,
            idat,
        })
    }

    /// The most bits a sample has, by `hvcC` and `pixi`: 8 when neither
    /// says more.
    pub(crate) fn bits(&self) -> u8 {
        let (luma, chroma) = self.hvcc.unwrap_or((8, 8));
        luma.max(chroma).max(self.pixi.unwrap_or(8))
    }

    /// Whether the helper turns the picture as it decodes it: an `irot`
    /// or `imir` of the primary or of a picture it's made of (libheif
    /// applies each on the way). Then what comes back is upright, and what
    /// the source's Exif says of its orientation no longer holds.
    pub(crate) fn turns(&self) -> bool {
        self.turned
    }

    /// The primary's size as the helper decodes it, `irot` applied, before
    /// any `clap`: `ispe`'s, turned.
    pub(crate) fn upright(&self) -> Option<(u32, u32)> {
        self.ispe
            .map(|(w, h)| if self.irot % 2 == 1 { (h, w) } else { (w, h) })
    }

    /// The first reason, in [module docs](self)' order, why Compress at
    /// `preset` refuses the file, with a helper that takes `bounds`.
    pub(crate) fn refusal(&self, preset: CompressPreset, bounds: Bounds) -> Option<Refusal> {
        let (max_pixels, max_side) = bounds.picture;
        let over = |(w, h): (u32, u32)| {
            u64::from(w) * u64::from(h) > max_pixels || w > max_side || h > max_side
        };
        match self.codec {
            Codec::Av1 => return Some(Refusal::Avif),
            Codec::Other(kind) => return Some(Refusal::Codec(kind)),
            Codec::Hevc => {}
        }
        if self.bits() > 8 {
            Some(Refusal::Deep)
        } else if !self.others.is_empty() {
            Some(Refusal::Pictures)
        } else if self.pasp.is_some_and(|(h, v)| h != v) {
            Some(Refusal::Pixels)
        } else if !bounds.nclx_profile && !self.icc && self.nclx.is_some_and(|n| !n.srgb_like()) {
            Some(Refusal::Wide)
        } else if preset == CompressPreset::High && self.upright().is_some_and(over) {
            Some(Refusal::High)
        } else {
            None
        }
    }

    /// Reads the carried items' data from `r`, the file [`Scan::read`]
    /// read ([module docs](self), step 5), and the first Exif's
    /// Orientation. An item with no data is no longer carried.
    pub(crate) fn load(&mut self, r: &mut dyn In) -> Result<(), Error> {
        let mut left = MAX_CARRIED;
        for item in &mut self.carried {
            if item.protection != 0 || item.method == 2 {
                return Err(Error::Refused(LAYOUT.into()));
            }
            let total = item
                .pieces
                .iter()
                .try_fold(0u64, |total, p| total.checked_add(p.end - p.start))
                .filter(|&total| total <= left)
                .ok_or_else(|| {
                    Error::TooLarge(format!(
                        "metadata items of over {} MiB to carry",
                        MAX_CARRIED >> 20
                    ))
                })?;
            left -= total;
            // Within MAX_CARRIED.
            let mut data = Vec::with_capacity(total as usize);
            for piece in &item.pieces {
                if item.method == 0 {
                    let at = data.len();
                    data.resize(at + (piece.end - piece.start) as usize, 0);
                    read_at(r, piece.start, &mut data[at..])?;
                } else {
                    // Within idat: carried() checked.
                    let bytes = usize::try_from(piece.start)
                        .ok()
                        .zip(usize::try_from(piece.end).ok())
                        .and_then(|(start, end)| self.idat.get(start..end))
                        .ok_or_else(|| {
                            Error::Broken(format!("item {}'s data runs past its end", item.id))
                        })?;
                    data.extend_from_slice(bytes);
                }
            }
            item.data = data;
        }
        self.carried.retain(|item| !item.data.is_empty());
        // Read: Compress holds the carried data from here on, not `idat`.
        self.idat = Vec::new();
        self.orientation = self
            .carried
            .iter()
            .find(|item| item.role == Role::Exif)
            .and_then(|item| orientation(&item.data));
        Ok(())
    }
}

/// The coded items the picture `primary` is made of ([module docs](self)),
/// in the order found: itself, or by `dimg` what it's derived from, each
/// once; and every picture on the way, `primary` first. A derived item
/// with nothing to derive from, or derived from an item that isn't there,
/// is broken.
fn coded(ix: &Index<'_>, primary: u32) -> Result<(Vec<u32>, Vec<u32>), Error> {
    let mut coded = Vec::new();
    let mut made_of = Vec::new();
    let mut seen = HashSet::from([primary]);
    let mut queue = VecDeque::from([primary]);
    while let Some(id) = queue.pop_front() {
        let Some(item) = ix.items.get(&id) else {
            return Err(Error::Broken(format!(
                "a picture made of item {id}, which isn't there"
            )));
        };
        made_of.push(id);
        if !DERIVED.contains(&&item.kind) {
            coded.push(id);
            continue;
        }
        let mut inputs: Vec<u32> = ix.to(id, b"dimg").collect();
        // A tone map's are its base image and its gain map: the picture
        // is the first.
        if &item.kind == b"tmap" {
            inputs.truncate(1);
        }
        if inputs.is_empty() {
            return Err(Error::Broken(format!(
                "a {} item made of nothing",
                name(&item.kind)
            )));
        }
        queue.extend(inputs.into_iter().filter(|&input| seen.insert(input)));
    }
    if coded.is_empty() {
        return Err(Error::Broken("a picture made of itself".into()));
    }
    Ok((coded, made_of))
}

/// The most bits per sample, luma and chroma, that the `hvcC` record `p`
/// says; `None` for one too short.
fn hvcc_bits(m: &Meta, p: &Child) -> Option<(u8, u8)> {
    let content = m.bytes.get(p.content())?;
    let (luma, chroma) = (content.get(17)?, content.get(18)?);
    Some(((luma & 7) + 8, (chroma & 7) + 8))
}

/// The most bits per channel that the `pixi` `p` says.
fn pixi_bits(m: &Meta, p: Option<&Child>) -> Option<u8> {
    let mut f = Fields::new(&m.bytes, p?.content());
    f.full().ok()?;
    let n = f.u8().ok()?;
    f.take(usize::from(n)).ok()?.iter().copied().max()
}

/// The pictures other than the primary that a reader shows on their own
/// ([module docs](self)), in `iinf`'s order.
fn others(m: &Meta, ix: &Index<'_>) -> Vec<u32> {
    let made_of: HashSet<u32> = m
        .refs
        .iter()
        .filter(|r| &r.kind == b"dimg")
        .flat_map(|r| m.to(r).iter().copied())
        .collect();
    let alternatives: HashSet<u32> = m
        .groups
        .iter()
        .filter(|g| &g.child.kind == b"altr" && m.members(g).contains(&m.primary))
        .flat_map(|g| m.members(g).iter().copied())
        .collect();
    m.items
        .iter()
        .filter(|item| {
            item.role() == Role::Image
                && !item.hidden
                && item.id != m.primary
                && &item.kind != b"tmap"
                && !ix.refers(item.id, b"thmb")
                && !ix.refers(item.id, b"auxl")
                && !made_of.contains(&item.id)
                && !alternatives.contains(&item.id)
        })
        .map(|item| item.id)
        .collect()
}

/// The metadata items that describe the primary ([module docs](self)), in
/// `iinf`'s order, with where their data is, checked to lie within the
/// file (`len` bytes) or `idat`; their data isn't read yet.
fn carried(m: &Meta, len: u64) -> Result<Vec<Carried>, Error> {
    let describe: HashSet<u32> = m
        .refs
        .iter()
        .filter(|r| &r.kind == b"cdsc" && m.to(r).contains(&m.primary))
        .map(|r| r.from)
        .collect();
    let locations: HashMap<u32, &Location> = m.locations.iter().map(|l| (l.id, l)).collect();
    let idat = m.idat().map_or(0, |r| r.len() as u64);
    let mut out = Vec::new();
    for item in m
        .items
        .iter()
        .filter(|item| describe.contains(&item.id) && item.role() != Role::Image)
    {
        let infe = Infe::read(m, item)?;
        let (method, pieces) = match locations.get(&item.id) {
            // No data: nothing to carry.
            None => (0, Vec::new()),
            Some(loc) => {
                let size = if loc.method == 1 { idat } else { len };
                let mut pieces = Vec::new();
                // Data in other items has no place of its own (refused by
                // load).
                if loc.method < 2 {
                    for span in m.spans(loc)? {
                        let end = span.end(size);
                        if span.start > end || end > size {
                            return Err(Error::Broken(format!(
                                "item {}'s data runs past its end",
                                item.id
                            )));
                        }
                        pieces.push(span.start..end);
                    }
                }
                (loc.method, pieces)
            }
        };
        out.push(Carried {
            id: item.id,
            kind: item.kind,
            role: item.role(),
            hidden: item.hidden,
            name: infe.name,
            content_type: infe.content_type,
            encoding: infe.encoding,
            data: Vec::new(),
            protection: infe.protection,
            method,
            pieces,
        });
    }
    Ok(out)
}

/// What an `infe` says of an item, past its id and type.
struct Infe {
    protection: u16,
    name: Vec<u8>,
    content_type: Vec<u8>,
    encoding: Vec<u8>,
}

impl Infe {
    /// `item`'s `infe`, read again: v0 and v1 hold a MIME item's name,
    /// content type and encoding; v2 and v3 the item's type, its name,
    /// and a `mime` item's content type and encoding or a `uri ` item's
    /// URI type. Strings with no NUL run to the end, as libheif reads
    /// them.
    fn read(m: &Meta, item: &Item) -> Result<Infe, Error> {
        let child = children(&m.bytes, item.range.clone(), 1)?;
        let Some(infe) = child.first() else {
            return Err(Error::Broken(format!("item {}'s infe", item.id)));
        };
        let mut f = Fields::new(&m.bytes, infe.content());
        let (version, _) = f.full()?;
        let typed = version >= 2;
        f.id(version == 3)?;
        let protection = f.u16()?;
        if typed {
            f.kind()?;
        }
        let name = f.text().to_vec();
        let (content_type, encoding) = match (typed, &item.kind) {
            (false, _) | (true, b"mime") => (f.text().to_vec(), f.text().to_vec()),
            (true, b"uri ") => (f.text().to_vec(), Vec::new()),
            _ => (Vec::new(), Vec::new()),
        };
        Ok(Infe {
            protection,
            name,
            content_type,
            encoding,
        })
    }
}

/// IFD0's Orientation (1 to 8) of the Exif item `data`: its 4-byte header
/// offset, then a TIFF structure (after `Exif\0\0` when there is one), its
/// first [`MAX_EXIF`] walked, as Remove Metadata reads it.
fn orientation(data: &[u8]) -> Option<u16> {
    let offset = u32::from_be_bytes(*data.first_chunk::<4>()?);
    let skip = usize::try_from(offset).ok()?.checked_add(4)?;
    let tiff = data.get(skip..)?;
    let tiff = tiff.strip_prefix(b"Exif\0\0").unwrap_or(tiff);
    exif::walk(&tiff[..tiff.len().min(MAX_EXIF)]).orientation
}

/// What Compress on HEIC knows of `src` (named in `names`) at `preset`
/// before its helper decodes it, or why it stops there ([module
/// docs](self)): the content, the boxes and `meta`, the refusals, the
/// carried data, and last whether `encoder` can write HEIC at all.
pub(crate) fn check(
    src: &Path,
    names: &Names,
    preset: CompressPreset,
    encoder: &Encoder,
    cancel: &CancelToken,
) -> Result<Scan, GrapeError> {
    let fail = |e: GrapeError| publish::or_cancelled(cancel, src, e);
    let file = File::open(src)
        .map_err(|e| fail(names.read_failed(format!("open {}: {e}", src.display()))))?;
    let len = file
        .metadata()
        .map_err(|e| fail(names.read_failed(format!("stat {}: {e}", src.display()))))?
        .len();
    let mut r = BufReader::with_capacity(64 * 1024, Cancellable::new(file, cancel));
    let mut head = Vec::with_capacity(proto::SNIFF_BYTES);
    Read::by_ref(&mut r)
        .take(proto::SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(|e| fail(names.read_failed(format!("read {}: {e}", src.display()))))?;
    match proto::sniff(&head) {
        Sniff::Heic => {}
        Sniff::Avif => return Err(strip::refused(names, src, AVIF)),
        Sniff::OtherHeif => return Err(heif::other_heif(names, "its ftyp names another codec")),
        Sniff::NotHeif => {
            return Err(names.decode_failed(format!("{}: not a HEIF file", src.display())))
        }
    }
    let failed = |e: Error| fail(scan_error(e, names, src));
    let mut scan = Scan::read(&mut r, len).map_err(failed)?;
    if let Some(refusal) = scan.refusal(preset, Bounds::of(encoder)) {
        return Err(strip::refused(names, src, refusal.why()));
    }
    scan.load(&mut r).map_err(failed)?;
    encoder.check_encode(names, cancel)?;
    Ok(scan)
}

/// The error for the user when the scan of `src` stopped.
fn scan_error(e: Error, names: &Names, src: &Path) -> GrapeError {
    match e {
        Error::Broken(why) => names.decode_failed(format!("{}: {why}", src.display())),
        Error::TooLarge(why) => names.too_large(format!("{}: {why}", src.display())),
        Error::Refused(why) => strip::refused(names, src, &why),
        Error::Read(e) | Error::Write(e) => {
            names.read_failed(format!("read {}: {e}", src.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;

    use grape_core::Format;
    use grape_heif_proto::HeifFormat;

    use super::super::tests::{
        auxc, bx, camera, hvcc, icc, imir, iphone, irot, ispe, layouts, meta_at, nclx, pasp, pixi,
        portrait, prop, simple, Builder, Item, Place, EXIF, GAIN, SECRET_XMP, THUMB, XMP,
    };
    use super::*;
    use crate::heif::tests::{fake, fake_image, nowhere};
    use crate::heif::Limits;
    use crate::output::unique_suffix;
    use crate::ErrorKind;
    use CompressPreset::{Balanced, High, Small};

    /// One HEVC picture ([`Encoder::max_picture`] for HEIC), and no profile
    /// back for an `nclx` (libheif's).
    const HEVC: Bounds = Bounds {
        picture: (35_651_584, 16_888),
        nclx_profile: false,
    };

    /// [`HEVC`] with the largest picture `pixels` and `side`.
    fn picture(pixels: u64, side: u32) -> Bounds {
        Bounds {
            picture: (pixels, side),
            ..HEVC
        }
    }

    fn read(file: &[u8]) -> Result<Scan, Error> {
        Scan::read(&mut Cursor::new(file), file.len() as u64)
    }

    /// `file` read and its carried data loaded.
    fn scan(file: &[u8]) -> Scan {
        let mut s = read(file).unwrap();
        s.load(&mut Cursor::new(file)).unwrap();
        s
    }

    fn loaded(file: &[u8]) -> Result<Scan, Error> {
        let mut s = read(file)?;
        s.load(&mut Cursor::new(file))?;
        Ok(s)
    }

    /// What `b`'s file is refused for at High.
    fn refusal(b: &Builder) -> Option<Refusal> {
        read(&b.build().0).unwrap().refusal(High, HEVC)
    }

    /// The carried items' ids.
    fn ids(s: &Scan) -> Vec<u32> {
        s.carried.iter().map(|c| c.id).collect()
    }

    /// Where item `id`'s `iloc` entry is in `file` (v1: id, construction
    /// method, data reference, extent count, offset, length).
    fn entry_at(file: &[u8], id: u32) -> usize {
        let top = Top::read(&mut Cursor::new(file), file.len() as u64).unwrap();
        let header = file_meta(&top).unwrap();
        let m = Meta::parse(load(&mut Cursor::new(file), &header).unwrap()).unwrap();
        let loc = m.locations.iter().find(|l| l.id == id).unwrap();
        meta_at(file) + loc.range.start
    }

    /// `simple()` with `props` as the primary's properties instead.
    fn primary_with(props: Vec<Vec<u8>>) -> Builder {
        let mut b = simple();
        b.items[0].props = (1..=props.len() as u16).map(|i| (i, i == 1)).collect();
        b.properties = props;
        b
    }

    /// A grid (item 1, in `idat`) of two tiles, 2 and 3, with `tiles`'
    /// properties (1-based into `props`) each, the grid having `grid`'s.
    fn grid(props: Vec<Vec<u8>>, grid: Vec<u16>, tiles: [Vec<u16>; 2]) -> Builder {
        let mut g = Item::new(1, *b"grid", Place::Idat, vec![0, 0, 0, 1, 0, 64, 0, 48]);
        g.props = grid.into_iter().map(|i| (i, false)).collect();
        let mut items = vec![g];
        for (id, props) in [2, 3].into_iter().zip(tiles) {
            let mut tile = Item::picture(id, 0xB0 | id as u8, 50);
            tile.hidden = true;
            tile.props = props.into_iter().map(|i| (i, i == 1)).collect();
            items.push(tile);
        }
        items.push(Item::exif(EXIF, &camera()));
        let mut b = Builder::new(items, 1);
        b.properties = props;
        b.refs = vec![(*b"dimg", 1, vec![2, 3]), (*b"cdsc", EXIF, vec![1])];
        b
    }

    #[test]
    fn an_iphone_like_file_gives_every_field() {
        let (file, layout) = iphone().build();
        let s = scan(&file);
        assert_eq!(s.primary, 1);
        assert_eq!(&s.kind, b"grid");
        assert_eq!(s.coded, [2, 3]);
        assert_eq!(s.codec, Codec::Hevc);
        assert_eq!(s.hvcc, Some((8, 8)));
        assert_eq!((s.pixi, s.bits()), (None, 8));
        // The grid has colr and irot, and no ispe.
        assert_eq!((s.ispe, s.upright()), (None, None));
        assert_eq!((s.irot, s.imir, s.turns()), (1, None, true));
        assert!(s.icc && s.nclx.is_none() && s.pasp.is_none() && !s.clap);
        // The thumbnail, the gain map, the depth map and the tone map are
        // no other pictures; the tiles are hidden.
        assert!(s.others.is_empty());
        // The Exif describes the grid; the XMP the gain map, so it goes
        // with it.
        assert_eq!(ids(&s), [EXIF]);
        let exif = &s.carried[0];
        assert_eq!(exif.data, layout.data(EXIF));
        assert_eq!(
            (&exif.kind, exif.role, exif.hidden),
            (b"Exif", Role::Exif, true)
        );
        assert!(exif.name.is_empty() && exif.content_type.is_empty() && exif.encoding.is_empty());
        assert_eq!(s.orientation, Some(1));
        for preset in [Small, Balanced, High] {
            assert_eq!(s.refusal(preset, HEVC), None);
        }
    }

    #[test]
    fn a_simple_file_carries_its_exif_and_xmp() {
        let (file, layout) = simple().build();
        let s = read(&file).unwrap();
        // Not read yet.
        assert!(s.carried.iter().all(|c| c.data.is_empty()));
        assert_eq!(s.orientation, None);
        let s = scan(&file);
        assert_eq!((s.primary, &s.kind, &s.coded[..]), (1, b"hvc1", &[1][..]));
        assert_eq!((s.ispe, s.upright()), (Some((64, 48)), Some((64, 48))));
        assert_eq!((s.irot, s.imir, s.turns()), (0, None, false));
        assert_eq!(ids(&s), [EXIF, XMP]);
        let xmp = &s.carried[1];
        assert_eq!(xmp.data, SECRET_XMP.as_bytes());
        assert_eq!(xmp.data, layout.data(XMP));
        assert_eq!((&xmp.kind, xmp.role), (b"mime", Role::Xmp));
        assert_eq!(xmp.content_type, b"application/rdf+xml");
        assert_eq!(s.carried[0].data, layout.data(EXIF));
        assert_eq!(s.orientation, Some(1));
    }

    #[test]
    fn every_layout_carries_its_metadata_from_where_it_is() {
        for (name, b) in layouts() {
            let (file, layout) = b.build();
            let s = loaded(&file).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let describe: Vec<u32> = b
                .refs
                .iter()
                .filter(|(kind, _, to)| kind == b"cdsc" && to.contains(&b.primary))
                .map(|(_, from, _)| *from)
                .collect();
            let expected: Vec<u32> = b
                .items
                .iter()
                .filter(|i| describe.contains(&i.id) && i.place != Place::Nowhere)
                .map(|i| i.id)
                .collect();
            assert_eq!(ids(&s), expected, "{name}");
            for c in &s.carried {
                assert_eq!(c.data, layout.data(c.id), "{name}: {}", c.id);
            }
            assert_eq!(s.primary, b.primary, "{name}");
            assert_eq!(s.refusal(High, HEVC), None, "{name}");
        }
    }

    #[test]
    fn a_sequence_is_refused_before_its_meta_is_read() {
        let mut b = simple();
        b.after = vec![bx(b"moov", b"")];
        let seq = |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == SEQUENCE);
        assert!(seq(&b.build().0));
        // A sequence with no meta at all.
        let only = [bx(b"ftyp", b"msf1\0\0\0\0msf1hevc"), bx(b"moov", b"")].concat();
        assert!(seq(&only));
    }

    #[test]
    fn what_cant_be_read_as_one_picture_is_broken() {
        let broken = |file: &[u8]| matches!(read(file), Err(Error::Broken(_)));
        // No ftyp first; no meta.
        let (file, _) = simple().build();
        assert!(broken(&file[24..]));
        assert!(broken(&bx(b"ftyp", b"heic\0\0\0\0mif1heic")));
        // The primary is the Exif.
        let mut b = simple();
        b.primary = EXIF;
        assert!(broken(&b.build().0));
        // A grid made of nothing, of an item that isn't there, of itself,
        // and two grids made of each other.
        let tiles = || [vec![1], vec![1]];
        let mut b = grid(vec![hvcc(8, 8)], vec![], tiles());
        b.refs.retain(|r| &r.0 != b"dimg");
        assert!(broken(&b.build().0));
        let mut b = grid(vec![hvcc(8, 8)], vec![], tiles());
        b.refs[0].2 = vec![2, 9];
        assert!(broken(&b.build().0));
        let mut b = grid(vec![hvcc(8, 8)], vec![], tiles());
        b.refs[0].2 = vec![1];
        assert!(broken(&b.build().0));
        let mut b = grid(vec![hvcc(8, 8)], vec![], tiles());
        b.items
            .push(Item::new(4, *b"grid", Place::Idat, vec![0; 8]));
        b.refs[0].2 = vec![4];
        b.refs.push((*b"dimg", 4, vec![1]));
        assert!(broken(&b.build().0));
        // It's still a picture made of tiles once one is listed twice.
        let mut b = grid(vec![hvcc(8, 8)], vec![], tiles());
        b.refs[0].2 = vec![2, 3, 2];
        assert_eq!(read(&b.build().0).unwrap().coded, [2, 3]);
    }

    #[test]
    fn a_picture_coded_in_av1_or_another_codec_is_refused() {
        for (kind, expected) in [
            (*b"av01", Some(Refusal::Avif)),
            (*b"avc1", Some(Refusal::Codec(*b"avc1"))),
            (*b"jpeg", Some(Refusal::Codec(*b"jpeg"))),
            (*b"j2k1", Some(Refusal::Codec(*b"j2k1"))),
            (*b"vvc1", Some(Refusal::Codec(*b"vvc1"))),
            (*b"unci", Some(Refusal::Codec(*b"unci"))),
            (*b"hvc1", None),
        ] {
            let mut b = simple();
            b.items[0].kind = kind;
            assert_eq!(refusal(&b), expected, "{kind:?}");
            // A grid of it.
            let mut b = grid(vec![hvcc(8, 8)], vec![], [vec![1], vec![1]]);
            b.items[2].kind = kind;
            assert_eq!(refusal(&b), expected, "grid of {kind:?}");
        }
        // Another codec is told whichever tile has it; AV1 only with HEVC.
        let mut b = grid(vec![hvcc(8, 8)], vec![], [vec![1], vec![1]]);
        b.items[1].kind = *b"av01";
        b.items[2].kind = *b"jpeg";
        assert_eq!(refusal(&b), Some(Refusal::Codec(*b"jpeg")));
        b.items[1].kind = *b"jpeg";
        b.items[2].kind = *b"av01";
        assert_eq!(refusal(&b), Some(Refusal::Codec(*b"jpeg")));
        // Before the bit depth.
        let mut b = primary_with(vec![hvcc(8, 8), ispe(64, 48), pixi(&[10, 10, 10])]);
        b.items[0].kind = *b"av01";
        assert_eq!(refusal(&b), Some(Refusal::Avif));
        // A tone map's picture is its base image, the first of its two:
        // not its gain map.
        let mut b = grid(vec![hvcc(8, 8)], vec![], [vec![1], vec![1]]);
        b.items[0].kind = *b"tmap";
        b.items[2].kind = *b"av01";
        let s = read(&b.build().0).unwrap();
        assert_eq!((s.coded, s.codec), (vec![2], Codec::Hevc));
    }

    #[test]
    fn more_than_8_bits_is_refused() {
        for (props, deep) in [
            (vec![hvcc(10, 10)], true),
            (vec![hvcc(10, 8)], true),
            (vec![hvcc(8, 10)], true),
            (vec![hvcc(12, 12)], true),
            (vec![hvcc(8, 8), pixi(&[8, 10, 8])], true),
            (vec![hvcc(8, 8), pixi(&[16])], true),
            (vec![hvcc(8, 8), pixi(&[8, 8, 8])], false),
            (vec![hvcc(8, 8)], false),
            // A record too short to say.
            (vec![prop(b"hvcC", &[1; 18])], false),
        ] {
            let b = primary_with(props.clone());
            let s = read(&b.build().0).unwrap();
            assert_eq!(s.bits() > 8, deep, "{props:?}");
            assert_eq!(refusal(&b) == Some(Refusal::Deep), deep, "{props:?}");
        }
        let s = read(&primary_with(vec![hvcc(10, 8), pixi(&[12])]).build().0).unwrap();
        assert_eq!((s.hvcc, s.pixi, s.bits()), (Some((10, 8)), Some(12), 12));
        // A grid's second tile; a pixi on a tile or on the grid.
        let props = vec![hvcc(8, 8), hvcc(10, 10), pixi(&[10, 10, 10])];
        let b = grid(props.clone(), vec![], [vec![1], vec![2]]);
        assert_eq!(refusal(&b), Some(Refusal::Deep));
        let b = grid(props.clone(), vec![], [vec![1], vec![1, 3]]);
        assert_eq!(refusal(&b), Some(Refusal::Deep));
        let b = grid(props.clone(), vec![3], [vec![1], vec![1]]);
        assert_eq!(refusal(&b), Some(Refusal::Deep));
        let b = grid(props, vec![], [vec![1], vec![1]]);
        assert_eq!(refusal(&b), None);
        // A 10-bit thumbnail or alpha plane isn't the picture.
        let mut b = simple();
        b.properties.push(hvcc(10, 10));
        b.properties.push(auxc("urn:mpeg:hevc:2015:auxid:1"));
        let mut thumb = Item::picture(THUMB, 0xC0, 10);
        thumb.props = vec![(3, true)];
        let mut alpha = Item::picture(GAIN, 0xD0, 10);
        alpha.hidden = true;
        alpha.props = vec![(3, true), (4, true)];
        b.items.extend([thumb, alpha]);
        b.refs
            .extend([(*b"thmb", THUMB, vec![1]), (*b"auxl", GAIN, vec![1])]);
        assert_eq!(refusal(&b), None);
    }

    #[test]
    fn another_picture_is_refused_but_a_thumbnail_auxiliary_tone_map_alternative_or_tile_isnt() {
        // `simple` and picture 5.
        let with = |change: &dyn Fn(&mut Builder)| {
            let mut b = simple();
            b.items.push(Item::picture(5, 0xA5, 10));
            change(&mut b);
            read(&b.build().0).unwrap()
        };
        let s = with(&|_| {});
        assert_eq!(s.others, [5]);
        assert_eq!(s.refusal(High, HEVC), Some(Refusal::Pictures));
        assert_eq!(s_refusal(&with(&|_| {}), Small), Some(Refusal::Pictures));
        for (why, change) in [
            (
                "hidden",
                &(|b: &mut Builder| b.items[3].hidden = true) as &dyn Fn(&mut Builder),
            ),
            ("thumbnail", &|b| b.refs.push((*b"thmb", 5, vec![1]))),
            ("auxiliary", &|b| b.refs.push((*b"auxl", 5, vec![1]))),
            ("tone map", &|b| b.items[3].kind = *b"tmap"),
            ("alternative", &|b| {
                b.groups.push((*b"altr", 50, vec![5, 1]))
            }),
            ("not a picture", &|b| b.items[3].kind = *b"abcd"),
        ] {
            let s = with(change);
            assert!(s.others.is_empty(), "{why}");
            assert_eq!(s.refusal(High, HEVC), None, "{why}");
        }
        for (why, change) in [
            (
                "another's alternative",
                &(|b: &mut Builder| b.groups.push((*b"altr", 50, vec![5, 6])))
                    as &dyn Fn(&mut Builder),
            ),
            ("a stereo pair", &|b| {
                b.groups.push((*b"ster", 50, vec![1, 5]))
            }),
            ("a thumbnail's picture", &|b| {
                b.refs.push((*b"thmb", 1, vec![5]))
            }),
        ] {
            assert_eq!(with(change).others, [5], "{why}");
        }
        // Tiles that aren't hidden are the grid's picture.
        let mut b = grid(vec![hvcc(8, 8)], vec![], [vec![1], vec![1]]);
        b.items[1].hidden = false;
        b.items[2].hidden = false;
        assert_eq!(refusal(&b), None);
        // In iinf's order.
        let s = with(&|b| {
            b.items.insert(1, Item::picture(7, 0xA7, 10));
        });
        assert_eq!(s.others, [7, 5]);
    }

    fn s_refusal(s: &Scan, preset: CompressPreset) -> Option<Refusal> {
        s.refusal(preset, HEVC)
    }

    #[test]
    fn non_square_pixels_are_refused() {
        for (h, v, refused) in [(4, 3, true), (3, 4, true), (1, 1, false), (2, 2, false)] {
            let b = primary_with(vec![hvcc(8, 8), ispe(64, 48), pasp(h, v)]);
            assert_eq!(read(&b.build().0).unwrap().pasp, Some((h, v)));
            assert_eq!(refusal(&b) == Some(Refusal::Pixels), refused, "{h}:{v}");
        }
        // A grid's, or its first tile's when it has none.
        let props = vec![hvcc(8, 8), pasp(4, 3), pasp(1, 1)];
        assert_eq!(
            refusal(&grid(props.clone(), vec![], [vec![1, 2], vec![1, 2]])),
            Some(Refusal::Pixels)
        );
        assert_eq!(
            refusal(&grid(props.clone(), vec![2], [vec![1], vec![1]])),
            Some(Refusal::Pixels)
        );
        assert_eq!(
            refusal(&grid(props, vec![3], [vec![1, 2], vec![1, 2]])),
            None
        );
    }

    #[test]
    fn over_one_hevc_picture_is_refused_at_high_only() {
        for ((w, h), over) in [
            ((8000, 6000), true),
            ((16_889, 100), true),
            ((100, 16_889), true),
            ((5971, 5971), true),
            ((5970, 5970), false),
            ((16_888, 2111), false),
            ((4032, 3024), false),
        ] {
            for angle in [0, 1, 2, 3] {
                let b = primary_with(vec![hvcc(8, 8), ispe(w, h), irot(angle)]);
                let s = read(&b.build().0).unwrap();
                let upright = if angle % 2 == 1 { (h, w) } else { (w, h) };
                assert_eq!(s.upright(), Some(upright));
                let high = over.then_some(Refusal::High);
                assert_eq!(s.refusal(High, HEVC), high, "{w}x{h}");
                assert_eq!(s.refusal(Balanced, HEVC), None);
                assert_eq!(s.refusal(Small, HEVC), None);
            }
        }
        // By the bounds given.
        let s = read(&primary_with(vec![hvcc(8, 8), ispe(64, 48)]).build().0).unwrap();
        assert_eq!(s.refusal(High, picture(64 * 48, 64)), None);
        assert_eq!(
            s.refusal(High, picture(64 * 48 - 1, 64)),
            Some(Refusal::High)
        );
        assert_eq!(s.refusal(High, picture(64 * 48, 63)), Some(Refusal::High));
        // With no ispe, the helper's own cap is what holds.
        let s = read(&primary_with(vec![hvcc(8, 8)]).build().0).unwrap();
        assert_eq!((s.ispe, s.refusal(High, picture(1, 1))), (None, None));
    }

    #[test]
    fn the_refusals_come_in_their_order() {
        let mut b = primary_with(vec![
            hvcc(10, 10),
            ispe(8000, 6000),
            pasp(4, 3),
            nclx(12, 13, 6, true),
        ]);
        b.items.push(Item::picture(5, 0xA5, 10));
        assert_eq!(refusal(&b), Some(Refusal::Deep));
        b.properties[0] = hvcc(8, 8);
        assert_eq!(refusal(&b), Some(Refusal::Pictures));
        b.items[3].hidden = true;
        assert_eq!(refusal(&b), Some(Refusal::Pixels));
        b.properties[2] = pasp(1, 1);
        assert_eq!(refusal(&b), Some(Refusal::Wide));
        b.properties[3] = nclx(1, 13, 6, true);
        assert_eq!(refusal(&b), Some(Refusal::High));
        let s = read(&b.build().0).unwrap();
        assert_eq!(s.refusal(Balanced, HEVC), None);
        // The words.
        let words = [
            (Refusal::Avif, AVIF),
            (Refusal::Codec(*b"unci"), CODEC),
            (Refusal::Deep, DEEP),
            (Refusal::Pictures, PICTURES),
            (Refusal::Pixels, PIXELS),
            (Refusal::Wide, WIDE),
            (Refusal::High, HIGH),
        ];
        for (refusal, why) in words {
            assert_eq!(refusal.why(), why);
        }
    }

    #[test]
    fn turns_are_read_along_the_picture() {
        for (props, turns, angle, axis) in [
            (vec![irot(0)], false, 0, None),
            (vec![irot(1)], true, 1, None),
            (vec![irot(2)], true, 2, None),
            (vec![irot(7)], true, 3, None),
            (vec![imir(1)], true, 0, Some(1)),
            (vec![imir(0), irot(3)], true, 3, Some(0)),
            // Samsung blanks its irot into a free box.
            (vec![bx(b"free", &[0])], false, 0, None),
        ] {
            let mut all = vec![hvcc(8, 8), ispe(64, 48)];
            all.extend(props.clone());
            let s = read(&primary_with(all).build().0).unwrap();
            assert_eq!(
                (s.turns(), s.irot, s.imir),
                (turns, angle, axis),
                "{props:?}"
            );
        }
        // An iden turned, or the picture it's made of.
        for (iden, base, turns, angle) in [
            (vec![2u16, 3], vec![1u16, 2], true, 1),
            (vec![2], vec![1, 2, 3], true, 0),
            (vec![2], vec![1, 2], false, 0),
            (vec![2, 4], vec![1, 2], false, 0),
        ] {
            let mut i = Item::new(1, *b"iden", Place::Nowhere, Vec::new());
            i.props = iden.iter().map(|&p| (p, false)).collect();
            let mut base_item = Item::picture(2, 0xB2, 20);
            base_item.hidden = true;
            base_item.props = base.iter().map(|&p| (p, p == 1)).collect();
            let mut b = Builder::new(vec![i, base_item], 1);
            b.properties = vec![hvcc(8, 8), ispe(64, 48), irot(1), irot(0)];
            b.refs = vec![(*b"dimg", 1, vec![2])];
            let s = read(&b.build().0).unwrap();
            assert_eq!(s.coded, [2]);
            assert_eq!((s.turns(), s.irot), (turns, angle), "{iden:?} {base:?}");
        }
    }

    #[test]
    fn color_is_read_from_the_picture_or_its_first_tile() {
        for ((p, t), srgb) in [
            ((1, 13), true),
            ((2, 2), true),
            ((1, 6), true),
            ((1, 1), true),
            // BT.601's primaries, 625 and 525 lines: SDR, close to sRGB.
            ((5, 1), true),
            ((6, 6), true),
            ((5, 13), true),
            ((12, 13), false),
            ((9, 16), false),
            ((9, 18), false),
            ((1, 16), false),
            ((5, 16), false),
            ((6, 18), false),
        ] {
            let s = read(
                &primary_with(vec![hvcc(8, 8), nclx(p, t, 6, true)])
                    .build()
                    .0,
            )
            .unwrap();
            let n = s.nclx.unwrap();
            assert_eq!(
                (n.primaries, n.transfer, n.matrix, n.full_range),
                (p, t, 6, true)
            );
            assert_eq!(n.srgb_like(), srgb, "{p} {t}");
            assert!(!s.icc);
            // With no profile, and a helper that hands back none for an
            // nclx, it's refused before the decode; one that hands back its
            // color space's (ImageIO) decides after it.
            let wide = (!srgb).then_some(Refusal::Wide);
            assert_eq!(s.refusal(Balanced, HEVC), wide, "{p} {t}");
            let mac = Bounds {
                nclx_profile: true,
                ..HEVC
            };
            assert_eq!(s.refusal(Balanced, mac), None, "{p} {t}");
        }
        // The item 6 samples: BT.601's 5/1/5 and 6/6/6 kept, P3's 12/13/6
        // and BT.2020 PQ's 9/16/9 refused.
        for ((p, t, m), refused) in [
            ((5, 1, 5), false),
            ((6, 6, 6), false),
            ((12, 13, 6), true),
            ((9, 16, 9), true),
        ] {
            let s = read(
                &primary_with(vec![hvcc(8, 8), nclx(p, t, m, false)])
                    .build()
                    .0,
            )
            .unwrap();
            let wide = refused.then_some(Refusal::Wide);
            assert_eq!(s.refusal(Small, HEVC), wide, "{p}/{t}/{m}");
            // An ICC profile beside it decides the color: kept.
            let props = vec![hvcc(8, 8), icc(b"prof", b"p"), nclx(p, t, m, false)];
            let s = read(&primary_with(props).build().0).unwrap();
            assert_eq!(s.refusal(Small, HEVC), None, "{p}/{t}/{m}");
        }
        for kind in [b"prof", b"rICC"] {
            let s = read(
                &primary_with(vec![hvcc(8, 8), icc(kind, b"profile")])
                    .build()
                    .0,
            )
            .unwrap();
            assert!(s.icc && s.nclx.is_none(), "{kind:?}");
        }
        let props = vec![hvcc(8, 8), icc(b"prof", b"p"), nclx(12, 13, 0, false)];
        let s = read(&primary_with(props).build().0).unwrap();
        assert!(s.icc);
        assert_eq!(
            s.nclx.map(|n| (n.primaries, n.full_range)),
            Some((12, false))
        );
        // The grid's own, or its first tile's.
        let props = vec![hvcc(8, 8), nclx(12, 13, 0, true), nclx(1, 13, 6, true)];
        let tiled = |grid_props| {
            read(
                &grid(props.clone(), grid_props, [vec![1, 2], vec![1, 3]])
                    .build()
                    .0,
            )
            .unwrap()
        };
        assert_eq!(tiled(vec![]).nclx.map(|n| n.primaries), Some(12));
        assert_eq!(tiled(vec![3]).nclx.map(|n| n.primaries), Some(1));
        // A clap only crops.
        let s = read(
            &primary_with(vec![hvcc(8, 8), ispe(64, 48), bx(b"clap", &[0; 32])])
                .build()
                .0,
        )
        .unwrap();
        assert!(s.clap);
        assert_eq!(s.upright(), Some((64, 48)));
    }

    #[test]
    fn every_item_that_describes_the_primary_is_carried_with_its_fields() {
        let mut b = simple();
        let mut text = Item::mime(12, "text/plain", b"a note");
        text.name = Some("Note");
        text.encoding = Some("gzip");
        let mut uri = Item::new(13, *b"uri ", Place::Mdat, b"uri payload".to_vec());
        uri.text = Some("urn:example:region");
        let iptc = Item::new(14, *b"iptc", Place::Idat, b"\x1c\x02\x5aBudapest".to_vec());
        let odd = Item::new(15, *b"E\xEAif", Place::Mdat, b"exif-ish".to_vec());
        let mut old = Item::new(16, *b"mime", Place::Mdat, b"<x:xmpmeta/>".to_vec());
        old.v1 = true;
        old.name = Some("XMP");
        old.text = Some("application/rdf+xml");
        // Not carried: what describes another picture, or nothing, and a
        // picture that says it describes the primary.
        let gain_xmp = Item::xmp(17, "<x:xmpmeta>gain</x:xmpmeta>");
        let loose = Item::exif(18, &portrait());
        let mut picture = Item::picture(19, 0xA9, 10);
        picture.hidden = true;
        let mut gain = Item::picture(GAIN, 0xD0, 10);
        gain.hidden = true;
        b.items
            .extend([text, uri, iptc, odd, old, gain_xmp, loose, picture, gain]);
        for id in [12, 13, 14, 15, 16, 19] {
            b.refs.push((*b"cdsc", id, vec![1]));
        }
        b.refs.push((*b"cdsc", 17, vec![GAIN]));
        b.refs.push((*b"auxl", GAIN, vec![1]));
        let (file, layout) = b.build();
        let s = scan(&file);
        assert_eq!(ids(&s), [EXIF, XMP, 12, 13, 14, 15, 16]);
        let c = |id| s.carried.iter().find(|c| c.id == id).unwrap();
        for id in ids(&s) {
            assert_eq!(c(id).data, layout.data(id), "{id}");
        }
        assert_eq!(
            (&c(12).kind, c(12).role, c(12).hidden),
            (b"mime", Role::Mime, true)
        );
        assert_eq!(
            (&c(12).name[..], &c(12).content_type[..]),
            (&b"Note"[..], &b"text/plain"[..])
        );
        assert_eq!(c(12).encoding, b"gzip");
        assert_eq!(
            (&c(13).kind, c(13).role, c(13).hidden),
            (b"uri ", Role::Uri, false)
        );
        assert_eq!(
            (&c(13).content_type[..], &c(13).encoding[..]),
            (&b"urn:example:region"[..], &b""[..])
        );
        assert_eq!((&c(14).kind, c(14).role), (b"iptc", Role::Iptc));
        assert_eq!((&c(15).kind, c(15).role), (b"E\xEAif", Role::Other));
        assert!(c(15).content_type.is_empty());
        assert_eq!((&c(16).kind, c(16).role), (b"mime", Role::Xmp));
        assert_eq!(
            (&c(16).name[..], &c(16).content_type[..]),
            (&b"XMP"[..], &b"application/rdf+xml"[..])
        );
        // The carried Exif is the first: Orientation 1, not the loose one's 6.
        assert_eq!(s.orientation, Some(1));
    }

    #[test]
    fn an_item_with_no_data_isnt_carried() {
        let mut b = simple();
        b.items
            .push(Item::new(12, *b"mime", Place::Nowhere, Vec::new()));
        b.items[3].text = Some("text/plain");
        b.refs.push((*b"cdsc", 12, vec![1]));
        let (file, _) = b.build();
        assert_eq!(ids(&read(&file).unwrap()), [EXIF, XMP, 12]);
        assert_eq!(ids(&scan(&file)), [EXIF, XMP]);
    }

    #[test]
    fn data_past_its_end_is_broken() {
        // The XMP is last in mdat.
        let (file, _) = simple().build();
        let broken = |file: &[u8]| matches!(read(file), Err(Error::Broken(_)));
        assert!(broken(&file[..file.len() - 1]));
        // Past idat's end.
        let mut b = simple();
        b.items[1].place = Place::Idat;
        let (mut file, _) = b.build();
        let at = entry_at(&file, EXIF);
        file[at + 12..at + 16].copy_from_slice(&10_000u32.to_be_bytes());
        assert!(broken(&file));
    }

    #[test]
    fn where_the_data_is_read_from() {
        let mut b = simple();
        b.after = vec![bx(b"free", b"after")];
        let (file, layout) = b.build();
        let at = entry_at(&file, XMP);
        // Another data reference is this file's, as libheif reads it.
        let mut other = file.clone();
        other[at + 4..at + 6].copy_from_slice(&1u16.to_be_bytes());
        assert_eq!(scan(&other).carried[1].data, layout.data(XMP));
        // An extent of length 0 runs to the end of the file.
        let mut to_end = file.clone();
        to_end[at + 12..at + 16].copy_from_slice(&0u32.to_be_bytes());
        let start = layout.file_ranges(XMP)[0].start as usize;
        assert_eq!(scan(&to_end).carried[1].data, &file[start..]);
        // Several extents with indices, and data in idat: `layouts`.
    }

    #[test]
    fn data_in_other_items_or_protected_is_refused_once_the_refusals_pass() {
        let (mut file, _) = simple().build();
        let at = entry_at(&file, XMP);
        file[at + 2..at + 4].copy_from_slice(&2u16.to_be_bytes());
        let mut s = read(&file).unwrap();
        assert_eq!(s.refusal(High, HEVC), None);
        let r = s.load(&mut Cursor::new(&file));
        assert!(
            matches!(r, Err(Error::Refused(ref why)) if why == LAYOUT),
            "{r:?}"
        );
        let mut b = simple();
        b.items[2].protection = 1;
        let r = loaded(&b.build().0);
        assert!(
            matches!(r, Err(Error::Refused(ref why)) if why == LAYOUT),
            "{r:?}"
        );
    }

    #[test]
    fn up_to_16_mib_of_metadata_is_carried() {
        let base = simple();
        let small: usize = base.items[1..].iter().map(|i| i.data.len()).sum();
        for (extra, ok) in [(0, true), (1, false)] {
            let mut b = base.clone();
            let n = MAX_CARRIED as usize - small + extra;
            b.items
                .push(Item::mime(12, "application/octet-stream", &vec![7; n]));
            b.refs.push((*b"cdsc", 12, vec![1]));
            let r = loaded(&b.build().0);
            match ok {
                true => assert_eq!(r.unwrap().carried[2].data.len(), n),
                false => assert!(matches!(r, Err(Error::TooLarge(_))), "{r:?}"),
            }
        }
    }

    #[test]
    fn the_first_exifs_orientation_is_read() {
        let orientation = |items: Vec<Item>, describe: Vec<u32>| {
            let mut b = simple();
            b.items.truncate(1);
            b.refs.clear();
            for id in describe {
                b.refs.push((*b"cdsc", id, vec![1]));
            }
            b.items.extend(items);
            scan(&b.build().0).orientation
        };
        assert_eq!(
            orientation(vec![Item::exif(EXIF, &portrait())], vec![EXIF]),
            Some(6)
        );
        // Bare, behind an offset of 0.
        let mut bare = Item::exif(EXIF, &[]);
        bare.data = [&0u32.to_be_bytes()[..], &portrait()].concat();
        assert_eq!(orientation(vec![bare], vec![EXIF]), Some(6));
        // An offset past its end; none.
        let mut past = Item::exif(EXIF, &portrait());
        past.data[..4].copy_from_slice(&1000u32.to_be_bytes());
        assert_eq!(orientation(vec![past], vec![EXIF]), None);
        assert_eq!(orientation(vec![], vec![]), None);
        // The first of two; one that describes nothing isn't read.
        let two = vec![Item::exif(12, &portrait()), Item::exif(EXIF, &camera())];
        assert_eq!(orientation(two.clone(), vec![12, EXIF]), Some(6));
        assert_eq!(orientation(two, vec![EXIF]), Some(1));
    }

    #[test]
    fn junk_and_cuts_never_panic() {
        let (file, _) = iphone().build();
        // Every byte of meta, and into mdat.
        let at = meta_at(&file);
        for i in at..(at + 2000).min(file.len()) {
            for v in [0, 1, 0x7F, 0xFF] {
                let mut f = file.clone();
                f[i] = v;
                if let Ok(mut s) = read(&f) {
                    let _ = s.refusal(High, HEVC);
                    let _ = s.load(&mut Cursor::new(&f));
                }
            }
        }
        for n in 0..file.len() {
            if let Ok(mut s) = read(&file[..n]) {
                let _ = s.load(&mut Cursor::new(&file[..n]));
            }
        }
    }

    /// A folder for one test's files, removed with it.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            let path = std::env::temp_dir().join(format!("grape-scan-unit-{}", unique_suffix()));
            fs::create_dir_all(path.join("work")).unwrap();
            TestDir(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// [`check`] on `file`, as `photo.heic`, at `preset`, with a helper
    /// started by `launch`: the source is unchanged and the helper's work
    /// folder is gone after.
    fn check_file(
        file: &[u8],
        preset: CompressPreset,
        launch: crate::helper::Launch,
        cancel: &CancelToken,
    ) -> Result<Scan, GrapeError> {
        let dir = TestDir::new();
        let src = dir.0.join("photo.heic");
        fs::write(&src, file).unwrap();
        let names = Names::new(
            &src,
            &dir.0.join("photo-1.heic"),
            Format::Heic,
            Format::Heic,
        );
        let limits = Limits {
            work_parent: dir.0.join("work"),
            ..Limits::default()
        };
        let encoder = Encoder::new(launch, limits, HeifFormat::Heic);
        let r = check(&src, &names, preset, &encoder, cancel);
        assert_eq!(fs::read(&src).unwrap(), file);
        assert_eq!(fs::read_dir(dir.0.join("work")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
        r
    }

    /// `check_file` with a new token, for its error.
    fn check_err(file: &[u8], preset: CompressPreset, launch: crate::helper::Launch) -> GrapeError {
        check_file(file, preset, launch, &CancelToken::new()).unwrap_err()
    }

    #[test]
    fn check_reads_the_file_and_asks_the_helper_last() {
        let (file, layout) = simple().build();
        let s = check_file(&file, High, fake("ok"), &CancelToken::new()).unwrap();
        assert_eq!(s.carried[1].data, layout.data(XMP));
        assert_eq!(s.orientation, Some(1));
        // A helper that can't write HEIC: D33's words.
        let e = check_err(&file, Balanced, fake("no-heic-encoder"));
        assert_eq!(e.kind, ErrorKind::NotYetSupported);
        assert!(e.user_message.contains("can't write HEIC files"), "{e:?}");
        // The helper is asked last: one that can't start is what a file
        // Compress takes hears, and a file it refuses never starts one.
        let e = check_err(&file, Small, nowhere());
        assert_eq!(e.user_message, "Grape's image helper couldn't start.");
        let deep = primary_with(vec![hvcc(10, 10), ispe(64, 48)]).build().0;
        let e = check_err(&deep, Small, nowhere());
        assert_eq!(e.kind, ErrorKind::NotYetSupported);
        assert_eq!(e.user_message, format!("photo.heic {DEEP}"));
    }

    #[test]
    fn check_says_why_in_grapes_words() {
        let refused = |file: &[u8], preset, why: &str| {
            let e = check_err(file, preset, nowhere());
            assert_eq!(e.kind, ErrorKind::NotYetSupported, "{e:?}");
            assert_eq!(e.user_message, format!("photo.heic {why}"));
        };
        // AVIF content named .heic, and an AV1 picture with no codec brand.
        let avif = fake_image(HeifFormat::Avif, 2, 2, 3, &[0; 12], None);
        refused(&avif, Balanced, AVIF);
        let mut b = simple();
        b.brands = b"mif1\0\0\0\0mif1".to_vec();
        b.items[0].kind = *b"av01";
        refused(&b.build().0, Balanced, AVIF);
        b.items[0].kind = *b"unci";
        refused(&b.build().0, Balanced, CODEC);
        let mut b = simple();
        b.after = vec![bx(b"moov", b"")];
        refused(&b.build().0, Balanced, SEQUENCE);
        let mut b = simple();
        b.items.push(Item::picture(5, 0xA5, 10));
        refused(&b.build().0, Balanced, PICTURES);
        refused(
            &primary_with(vec![hvcc(8, 8), pasp(4, 3)]).build().0,
            Balanced,
            PIXELS,
        );
        // P3 by its nclx alone: libheif hands back no profile, so the PC
        // refuses it before the helper starts; a Mac's ImageIO hands back
        // its color space's, so the helper is asked.
        let p3 = primary_with(vec![hvcc(8, 8), nclx(12, 13, 6, true)])
            .build()
            .0;
        if cfg!(target_os = "macos") {
            let e = check_err(&p3, Balanced, nowhere());
            assert_eq!(e.user_message, "Grape's image helper couldn't start.");
        } else {
            refused(&p3, Balanced, WIDE);
        }
        let big = primary_with(vec![hvcc(8, 8), ispe(8000, 6000)]).build().0;
        refused(&big, High, HIGH);
        let e = check_err(&big, Balanced, nowhere());
        assert_eq!(e.user_message, "Grape's image helper couldn't start.");
        let (mut file, _) = simple().build();
        let at = entry_at(&file, XMP);
        file[at + 2..at + 4].copy_from_slice(&2u16.to_be_bytes());
        refused(&file, Balanced, LAYOUT);
        // HEIF that names another codec: D33's words.
        let avci = bx(b"ftyp", b"avci\0\0\0\0mif1avci");
        let e = check_err(&avci, Balanced, nowhere());
        assert_eq!(e.kind, ErrorKind::DecodeFailed);
        assert_eq!(
            e.user_message,
            "photo.heic is a kind of HEIF image Grape can't read yet."
        );
        // Broken, and not HEIF at all.
        for file in [
            bx(b"ftyp", b"heic\0\0\0\0mif1heic"),
            b"\x89PNG\r\n\x1a\n".to_vec(),
        ] {
            let e = check_err(&file, Balanced, nowhere());
            assert_eq!(e.kind, ErrorKind::DecodeFailed);
            assert_eq!(e.user_message, "photo.heic isn't a readable HEIC image.");
        }
        // Over the metadata cap.
        let mut b = simple();
        b.items
            .push(Item::mime(12, "text/plain", &vec![7; MAX_CARRIED as usize]));
        b.refs.push((*b"cdsc", 12, vec![1]));
        let e = check_err(&b.build().0, Balanced, nowhere());
        assert_eq!(e.user_message, "photo.heic is too large to convert.");
    }

    #[test]
    fn check_reads_nothing_it_cant_and_stops_on_a_cancel() {
        let dir = TestDir::new();
        let src = dir.0.join("gone.heic");
        let names = Names::new(&src, &dir.0.join("gone-1.heic"), Format::Heic, Format::Heic);
        let encoder = Encoder::new(nowhere(), Limits::default(), HeifFormat::Heic);
        let e = check(&src, &names, Balanced, &encoder, &CancelToken::new()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::ReadFailed);
        assert_eq!(e.user_message, "Grape couldn't read gone.heic.");
        let cancel = CancelToken::new();
        cancel.cancel();
        let e = check_file(&simple().build().0, Balanced, fake("ok"), &cancel).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled);
    }
}
