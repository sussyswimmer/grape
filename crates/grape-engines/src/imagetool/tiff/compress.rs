//! Compress on TIFF (D28): lossless at every preset. Every page and SubIFD
//! is written again compact by the writer Remove Metadata uses
//! ([`write`](super::write)), in the source's byte order and kind, with
//! every tag; each strip or tile of an IFD Compress can decode is deflated
//! again ([`codec`](super::codec)), at a zlib level from what the whole
//! file has to recode ([`level`]), differenced when its samples allow, and
//! read back before the file is published.
//!
//! - **Recoded:** IFDs of compression 1 (None), 5 (LZW), 8 or 32946
//!   (Deflate) or 32773 (PackBits), with a FillOrder of 1 or none, whose
//!   blocks [`Geometry`] can size. Only their Compression (to 8), their
//!   Predictor and their blocks' offsets and counts change: RowsPerStrip,
//!   the tiles and the planes stay.
//! - **Copied as they are**, with their tags ([`How::Copy`]): JPEG, fax,
//!   JBIG, JPEG 2000, LZMA, ZSTD, WebP and other codecs; FillOrder 2
//!   (libtiff reverses the compressed bytes, so new Deflate data would come
//!   out wrong); layouts that can't be sized; an IFD with an empty block
//!   (GDAL's sparse tiles, which stay 0 bytes at 0: as Deflate, the tiff
//!   crate and Pillow fail on them); and an IFD with a block that
//!   can't be decoded (old-style LZW, data that's broken or short), whose
//!   recode writes more bytes than a copy of it would (each block it lists
//!   once, its whole size unless it's copied already, even when it's
//!   recoded already, which costs the recode nothing), or whose recode
//!   takes the output past 95 % of the source, whose blocks written
//!   already are taken back ([`Recoder`]).
//! - **Predictor:** a source's own (2, 3 or another) on LZW or Deflate
//!   stays, its decoded bytes deflated as they are. Otherwise (None and
//!   PackBits, whatever their 317 says, since libtiff applies a predictor
//!   only inside LZW and Deflate; LZW or Deflate without one) predictor 2
//!   is added when the photometric interpretation is 0, 1, 2 or 5, every
//!   sample is 8 or 16 bits, the sample format is 1 or 2 (or not given)
//!   and a row is at most [`MAX_ROW`], a pixel's samples apart, or 1 when
//!   the planes are; and 317 is 1 when it's given.
//! - **Kept:** every other tag, with the Exif, GPS and Interop IFDs moved
//!   with their values; XMP, IPTC, Photoshop's resources and layers,
//!   GeoTIFF, ICC, Orientation, a MakerNote (as bytes). FreeOffsets and
//!   FreeByteCounts (288, 289) go, as they'd point at nothing; so does an
//!   entry, or an Exif, GPS or Interop IFD, that can't be read. Any other
//!   entry typed IFD or IFD8 is refused ([`UNMOVABLE`]): what it points at
//!   can't be moved.
//! - **Checked:** each block written is read back through a second handle
//!   on the output, inflated, its predictor undone, and the CRC32 of what
//!   it holds must be what the source's decoded to; then the output is
//!   walked again, and its IFDs must have their blocks where they were
//!   written ([`Wrote::check`]). Otherwise it's Grape's own failure.
//! - **Size:** with nothing to recode it ends `NoChange` before anything is
//!   created ([`Planned::Nothing`]); once the output passes 95 % of the
//!   source while it's written, at once ([`Stopped::NotSmaller`]), unless
//!   that's while an IFD is recoded, which is copied as it is instead if
//!   that fits; and the runner holds the finished file to 95 % too. Never
//!   "Try Small": the presets are the same.
//! - **Held:** the model, a step of decoded bytes (or a row, when a row is
//!   longer), the codecs' state, and at most [`MAX_PENDING`] blocks waiting
//!   to be read back. The token is checked at every step of decoded bytes:
//!   256 KiB, or, when a predictor runs over them, as many whole rows as
//!   fit, one at least, and a row may be up to [`MAX_ROW`]. Within a step,
//!   the `Cancellable` source and temp file catch a cancel at their next
//!   read or write, 64 KiB of either (13 to 98 ms in a review's measure);
//!   one that lands in the temp file's last flush is a cancel too.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, SeekFrom, Write};
use std::path::Path;

use grape_core::{Format, Job};

use super::super::strip::{self as strips, Error, In, Out};
use super::codec::{numbers, Codec, Codecs, Fail, Geometry, Predictor, Recipe, Why, MAX_SAMPLES};
use super::read::{repeats, Block, Entry, Ifd, Layout, Meta, Order, Tiff};
use super::write::{self, Blocks, Data, Dir, Field, Image, Sink, Written};
use crate::cancel::Cancellable;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{CancelToken, EngineCtx, GrapeError, JobOutcome, Note};

/// The longest row Compress differences: one is held whole.
pub(crate) const MAX_ROW: u64 = 64 << 20;

/// The most blocks written and not yet read back, and their bytes: past
/// either, they're read back before the next.
pub(crate) const MAX_PENDING: usize = 4_096;
const MAX_PENDING_BYTES: u64 = 4 << 20;

/// The most reasons the log keeps.
const MAX_LOG: usize = 16;

/// The most places kept of shared blocks written in a way other than the
/// one they were first written in (another recipe's, or copied as they
/// are): past them, such a block is written again each time it's listed.
const MAX_MORE: usize = 65_536;

/// The most decoded bytes Compress recodes at zlib's level 9 in a file,
/// and at level 6; past them, [`LEVEL_PAST`] (D28's Q10).
const LEVEL_9_UP_TO: u64 = 16 << 20;
const LEVEL_6_UP_TO: u64 = 256 << 20;

/// The level past [`LEVEL_6_UP_TO`], so that 1 GiB takes about a minute
/// at most: of the levels that ran at 18 MB/s or more on photo-like scans
/// in a Linux release build (1 to 5), the one that wrote the least, and
/// faster than miniz_oxide's 4 and 5, which wrote more.
const LEVEL_PAST: u32 = 3;

/// The zlib level Compress deflates a file's blocks at, from the decoded
/// bytes it recodes in the whole file, a block listed more than once
/// counted once for each way it's recoded ([`Ways`]): 9 for up to 16 MiB,
/// 6 for up to 256 MiB, [`LEVEL_PAST`] past that (D28's Q10). The presets
/// are the same.
pub(crate) fn level(recoded: u64) -> u32 {
    if recoded <= LEVEL_9_UP_TO {
        9
    } else if recoded <= LEVEL_6_UP_TO {
        6
    } else {
        LEVEL_PAST
    }
}

/// Why a file with an entry that points where Compress can't follow is
/// refused.
pub(crate) const UNMOVABLE: &str = "has data Compress can't move yet.";

/// How an image IFD's blocks are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum How {
    /// Decoded and deflated again, and these fields set (259, and 317 when
    /// it changes).
    Recode {
        codec: Codec,
        geometry: Geometry,
        predictor: Option<Predictor>,
        fields: Vec<Field>,
    },
    /// Copied as they are: why, for the log.
    Copy(Why),
}

/// What [`Planned::of`] made of a TIFF.
#[derive(Debug)]
pub(crate) enum Planned {
    /// No IFD Compress can recode: why, for the log.
    Nothing(String),
    Plan(Plan),
}

/// Compress's plan for one TIFF.
#[derive(Debug)]
pub(crate) struct Plan {
    layout: Layout,
    pages: Vec<Dir>,
    /// How each image IFD's blocks are written, in the order the writer
    /// writes them (`Tiff::ifds`').
    hows: Vec<How>,
    /// The decoded bytes of the blocks recoded and the bytes of those
    /// copied, each block once for each way it's written ([`Ways`]): what
    /// progress counts.
    work: u64,
    /// The zlib level the blocks are deflated at ([`level`]).
    level: u32,
    /// What the plan dropped, for the log.
    log: Log,
    /// A test's broken encoder ([`Codecs::broken`]).
    #[cfg(test)]
    broken: bool,
}

/// Reasons for the log: the first [`MAX_LOG`], and how many more.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Log {
    lines: Vec<String>,
    more: usize,
}

impl Log {
    fn add(&mut self, line: String) {
        if self.lines.len() < MAX_LOG {
            self.lines.push(line);
        } else {
            self.more += 1;
        }
    }

    /// `detail`, then what the log holds, if anything.
    pub(crate) fn after(&self, detail: String) -> String {
        if self.lines.is_empty() {
            detail
        } else {
            format!("{detail}; {self}")
        }
    }
}

impl std::fmt::Display for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.lines.join("; "))?;
        if self.more > 0 {
            write!(f, "; and {} more", self.more)?;
        }
        Ok(())
    }
}

impl Planned {
    /// Walks the TIFF `r` (`len` bytes) and plans how each IFD is written,
    /// or refuses it ([module docs](self)); the walk refuses DNG and
    /// old-style JPEG.
    pub(crate) fn of(r: &mut dyn In, len: u64) -> Result<Planned, Error> {
        let tiff = Tiff::read(r, len)?;
        let order = tiff.layout.order;
        // Sample sizes and formats read out of line, each time, come to
        // what the file holds at most: only arrays that overlap come to
        // more.
        let mut left = len;
        let mut hows = Vec::new();
        // Each block once for each way it's written.
        let mut ways = Ways::new(tiff.shared());
        let (mut recoded, mut copied): (u64, u64) = (0, 0);
        let mut log = Log::default();
        for (n, ifd) in tiff.ifds().into_iter().enumerate() {
            let how = how(r, ifd, order, &mut left)?;
            let blocks = ifd.blocks.iter().enumerate().filter(|(_, b)| b.len > 0);
            match &how {
                How::Recode {
                    codec,
                    geometry,
                    predictor,
                    ..
                } => {
                    for (i, block) in blocks {
                        let recipe = Recipe {
                            codec: *codec,
                            expected: geometry.expected(i as u64),
                            row: geometry.row,
                            predictor: *predictor,
                        };
                        if ways.new_way(block, Some(recipe)) {
                            recoded = recoded.saturating_add(recipe.expected);
                        }
                    }
                }
                How::Copy(why) => {
                    log.add(format!("IFD {n} copied as it is: {why}"));
                    for (_, block) in blocks {
                        if ways.new_way(block, None) {
                            copied = copied.saturating_add(block.len);
                        }
                    }
                }
            }
            hows.push(how);
        }
        drop(ways);
        if !hows.iter().any(|how| matches!(how, How::Recode { .. })) {
            return Ok(Planned::Nothing(log.to_string()));
        }
        let Tiff { layout, pages } = tiff;
        let pages = pages
            .into_iter()
            .map(|page| dir(page, &mut log))
            .collect::<Result<_, _>>()?;
        Ok(Planned::Plan(Plan {
            layout,
            pages,
            hows,
            work: recoded.saturating_add(copied),
            level: level(recoded),
            log,
            #[cfg(test)]
            broken: false,
        }))
    }
}

/// The ways the blocks listed more than once are written, as the
/// [`Recoder`] writes them, so that the plan counts what it will write:
/// once for each way (copied as it is, or each [`Recipe`]), and past
/// [`MAX_MORE`] ways besides a block's first, each time it's listed.
struct Ways {
    shared: Vec<Block>,
    /// Each shared block's first way: 0 for none yet, 1 for copied, or its
    /// recipe's number.
    first: Vec<u32>,
    more: HashSet<(usize, u32)>,
    /// The recipes met, numbered from 2.
    recipes: HashMap<Recipe, u32>,
}

impl Ways {
    fn new(shared: Vec<Block>) -> Ways {
        Ways {
            first: vec![0; shared.len()],
            shared,
            more: HashSet::new(),
            recipes: HashMap::new(),
        }
    }

    /// Whether `block` is written as `recipe` (copied as it is for `None`)
    /// for the first time, noting that it is.
    fn new_way(&mut self, block: &Block, recipe: Option<Recipe>) -> bool {
        let Ok(k) = self.shared.binary_search(block) else {
            return true;
        };
        let way = match recipe {
            None => 1,
            Some(recipe) => {
                let next = u32::try_from(self.recipes.len() + 2).unwrap_or(u32::MAX);
                *self.recipes.entry(recipe).or_insert(next)
            }
        };
        let Some(first) = self.first.get_mut(k) else {
            return true;
        };
        if *first == 0 {
            *first = way;
            return true;
        }
        if *first == way || self.more.contains(&(k, way)) {
            return false;
        }
        if self.more.len() < MAX_MORE {
            self.more.insert((k, way));
        }
        true
    }
}

impl Plan {
    /// Writes the plan into `w` (a new, empty file), copying and decoding
    /// from the source `r` it was made from and reading each block written
    /// back through `back` (a second handle on what `w` writes); stops once
    /// the output passes `most` bytes. Gives what it wrote; the caller cuts
    /// the file at its size (an IFD taken back may have left bytes past
    /// it).
    pub(crate) fn write(
        &self,
        r: &mut dyn In,
        w: &mut dyn Out,
        back: &mut dyn In,
        most: u64,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(f32),
    ) -> Result<Wrote, Halt> {
        let mut recoder = Recoder::new(self, back, most, cancel, progress);
        let written = write::write_with(r, w, self.layout, &self.pages, &mut recoder);
        let len = match (recoder.stop.take(), written) {
            (None, Ok(len)) => len,
            (Some(why), _) => {
                return Err(Halt {
                    why,
                    log: recoder.log,
                })
            }
            (None, Err(e)) => {
                return Err(Halt {
                    why: Stopped::Failed(e),
                    log: recoder.log,
                })
            }
        };
        Ok(Wrote {
            len,
            layout: self.layout,
            ifds: self.hows.len(),
            shape: recoder.shape.finalize(),
            log: recoder.log,
        })
    }
}

/// What [`Plan::write`] wrote.
#[derive(Debug)]
pub(crate) struct Wrote {
    /// The output's size.
    pub(crate) len: u64,
    /// Its byte order and kind, and how many image IFDs it has.
    layout: Layout,
    ifds: usize,
    /// The CRC32 of every block's place and size in the output, IFD by IFD.
    shape: u32,
    /// What was copied as it is or dropped, and why.
    pub(crate) log: Log,
}

impl Wrote {
    /// Reads the output `r` again, which the plan needn't be held for: the
    /// same byte order and kind, and the same image IFDs, with their blocks
    /// where they were written. Gives what's wrong.
    pub(crate) fn check(&self, r: &mut dyn In) -> Result<(), String> {
        let out = Tiff::read(r, self.len).map_err(|e| format!("it can't be read: {e:?}"))?;
        if (out.layout.order, out.layout.big) != (self.layout.order, self.layout.big) {
            return Err("it has another byte order or kind".into());
        }
        let ifds = out.ifds();
        if ifds.len() != self.ifds {
            return Err(format!("it has {} IFDs, not {}", ifds.len(), self.ifds));
        }
        let mut shape = crc32fast::Hasher::new();
        for ifd in ifds {
            add_shape(&mut shape, ifd.blocks.iter().map(|b| (b.offset, b.len)));
        }
        if shape.finalize() != self.shape {
            return Err("its blocks aren't where they were written".into());
        }
        Ok(())
    }
}

/// Why [`Plan::write`] stopped, and what its log held by then.
#[derive(Debug)]
pub(crate) struct Halt {
    pub(crate) why: Stopped,
    pub(crate) log: Log,
}

/// Why [`Plan::write`] stopped.
#[derive(Debug)]
pub(crate) enum Stopped {
    /// The output passed 95 % of the source's size.
    NotSmaller,
    /// A block read back wrong: Grape's own failure.
    Wrong(String),
    /// Reading the source, writing the output, or the job failed.
    Failed(Error),
}

/// How `ifd`'s blocks are written ([module docs](self)).
fn how(r: &mut dyn In, ifd: &Ifd, order: Order, left: &mut u64) -> Result<How, Error> {
    let copy = |why: String| Ok(How::Copy(why));
    let Some(compression) = ifd.get(259).map_or(Some(1), |e| e.uint(order)) else {
        return copy("its Compression can't be read".into());
    };
    let Some(codec) = Codec::of(compression) else {
        return copy(format!("its compression is {compression}"));
    };
    // FillOrder 2: libtiff reverses the compressed bytes' bits.
    match ifd.get(266).map(|e| e.uint(order)) {
        None | Some(Some(1)) => {}
        Some(Some(fill)) => return copy(format!("its FillOrder is {fill}")),
        Some(None) => return copy("its FillOrder can't be read".into()),
    }
    let geometry = match Geometry::of(r, ifd, order, left)? {
        Ok(geometry) => geometry,
        Err(why) => return copy(why),
    };
    if geometry.blocks() != ifd.blocks.len() as u64 {
        return copy(format!(
            "it has {} blocks, where its layout has {}",
            ifd.blocks.len(),
            geometry.blocks()
        ));
    }
    // An empty block (GDAL's sparse tiles) isn't Deflate data: the tiff
    // crate and Pillow fail on it once the IFD says Deflate.
    if ifd.blocks.iter().any(|b| b.len == 0) {
        return copy("it has empty strips or tiles".into());
    }
    let own = match ifd.get(317) {
        None => 1,
        Some(entry) => match entry.uint(order) {
            Some(own) => own,
            None => return copy("its Predictor can't be read".into()),
        },
    };
    let short = |tag: u16, v: u64| Field {
        tag,
        kind: 3,
        count: 1,
        value: Data::Bytes(order.bytes(v, 2)),
    };
    let mut fields = vec![short(259, 8)];
    let predictor = if matches!(codec, Codec::Lzw | Codec::Deflate) && own != 1 {
        // Already in the decoded bytes, which stay as they are.
        None
    } else {
        let added = added(r, ifd, order, &geometry, left)?;
        if added.is_some() {
            fields.push(short(317, 2));
        } else if ifd.get(317).is_some() {
            fields.push(short(317, 1));
        }
        added
    };
    Ok(How::Recode {
        codec,
        geometry,
        predictor,
        fields,
    })
}

/// The predictor Compress adds to `ifd`'s unpredicted samples, if it adds
/// one ([module docs](self)).
fn added(
    r: &mut dyn In,
    ifd: &Ifd,
    order: Order,
    geometry: &Geometry,
    left: &mut u64,
) -> Result<Option<Predictor>, Error> {
    let photometric = ifd.uint(262, order);
    if !matches!(photometric, Some(0 | 1 | 2 | 5))
        || geometry.subsampled
        || !matches!(geometry.bits, 8 | 16)
        || geometry.row > MAX_ROW
    {
        return Ok(None);
    }
    let formats = match ifd.get(339) {
        None => vec![1],
        Some(entry) => numbers(r, entry, order, MAX_SAMPLES, left)?.unwrap_or_default(),
    };
    if formats.is_empty() || formats.iter().any(|f| !matches!(f, 1 | 2)) {
        return Ok(None);
    }
    let stride = if geometry.planar {
        1
    } else {
        usize::try_from(geometry.samples).unwrap_or(usize::MAX)
    };
    Ok(Some(Predictor {
        wide: geometry.bits == 16,
        stride,
        order,
    }))
}

/// The image IFD `ifd` as it will be written: every entry but the ones the
/// writer makes or that would point at nothing, its Exif and GPS IFDs,
/// and its SubIFDs.
fn dir(ifd: Ifd, log: &mut Log) -> Result<Dir, Error> {
    let mut fields = Vec::new();
    for entry in &ifd.entries {
        match entry.tag {
            // Made again by the writer, or below.
            273 | 279 | 324 | 325 | 330 | 34665 | 34853 => {}
            // FreeOffsets and FreeByteCounts: what they point at is gone.
            288 | 289 => {}
            _ => fields.extend(field(entry, log)?),
        }
    }
    let mut children = Vec::new();
    for (tag, meta) in [(34665, &ifd.exif), (34853, &ifd.gps)] {
        if let Some(meta) = meta {
            children.extend(meta_dir(meta, log)?.map(|dir| (tag, dir)));
        }
    }
    let subs = ifd
        .subs
        .into_iter()
        .map(|chain| chain.into_iter().map(|sub| dir(sub, log)).collect())
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

/// An Exif, GPS or Interop IFD as it will be written, with its Interop
/// IFD; `None` when it can't be read.
fn meta_dir(meta: &Meta, log: &mut Log) -> Result<Option<Dir>, Error> {
    if meta.unread {
        log.add(format!(
            "an Exif, GPS or Interop IFD at {} can't be read, and goes",
            meta.at
        ));
        return Ok(None);
    }
    let mut fields = Vec::new();
    for entry in &meta.entries {
        if entry.tag != 40965 {
            fields.extend(field(entry, log)?);
        }
    }
    let children = match &meta.interop {
        Some(interop) => meta_dir(interop, log)?
            .map(|dir| (40965, dir))
            .into_iter()
            .collect(),
        None => Vec::new(),
    };
    Ok(Some(Dir {
        fields,
        image: None,
        children,
    }))
}

/// `entry` as it's written: as it is; `None` when its value can't be read
/// (an unknown type, or past the file's end). One typed IFD or IFD8
/// points where Compress can't follow, and is refused.
fn field(entry: &Entry, log: &mut Log) -> Result<Option<Field>, Error> {
    if matches!(entry.kind, 13 | 18) {
        return Err(Error::Refused(UNMOVABLE.into()));
    }
    let field = Field::copy(entry);
    if field.is_none() {
        log.add(format!("tag {} can't be read, and goes", entry.tag));
    }
    Ok(field)
}

/// Adds blocks' places and sizes to `shape`.
fn add_shape(shape: &mut crc32fast::Hasher, blocks: impl Iterator<Item = (u64, u64)>) {
    for (offset, len) in blocks {
        shape.update(&offset.to_le_bytes());
        shape.update(&len.to_le_bytes());
    }
}

/// Where a block listed more than once went (at 0 until it's written), and
/// how: 0 for copied as it is, or the number of the [`Recipe`] it was
/// recoded by (a strip shorter than the others is recoded by another).
#[derive(Debug, Clone, Copy, Default)]
struct Place {
    at: u64,
    len: u64,
    how: u32,
    /// In a block's first place: the number of the last IFD whose recode
    /// met it, plus 1, so that what a copy of that IFD would write counts
    /// it once (it takes the padding after `how`).
    seen: u32,
}

/// What [`Recoder::recode`] made of an IFD's blocks.
enum Recode {
    /// Every block recoded.
    Done(Recoded),
    /// Given up, to be copied as it is: why, for the log.
    Copy(Why),
}

/// An IFD's blocks, recoded.
struct Recoded {
    written: Written,
    /// The bytes the recode wrote (not the blocks it found written already
    /// in the same way), and the bytes a copy of the IFD as it is would
    /// write: each block it lists, once, but those copied already.
    made: u64,
    copy: u64,
}

/// Why an IFD whose recode took the output past the most is copied as it
/// is instead.
const PASSED: &str = "recoded, it took the output past 95 % of the source";

/// The output while a block is written: a write that would take it past
/// `most` bytes fails, and says so.
struct Capped<'s, 'w> {
    out: &'s mut Sink<'w>,
    most: u64,
    over: bool,
}

impl Write for Capped<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.out.at().saturating_add(buf.len() as u64) > self.most {
            self.over = true;
            return Err(io::Error::other("the output passed 95 % of the source"));
        }
        self.out.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Progress and the cancel check, at each step of decoded bytes.
struct Ticker<'p> {
    done: u64,
    work: u64,
    shown: f32,
    progress: &'p mut dyn FnMut(f32),
    cancel: &'p CancelToken,
}

impl Ticker<'_> {
    /// `n` more bytes are done: from 0.02 to 0.95 over the plan's work.
    fn tick(&mut self, n: u64) -> Result<(), Error> {
        if self.cancel.is_cancelled() {
            return Err(Error::Write(io::Error::other("cancelled")));
        }
        self.done = self.done.saturating_add(n);
        let part = self.done as f64 / self.work.max(1) as f64;
        let now = (0.02 + 0.93 * part.min(1.0)) as f32;
        if now > self.shown {
            self.shown = now;
            (self.progress)(now);
        }
        Ok(())
    }
}

/// Compress's [`Blocks`]: each image IFD's blocks recoded or copied as its
/// [`How`] says, read back, and taken back to be copied when one can't be
/// decoded.
struct Recoder<'a> {
    hows: &'a [How],
    codecs: Codecs,
    /// The second handle on the output, blocks are read back through.
    back: &'a mut dyn In,
    /// Blocks written and not yet read back: where, how, and the CRC32 of
    /// what the source's decoded to.
    pending: Vec<(Block, Recipe, u32)>,
    pending_bytes: u64,
    /// The blocks listed more than once, sorted; where each was first
    /// written, and how (at 0 until it's written), which is when it's
    /// counted against the source's size, once; and where it went written
    /// in other ways, up to [`MAX_MORE`] of them, those of the IFD being
    /// written noted to forget if it's taken back.
    shared: Vec<Block>,
    placed: Vec<Place>,
    more: HashMap<(usize, u32), Place>,
    more_now: Vec<(usize, u32)>,
    /// The recipes of shared blocks written, numbered from 1.
    recipes: HashMap<Recipe, u32>,
    most: u64,
    buf: Vec<u8>,
    ticker: Ticker<'a>,
    /// The blocks' places and sizes, as [`Wrote::check`] reads them.
    shape: crc32fast::Hasher,
    log: Log,
    /// Why it stopped the writer, when it did.
    stop: Option<Stopped>,
}

impl<'a> Recoder<'a> {
    fn new(
        plan: &'a Plan,
        back: &'a mut dyn In,
        most: u64,
        cancel: &'a CancelToken,
        progress: &'a mut dyn FnMut(f32),
    ) -> Recoder<'a> {
        let images = write::images(&plan.pages);
        let shared = repeats(|| {
            let blocks = images.iter().flat_map(|image| &image.blocks);
            blocks.filter(|b| b.len > 0)
        });
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut codecs = Codecs::new(plan.level);
        #[cfg(test)]
        {
            codecs.broken = plan.broken;
        }
        Recoder {
            hows: &plan.hows,
            codecs,
            back,
            pending: Vec::new(),
            pending_bytes: 0,
            placed: vec![Place::default(); shared.len()],
            shared,
            more: HashMap::new(),
            more_now: Vec::new(),
            recipes: HashMap::new(),
            most,
            buf: vec![0; 64 * 1024],
            ticker: Ticker {
                done: 0,
                work: plan.work,
                shown: 0.0,
                progress,
                cancel,
            },
            shape: crc32fast::Hasher::new(),
            log: plan.log.clone(),
            stop: None,
        }
    }

    /// Stops the writer for `why`.
    fn stop(&mut self, why: Stopped) -> Error {
        self.stop = Some(why);
        Error::Write(io::Error::other("Compress stopped"))
    }

    /// Where the shared `block`, written as `how` (0 copied, else a
    /// recipe's number), went already; and its index among the shared.
    fn placed(&self, block: Block, how: u32) -> (Option<Place>, Option<usize>) {
        let Ok(k) = self.shared.binary_search(&block) else {
            return (None, None);
        };
        (self.place(k, how), Some(k))
    }

    /// Where the shared block `k`, written as `how`, went already.
    fn place(&self, k: usize, how: u32) -> Option<Place> {
        let first = self.placed.get(k)?;
        if first.at > 0 && first.how == how {
            Some(*first)
        } else {
            self.more.get(&(k, how)).copied()
        }
    }

    /// Whether IFD `n`'s recode meets the shared block `k` for the first
    /// time, noting that it has.
    fn first_in(&mut self, k: usize, n: usize) -> bool {
        let Some(first) = self.placed.get_mut(k) else {
            return true;
        };
        let mark = u32::try_from(n + 1).unwrap_or(u32::MAX);
        std::mem::replace(&mut first.seen, mark) != mark
    }

    /// Notes that a block, the shared one `k` if it is one, went to
    /// `place`: whether that's the first time it's written in any way, the
    /// one time it's counted against the source's size.
    fn put(&mut self, k: Option<usize>, place: Place) -> bool {
        let Some(first) = k.and_then(|k| self.placed.get_mut(k)) else {
            return true;
        };
        if first.at == 0 {
            *first = Place {
                seen: first.seen,
                ..place
            };
            return true;
        }
        if let Some(k) = k.filter(|_| self.more.len() < MAX_MORE) {
            self.more.insert((k, place.how), place);
            self.more_now.push((k, place.how));
        }
        false
    }

    /// The number a shared block written as `recipe` is noted with.
    fn number(&mut self, recipe: Recipe) -> u32 {
        let next = self.recipes.len() as u32 + 1;
        *self.recipes.entry(recipe).or_insert(next)
    }

    /// Copies `image`'s blocks as they are.
    fn copy(&mut self, r: &mut dyn In, out: &mut Sink, image: &Image) -> Result<Written, Error> {
        let mut written = Written {
            offsets: Vec::with_capacity(image.blocks.len()),
            ..Written::default()
        };
        for &block in &image.blocks {
            if block.len == 0 {
                written.offsets.push(0);
                continue;
            }
            let (place, k) = self.placed(block, 0);
            if let Some(place) = place {
                written.offsets.push(place.at);
                continue;
            }
            out.align()?;
            let at = out.at();
            let mut capped = Capped {
                out,
                most: self.most,
                over: false,
            };
            let copied = write::copy(r, &mut capped, block.offset, block.len, &mut self.buf);
            if capped.over {
                return Err(self.stop(Stopped::NotSmaller));
            }
            copied?;
            self.ticker.tick(block.len)?;
            let place = Place {
                at,
                len: block.len,
                how: 0,
                seen: 0,
            };
            if self.put(k, place) {
                written.taken = written.taken.saturating_add(block.len);
            }
            written.offsets.push(at);
        }
        Ok(written)
    }

    /// Recodes `image`'s blocks as `how` says; gives up on them when one
    /// can't be decoded, or when they take the output past the most (a
    /// copy may fit where they didn't: noise grows recoded).
    fn recode(
        &mut self,
        r: &mut dyn In,
        out: &mut Sink,
        image: &Image,
        how: &How,
        n: usize,
    ) -> Result<Recode, Error> {
        match self.recode_blocks(r, out, image, how, n) {
            Ok(recoded) => Ok(recoded),
            Err(Fail::Undecodable(why)) => Ok(Recode::Copy(why)),
            Err(Fail::Job(e)) => Err(e),
        }
    }

    /// [`Recoder::recode`]'s work: [`Fail::Undecodable`] when a block can't
    /// be decoded.
    fn recode_blocks(
        &mut self,
        r: &mut dyn In,
        out: &mut Sink,
        image: &Image,
        how: &How,
        n: usize,
    ) -> Result<Recode, Fail> {
        let How::Recode {
            codec,
            geometry,
            predictor,
            fields,
        } = how
        else {
            return Err(Fail::Job(Error::Broken("an IFD to copy, recoded".into())));
        };
        let mut offsets = Vec::with_capacity(image.blocks.len());
        let mut lens = Vec::with_capacity(image.blocks.len());
        let mut taken: u64 = 0;
        let (mut made, mut copy) = (0u64, 0u64);
        // No block is empty ([`how`]).
        for (i, &block) in image.blocks.iter().enumerate() {
            let recipe = Recipe {
                codec: *codec,
                expected: geometry.expected(i as u64),
                row: geometry.row,
                predictor: *predictor,
            };
            let k = self.shared.binary_search(&block).ok();
            // A copy would write it, once, unless it's copied already;
            // even if it's recoded already, which costs the recode nothing.
            let fresh = match k {
                Some(k) => self.first_in(k, n) && self.place(k, 0).is_none(),
                None => true,
            };
            if fresh {
                copy = copy.saturating_add(block.len);
            }
            let number = match k {
                Some(_) => self.number(recipe),
                None => 0,
            };
            if let Some(place) = k.and_then(|k| self.place(k, number)) {
                offsets.push(place.at);
                lens.push(place.len);
                continue;
            }
            out.align()?;
            let at = out.at();
            let mut capped = Capped {
                out,
                most: self.most,
                over: false,
            };
            let ticker = &mut self.ticker;
            let crc = self
                .codecs
                .recode(r, block, recipe, &mut capped, &mut |n| ticker.tick(n));
            if capped.over {
                return Ok(Recode::Copy(PASSED.into()));
            }
            let crc = crc?;
            let len = out.at() - at;
            let place = Place {
                at,
                len,
                how: number,
                seen: 0,
            };
            if self.put(k, place) {
                taken = taken.saturating_add(block.len);
            }
            made = made.saturating_add(len);
            let written = Block { offset: at, len };
            offsets.push(at);
            lens.push(len);
            self.pending.push((written, recipe, crc));
            self.pending_bytes = self.pending_bytes.saturating_add(len);
            if self.pending.len() >= MAX_PENDING || self.pending_bytes >= MAX_PENDING_BYTES {
                self.read_back(out)?;
            }
        }
        let written = Written {
            offsets,
            lens: Some(lens),
            fields: fields.clone(),
            taken,
        };
        Ok(Recode::Done(Recoded {
            written,
            made,
            copy,
        }))
    }

    /// Reads back each block waiting to be: it must give the CRC32 its
    /// source's decoded bytes did.
    fn read_back(&mut self, out: &mut Sink) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        out.flush().map_err(Error::Write)?;
        // Nothing read before may stay buffered: an IFD taken back is
        // written over.
        self.back.seek(SeekFrom::Start(0)).map_err(Error::Read)?;
        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        for (block, recipe, crc) in pending {
            let wrong = match self.codecs.check(self.back, block, recipe) {
                Ok(back) if back == crc => continue,
                Ok(back) => format!("its CRC32 is {back:08x}, not {crc:08x}"),
                Err(Fail::Undecodable(why)) => why,
                Err(Fail::Job(e)) => return Err(e),
            };
            let why = format!(
                "the block written at {} reads back wrong: {wrong}",
                block.offset
            );
            return Err(self.stop(Stopped::Wrong(why)));
        }
        Ok(())
    }

    /// Takes back what was written of `image`, IFD `n`, from `start` on,
    /// for `why`: the places its blocks went are forgotten.
    fn take_back(
        &mut self,
        out: &mut Sink,
        image: &Image,
        start: u64,
        n: usize,
        why: &str,
    ) -> Result<(), Error> {
        out.rewind(start)?;
        self.pending.retain(|(block, ..)| block.offset < start);
        self.pending_bytes = self.pending.iter().map(|(block, ..)| block.len).sum();
        for block in &image.blocks {
            let k = self.shared.binary_search(block).ok();
            if let Some(first) = k.and_then(|k| self.placed.get_mut(k)) {
                if first.at >= start {
                    *first = Place::default();
                }
            }
        }
        for key in self.more_now.drain(..) {
            self.more.remove(&key);
        }
        self.log.add(format!("IFD {n} copied as it is: {why}"));
        Ok(())
    }
}

impl Blocks for Recoder<'_> {
    fn write(
        &mut self,
        n: usize,
        r: &mut dyn In,
        out: &mut Sink,
        image: &Image,
    ) -> Result<Written, Error> {
        let Some(how) = self.hows.get(n) else {
            return Err(Error::Broken(format!("no plan for IFD {n}")));
        };
        self.more_now.clear();
        let written = match how {
            How::Copy(_) => self.copy(r, out, image)?,
            How::Recode { .. } => {
                let start = out.at();
                let recoded = match self.recode(r, out, image, how, n)? {
                    // Larger than a copy (noise, data deflated better
                    // already, or blocks copied already): copied as it is.
                    Recode::Done(Recoded { made, copy, .. }) if made > copy => Err(format!(
                        "recoded, its blocks came to {made} bytes, where a copy writes {copy}"
                    )),
                    Recode::Done(recoded) => Ok(recoded.written),
                    Recode::Copy(why) => Err(why),
                };
                match recoded {
                    Ok(written) => {
                        self.read_back(out)?;
                        written
                    }
                    // Copied, held to the most too.
                    Err(why) => {
                        self.take_back(out, image, start, n, &why)?;
                        self.copy(r, out, image)?
                    }
                }
            }
        };
        let lens: Box<dyn Iterator<Item = u64>> = match &written.lens {
            Some(lens) => Box::new(lens.iter().copied()),
            None => Box::new(image.blocks.iter().map(|b| b.len)),
        };
        add_shape(&mut self.shape, written.offsets.iter().copied().zip(lens));
        Ok(written)
    }
}

/// `e` with what `log` holds after its detail.
fn logged(mut e: GrapeError, log: &Log) -> GrapeError {
    e.detail = log.after(std::mem::take(&mut e.detail));
    e
}

/// Runs Compress on the TIFF `src` (named as `from`) into `dest`.
pub(crate) fn run(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, from);
    let dir = publish::dest_dir(dest, &names)?;
    let cancel = &ctx.cancel;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let file =
        File::open(src).map_err(|e| names.read_failed(format!("open {}: {e}", src.display())))?;
    let before = file
        .metadata()
        .map_err(|e| names.read_failed(format!("stat {}: {e}", src.display())))?
        .len();
    let failed = |e: Error, temp: Option<&Path>| {
        let e = match e {
            Error::Broken(why) => names.decode_failed(format!("{}: {why}", src.display())),
            Error::TooLarge(why) => names.too_large(format!("{}: {why}", src.display())),
            Error::Refused(why) => strips::refused(&names, src, &why),
            Error::Read(e) => names.read_failed(format!("read {}: {e}", src.display())),
            Error::Write(e) => {
                let at =
                    temp.map_or_else(|| dir.display().to_string(), |t| t.display().to_string());
                names.write_failed(format!("write {at}: {e}"))
            }
        };
        publish::or_cancelled(cancel, src, e)
    };
    let mut reader = BufReader::with_capacity(64 * 1024, Cancellable::new(file, cancel));
    let plan = match Planned::of(&mut reader, before).map_err(|e| failed(e, None))? {
        Planned::Nothing(why) => {
            let detail = format!("{}: nothing Compress re-encodes: {why}", src.display());
            return Err(names.not_smaller(false, detail));
        }
        Planned::Plan(plan) => plan,
    };
    progress(0.02);

    // From here on, every error says why IFDs were copied as they are: the
    // plan knows already for those it doesn't recode.
    let planned = |e| logged(e, &plan.log);
    publish::ensure_dir(job, ctx, dir, &names).map_err(planned)?;
    let (temp, out) = TempFile::create(dir, from.output_ext()).map_err(|e| {
        planned(names.write_failed(format!("create temp in {}: {e}", dir.display())))
    })?;
    let path = temp.path();
    let reopen = || {
        let file = File::open(path)
            .map_err(|e| names.write_failed(format!("open {}: {e}", path.display())))?;
        Ok::<_, GrapeError>(BufReader::with_capacity(
            64 * 1024,
            Cancellable::new(file, cancel),
        ))
    };
    let mut back = reopen().map_err(planned)?;
    let mut writer = BufWriter::with_capacity(64 * 1024, Cancellable::new(out, cancel));
    // 95 % of the source (D26's exact rule).
    let most = before.saturating_mul(19) / 20;
    let wrote = plan.write(&mut reader, &mut writer, &mut back, most, cancel, progress);
    let mut wrote = wrote.map_err(|Halt { why, log }| {
        let e = match why {
            Stopped::NotSmaller => {
                let detail = format!(
                    "{}: the output passed 95 % of {before} bytes",
                    src.display()
                );
                publish::or_cancelled(cancel, src, names.not_smaller(false, detail))
            }
            Stopped::Wrong(why) => {
                let e = names.encode_failed(format!("check {}: {why}", path.display()));
                publish::or_cancelled(cancel, src, e)
            }
            Stopped::Failed(e) => failed(e, Some(path)),
        };
        logged(e, &log)
    })?;
    // The writer's log holds the plan's, and what it took back.
    let log = std::mem::take(&mut wrote.log);
    let logged = |e| logged(e, &log);
    // Cut what an IFD taken back left past the end.
    strips::close(writer, Some(wrote.len), path, &names, src, cancel).map_err(logged)?;
    drop(reader);
    progress(0.97);
    let after = wrote.len;
    if after.saturating_mul(20) > before.saturating_mul(19) {
        let detail = format!("{after} bytes from {before} in {}", src.display());
        return Err(logged(names.not_smaller(false, detail)));
    }
    // The plan is let go of first: the check holds the output's model.
    drop(plan);
    drop(back);
    if let Err(why) = wrote.check(&mut reopen().map_err(logged)?) {
        let e = names.encode_failed(format!("check {}: {why}", path.display()));
        return Err(logged(publish::or_cancelled(cancel, src, e)));
    }
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let mut outcome = publish::finalize(&temp, job, src, dest, dir, &names).map_err(logged)?;
    progress(1.0);
    outcome.notes.push(Note::Compressed { before, after });
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;
    use std::path::PathBuf;

    use super::super::read::{read_value, MISSIZED};
    use super::super::tests::{
        gray, miscounted, picture, set_block, set_entry, Builder, Page, B, V,
    };
    use super::*;

    /// A file the test removes when it's done with it.
    struct Temp(PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn planned(file: &[u8]) -> Result<Planned, Error> {
        Planned::of(&mut Cursor::new(file), file.len() as u64)
    }

    fn plan(file: &[u8]) -> Plan {
        match planned(file).unwrap() {
            Planned::Plan(plan) => plan,
            Planned::Nothing(why) => panic!("nothing to recode: {why}"),
        }
    }

    /// `plan` written from `file` into a real file read back through a
    /// second handle, up to `most` bytes; the output, cut at its size and
    /// checked, and what was written.
    fn written(
        file: &[u8],
        plan: &Plan,
        most: u64,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(f32),
    ) -> Result<(Vec<u8>, Wrote), Halt> {
        let name = format!("grape-tiff-compress-{}.tif", crate::output::unique_suffix());
        let temp = Temp(std::env::temp_dir().join(name));
        let mut w = BufWriter::new(File::create_new(&temp.0).unwrap());
        let mut back = BufReader::new(File::open(&temp.0).unwrap());
        let wrote = plan.write(
            &mut Cursor::new(file),
            &mut w,
            &mut back,
            most,
            cancel,
            progress,
        )?;
        let out = w.into_inner().unwrap();
        out.set_len(wrote.len).unwrap();
        drop(out);
        let bytes = std::fs::read(&temp.0).unwrap();
        wrote.check(&mut Cursor::new(&bytes)).unwrap();
        Ok((bytes, wrote))
    }

    fn compressed(file: &[u8]) -> Vec<u8> {
        let plan = plan(file);
        written(file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {})
            .unwrap()
            .0
    }

    fn read(file: &[u8]) -> Tiff {
        Tiff::read(&mut Cursor::new(file), file.len() as u64).unwrap()
    }

    /// Samples the tiff crate decoded, as little-endian bytes.
    fn le(result: tiff::decoder::DecodingResult) -> Vec<u8> {
        use tiff::decoder::DecodingResult::*;
        match result {
            U8(v) => v,
            U16(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            U32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            F64(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            _ => panic!("another sample type"),
        }
    }

    /// Each image IFD of `file`, pages and SubIFDs, decoded by the tiff
    /// crate ([`decoded_at`]).
    fn decoded(file: &[u8]) -> Vec<(tiff::ColorType, Vec<u8>)> {
        let tiff = read(file);
        tiff.ifds()
            .iter()
            .map(|ifd| decoded_at(file, ifd.at))
            .collect()
    }

    /// The IFD at `at` of `file` decoded by the tiff crate, the header
    /// pointed at it.
    fn decoded_at(file: &[u8], at: u64) -> (tiff::ColorType, Vec<u8>) {
        let layout = read(file).layout;
        let (start, size) = if layout.big { (8, 8) } else { (4, 4) };
        let mut copy = file.to_vec();
        copy[start..start + size].copy_from_slice(&layout.order.bytes(at, size));
        let mut decoder = tiff::decoder::Decoder::new(Cursor::new(&copy)).unwrap();
        let color = decoder.colortype().unwrap();
        (color, le(decoder.read_image().unwrap()))
    }

    /// Each entry's tag and value bytes, but the ones Compress makes or
    /// changes.
    fn values(file: &[u8], entries: &[Entry]) -> Vec<(u16, u16, u64, Vec<u8>)> {
        let mut r = Cursor::new(file);
        let mut all: Vec<_> = entries
            .iter()
            .filter(|e| {
                !matches!(
                    e.tag,
                    259 | 273 | 279 | 288 | 289 | 317 | 324 | 325 | 330 | 34665 | 34853 | 40965
                )
            })
            .map(|e| {
                let value = read_value(&mut r, e, 1 << 20).unwrap().unwrap();
                (e.tag, e.kind, e.count, value)
            })
            .collect();
        all.sort();
        all
    }

    /// A smooth picture's samples, `n` bytes of them.
    fn smooth(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| ((i / 3) % 200 + (i % 3) * 20) as u8)
            .collect()
    }

    /// Five pages from the tiff crate: LZW RGB in strips of 7 rows, Deflate
    /// 16-bit gray differenced, PackBits float, uncompressed RGBA and LZW
    /// gray.
    fn five_pages(big: bool) -> Vec<u8> {
        use tiff::encoder::{colortype, Compression, DeflateLevel, TiffEncoder, TiffKind};
        use tiff::tags::Predictor as P;
        fn pages<K: TiffKind>(e: TiffEncoder<&mut Cursor<Vec<u8>>, K>) {
            let mut e = e.with_compression(Compression::Lzw);
            let mut rgb = e.new_image::<colortype::RGB8>(40, 30).unwrap();
            rgb.rows_per_strip(7).unwrap();
            rgb.write_data(&smooth(40 * 30 * 3)).unwrap();
            let mut e = e
                .with_compression(Compression::Deflate(DeflateLevel::Fast))
                .with_predictor(P::Horizontal);
            let gray16: Vec<u16> = (0..40 * 30).map(|i| (i * 37 % 4_000) as u16).collect();
            e.write_image::<colortype::Gray16>(40, 30, &gray16).unwrap();
            let mut e = e
                .with_compression(Compression::Packbits)
                .with_predictor(P::None);
            let float: Vec<f32> = (0..40 * 30).map(|i| (i / 40) as f32 / 3.0).collect();
            e.write_image::<colortype::Gray32Float>(40, 30, &float)
                .unwrap();
            let mut e = e.with_compression(Compression::Uncompressed);
            e.write_image::<colortype::RGBA8>(40, 30, &smooth(40 * 30 * 4))
                .unwrap();
            let mut e = e.with_compression(Compression::Lzw);
            e.write_image::<colortype::Gray8>(40, 30, &smooth(40 * 30))
                .unwrap();
        }
        let mut out = Cursor::new(Vec::new());
        if big {
            pages(TiffEncoder::new_big(&mut out).unwrap());
        } else {
            pages(TiffEncoder::new(&mut out).unwrap());
        }
        out.into_inner()
    }

    #[test]
    fn every_page_decodes_to_the_same_samples_deflated() {
        for big in [false, true] {
            let file = five_pages(big);
            let out = compressed(&file);
            assert_eq!(decoded(&out), decoded(&file));
            let (a, b) = (read(&file), read(&out));
            assert_eq!((b.layout.big, b.layout.order), (big, Order::Little));
            let o = b.layout.order;
            for (pa, pb) in a.pages.iter().zip(&b.pages) {
                assert_eq!(pb.uint(259, o), Some(8));
                assert_eq!(values(&file, &pa.entries), values(&out, &pb.entries));
                assert_eq!(pa.blocks.len(), pb.blocks.len());
            }
            // Differenced: the 8 and 16-bit pages; the 16-bit one was
            // already. The float one isn't.
            let predictors: Vec<_> = b.pages.iter().map(|p| p.uint(317, o)).collect();
            assert_eq!(predictors, [Some(2), Some(2), Some(1), Some(2), Some(2)]);
            assert!(out.len() < file.len());
        }
    }

    /// A gray page `w` × `h` of `bits` bits in `order`, uncompressed, in
    /// strips of `rows` rows, from `samples`.
    fn page(w: u32, h: u32, rows: u32, bits: u16, samples: &[u8]) -> Page {
        let strip = (w * u32::from(bits) / 8 * rows) as usize;
        let mut page = gray(w, h, rows, samples.chunks(strip).map(B::data).collect());
        page.tags[2] = (258, V::Short(vec![bits]));
        page
    }

    #[test]
    fn big_endian_planar_tiled_float_and_subifd_pages_decode_the_same() {
        // 16-bit gray, big-endian, with a SubIFD of 8-bit gray.
        let gray16: Vec<u8> = (0..40 * 30u32)
            .flat_map(|i| ((i * 53 % 3_000) as u16).to_be_bytes())
            .collect();
        let mut first = page(40, 30, 8, 16, &gray16);
        first.subs = vec![vec![page(20, 10, 10, 8, &smooth(200))]];
        // Planar RGB: three planes of two strips each.
        let mut planar = Page {
            tags: vec![
                (256, V::Long(vec![30])),
                (257, V::Long(vec![20])),
                (258, V::Short(vec![8, 8, 8])),
                (262, V::Short(vec![2])),
                (277, V::Short(vec![3])),
                (278, V::Long(vec![12])),
                (284, V::Short(vec![2])),
            ],
            ..Page::default()
        };
        for plane in 0..3u8 {
            let bytes: Vec<u8> = (0..600).map(|i| (i / 30) as u8 * 3 + plane).collect();
            planar.blocks.push(B::Data(bytes[..360].to_vec()));
            planar.blocks.push(B::Data(bytes[360..].to_vec()));
        }
        // Tiles of 16 × 16 over 40 × 20, the edge ones full size.
        let tiled = Page {
            tags: vec![
                (256, V::Long(vec![40])),
                (257, V::Long(vec![20])),
                (258, V::Short(vec![8])),
                (262, V::Short(vec![1])),
                (322, V::Short(vec![16])),
                (323, V::Short(vec![16])),
            ],
            blocks: (0..6)
                .map(|t| B::Data(smooth(256 + t)[t..].to_vec()))
                .collect(),
            tiled: true,
            ..Page::default()
        };
        // 32-bit float, big-endian.
        let floats: Vec<u8> = (0..20 * 10)
            .flat_map(|i| (i as f32 / 7.0).to_be_bytes())
            .collect();
        let mut float = page(20, 10, 10, 32, &floats);
        float.tags.push((339, V::Short(vec![3])));
        let file = Builder::new(b'M', false, vec![first, planar, tiled, float]).build();
        let out = compressed(&file);
        assert_eq!(decoded(&out), decoded(&file));
        let back = read(&out);
        let o = back.layout.order;
        assert_eq!(o, Order::Big);
        let predictors: Vec<_> = back.ifds().iter().map(|i| i.uint(317, o)).collect();
        assert_eq!(predictors, [Some(2), Some(2), Some(2), Some(2), None]);
        assert!(back.ifds().iter().all(|ifd| ifd.uint(259, o) == Some(8)));
        // Tiles and planes stay as they were.
        assert_eq!(back.pages[1].blocks.len(), 6);
        assert!(back.pages[2].tiled);
        assert_eq!(back.pages[2].uint(322, o), Some(16));
    }

    #[test]
    fn a_stray_predictor_on_none_or_packbits_is_set_again() {
        // libtiff reads a predictor only inside LZW and Deflate: these
        // samples are the picture as they are.
        let samples = smooth(32 * 8);
        let packbits: Vec<u8> = samples
            .chunks(128)
            .flat_map(|run| [&[run.len() as u8 - 1][..], run].concat())
            .collect();
        for (compression, data) in [(1, samples.clone()), (32773, packbits)] {
            let mut gray = page(32, 8, 8, 8, &samples);
            gray.blocks = vec![B::Data(data.clone())];
            gray.tags.push((259, V::Short(vec![compression])));
            gray.tags.push((317, V::Short(vec![2])));
            let mut float = page(16, 4, 4, 32, &samples);
            float.blocks = vec![B::Data(data)];
            float.tags[2] = (258, V::Short(vec![32]));
            float.tags.extend([
                (259, V::Short(vec![compression])),
                (317, V::Short(vec![2])),
                (339, V::Short(vec![3])),
            ]);
            let file = Builder::new(b'I', false, vec![gray, float]).build();
            let out = compressed(&file);
            let pages = decoded(&out);
            assert_eq!(pages[0].1, samples, "{compression}");
            assert_eq!(pages[1].1, samples, "{compression}");
            let back = read(&out);
            let o = back.layout.order;
            assert_eq!(back.pages[0].uint(317, o), Some(2));
            assert_eq!(back.pages[1].uint(317, o), Some(1));
        }
    }

    /// A gray 8 × 8 page of one strip of `data`, `compression`.
    fn coded(compression: u16, data: Vec<u8>) -> Page {
        let mut page = gray(8, 8, 8, vec![B::Data(data)]);
        page.tags.push((259, V::Short(vec![compression])));
        page
    }

    #[test]
    fn fill_order_2_jpeg_fax_and_other_codecs_are_copied_as_they_are() {
        let mut reversed = coded(1, smooth(64));
        reversed.tags.push((266, V::Short(vec![2])));
        let copied = vec![
            reversed,
            coded(7, b"\xFF\xD8 not really a JPEG \xFF\xD9".to_vec()),
            coded(4, b"fax data".to_vec()),
            coded(50000, b"ZSTD or anything".to_vec()),
        ];
        let mut pages = copied.clone();
        pages.push(coded(1, smooth(64)));
        let file = Builder::new(b'M', true, pages).build();
        let out = compressed(&file);
        let (a, b) = (read(&file), read(&out));
        let o = b.layout.order;
        for (pa, pb) in a.pages.iter().zip(&b.pages).take(copied.len()) {
            assert_eq!(values(&file, &pa.entries), values(&out, &pb.entries));
            assert_eq!(pb.uint(259, o), pa.uint(259, o));
            let bytes = |f: &[u8], b: &Block| f[b.offset as usize..][..b.len as usize].to_vec();
            assert_eq!(bytes(&file, &pa.blocks[0]), bytes(&out, &pb.blocks[0]));
        }
        assert_eq!(b.pages[4].uint(259, o), Some(8));
        // A file of nothing but those is nothing to recode.
        let file = Builder::new(b'I', false, copied).build();
        let Planned::Nothing(why) = planned(&file).unwrap() else {
            panic!("planned")
        };
        assert!(
            why.contains("FillOrder") && why.contains("compression is 7"),
            "{why}"
        );
    }

    #[test]
    fn an_ifd_with_a_block_it_cant_decode_is_taken_back_and_copied() {
        let lzw = |bytes: &[u8]| {
            weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
                .encode(bytes)
                .unwrap()
        };
        let picture = smooth(64 * 30);
        let strips: Vec<B> = picture.chunks(64 * 10).map(|s| B::Data(lzw(s))).collect();
        for bad in [
            // Junk, LZW cut short, and old-style LZW, as the last strip.
            vec![0xFF; 40],
            lzw(&picture[..64 * 10])[..20].to_vec(),
            vec![0x00, 0x01, 0x20, 0x40],
        ] {
            let mut broken = gray(64, 30, 10, strips.clone());
            broken.blocks[2] = B::Data(bad);
            broken.tags.push((259, V::Short(vec![5])));
            let mut fine = gray(64, 30, 10, strips.clone());
            fine.tags.push((259, V::Short(vec![5])));
            let file = Builder::new(b'I', false, vec![broken, fine]).build();
            let plan = plan(&file);
            let (out, wrote) =
                written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap();
            let (a, b) = (read(&file), read(&out));
            let o = b.layout.order;
            // The first page is the source's, block for block; the second
            // is deflated, right after it.
            assert_eq!(b.pages[0].uint(259, o), Some(5));
            for (ba, bb) in a.pages[0].blocks.iter().zip(&b.pages[0].blocks) {
                let bytes = |f: &[u8], b: &Block| f[b.offset as usize..][..b.len as usize].to_vec();
                assert_eq!(bytes(&file, ba), bytes(&out, bb));
            }
            assert_eq!(b.pages[1].uint(259, o), Some(8));
            assert_eq!(decoded_at(&out, b.pages[1].at).1, picture);
            assert!(
                wrote.log.to_string().contains("IFD 0 copied as it is"),
                "{}",
                wrote.log
            );
            assert_eq!(wrote.len, out.len() as u64);
        }
    }

    #[test]
    fn exif_gps_interop_and_every_other_tag_are_moved_with_their_values() {
        let mut first = page(32, 8, 4, 8, &smooth(256));
        first.tags.extend([
            (270, V::Ascii("a description of the scan")),
            (274, V::Short(vec![6])),
            (305, V::Ascii("Scanner 2000")),
            (700, V::Bytes(1, b"<x:xmpmeta>the XMP</x:xmpmeta>".to_vec())),
            (
                33723,
                V::Bytes(4, b"\x1c\x02\x05\0\x08IPTC here\0\0\0".to_vec()),
            ),
            (
                34377,
                V::Bytes(7, b"8BIM\x03\xED\0\0\0\0\0\x10resolution here!".to_vec()),
            ),
            (34675, V::Bytes(7, vec![9; 300])),
            (288, V::Long(vec![9])),
            (289, V::Long(vec![1])),
        ]);
        first.exif = Some(vec![
            (0x9003, V::Ascii("2024:05:01 10:20:30")),
            (0x927C, V::Bytes(7, b"a maker note".to_vec())),
        ]);
        first.interop = Some(vec![(1, V::Ascii("R98"))]);
        first.gps = Some(vec![
            (1, V::Ascii("N")),
            (2, V::Rational(vec![(48, 1), (8, 1), (0, 1)])),
        ]);
        for (order, big) in [(b'I', false), (b'M', true)] {
            let file = Builder::new(order, big, vec![first.clone()]).build();
            let out = compressed(&file);
            let (a, b) = (read(&file), read(&out));
            let (pa, pb) = (&a.pages[0], &b.pages[0]);
            assert_eq!(values(&file, &pa.entries), values(&out, &pb.entries));
            assert!(pb.get(288).is_none() && pb.get(289).is_none());
            let (ea, eb) = (pa.exif.as_ref().unwrap(), pb.exif.as_ref().unwrap());
            assert_eq!(values(&file, &ea.entries), values(&out, &eb.entries));
            let interop = |m: &Meta, f: &[u8]| values(f, &m.interop.as_ref().unwrap().entries);
            assert_eq!(interop(ea, &file), interop(eb, &out));
            let (ga, gb) = (pa.gps.as_ref().unwrap(), pb.gps.as_ref().unwrap());
            assert_eq!(values(&file, &ga.entries), values(&out, &gb.entries));
            assert_eq!(decoded(&out), decoded(&file));
        }
    }

    #[test]
    fn an_entry_typed_ifd_that_isnt_followed_is_refused() {
        // A GlobalParametersIFD (400) typed IFD, on a page and in an Exif
        // IFD.
        let mut on_page = page(8, 8, 8, 8, &smooth(64));
        on_page
            .tags
            .push((400, V::Bytes(13, 8u32.to_le_bytes().to_vec())));
        let mut in_exif = page(8, 8, 8, 8, &smooth(64));
        in_exif.exif = Some(vec![(0xBEEF, V::Bytes(13, 8u32.to_le_bytes().to_vec()))]);
        for page in [on_page, in_exif] {
            let file = Builder::new(b'I', false, vec![page.clone()]).build();
            let Err(Error::Refused(why)) = planned(&file) else {
                panic!("not refused")
            };
            assert_eq!(why, UNMOVABLE);
            // With nothing to recode, there's nothing to say about it.
            let mut jpeg = page;
            jpeg.tags.push((259, V::Short(vec![7])));
            let file = Builder::new(b'I', false, vec![jpeg]).build();
            assert!(matches!(planned(&file), Ok(Planned::Nothing(_))));
        }
    }

    #[test]
    fn a_block_listed_twice_is_written_once_for_each_size_it_decodes_to() {
        // Strips 0 and 1 are the same 2-row strip; so is the last, which
        // has 1 row.
        let strip = smooth(32 * 2);
        let gray = gray(32, 5, 2, vec![B::Data(strip), B::Same(0), B::Same(0)]);
        let mut sparse = Page {
            tags: vec![
                (256, V::Long(vec![32])),
                (257, V::Long(vec![32])),
                (258, V::Short(vec![8])),
                (262, V::Short(vec![1])),
                (322, V::Short(vec![16])),
                (323, V::Short(vec![16])),
            ],
            blocks: vec![B::Data(smooth(256)), B::Empty, B::Empty, B::Same(0)],
            tiled: true,
            ..Page::default()
        };
        sparse.tags.push((42113, V::Ascii("0")));
        let file = Builder::new(b'I', false, vec![gray, sparse]).build();
        let out = compressed(&file);
        let back = read(&out);
        let strips = &back.pages[0].blocks;
        assert_eq!(strips[0], strips[1]);
        assert_ne!(strips[0], strips[2]);
        let tiles = &back.pages[1].blocks;
        assert_eq!(tiles[1], Block { offset: 0, len: 0 });
        assert_eq!(tiles[2], Block { offset: 0, len: 0 });
        assert_eq!(tiles[0], tiles[3]);
        // The decoded strips are the source's, the last cut at its row.
        let mut decoder = tiff::decoder::Decoder::new(Cursor::new(&out)).unwrap();
        assert_eq!(
            le(decoder.read_image().unwrap()),
            [&smooth(64)[..], &smooth(64), &smooth(32)].concat()
        );
    }

    #[test]
    fn byte_counts_libtiff_ignores_are_recoded_as_libtiff_reads_them() {
        for (strips, counts) in [
            (1, vec![0]),
            (1, vec![15_000]),
            (10, [3_000, 1_500].repeat(5)),
        ] {
            let out = compressed(&miscounted(strips, 1, counts.clone()));
            let back = read(&out);
            assert_eq!(back.pages[0].uint(259, back.layout.order), Some(8));
            assert_eq!(
                decoded_at(&out, back.pages[0].at).1,
                picture(),
                "{counts:?}"
            );
        }
        let file = miscounted(2, 1, vec![15_000, 7_500]);
        assert!(matches!(planned(&file), Err(Error::Refused(why)) if why == MISSIZED));
    }

    #[test]
    fn an_ifd_with_empty_tiles_is_copied_as_it_is() {
        // GDAL's sparse tiles: 0 bytes at 0. A page of them stays
        // uncompressed, the empty ones empty; the page after is recoded.
        let tiled = Page {
            tags: vec![
                (256, V::Long(vec![32])),
                (257, V::Long(vec![32])),
                (258, V::Short(vec![8])),
                (262, V::Short(vec![1])),
                (322, V::Short(vec![16])),
                (323, V::Short(vec![16])),
            ],
            blocks: vec![
                B::Data(smooth(256)),
                B::Empty,
                B::Empty,
                B::Data(smooth(256)),
            ],
            tiled: true,
            ..Page::default()
        };
        let file = Builder::new(b'I', false, vec![tiled, page(32, 8, 8, 8, &smooth(256))]).build();
        let plan = plan(&file);
        let (out, wrote) =
            written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap();
        let (a, b) = (read(&file), read(&out));
        let o = b.layout.order;
        assert_eq!(b.pages[0].uint(259, o), None);
        let empty = Block { offset: 0, len: 0 };
        assert_eq!(b.pages[0].blocks[1..3], [empty, empty]);
        let bytes = |f: &[u8], b: &Block| f[b.offset as usize..][..b.len as usize].to_vec();
        for (ba, bb) in a.pages[0].blocks.iter().zip(&b.pages[0].blocks) {
            assert_eq!(bytes(&file, ba), bytes(&out, bb));
        }
        assert_eq!(b.pages[1].uint(259, o), Some(8));
        assert!(
            wrote
                .log
                .to_string()
                .contains("IFD 0 copied as it is: it has empty"),
            "{}",
            wrote.log
        );
    }

    /// LZW of `bytes`, as TIFF writes it.
    fn lzw(bytes: &[u8]) -> Vec<u8> {
        weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
            .encode(bytes)
            .unwrap()
    }

    /// Samples no codec shrinks much, `n` bytes of them.
    fn noise(n: usize) -> Vec<u8> {
        let mut x: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    /// `file` with block `n` of page `to` pointing at block `m` of page
    /// `from`.
    fn shared(mut file: Vec<u8>, (from, m): (usize, usize), (to, n): (usize, usize)) -> Vec<u8> {
        let tiff = read(&file);
        let block = tiff.pages[from].blocks[m];
        set_block(&mut file, tiff.pages[to].at, n, block.offset, block.len);
        file
    }

    #[test]
    fn a_block_shared_by_strips_of_other_sizes_is_counted_once() {
        // 200 × 950 in strips of 100 rows, all ten listing one block of
        // 20,000 bytes: the last strip, of 50 rows, is recoded from it
        // too, and it was counted twice against a file of 20,190 bytes.
        let strips = [vec![B::Data(smooth(20_000))], vec![B::Same(0); 9]].concat();
        let file = Builder::new(b'I', false, vec![gray(200, 950, 100, strips)]).build();
        assert!(file.len() < 2 * 20_000);
        let out = compressed(&file);
        let want: Vec<u8> = [smooth(20_000).repeat(9), smooth(10_000)].concat();
        assert_eq!(decoded_at(&out, read(&out).pages[0].at).1, want);
        // An LZW strip shared by a 500 × 400 page and two 500 × 200 ones:
        // written once for each height.
        let rows = noise(500 * 400);
        let page = |h: u32, data: Vec<u8>| {
            let mut page = gray(500, h, h, vec![B::Data(data)]);
            page.tags.push((259, V::Short(vec![5])));
            page
        };
        let pages = vec![
            page(400, lzw(&rows)),
            page(200, vec![0]),
            page(200, vec![0]),
        ];
        let file = Builder::new(b'I', false, pages).build();
        let file = shared(shared(file, (0, 0), (1, 0)), (0, 0), (2, 0));
        let out = compressed(&file);
        let back = read(&out);
        let at = |n: usize| back.pages[n].blocks[0];
        assert_eq!(at(1), at(2));
        assert_ne!(at(0), at(1));
        assert_eq!(decoded_at(&out, back.pages[0].at).1, rows);
        assert_eq!(decoded_at(&out, back.pages[2].at).1, rows[..500 * 200]);
    }

    #[test]
    fn a_shared_block_of_an_ifd_taken_back_is_counted_once() {
        // Page A lists X and a broken strip, page B X and a good one; X is
        // most of the file. Either way round, A is copied as it is and B
        // recoded: both strips deflate well, so B's recode writes less than
        // its copy would, even once X is copied.
        let x = smooth(64 * 100);
        let good: Vec<u8> = smooth(64 * 100).into_iter().rev().collect();
        for a_first in [true, false] {
            let page = |first: Vec<u8>, second: Vec<u8>| {
                let mut page = gray(64, 200, 100, vec![B::Data(first), B::Data(second)]);
                page.tags.push((259, V::Short(vec![5])));
                page
            };
            // B's first strip is A's X, pointed at once the file is built.
            let (a, b) = (page(lzw(&x), vec![0xFF; 40]), page(vec![0], lzw(&good)));
            let (pages, a_at) = if a_first {
                (vec![a, b], 0)
            } else {
                (vec![b, a], 1)
            };
            let file = Builder::new(b'I', false, pages).build();
            let file = shared(file, (a_at, 0), (1 - a_at, 0));
            let plan = plan(&file);
            let (out, wrote) =
                written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap();
            let back = read(&out);
            let o = back.layout.order;
            assert_eq!(back.pages[a_at].uint(259, o), Some(5));
            assert_eq!(back.pages[1 - a_at].uint(259, o), Some(8));
            let b_page = decoded_at(&out, back.pages[1 - a_at].at).1;
            assert_eq!(b_page, [&x[..], &good].concat());
            let log = wrote.log.to_string();
            assert!(
                log.contains(&format!("IFD {a_at} copied as it is")),
                "{log}"
            );
        }
    }

    #[test]
    fn an_ifd_that_grows_recoded_is_taken_back_and_copied() {
        // Noise, uncompressed, deflates to a few bytes more than it is;
        // the smooth page after it shrinks.
        let file = Builder::new(
            b'M',
            false,
            vec![
                page(64, 64, 16, 8, &noise(64 * 64)),
                page(64, 64, 16, 8, &smooth(64 * 64)),
            ],
        )
        .build();
        let plan = plan(&file);
        let (out, wrote) =
            written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap();
        let (a, b) = (read(&file), read(&out));
        let o = b.layout.order;
        assert_eq!(b.pages[0].uint(259, o), None);
        let bytes = |f: &[u8], b: &Block| f[b.offset as usize..][..b.len as usize].to_vec();
        for (ba, bb) in a.pages[0].blocks.iter().zip(&b.pages[0].blocks) {
            assert_eq!(bytes(&file, ba), bytes(&out, bb));
        }
        assert_eq!(b.pages[1].uint(259, o), Some(8));
        assert_eq!(decoded(&out), decoded(&file));
        let log = wrote.log.to_string();
        assert!(
            log.contains("IFD 0 copied as it is: recoded, its blocks"),
            "{log}"
        );
    }

    #[test]
    fn an_ifd_that_passes_the_most_recoded_is_copied_when_that_fits() {
        // A smooth page, then noise, which grows recoded: with the most
        // where the noise ends copied, its recode passes it, and the copy
        // takes its place.
        let file = Builder::new(
            b'I',
            false,
            vec![
                page(64, 64, 16, 8, &smooth(64 * 64)),
                page(64, 64, 16, 8, &noise(64 * 64)),
            ],
        )
        .build();
        let plan = plan(&file);
        let (whole, _) = written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap();
        let blocks = &read(&whole).pages[1].blocks;
        let most = blocks.iter().map(|b| b.offset + b.len).max().unwrap();
        let (out, wrote) = written(&file, &plan, most, &CancelToken::new(), &mut |_| {}).unwrap();
        assert_eq!(out, whole);
        let log = wrote.log.to_string();
        assert!(
            log.contains(&format!("IFD 1 copied as it is: {PASSED}")),
            "{log}"
        );
        // A byte less, and the copy doesn't fit either.
        let stopped = written(&file, &plan, most - 1, &CancelToken::new(), &mut |_| {});
        assert!(
            matches!(
                stopped,
                Err(Halt {
                    why: Stopped::NotSmaller,
                    ..
                })
            ),
            "{stopped:?}"
        );
    }

    #[test]
    fn an_ifd_is_copied_only_when_that_writes_less_than_its_recode() {
        // Pages, and the blocks one points at another's: (page, block).
        type Share = ((usize, usize), (usize, usize));
        let plan_and_write = |pages: Vec<Page>, shares: &[Share]| {
            let mut file = Builder::new(b'I', false, pages).build();
            for &(from, to) in shares {
                file = shared(file, from, to);
            }
            let plan = plan(&file);
            let (out, wrote) =
                written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap();
            (read(&out), wrote.log.to_string(), out)
        };
        // Page B lists page A's strip, recoded already, and noise, which
        // grows recoded: a copy of B would write A's strip again too, so B
        // stays recoded.
        let x = smooth(64 * 64);
        let y = noise(64 * 64);
        let a = page(64, 64, 64, 8, &x);
        let b = gray(
            64,
            128,
            64,
            vec![B::Data(vec![0; 64 * 64]), B::Data(y.clone())],
        );
        let (back, log, out) = plan_and_write(vec![a, b], &[((0, 0), (1, 0))]);
        let o = back.layout.order;
        assert_eq!(back.pages[1].uint(259, o), Some(8), "{log}");
        assert_eq!(back.pages[1].blocks[0], back.pages[0].blocks[0]);
        assert_eq!(
            decoded_at(&out, back.pages[1].at).1,
            [x.clone(), y].concat()
        );
        // Page B lists page A's strip, which A copied as it is (FillOrder
        // 2): its copy writes nothing, so B is copied, and points there.
        let mut a = page(64, 64, 64, 8, &x);
        a.tags.push((266, V::Short(vec![2])));
        let b = gray(64, 64, 64, vec![B::Data(vec![0; 64 * 64])]);
        let (back, log, _) = plan_and_write(vec![a, b], &[((0, 0), (1, 0))]);
        assert_eq!(back.pages[1].uint(259, o), None);
        assert_eq!(back.pages[1].blocks[0], back.pages[0].blocks[0]);
        assert!(
            log.contains("IFD 1 copied as it is: recoded, its blocks came to"),
            "{log}"
        );
        // One block of noise listed twice, recoded whole and cut at the
        // last strip's 48 rows: twice the noise, where a copy writes it
        // once.
        let x = [noise(64 * 48), vec![0; 64 * 16]].concat();
        let c = gray(64, 112, 64, vec![B::Data(x), B::Same(0)]);
        let (back, log, _) = plan_and_write(vec![c], &[]);
        assert_eq!(back.pages[0].uint(259, o), None);
        let blocks = &back.pages[0].blocks;
        assert_eq!(blocks[0], blocks[1]);
        assert!(
            log.contains("IFD 0 copied as it is: recoded, its blocks came to"),
            "{log}"
        );
    }

    #[test]
    fn the_level_goes_down_as_the_bytes_to_recode_go_up() {
        let mib = 1 << 20;
        for (bytes, want) in [
            (0, 9),
            (16 * mib, 9),
            (16 * mib + 1, 6),
            (256 * mib, 6),
            (256 * mib + 1, 3),
            (u64::MAX, 3),
        ] {
            assert_eq!(level(bytes), want, "{bytes}");
        }
        // The decoded bytes of what's recoded, not the bytes of what's
        // copied as it is: 17 MiB of JPEG data, then 64 × 64 samples.
        let jpeg = coded(7, vec![0xD8; 17 << 20]);
        let file = Builder::new(
            b'I',
            false,
            vec![jpeg, page(64, 64, 16, 8, &smooth(64 * 64))],
        )
        .build();
        let plan = plan(&file);
        assert_eq!((plan.work, plan.level), ((17 << 20) + 64 * 64, 9));
    }

    #[test]
    fn a_block_listed_more_than_once_counts_once_for_each_way_it_is_written() {
        // 2,000 × 9,500 in strips of 1,000 rows, all ten listing one block
        // of 2,000,000 bytes: recoded whole, and cut at the last strip's
        // 500 rows, 3 MB in all (counted once a strip, it came to 19 MB, and
        // level 6).
        let strips = [vec![B::Data(smooth(2_000_000))], vec![B::Same(0); 9]].concat();
        let file = Builder::new(b'I', false, vec![gray(2_000, 9_500, 1_000, strips)]).build();
        let strips = plan(&file);
        assert_eq!((strips.work, strips.level), (3_000_000, 9));
        // 17 LZW pages of one 1 MiB strip, and two JPEG pages of one block,
        // each a block that every page lists.
        let picture = smooth(1 << 20);
        let lzw_page = |data: Vec<u8>| {
            let mut page = gray(1_024, 1_024, 1_024, vec![B::Data(data)]);
            page.tags.push((259, V::Short(vec![5])));
            page
        };
        let mut pages = vec![lzw_page(lzw(&picture))];
        pages.extend((0..16).map(|_| lzw_page(vec![0])));
        pages.extend([coded(7, vec![0xD8; 300]), coded(7, vec![0])]);
        let mut file = Builder::new(b'I', false, pages).build();
        for to in 1..17 {
            file = shared(file, (0, 0), (to, 0));
        }
        let file = shared(file, (17, 0), (18, 0));
        let pages = plan(&file);
        assert_eq!((pages.work, pages.level), ((1 << 20) + 300, 9));
    }

    #[test]
    fn the_output_stops_once_it_passes_the_most() {
        let file = five_pages(false);
        let plan = plan(&file);
        let result = written(&file, &plan, 300, &CancelToken::new(), &mut |_| {});
        assert!(
            matches!(
                result,
                Err(Halt {
                    why: Stopped::NotSmaller,
                    ..
                })
            ),
            "{result:?}"
        );
        // At its own size, it's written.
        let (out, _) = written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap();
        let most = out.len() as u64;
        assert!(written(&file, &plan, most, &CancelToken::new(), &mut |_| {}).is_ok());
    }

    #[test]
    fn whatever_stops_the_writer_hands_back_the_log() {
        // A page of JPEG data, copied as it is (said in the log), then
        // one that's recoded.
        let file = Builder::new(
            b'I',
            false,
            vec![
                coded(7, vec![0xD8; 64]),
                page(64, 64, 16, 8, &smooth(64 * 64)),
            ],
        )
        .build();
        let mut plan = plan(&file);
        let said = |halt: &Halt| halt.log.to_string().contains("IFD 0 copied as it is");
        // Past the most, a block read back wrong, and a cancel.
        let stopped = written(&file, &plan, 100, &CancelToken::new(), &mut |_| {}).unwrap_err();
        assert!(matches!(stopped.why, Stopped::NotSmaller) && said(&stopped));
        let cancel = CancelToken::new();
        let stopped =
            written(&file, &plan, u64::MAX, &cancel, &mut |_| cancel.cancel()).unwrap_err();
        assert!(matches!(stopped.why, Stopped::Failed(_)) && said(&stopped));
        plan.broken = true;
        let stopped =
            written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap_err();
        assert!(matches!(stopped.why, Stopped::Wrong(_)) && said(&stopped));
    }

    #[test]
    fn a_broken_encoder_is_caught_reading_back() {
        let file = five_pages(true);
        let mut plan = plan(&file);
        plan.broken = true;
        let result = written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {});
        let Err(Halt {
            why: Stopped::Wrong(why),
            ..
        }) = result
        else {
            panic!("{result:?}")
        };
        assert!(why.contains("reads back wrong"), "{why}");
    }

    #[test]
    fn a_cancel_stops_within_a_block_and_progress_rises() {
        // One 4 MiB strip.
        let file = Builder::new(
            b'I',
            false,
            vec![page(2_048, 2_048, 2_048, 8, &smooth(1 << 22))],
        )
        .build();
        let plan = plan(&file);
        let cancel = CancelToken::new();
        let mut seen = Vec::new();
        let result = written(&file, &plan, u64::MAX, &cancel, &mut |p| {
            seen.push(p);
            if seen.len() == 3 {
                cancel.cancel();
            }
        });
        assert!(
            matches!(
                result,
                Err(Halt {
                    why: Stopped::Failed(Error::Write(_)),
                    ..
                })
            ),
            "{result:?}"
        );
        // Cancelled 3 steps into 16.
        assert_eq!(seen.len(), 3);
        assert!(
            seen.windows(2).all(|w| w[0] < w[1]) && seen[2] < 0.2,
            "{seen:?}"
        );
        // Uncancelled, it reports each step up to 0.95.
        let mut seen = Vec::new();
        written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |p| {
            seen.push(p)
        })
        .unwrap();
        assert_eq!(seen.len(), 16);
        assert!((seen[15] - 0.95).abs() < 1e-6, "{seen:?}");
    }

    #[test]
    fn the_check_finds_a_block_that_isnt_where_it_was_written() {
        let file = five_pages(false);
        let plan = plan(&file);
        let (mut out, wrote) =
            written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap();
        // The last page's one strip a byte shorter.
        let last = read(&out).pages[4].clone();
        let strip = last.blocks[0];
        set_entry(&mut out, last.at, 279, 1, strip.len - 1);
        let why = wrote.check(&mut Cursor::new(&out)).unwrap_err();
        assert!(why.contains("aren't where"), "{why}");
        // One IFD fewer.
        let mut fewer = out.clone();
        let fourth = read(&out).pages[3].at;
        let next = fourth + 2 + 12 * read(&out).pages[3].entries.len() as u64;
        fewer[next as usize..][..4].copy_from_slice(&[0; 4]);
        let why = wrote.check(&mut Cursor::new(&fewer)).unwrap_err();
        assert!(why.contains("4 IFDs, not 5"), "{why}");
    }

    #[test]
    fn an_exif_ifd_that_cant_be_read_goes() {
        let mut first = page(8, 8, 8, 8, &smooth(64));
        first.tags.push((34665, V::Long(vec![1 << 30])));
        let file = Builder::new(b'I', false, vec![first]).build();
        let plan = plan(&file);
        let (out, wrote) =
            written(&file, &plan, u64::MAX, &CancelToken::new(), &mut |_| {}).unwrap();
        assert!(read(&out).pages[0].get(34665).is_none());
        assert!(
            wrote.log.to_string().contains("can't be read"),
            "{}",
            wrote.log
        );
        assert!(matches!(
            read(&file).pages[0].exif,
            Some(Meta { unread: true, .. })
        ));
    }
}
