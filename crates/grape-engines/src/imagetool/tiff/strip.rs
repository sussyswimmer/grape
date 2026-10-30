//! Remove Metadata on TIFF (D28): every page and SubIFD written back
//! compact ([`write`](super::write)), their image blocks byte for byte, in
//! the source's byte order and kind, with only the tags that describe the
//! picture.
//!
//! - **Kept:** the layout, color, resolution and page tags ([`kept`]), ICC
//!   and Orientation among them, each only in a type and count libtiff 4.5
//!   reads it in, and where it reads it ([`shape`], [`Walk::keeps`]);
//!   Photoshop's resources, filtered
//!   ([`irb`](super::irb)): a block that IFDs share is filtered once, and
//!   held and written once; with a block past [`MAX_IRB`], past that much
//!   of them kept in all, or more of them read than the file holds
//!   (blocks that overlap), the file is too large.
//! - **Dropped and reported:** every other tag ([`field`]), a kept one in
//!   another type or count (as other fields), and the Exif
//!   and GPS IFDs, walked as [`exif::classify`] does (one that pages share,
//!   once). Padding, and blank values of up to [`MAX_JUDGED`] bytes, go
//!   unsaid: a value that entries share is read once, and at most
//!   [`JUDGE_BUDGET`] in all, past which a value is reported unread.
//! - **Written back:** the DCF Adobe RGB Exif (ColorSpace uncalibrated,
//!   Interop `R03`), when that's how an IFD with no ICC profile said its
//!   color: each such IFD gets its own, even when they shared one.
//! - **Refused:** DNG, old-style JPEG and a Compression libtiff can't read
//!   (both tools); data of a Compression whose stream may hold metadata
//!   that Grape doesn't read ([`coded`]: JPEG 2000, JPEG XL, JPEG XR, PNG,
//!   and every code it doesn't know); metadata inside JPEG data (the codes
//!   that hold JPEG streams, as libtiff reads Compression; an APP1, APP13
//!   or COM in a block, or in a JPEGTables, any IFD's, read as libtiff
//!   reads it ([`tables`]): up to its first scan when that's a sequential
//!   frame's only one, and through every scan otherwise), and JPEG data
//!   Grape can't check, whose segments or scans run past the first
//!   [`MAX_SCAN`] bytes read before that, that's hierarchical, or
//!   JPEGTables libtiff can't read as bytes, or that are more than tables
//!   ([`only_tables`]); metadata inside WebP data (an
//!   EXIF or XMP chunk, [`chunks`]), and WebP data Grape can't check: not
//!   a RIFF container that covers its block, nor a bare bitstream, or
//!   whose chunks run on past the first [`MAX_CHUNKS`]; ImageJ and OME
//!   files, whose layout is in their description (any of an IFD's); a
//!   tag it keeps listed twice with different values ([`twice_kept`]);
//!   and a tag a page's layout or samples are read from, or a palette
//!   page's ColorMap, in a type or count libtiff doesn't read
//!   ([`LAID_OUT`]), which can't go as other fields.
//! - **Checked:** [`Plan::check`] reads the output again before it's
//!   published.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::super::exif;
use super::super::strip::{read_at, Error, Found, In, Out};
use super::irb;
use super::read::{
    integer, number, per_sample, read_value, samples, Alike, Block, Entry, Ifd, Layout, Meta,
    Order, Tiff, Value, SHORT, TWICE, UNREAD_LAYOUT,
};
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

/// The most descriptions out of line remembered as read for ImageJ's and
/// OME's, so that one that IFDs share is read once: past them, one is read
/// again each time, from what the file holds.
const MAX_DESCRIPTIONS: usize = 4_096;

/// The most bytes of Photoshop's resources the plan holds, filtered, each
/// block once however many IFDs name it; and the longest block it reads.
const MAX_IRB: u64 = 16 << 20;

/// How much of each JPEG block (and of its tables) is read for segments.
const MAX_SCAN: u64 = 64 * 1024;

/// The most chunks of a WebP block read for EXIF and XMP: past them, what
/// follows can't be seen.
const MAX_CHUNKS: usize = 64;

/// Why a file with metadata inside its JPEG data is refused.
const IN_DATA: &str = "has metadata inside its image data, which Grape can't remove yet.";

/// Why a file whose `kind` of data (JPEG, WebP, JPEG 2000, …, or "image"
/// for a Compression Grape doesn't know) runs on past what's read of it,
/// or isn't read at all, is refused.
fn unchecked(kind: &str) -> String {
    format!("has {kind} data Grape can't check for metadata yet.")
}

/// What Remove Metadata makes of the data of Compression `n`, an allowlist:
/// `Ok(None)` for a stream that holds nothing a reader reads but the
/// picture, copied as it is; `Ok(Some(kind))` for one Grape reads for
/// metadata; `Err(kind)` for any other, whose kind of data, where it's
/// known, the refusal names ([`unchecked`]).
///
/// - **Nothing but the picture:** None (1); CCITT's run codes (2, 3, 4 and
///   32771, T.4 and T.6, which hold only runs and EOLs); LZW (5); Deflate
///   (8, 32946, and PixTIFF's 50013, which tifffile reads as Deflate: a
///   zlib stream, whose header holds no name or comment, as gzip's does);
///   PackBits (32773), NeXT (32766) and ThunderScan (32809), runs and codes
///   of the samples; PixarLog (32909, zlib of the samples); SGILog (34676,
///   34677, runs of LogLuv samples); LERC (34887, a Lerc2 blob of the
///   samples' sizes, masks and values, in Deflate or ZSTD as its
///   parameters say); LZMA (34925, an .xz stream, whose headers hold only
///   flags, sizes and checks); ZSTD (50000, and GDAL's first code, 34926)
///   and JBIG (34661): a ZSTD stream's skippable frames and a JBIG
///   stream's COMMENT segments are skipped by their decoders, which no
///   reader shows, as bytes past any stream's end are.
/// - **Read:** JPEG streams ([`Kind::Jpeg`]): JPEG (7), DNG's lossy JPEG
///   (34892, in a file that isn't a DNG, which is refused), Bio-Formats'
///   (33007, which tifffile reads as JPEG) and Leaf's lossless JPEG (99);
///   WebP ([`Kind::Webp`]): 50001, and GDAL's first code, 34927.
/// - **Refused:** JPEG 2000 (Aperio's 33003 to 33005, and 34712), JPEG XL
///   (GDAL's 50002, and 52546 as DNG 1.7 numbers it), JPEG XR (34934, and
///   NDPI's 22610) and PNG (34933), whose boxes and chunks may hold Exif,
///   XMP and text; and every other code, camera raws among them, as
///   "image" data.
fn coded(n: u64) -> Result<Option<Kind>, &'static str> {
    match n {
        1..=5
        | 8
        | 32766
        | 32771
        | 32773
        | 32809
        | 32909
        | 32946
        | 34661
        | 34676
        | 34677
        | 34887
        | 34925
        | 34926
        | 50000
        | 50013 => Ok(None),
        7 | 99 | 33007 | 34892 => Ok(Some(Kind::Jpeg)),
        50001 | 34927 => Ok(Some(Kind::Webp)),
        33003..=33005 | 34712 => Err("JPEG 2000"),
        50002 | 52546 => Err("JPEG XL"),
        22610 | 34934 => Err("JPEG XR"),
        34933 => Err("PNG"),
        _ => Err("image"),
    }
}

/// Why an ImageJ or OME file is refused.
const DESCRIBED: &str = "keeps its layout in its description, so Remove Metadata would break it.";

/// Whether `tag` is kept: the picture's layout, color, resolution and
/// pages, with ICC and Orientation (D28's whitelist), and every tag a
/// codec libtiff 4.5 has reads to decode (`tif_dirinfo.c` and each codec's
/// own fields): Predictor, the fax options, JPEG's tables and LERC's
/// parameters, which say whether Deflate or ZSTD is inside it. Each only
/// as libtiff reads it ([`shape`]).
fn kept(tag: u16) -> bool {
    shape(tag).is_some()
}

/// The types and counts libtiff 4.5.1 reads a tag Remove Metadata keeps in
/// (`TIFFReadDirectory` and `TIFFFetchNormalTag` in `tif_dirread.c`, by
/// each field's `set_field_type` in `tif_dirinfo.c` and the codecs'), which
/// is all it keeps of it: an entry of another type or count, which
/// libtiff ignores ("Incorrect value", "incorrect count … tag ignored") or
/// fails on and other readers may show (Pillow shows an ASCII Orientation
/// as its text), goes, as other fields ([`Walk::keeps`]). Values aren't
/// checked: a number out of its range is a number still.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Integers of any type but IFD and IFD8 ([`integer`]), as
    /// `TIFFReadDirEntryShort`, `…Long` and their arrays read them.
    Ints(Count),
    /// Numbers of any type libtiff reads as floating point
    /// (`TIFFReadDirEntryFloat`, `…FloatArray`, `…DoubleArray`): the
    /// integers, RATIONAL, SRATIONAL, FLOAT and DOUBLE.
    Reals(Count),
    /// Bytes as `TIFFReadDirEntryByteArray` reads them: BYTE, ASCII,
    /// UNDEFINED and the other integer types.
    Bytes(Count),
    /// Text: ASCII alone.
    Text(Count),
    /// GDAL's no-data value, which libtiff doesn't know: ASCII, as GDAL
    /// writes it, that reads as a number ([`numeric`]).
    Number,
}

/// How many values a [`Shape`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Count {
    /// This many.
    Is(u64),
    /// From the first to the second, both included.
    Within(u64, u64),
    /// One, or at least one for each sample (SamplesPerPixel), as
    /// `TIFFReadDirEntryPersampleShort` reads them.
    PerSample,
    /// One for each sample.
    Samples,
    /// At most one for each sample, none included, as `setExtraSamples`
    /// takes them.
    UpToSamples,
    /// Three for each value a sample can take, 2^BitsPerSample, or one
    /// for each too when it says so: as `TIFFReadDirectory` reads ColorMap
    /// and TransferFunction, of a BitsPerSample of at most 24 listed
    /// before them ([`Walk::bits`]), and ignores them otherwise.
    Table(bool),
}

/// The [`Shape`] of a tag Remove Metadata keeps, or `None` for one it
/// drops. A tag libtiff doesn't know (TransferRange, GDAL's no-data value)
/// takes the type TIFF 6 or GDAL gives it.
fn shape(tag: u16) -> Option<Shape> {
    use Count::*;
    use Shape::*;
    // libtiff's arrays of 2^32 − 1 values at most, or 65,535.
    let (c32, c16) = (u64::from(u32::MAX), u64::from(u16::MAX));
    Some(match tag {
        // One number: NewSubfileType, the size, PhotometricInterpretation,
        // Threshholding, CellWidth, CellLength, FillOrder, Orientation,
        // SamplesPerPixel, RowsPerStrip, PlanarConfiguration, the fax
        // options, ResolutionUnit, Predictor, the tile size, InkSet,
        // NumberOfInks, YCbCrPositioning, Matteing, ImageDepth and
        // TileDepth. SubfileType, GrayResponseUnit and GrayResponseCurve
        // libtiff never reads (`FIELD_IGNORE`): they go.
        254
        | 256
        | 257
        | 262..=266
        | 274
        | 277
        | 278
        | 284
        | 292
        | 293
        | 296
        | 317
        | 322
        | 323
        | 332
        | 334
        | 531
        | 32995
        | 32997
        | 32998 => Ints(Is(1)),
        // One, or one for each sample: BitsPerSample, Compression,
        // MinSampleValue, MaxSampleValue, SampleFormat and DataType.
        258 | 259 | 280 | 281 | 339 | 32996 => Ints(PerSample),
        // Two: PageNumber, HalftoneHints, DotRange, YCbCrSubsampling and
        // CFARepeatPatternDim; TransferRange's six.
        297 | 321 | 336 | 530 | 33421 => Ints(Is(2)),
        342 => Ints(Is(6)),
        // Tables: TransferFunction and ColorMap; ExtraSamples;
        // LercParameters, its version and what's inside at least.
        301 => Ints(Table(true)),
        320 => Ints(Table(false)),
        338 => Ints(UpToSamples),
        50674 => Ints(Within(2, c32)),
        // The resolution and position; WhitePoint, the primaries, the
        // YCbCr coefficients and ReferenceBlackWhite; SMinSampleValue and
        // SMaxSampleValue, one for each sample.
        282 | 283 | 286 | 287 => Reals(Is(1)),
        318 => Reals(Is(2)),
        529 => Reals(Is(3)),
        319 | 532 => Reals(Is(6)),
        340 | 341 => Reals(Samples),
        // JPEGTables and ICC; CFAPattern.
        347 | 34675 => Bytes(Within(1, c32)),
        33422 => Bytes(Within(1, c16)),
        // InkNames: the names of a separated page's inks, text by design.
        333 => Text(Within(1, c16)),
        42113 => Number,
        _ => return None,
    })
}

/// Whether GDAL's no-data value `text` reads as a number, as GDAL writes
/// it (`%.18g`, `nan`, `inf`): spaces around it allowed, NULs only after
/// it, nothing else.
fn numeric(text: &[u8]) -> bool {
    let end = text.iter().rposition(|&b| b != 0).map_or(0, |at| at + 1);
    let text = &text[..end];
    !text.contains(&0) && std::str::from_utf8(text).is_ok_and(|t| t.trim().parse::<f64>().is_ok())
}

/// The longest GDAL no-data value read ([`numeric`]): a number's text, of
/// any precision, is shorter.
const MAX_NUMBER: u64 = 64;

/// Whether `ifd` lists a tag Remove Metadata keeps (Photoshop's resources
/// among them), or the Exif ColorSpace or Interop index that decide the
/// DCF Exif, twice or more, its entries not all the same value
/// ([`Alike::twice`]): the output would hold only the first, which readers
/// that take the last (Pillow; libtiff in an Exif IFD) don't read in the
/// source.
fn twice_kept(r: &mut dyn In, ifd: &Ifd, order: Order, alike: &mut Alike) -> Result<bool, Error> {
    let exif = ifd.exif.as_ref();
    let interop = exif.and_then(|exif| exif.interop.as_deref());
    let mut lists = vec![(&ifd.entries, 0)];
    lists.extend(exif.map(|exif| (&exif.entries, 0xA001)));
    lists.extend(interop.map(|interop| (&interop.entries, 1)));
    for (entries, decides) in lists {
        let picks = |tag| match decides {
            0 => kept(tag) || tag == 34377,
            decides => tag == decides,
        };
        if alike.twice(r, entries, order, picks)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The tags Remove Metadata keeps that say how a page's data is laid out
/// or what its samples are: those either TIFF tool decides on to read its
/// blocks but the ones the walk read already (Compression, the blocks and
/// the SubIFDs), and ExtraSamples. An entry of one that libtiff can't read
/// ([`shape`]; it fails on most, and ignores FillOrder, Predictor and
/// YCbCrSubsampling) can't go as other fields: readers that read it
/// (Pillow reads a SampleFormat of FLOAT 3.0 as float samples) would read
/// the output as another picture. The file is refused instead
/// ([`UNREAD_LAYOUT`]), and so for a palette page's ColorMap
/// ([`Walk::lays_out`]).
const LAID_OUT: [u16; 16] = [
    256, 257, 258, 262, 266, 277, 278, 284, 317, 322, 323, 338, 339, 530, 32997, 32998,
];

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
        // A description that IFDs share is read once, while it's
        // remembered ([`MAX_DESCRIPTIONS`]); one in its entry isn't.
        let mut descriptions = HashSet::new();
        let mut alike = Alike::new(len);
        for ifd in tiff.ifds() {
            // Every one: tifffile writes a description it's given and then
            // its own (the picture's shape, or ImageJ's alone in an ImageJ
            // file), and reads ImageJ's from either of a page's first two.
            for entry in ifd.entries.iter().filter(|e| e.tag == 270) {
                let at = matches!(entry.value, Value::At { .. });
                if at && descriptions.contains(&entry.value) {
                    continue;
                }
                let text = look(r, entry, MAX_JUDGED, &mut left)?.map(|(text, _)| text);
                let text = text.unwrap_or_default();
                if text.starts_with(b"ImageJ=") || text.windows(4).any(|w| w == b"<OME") {
                    return Err(Error::Refused(DESCRIBED.into()));
                }
                if at && descriptions.len() < MAX_DESCRIPTIONS {
                    descriptions.insert(entry.value);
                }
            }
            if twice_kept(r, ifd, order, &mut alike)? {
                return Err(Error::Refused(TWICE.into()));
            }
        }
        if let Some(kind) = tiff
            .ifds()
            .iter()
            .find_map(|ifd| coded(ifd.compression).err())
        {
            return Err(Error::Refused(unchecked(kind)));
        }
        // JPEG and WebP data, and JPEGTables in any IFD: libtiff hands any
        // IFD's to whoever asks for them. Tables of no bytes are none.
        let scanned: Vec<&Ifd> = tiff
            .ifds()
            .into_iter()
            .filter(|ifd| matches!(coded(ifd.compression), Ok(Some(_))) || tables_of(ifd).is_some())
            .collect();
        if !scanned.is_empty() {
            let shared = tiff.shared();
            let mut scan = Scan {
                order,
                shared: &shared,
                scanned: vec![0; shared.len()],
                tables: HashSet::new(),
                left,
            };
            for ifd in scanned {
                match scan.seen(r, ifd)? {
                    (Seen::Clean, _) => {}
                    (Seen::Metadata, _) => return Err(Error::Refused(IN_DATA.into())),
                    (Seen::Unseen, kind) => return Err(Error::Refused(unchecked(kind))),
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
            bits_left: len,
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
            bits_left: u64::MAX,
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

/// The kinds of image data Remove Metadata reads for metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Read as [`segments`] reads it.
    Jpeg,
    /// Read as [`chunks`] reads it.
    Webp,
}

impl Kind {
    /// Its name, for the refusal.
    fn name(self) -> &'static str {
        match self {
            Kind::Jpeg => "JPEG",
            Kind::Webp => "WebP",
        }
    }

    /// Its bit in [`Scan::scanned`].
    fn bit(self) -> u8 {
        match self {
            Kind::Jpeg => 1,
            Kind::Webp => 2,
        }
    }
}

/// The scan of JPEG and WebP data for metadata, which reads a block, or a
/// JPEGTables value, once however many times IFDs list it: a block once
/// as each kind of data IFDs list it as. One that held metadata, or
/// couldn't be seen through, ended the scan.
struct Scan<'s> {
    /// The file's byte order, which JPEGTables are read in.
    order: Order,
    /// The blocks listed more than once, sorted ([`Tiff::shared`]).
    shared: &'s [Block],
    /// As which kinds it has read each of them: a bit for each
    /// ([`Kind::bit`]).
    scanned: Vec<u8>,
    /// The JPEGTables values it has read, by where they are and their type.
    tables: HashSet<(Value, u16)>,
    /// What's left of the bytes it may read of JPEGTables values ([`look`]).
    left: u64,
}

impl Scan<'_> {
    /// What the JPEG or WebP data of `ifd` holds: its JPEGTables, as
    /// libtiff reads them ([`tables`]), as [`segments`] reads them; then
    /// each of its blocks, as the [`Kind`] of its Compression. The first
    /// that isn't clean, with the kind of data it is; JPEGTables libtiff
    /// can't read as bytes can't be seen through.
    fn seen(&mut self, r: &mut dyn In, ifd: &Ifd) -> Result<(Seen, &'static str), Error> {
        let fresh = |e: &&Entry| self.tables.insert((e.value, e.kind));
        if let Some(entry) = tables_of(ifd).filter(fresh) {
            let Some((tables, whole)) = tables(r, entry, self.order, MAX_SCAN, &mut self.left)?
            else {
                return Ok((Seen::Unseen, "JPEG"));
            };
            // Metadata among them is said to be; anything but tables
            // (what follows their EOI, say) can't be seen through.
            let seen = match segments(&tables, whole) {
                Seen::Clean if !only_tables(&tables, whole) => Seen::Unseen,
                seen => seen,
            };
            if seen != Seen::Clean {
                return Ok((seen, "JPEG"));
            }
        }
        let Ok(Some(kind)) = coded(ifd.compression) else {
            return Ok((Seen::Clean, "JPEG"));
        };
        let mut buf = Vec::new();
        for block in ifd.blocks.iter().filter(|b| b.len > 0) {
            // A block IFDs share is read once as each kind they list it as.
            let shared = self.shared.binary_search(block).ok();
            if let Some(scanned) = shared.and_then(|n| self.scanned.get_mut(n)) {
                if *scanned & kind.bit() != 0 {
                    continue;
                }
                *scanned |= kind.bit();
            }
            let seen = match kind {
                Kind::Webp => chunks(r, *block)?,
                Kind::Jpeg => {
                    buf.resize(block.len.min(MAX_SCAN) as usize, 0);
                    read_at(r, block.offset, &mut buf)?;
                    segments(&buf, block.len <= MAX_SCAN)
                }
            };
            if seen != Seen::Clean {
                return Ok((seen, kind.name()));
            }
        }
        Ok((Seen::Clean, kind.name()))
    }
}

/// What the WebP data at `block` holds ([`Seen`]), as libwebp reads it.
/// Grape checks two forms, which libtiff's writer and libwebp's make:
///
/// - **A RIFF container** that covers the block, or all of it but a last
///   byte (a pad): its chunks are read by their headers, one after the
///   other to its end, at most [`MAX_CHUNKS`] of them; an EXIF or XMP
///   chunk is metadata, and one that runs past the container's end, or
///   chunks past [`MAX_CHUNKS`], can't be seen through.
/// - **A bare VP8 or VP8L bitstream** ([`bare`]), which libwebp reads too,
///   and which has no chunks.
///
/// Anything else can't be seen through: a container that ends before the
/// block does, with chunks after it, or chunks with no RIFF header, which
/// libwebp walks too when the first is an ALPH. The headers are read from
/// one read of the block's first [`MAX_SCAN`] bytes, and one past them
/// from a read of the [`HOP`] bytes it starts ([`Window`]).
fn chunks(r: &mut dyn In, block: Block) -> Result<Seen, Error> {
    let le = |b: &[u8]| u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let mut window = Window::new(r, block)?;
    let head = window.at(r, 0, 12)?;
    if head.len() < 12 || &head[..4] != b"RIFF" || &head[8..] != b"WEBP" {
        let bare = bare(window.at(r, 0, 10)?);
        return Ok(if bare { Seen::Clean } else { Seen::Unseen });
    }
    let end = 8 + le(&head[4..8]);
    if end != block.len && end + 1 != block.len {
        return Ok(Seen::Unseen);
    }
    let mut at = 12;
    for _ in 0..MAX_CHUNKS {
        if at >= end {
            return Ok(Seen::Clean);
        }
        if at + 8 > end {
            return Ok(Seen::Unseen);
        }
        let chunk = window.at(r, at, 8)?;
        if matches!(&chunk[..4], b"EXIF" | b"XMP ") {
            return Ok(Seen::Metadata);
        }
        let size = le(&chunk[4..]);
        if at + 8 + size > end {
            return Ok(Seen::Unseen);
        }
        // A pad byte after an odd chunk, which the last may leave out.
        at += 8 + size + (size & 1);
    }
    Ok(if at >= end { Seen::Clean } else { Seen::Unseen })
}

/// Whether `data` starts a bare VP8L bitstream (its signature, 0x2F, and
/// version 0) or a bare VP8 key frame (its frame tag's key-frame bit clear,
/// then its start code), as libwebp tells them (`VP8LCheckSignature`,
/// `VP8GetInfo`).
fn bare(data: &[u8]) -> bool {
    let vp8l = data.len() >= 5 && data[0] == 0x2F && data[4] >> 5 == 0;
    let vp8 = data.len() >= 10 && data[0] & 1 == 0 && data[3..6] == [0x9D, 0x01, 0x2A];
    vp8l || vp8
}

/// How much of a block is read at a header past what's been read of it.
const HOP: u64 = 4 * 1024;

/// What's been read of a block whose headers are walked: its first
/// [`MAX_SCAN`] bytes, then [`HOP`] bytes from a header past them, so that
/// headers close together cost one read.
struct Window {
    block: Block,
    /// Where in the block what's read starts.
    from: u64,
    bytes: Vec<u8>,
}

impl Window {
    /// The first [`MAX_SCAN`] bytes of `block`, read.
    fn new(r: &mut dyn In, block: Block) -> Result<Window, Error> {
        let mut bytes = vec![0; block.len.min(MAX_SCAN) as usize];
        read_at(r, block.offset, &mut bytes)?;
        Ok(Window {
            block,
            from: 0,
            bytes,
        })
    }

    /// The `n` bytes at `at` in the block, or as many as it holds: from
    /// what's read, or from [`HOP`] bytes read from `at` when they aren't
    /// all in it.
    fn at(&mut self, r: &mut dyn In, at: u64, n: usize) -> Result<&[u8], Error> {
        let len = self.block.len;
        let want = (n as u64).min(len.saturating_sub(at));
        let end = self.from + self.bytes.len() as u64;
        if at < self.from || at + want > end {
            self.bytes
                .resize(HOP.max(want).min(len.saturating_sub(at)) as usize, 0);
            read_at(r, self.block.offset + at, &mut self.bytes)?;
            self.from = at;
        }
        let start = (at - self.from) as usize;
        Ok(&self.bytes[start..start + want as usize])
    }
}

/// `ifd`'s JPEGTables, unless they hold no bytes: libtiff ignores those
/// (`JPEGVSetField` refuses a count of 0), as if they weren't there, and
/// Remove Metadata drops them unsaid ([`shape`]).
fn tables_of(ifd: &Ifd) -> Option<&Entry> {
    ifd.get(347).filter(|e| e.count > 0)
}

/// The first `max` bytes of the JPEGTables `entry` holds as libtiff 4.5
/// reads them (`TIFFReadDirEntryByteArray`), and whether that's all of
/// them: of ASCII, UNDEFINED or BYTE as they are; of SBYTE, SHORT, SSHORT,
/// LONG, SLONG, LONG8 or SLONG8 a byte for each value, each from 0 to 255.
/// `None` where libtiff can't read them: another type, more than 2^32 - 1
/// values, or a value of those read past a byte or negative. What's read
/// is taken from `left`, as [`look`] takes it.
fn tables(
    r: &mut dyn In,
    entry: &Entry,
    order: Order,
    max: u64,
    left: &mut u64,
) -> Result<Option<(Vec<u8>, bool)>, Error> {
    let (size, signed) = match entry.kind {
        2 | 7 => (1, false),
        kind => match integer(kind) {
            Some(each) => each,
            None => return Ok(None),
        },
    };
    if entry.count > u64::from(u32::MAX) {
        return Ok(None);
    }
    let n = entry.count.min(max);
    let bytes = match entry.value {
        Value::Inline { bytes, len } => bytes[..usize::from(len)].to_vec(),
        Value::At { offset, .. } => {
            let want = n * size as u64;
            *left = left.checked_sub(want).ok_or_else(|| {
                Error::TooLarge(
                    "descriptions or JPEG tables that overlap, more to read than it holds".into(),
                )
            })?;
            let mut bytes = vec![0; want as usize];
            read_at(r, offset, &mut bytes)?;
            bytes
        }
        Value::Unread => return Ok(None),
    };
    let mut each = Vec::with_capacity(bytes.len() / size);
    for b in bytes.chunks_exact(size) {
        let v = order.uint(b);
        if (signed && v >> (8 * size - 1) == 1) || v > 0xFF {
            return Ok(None);
        }
        each.push(v as u8);
    }
    Ok(Some((each, n == entry.count)))
}

/// Whether JPEGTables `tables` (all of them, if `whole`) are tables alone,
/// as libtiff's writer makes them (`jpeg_write_tables`) and libjpeg reads
/// them, up to their EOI, as abbreviated tables: SOI, then DQT, DHT, DAC
/// and DRI segments, each after any fill bytes, then EOI, their last
/// bytes. What follows an EOI, comes before the SOI or isn't a table, no
/// reader reads, so it can't be checked as one would.
fn only_tables(tables: &[u8], whole: bool) -> bool {
    let Some(mut rest) = tables.strip_prefix(&[0xFF, 0xD8]) else {
        return false;
    };
    loop {
        let fill = rest.iter().take_while(|&&b| b == 0xFF).count();
        let Some(&marker) = rest.get(fill).filter(|_| fill > 0) else {
            return false;
        };
        rest = &rest[fill + 1..];
        match marker {
            0xD9 => return whole && rest.is_empty(),
            0xDB | 0xC4 | 0xCC | 0xDD => {
                let Some(len) = rest.get(..2) else {
                    return false;
                };
                let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
                match rest.get(len..) {
                    Some(next) if len >= 2 => rest = next,
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
}

/// What JPEG or WebP data holds, as far as it's read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// No APP1, APP13 or COM segment (no EXIF or XMP chunk), or not
    /// segments (chunks) Grape knows, which are copied as they are.
    Clean,
    /// An APP1, APP13 or COM segment, or an EXIF or XMP chunk.
    Metadata,
    /// What Grape can't see through: segments or scans that run on past
    /// what was read, a hierarchical JPEG (DHP), whose frames after the
    /// first it doesn't read, or WebP chunks past [`MAX_CHUNKS`].
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
    /// What's left of the bytes of BitsPerSample it may read out of line
    /// to size ColorMaps and TransferFunctions ([`Walk::bits`]): what the
    /// file holds, which only pages that all point at one array come to
    /// more than.
    bits_left: u64,
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
                    if first && self.keeps(&ifd, entry)? {
                        let field = Field::copy(entry)
                            .ok_or_else(|| Error::Broken(format!("tag {tag} can't be read")))?;
                        fields.push(field);
                    } else if first {
                        // Not as libtiff reads it: other readers may show
                        // what it holds, or read the picture by it.
                        if self.lays_out(&ifd, tag)? {
                            return Err(Error::Refused(UNREAD_LAYOUT.into()));
                        }
                        self.report(MetaField::Other, entry)?;
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

    /// Whether the entry `entry` of `ifd`, for a tag Remove Metadata keeps,
    /// is kept: in a type and count libtiff 4.5 reads it in ([`shape`]),
    /// and where libtiff reads it (JPEGTables on JPEG data, which tifffile
    /// reads too, LercParameters on LERC data, as `TIFFReadDirectory`
    /// ignores a codec's tags on another's; InkNames on a separated page,
    /// PhotometricInterpretation 5, whose inks they name).
    fn keeps(&mut self, ifd: &Ifd, entry: &Entry) -> Result<bool, Error> {
        let Some(shape) = shape(entry.tag) else {
            return Ok(false);
        };
        let here = match entry.tag {
            347 => matches!(coded(ifd.compression), Ok(Some(Kind::Jpeg))),
            50674 => ifd.compression == 34887,
            333 => match ifd.get(262) {
                Some(photometric) => number(self.r, photometric, self.order, SHORT)? == Some(5),
                None => false,
            },
            _ => true,
        };
        let (count, of) = match shape {
            Shape::Ints(count) => (count, integer(entry.kind).is_some()),
            Shape::Reals(count) => {
                let real = matches!(entry.kind, 5 | 10 | 11 | 12);
                (count, real || integer(entry.kind).is_some())
            }
            Shape::Bytes(count) => {
                let bytes = matches!(entry.kind, 2 | 7);
                (count, bytes || integer(entry.kind).is_some())
            }
            Shape::Text(count) => (count, entry.kind == 2),
            Shape::Number => {
                let text = match entry.kind {
                    2 => head(self.r, entry, MAX_NUMBER)?,
                    _ => None,
                };
                let number = text.is_some_and(|(text, whole)| whole && numeric(&text));
                return Ok(here && number);
            }
        };
        if !here || !of {
            return Ok(false);
        }
        let n = entry.count;
        Ok(match count {
            Count::Is(want) => n == want,
            Count::Within(least, most) => (least..=most).contains(&n),
            Count::PerSample => n == 1 || samples(self.r, ifd, self.order)?.is_some_and(|s| n >= s),
            Count::Samples => samples(self.r, ifd, self.order)? == Some(n),
            Count::UpToSamples => samples(self.r, ifd, self.order)?.is_some_and(|s| n <= s),
            Count::Table(once) => match self.bits(ifd, entry.tag)? {
                Some(bits) => n == 3 << bits || (once && n == 1 << bits),
                None => false,
            },
        })
    }

    /// BitsPerSample as libtiff reads it ([`per_sample`]) by the time it
    /// reads `ifd`'s first entry for `tag`, when that's at most 24: `None`
    /// where it isn't listed before it ("Ignoring … since BitsPerSample tag
    /// not found"), is more ("Ignoring … because BitsPerSample=…>24"), or
    /// can't be read, as libtiff can't load the page then. What's read out
    /// of line is taken from what the walk may read of them.
    fn bits(&mut self, ifd: &Ifd, tag: u16) -> Result<Option<u64>, Error> {
        let first = |tag| ifd.entries.iter().position(|e| e.tag == tag);
        let (Some(bits), Some(at)) = (first(258), first(tag)) else {
            return Ok(None);
        };
        if bits > at {
            return Ok(None);
        }
        let samples = samples(self.r, ifd, self.order)?;
        let entry = &ifd.entries[bits];
        let bits = per_sample(self.r, entry, self.order, samples, &mut self.bits_left)?;
        Ok(bits.filter(|&bits| bits <= 24))
    }

    /// Whether the tag `tag` of `ifd` says how the page's data is laid out
    /// or what its samples are ([`LAID_OUT`]; a palette page's ColorMap),
    /// so that an entry of it that isn't kept ([`Walk::keeps`]) can't go.
    fn lays_out(&mut self, ifd: &Ifd, tag: u16) -> Result<bool, Error> {
        Ok(LAID_OUT.contains(&tag)
            || (tag == 320
                && match ifd.get(262) {
                    Some(photometric) => number(self.r, photometric, self.order, SHORT)? == Some(3),
                    None => false,
                }))
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
    /// way: ColorSpace uncalibrated, and Interop `R03`. ColorSpace is read
    /// as libtiff 4.5.1 reads an Exif IFD's (`TIFFReadCustomDirectory`): a
    /// SHORT, another type ignored ("Wrong data type"), and of more than
    /// one value the first ("tag trimmed"), as Pillow reads the first of
    /// them, when they're in the entry; of more than it holds, libtiff
    /// reads the entry's offset as the value, so such a one says nothing.
    /// Its entries all agree, or the file was refused ([`twice_kept`]).
    fn adobe_rgb(&mut self, exif: &Meta) -> Result<bool, Error> {
        let uncalibrated = exif
            .entries
            .iter()
            .find(|e| e.tag == 0xA001)
            .is_some_and(|e| {
                matches!((e.kind, e.value), (3, Value::Inline { bytes, len })
                if len >= 2 && self.order.uint(&bytes[..2]) == 0xFFFF)
            });
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
    use super::super::read::MISSIZED;
    use super::super::tests::{
        gray, miscounted, picture, set_block, set_entry, shared_meta, Builder, Page, B, V,
    };
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
            res(2000, "Path 1", &[7; 22_000]),
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
    fn photoshop_paths_named_longer_than_they_were_are_written() {
        // A review's file: a 1 × 1 page whose resources hold a resolution,
        // 40 paths named a to n, and a clipping path naming a. Each path's
        // new name is longer than its old, so what's kept outgrew the
        // source, and the writer took the file for one made to grow ("too
        // large"): only the bytes it copies from the source count now.
        let names = b"abcdefghijklmn";
        let mut irb = res(1005, "", &[0, 72, 0, 0, 0, 1, 0, 1]);
        for n in 0..40 {
            let name = char::from(names[n % names.len()]).to_string();
            irb.extend(res(2000 + n as u16, &name, &[7; 26]));
        }
        irb.extend(res(2999, "", b"\x01a\0\0"));
        let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
        page.tags.push((34377, V::Bytes(7, irb.clone())));
        let file = Builder::new(b'I', false, vec![page]).build();
        let (out, found) = strip(&file);
        assert!(found.is_empty(), "{found:?}");
        let kept = value(&out, &read(&out).pages[0], 34377).unwrap();
        assert!(kept.len() > irb.len(), "{} {}", kept.len(), irb.len());
        assert!(out.len() > file.len(), "{} {}", out.len(), file.len());
        assert!(kept.windows(7).any(|w| w == b"Path 40"));
        assert!(kept.ends_with(&res(2999, "", b"\x06Path 1\0\0")));
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
    fn byte_counts_libtiff_ignores_are_copied_as_libtiff_reads_them() {
        // One strip of 0 or 15,000 bytes, and ten in pairs of 3,000 and
        // 1,500: libtiff reads the whole picture, so the output holds it.
        for (strips, counts) in [
            (1, vec![0]),
            (1, vec![15_000]),
            (10, [3_000, 1_500].repeat(5)),
        ] {
            let (out, _) = strip(&miscounted(strips, 1, counts.clone()));
            let mut decoder = tiff::decoder::Decoder::new(Cursor::new(&out)).unwrap();
            let tiff::decoder::DecodingResult::U8(back) = decoder.read_image().unwrap() else {
                panic!("not 8-bit")
            };
            assert_eq!(back, picture(), "{counts:?}");
            let lens: u64 = read(&out).pages[0].blocks.iter().map(|b| b.len).sum();
            assert_eq!(lens, 30_000);
        }
        // Two strips, the second short: refused before anything's written.
        let file = miscounted(2, 1, vec![15_000, 7_500]);
        assert!(matches!(plan(&file), Err(Error::Refused(why)) if why == MISSIZED));
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

    #[test]
    fn the_tags_a_codec_needs_to_decode_are_kept() {
        // LERC with Deflate or ZSTD inside (`tiffcp -c lerc:s1`): libtiff
        // reads which from LercParameters (50674), and without it can't
        // decode the strips ("lerc_getBlobInfo() failed"). It went, as
        // other fields.
        let mut page = gray(8, 8, 8, vec![B::data(&[7; 64])]);
        page.tags
            .extend([(259, V::Short(vec![34887])), (50674, V::Long(vec![4, 1]))]);
        let file = Builder::new(b'M', false, vec![page]).build();
        let (out, found) = strip(&file);
        assert!(found.is_empty(), "{found:?}");
        let tiff = read(&out);
        let entry = tiff.pages[0].get(50674).unwrap();
        assert_eq!((entry.kind, entry.count), (4, 2));
        assert_eq!(
            value(&out, &tiff.pages[0], 50674).unwrap(),
            [0, 0, 0, 4, 0, 0, 0, 1]
        );
    }

    #[test]
    fn a_kept_tag_is_kept_only_as_libtiff_reads_it() {
        // libtiff reads a tag Remove Metadata keeps in some types and
        // counts only, and where its codec or picture uses it; any other
        // entry it ignores, and Pillow shows: such an entry goes, as other
        // fields.
        let page = |compression: u16, tags: Vec<(u16, V)>| {
            let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
            page.tags.push((259, V::Short(vec![compression])));
            for (tag, value) in tags {
                page.tags.retain(|t| t.0 != tag);
                page.tags.push((tag, value));
            }
            Builder::new(b'I', false, vec![page]).build()
        };
        let gone = |file: Vec<u8>, tag: u16| {
            let (out, found) = strip(&file);
            assert_eq!(found, [Other].into(), "{tag}");
            assert!(read(&out).pages[0].get(tag).is_none(), "{tag}");
            for secret in [&b"secret"[..], b"SECRET", b"John"] {
                assert!(!out.windows(secret.len()).any(|w| w == secret), "{tag}");
            }
        };
        let stays = |file: Vec<u8>, tag: u16| {
            let (out, found) = strip(&file);
            assert!(found.is_empty(), "{tag}: {found:?}");
            let tiff = read(&out);
            let (a, b) = (
                read(&file).pages[0].get(tag).cloned(),
                tiff.pages[0].get(tag),
            );
            let a = a.unwrap();
            let b = b.unwrap();
            assert_eq!((a.kind, a.count), (b.kind, b.count), "{tag}");
            assert_eq!(
                value(&file, &read(&file).pages[0], tag),
                value(&out, &tiff.pages[0], tag)
            );
        };
        // Every tag it keeps, in a type libtiff doesn't read it in: text,
        // or for those it reads as bytes (text too), two FLOATs. The page
        // is compressed, one strip, so that no layout tag need be read;
        // one that says how the picture is laid out is refused instead.
        for tag in (0..=u16::MAX).filter(|&tag| kept(tag) && !matches!(tag, 259 | 347)) {
            let wrong = match shape(tag) {
                Some(Shape::Bytes(_)) => V::Bytes(11, b"secrets!".to_vec()),
                _ => V::Ascii("secret value"),
            };
            let file = page(8, vec![(tag, wrong)]);
            if LAID_OUT.contains(&tag) {
                let refused =
                    matches!(plan(&file), Err(Error::Refused(why)) if why == UNREAD_LAYOUT);
                assert!(refused, "{tag}");
            } else {
                gone(file, tag);
            }
        }
        // A review's: an Orientation, an XResolution and a LercParameters
        // as text, eight LONGs of LercParameters on an uncompressed page,
        // InkNames on a gray one, and a no-data value of words.
        let secret = b"SECRET_IN_ORIENTATION_51.5N_0.1W".to_vec();
        let longs = secret
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()));
        for (compression, tag, value) in [
            (1, 274, V::Ascii("SECRET_IN_ORIENTATION_51.5N_0.1W")),
            (1, 282, V::Ascii("secret resolution")),
            (1, 50674, V::Ascii("secret parameters")),
            (1, 50674, V::Long(longs.collect())),
            (1, 333, V::Ascii("shot by John at home")),
            (1, 42113, V::Ascii("shot by John at home")),
            // Counts libtiff ignores: two Orientations, one PageNumber, an
            // SMinSampleValue for two samples of one.
            (1, 274, V::Short(vec![1, 6])),
            (1, 297, V::Short(vec![1])),
            (1, 340, V::Rational(vec![(1, 1), (2, 1)])),
            // Words after a no-data value's NUL.
            (1, 42113, V::Bytes(2, b"-9999\0secret\0".to_vec())),
            // JPEGTables on LZW data, which libtiff doesn't read.
            (5, 347, V::Bytes(7, b"\xFF\xD8\xFF\xD9".to_vec())),
            // A review's: a ColorMap of 64 SHORTs on an 8-bit page, where
            // libtiff reads 3 × 256 ("Incorrect count"); a TransferFunction
            // of 100.
            (1, 320, V::Bytes(3, secret.repeat(4))),
            (1, 301, V::Short(vec![7; 100])),
            // SubfileType, GrayResponseUnit and GrayResponseCurve, which
            // libtiff never reads.
            (1, 255, V::Short(vec![1])),
            (1, 290, V::Short(vec![2])),
            (1, 291, V::Short(vec![7; 256])),
        ] {
            gone(page(compression, vec![(tag, value)]), tag);
        }
        // A ColorMap of 32-bit samples, which libtiff ignores past 24.
        let deep = page(
            8,
            vec![(258, V::Short(vec![32])), (320, V::Short(vec![7; 3]))],
        );
        gone(deep, 320);
        // And one listed before BitsPerSample, which libtiff ignores too
        // ("since BitsPerSample tag not found").
        let mut early = page(1, vec![(320, V::Short(vec![7; 768]))]);
        let at = read(&early).pages[0].at as usize;
        let entry = |file: &[u8], tag: u16| {
            (0..usize::from(u16::from_le_bytes([file[at], file[at + 1]])))
                .map(|n| at + 2 + 12 * n)
                .find(|&e| u16::from_le_bytes([file[e], file[e + 1]]) == tag)
                .unwrap()
        };
        let (bits, map) = (entry(&early, 258), entry(&early, 320));
        let (a, b) = (
            early[bits..bits + 12].to_vec(),
            early[map..map + 12].to_vec(),
        );
        early[bits..bits + 12].copy_from_slice(&b);
        early[map..map + 12].copy_from_slice(&a);
        gone(early, 320);
        // And kept as libtiff reads them: an Orientation typed LONG, an
        // XResolution typed SHORT, a no-data value that's a number, eight
        // LONGs of LercParameters on LERC data, InkNames on a separated
        // page, and an SMinSampleValue for its one sample.
        for (compression, tag, value) in [
            (1, 274, V::Long(vec![6])),
            (1, 282, V::Short(vec![300])),
            (1, 42113, V::Ascii("-9999")),
            (1, 42113, V::Ascii(" nan ")),
            (
                1,
                42113,
                V::Bytes(2, b"-3.4028234663852886e+38\0\0".to_vec()),
            ),
            (34887, 50674, V::Long(vec![4, 1, 0, 0, 0, 0, 0, 0])),
            (1, 340, V::Rational(vec![(1, 1)])),
            // A ColorMap of 3 × 256, a TransferFunction of 256 or 3 × 256,
            // on an 8-bit page.
            (1, 320, V::Short(vec![7; 768])),
            (1, 301, V::Short(vec![7; 256])),
            (1, 301, V::Short(vec![7; 768])),
        ] {
            stays(page(compression, vec![(tag, value)]), tag);
        }
        let separated = page(
            1,
            vec![
                (262, V::Short(vec![5])),
                (333, V::Ascii("Cyan\0Magenta\0Yellow\0Black")),
            ],
        );
        stays(separated, 333);
    }

    #[test]
    fn a_layout_tag_libtiff_cant_read_is_refused_not_dropped() {
        // The review's file: an 8 × 8 page of 32-bit float gray whose
        // SampleFormat is FLOAT 3.0, or DOUBLE or RATIONAL 3. libtiff
        // can't load it ("Incompatible type for SampleFormat"), Pillow
        // reads it as float (0.25 at (1, 0)); its SampleFormat dropped,
        // both read integers (1048576000). It's refused, as a layout Grape
        // can't read.
        let float = |kind: u16, value: Vec<u8>, extra: Vec<(u16, V)>| {
            let samples: Vec<u8> = (0..64)
                .flat_map(|i| (i as f32 / 4.0).to_le_bytes())
                .collect();
            let mut page = gray(8, 8, 8, vec![B::data(&samples)]);
            page.tags.retain(|t| t.0 != 258);
            page.tags
                .extend([(258, V::Short(vec![32])), (339, V::Bytes(kind, value))]);
            page.tags.extend(extra);
            Builder::new(b'I', false, vec![page]).build()
        };
        let refused =
            |file: &[u8]| matches!(plan(file), Err(Error::Refused(why)) if why == UNREAD_LAYOUT);
        for (kind, value) in [
            (11, 3.0_f32.to_le_bytes().to_vec()),
            (12, 3.0_f64.to_le_bytes().to_vec()),
            (5, [3_u32.to_le_bytes(), 1_u32.to_le_bytes()].concat()),
        ] {
            assert!(refused(&float(kind, value, vec![])), "{kind}");
        }
        // As a SHORT, it stays.
        let (out, found) = strip(&float(3, 3_u16.to_le_bytes().to_vec(), vec![]));
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(read(&out).pages[0].uint(339, Order::Little), Some(3));
        // ExtraSamples: none, or as many as the samples, stay; more go
        // with the file, as libtiff can't load the page.
        let two = |extra: V| {
            let mut page = gray(1, 1, 1, vec![B::data(b"xy")]);
            page.tags.extend([(277, V::Short(vec![2])), (338, extra)]);
            Builder::new(b'I', false, vec![page]).build()
        };
        for extra in [V::Short(vec![]), V::Short(vec![2]), V::Short(vec![2, 0])] {
            let (out, found) = strip(&two(extra.clone()));
            assert!(found.is_empty(), "{extra:?}: {found:?}");
            assert!(read(&out).pages[0].get(338).is_some());
        }
        assert!(refused(&two(V::Short(vec![2, 0, 0]))));
        // A palette page's ColorMap libtiff can't read, which it reads as
        // gray: refused; a gray page's goes.
        let mapped = |photometric: u16, map: V| {
            let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
            page.tags.retain(|t| t.0 != 262);
            page.tags
                .extend([(262, V::Short(vec![photometric])), (320, map)]);
            Builder::new(b'I', false, vec![page]).build()
        };
        assert!(refused(&mapped(3, V::Ascii("SECRET shot by John"))));
        assert!(refused(&mapped(3, V::Short(vec![7; 64]))));
        let (_, found) = strip(&mapped(1, V::Ascii("SECRET shot by John")));
        assert_eq!(found, [Other].into());
        let (out, found) = strip(&mapped(3, V::Short(vec![7; 768])));
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(read(&out).pages[0].get(320).unwrap().count, 768);
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
    fn the_exif_color_space_is_read_as_libtiff_reads_it() {
        // libtiff 4.5.1 reads an Exif IFD's ColorSpace only as a SHORT
        // ("Wrong data type 4 for \"ColorSpace\"; tag ignored"): one typed
        // LONG or SSHORT says nothing, and the DCF Exif doesn't go back,
        // where it did. Of more than one value it reads the first ("tag
        // trimmed"), as Pillow reads (65535, 1), when they're in the entry
        // (two in a classic file, four in a BigTIFF); of more, it reads the
        // entry's offset as the value (130 for values at 130), which isn't
        // a ColorSpace: such a one says nothing.
        for (big, space, back) in [
            (false, V::Long(vec![0xFFFF]), false),
            (false, V::Bytes(8, vec![0xFF, 0xFF]), false),
            (false, V::Short(vec![0xFFFF]), true),
            (false, V::Short(vec![0xFFFF, 1]), true),
            (false, V::Short(vec![1, 0xFFFF]), false),
            (false, V::Short(vec![0xFFFF, 1, 1]), false),
            (true, V::Short(vec![0xFFFF, 1, 1]), true),
            (true, V::Short(vec![0xFFFF, 1, 1, 1, 1]), false),
        ] {
            let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
            page.exif = Some(vec![(0xA001, space.clone())]);
            page.interop = Some(vec![(1, V::Ascii("R03"))]);
            let (out, _) = strip(&Builder::new(b'I', big, vec![page]).build());
            let exif = read(&out).pages[0].exif.clone();
            assert_eq!(exif.is_some(), back, "{big} {space:?}");
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
    fn a_kept_tag_listed_twice_differently_is_refused() {
        // Readers that take a tag's first entry and those that take its
        // last would see different files, the output only the first's: a
        // ColorMap twice (Pillow's pixels change), JPEGTables twice, the
        // second with an APP1, which went unread and unsaid, Photoshop's
        // resources twice, and a ColorSpace or an Interop index twice in
        // the Exif IFD, which decide the DCF Exif.
        let refused = |page: Page| match plan(&Builder::new(b'I', false, vec![page]).build()) {
            Err(Error::Refused(why)) => Some(why),
            Ok(_) => None,
            Err(e) => panic!("{e:?}"),
        };
        let app1 = [
            &b"\xFF\xD8"[..],
            b"\xFF\xE1\0\x0EExif\0\0MM\0*\0\0",
            b"\xFF\xD9",
        ]
        .concat();
        for (tag, first, second) in [
            (320, V::Short(vec![1; 768]), V::Short(vec![2; 768])),
            (
                347,
                V::Bytes(7, b"\xFF\xD8\xFF\xD9".to_vec()),
                V::Bytes(7, app1),
            ),
            (
                34377,
                V::Bytes(7, res(1005, "", &[0; 8])),
                V::Bytes(7, res(2000, "", &[0; 26])),
            ),
            (274, V::Short(vec![1]), V::Short(vec![6])),
            (
                34675,
                V::Bytes(7, vec![0; 200]),
                V::Bytes(7, [&[1][..], &[0; 199]].concat()),
            ),
        ] {
            let mut page = gray(8, 8, 8, vec![B::data(&[7; 64])]);
            page.tags.extend([(tag, first), (tag, second)]);
            assert_eq!(refused(page).as_deref(), Some(TWICE), "{tag}");
        }
        let exif = |space: &[u16], index: &[&'static str]| {
            let mut page = gray(8, 8, 8, vec![B::data(&[7; 64])]);
            page.exif = Some(space.iter().map(|&s| (0xA001, V::Short(vec![s]))).collect());
            page.interop = Some(index.iter().map(|&i| (1, V::Ascii(i))).collect());
            page
        };
        for page in [
            exif(&[1, 0xFFFF], &["R03"]),
            exif(&[0xFFFF], &["R98", "R03"]),
        ] {
            assert_eq!(refused(page).as_deref(), Some(TWICE));
        }
        // The same entry twice is one, kept once, and so is the same value
        // twice, in another type or another place (two profiles of 200
        // zeros); a dropped tag twice goes.
        let mut page = exif(&[0xFFFF, 0xFFFF], &["R03", "R03"]);
        page.tags.extend([
            (274, V::Short(vec![6])),
            (274, V::Short(vec![6])),
            (296, V::Short(vec![2])),
            (296, V::Long(vec![2])),
            (305, V::Ascii("secret one")),
            (305, V::Ascii("secret two")),
        ]);
        let mut profiled = gray(8, 8, 8, vec![B::data(&[7; 64])]);
        profiled.tags.extend([
            (34675, V::Bytes(7, vec![0; 200])),
            (34675, V::Bytes(7, vec![0; 200])),
        ]);
        let file = Builder::new(b'I', false, vec![page, profiled]).build();
        let (out, found) = strip(&file);
        assert_eq!(found, [Software].into());
        let tiff = read(&out);
        for (page, tag) in [(0, 274), (0, 296), (1, 34675)] {
            let all = tiff.pages[page].entries.iter().filter(|e| e.tag == tag);
            assert_eq!(all.count(), 1, "{tag}");
        }
        assert_eq!(value(&out, &tiff.pages[1], 34675).unwrap(), [0; 200]);
        assert!(dcf_only(tiff.pages[0].exif.as_ref().unwrap()));
    }

    #[test]
    fn every_description_is_read_for_imagej_and_ome() {
        // tifffile reads ImageJ's description from either of a page's
        // first two, the second too.
        for second in [
            "ImageJ=1.54f\nimages=3\n",
            "<?xml version=\"1.0\"?><OME xmlns=\"x\">",
        ] {
            let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
            page.tags
                .extend([(270, V::Ascii("A scan")), (270, V::Ascii(second))]);
            let Err(Error::Refused(why)) = plan(&Builder::new(b'I', false, vec![page]).build())
            else {
                panic!("{second} not refused")
            };
            assert_eq!(why, DESCRIBED);
        }
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
    fn jpeg_tables_must_be_soi_tables_and_eoi_alone() {
        // libjpeg reads tables only up to their EOI, so what follows it,
        // what comes before their SOI and segments that aren't tables no
        // reader shows: tables with an APP1 after their EOI, stray bytes
        // before their SOI, or an APP1 before it, kept the APP1.
        let (soi, eoi) = (b"\xFF\xD8".to_vec(), b"\xFF\xD9".to_vec());
        let (dqt, dht) = (segment(0xDB, &[0; 65]), segment(0xC4, &[0; 17]));
        let app1 = segment(0xE1, b"Exif\0\0MM\0*");
        let tables = [&soi[..], &dqt, &dht, &eoi].concat();
        let why = |tables: Vec<u8>| match plan(&jpeg_page(jpeg(b""), Some(tables))) {
            Ok(_) => None,
            Err(Error::Refused(why)) => Some(why),
            Err(e) => panic!("{e:?}"),
        };
        let unseen = Some(unchecked("JPEG"));
        assert_eq!(why(tables.clone()), None);
        // Fill bytes before a marker, and arithmetic coding's and restart
        // intervals' tables, are tables too.
        let dac = segment(0xCC, &[0, 1]);
        let dri = segment(0xDD, &[0, 4]);
        let filled = [&soi[..], b"\xFF\xFF", &dqt, &dac, &dri, &eoi].concat();
        assert_eq!(why(filled), None);
        for refused in [
            [&tables[..], &app1].concat(),
            [&tables[..], &[0]].concat(),
            [&b"\0\0"[..], &tables].concat(),
            [&app1[..], &tables].concat(),
            // An APP0 (a JFIF thumbnail), an APP2, a frame, stray bytes,
            // or no EOI.
            [&soi[..], &segment(0xE0, b"JFIF\0"), &dqt, &eoi].concat(),
            [&soi[..], &segment(0xE2, b"ICC_PROFILE\0"), &eoi].concat(),
            [&soi[..], &dqt, &frame(0xC0, 1), &eoi].concat(),
            [&soi[..], &dqt, b"\0", &eoi].concat(),
            [&soi[..], &dqt].concat(),
        ] {
            assert_eq!(why(refused.clone()), unseen, "{refused:?}");
        }
        // Metadata among the tables is said to be so.
        let inside = [&soi[..], &dqt, &app1, &eoi].concat();
        assert_eq!(why(inside).as_deref(), Some(IN_DATA));
    }

    #[test]
    fn jpeg_tables_of_no_bytes_are_none() {
        // libtiff ignores a JPEGTables of count 0 (`JPEGVSetField` refuses
        // it), on JPEG data as on any: it's none, and goes unsaid, where
        // it was refused as JPEG data Grape can't check.
        let data = jpeg(&[]);
        for compression in [7, 5] {
            let mut page = gray(8, 8, 8, vec![B::Data(data.clone())]);
            page.tags.extend([
                (259, V::Short(vec![compression])),
                (347, V::Bytes(7, Vec::new())),
            ]);
            let file = Builder::new(b'I', false, vec![page]).build();
            assert_eq!(read(&file).pages[0].get(347).unwrap().count, 0);
            let (out, found) = strip(&file);
            assert!(found.is_empty(), "{compression}: {found:?}");
            assert!(read(&out).pages[0].get(347).is_none(), "{compression}");
        }
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

    /// `bytes`, each as a value of type `kind` (little-endian).
    fn each_as(kind: u16, bytes: &[u8]) -> V {
        let size = super::super::read::type_size(kind).unwrap() as usize;
        V::Bytes(
            kind,
            bytes
                .iter()
                .flat_map(|&b| u64::from(b).to_le_bytes()[..size].to_vec())
                .collect(),
        )
    }

    /// A page of `compression` (a JPEG strip for 7, gray bytes otherwise)
    /// whose JPEGTables are `tables`, classic or BigTIFF.
    fn with_tables(big: bool, compression: u16, tables: V) -> Vec<u8> {
        let strip = if compression == 7 {
            jpeg(b"")
        } else {
            vec![7; 64]
        };
        let mut page = gray(8, 8, 8, vec![B::Data(strip)]);
        page.tags
            .extend([(259, V::Short(vec![compression])), (347, tables)]);
        Builder::new(b'I', big, vec![page]).build()
    }

    #[test]
    fn jpeg_tables_are_read_as_libtiff_reads_them_whatever_their_type() {
        // Tables of SOI, an Exif APP1 and EOI, each byte a value of a type
        // libtiff reads JPEGTables in (`TIFFReadDirEntryByteArray`), which
        // hands libjpeg, and TIFFGetField, the stream: refused, in a JPEG
        // page or any other; tables of SOI and EOI alone are kept.
        let app1 = b"\xFF\xE1\0\x0EExif\0\0MM\0*\0\0";
        let stream = [&b"\xFF\xD8"[..], app1, b"\xFF\xD9"].concat();
        let clean = b"\xFF\xD8\xFF\xD9";
        let why = |file: &[u8]| match plan(file) {
            Ok(_) => None,
            Err(Error::Refused(why)) => Some(why),
            Err(e) => panic!("{e:?}"),
        };
        for (big, kind) in [
            (false, 1),
            (false, 2),
            (false, 3),
            (false, 4),
            (false, 7),
            (false, 8),
            (false, 9),
            (true, 16),
            (true, 17),
        ] {
            for compression in [7, 1] {
                let file = with_tables(big, compression, each_as(kind, &stream));
                assert_eq!(why(&file).as_deref(), Some(IN_DATA), "{kind} {compression}");
                let file = with_tables(big, compression, each_as(kind, clean));
                assert_eq!(why(&file), None, "{kind} {compression}");
            }
        }
        // Where libtiff can't read them as bytes, what readers take from
        // them can't be checked: an SBYTE stream (0xFF is negative), a
        // SHORT past a byte or a negative SSHORT among the values, and a
        // type it doesn't read them in.
        let mut wide = each_as(3, clean);
        if let V::Bytes(_, bytes) = &mut wide {
            bytes[3] = 1;
        }
        let mut negative = each_as(8, clean);
        if let V::Bytes(_, bytes) = &mut negative {
            bytes[2..4].copy_from_slice(&[0xFF, 0xFF]);
        }
        for tables in [
            each_as(6, &stream),
            wide,
            negative,
            V::Rational(vec![(0xFF, 0xD8)]),
            V::Bytes(11, vec![0; 8]),
            V::Bytes(13, vec![0xFF, 0xD8, 0xFF, 0xD9]),
        ] {
            for compression in [7, 1] {
                let file = with_tables(false, compression, tables.clone());
                assert_eq!(
                    why(&file).as_deref(),
                    Some(unchecked("JPEG").as_str()),
                    "{tables:?}"
                );
            }
        }
    }

    /// A RIFF chunk, padded to even.
    fn chunk(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let len = (data.len() as u32).to_le_bytes();
        let pad = vec![0; data.len() % 2];
        [&fourcc[..], &len, data, &pad].concat()
    }

    /// A WebP file of `chunks`.
    fn webp(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body = chunks.concat();
        let len = (body.len() as u32 + 4).to_le_bytes();
        [&b"RIFF"[..], &len, b"WEBP", &body].concat()
    }

    /// A gray 8 × 8 page of one strip, `data`, of `compression`.
    fn coded_page(compression: u16, data: Vec<u8>) -> Vec<u8> {
        let mut page = gray(8, 8, 8, vec![B::Data(data)]);
        page.tags.push((259, V::Short(vec![compression])));
        Builder::new(b'I', false, vec![page]).build()
    }

    #[test]
    fn webp_data_with_exif_or_xmp_chunks_is_refused_and_plain_webp_kept() {
        let refused = |file: &[u8]| match plan(file) {
            Ok(_) => None,
            Err(Error::Refused(why)) => Some(why),
            Err(e) => panic!("{e:?}"),
        };
        // A lossless picture's chunk (the bytes don't matter), in an
        // extended file as Pillow writes it with its Exif and XMP after
        // the picture, each alone, and with neither.
        let vp8x = chunk(b"VP8X", &[0x0C, 0, 0, 0, 7, 0, 0, 7, 0, 0]);
        let vp8l = chunk(b"VP8L", &[0x2F, 7, 0xC0, 0x01, 0, 0x55, 0x55, 0x55]);
        let exif = chunk(b"EXIF", b"MM\0*\0\0\0\x08\0\0");
        let xmp = chunk(b"XMP ", b"<x:xmpmeta>a secret</x:xmpmeta>");
        let icc = chunk(b"ICCP", &[1; 20]);
        for (chunks, metadata) in [
            (
                vec![vp8x.clone(), vp8l.clone(), exif.clone(), xmp.clone()],
                true,
            ),
            (vec![vp8x.clone(), vp8l.clone(), exif.clone()], true),
            (
                vec![vp8x.clone(), icc.clone(), vp8l.clone(), xmp.clone()],
                true,
            ),
            (vec![vp8x.clone(), icc.clone(), vp8l.clone()], false),
            (vec![vp8l.clone()], false),
        ] {
            let file = coded_page(50001, webp(&chunks));
            let want = metadata.then_some(IN_DATA.to_string());
            assert_eq!(refused(&file), want, "{chunks:?}");
        }
        // A bare bitstream, which libwebp reads too, has no chunks: VP8L's
        // (its signature) or a VP8 key frame's (its start code).
        assert_eq!(refused(&coded_page(50001, vp8l[8..].to_vec())), None);
        let key_frame = [0x50, 0x02, 0, 0x9D, 0x01, 0x2A, 8, 0, 8, 0, 0x55, 0x55];
        assert_eq!(refused(&coded_page(50001, key_frame.to_vec())), None);
        // One pad byte after the container is fine.
        let mut padded = webp(&[vp8x.clone(), vp8l.clone()]);
        padded.push(0);
        assert_eq!(refused(&coded_page(50001, padded)), None);
        // Anything else can't be checked: the review's container whose
        // size covers only its VP8L, its EXIF and XMP after it in the
        // strip; its ALPH, EXIF and VP8L chunks with no RIFF header, which
        // libwebp walks too; a chunk past the container's end; two bytes
        // past it; a JPEG stream; a VP8 frame that isn't a key frame.
        let unseen = "has WebP data Grape can't check for metadata yet.";
        let mut short = webp(std::slice::from_ref(&vp8l));
        short.extend([exif.clone(), xmp.clone()].concat());
        let headless = [chunk(b"ALPH", &[0; 9]), exif.clone(), vp8l.clone()].concat();
        let mut cut = webp(&[vp8l.clone(), exif.clone()]);
        cut[16..20].copy_from_slice(&1_000u32.to_le_bytes());
        let mut two = webp(std::slice::from_ref(&vp8l));
        two.extend([0, 0]);
        let mut inter = key_frame;
        inter[0] |= 1;
        for data in [short, headless, cut, two, jpeg(b""), inter.to_vec()] {
            let file = coded_page(50001, data.clone());
            assert_eq!(refused(&file).as_deref(), Some(unseen), "{data:?}");
        }
        // 64 chunks, read by their headers, then an EXIF: past what's read.
        let many = [vec![chunk(b"ABCD", b"x"); 64], vec![exif.clone()]].concat();
        let file = coded_page(50001, webp(&many));
        assert_eq!(refused(&file).as_deref(), Some(unseen));
        let fewer = [vec![chunk(b"ABCD", b"x"); 63], vec![exif]].concat();
        let file = coded_page(50001, webp(&fewer));
        assert_eq!(refused(&file).as_deref(), Some(IN_DATA));
        // Another codec's strip isn't read as WebP.
        let file = coded_page(50000, webp(&[vp8l, xmp]));
        assert_eq!(refused(&file), None);
    }

    #[test]
    fn a_block_jpeg_and_webp_ifds_share_is_read_as_both() {
        // A strip a WebP page and a JPEG page list: a JPEG with an Exif
        // APP1, read first as WebP; and a WebP with an EXIF chunk, read
        // first as JPEG (not a stream it knows). Each block was read once,
        // as the first IFD's kind, and passed the other's check. (A JPEG
        // stream isn't WebP data Grape can check, now, either.)
        let app1 = b"\xFF\xE1\0\x0EExif\0\0MM\0*\0\0";
        let vp8l = chunk(b"VP8L", &[0x2F, 7, 0xC0, 0x01, 0, 0x55, 0x55, 0x55]);
        let exif = chunk(b"EXIF", b"MM\0*\0\0\0\x08\0\0");
        let unseen = unchecked("WebP");
        for (data, first, second, want) in [
            (jpeg(app1), 50001, 7, unseen.as_str()),
            (webp(&[vp8l, exif]), 7, 50001, IN_DATA),
        ] {
            let page = |compression: u16| {
                let mut page = gray(8, 8, 8, vec![B::Data(data.clone())]);
                page.tags.push((259, V::Short(vec![compression])));
                page
            };
            let mut file = Builder::new(b'I', false, vec![page(first), page(second)]).build();
            let tiff = read(&file);
            let Block { offset, len } = tiff.pages[0].blocks[0];
            set_block(&mut file, tiff.pages[1].at, 0, offset, len);
            let Err(Error::Refused(why)) = plan(&file) else {
                panic!("{first} then {second} not refused")
            };
            assert_eq!(why, want, "{first} then {second}");
        }
    }

    #[test]
    fn jpeg_2000_and_jpeg_xl_data_is_refused_as_unchecked() {
        for (compression, kind) in [
            (33003, "JPEG 2000"),
            (33004, "JPEG 2000"),
            (33005, "JPEG 2000"),
            (34712, "JPEG 2000"),
            (50002, "JPEG XL"),
            // JPEG XL as DNG 1.7 numbers it.
            (52546, "JPEG XL"),
            (22610, "JPEG XR"),
            (34934, "JPEG XR"),
            (34933, "PNG"),
            // Codes Grape doesn't know: camera raws, GoPro's, Pixar's
            // film, Kodak's DCS, TIFF/IT, JBIG2.
            (9, "image"),
            (32767, "image"),
            (32908, "image"),
            (32947, "image"),
            (32895, "image"),
            (34713, "image"),
            (34715, "image"),
            (65000, "image"),
            (65535, "image"),
        ] {
            let file = coded_page(compression, b"\0\0\0\x0CjP  \r\n\x87\n".to_vec());
            let Err(Error::Refused(why)) = plan(&file) else {
                panic!("{compression} not refused")
            };
            assert_eq!(
                why,
                format!("has {kind} data Grape can't check for metadata yet.")
            );
        }
    }

    #[test]
    fn only_data_that_cant_hold_metadata_or_that_grape_reads_goes_through() {
        // Codecs whose streams hold nothing a reader reads but the
        // picture are copied as they are, whatever their bytes say.
        let app1 = b"\xFF\xE1\0\x0EExif\0\0MM\0*\0\0";
        for compression in [
            1, 2, 3, 4, 5, 8, 32766, 32771, 32773, 32809, 32909, 32946, 34661, 34676, 34677, 34887,
            34925, 34926, 50000, 50013,
        ] {
            let file = coded_page(compression, jpeg(app1));
            assert!(plan(&file).is_ok(), "{compression}");
        }
        // The codes that hold JPEG streams are read as JPEG: DNG's lossy
        // JPEG (34892), Bio-Formats' (33007) and Leaf's lossless (99)
        // too; WebP's old code (34927) as WebP.
        let vp8l = chunk(b"VP8L", &[0x2F, 7, 0xC0, 0x01, 0, 0x55, 0x55, 0x55]);
        let exif = chunk(b"EXIF", b"MM\0*\0\0\0\x08\0\0");
        for (compression, plain, tagged) in [
            (7, jpeg(b""), jpeg(app1)),
            (99, jpeg(b""), jpeg(app1)),
            (33007, jpeg(b""), jpeg(app1)),
            (34892, jpeg(b""), jpeg(app1)),
            (
                50001,
                webp(std::slice::from_ref(&vp8l)),
                webp(&[vp8l.clone(), exif.clone()]),
            ),
            (
                34927,
                webp(std::slice::from_ref(&vp8l)),
                webp(&[vp8l.clone(), exif.clone()]),
            ),
        ] {
            assert!(
                plan(&coded_page(compression, plain)).is_ok(),
                "{compression}"
            );
            let Err(Error::Refused(why)) = plan(&coded_page(compression, tagged)) else {
                panic!("{compression} not refused")
            };
            assert_eq!(why, IN_DATA, "{compression}");
        }
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
        assert_eq!(why, unchecked("JPEG"));
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
            assert_eq!(why, unchecked("JPEG"));
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
        assert_eq!(why, unchecked("JPEG"));
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
            assert_eq!(why, unchecked("JPEG"));
        }
        // A stream that ends there, read whole, has nothing after it.
        assert_eq!(segments(b"\xFF\xD8\xFF\xE2\0\x10ab", true), Seen::Clean);
        assert_eq!(segments(b"\xFF\xD8\xFF\xE2\0\x10ab", false), Seen::Unseen);
        assert_eq!(segments(b"\xFF\xD8\xFF\xDA\0\x02", false), Seen::Clean);
    }

    /// A source that counts the bytes read from it, and its reads and
    /// seeks: what reach the file.
    #[derive(Default)]
    struct Tally<'f> {
        file: Cursor<&'f [u8]>,
        read: u64,
        calls: u64,
    }

    impl std::io::Read for Tally<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.file.read(buf)?;
            self.read += n as u64;
            self.calls += 1;
            Ok(n)
        }
    }

    impl std::io::Seek for Tally<'_> {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.calls += 1;
            self.file.seek(pos)
        }
    }

    #[test]
    fn a_webp_blocks_chunks_are_read_from_one_read_of_it() {
        // 2,000 WebP strips of 64 chunks each: each chunk's header was read
        // alone, a seek and a read of the file each (1.32 million of each
        // at 20,000 strips); a block's first 64 KiB are read once now, and
        // its chunks walked in them.
        let vp8l = chunk(b"VP8L", &[0x2F, 7, 0xC0, 0x01, 0, 0x55, 0x55, 0x55]);
        let strip = webp(&[vec![chunk(b"ABCD", b"x"); 63], vec![vp8l]].concat());
        let mut page = gray(
            8,
            2_000,
            1,
            (0..2_000).map(|_| B::Data(strip.clone())).collect(),
        );
        page.tags.push((259, V::Short(vec![50001])));
        let file = Builder::new(b'I', false, vec![page]).build();
        let mut r = std::io::BufReader::with_capacity(
            64 * 1024,
            Tally {
                file: Cursor::new(&file),
                ..Tally::default()
            },
        );
        Plan::new(&mut r, file.len() as u64).unwrap();
        // A seek to each block and a read of it (and the reader's own
        // asks for where it is), and the IFD's.
        let calls = r.get_ref().calls;
        assert!(calls < 4 * 2_000 + 1_000, "{calls} reads and seeks");
    }

    #[test]
    fn what_ifds_share_or_the_report_has_said_is_read_once() {
        // 40 JPEG pages whose strip and description are the first page's,
        // of over 64 KiB each, and JPEGTables, of 60 KB, and whose
        // Software tags are 64 KiB each, all different.
        let strip = [jpeg(b""), vec![0; 64 * 1024]].concat();
        let dqt = segment(0xDB, &[0; 65]);
        let tables = [&b"\xFF\xD8"[..], &dqt.repeat(900), b"\xFF\xD9"].concat();
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
                ..Tally::default()
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
                ..Tally::default()
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
    fn blocks_listed_out_of_the_files_order_read_only_their_own_bytes() {
        // A page of 4,096 one-byte strips, last first in the file, between
        // a 64 KiB profile and 64 KiB of padding, plain or JPEG (scanned
        // for metadata): refilling a 64 KiB buffer for each would read
        // 256 MiB to write them, and as much again to scan them.
        for compression in [1, 7] {
            let strips = (0..4096).map(|i| B::Data(vec![(i % 251) as u8])).collect();
            let mut page = gray(1, 4096, 1, strips);
            page.tags.extend([
                (259, V::Short(vec![compression])),
                (34675, V::Bytes(7, vec![1; 64 * 1024])),
            ]);
            page.tags.sort_by_key(|t| t.0);
            let file = Builder {
                reversed: true,
                tail: vec![0; 64 * 1024],
                ..Builder::new(b'I', false, vec![page])
            }
            .build();
            let mut r = std::io::BufReader::with_capacity(
                64 * 1024,
                Tally {
                    file: Cursor::new(&file),
                    ..Tally::default()
                },
            );
            let plan = Plan::new(&mut r, file.len() as u64).unwrap();
            let mut out = Cursor::new(Vec::new());
            plan.write(&mut r, &mut out, &CancelToken::new()).unwrap();
            let read = r.get_ref().read;
            assert!(
                read < 2 * file.len() as u64,
                "{compression}: {read} bytes read"
            );
        }
    }

    #[test]
    fn ifds_laid_out_last_first_read_only_their_own_bytes() {
        // 4,096 pages of one strip, whose IFDs are laid out last first:
        // refilling a 64 KiB buffer for each would read 256 MiB.
        let pages = 4_096u32;
        let ifd = 2 + 12 * 7 + 4;
        let strips = 8 + pages * ifd;
        let place = |n: u32| 8 + (pages - 1 - n) * ifd;
        let mut file = b"II*\0".to_vec();
        file.extend(place(0).to_le_bytes());
        for n in (0..pages).rev() {
            let entries: [(u16, u16, u32, u32); 7] = [
                (256, 4, 1, 1),
                (257, 4, 1, 1),
                (258, 3, 1, 8),
                (262, 3, 1, 1),
                (273, 4, 1, strips + n),
                (278, 4, 1, 1),
                (279, 4, 1, 1),
            ];
            file.extend(7u16.to_le_bytes());
            for (tag, kind, count, value) in entries {
                file.extend(tag.to_le_bytes());
                file.extend(kind.to_le_bytes());
                file.extend(count.to_le_bytes());
                file.extend(value.to_le_bytes());
            }
            let next = if n + 1 < pages { place(n + 1) } else { 0 };
            file.extend(next.to_le_bytes());
        }
        file.extend((0..pages).map(|n| n as u8));
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
        // 40 JPEG pages whose descriptions are 64 KiB of one run of text,
        // each 2 bytes further on, or whose JPEG tables are tables of
        // nearly 64 KiB, each the DQT's data of the one before, 6 bytes
        // further on: 2.5 MiB to read from a file of under 100 KB.
        let run = b"A scan ".repeat(10_000);
        let mut nested = [
            &b"\xFF\xD8"[..],
            &segment(0xDB, &vec![0; 65_528 - 39 * 8 - 8]),
            b"\xFF\xD9",
        ]
        .concat();
        for _ in 1..40 {
            nested = [&b"\xFF\xD8"[..], &segment(0xDB, &nested), b"\xFF\xD9"].concat();
        }
        assert_eq!(nested.len(), 65_528);
        for (tag, kind, run, step, shrink) in [(270, 2, &run, 2, 0), (347, 7, &nested, 6, 8)] {
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
            let Value::At { offset, len } = tiff.pages[0].get(tag).unwrap().value else {
                panic!("an inline {tag}")
            };
            let len = len.min(64 * 1024);
            for (n, page) in (0..).zip(&tiff.pages) {
                set_entry(&mut file, page.at, tag, len - shrink * n, offset + step * n);
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
