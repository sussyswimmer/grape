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
//!   worked out only for uncompressed data. A block of 0 bytes stays empty
//!   (GDAL's sparse tiles); blocks that are the same bytes are fine, and any
//!   other overlap, between blocks or with an IFD, is [`Error::Broken`].
//! - **Refused:** a DNG, or an old-style JPEG, in any image IFD:
//!   [`Error::Refused`] as soon as the walk reads that IFD, before its
//!   strips or tiles, which an old-style JPEG may not have.
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

/// The most samples a pixel has, for a BitsPerSample read to work out
/// byte counts.
const MAX_SAMPLES: usize = 1 << 16;

/// The most bytes read in one go when reading an array.
const STEP: usize = 64 * 1024;

/// A TIFF's byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Its strips (273, 279) or tiles (324, 325), in order.
    pub(crate) blocks: Vec<Block>,
    /// Tiles rather than strips.
    pub(crate) tiled: bool,
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
    /// SHORT, LONG or LONG8).
    pub(crate) fn uint(&self, tag: u16, order: Order) -> Option<u64> {
        self.get(tag).and_then(|e| e.uint(order))
    }
}

impl Entry {
    /// The one unsigned number this holds inline (a BYTE, SHORT, LONG,
    /// LONG8, IFD or IFD8), if so.
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
        self.ifds()
            .into_iter()
            .flat_map(|ifd| &ifd.blocks)
            .filter(|b| b.len > 0)
    }

    /// The blocks listed more than once, sorted ([`repeats`]): worked out
    /// when asked, so that a model read only to be checked never holds
    /// them.
    pub(crate) fn shared(&self) -> Vec<Block> {
        repeats(|| self.listed())
    }

    /// Fails when two blocks overlap without being the same bytes, or a
    /// block or an IFD overlaps an IFD.
    fn check_overlaps(&self) -> Result<(), Error> {
        let layout = self.layout;
        let all = self.ifds();
        let mut ifds: Vec<(u64, u64)> = Vec::new();
        let span = |at: u64, n: usize| (at, at + layout.ifd_len(n as u64));
        for ifd in &all {
            ifds.push(span(ifd.at, ifd.entries.len()));
            for meta in [&ifd.exif, &ifd.gps].into_iter().flatten() {
                let interop = meta.interop.as_deref();
                for meta in std::iter::once(meta).chain(interop) {
                    if !meta.unread {
                        ifds.push(span(meta.at, meta.entries.len()));
                    }
                }
            }
        }
        // An Exif, GPS or Interop IFD that IFDs share is listed for each.
        ifds.sort_unstable();
        ifds.dedup();
        let ifds_apart = ifds.windows(2).all(|w| w[1].0 >= w[0].1);
        if ifds_apart && clear_of(InOrder::new(|| self.listed()).iter(), &ifds) {
            Ok(())
        } else {
            Err(Error::Broken("blocks or IFDs overlap".into()))
        }
    }
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

/// Why neither TIFF tool can change a file with the image IFD `ifd`, if
/// one can't: the end of a sentence that starts with its name.
fn refusal(ifd: &Ifd, order: Order) -> Option<&'static str> {
    if ifd.get(50706).is_some() {
        Some("is a camera raw (DNG) file, which Grape can't change yet.")
    } else if ifd.uint(259, order) == Some(6) || ifd.get(513).is_some() || ifd.get(514).is_some() {
        Some("uses an old kind of TIFF compression that Grape can't rewrite yet.")
    } else {
        None
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
            blocks: Vec::new(),
            tiled: false,
            subs: Vec::new(),
            exif: None,
            gps: None,
        };
        // Before its blocks: an old-style JPEG may have none.
        if let Some(why) = refusal(&ifd, self.layout.order) {
            return Err(Error::Refused(why.into()));
        }
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
        let at = match entry.uint(self.layout.order) {
            Some(0) => return Ok(None),
            Some(at) if matches!(entry.kind, 4 | 13 | 16 | 18) => at,
            _ => return Ok(Some(Meta::unread(0))),
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
        let counts = ifd.get(if tiled { 325 } else { 279 }).cloned();
        let estimated = match counts {
            Some(_) => Vec::new(),
            None => self.estimate(ifd, blocks.len())?,
        };
        let mut lens = 0;
        let set = |len| {
            if let Some(block) = blocks.get_mut(lens) {
                block.len = len;
            }
            lens += 1;
        };
        match counts {
            Some(entry) => self.each_uint(&entry, room, set)?,
            None => estimated.into_iter().for_each(set),
        }
        if lens != blocks.len() {
            return Err(Error::Broken(format!(
                "{} offsets but {lens} byte counts",
                blocks.len()
            )));
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

    /// The byte counts of an uncompressed IFD that doesn't say them: as
    /// many as its `want` offsets, or it's broken.
    fn estimate(&mut self, ifd: &Ifd, want: usize) -> Result<Vec<u64>, Error> {
        let o = self.layout.order;
        let get = |tag| ifd.uint(tag, o);
        let unsure = |why: &str| Error::Broken(format!("no byte counts, and {why}"));
        if get(259).unwrap_or(1) != 1 {
            return Err(unsure("the data is compressed"));
        }
        if get(262) == Some(6) {
            return Err(unsure("the data is YCbCr"));
        }
        let (Some(width), Some(height)) = (get(256), get(257)) else {
            return Err(unsure("no size"));
        };
        let spp = get(277).unwrap_or(1);
        let planar = get(284).unwrap_or(1) == 2;
        let bps = match ifd.get(258).cloned() {
            Some(bps) => self.uints(&bps, MAX_SAMPLES)?,
            None => vec![1],
        };
        let overflow = || unsure("sizes past any file");
        // Bits a pixel, in each plane.
        let bits: Vec<u64> = match (planar, bps.as_slice()) {
            (true, [one]) if spp <= MAX_SAMPLES as u64 => vec![*one; spp as usize],
            (true, [_]) => return Err(unsure("too many samples")),
            (true, all) => all.to_vec(),
            (false, [one]) => vec![one.saturating_mul(spp)],
            (false, all) => {
                let sum = all.iter().try_fold(0u64, |sum, &b| sum.checked_add(b));
                vec![sum.ok_or_else(overflow)?]
            }
        };
        let row = |w: u64, bits: u64| w.checked_mul(bits).map(|b| b.div_ceil(8));
        // The strips or tiles of one plane, and each one's rows.
        let (per_plane, rows) = if ifd.tiled {
            let (Some(tw), Some(th)) = (get(322), get(323)) else {
                return Err(unsure("no tile size"));
            };
            if tw == 0 || th == 0 {
                return Err(unsure("an empty tile size"));
            }
            (width.div_ceil(tw).saturating_mul(height.div_ceil(th)), th)
        } else {
            let rows = get(278).unwrap_or(height).clamp(1, height.max(1));
            (height.div_ceil(rows), rows)
        };
        if per_plane.checked_mul(bits.len() as u64) != Some(want as u64) {
            return Err(unsure(&format!("{want} offsets for another layout")));
        }
        let mut lens = Vec::with_capacity(want);
        for &bits in &bits {
            if ifd.tiled {
                let tw = get(322).unwrap_or(0);
                let len = row(tw, bits).and_then(|r| r.checked_mul(rows));
                lens.extend(std::iter::repeat_n(
                    len.ok_or_else(overflow)?,
                    per_plane as usize,
                ));
            } else {
                let row = row(width, bits).ok_or_else(overflow)?;
                for n in 0..per_plane {
                    let tall = rows.min(height - n * rows);
                    lens.push(row.checked_mul(tall).ok_or_else(overflow)?);
                }
            }
        }
        Ok(lens)
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

    use super::super::tests::{gray, set_entry, shared_meta, Builder, Page, B, V};
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
        into_ifd.offsets = Some(vec![8, 9]);
        into_ifd.counts = Some(vec![1, 1]);
        let file = Builder::new(b'I', false, vec![into_ifd]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
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
    fn bits_per_sample_past_any_number_is_broken() {
        // Two samples of 2^63 bits each, chunky, with no byte counts.
        let mut page = gray(1, 1, 1, vec![B::data(b"xy")]);
        page.tags.retain(|t| t.0 != 258);
        let half = (1u64 << 63).to_le_bytes();
        page.tags.extend([
            (258, V::Bytes(16, [half, half].concat())),
            (277, V::Short(vec![2])),
        ]);
        page.counts = Some(Vec::new());
        let file = Builder::new(b'I', true, vec![page]).build();
        assert!(matches!(read(&file), Err(Error::Broken(_))));
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
