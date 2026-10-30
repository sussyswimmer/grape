//! A TIFF's model (D28): its header and every IFD the tools keep, read by
//! seeking, never mapped or loaded whole.
//!
//! - **Kinds:** classic (42: 12-byte entries, 4-byte counts and offsets,
//!   values of up to 4 bytes inline) and BigTIFF (43: 20-byte entries,
//!   8-byte counts and offsets, values of up to 8 bytes inline, and the
//!   types LONG8, SLONG8 and IFD8), in either byte order.
//! - **The walk:** the main chain (the pages) in order; each image IFD's
//!   SubIFDs (330), each a chain, at most [`MAX_DEPTH`] deep; and its Exif
//!   (34665) and GPS (34853) IFDs, with the Exif IFD's Interop (40965). An
//!   IFD is read once: a page's `next` that comes back to a page, or that
//!   points outside the file, ends the chain, as libtiff does; an Exif, GPS
//!   or Interop IFD that several IFDs point at is shared, as Pillow,
//!   tifffile and ImageMagick read it; any other IFD reached twice (a
//!   SubIFD, or an IFD that is two kinds of IFD) is [`Error::Broken`]. An
//!   Exif, GPS or Interop IFD that can't be read is kept as such
//!   ([`Meta::unread`]).
//! - **What it holds:** each image IFD's entries, their values inline or as
//!   a place in the file (never byte-swapped, never held: Photoshop types
//!   its IPTC as LONG), and its strips or tiles. A missing byte count is
//!   worked out only for uncompressed data, and so are the counts libtiff
//!   4.5 ignores and works out ([`Walk::recount`]). A block of 0 bytes at
//!   0 stays empty (GDAL's sparse tiles); blocks that are the same bytes
//!   are fine, and any other overlap, between blocks, or with an IFD or a
//!   value out of line (any IFD's), is [`Error::Broken`], or [`MISSIZED`]
//!   when only blocks worked out from the picture overlap
//!   ([`Tiff::check_overlaps`]).
//! - **Tags read as libtiff 4.5 reads them:** every integer tag either
//!   tool decides on, by [`number`], [`per_sample`] and [`pair`] (any
//!   integer type but IFD and IFD8, signed ones not negative, a classic
//!   file's LONG8 and SLONG8 out of line), so that Grape and libtiff see
//!   the same file; where libtiff can't read one, and other readers might
//!   read it otherwise, the file is refused, or Compress copies the IFD.
//! - **Refused:** a DNG, an old-style JPEG, or a Compression libtiff can't
//!   read, in any image IFD: [`Error::Refused`] as soon as the walk reads
//!   that IFD, before its strips or tiles, which an old-style JPEG may not
//!   have; a layout libtiff can't read where Grape needs it to size or
//!   pick the blocks ([`UNREAD_LAYOUT`]); and byte counts
//!   libtiff takes at their word that readers which size data themselves
//!   read past ([`MISSIZED`]): a block of 0 bytes at a nonzero offset, or
//!   an uncompressed one shorter than the picture says.
//! - **Caps:** [`MAX_IFDS`] image IFDs, [`MAX_ENTRIES`] entries in an IFD
//!   and [`MAX_ALL_ENTRIES`] in all (a shared IFD's each time it's
//!   shared), [`MAX_BLOCKS`] blocks, SubIFDs [`MAX_DEPTH`] deep. Over one,
//!   the file is refused ([`Error::TooLarge`], or [`Error::Broken`] for an
//!   IFD's entries), never cut short.

use std::collections::{HashMap, HashSet};

use super::super::exif;
use super::super::strip::{read_at, Error, In};

/// The most image IFDs (pages and SubIFDs) in a file.
pub(crate) const MAX_IFDS: usize = 65_536;

/// The most entries in one IFD; one claiming more is broken.
pub(crate) const MAX_ENTRIES: u64 = 4_096;

/// The most entries in the whole file, so the model stays under about
/// 40 MiB.
pub(crate) const MAX_ALL_ENTRIES: usize = 1 << 20;

/// The most strips and tiles in the whole file (a 64 MiB model).
pub(crate) const MAX_BLOCKS: usize = 4 << 20;

/// How deep SubIFDs nest under a page.
pub(crate) const MAX_DEPTH: usize = 4;

/// The most values a per-sample tag (Compression, BitsPerSample,
/// SampleFormat) is read with ([`per_sample`]): more, and Grape can't read
/// it.
pub(crate) const MAX_SAMPLES: u64 = 1 << 16;

/// The most bytes read in one go when reading an array.
const STEP: usize = 64 * 1024;

/// A TIFF's byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Order {
    Little,
    Big,
}

impl Order {
    /// The unsigned number `b` (1 to 8 bytes) holds in this order.
    pub(crate) fn uint(self, b: &[u8]) -> u64 {
        let digits = |acc: u64, &x: &u8| acc << 8 | u64::from(x);
        match self {
            Order::Little => b.iter().rev().fold(0, digits),
            Order::Big => b.iter().fold(0, digits),
        }
    }

    /// `v` as `size` bytes (2, 4 or 8) in this order.
    pub(crate) fn bytes(self, v: u64, size: usize) -> Vec<u8> {
        let all = match self {
            Order::Little => v.to_le_bytes(),
            Order::Big => v.to_be_bytes(),
        };
        match self {
            Order::Little => all[..size].to_vec(),
            Order::Big => all[8 - size..].to_vec(),
        }
    }
}

/// A TIFF's byte order and kind, and the file's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) order: Order,
    /// BigTIFF rather than classic.
    pub(crate) big: bool,
    /// The file's size in bytes.
    pub(crate) len: u64,
}

impl Layout {
    /// The header's size.
    pub(crate) fn header(self) -> u64 {
        if self.big {
            16
        } else {
            8
        }
    }

    /// The size of an offset, a count in an entry, and an IFD's next.
    pub(crate) fn word(self) -> u64 {
        if self.big {
            8
        } else {
            4
        }
    }

    /// The size of an IFD's entry count.
    pub(crate) fn count_len(self) -> u64 {
        if self.big {
            8
        } else {
            2
        }
    }

    /// The size of an entry.
    pub(crate) fn entry_len(self) -> u64 {
        if self.big {
            20
        } else {
            12
        }
    }

    /// The size of an IFD of `n` entries.
    pub(crate) fn ifd_len(self, n: u64) -> u64 {
        self.count_len() + n * self.entry_len() + self.word()
    }
}

/// The size of one value of field type `kind`, BigTIFF's types included.
pub(crate) fn type_size(kind: u16) -> Option<u64> {
    match kind {
        16..=18 => Some(8),
        _ => exif::type_size(kind).map(|size| size as u64),
    }
}

/// One IFD entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) tag: u16,
    /// The field type (1 BYTE, 2 ASCII, 3 SHORT, …).
    pub(crate) kind: u16,
    pub(crate) count: u64,
    pub(crate) value: Value,
}

/// Where an entry's value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Value {
    /// In the entry itself: the first `len` of `bytes`, in the file's order.
    Inline { bytes: [u8; 8], len: u8 },
    /// `len` bytes at `offset` in the file.
    At { offset: u64, len: u64 },
    /// What can't be read: an unknown type, or bytes past the file's end.
    Unread,
}

/// A strip or a tile: where its bytes are. An empty one is 0 bytes at 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Block {
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

/// An image IFD: a page, or a SubIFD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ifd {
    /// Where it is in the file.
    pub(crate) at: u64,
    /// Its entries, in the file's order.
    pub(crate) entries: Vec<Entry>,
    /// Its Compression (259) as libtiff reads it ([`per_sample`]): 1 when
    /// it's absent.
    pub(crate) compression: u64,
    /// Its strips (273, 279) or tiles (324, 325), in order.
    pub(crate) blocks: Vec<Block>,
    /// Tiles rather than strips.
    pub(crate) tiled: bool,
    /// Its blocks' sizes were worked out from the picture, not read: its
    /// byte counts are missing, or libtiff ignores them ([`Walk::recount`]).
    pub(crate) estimated: bool,
    /// Its SubIFDs (330): each the head of a chain, usually of one.
    pub(crate) subs: Vec<Vec<Ifd>>,
    /// Its Exif IFD (34665).
    pub(crate) exif: Option<Meta>,
    /// Its GPS IFD (34853).
    pub(crate) gps: Option<Meta>,
}

/// An Exif, GPS or Interop IFD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Meta {
    /// Where it is in the file (0 when its pointer can't be read).
    pub(crate) at: u64,
    /// Its entries; none when it couldn't be read.
    pub(crate) entries: Vec<Entry>,
    /// It couldn't be read: its pointer, or the IFD itself, is broken.
    pub(crate) unread: bool,
    /// An Exif IFD's Interop IFD (40965).
    pub(crate) interop: Option<Box<Meta>>,
}

impl Meta {
    fn unread(at: u64) -> Meta {
        Meta {
            at,
            entries: Vec::new(),
            unread: true,
            interop: None,
        }
    }
}

impl Ifd {
    /// Its first entry for `tag`.
    pub(crate) fn get(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// The one unsigned number `tag` holds, if it holds one inline (a BYTE,
    /// SHORT, LONG or LONG8): for the tests, as the tools read every tag
    /// they decide on as libtiff reads it ([`number`]).
    #[cfg(test)]
    pub(crate) fn uint(&self, tag: u16, order: Order) -> Option<u64> {
        self.get(tag).and_then(|e| e.uint(order))
    }
}

impl Entry {
    /// The one unsigned number this holds inline (a BYTE, SHORT, LONG,
    /// LONG8, IFD or IFD8), if so: for the tests ([`Ifd::uint`]).
    #[cfg(test)]
    pub(crate) fn uint(&self, order: Order) -> Option<u64> {
        match (self.kind, self.count, self.value) {
            (1 | 3 | 4 | 13 | 16 | 18, 1, Value::Inline { bytes, len }) => {
                Some(order.uint(&bytes[..usize::from(len)]))
            }
            _ => None,
        }
    }
}

/// A TIFF's model ([module docs](self)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tiff {
    pub(crate) layout: Layout,
    /// The pages, in order.
    pub(crate) pages: Vec<Ifd>,
}

impl Tiff {
    /// Reads the TIFF `r`, `len` bytes long ([module docs](self)).
    pub(crate) fn read(r: &mut dyn In, len: u64) -> Result<Tiff, Error> {
        let mut head = [0; 8];
        read_at(r, 0, &mut head)?;
        let order = match &head[..2] {
            b"II" => Order::Little,
            b"MM" => Order::Big,
            _ => return Err(Error::Broken("no TIFF header".into())),
        };
        let (big, first) = match order.uint(&head[2..4]) {
            42 => (false, order.uint(&head[4..8])),
            // BigTIFF: the offsets' size (8), 0, then the first offset.
            43 if order.uint(&head[4..6]) == 8 && order.uint(&head[6..8]) == 0 => {
                let mut first = [0; 8];
                read_at(r, 8, &mut first)?;
                (true, order.uint(&first))
            }
            _ => return Err(Error::Broken("no TIFF header".into())),
        };
        let layout = Layout { order, big, len };
        let mut walk = Walk {
            r,
            layout,
            seen: HashMap::new(),
            ifds: 0,
            entries: 0,
            blocks: 0,
            samples_left: len,
        };
        let mut pages = Vec::new();
        let mut at_pages = HashSet::new();
        let mut at = first;
        while at != 0 {
            if walk.seen.insert(at, Seen::Image).is_some() {
                // Back to a page: the chain loops, and ends there. Any
                // other IFD is something else's.
                if at_pages.contains(&at) {
                    break;
                }
                return Err(Error::Broken("a page that is another IFD".into()));
            }
            // A page outside the file ends the chain.
            let Some((entries, next)) = walk.raw(at)? else {
                break;
            };
            at_pages.insert(at);
            pages.push(walk.image(at, entries, 0)?);
            at = next;
        }
        if pages.is_empty() {
            return Err(Error::Broken("no IFD in the file".into()));
        }
        let tiff = Tiff { layout, pages };
        tiff.check_overlaps()?;
        Ok(tiff)
    }

    /// Every image IFD: each page, then its SubIFDs, depth first.
    pub(crate) fn ifds(&self) -> Vec<&Ifd> {
        fn add<'a>(ifd: &'a Ifd, all: &mut Vec<&'a Ifd>) {
            all.push(ifd);
            for sub in ifd.subs.iter().flatten() {
                add(sub, all);
            }
        }
        let mut all = Vec::new();
        for page in &self.pages {
            add(page, &mut all);
        }
        all
    }

    /// Every block with bytes, IFD by IFD.
    fn listed(&self) -> impl Iterator<Item = &Block> {
        self.with_bytes(false)
    }

    /// Every block with bytes, IFD by IFD; with `given`, only those of
    /// IFDs whose blocks the file gave the sizes of.
    fn with_bytes(&self, given: bool) -> impl Iterator<Item = &Block> {
        self.ifds()
            .into_iter()
            .filter(move |ifd| !(given && ifd.estimated))
            .flat_map(|ifd| &ifd.blocks)
            .filter(|b| b.len > 0)
    }

    /// The blocks listed more than once, sorted ([`repeats`]): worked out
    /// when asked, so that a model read only to be checked never holds
    /// them.
    pub(crate) fn shared(&self) -> Vec<Block> {
        repeats(|| self.listed())
    }

    /// Fails when two blocks overlap without being the same bytes, an IFD
    /// overlaps another, or a block reaches into an IFD or a value out of
    /// line, of any IFD the walk read (an Exif, GPS or Interop IFD's too):
    /// [`Error::Broken`], or, when only blocks worked out from the picture
    /// do, [`MISSIZED`], as libtiff would read the IFD or the value as
    /// pixels. What the check holds besides the model: 16 bytes for each
    /// IFD and value out of line (the values at most [`MAX_ALL_ENTRIES`]),
    /// and the sorted references [`InOrder`] may make.
    fn check_overlaps(&self) -> Result<(), Error> {
        let layout = self.layout;
        let all = self.ifds();
        let mut ifds: Vec<(u64, u64)> = Vec::new();
        let span = |at: u64, n: usize| (at, at + layout.ifd_len(n as u64));
        for ifd in &all {
            ifds.push(span(ifd.at, ifd.entries.len()));
            ifds.extend(metas(ifd).map(|meta| span(meta.at, meta.entries.len())));
        }
        // An Exif, GPS or Interop IFD that IFDs share is listed for each.
        ifds.sort_unstable();
        ifds.dedup();
        if !ifds.windows(2).all(|w| w[1].0 >= w[0].1) {
            return Err(Error::Broken("IFDs overlap".into()));
        }
        // Where no block may reach: the IFDs, and the values out of line,
        // counted first so the list isn't grown past their number, merged
        // where they meet.
        let mut taken = ifds;
        taken.reserve_exact(all.iter().map(|ifd| values(ifd).count()).sum());
        for ifd in &all {
            taken.extend(values(ifd));
        }
        let taken = merged(taken);
        let clear = |given| clear_of(InOrder::new(|| self.with_bytes(given)).iter(), &taken);
        if clear(false) {
            Ok(())
        } else if all.iter().any(|ifd| ifd.estimated) && clear(true) {
            Err(Error::Refused(MISSIZED.into()))
        } else {
            Err(Error::Broken(
                "blocks reach into each other, an IFD or a value".into(),
            ))
        }
    }
}

/// The Exif, GPS and Interop IFDs of `ifd` the walk could read.
fn metas(ifd: &Ifd) -> impl Iterator<Item = &Meta> {
    [&ifd.exif, &ifd.gps]
        .into_iter()
        .flatten()
        .flat_map(|meta| std::iter::once(meta).chain(meta.interop.as_deref()))
        .filter(|meta| !meta.unread)
}

/// Where each value out of line of `ifd` and its Exif, GPS and Interop
/// IFDs is: its start and end, within the file.
fn values(ifd: &Ifd) -> impl Iterator<Item = (u64, u64)> + '_ {
    let entries = ifd
        .entries
        .iter()
        .chain(metas(ifd).flat_map(|m| &m.entries));
    entries.filter_map(|entry| match entry.value {
        Value::At { offset, len } => Some((offset, offset + len)),
        _ => None,
    })
}

/// `spans` (start and end) in order, those that meet or touch merged: so
/// in order and apart.
fn merged(mut spans: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    spans.sort_unstable();
    spans.dedup_by(|next, last| {
        let meets = next.0 <= last.1;
        if meets {
            last.1 = last.1.max(next.1);
        }
        meets
    });
    spans
}

/// The blocks that `listed` (blocks with bytes, given afresh each call)
/// holds more than once, sorted, so that a writer writes each once.
/// They're counted before they're collected, so the list is held at its
/// size, never grown beside the sorted references.
pub(crate) fn repeats<'a, I>(listed: impl Fn() -> I) -> Vec<Block>
where
    I: Iterator<Item = &'a Block> + 'a,
{
    let blocks = InOrder::new(listed);
    let mut shared = Vec::with_capacity(each_repeat(blocks.iter()).count());
    shared.extend(each_repeat(blocks.iter()).copied());
    shared
}

/// Each block that `blocks` (in the file's order, a repeat in a row)
/// holds more than once, once.
fn each_repeat<'a>(blocks: impl Iterator<Item = &'a Block>) -> impl Iterator<Item = &'a Block> {
    let mut last = None;
    let mut said = None;
    blocks.filter(move |&block| {
        let first = last == Some(block) && said != Some(block);
        if first {
            said = Some(block);
        }
        last = Some(block);
        first
    })
}

/// The blocks `listed` gives, in the file's order, one that's listed more
/// than once that many times in a row: as they're listed when that's
/// their order already, as writers lay them out (Grape's output among
/// them); otherwise through a sorted list of references to them, never a
/// copy.
struct InOrder<'a, L> {
    listed: L,
    /// The references, sorted, when the blocks aren't listed in order.
    sorted: Option<Vec<&'a Block>>,
}

impl<'a, L, I> InOrder<'a, L>
where
    L: Fn() -> I,
    I: Iterator<Item = &'a Block> + 'a,
{
    fn new(listed: L) -> Self {
        let in_order = listed()
            .zip(listed().skip(1))
            .all(|(a, b)| a == b || apart(a, b));
        let sorted = (!in_order).then(|| {
            let mut sorted: Vec<&Block> = Vec::with_capacity(listed().count());
            sorted.extend(listed());
            sorted.sort_unstable();
            sorted
        });
        InOrder { listed, sorted }
    }

    /// The blocks, in the file's order, afresh each call.
    fn iter(&self) -> Box<dyn Iterator<Item = &'a Block> + '_> {
        match &self.sorted {
            Some(sorted) => Box::new(sorted.iter().copied()),
            None => Box::new((self.listed)()),
        }
    }
}

/// Whether block `b` starts where block `a` ends, or after.
fn apart(a: &Block, b: &Block) -> bool {
    b.offset >= a.offset + a.len
}

/// Whether the blocks `blocks` (in the file's order, a repeat in a row)
/// are apart from each other but for repeats, and none reaches into an
/// IFD of `ifds` (in order, apart).
fn clear_of<'a>(blocks: impl Iterator<Item = &'a Block>, ifds: &[(u64, u64)]) -> bool {
    let mut ifds = ifds.iter().peekable();
    let mut last: Option<&Block> = None;
    for block in blocks {
        match last {
            Some(last) if last == block => continue,
            Some(last) if !apart(last, block) => return false,
            _ => last = Some(block),
        }
        // An IFD that ends before this block can't meet a later one.
        while ifds.next_if(|ifd| ifd.1 <= block.offset).is_some() {}
        if ifds
            .peek()
            .is_some_and(|ifd| ifd.0 < block.offset + block.len)
        {
            return false;
        }
    }
    true
}

/// Why a file with a Compression libtiff can't read is refused: whether
/// its data is compressed, which decides how its blocks are sized and
/// checked, can't be told.
pub(crate) const UNREAD_COMPRESSION: &str =
    "says how it's compressed in a way Grape can't read yet.";

/// The image IFD `ifd`'s Compression, given as libtiff reads it
/// (`compression`, `None` where it can't), or why neither TIFF tool can
/// change a file with it: the end of a sentence that starts with its name.
fn admitted(ifd: &Ifd, compression: Option<u64>) -> Result<u64, &'static str> {
    if ifd.get(50706).is_some() {
        Err("is a camera raw (DNG) file, which Grape can't change yet.")
    } else if compression == Some(6) || ifd.get(513).is_some() || ifd.get(514).is_some() {
        Err("uses an old kind of TIFF compression that Grape can't rewrite yet.")
    } else {
        compression.ok_or(UNREAD_COMPRESSION)
    }
}

/// The most a LONG holds, and what libtiff takes a RowsPerStrip, a tile's
/// size or a TileDepth of it as: the picture's.
const LONG: u64 = u32::MAX as u64;

/// The most a SHORT holds.
pub(crate) const SHORT: u64 = u16::MAX as u64;

/// Each value's size and whether it's signed, for the integer types
/// libtiff 4.5 reads a number tag in: any but IFD and IFD8.
fn integer(kind: u16) -> Option<(usize, bool)> {
    match kind {
        1 => Some((1, false)),
        6 => Some((1, true)),
        3 => Some((2, false)),
        8 => Some((2, true)),
        4 => Some((4, false)),
        9 => Some((4, true)),
        16 => Some((8, false)),
        17 => Some((8, true)),
        _ => None,
    }
}

/// The numbers `entry` holds as libtiff 4.5 reads an integer tag's
/// (`TIFFReadDirEntryShortArray`, `TIFFReadDirEntryLongArray`): of any
/// integer type but IFD and IFD8, signed ones not negative, each at most
/// `max`; read from `r` when they aren't inline (a classic file's LONG8
/// and SLONG8 never are), the bytes taken from `left`, past which the
/// file is too large. `None` where libtiff can't read them, and for more
/// than `cap` of them.
pub(crate) fn numbers(
    r: &mut dyn In,
    entry: &Entry,
    order: Order,
    max: u64,
    cap: u64,
    left: &mut u64,
) -> Result<Option<Vec<u64>>, Error> {
    let Some((size, signed)) = integer(entry.kind) else {
        return Ok(None);
    };
    if entry.count > cap {
        return Ok(None);
    }
    if let Value::At { len, .. } = entry.value {
        *left = left
            .checked_sub(len)
            .ok_or_else(|| Error::TooLarge("tags read past the file's size".into()))?;
    }
    let Some(bytes) = read_value(r, entry, cap.saturating_mul(8))? else {
        return Ok(None);
    };
    let mut all = Vec::with_capacity(bytes.len() / size);
    for b in bytes.chunks_exact(size) {
        let n = order.uint(b);
        if (signed && n >> (8 * size - 1) == 1) || n > max {
            return Ok(None);
        }
        all.push(n);
    }
    Ok(Some(all))
}

/// The one number `entry` holds as libtiff 4.5 reads a SHORT or LONG tag
/// (`TIFFReadDirEntryShort`, `TIFFReadDirEntryLong`), at most `max`, as
/// [`numbers`] reads it (8 bytes at most); `None` where libtiff can't read
/// it, and for more than one value.
pub(crate) fn number(
    r: &mut dyn In,
    entry: &Entry,
    order: Order,
    max: u64,
) -> Result<Option<u64>, Error> {
    if entry.count != 1 {
        return Ok(None);
    }
    let mut free = u64::MAX;
    let one = numbers(r, entry, order, max, 1, &mut free)?;
    Ok(one.and_then(|n| n.first().copied()))
}

/// The one number a per-sample tag's `entry` holds as libtiff 4.5 reads
/// Compression, BitsPerSample and SampleFormat (`TIFFReadDirEntryShort`,
/// then `TIFFReadDirEntryPersampleShort`): its one value, or of more, at
/// least `samples` (SamplesPerPixel, [`samples`]), every one a SHORT's
/// ([`numbers`], at most [`MAX_SAMPLES`] of them, taken from `left`),
/// and the first `samples` the same. `None` where libtiff can't read it.
pub(crate) fn per_sample(
    r: &mut dyn In,
    entry: &Entry,
    order: Order,
    samples: Option<u64>,
    left: &mut u64,
) -> Result<Option<u64>, Error> {
    if entry.count == 1 {
        return number(r, entry, order, SHORT);
    }
    let Some(samples) = samples.filter(|&n| n > 0 && entry.count >= n) else {
        return Ok(None);
    };
    let all = numbers(r, entry, order, SHORT, MAX_SAMPLES, left)?.unwrap_or_default();
    let first = all.first().copied();
    let used = usize::try_from(samples).unwrap_or(usize::MAX);
    Ok(first.filter(|&v| all.iter().take(used).all(|&n| n == v)))
}

/// SamplesPerPixel as libtiff 4.5 reads it ([`number`]): 1 when it's
/// absent; `None` where libtiff can't read it, or it's 0.
pub(crate) fn samples(r: &mut dyn In, ifd: &Ifd, order: Order) -> Result<Option<u64>, Error> {
    match ifd.get(277) {
        None => Ok(Some(1)),
        Some(entry) => Ok(number(r, entry, order, SHORT)?.filter(|&n| n > 0)),
    }
}

/// YCbCrSubsampling as libtiff 4.5 reads it: exactly two numbers, each a
/// SHORT's ([`numbers`], taken from `left`). `None` otherwise, where
/// libtiff keeps its default, 2 × 2, and other readers may not.
pub(crate) fn pair(
    r: &mut dyn In,
    entry: &Entry,
    order: Order,
    left: &mut u64,
) -> Result<Option<(u64, u64)>, Error> {
    if entry.count != 2 {
        return Ok(None);
    }
    Ok(match numbers(r, entry, order, SHORT, 2, left)?.as_deref() {
        Some(&[h, v]) => Some((h, v)),
        _ => None,
    })
}

/// The IFD an Exif, GPS or Interop pointer `entry` points at, as libtiff
/// 4.5 reads one (`TIFFReadDirEntryIfd8`): one LONG, IFD, LONG8 or IFD8,
/// read from `r` when it isn't inline (a classic file's LONG8 and IFD8);
/// `None` where libtiff can't read it.
fn pointer(r: &mut dyn In, entry: &Entry, order: Order) -> Result<Option<u64>, Error> {
    let size = match entry.kind {
        4 | 13 => 4,
        16 | 18 => 8,
        _ => return Ok(None),
    };
    if entry.count != 1 {
        return Ok(None);
    }
    let mut bytes = [0; 8];
    match entry.value {
        Value::Inline { bytes: inline, .. } => bytes = inline,
        Value::At { offset, len: 8 } => read_at(r, offset, &mut bytes)?,
        Value::At { .. } | Value::Unread => return Ok(None),
    }
    Ok(Some(order.uint(&bytes[..size])))
}

/// An image IFD's layout as libtiff 4.5 reads it (`TIFFReadDirectory`):
/// ImageWidth, ImageLength, SamplesPerPixel, PlanarConfiguration,
/// RowsPerStrip, TileWidth, TileLength, ImageDepth and TileDepth, each as
/// [`number`] reads it, and the strips or tiles it counts
/// (`TIFFNumberOfStrips`, `TIFFNumberOfTiles`). It tells tiles by a
/// TileWidth or TileLength, not by the tags the blocks are listed under,
/// and takes one it isn't given from a RowsPerStrip the IFD lists before
/// them ([`Dims::of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dims {
    pub(crate) width: u64,
    pub(crate) height: u64,
    /// SamplesPerPixel: 1 when it's absent.
    pub(crate) samples: u64,
    /// The planes apart (PlanarConfiguration 2).
    pub(crate) planar: bool,
    /// RowsPerStrip, [`LONG`] when it's absent.
    rows: u64,
    /// TileWidth and TileLength (0 for one that's absent), for tiles.
    tile: Option<(u64, u64)>,
    /// ImageDepth and TileDepth, 1 when absent: the tiles are this many
    /// deep, which libtiff counts and strips ignore.
    depth: (u64, u64),
}

impl Dims {
    /// `ifd`'s layout ([`Dims`]), or why libtiff can't read it: a tag it
    /// can't read, no size, SamplesPerPixel, RowsPerStrip or TileDepth 0,
    /// a PlanarConfiguration other than 1 and 2, or no strips or tiles by
    /// its count. A LONG8 or SLONG8 out of line is read from `r`.
    pub(crate) fn of(
        r: &mut dyn In,
        ifd: &Ifd,
        order: Order,
    ) -> Result<Result<Dims, String>, Error> {
        let tags = [
            (256, "ImageWidth", LONG, 0),
            (257, "ImageLength", LONG, 0),
            (277, "SamplesPerPixel", SHORT, 1),
            (284, "PlanarConfiguration", SHORT, 1),
            (278, "RowsPerStrip", LONG, LONG),
            (322, "TileWidth", LONG, 0),
            (323, "TileLength", LONG, 0),
            (32997, "ImageDepth", LONG, 1),
            (32998, "TileDepth", LONG, 1),
        ];
        let mut n = [0; 9];
        for (slot, (tag, name, max, default)) in n.iter_mut().zip(tags) {
            *slot = match ifd.get(tag) {
                None => default,
                Some(entry) => match number(r, entry, order, max)? {
                    Some(n) => n,
                    None => return Ok(Err(format!("its {name} can't be read"))),
                },
            };
        }
        let [width, height, samples, planar, rows, tw, tl, depth, tile_depth] = n;
        let not = |why: String| Ok(Err(why));
        if ifd.get(256).is_none() && ifd.get(257).is_none() {
            return not("it has no size".into());
        }
        if width == 0 || height == 0 {
            return not(format!("it's {width} × {height}"));
        }
        let planar = match planar {
            1 => false,
            2 => true,
            n => return not(format!("its PlanarConfiguration is {n}")),
        };
        for (name, n) in [
            ("SamplesPerPixel", samples),
            ("RowsPerStrip", rows),
            ("TileDepth", tile_depth),
        ] {
            if n == 0 {
                return not(format!("its {name} is 0"));
            }
        }
        let tiled = ifd.get(322).is_some() || ifd.get(323).is_some();
        // libtiff sets the tags in the order the IFD lists them (the first
        // of each), and a RowsPerStrip set before either tile size sets
        // both (`_TIFFVSetField`): TileLength to it, and TileWidth to the
        // ImageWidth set by then, 0 before it; a tile size listed sets its
        // own after that.
        let at = |tag| ifd.entries.iter().position(|e| e.tag == tag);
        let first_tile = [at(322), at(323)].into_iter().flatten().min();
        let (tw, tl) = match (at(278), first_tile) {
            (Some(r), Some(t)) if r < t => {
                let width = if at(256).is_some_and(|w| w < r) {
                    width
                } else {
                    0
                };
                (
                    if ifd.get(322).is_some() { tw } else { width },
                    if ifd.get(323).is_some() { tl } else { rows },
                )
            }
            _ => (tw, tl),
        };
        let dims = Dims {
            width,
            height,
            samples,
            planar,
            rows,
            tile: tiled.then_some((tw, tl)),
            depth: (depth, tile_depth),
        };
        if dims.count() == 0 {
            return not("libtiff counts no strips or tiles in it".into());
        }
        Ok(Ok(dims))
    }

    /// Tiles rather than strips.
    pub(crate) fn tiled(&self) -> bool {
        self.tile.is_some()
    }

    /// The pixel rows of a full strip: RowsPerStrip, at most the picture's.
    pub(crate) fn rows(&self) -> u64 {
        self.rows.min(self.height)
    }

    /// A tile's width and length, for tiles: TileWidth and TileLength, the
    /// picture's for [`LONG`].
    pub(crate) fn tile(&self) -> Option<(u64, u64)> {
        let whole = |n, all| if n == LONG { all } else { n };
        let (tw, tl) = self.tile?;
        Some((whole(tw, self.width), whole(tl, self.height)))
    }

    /// The strips in each plane as libtiff counts them: ImageLength over
    /// RowsPerStrip, one without it; 0 past 32 bits.
    fn strips_per_plane(&self) -> u64 {
        if self.rows == LONG {
            1
        } else {
            howmany(self.height, self.rows)
        }
    }

    /// The slices a tile holds: TileDepth, ImageDepth's for [`LONG`]; 1
    /// for strips, which ignore it.
    pub(crate) fn tile_depth(&self) -> u64 {
        match (self.tile, self.depth) {
            (Some(_), (depth, LONG)) => depth,
            (Some(_), (_, td)) => td,
            (None, _) => 1,
        }
    }

    /// The strips or tiles in each plane as libtiff counts them: its
    /// strips, or the tiles across, down and deep; 0 past 32 bits.
    pub(crate) fn per_plane(&self) -> u64 {
        let Some((tw, tl)) = self.tile() else {
            return self.strips_per_plane();
        };
        let across = times(howmany(self.width, tw), howmany(self.height, tl));
        times(across, howmany(self.depth.0, self.tile_depth()))
    }

    /// The strips or tiles libtiff reads: those of a plane, times the
    /// samples when the planes are apart; 0 past 32 bits, where libtiff
    /// can't read the IFD.
    pub(crate) fn count(&self) -> u64 {
        if self.planar {
            times(self.per_plane(), self.samples)
        } else {
            self.per_plane()
        }
    }
}

/// `x` over `y`, rounded up, as libtiff works it out (`TIFFhowmany_32`): 0
/// for `y` 0, and where `x + y - 1` passes 32 bits.
fn howmany(x: u64, y: u64) -> u64 {
    if y == 0 || x + y > LONG {
        0
    } else {
        x.div_ceil(y)
    }
}

/// `a` times `b`, as libtiff works it out (`_TIFFMultiply32`): 0 past 32
/// bits.
fn times(a: u64, b: u64) -> u64 {
    a.checked_mul(b).filter(|&n| n <= LONG).unwrap_or(0)
}

/// Why a file whose byte counts libtiff takes at their word, and readers
/// that size data themselves don't, is refused ([`Walk::recount`]).
pub(crate) const MISSIZED: &str =
    "says the wrong size for some of its image data, which Grape can't rewrite yet.";

/// Why a file is refused whose layout libtiff can't read where Grape needs
/// it: to pick the blocks libtiff reads of several listed, or to size
/// uncompressed ones ([`Walk::recount`]), however right their counts are.
pub(crate) const UNREAD_LAYOUT: &str =
    "says how its image data is laid out in a way Grape can't read yet.";

/// Why an uncompressed IFD's blocks can't be sized ([`Walk::sizes`]).
enum Unsized {
    /// A layout, BitsPerSample, PhotometricInterpretation or
    /// YCbCrSubsampling libtiff can't read or count: why.
    Unread(String),
    /// Sizes past any number, or offsets listed for another layout: why.
    Wrong(String),
}

impl Unsized {
    fn why(&self) -> &str {
        match self {
            Unsized::Unread(why) | Unsized::Wrong(why) => why,
        }
    }
}

/// How an uncompressed IFD's strips or tiles are sized from its picture,
/// as libtiff sizes them: rows of its pixels' samples (of one sample's
/// when the planes are apart, or of YCbCr sampling units when the chroma
/// is subsampled, TIFF 6 §21), RowsPerStrip or the tile's length of them,
/// a plane's last strip holding the rows left.
struct Sizes {
    /// The bytes of a block's row, in each plane.
    row: Vec<u64>,
    /// The blocks in each plane.
    per_plane: u64,
    /// The pixel rows of a full block.
    rows: u64,
    /// The pixel rows a row of a block covers: 1, or the vertical
    /// subsampling.
    per_row: u64,
    /// The picture's rows, for a plane's last strip; `None` for tiles,
    /// which are always full.
    height: Option<u64>,
}

impl Sizes {
    /// The bytes block `n` holds; no more than a full block's, which
    /// [`Walk::sizes`] made sure has a size.
    fn of(&self, n: u64) -> u64 {
        let per_plane = self.per_plane.max(1);
        let row = usize::try_from(n / per_plane)
            .ok()
            .and_then(|plane| self.row.get(plane))
            .copied()
            .unwrap_or(0);
        let rows = match self.height {
            Some(height) => self
                .rows
                .min(height.saturating_sub(n % per_plane * self.rows)),
            None => self.rows,
        };
        rows.div_ceil(self.per_row).saturating_mul(row)
    }

    /// The bytes libtiff wants of a lone strip before it takes its count
    /// at its word (`ByteCountLooksBad`): its scanline's bytes times the
    /// picture's rows, where a scanline of subsampled YCbCr is a row of
    /// sampling units over the vertical subsampling, rounded down (27
    /// bytes for 5 × 3 at 2 × 2, where the strip holds 36).
    fn lone(&self) -> u64 {
        let row = self.row.first().copied().unwrap_or(0);
        let rows = self.height.unwrap_or(self.rows);
        (row / self.per_row.max(1)).saturating_mul(rows)
    }
}

/// Reads `len` bytes of `entry`'s value, and only them ([`read_at`]), or
/// `None` when it can't be read or is longer than `max`.
pub(crate) fn read_value(
    r: &mut dyn In,
    entry: &Entry,
    max: u64,
) -> Result<Option<Vec<u8>>, Error> {
    match entry.value {
        Value::Inline { bytes, len } => Ok(Some(bytes[..usize::from(len)].to_vec())),
        Value::At { offset, len } if len <= max => {
            let mut value = vec![0; usize_of(len)?];
            read_at(r, offset, &mut value)?;
            Ok(Some(value))
        }
        Value::At { .. } | Value::Unread => Ok(None),
    }
}

fn usize_of(n: u64) -> Result<usize, Error> {
    usize::try_from(n).map_err(|_| Error::TooLarge(format!("a value of {n} bytes")))
}

/// What the walk read at an offset.
enum Seen {
    /// A page or a SubIFD.
    Image,
    /// An Exif, GPS or Interop IFD, kept for the IFDs that share it.
    Meta(exif::Ifd, Meta),
}

/// The walk's state: what it has read, and its counts against the caps.
struct Walk<'r> {
    r: &'r mut dyn In,
    layout: Layout,
    /// The IFDs read, by offset.
    seen: HashMap<u64, Seen>,
    ifds: usize,
    entries: usize,
    blocks: usize,
    /// What's left of the bytes of per-sample tags (Compression and
    /// BitsPerSample) and YCbCrSubsampling it may read out of line, the
    /// file's size at first: pages can all point at one long array.
    samples_left: u64,
}

impl Walk<'_> {
    /// The entries of the IFD at `at` and the next IFD's offset (0 when the
    /// file ends before it), or `None` when the IFD starts outside the file.
    fn raw(&mut self, at: u64) -> Result<Option<(Vec<Entry>, u64)>, Error> {
        let l = self.layout;
        if at < l.header() || at.saturating_add(l.count_len()) > l.len {
            return Ok(None);
        }
        let mut count = [0; 8];
        let count = &mut count[..l.count_len() as usize];
        read_at(self.r, at, count)?;
        let n = l.order.uint(count);
        if n > MAX_ENTRIES {
            return Err(Error::Broken(format!("an IFD of {n} entries")));
        }
        self.count(n as usize)?;
        // The entries, then the next IFD's offset when the file holds it.
        let end = at + l.count_len() + n * l.entry_len();
        let next_len = if end + l.word() <= l.len { l.word() } else { 0 };
        let mut bytes = vec![0; (n * l.entry_len() + next_len) as usize];
        read_at(self.r, at + l.count_len(), &mut bytes)?;
        let (bytes, next) = bytes.split_at((n * l.entry_len()) as usize);
        let next = if next.is_empty() {
            0
        } else {
            l.order.uint(next)
        };
        let entries = bytes
            .chunks_exact(l.entry_len() as usize)
            .map(|e| self.entry(e))
            .collect();
        Ok(Some((entries, next)))
    }

    /// Counts `n` more entries against [`MAX_ALL_ENTRIES`].
    fn count(&mut self, n: usize) -> Result<(), Error> {
        self.entries += n;
        if self.entries > MAX_ALL_ENTRIES {
            return Err(Error::TooLarge(format!(
                "over {MAX_ALL_ENTRIES} IFD entries"
            )));
        }
        Ok(())
    }

    /// The entry `e` (its 12 or 20 bytes).
    fn entry(&self, e: &[u8]) -> Entry {
        let l = self.layout;
        let o = l.order;
        let (count, field) = if l.big {
            (o.uint(&e[4..12]), &e[12..20])
        } else {
            (o.uint(&e[4..8]), &e[8..12])
        };
        let kind = o.uint(&e[2..4]) as u16;
        let value = match type_size(kind).and_then(|size| size.checked_mul(count)) {
            Some(len) if len <= field.len() as u64 => {
                let mut bytes = [0; 8];
                bytes[..len as usize].copy_from_slice(&field[..len as usize]);
                Value::Inline {
                    bytes,
                    len: len as u8,
                }
            }
            Some(len) => {
                let offset = o.uint(field);
                match offset.checked_add(len) {
                    Some(end) if end <= l.len => Value::At { offset, len },
                    _ => Value::Unread,
                }
            }
            None => Value::Unread,
        };
        Entry {
            tag: o.uint(&e[0..2]) as u16,
            kind,
            count,
            value,
        }
    }

    /// The image IFD at `at`, with `entries`, `depth` SubIFDs down.
    fn image(&mut self, at: u64, entries: Vec<Entry>, depth: usize) -> Result<Ifd, Error> {
        self.ifds += 1;
        if self.ifds > MAX_IFDS {
            return Err(Error::TooLarge(format!("over {MAX_IFDS} IFDs")));
        }
        let mut ifd = Ifd {
            at,
            entries,
            compression: 1,
            blocks: Vec::new(),
            tiled: false,
            estimated: false,
            subs: Vec::new(),
            exif: None,
            gps: None,
        };
        // Before its blocks: an old-style JPEG may have none.
        let compression = self.compression(&ifd)?;
        ifd.compression = admitted(&ifd, compression).map_err(|why| Error::Refused(why.into()))?;
        self.blocks(&mut ifd)?;
        if let Some(subs) = ifd.get(330).cloned() {
            if depth >= MAX_DEPTH {
                return Err(Error::TooLarge(format!("SubIFDs over {MAX_DEPTH} deep")));
            }
            for head in self.uints(&subs, MAX_IFDS)? {
                if head != 0 {
                    let chain = self.chain(head, depth + 1)?;
                    ifd.subs.push(chain);
                }
            }
        }
        ifd.exif = self.meta(ifd.get(34665).cloned(), exif::Ifd::Exif)?;
        ifd.gps = self.meta(ifd.get(34853).cloned(), exif::Ifd::Gps)?;
        Ok(ifd)
    }

    /// `ifd`'s Compression as libtiff reads it ([`per_sample`]): 1 when
    /// it's absent; `None` where libtiff can't read it.
    fn compression(&mut self, ifd: &Ifd) -> Result<Option<u64>, Error> {
        let o = self.layout.order;
        let Some(entry) = ifd.get(259) else {
            return Ok(Some(1));
        };
        let samples = samples(self.r, ifd, o)?;
        per_sample(self.r, entry, o, samples, &mut self.samples_left)
    }

    /// The chain of SubIFDs starting at `at`, `depth` down.
    fn chain(&mut self, mut at: u64, depth: usize) -> Result<Vec<Ifd>, Error> {
        let mut chain = Vec::new();
        while at != 0 {
            if self.seen.insert(at, Seen::Image).is_some() {
                return Err(Error::Broken("an IFD reached twice".into()));
            }
            let Some((entries, next)) = self.raw(at)? else {
                return Err(Error::Broken("a SubIFD outside the file".into()));
            };
            chain.push(self.image(at, entries, depth)?);
            at = next;
        }
        Ok(chain)
    }

    /// The Exif, GPS or Interop IFD (`kind`) `entry` points at, if any;
    /// with its Interop IFD when it's an Exif IFD. One that other IFDs
    /// point at too is read once, and its entries counted each time.
    fn meta(&mut self, entry: Option<Entry>, kind: exif::Ifd) -> Result<Option<Meta>, Error> {
        let Some(entry) = entry else {
            return Ok(None);
        };
        let at = match pointer(self.r, &entry, self.layout.order)? {
            Some(0) => return Ok(None),
            Some(at) => at,
            None => return Ok(Some(Meta::unread(0))),
        };
        match self.seen.get(&at) {
            Some(Seen::Meta(seen, meta)) if *seen == kind => {
                let meta = meta.clone();
                let interop = meta.interop.as_ref().map_or(0, |i| i.entries.len());
                self.count(meta.entries.len() + interop)?;
                return Ok(Some(meta));
            }
            Some(_) => return Err(Error::Broken("an IFD reached twice".into())),
            None => {}
        }
        // Unread while it's read, so that it can't lead back to itself.
        self.seen.insert(at, Seen::Meta(kind, Meta::unread(at)));
        let meta = match self.raw(at) {
            Ok(Some((entries, _))) => {
                let interop = if kind == exif::Ifd::Exif {
                    let entry = entries.iter().find(|e| e.tag == 40965).cloned();
                    self.meta(entry, exif::Ifd::Interop)?.map(Box::new)
                } else {
                    None
                };
                Meta {
                    at,
                    entries,
                    unread: false,
                    interop,
                }
            }
            Ok(None) | Err(Error::Broken(_)) => Meta::unread(at),
            Err(e) => return Err(e),
        };
        self.seen.insert(at, Seen::Meta(kind, meta.clone()));
        Ok(Some(meta))
    }

    /// Reads `ifd`'s strips or tiles into its blocks.
    fn blocks(&mut self, ifd: &mut Ifd) -> Result<(), Error> {
        let (offsets, tiled) = match (ifd.get(273), ifd.get(324)) {
            (Some(strips), None) => (strips.clone(), false),
            (None, Some(tiles)) => (tiles.clone(), true),
            (Some(_), Some(_)) => return Err(Error::Broken("both strips and tiles".into())),
            (None, None) => return Err(Error::Broken("an IFD with no image data".into())),
        };
        ifd.tiled = tiled;
        let room = MAX_BLOCKS.saturating_sub(self.blocks);
        if offsets.count > room as u64 {
            return Err(Error::TooLarge(format!(
                "over {MAX_BLOCKS} strips or tiles"
            )));
        }
        // The offsets and the counts go straight into the blocks.
        let mut blocks = Vec::with_capacity(offsets.count as usize);
        self.each_uint(&offsets, room, |offset| {
            blocks.push(Block { offset, len: 0 });
        })?;
        self.blocks += blocks.len();
        // Uncompressed (1, or no Compression): sized from the picture.
        let plain = ifd.compression == 1;
        match ifd.get(if tiled { 325 } else { 279 }).cloned() {
            Some(counts) => {
                let mut lens = 0;
                self.each_uint(&counts, room, |len| {
                    if let Some(block) = blocks.get_mut(lens) {
                        block.len = len;
                    }
                    lens += 1;
                })?;
                if lens != blocks.len() {
                    return Err(Error::Broken(format!(
                        "{} offsets but {lens} byte counts",
                        blocks.len()
                    )));
                }
                ifd.estimated = self.recount(ifd, &mut blocks, plain)?;
            }
            None => {
                let unsure = |why: &str| Error::Broken(format!("no byte counts, and {why}"));
                if !plain {
                    return Err(unsure("the data is compressed"));
                }
                let sizes = self
                    .sizes(ifd, blocks.len())?
                    .map_err(|why| unsure(why.why()))?;
                for (n, block) in (0..).zip(&mut blocks) {
                    block.len = sizes.of(n);
                }
                ifd.estimated = true;
            }
        }
        let l = self.layout;
        for block in &mut blocks {
            let Block { offset, len } = *block;
            if len == 0 {
                block.offset = 0;
            } else if offset < l.header() || offset.checked_add(len).is_none_or(|e| e > l.len) {
                return Err(Error::Broken(format!(
                    "a block of {len} bytes at {offset}, outside the file"
                )));
            }
        }
        ifd.blocks = blocks;
        Ok(())
    }

    /// Checks the byte counts an IFD's `blocks` were given (uncompressed
    /// data when `plain`) as libtiff 4.5 reads them (`TIFFReadDirectory`),
    /// so that both tools copy what readers read ([module docs](self)).
    /// libtiff reads as many entries as its own count of strips or tiles
    /// ([`Dims::count`]), 0 for those missing, and checks those, as the
    /// kind it reads them as, whichever tags list them:
    ///
    /// - **Worked out from the picture** ([`Walk::sizes`]) where libtiff
    ///   ignores them ("Bogus" and "Wrong" StripByteCounts): a lone strip
    ///   at a nonzero offset whose count is 0, or, uncompressed, that runs
    ///   past the file's end or is short of what libtiff wants of it
    ///   ([`Sizes::lone`]); and uncompressed
    ///   strips or tiles, more than 2 in one plane, whose first two counts
    ///   differ and aren't 0. Compressed data, a layout Grape can't size,
    ///   or blocks listed that aren't libtiff's (more or fewer of them), is
    ///   refused instead ([`MISSIZED`]): what Grape copies and what libtiff
    ///   reads may differ.
    /// - **Refused** ([`MISSIZED`]): a block of 0 bytes at a nonzero offset
    ///   (one at 0 is empty, GDAL's sparse tiles), an uncompressed block
    ///   libtiff reads that's shorter than the picture says, which libtiff
    ///   fails on and Pillow and ImageMagick read past, and uncompressed
    ///   blocks whose sizes are past any number.
    /// - **Refused as unread** ([`UNREAD_LAYOUT`]), however right the
    ///   counts: more than one entry where libtiff can't read the layout
    ///   ([`Dims::of`]), as which it would read can't be told, and
    ///   uncompressed blocks Grape can't size ([`Walk::sizes`]: a layout,
    ///   BitsPerSample, Photometric or YCbCrSubsampling libtiff can't read,
    ///   a lone strip's too), as how much of the picture readers read from
    ///   them can't be checked.
    ///
    /// Gives whether it worked them out.
    fn recount(&mut self, ifd: &Ifd, blocks: &mut [Block], plain: bool) -> Result<bool, Error> {
        let l = self.layout;
        let listed = blocks.len() as u64;
        // Where libtiff can't count them, a lone compressed entry is the
        // one it would read; of more, it can't be told which, nor, for
        // uncompressed data, how much of the picture readers that size it
        // themselves read from it.
        let (count, tiled, apart) = match Dims::of(self.r, ifd, l.order)? {
            Ok(dims) => (dims.count(), dims.tiled(), dims.planar),
            Err(_) if listed == 1 && !plain => (1, ifd.tiled, false),
            Err(_) => return Err(Error::Refused(UNREAD_LAYOUT.into())),
        };
        // Uncompressed, sized as libtiff sizes them, as the kind it reads
        // them as, whichever tags list them; when they can't be (a tag
        // libtiff can't read), what readers read of them can't be checked.
        let sizes = match usize::try_from(count) {
            Ok(want) if plain => match self.sizes(ifd, want)? {
                Ok(sizes) => Some(sizes),
                Err(Unsized::Unread(_)) => return Err(Error::Refused(UNREAD_LAYOUT.into())),
                Err(Unsized::Wrong(_)) => return Err(Error::Refused(MISSIZED.into())),
            },
            _ => None,
        };
        let short = |n: u64, block: &Block| sizes.as_ref().is_some_and(|s| block.len < s.of(n));
        // The entries libtiff reads: 0 for those it counts and isn't given.
        let entry = |n: usize| {
            blocks
                .get(n)
                .copied()
                .unwrap_or(Block { offset: 0, len: 0 })
        };
        let ignored = if count == 1 && !tiled {
            let one = entry(0);
            let past = one.offset <= l.len && one.len > l.len - one.offset;
            let short = sizes.as_ref().is_some_and(|s| one.len < s.lone());
            one.offset != 0 && (one.len == 0 || (plain && (past || short)))
        } else {
            let (first, second) = (entry(0), entry(1));
            plain
                && !apart
                && count > 2
                && first.len != second.len
                && first.len != 0
                && second.len != 0
        };
        if ignored {
            let Some(sizes) = sizes.as_ref().filter(|_| count == listed) else {
                return Err(Error::Refused(MISSIZED.into()));
            };
            for (n, block) in (0..).zip(blocks.iter_mut()) {
                block.len = sizes.of(n);
            }
        }
        // Those libtiff doesn't read are copied as they are.
        let wrong = |(n, block): (u64, &Block)| {
            (block.len == 0 && block.offset != 0) || (block.len > 0 && n < count && short(n, block))
        };
        if (0..).zip(blocks.iter()).any(wrong) {
            return Err(Error::Refused(MISSIZED.into()));
        }
        Ok(ignored)
    }

    /// How an uncompressed IFD's `want` strips or tiles are sized from its
    /// picture, as libtiff sizes them ([`Sizes`]): as the kind it reads
    /// them as, whichever tags list them, from its layout and
    /// BitsPerSample, PhotometricInterpretation and YCbCrSubsampling as
    /// libtiff reads them ([`Dims`], [`per_sample`], [`number`], [`pair`]),
    /// or why they can't be; what's read out of line counts against what
    /// the walk may read of them.
    fn sizes(&mut self, ifd: &Ifd, want: usize) -> Result<Result<Sizes, Unsized>, Error> {
        let o = self.layout.order;
        let unread = |why: &str| Ok(Err(Unsized::Unread(why.into())));
        let dims = match Dims::of(self.r, ifd, o)? {
            Ok(dims) => dims,
            Err(why) => return unread(&why),
        };
        let Dims {
            width,
            height,
            samples: spp,
            planar,
            ..
        } = dims;
        // One value for every sample, as libtiff reads it.
        let bps = match ifd.get(258) {
            None => 1,
            Some(entry) => match per_sample(self.r, entry, o, Some(spp), &mut self.samples_left)? {
                Some(bps) => bps,
                None => return unread("its BitsPerSample can't be read"),
            },
        };
        let photometric = match ifd.get(262) {
            None => None,
            Some(entry) => match number(self.r, entry, o, SHORT)? {
                Some(photometric) => Some(photometric),
                None => return unread("its PhotometricInterpretation can't be read"),
            },
        };
        // YCbCr's chroma is subsampled 2 × 2 unless it says otherwise, in
        // sampling units of h × v luma samples and two chroma samples
        // (TIFF 6 §21), when its three samples are in one plane.
        let (h, v) = match (photometric, planar) {
            (Some(6), false) => match ifd.get(530) {
                None => (2, 2),
                Some(entry) => match pair(self.r, entry, o, &mut self.samples_left)? {
                    Some(sub) => sub,
                    None => return unread("its YCbCrSubsampling can't be read"),
                },
            },
            _ => (1, 1),
        };
        let subsampled = (h, v) != (1, 1);
        if subsampled && !(matches!((h, v), (1 | 2 | 4, 1 | 2 | 4)) && spp == 3) {
            return unread(&format!("its YCbCrSubsampling is {h} × {v}"));
        }
        let past = || Ok(Err(Unsized::Wrong("sizes past any number".into())));
        // Bits a pixel, or a sampling unit, in each plane (SamplesPerPixel
        // is a SHORT's, so the planes are few enough to hold).
        let bits: Vec<u64> = if planar {
            vec![bps; usize::try_from(spp).unwrap_or(0)]
        } else if subsampled {
            vec![bps * (h * v + 2)]
        } else {
            vec![bps * spp]
        };
        // A full block's width and rows, and a plane's last strip's rows.
        let per_plane = dims.per_plane();
        let (across, rows, height) = match dims.tile() {
            Some((tw, th)) => (tw, th, None),
            None => (width, dims.rows(), Some(height)),
        };
        if per_plane.checked_mul(bits.len() as u64) != Some(want as u64) {
            return Ok(Err(Unsized::Wrong(format!(
                "{want} offsets for another layout"
            ))));
        }
        let mut row = Vec::with_capacity(bits.len());
        for bits in bits {
            // A row's bytes, and a full block's, must have a size.
            let bytes = across.div_ceil(h).checked_mul(bits).map(|b| b.div_ceil(8));
            match bytes.filter(|b| b.checked_mul(rows.div_ceil(v)).is_some()) {
                Some(bytes) => row.push(bytes),
                None => return past(),
            }
        }
        Ok(Ok(Sizes {
            row,
            per_plane,
            rows,
            per_row: v,
            height,
        }))
    }

    /// The unsigned numbers `entry` holds (SHORT, LONG, LONG8, IFD or IFD8),
    /// at most `cap` of them.
    fn uints(&mut self, entry: &Entry, cap: usize) -> Result<Vec<u64>, Error> {
        let mut all = Vec::with_capacity(entry.count.min(cap as u64) as usize);
        self.each_uint(entry, cap, |n| all.push(n))?;
        Ok(all)
    }

    /// Gives `f` each unsigned number `entry` holds (SHORT, LONG, LONG8, IFD
    /// or IFD8), in order; at most `cap` of them, or it's too large.
    fn each_uint(
        &mut self,
        entry: &Entry,
        cap: usize,
        mut f: impl FnMut(u64),
    ) -> Result<(), Error> {
        let size = match entry.kind {
            3 => 2,
            4 | 13 => 4,
            16 | 18 => 8,
            kind => {
                return Err(Error::Broken(format!(
                    "tag {} of type {kind}, not a number",
                    entry.tag
                )))
            }
        };
        if entry.count > cap as u64 {
            return Err(Error::TooLarge(format!(
                "tag {} holds {} numbers",
                entry.tag, entry.count
            )));
        }
        let o = self.layout.order;
        match entry.value {
            Value::Inline { bytes, len } => {
                for b in bytes[..usize::from(len)].chunks_exact(size) {
                    f(o.uint(b));
                }
                Ok(())
            }
            Value::At { offset, len } => {
                let mut step = vec![0; STEP.min(usize_of(len)?)];
                let mut done = 0;
                while done < len {
                    let take = &mut step[..STEP.min((len - done) as usize)];
                    read_at(self.r, offset + done, take)?;
                    for b in take.chunks_exact(size) {
                        f(o.uint(b));
                    }
                    done += take.len() as u64;
                }
                Ok(())
            }
            Value::Unread => Err(Error::Broken(format!("tag {} can't be read", entry.tag))),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use super::super::tests::{
        gray, miscounted, picture, set_entry, shared_meta, Builder, Page, B, V,
    };
    use super::*;

    fn read(bytes: &[u8]) -> Result<Tiff, Error> {
        Tiff::read(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    /// The bytes of `block` in `file`.
    fn bytes_of(file: &[u8], block: &Block) -> Vec<u8> {
        file[block.offset as usize..(block.offset + block.len) as usize].to_vec()
    }

    /// Two pages: the first with a chain of two SubIFDs, one holding a
    /// SubIFD of its own, an Exif IFD with an Interop IFD, and a GPS IFD;
    /// the second tiled.
    fn sample(order: u8, big: bool) -> Builder {
        let mut first = gray(4, 2, 1, vec![B::data(b"abcd"), B::data(b"efgh")]);
        let mut nested = gray(2, 1, 1, vec![B::data(b"ij")]);
        nested.subs = vec![vec![gray(1, 1, 1, vec![B::data(b"k")])]];
        first.subs = vec![vec![nested, gray(1, 1, 1, vec![B::data(b"l")])]];
        first.exif = Some(vec![(0x9003, V::Ascii("2024:05:01 10:20:30"))]);
        first.interop = Some(vec![(1, V::Ascii("R03"))]);
        first.gps = Some(vec![(2, V::Rational(vec![(48, 1), (8, 1), (0, 1)]))]);
        let second = Page {
            tags: vec![
                (256, V::Long(vec![32])),
                (257, V::Long(vec![16])),
                (258, V::Short(vec![8])),
                (262, V::Short(vec![1])),
                (322, V::Short(vec![16])),
                (323, V::Short(vec![16])),
            ],
            blocks: vec![B::data(&[1; 256]), B::data(&[2; 256])],
            tiled: true,
            ..Page::default()
        };
        Builder::new(order, big, vec![first, second])
    }

    #[test]
    fn every_ifd_is_read_in_both_orders_and_kinds() {
        for (order, big) in [(b'I', false), (b'M', false), (b'I', true), (b'M', true)] {
            let file = sample(order, big).build();
            let tiff = read(&file).unwrap();
            assert_eq!(tiff.layout.big, big);
            assert_eq!(tiff.layout.order == Order::Big, order == b'M');
            let [first, second] = &tiff.pages[..] else {
                panic!("{} pages", tiff.pages.len())
            };
            let blocks = |ifd: &Ifd| -> Vec<Vec<u8>> {
                ifd.blocks.iter().map(|b| bytes_of(&file, b)).collect()
            };
            assert_eq!(blocks(first), [b"abcd".to_vec(), b"efgh".to_vec()]);
            assert_eq!(first.subs.len(), 1);
            assert_eq!(first.subs[0].len(), 2);
            assert_eq!(blocks(&first.subs[0][0]), [b"ij".to_vec()]);
            assert_eq!(blocks(&first.subs[0][0].subs[0][0]), [b"k".to_vec()]);
            assert_eq!(blocks(&first.subs[0][1]), [b"l".to_vec()]);
            assert_eq!(first.uint(256, tiff.layout.order), Some(4));
            let exif = first.exif.as_ref().unwrap();
            assert_eq!(exif.entries[0].tag, 0x9003);
            let interop = exif.interop.as_ref().unwrap();
            assert_eq!(interop.entries[0].tag, 1);
            assert_eq!(first.gps.as_ref().unwrap().entries[0].tag, 2);
            assert!(second.tiled && !first.tiled);
            assert_eq!(blocks(second), [vec![1; 256], vec![2; 256]]);
            // The pages, the SubIFD chain of two, and the one under it.
            assert_eq!(tiff.ifds().len(), 5);
            assert!(tiff.shared().is_empty());
            // Out-of-line values are read where they are.
            let date = exif.entries[0].clone();
            let mut r = Cursor::new(&file[..]);
            let value = read_value(&mut r, &date, 64).unwrap().unwrap();
            assert_eq!(value, b"2024:05:01 10:20:30\0");
            assert_eq!(read_value(&mut r, &date, 19).unwrap(), None);
        }
    }

    /// A 2-page RGB TIFF from the tiff crate, classic or BigTIFF.
    fn tiff_crate(big: bool) -> Vec<u8> {
        use tiff::encoder::{colortype::RGB8, TiffEncoder};
        let pixels: Vec<u8> = (0..30 * 20 * 3).map(|i| (i % 251) as u8).collect();
        let mut out = Cursor::new(Vec::new());
        if big {
            let mut encoder = TiffEncoder::new_big(&mut out).unwrap();
            for _ in 0..2 {
                encoder.write_image::<RGB8>(30, 20, &pixels).unwrap();
            }
        } else {
            let mut encoder = TiffEncoder::new(&mut out).unwrap();
            for _ in 0..2 {
                encoder.write_image::<RGB8>(30, 20, &pixels).unwrap();
            }
        }
        out.into_inner()
    }

    #[test]
    fn the_tiff_crates_files_read_as_it_wrote_them() {
        for big in [false, true] {
            let file = tiff_crate(big);
            let tiff = read(&file).unwrap();
            assert_eq!(tiff.layout.big, big);
            assert_eq!(tiff.pages.len(), 2);
            for page in &tiff.pages {
                let o = tiff.layout.order;
                assert_eq!((page.uint(256, o), page.uint(257, o)), (Some(30), Some(20)));
                let total: u64 = page.blocks.iter().map(|b| b.len).sum();
                assert_eq!(total, 30 * 20 * 3);
            }
        }
    }

    #[test]
    fn the_builders_files_decode_with_the_tiff_crate() {
        use tiff::decoder::{Decoder, DecodingResult};
        // Gray strips, then a tiled page, in every order and kind.
        for (order, big) in [(b'I', false), (b'M', false), (b'I', true), (b'M', true)] {
            let tiled = Page {
                tags: vec![
                    (256, V::Long(vec![20])),
                    (257, V::Long(vec![17])),
                    (258, V::Short(vec![8])),
                    (262, V::Short(vec![1])),
                    (322, V::Short(vec![16])),
                    (323, V::Short(vec![16])),
                ],
                blocks: (0..4).map(|n| B::Data(vec![n * 60; 256])).collect(),
                tiled: true,
                ..Page::default()
            };
            let pages = vec![
                gray(
                    3,
                    5,
                    2,
                    vec![B::data(&[1; 6]), B::data(&[2; 6]), B::data(&[3; 3])],
                ),
                tiled,
            ];
            let file = Builder::new(order, big, pages).build();
            let mut decoder = Decoder::new(Cursor::new(&file)).unwrap();
            let DecodingResult::U8(first) = decoder.read_image().unwrap() else {
                panic!("not 8-bit")
            };
            assert_eq!(
                first,
                [[1; 6], [2; 6]]
                    .concat()
                    .into_iter()
                    .chain([3; 3])
                    .collect::<Vec<_>>()
            );
            decoder.next_image().unwrap();
            let DecodingResult::U8(second) = decoder.read_image().unwrap() else {
                panic!("not 8-bit")
            };
            assert_eq!(second.len(), 20 * 17);
            assert_eq!((second[0], second[16], second[16 * 20]), (0, 60, 120));
            assert!(!decoder.more_images());
        }
    }

    #[test]
    fn a_main_chain_that_loops_or_leaves_the_file_ends_there() {
        for big in [false, true] {
            let builder = sample(b'I', big);
            let len = builder.build().len() as u64;
            // The last page's next: back to the first, or past the end.
            for next in [builder.header(), len + 100, len - 1] {
                let file = Builder {
                    last_next: Some(next),
                    ..sample(b'I', big)
                }
                .build();
                assert_eq!(read(&file).unwrap().pages.len(), 2, "{next}");
            }
            // Into a SubIFD: that IFD is two things.
            let tiff = read(&builder.build()).unwrap();
            let sub = tiff.pages[0].subs[0][0].at;
            let file = Builder {
                last_next: Some(sub),
                ..sample(b'I', big)
            }
            .build();
            assert!(matches!(read(&file), Err(Error::Broken(_))));
        }
    }

    /// `file` with the next offset of the IFD at `at` (of `n` entries) set
    /// to `to`.
    fn set_next(file: &mut [u8], big: bool, at: u64, n: u64, to: u64) {
        let l = Layout {
            order: Order::Little,
            big,
            len: 0,
        };
        let pos = (at + l.count_len() + n * l.entry_len()) as usize;
        if big {
            file[pos..pos + 8].copy_from_slice(&to.to_le_bytes());
        } else {
            file[pos..pos + 4].copy_from_slice(&(to as u32).to_le_bytes());
        }
    }

    #[test]
    fn an_ifd_reached_twice_off_the_main_chain_is_broken() {
        for big in [false, true] {
            let file = sample(b'I', big).build();
            let tiff = read(&file).unwrap();
            let first = &tiff.pages[0];
            let sub = &first.subs[0][1];
            // A SubIFD whose next is the page above it, or itself.
            for to in [first.at, sub.at] {
                let mut looped = file.clone();
                set_next(&mut looped, big, sub.at, sub.entries.len() as u64, to);
                assert!(matches!(read(&looped), Err(Error::Broken(_))), "{to}");
            }
            // A SubIFD outside the file.
            let mut outside = file.clone();
            set_next(&mut outside, big, sub.at, sub.entries.len() as u64, 1 << 30);
            assert!(matches!(read(&outside), Err(Error::Broken(_))));
        }
        // An Exif IFD that is the page itself.
        let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
        page.tags.push((34665, V::Long(vec![8])));
        let file = Builder::new(b'I', false, vec![page]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
    }

    #[test]
    fn an_exif_gps_or_interop_ifd_that_ifds_share_is_read_once() {
        for big in [false, true] {
            let tiff = read(&shared_meta(b'I', big)).unwrap();
            let [first, second] = &tiff.pages[..] else {
                panic!("{} pages", tiff.pages.len())
            };
            assert!(first.exif.as_ref().unwrap().interop.is_some());
            assert_eq!(second.exif, first.exif);
            assert_eq!(second.gps, first.gps);
        }
        // Two Exif IFDs that share an Interop IFD.
        let mut first = gray(1, 1, 1, vec![B::data(b"x")]);
        first.exif = Some(vec![(0xA001, V::Short(vec![1]))]);
        first.interop = Some(vec![(1, V::Ascii("R98"))]);
        let mut second = gray(1, 1, 1, vec![B::data(b"y")]);
        second.exif = Some(vec![(0xA001, V::Short(vec![1])), (40965, V::Long(vec![0]))]);
        let mut file = Builder::new(b'I', false, vec![first, second]).build();
        let tiff = read(&file).unwrap();
        let exifs: Vec<&Meta> = tiff.pages.iter().flat_map(|p| p.exif.as_ref()).collect();
        let interop = exifs[0].interop.as_ref().unwrap().at;
        let at = exifs[1].at;
        set_entry(&mut file, at, 40965, 1, interop);
        let tiff = read(&file).unwrap();
        let interops: Vec<_> = tiff
            .pages
            .iter()
            .map(|page| page.exif.as_ref().unwrap().interop.clone())
            .collect();
        assert!(interops[0].is_some());
        assert_eq!(interops[0], interops[1]);
    }

    #[test]
    fn an_ifd_that_is_two_kinds_of_ifd_is_broken() {
        let file = shared_meta(b'I', false);
        let tiff = read(&file).unwrap();
        let [first, second] = &tiff.pages[..] else {
            panic!("{} pages", tiff.pages.len())
        };
        let exif = first.exif.as_ref().unwrap();
        let gps = first.gps.as_ref().unwrap().at;
        // The second page's Exif pointer at the GPS IFD.
        let mut crossed = file.clone();
        set_entry(&mut crossed, second.at, 34665, 1, gps);
        assert!(matches!(read(&crossed), Err(Error::Broken(_))));
        // An Exif IFD that is its own Interop IFD.
        let mut own = file.clone();
        set_entry(&mut own, exif.at, 40965, 1, exif.at);
        assert!(matches!(read(&own), Err(Error::Broken(_))));
        // A GPS pointer at the other page.
        let mut page = file;
        set_entry(&mut page, second.at, 34853, 1, first.at);
        assert!(matches!(read(&page), Err(Error::Broken(_))));
    }

    #[test]
    fn an_exif_or_gps_ifd_that_cant_be_read_is_kept_as_unread() {
        let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
        page.tags.push((34665, V::Long(vec![1 << 30])));
        page.tags.push((34853, V::Short(vec![60])));
        let file = Builder::new(b'M', false, vec![page]).build();
        let tiff = read(&file).unwrap();
        let ifd = &tiff.pages[0];
        assert!(ifd.exif.as_ref().unwrap().unread);
        assert!(ifd.gps.as_ref().unwrap().unread);
        // A pointer of 0 is none.
        let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
        page.tags.push((34665, V::Long(vec![0])));
        let file = Builder::new(b'M', false, vec![page]).build();
        assert_eq!(read(&file).unwrap().pages[0].exif, None);
    }

    #[test]
    fn shared_reordered_and_empty_blocks_are_fine_and_other_overlaps_arent() {
        let page = gray(
            2,
            4,
            1,
            vec![B::data(b"ab"), B::Same(0), B::Empty, B::data(b"cd")],
        );
        let builder = Builder {
            reversed: true,
            tail: b"orphaned".to_vec(),
            ..Builder::new(b'I', false, vec![page])
        };
        let file = builder.build();
        let tiff = read(&file).unwrap();
        let blocks = &tiff.pages[0].blocks;
        assert_eq!(blocks[0], blocks[1]);
        assert_eq!(tiff.shared(), [blocks[0]]);
        assert_eq!(blocks[2], Block { offset: 0, len: 0 });
        assert!(blocks[3].offset < blocks[0].offset);
        assert_eq!(bytes_of(&file, &blocks[3]), b"cd");
        // A block twice in a row, in the file's order: listed as it is.
        let page = gray(2, 3, 1, vec![B::data(b"ab"), B::Same(0), B::data(b"cd")]);
        let file = Builder::new(b'I', false, vec![page]).build();
        let tiff = read(&file).unwrap();
        assert_eq!(tiff.shared(), [tiff.pages[0].blocks[0]]);
        // And one that runs into the next after it.
        let mut over = gray(2, 4, 1, vec![B::data(b"ab"), B::Same(0), B::data(b"cd")]);
        over.blocks.push(B::data(b"ef"));
        over.counts = Some(vec![3, 3, 2, 2]);
        let file = Builder::new(b'I', false, vec![over]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
        // A strip that runs one byte into the next.
        let page = gray(2, 2, 1, vec![B::data(b"ab"), B::data(b"cd")]);
        let mut page_over = page.clone();
        page_over.counts = Some(vec![3, 2]);
        let file = Builder::new(b'I', false, vec![page_over]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
        // A strip over the IFD.
        let mut into_ifd = page;
        into_ifd.offsets = Some(vec![8, 10]);
        into_ifd.counts = Some(vec![2, 2]);
        let file = Builder::new(b'I', false, vec![into_ifd]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
    }

    #[test]
    fn a_block_that_reaches_into_a_value_is_refused() {
        let refused =
            |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == MISSIZED);
        let broken = |file: &[u8]| matches!(read(file), Err(Error::Broken(_)));
        // 300 × 100 with only 20,000 bytes of its picture, then an XMP
        // packet (700) of 13,500 bytes; `strips` strips listed from the
        // picture's start, each a strip's size apart, with `counts`.
        let xmp = b"<x:xmpmeta>a secret</x:xmpmeta>".repeat(450);
        let packet = |strips: u64, counts: Vec<u64>| {
            let rows = 100 / strips;
            let mut page = gray(300, 100, rows as u32, vec![B::data(&picture()[..20_000])]);
            page.tags.push((700, V::Bytes(1, vec![0; 8])));
            page.counts = Some(counts);
            page.offsets = Some(vec![0; strips as usize]);
            let build = |page: Page| {
                let pages = vec![page];
                let tail = xmp.clone();
                Builder {
                    tail,
                    ..Builder::new(b'I', false, pages)
                }
                .build()
            };
            let at = (build(page.clone()).len() - xmp.len() - 20_000) as u64;
            page.offsets = Some((0..strips).map(|i| at + i * 300 * rows).collect());
            let mut file = build(page);
            set_entry(&mut file, 8, 700, xmp.len() as u64, at + 20_000);
            file
        };
        // A lone strip counted 0 ("Bogus") is worked out to the whole
        // picture, and so are ten counted "Wrong" to 3,000 bytes each:
        // they run into the packet, which libtiff reads as pixels.
        assert!(refused(&packet(1, vec![0])));
        assert!(refused(&packet(10, [3_000, 1_500].repeat(5))));
        // Counted so at their word, they're broken.
        assert!(broken(&packet(10, vec![3_000; 10])));
        // A MakerNote in an Exif IFD, pointed into a strip.
        let mut page = gray(30, 10, 10, vec![B::data(&picture()[..300])]);
        page.exif = Some(vec![(0x927C, V::Bytes(7, b"a maker note".to_vec()))]);
        let mut file = Builder::new(b'M', true, vec![page]).build();
        let tiff = read(&file).unwrap();
        let strip = tiff.pages[0].blocks[0];
        let exif = tiff.pages[0].exif.as_ref().unwrap().at;
        set_entry(&mut file, exif, 0x927C, 12, strip.offset + 280);
        assert!(broken(&file));
    }

    fn lens(tiff: &Tiff) -> Vec<u64> {
        tiff.pages[0].blocks.iter().map(|b| b.len).collect()
    }

    #[test]
    fn byte_counts_libtiff_ignores_are_worked_out_from_the_picture() {
        // A lone strip whose count is 0, half the picture, or past the
        // file's end: libtiff's "Bogus StripByteCounts".
        for count in [0, 15_000, 29_999, 1 << 40] {
            let tiff = read(&miscounted(1, 1, vec![count])).unwrap();
            assert_eq!(lens(&tiff), [30_000], "{count}");
        }
        // More of it than the picture is taken at its word.
        let mut longer = miscounted(1, 1, vec![30_001]);
        longer.push(0);
        assert_eq!(lens(&read(&longer).unwrap()), [30_001]);
        // Strips whose first two counts differ and aren't 0: "Wrong
        // StripByteCounts", the last strip of what's left.
        let tiff = read(&miscounted(10, 1, [3_000, 1_500].repeat(5))).unwrap();
        assert_eq!(lens(&tiff), [3_000; 10]);
        let tiff = read(&miscounted(3, 1, vec![12_000, 6_000, 1])).unwrap();
        assert_eq!(lens(&tiff), [10_200, 10_200, 9_600]);
        // Tiles too: 16 × 16 over 64 × 48, 12 of 256 bytes.
        let tiled = Page {
            tags: vec![
                (256, V::Long(vec![64])),
                (257, V::Long(vec![48])),
                (258, V::Short(vec![8])),
                (262, V::Short(vec![1])),
                (322, V::Short(vec![16])),
                (323, V::Short(vec![16])),
            ],
            blocks: (0..12).map(|n| B::Data(vec![n; 256])).collect(),
            tiled: true,
            counts: Some([256, 128].repeat(6)),
            ..Page::default()
        };
        let file = Builder::new(b'M', true, vec![tiled]).build();
        assert_eq!(lens(&read(&file).unwrap()), [256; 12]);
        // Worked out past the file's end, it's cut short.
        let mut cut = miscounted(1, 1, vec![0]);
        cut.truncate(cut.len() - 1);
        assert!(matches!(read(&cut), Err(Error::Broken(_))));
    }

    #[test]
    fn byte_counts_libtiff_trusts_and_readers_dont_are_refused() {
        let refused =
            |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == MISSIZED);
        for (strips, compression, counts) in [
            // A first count of 0, or a short strip after two that agree.
            (10, 1, [&[0][..], &[3_000; 9]].concat()),
            (10, 1, [&[3_000, 3_000, 1_500][..], &[3_000; 7]].concat()),
            // Two strips: neither "Bogus" nor "Wrong" to libtiff.
            (2, 1, vec![15_000, 7_500]),
            (2, 1, vec![15_000, 0]),
            // LZW (the bytes don't matter): 0 at a nonzero offset, which
            // libtiff guesses at from the file's size, IFDs and all.
            (1, 5, vec![0]),
            (2, 5, vec![15_000, 0]),
        ] {
            assert!(
                refused(&miscounted(strips, compression, counts.clone())),
                "{counts:?}"
            );
        }
        // Planes apart: libtiff trusts them, and a short one is refused.
        let mut planar = gray(
            100,
            100,
            100,
            (0..3).map(|n| B::Data(vec![n; 10_000])).collect(),
        );
        planar
            .tags
            .extend([(277, V::Short(vec![3])), (284, V::Short(vec![2]))]);
        planar.counts = Some(vec![10_000, 5_000, 10_000]);
        let file = Builder::new(b'I', false, vec![planar.clone()]).build();
        assert!(refused(&file));
        planar.counts = None;
        let file = Builder::new(b'I', false, vec![planar]).build();
        assert_eq!(lens(&read(&file).unwrap()), [10_000; 3]);
        // A lone strip at 0 of 0 bytes is empty, as GDAL leaves a sparse
        // one, whatever the compression.
        for compression in [1, 5] {
            let mut sparse = gray(300, 100, 100, vec![B::Empty]);
            sparse.tags.push((259, V::Short(vec![compression])));
            let tiff = read(&Builder::new(b'I', false, vec![sparse]).build()).unwrap();
            assert_eq!(tiff.pages[0].blocks, [Block { offset: 0, len: 0 }]);
        }
    }

    #[test]
    fn byte_counts_are_checked_on_the_strips_libtiff_counts() {
        let refused =
            |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == MISSIZED);
        // 300 × 100, uncompressed, the first `n` of its ten 3,000-byte
        // strips listed with `counts`, and RowsPerStrip `rows` or none.
        let listed = |n: usize, rows: Option<u32>, counts: Vec<u64>| {
            let strips = picture().chunks(3_000).take(n).map(B::data).collect();
            let mut page = gray(300, 100, 10, strips);
            page.tags.retain(|t| t.0 != 278);
            page.tags
                .extend(rows.map(|rows| (278, V::Long(vec![rows]))));
            page.counts = Some(counts);
            Builder::new(b'I', false, vec![page]).build()
        };
        let full = vec![3_000; 10];
        // More than libtiff's: with no RowsPerStrip it reads one strip,
        // the first, whose count it ignores as short of the picture
        // ("Bogus") and reads the whole picture from there.
        let mut short = full.clone();
        short[3] = 10;
        assert!(refused(&listed(10, None, short)));
        let mut last = full.clone();
        last[9] = 1_500;
        assert!(refused(&listed(10, None, last)));
        // In strips of 34 rows it reads three, the first short of its rows.
        assert!(refused(&listed(10, Some(34), full.clone())));
        // Fewer: in strips of 10 rows it reads ten, the eight missing at 0,
        // and ignores counts whose first two differ ("Wrong").
        assert!(refused(&listed(2, Some(10), vec![3_000, 1_500])));
        // Taken at their word, they're copied as they are: five of ten, or
        // the whole picture as the strip libtiff reads, and a block after.
        let five = read(&listed(5, Some(10), vec![3_000; 5])).unwrap();
        assert_eq!(lens(&five), [3_000; 5]);
        let mut page = gray(300, 100, 100, vec![B::Data(picture()), B::data(&[9; 100])]);
        page.tags.retain(|t| t.0 != 278);
        let file = Builder::new(b'I', false, vec![page]).build();
        assert_eq!(lens(&read(&file).unwrap()), [30_000, 100]);
    }

    #[test]
    fn blocks_listed_as_the_other_kind_are_checked_as_libtiff_reads_them() {
        let refused =
            |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == MISSIZED);
        // Listed as tiles with no tile size, which libtiff reads as strips:
        // ten, the fourth counted 10, with no RowsPerStrip, where it reads
        // one, short of the picture, and the picture from it.
        let mut counts = vec![3_000; 10];
        counts[3] = 10;
        let mut page = gray(300, 100, 10, picture().chunks(3_000).map(B::data).collect());
        page.tags.retain(|t| t.0 != 278);
        page.tiled = true;
        page.counts = Some(counts);
        assert!(refused(&Builder::new(b'I', false, vec![page]).build()));
        // One counted 10, of a 90 × 90 picture: worked out to it, as libtiff
        // works it out.
        let mut page = gray(90, 90, 90, vec![B::Data(picture()[..8_100].to_vec())]);
        page.tiled = true;
        page.counts = Some(vec![10]);
        let tiff = read(&Builder::new(b'I', true, vec![page]).build()).unwrap();
        assert_eq!(lens(&tiff), [8_100]);
        // Listed as strips beside a tile size, which libtiff reads as
        // tiles: four 32 × 32 tiles of 64 × 64, one short of its 1,024
        // bytes, refused; their first two counts differ ("Wrong"), worked
        // out as tiles.
        let tiles = |counts: Vec<u64>| {
            let mut page = gray(
                64,
                64,
                16,
                picture()[..4_096].chunks(1_024).map(B::data).collect(),
            );
            page.tags
                .extend([(322, V::Short(vec![32])), (323, V::Short(vec![32]))]);
            page.counts = Some(counts);
            Builder::new(b'I', false, vec![page]).build()
        };
        assert!(refused(&tiles(vec![1_024, 1_024, 10, 1_024])));
        let tiff = read(&tiles(vec![1_024, 512, 1_024, 1_024])).unwrap();
        assert_eq!(lens(&tiff), [1_024; 4]);
        // With no counts at all, they're sized as libtiff reads them too.
        let tiff = read(&tiles(Vec::new())).unwrap();
        assert_eq!(lens(&tiff), [1_024; 4]);
    }

    /// An entry for `tag` of type `kind` holding `bytes` inline, `count`
    /// values of it.
    fn inline(tag: u16, kind: u16, count: u64, value: &[u8]) -> Entry {
        let mut bytes = [0; 8];
        bytes[..value.len()].copy_from_slice(value);
        let len = value.len() as u8;
        Entry {
            tag,
            kind,
            count,
            value: Value::Inline { bytes, len },
        }
    }

    #[test]
    fn a_layout_number_is_read_as_libtiff_reads_a_short_or_a_long() {
        let mut r = Cursor::new(vec![0, 0, 100, 0, 0, 0, 0, 0, 0, 0]);
        let number = |r: &mut Cursor<Vec<u8>>, entry: &Entry, max: u64| {
            number(r, entry, Order::Little, max).unwrap()
        };
        let long = u64::from(u32::MAX);
        // Any integer type, signed ones not negative.
        for (kind, bytes, want) in [
            (1, &[200][..], Some(200)),
            (6, &[100], Some(100)),
            (6, &[0x80], None),
            (3, &[0x34, 0x12], Some(0x1234)),
            (8, &[0xFE, 0xFF], None),
            (8, &[0xFF, 0x7F], Some(0x7FFF)),
            (4, &[1, 0, 1, 0], Some(0x1_0001)),
            (9, &[0, 0, 0, 0x80], None),
            (16, &[5, 0, 0, 0, 0, 0, 0, 0], Some(5)),
            (17, &[0, 0, 0, 0, 0, 0, 0, 0x80], None),
            // A LONG8 past 32 bits; and IFD, IFD8, RATIONAL and ASCII.
            (16, &[0, 0, 0, 0, 1, 0, 0, 0], None),
            (13, &[8, 0, 0, 0], None),
            (18, &[8, 0, 0, 0, 0, 0, 0, 0], None),
            (5, &[1, 0, 0, 0, 1, 0, 0, 0], None),
            (2, &[b'1', 0], None),
        ] {
            let count = if kind == 2 { 2 } else { 1 };
            let entry = inline(256, kind, count, bytes);
            assert_eq!(number(&mut r, &entry, long), want, "{kind} {bytes:?}");
        }
        // Read as a SHORT, a LONG past 16 bits isn't one.
        let wide = inline(277, 4, 1, &[0x70, 0x11, 1, 0]);
        assert_eq!(number(&mut r, &wide, u64::from(u16::MAX)), None);
        // Two values aren't one.
        assert_eq!(
            number(&mut r, &inline(277, 3, 2, &[3, 0, 3, 0]), long),
            None
        );
        // A classic file's LONG8 and SLONG8 out of line, read from the file.
        for kind in [16, 17] {
            let at = Entry {
                tag: 257,
                kind,
                count: 1,
                value: Value::At { offset: 2, len: 8 },
            };
            assert_eq!(number(&mut r, &at, long), Some(100));
        }
    }

    #[test]
    fn a_per_sample_tag_is_read_as_libtiff_reads_compression() {
        // At 2, three SHORTs: 7, 7 and 5.
        let mut r = Cursor::new(vec![0, 0, 7, 0, 7, 0, 5, 0]);
        let mut read = |entry: &Entry, samples: Option<u64>| {
            let mut left = u64::MAX;
            per_sample(&mut r, entry, Order::Little, samples, &mut left).unwrap()
        };
        // One value of any integer type libtiff reads, whatever the
        // samples, signed ones not negative.
        for (kind, bytes) in [(1, &[7][..]), (6, &[7]), (8, &[7, 0]), (9, &[7, 0, 0, 0])] {
            assert_eq!(read(&inline(259, kind, 1, bytes), None), Some(7), "{kind}");
        }
        assert_eq!(read(&inline(259, 8, 1, &[0xF9, 0xFF]), Some(1)), None);
        // More: at least SamplesPerPixel of them, the first that many the
        // same, every one a SHORT's.
        let two = inline(259, 3, 2, &[7, 0, 7, 0]);
        assert_eq!(read(&two, Some(1)), Some(7));
        assert_eq!(read(&two, Some(2)), Some(7));
        assert_eq!(read(&two, Some(3)), None);
        assert_eq!(read(&two, None), None);
        let three = Entry {
            tag: 258,
            kind: 3,
            count: 3,
            value: Value::At { offset: 2, len: 6 },
        };
        assert_eq!(read(&three, Some(2)), Some(7));
        assert_eq!(read(&three, Some(3)), None);
        let past = inline(259, 4, 2, &[7, 0, 0, 0, 0, 0, 1, 0]);
        assert_eq!(read(&past, Some(1)), None);
        let negative = inline(259, 8, 2, &[7, 0, 0xFF, 0xFF]);
        assert_eq!(read(&negative, Some(1)), None);
        assert_eq!(read(&inline(259, 3, 0, &[]), Some(1)), None);
        assert_eq!(read(&inline(259, 5, 1, &[7, 0, 0, 0]), Some(1)), None);
        // What's read out of line comes out of the budget.
        let mut left = 6;
        let mut r = Cursor::new(vec![0, 0, 7, 0, 7, 0, 5, 0]);
        let got = per_sample(&mut r, &three, Order::Little, Some(1), &mut left);
        assert_eq!((got.unwrap(), left), (Some(7), 0));
        let got = per_sample(&mut r, &three, Order::Little, Some(1), &mut left);
        assert!(matches!(got, Err(Error::TooLarge(_))), "{got:?}");
    }

    #[test]
    fn ycbcr_subsampling_is_two_numbers_as_libtiff_reads_them() {
        let mut r = Cursor::new(Vec::new());
        let mut read = |entry: &Entry| {
            let mut left = u64::MAX;
            pair(&mut r, entry, Order::Little, &mut left).unwrap()
        };
        assert_eq!(read(&inline(530, 3, 2, &[2, 0, 1, 0])), Some((2, 1)));
        assert_eq!(read(&inline(530, 1, 2, &[4, 2])), Some((4, 2)));
        assert_eq!(read(&inline(530, 8, 2, &[2, 0, 2, 0])), Some((2, 2)));
        assert_eq!(read(&inline(530, 8, 2, &[2, 0, 0xFE, 0xFF])), None);
        assert_eq!(read(&inline(530, 3, 1, &[2, 0])), None);
        assert_eq!(read(&inline(530, 1, 3, &[2, 2, 2])), None);
    }

    #[test]
    fn compression_is_read_as_libtiff_reads_it_and_refused_where_it_cant() {
        // A gray 8 × 8 page of one PackBits strip, its Compression `value`,
        // and `samples` samples a pixel.
        let file = |value: V, samples: u16| {
            let mut page = gray(8, 8, 8, vec![B::data(b"\x3F packbits")]);
            page.tags
                .extend([(259, value), (277, V::Short(vec![samples]))]);
            Builder::new(b'I', false, vec![page]).build()
        };
        let compression =
            |value: V, samples: u16| read(&file(value, samples)).map(|t| t.pages[0].compression);
        let le = |n: u16| n.to_le_bytes().to_vec();
        assert_eq!(
            compression(V::Bytes(8, le(32773)[..2].to_vec()), 1).ok(),
            None
        );
        assert_eq!(compression(V::Bytes(8, le(5)), 1).unwrap(), 5);
        assert_eq!(
            compression(V::Bytes(9, 5u32.to_le_bytes().to_vec()), 1).unwrap(),
            5
        );
        assert_eq!(compression(V::Short(vec![5, 5, 5]), 3).unwrap(), 5);
        assert_eq!(compression(V::Short(vec![5, 5, 1]), 2).unwrap(), 5);
        // Where libtiff can't read it, nor so the file: refused.
        let unread = |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == UNREAD_COMPRESSION);
        for (value, samples) in [
            (V::Short(vec![5, 1]), 2),
            (V::Short(vec![5, 5]), 3),
            (V::Bytes(8, le(0xFFFB)), 1),
            (V::Long(vec![70_000]), 1),
            (V::Rational(vec![(5, 1)]), 1),
        ] {
            assert!(unread(&file(value.clone(), samples)), "{value:?}");
        }
        // An old-style JPEG, 6 in any of those, is refused as one.
        for (value, samples) in [
            (V::Bytes(8, le(6)), 1),
            (V::Bytes(16, 6u64.to_le_bytes().to_vec()), 1),
            (V::Short(vec![6, 6, 6]), 3),
        ] {
            let got = read(&file(value.clone(), samples));
            assert!(
                matches!(&got, Err(Error::Refused(why)) if why.contains("old kind")),
                "{value:?}: {got:?}"
            );
        }
    }

    #[test]
    fn bits_per_sample_and_photometric_are_read_as_libtiff_reads_them() {
        // A gray 300 × 100 page in ten strips of 10 rows, the fourth short:
        // refused, whatever integer type libtiff reads its BitsPerSample
        // in; and with its count of 10, at 8 bits, one strip of no
        // RowsPerStrip: worked out to the picture.
        let refused =
            |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == MISSIZED);
        for (big, bits) in [
            (false, V::Bytes(1, vec![8])),
            (false, V::Bytes(6, vec![8])),
            (false, V::Bytes(8, 8u16.to_le_bytes().to_vec())),
            (false, V::Bytes(9, 8u32.to_le_bytes().to_vec())),
            (true, V::Bytes(17, 8u64.to_le_bytes().to_vec())),
            (false, V::Short(vec![8, 8])),
        ] {
            let mut page = gray(300, 100, 10, picture().chunks(3_000).map(B::data).collect());
            page.tags[2] = (258, bits.clone());
            let mut counts = vec![3_000; 10];
            counts[3] = 10;
            page.counts = Some(counts);
            assert!(
                refused(&Builder::new(b'I', big, vec![page.clone()]).build()),
                "{bits:?}"
            );
            let mut lone = page;
            lone.tags.retain(|t| t.0 != 278);
            lone.blocks = vec![B::Data(picture())];
            lone.counts = Some(vec![10]);
            let tiff = read(&Builder::new(b'I', big, vec![lone]).build()).unwrap();
            assert_eq!(lens(&tiff), [30_000], "{bits:?}");
        }
        // RGB 30 × 20, its Compression a SHORT for each sample: one strip
        // counted 1,000 of its 1,800 bytes, which libtiff ignores, and ten
        // of two rows, counted 180, 179 and on, which it ignores too.
        let mut rgb = gray(30, 20, 20, vec![B::Data(vec![7; 1_800])]);
        rgb.tags[2] = (258, V::Short(vec![8, 8, 8]));
        rgb.tags[3] = (262, V::Bytes(8, 2u16.to_le_bytes().to_vec()));
        rgb.tags
            .extend([(259, V::Short(vec![1, 1, 1])), (277, V::Short(vec![3]))]);
        rgb.counts = Some(vec![1_000]);
        let tiff = read(&Builder::new(b'I', false, vec![rgb.clone()]).build()).unwrap();
        assert_eq!(lens(&tiff), [1_800]);
        rgb.tags[4] = (278, V::Long(vec![2]));
        rgb.blocks = (0..10).map(|n| B::Data(vec![n; 180])).collect();
        rgb.counts = Some([180, 179].repeat(5));
        let tiff = read(&Builder::new(b'I', false, vec![rgb]).build()).unwrap();
        assert_eq!(lens(&tiff), [180; 10]);
    }

    /// An image IFD of `entries` (numbers inline, little-endian), listing
    /// strips.
    fn ifd_of(entries: &[(u16, u16, u64)]) -> Ifd {
        let size = |kind| type_size(kind).unwrap_or(4) as usize;
        Ifd {
            at: 8,
            entries: entries
                .iter()
                .map(|&(tag, kind, n)| inline(tag, kind, 1, &n.to_le_bytes()[..size(kind)]))
                .collect(),
            compression: 1,
            blocks: Vec::new(),
            tiled: false,
            estimated: false,
            subs: Vec::new(),
            exif: None,
            gps: None,
        }
    }

    fn dims(entries: &[(u16, u16, u64)]) -> Result<Dims, String> {
        Dims::of(
            &mut Cursor::new(Vec::new()),
            &ifd_of(entries),
            Order::Little,
        )
        .unwrap()
    }

    #[test]
    fn the_layout_and_its_count_are_libtiffs() {
        let size = [(256, 4, 300), (257, 4, 100)];
        let with = |more: &[(u16, u16, u64)]| [&size[..], more].concat();
        let count = |more: &[(u16, u16, u64)]| dims(&with(more)).map(|d| (d.count(), d.tiled()));
        // One strip with no RowsPerStrip, or one of 2^32 - 1 rows; ImageLength
        // over it otherwise, in each plane when they're apart.
        assert_eq!(count(&[]), Ok((1, false)));
        assert_eq!(count(&[(278, 4, u64::from(u32::MAX))]), Ok((1, false)));
        assert_eq!(count(&[(278, 3, 30)]), Ok((4, false)));
        let planes = [(277, 3, 3), (284, 3, 2), (278, 3, 10)];
        assert_eq!(count(&planes), Ok((30, false)));
        // An ImageDepth: strips ignore it; tiles are counted deep, over the
        // TileDepth. A TileWidth or TileLength makes tiles, whatever the
        // blocks are listed under; one of 2^32 - 1 is the picture's.
        assert_eq!(count(&[(32997, 4, 2)]), Ok((1, false)));
        let tiles = [(322, 3, 32), (323, 3, 32)];
        assert_eq!(count(&tiles), Ok((40, true)));
        assert_eq!(
            count(&[&tiles[..], &[(32997, 4, 2)]].concat()),
            Ok((80, true))
        );
        let deep = [&tiles[..], &[(32997, 4, 5), (32998, 4, 2)]].concat();
        assert_eq!(count(&deep), Ok((120, true)));
        assert_eq!(
            count(&[(322, 4, 32), (323, 4, u64::from(u32::MAX))]),
            Ok((10, true))
        );
        // What libtiff can't read: a tag it can't read, no size, 0 for a
        // size, the samples, the rows or the tile depth, another planar
        // configuration, and no strips or tiles by its count (a lone
        // TileWidth, or a count past 32 bits).
        for (more, why) in [
            (vec![(278, 5, 0)], "RowsPerStrip can't be read"),
            (vec![(277, 4, 70_000)], "SamplesPerPixel can't be read"),
            (vec![(277, 3, 0)], "SamplesPerPixel is 0"),
            (vec![(284, 3, 3)], "PlanarConfiguration is 3"),
            (vec![(278, 3, 0)], "RowsPerStrip is 0"),
            (vec![(32998, 4, 0), (322, 3, 16)], "TileDepth is 0"),
            (vec![(322, 3, 16)], "no strips or tiles"),
        ] {
            let got = count(&more).unwrap_err();
            assert!(got.contains(why), "{why}: {got}");
        }
        let huge = [
            (256, 4, 1),
            (257, 4, 1 << 31),
            (277, 3, 4),
            (284, 3, 2),
            (278, 3, 1),
        ];
        assert!(dims(&huge).unwrap_err().contains("no strips or tiles"));
        assert_eq!(dims(&[(258, 3, 8)]), Err("it has no size".into()));
        assert!(dims(&[(257, 4, 100)]).unwrap_err().contains("0 × 100"));
        assert!(dims(&[(256, 4, 300), (257, 4, 0)])
            .unwrap_err()
            .contains("300 × 0"));
    }

    #[test]
    fn a_tile_size_left_out_is_libtiffs_from_a_rowsperstrip_listed_first() {
        // 64 × 64, its entries in the order given (as libtiff sets them).
        let count = |entries: &[(u16, u16, u64)]| dims(entries).map(|d| (d.count(), d.tile()));
        let with = |more: &[(u16, u16, u64)]| [&[(256, 4, 64), (257, 4, 64)][..], more].concat();
        // A RowsPerStrip before a TileLength: tiles as wide as the picture,
        // so that strips of 16 rows are its four tiles; before a TileWidth,
        // tiles of its rows; before both, both its own.
        let length = with(&[(278, 3, 16), (323, 3, 16)]);
        assert_eq!(count(&length), Ok((4, Some((64, 16)))));
        let width = with(&[(278, 3, 16), (322, 3, 32)]);
        assert_eq!(count(&width), Ok((8, Some((32, 16)))));
        let both = with(&[(278, 3, 16), (322, 3, 32), (323, 3, 32)]);
        assert_eq!(count(&both), Ok((4, Some((32, 32)))));
        // A RowsPerStrip of 2^32 - 1: tiles as long as the picture.
        let whole = with(&[(278, 4, u64::from(u32::MAX)), (323, 3, 16)]);
        assert_eq!(count(&whole), Ok((4, Some((64, 16)))));
        let whole = with(&[(278, 4, u64::from(u32::MAX)), (322, 3, 16)]);
        assert_eq!(count(&whole), Ok((4, Some((16, 64)))));
        // A RowsPerStrip after the tile size sets nothing, nor one before
        // the ImageWidth a TileWidth: libtiff counts no tiles.
        assert!(count(&with(&[(323, 3, 16), (278, 3, 16)])).is_err());
        let early = [(278, 3, 16), (256, 4, 64), (257, 4, 64), (323, 3, 16)];
        assert!(count(&early).is_err());
        let early = [(257, 4, 64), (278, 3, 16), (256, 4, 64), (322, 3, 32)];
        assert_eq!(count(&early), Ok((8, Some((32, 16)))));
    }

    #[test]
    fn a_layout_libtiff_cant_read_is_refused_but_for_one_compressed_block() {
        let refused =
            |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == MISSIZED);
        let unread =
            |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == UNREAD_LAYOUT);
        // Ten strips of a page whose RowsPerStrip is a RATIONAL, which
        // libtiff can't read: which strips it would read can't be told.
        let strips = picture().chunks(3_000).map(B::data).collect();
        let mut page = gray(300, 100, 10, strips);
        page.tags[4] = (278, V::Rational(vec![(10, 1)]));
        assert!(unread(&Builder::new(b'I', false, vec![page]).build()));
        // A lone uncompressed strip where libtiff counts no strips or tiles
        // (a RowsPerStrip of 2^32 - 2, a lone TileWidth), or can't read the
        // layout: Pillow reads the whole picture from it, which Grape can't
        // size as libtiff would, so a strip counted short of it can't be
        // told from a whole one. Refused, short or whole, as a layout Grape
        // can't read.
        let lone = |tags: &[(u16, V)], count: u64| {
            let mut page = gray(300, 100, 100, vec![B::Data(picture())]);
            page.tags.retain(|t| t.0 != 278);
            page.tags.extend(tags.iter().cloned());
            page.counts = Some(vec![count]);
            Builder::new(b'I', false, vec![page]).build()
        };
        for tags in [
            vec![(278, V::Long(vec![u32::MAX - 1]))],
            vec![(278, V::Rational(vec![(100, 1)]))],
            vec![(322, V::Long(vec![304]))],
        ] {
            for count in [10, 30_000] {
                assert!(unread(&lone(&tags, count)), "{tags:?} {count}");
            }
        }
        // A TileWidth or a TileLength beside a RowsPerStrip, which libtiff
        // reads as one tile of 304 × 100, or the picture's 300 × 100:
        // refused short of it, and taken whole.
        let rows = (278, V::Long(vec![100]));
        let wide = [rows.clone(), (322, V::Long(vec![304]))];
        assert!(refused(&lone(&wide, 10)));
        assert!(refused(&lone(&wide, 30_000)));
        let long = [rows, (323, V::Short(vec![100]))];
        assert!(refused(&lone(&long, 10)));
        assert_eq!(lens(&read(&lone(&long, 30_000)).unwrap()), [30_000]);
        // A lone compressed one is the one libtiff would read, as it is.
        let mut page = gray(300, 100, 100, vec![B::Data(picture())]);
        page.tags[4] = (278, V::Rational(vec![(100, 1)]));
        page.tags.push((259, V::Short(vec![5])));
        let tiff = read(&Builder::new(b'I', false, vec![page]).build()).unwrap();
        assert_eq!(lens(&tiff), [30_000]);
    }

    #[test]
    fn sizing_tags_libtiff_cant_read_are_refused_however_right_the_counts() {
        // A gray 300 × 100 page in ten strips of 10 rows, each counted its
        // 3,000 bytes, with a PhotometricInterpretation, BitsPerSample,
        // PlanarConfiguration or RowsPerStrip libtiff can't read or count:
        // its counts are right, but it can't be sized as libtiff sizes it,
        // and other readers may read it otherwise.
        let unread =
            |file: &[u8]| matches!(read(file), Err(Error::Refused(why)) if why == UNREAD_LAYOUT);
        let whole = || gray(300, 100, 10, picture().chunks(3_000).map(B::data).collect());
        assert!(read(&Builder::new(b'I', false, vec![whole()]).build()).is_ok());
        for (tag, value) in [
            (262, V::Rational(vec![(1, 1)])),
            (262, V::Short(vec![1, 1])),
            (258, V::Rational(vec![(8, 1)])),
            (284, V::Short(vec![3])),
            (278, V::Long(vec![u32::MAX - 1])),
        ] {
            let mut page = whole();
            page.tags.retain(|t| t.0 != tag);
            page.tags.push((tag, value.clone()));
            let file = Builder::new(b'I', false, vec![page]).build();
            assert!(unread(&file), "{tag} {value:?}: {:?}", read(&file));
        }
    }

    #[test]
    fn subsampled_ycbcr_is_sized_in_sampling_units() {
        // 5 × 3 in one strip: subsampled 2 × 2 (the default), 3 units of
        // 6 bytes a row, in 2 rows of them; 4 × 2, 2 units of 10 bytes;
        // 1 × 1, three samples a pixel. libtiff takes a lone strip's count
        // at its word from its scanline's bytes times the rows (a row of
        // units over the vertical subsampling, rounded down): from 27, 30
        // and 45 bytes.
        for (sub, len, lone) in [
            (None, 36, 27),
            (Some(vec![4, 2]), 40, 30),
            (Some(vec![1, 1]), 45, 45),
        ] {
            let mut page = gray(5, 3, 3, vec![B::Data(vec![7; len as usize])]);
            page.tags[2] = (258, V::Short(vec![8, 8, 8]));
            page.tags[3] = (262, V::Short(vec![6]));
            page.tags.push((277, V::Short(vec![3])));
            page.tags.extend(sub.map(|s| (530, V::Short(s))));
            let file = |counts: Vec<u64>| {
                let mut page = page.clone();
                page.counts = Some(counts);
                Builder::new(b'I', false, vec![page]).build()
            };
            // With no byte count, and with one short of libtiff's figure,
            // it's worked out to the strip's size.
            for counts in [Vec::new(), vec![lone - 1]] {
                assert_eq!(lens(&read(&file(counts)).unwrap()), [len]);
            }
            // From there, short of the strip, libtiff can't read it.
            for count in lone..len {
                let refused = read(&file(vec![count]));
                assert!(
                    matches!(&refused, Err(Error::Refused(why)) if why == MISSIZED),
                    "{count}: {refused:?}"
                );
            }
        }
    }

    #[test]
    fn bits_per_sample_counts_as_many_values_as_samples() {
        // RGB, 10 × 7 in strips of 3 rows, its BitsPerSample of four
        // values: libtiff reads the first three, 30 bytes a row, so the
        // strips are 90, 90 and 30 bytes, counted or worked out.
        let mut page = gray(
            10,
            7,
            3,
            vec![B::data(&[1; 90]), B::data(&[2; 90]), B::data(&[3; 30])],
        );
        page.tags[1] = (257, V::Long(vec![7]));
        page.tags[2] = (258, V::Short(vec![8; 4]));
        page.tags[3] = (262, V::Short(vec![2]));
        page.tags.push((277, V::Short(vec![3])));
        for counts in [None, Some(Vec::new())] {
            let mut page = page.clone();
            page.counts = counts;
            let tiff = read(&Builder::new(b'I', false, vec![page]).build()).unwrap();
            assert_eq!(lens(&tiff), [90, 90, 30]);
        }
        // Fewer than the samples, but for one, can't be read.
        page.tags[2] = (258, V::Short(vec![8, 8]));
        page.counts = Some(Vec::new());
        let file = Builder::new(b'I', false, vec![page]).build();
        assert!(matches!(read(&file), Err(Error::Broken(why)) if why.contains("BitsPerSample")));
    }

    #[test]
    fn bits_per_sample_is_read_at_most_the_files_size_in_all() {
        // Pages that don't say their byte counts, each pointing its
        // BitsPerSample at one array of 1,000 numbers: 8, then zeros.
        let mut page = gray(1, 1, 1, vec![B::data(&[7])]);
        page.counts = Some(Vec::new());
        let share = |n: usize| {
            let mut file = Builder::new(b'I', false, vec![page.clone(); n]).build();
            let ats: Vec<u64> = read(&file).unwrap().pages.iter().map(|p| p.at).collect();
            file.resize(file.len().next_multiple_of(2), 0);
            let array = file.len() as u64;
            file.extend_from_slice(&8u16.to_le_bytes());
            file.resize(file.len() + 999 * 2, 0);
            for at in ats {
                set_entry(&mut file, at, 258, 1_000, array);
            }
            file
        };
        // One page reads it once, within the file's size.
        let one = share(1);
        assert_eq!(read(&one).unwrap().pages[0].blocks[0].len, 1);
        // 64 pages would read it 64 times: 125 KiB of a 7 KiB file.
        assert!(matches!(read(&share(64)), Err(Error::TooLarge(_))));
    }

    #[test]
    fn missing_byte_counts_are_worked_out_only_for_uncompressed_data() {
        let mut page = gray(
            3,
            5,
            2,
            vec![B::data(&[0; 6]), B::data(&[0; 6]), B::data(&[0; 3])],
        );
        page.counts = Some(Vec::new());
        let file = Builder::new(b'M', false, vec![page.clone()]).build();
        let lens: Vec<u64> = read(&file).unwrap().pages[0]
            .blocks
            .iter()
            .map(|b| b.len)
            .collect();
        assert_eq!(lens, [6, 6, 3]);
        // Planar RGB, 16 bits: a plane of 3×5 at 2 rows a strip is 12, 12
        // and 6 bytes, three times.
        let mut planar = page.clone();
        planar.tags = vec![
            (256, V::Long(vec![3])),
            (257, V::Long(vec![5])),
            (258, V::Short(vec![16, 16, 16])),
            (262, V::Short(vec![2])),
            (277, V::Short(vec![3])),
            (278, V::Long(vec![2])),
            (284, V::Short(vec![2])),
        ];
        planar.blocks = [12, 12, 6]
            .repeat(3)
            .into_iter()
            .map(|n| B::Data(vec![0; n]))
            .collect();
        let file = Builder::new(b'I', true, vec![planar.clone()]).build();
        let lens: Vec<u64> = read(&file).unwrap().pages[0]
            .blocks
            .iter()
            .map(|b| b.len)
            .collect();
        assert_eq!(lens, [12, 12, 6].repeat(3));
        // Tiles: 16×16 at 8 bits is 256 bytes each.
        let tiled = Page {
            tags: vec![
                (256, V::Long(vec![20])),
                (257, V::Long(vec![17])),
                (258, V::Short(vec![8])),
                (322, V::Short(vec![16])),
                (323, V::Short(vec![16])),
            ],
            blocks: (0..4).map(|_| B::Data(vec![0; 256])).collect(),
            tiled: true,
            counts: Some(Vec::new()),
            ..Page::default()
        };
        let file = Builder::new(b'I', false, vec![tiled]).build();
        let lens: Vec<u64> = read(&file).unwrap().pages[0]
            .blocks
            .iter()
            .map(|b| b.len)
            .collect();
        assert_eq!(lens, [256; 4]);
        // As many offsets as another layout would have: no guess.
        planar.blocks.pop();
        let file = Builder::new(b'I', false, vec![planar]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
        // LZW: no guess.
        page.tags.push((259, V::Short(vec![5])));
        let file = Builder::new(b'M', false, vec![page]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
    }

    #[test]
    fn sizes_past_any_number_are_broken() {
        // 2^32 - 1 pixels a row of 65,535 samples of 65,535 bits, in one
        // strip of 16 rows, with no byte counts: 2^64 bytes and more.
        let mut page = gray(u32::MAX, 16, 16, vec![B::data(b"xy")]);
        page.tags[2] = (258, V::Short(vec![u16::MAX]));
        page.tags.push((277, V::Short(vec![u16::MAX])));
        page.counts = Some(Vec::new());
        let file = Builder::new(b'I', true, vec![page.clone()]).build();
        let got = read(&file);
        assert!(
            matches!(&got, Err(Error::Broken(why)) if why.contains("past any number")),
            "{got:?}"
        );
        // Counted, its count can't be right: refused as wrong-size.
        page.counts = Some(vec![2]);
        let got = read(&Builder::new(b'I', true, vec![page]).build());
        assert!(
            matches!(&got, Err(Error::Refused(why)) if why == MISSIZED),
            "{got:?}"
        );
        // BitsPerSample past a SHORT, which libtiff can't read: two
        // samples of 2^63 bits each.
        let mut page = gray(1, 1, 1, vec![B::data(b"xy")]);
        let half = (1u64 << 63).to_le_bytes();
        page.tags[2] = (258, V::Bytes(16, [half, half].concat()));
        page.tags.push((277, V::Short(vec![2])));
        page.counts = Some(Vec::new());
        let file = Builder::new(b'I', true, vec![page]).build();
        let got = read(&file);
        assert!(
            matches!(&got, Err(Error::Broken(why)) if why.contains("BitsPerSample")),
            "{got:?}"
        );
    }

    #[test]
    fn every_cap_is_refused_never_cut_short() {
        // Too many entries in one IFD: the builder adds 273 and 279.
        let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
        let extra = MAX_ENTRIES as u16 - page.tags.len() as u16 - 2;
        page.tags
            .extend((0..extra).map(|n| (40000 + n, V::Short(vec![0]))));
        let file = Builder::new(b'I', false, vec![page.clone()]).build();
        assert_eq!(
            read(&file).unwrap().pages[0].entries.len(),
            MAX_ENTRIES as usize
        );
        page.tags.push((50000, V::Short(vec![0])));
        let file = Builder::new(b'I', false, vec![page.clone()]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
        // Too many entries in all.
        page.tags.pop();
        let pages = vec![page; MAX_ALL_ENTRIES / MAX_ENTRIES as usize + 1];
        let file = Builder::new(b'I', false, pages).build();
        assert!(matches!(read(&file), Err(Error::TooLarge(_))));
        // Too many IFDs.
        let pages = vec![gray(1, 1, 1, vec![B::Empty]); MAX_IFDS + 1];
        let file = Builder::new(b'I', false, pages).build();
        assert!(matches!(read(&file), Err(Error::TooLarge(_))));
        // Too many blocks.
        let mut page = gray(1, 1, 1, Vec::new());
        page.offsets = Some(vec![0; MAX_BLOCKS + 1]);
        page.counts = Some(vec![0; MAX_BLOCKS + 1]);
        let file = Builder::new(b'I', false, vec![page]).build();
        assert!(matches!(read(&file), Err(Error::TooLarge(_))));
        // SubIFDs too deep: a page and five levels under it.
        let mut deep = gray(1, 1, 1, vec![B::data(b"x")]);
        for _ in 0..=MAX_DEPTH {
            let mut up = gray(1, 1, 1, vec![B::data(b"x")]);
            up.subs = vec![vec![deep]];
            deep = up;
        }
        let file = Builder::new(b'I', false, vec![deep.clone()]).build();
        assert!(matches!(read(&file), Err(Error::TooLarge(_))));
        // Four levels are fine.
        let file = Builder::new(b'I', false, deep.subs.remove(0)).build();
        assert_eq!(read(&file).unwrap().ifds().len(), MAX_DEPTH + 1);
    }

    #[test]
    fn a_truncated_file_is_broken_at_every_byte_and_never_panics() {
        for big in [false, true] {
            let file = sample(b'M', big).build();
            for n in 0..file.len() {
                assert!(read(&file[..n]).is_err(), "cut at {n}");
            }
        }
    }

    #[test]
    fn junk_is_broken() {
        for junk in [
            &b""[..],
            b"II*\0",
            b"II*\0\0\0\0\0",
            b"MM\0+\0\x04\0\0\0\0\0\0\0\0\0\x10",
            b"XX*\0\x08\0\0\0",
            b"II*\0\x08\0\0\0\0\0\0\0\0\0",
        ] {
            assert!(read(junk).is_err(), "{junk:?}");
        }
        // An IFD with no image data, and one with both strips and tiles.
        let file = Builder::new(
            b'I',
            false,
            vec![Page {
                tags: vec![(256, V::Short(vec![1]))],
                ..Page::default()
            }],
        )
        .build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
        let mut both = gray(1, 1, 1, vec![B::data(b"x")]);
        both.tags.push((324, V::Long(vec![20])));
        let file = Builder::new(b'I', false, vec![both]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
    }

    #[test]
    fn dng_and_old_style_jpeg_are_refused_in_any_ifd() {
        for (tag, value, refused) in [
            (50706, V::Bytes(1, vec![1, 4, 0, 0]), "DNG"),
            (259, V::Short(vec![6]), "old kind"),
            (513, V::Long(vec![0]), "old kind"),
            (514, V::Long(vec![0]), "old kind"),
        ] {
            let mut sub = gray(1, 1, 1, vec![B::data(b"x")]);
            sub.tags.push((tag, value));
            let mut page = gray(1, 1, 1, vec![B::data(b"y")]);
            page.subs = vec![vec![sub]];
            let file = Builder::new(b'I', false, vec![page]).build();
            let Err(Error::Refused(why)) = read(&file) else {
                panic!("{tag} not refused")
            };
            assert!(why.contains(refused), "{tag}");
        }
    }

    #[test]
    fn an_old_style_jpeg_with_no_strips_is_refused_not_broken() {
        // JPEGInterchangeFormat and its length, and no 273 or 324.
        let page = Page {
            tags: vec![
                (256, V::Long(vec![8])),
                (257, V::Long(vec![8])),
                (258, V::Short(vec![8])),
                (259, V::Short(vec![6])),
                (262, V::Short(vec![1])),
                (513, V::Long(vec![0])),
                (514, V::Long(vec![0])),
            ],
            ..Page::default()
        };
        for big in [false, true] {
            let file = Builder::new(b'M', big, vec![page.clone()]).build();
            let Err(Error::Refused(why)) = read(&file) else {
                panic!("{:?}", read(&file))
            };
            assert!(why.contains("old kind"), "{why}");
        }
    }

    #[test]
    fn numbers_read_and_write_in_both_orders() {
        assert_eq!(Order::Little.uint(&[1, 2]), 0x0201);
        assert_eq!(Order::Big.uint(&[1, 2, 3]), 0x01_0203);
        for order in [Order::Little, Order::Big] {
            for size in [2, 4, 8] {
                let v = 0x0102_0304_0506_0708 >> (64 - 8 * size);
                assert_eq!(order.uint(&order.bytes(v, size)), v);
            }
        }
        assert_eq!(Order::Big.bytes(0x0102, 2), [1, 2]);
        assert_eq!(Order::Little.bytes(0x0102, 4), [2, 1, 0, 0]);
    }
}
