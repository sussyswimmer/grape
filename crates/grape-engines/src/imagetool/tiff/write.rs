//! The TIFF writer both TIFF tools share (D28): a compact file, in the
//! source's byte order and kind, of the IFDs a tool chose ([`Dir`]).
//!
//! 1. The header, with a placeholder for the first IFD's offset.
//! 2. Every image IFD's blocks, in IFD order, written by the tool's
//!    [`Blocks`]: copied from the source by Remove Metadata's [`Copier`],
//!    each on an even offset, a block listed more than once written once
//!    ([`repeats`] finds them), an empty one left 0 bytes at 0; or
//!    re-encoded by Compress, with the fields that change with them.
//! 3. Every IFD, each followed by its values that don't fit in their
//!    entries, on even offsets: the entries sorted by tag, with the block
//!    offsets and counts, the SubIFDs (330) and the pointers to the Exif,
//!    GPS and Interop IFDs written fresh. A value that entries copy from
//!    the same place in the source, or that fields share
//!    ([`Data::Shared`]), is written once, as a block is. The region is
//!    laid out before it's written, so every pointer is known when it's
//!    written.
//! 4. A seek back to the header for the first IFD's offset.
//!
//! Nothing else from the source reaches the output. A classic file that
//! would need an offset past 4 GiB is [`Error::TooLarge`], and so is one
//! whose blocks and fields' values, each counted once, would come to more
//! bytes than the source has (values that overlap): so an output outgrows
//! its source only by the offsets, counts, IFDs and padding Grape writes.
//! Reads and writes go through the caller's `Cancellable` source and temp
//! file, and the [`Copier`] checks the token before each block.

use std::collections::{HashMap, HashSet};
use std::io::{self, SeekFrom, Write};
use std::sync::Arc;

use super::super::strip::{read_at, Error, In, Out};
use super::read::{repeats, type_size, Block, Entry, Layout, Order, Value};
use crate::CancelToken;

/// The buffer blocks and values are copied through.
const COPY: usize = 64 * 1024;

/// An entry to write, besides the ones the writer makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    pub(crate) tag: u16,
    /// The field type, as in the source.
    pub(crate) kind: u16,
    pub(crate) count: u64,
    pub(crate) value: Data,
}

/// Where a field's value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Data {
    /// These bytes, in the file's byte order.
    Bytes(Vec<u8>),
    /// These bytes, in the file's byte order, which other fields may hold
    /// too (the same `Arc`): written once however many do.
    Shared(Arc<Vec<u8>>),
    /// `len` bytes at `offset` in the source, copied as they are.
    Source { offset: u64, len: u64 },
}

impl Field {
    /// The source's `entry` as it is; `None` when its value can't be read.
    pub(crate) fn copy(entry: &Entry) -> Option<Field> {
        let value = match entry.value {
            Value::Inline { bytes, len } => Data::Bytes(bytes[..usize::from(len)].to_vec()),
            Value::At { offset, len } => Data::Source { offset, len },
            Value::Unread => return None,
        };
        Some(Field {
            tag: entry.tag,
            kind: entry.kind,
            count: entry.count,
            value,
        })
    }

    fn len(&self) -> u64 {
        match &self.value {
            Data::Bytes(bytes) => bytes.len() as u64,
            Data::Shared(bytes) => bytes.len() as u64,
            Data::Source { len, .. } => *len,
        }
    }
}

/// An IFD to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dir {
    /// Its entries but the ones the writer makes; a field for one of
    /// those tags, or a second field for a tag, is left out.
    pub(crate) fields: Vec<Field>,
    /// An image IFD's data; `None` for an Exif, GPS or Interop IFD.
    pub(crate) image: Option<Image>,
    /// The IFDs its pointer tags lead to, with those tags: the Exif
    /// (34665), GPS (34853) or Interop (40965) IFD.
    pub(crate) children: Vec<(u16, Dir)>,
}

/// An image IFD's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Image {
    /// Its strips or tiles in the source, in order.
    pub(crate) blocks: Vec<Block>,
    /// Tiles (324, 325) rather than strips (273, 279).
    pub(crate) tiled: bool,
    /// Its SubIFD chains (330).
    pub(crate) subs: Vec<Vec<Dir>>,
}

/// Writes each image IFD's blocks: copies them as they are ([`Copier`],
/// Remove Metadata), or re-encodes them (Compress).
pub(crate) trait Blocks {
    /// Writes the blocks of `image`, the image IFD number `n` in the order
    /// they're written (each page, then its SubIFDs, depth first, as
    /// `Tiff::ifds` lists them), at the end of `out`.
    fn write(
        &mut self,
        n: usize,
        r: &mut dyn In,
        out: &mut Sink,
        image: &Image,
    ) -> Result<Written, Error>;
}

/// What a [`Blocks`] wrote for an image IFD.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Written {
    /// Where each of its blocks went, in order: 0 for an empty one.
    pub(crate) offsets: Vec<u64>,
    /// Each one's bytes, when they aren't the source's.
    pub(crate) lens: Option<Vec<u64>>,
    /// Fields that take the place of the IFD's own for their tags, or that
    /// it gains: the ones that change with its blocks.
    pub(crate) fields: Vec<Field>,
    /// The bytes it took from the source, each block once: counted, with
    /// the values, against the source's size.
    pub(crate) taken: u64,
}

/// Remove Metadata's [`Blocks`]: each block copied as it is, one listed
/// more than once written once.
pub(crate) struct Copier<'c> {
    /// The blocks listed more than once ([`repeats`]), and where each went:
    /// 0 until it's written, since no block is written at 0.
    shared: Vec<Block>,
    placed: Vec<u64>,
    buf: Vec<u8>,
    cancel: &'c CancelToken,
}

impl<'c> Copier<'c> {
    /// A copier for the image IFDs of `pages`, which checks `cancel` before
    /// each block.
    pub(crate) fn new(pages: &[Dir], cancel: &'c CancelToken) -> Copier<'c> {
        let images = images(pages);
        let shared = repeats(|| {
            let blocks = images.iter().flat_map(|image| &image.blocks);
            blocks.filter(|b| b.len > 0)
        });
        Copier {
            placed: vec![0; shared.len()],
            shared,
            buf: vec![0; COPY],
            cancel,
        }
    }
}

impl Blocks for Copier<'_> {
    fn write(
        &mut self,
        _n: usize,
        r: &mut dyn In,
        out: &mut Sink,
        image: &Image,
    ) -> Result<Written, Error> {
        let mut written = Written {
            offsets: Vec::with_capacity(image.blocks.len()),
            ..Written::default()
        };
        for &block in &image.blocks {
            if self.cancel.is_cancelled() {
                return Err(Error::Write(io::Error::other("cancelled")));
            }
            if block.len == 0 {
                written.offsets.push(0);
                continue;
            }
            // Where it went, if it's a shared block written already.
            let place = self
                .shared
                .binary_search(&block)
                .ok()
                .and_then(|n| self.placed.get_mut(n));
            let at = match place {
                Some(&mut at) if at > 0 => at,
                place => {
                    out.align()?;
                    let at = out.at();
                    copy(r, out, block.offset, block.len, &mut self.buf)?;
                    written.taken = sum(written.taken, block.len)?;
                    if let Some(place) = place {
                        *place = at;
                    }
                    at
                }
            };
            written.offsets.push(at);
        }
        Ok(written)
    }
}

/// The images of `dirs` and of every IFD under them, depth first.
pub(crate) fn images(dirs: &[Dir]) -> Vec<&Image> {
    fn add<'d>(dir: &'d Dir, all: &mut Vec<&'d Image>) {
        if let Some(image) = &dir.image {
            all.push(image);
            for sub in image.subs.iter().flatten() {
                add(sub, all);
            }
        }
    }
    let mut all = Vec::new();
    for dir in dirs {
        add(dir, &mut all);
    }
    all
}

/// An entry's value, as the writer makes it.
enum Made {
    /// The IFD's field number n.
    Field(usize),
    /// Field number n of those its [`Written`] gave.
    Written(usize),
    /// The output's block offsets.
    Offsets,
    /// The output's block byte counts.
    Counts,
    /// The offsets of these IFDs (by their number among all of them).
    Ifds(Vec<usize>),
}

/// Where a value that entries repeat comes from, so it's written once.
#[derive(PartialEq, Eq, Hash)]
enum Key {
    /// Bytes at (offset, len) in the source.
    Source(u64, u64),
    /// Bytes fields share, by where they're held.
    Shared(*const Vec<u8>),
}

/// Where an entry's value goes.
enum Place {
    /// In the entry.
    Inline,
    /// Here, after the IFD.
    Here(u64),
    /// Where an earlier entry's copy of the same bytes went.
    There(u64),
}

/// One IFD as it's laid out.
struct Node<'d> {
    dir: &'d Dir,
    /// The entries the writer makes: (tag, type, count, value).
    made: Vec<(u16, u16, u64, Made)>,
    /// (tag, type, count, value), sorted by tag.
    entries: Vec<(u16, u16, u64, Made)>,
    /// The next IFD in its chain.
    next: Option<usize>,
    /// Where it goes.
    at: u64,
    /// Where each entry's value goes.
    values: Vec<Place>,
    /// What its blocks' writing gave: where they are in the output, and
    /// the fields that change with them.
    written: Written,
}

/// Writes `pages`, the main chain, into `w` (a new, empty file) as a TIFF
/// of `layout`'s byte order and kind, copying values and blocks from the
/// source `r` ([module docs](self)); checks `cancel` before each block.
pub(crate) fn write(
    r: &mut dyn In,
    w: &mut dyn Out,
    layout: Layout,
    pages: &[Dir],
    cancel: &CancelToken,
) -> Result<(), Error> {
    write_with(r, w, layout, pages, &mut Copier::new(pages, cancel)).map(|_| ())
}

/// Writes `pages` as [`write`] does, each image IFD's blocks by `blocks`;
/// gives the output's size.
pub(crate) fn write_with(
    r: &mut dyn In,
    w: &mut dyn Out,
    layout: Layout,
    pages: &[Dir],
    blocks: &mut dyn Blocks,
) -> Result<u64, Error> {
    let mut nodes = Vec::new();
    let Some(first) = chain(pages, &mut nodes, layout) else {
        return Err(Error::Broken("no page to write".into()));
    };
    let o = layout.order;
    let word = layout.word() as usize;
    let mut out = Sink { w, at: 0 };
    out.put(match o {
        Order::Little => b"II",
        Order::Big => b"MM",
    })?;
    if layout.big {
        out.put(&o.bytes(43, 2))?;
        out.put(&o.bytes(8, 2))?;
        out.put(&[0, 0])?;
    } else {
        out.put(&o.bytes(42, 2))?;
    }
    out.put(&o.bytes(0, word))?;

    let mut buf = vec![0; COPY];
    // The bytes taken from the source: each block and value once.
    let mut taken: u64 = 0;
    let mut n = 0;
    for node in &mut nodes {
        let Some(image) = &node.dir.image else {
            continue;
        };
        let written = blocks.write(n, r, &mut out, image)?;
        n += 1;
        fits(layout, out.at)?;
        let n_lens = written.lens.as_ref().map_or(image.blocks.len(), Vec::len);
        if written.offsets.len() != image.blocks.len() || n_lens != image.blocks.len() {
            return Err(Error::Broken(format!(
                "{} blocks written for {}",
                written.offsets.len(),
                image.blocks.len()
            )));
        }
        taken = sum(taken, written.taken)?;
        node.written = written;
        node.entries = entries(node);
    }
    for node in nodes.iter_mut().filter(|node| node.dir.image.is_none()) {
        node.entries = entries(node);
    }

    // The IFDs, laid out before they're written. A value copied from the
    // source, or shared, is written once however many entries hold it, as
    // blocks are.
    let mut copied: HashMap<Key, u64> = HashMap::new();
    let mut pos = out.at;
    for node in &mut nodes {
        pos = sum(pos, pos % 2)?;
        node.at = pos;
        pos = sum(pos, layout.ifd_len(node.entries.len() as u64))?;
        let mut values = Vec::with_capacity(node.entries.len());
        for (_, kind, count, made) in &node.entries {
            let field = match made {
                Made::Field(n) => Some(&node.dir.fields[*n]),
                Made::Written(n) => Some(&node.written.fields[*n]),
                _ => None,
            };
            let len = field.map_or_else(|| type_size(*kind).unwrap_or(8) * count, Field::len);
            let key = match field.map(|f| &f.value) {
                Some(&Data::Source { offset, len }) => Some(Key::Source(offset, len)),
                Some(Data::Shared(bytes)) => Some(Key::Shared(Arc::as_ptr(bytes))),
                _ => None,
            };
            if len <= layout.word() {
                values.push(Place::Inline);
            } else if let Some(&at) = key.as_ref().and_then(|key| copied.get(key)) {
                values.push(Place::There(at));
            } else {
                pos = sum(pos, pos % 2)?;
                values.push(Place::Here(pos));
                if let Some(key) = key {
                    copied.insert(key, pos);
                }
                if field.is_some() {
                    taken = sum(taken, len)?;
                }
                pos = sum(pos, len)?;
            }
        }
        node.values = values;
    }
    fits(layout, pos)?;
    // Blocks never overlap, and each block and value is counted once, so
    // only values that overlap each other or a block could take more than
    // the source holds: a file made to grow.
    if taken > layout.len {
        return Err(Error::TooLarge(format!(
            "values that copy {taken} bytes of a {}-byte file",
            layout.len
        )));
    }
    let ats: Vec<u64> = nodes.iter().map(|node| node.at).collect();
    for node in &nodes {
        out.pad_to(node.at)?;
        out.put(&o.bytes(node.entries.len() as u64, layout.count_len() as usize))?;
        for ((tag, kind, count, made), place) in node.entries.iter().zip(&node.values) {
            out.put(&o.bytes(u64::from(*tag), 2))?;
            out.put(&o.bytes(u64::from(*kind), 2))?;
            out.put(&o.bytes(*count, word))?;
            match place {
                Place::Here(at) | Place::There(at) => out.put(&o.bytes(*at, word))?,
                Place::Inline => {
                    let mut field = Vec::with_capacity(word);
                    value(r, &mut field, o, node, *kind, made, &ats, &mut buf)?;
                    field.resize(word, 0);
                    out.put(&field)?;
                }
            }
        }
        out.put(&o.bytes(node.next.map_or(0, |n| ats[n]), word))?;
        for ((_, kind, _, made), place) in node.entries.iter().zip(&node.values) {
            if let Place::Here(at) = place {
                out.pad_to(*at)?;
                value(r, &mut out, o, node, *kind, made, &ats, &mut buf)?;
            }
        }
    }

    // The first IFD's offset, in the header.
    let len = out.at;
    out.w
        .seek(SeekFrom::Start(layout.header() - layout.word()))
        .map_err(Error::Write)?;
    out.w
        .write_all(&o.bytes(ats[first], word))
        .map_err(Error::Write)?;
    Ok(len)
}

/// `a + b`, or [`Error::TooLarge`] past any number: sizes that only a
/// broken plan could add up to.
fn sum(a: u64, b: u64) -> Result<u64, Error> {
    a.checked_add(b)
        .ok_or_else(|| Error::TooLarge("an output past any size".into()))
}

/// Fails when `at` is past what a classic file's offsets can say.
fn fits(layout: Layout, at: u64) -> Result<(), Error> {
    if layout.big || at <= u64::from(u32::MAX) {
        Ok(())
    } else {
        Err(Error::TooLarge(format!(
            "{at} bytes, past a classic TIFF's 4 GiB"
        )))
    }
}

/// Adds the chain `dirs` (and every IFD under each) to `nodes`; gives its
/// head.
fn chain<'d>(dirs: &'d [Dir], nodes: &mut Vec<Node<'d>>, layout: Layout) -> Option<usize> {
    let mut head = None;
    let mut last: Option<usize> = None;
    for dir in dirs {
        let n = add(dir, nodes, layout);
        match last {
            Some(prev) => nodes[prev].next = Some(n),
            None => head = Some(n),
        }
        last = Some(n);
    }
    head
}

/// Adds `dir`, then the IFDs under it, to `nodes`; gives its number.
fn add<'d>(dir: &'d Dir, nodes: &mut Vec<Node<'d>>, layout: Layout) -> usize {
    let at = nodes.len();
    nodes.push(Node {
        dir,
        made: Vec::new(),
        entries: Vec::new(),
        next: None,
        at: 0,
        values: Vec::new(),
        written: Written::default(),
    });
    // Offsets and counts are LONG in a classic file and LONG8 in a
    // BigTIFF; pointers LONG or IFD8, as libtiff writes them.
    let number = if layout.big { 16 } else { 4 };
    let pointer = if layout.big { 18 } else { 4 };
    let mut made = Vec::new();
    if let Some(image) = &dir.image {
        let (offsets, counts) = if image.tiled { (324, 325) } else { (273, 279) };
        let n = image.blocks.len() as u64;
        made.push((offsets, number, n, Made::Offsets));
        made.push((counts, number, n, Made::Counts));
        let heads: Vec<usize> = image
            .subs
            .iter()
            .filter_map(|subs| chain(subs, nodes, layout))
            .collect();
        if !heads.is_empty() {
            made.push((330, pointer, heads.len() as u64, Made::Ifds(heads)));
        }
    }
    for (tag, child) in &dir.children {
        let n = add(child, nodes, layout);
        made.push((*tag, pointer, 1, Made::Ifds(vec![n])));
    }
    nodes[at].made = made;
    at
}

/// `node`'s entries, sorted by tag, once its blocks are written: the ones
/// the writer makes, the fields its blocks' writing gave, then its own,
/// each tag once.
fn entries(node: &mut Node) -> Vec<(u16, u16, u64, Made)> {
    let made = std::mem::take(&mut node.made);
    let mut tags: HashSet<u16> = [273, 279, 324, 325, 330].into();
    tags.extend(made.iter().map(|m| m.0));
    let fields = |fields: &[Field], made: fn(usize) -> Made| {
        fields
            .iter()
            .enumerate()
            .map(move |(n, f)| (f.tag, f.kind, f.count, made(n)))
            .collect::<Vec<_>>()
    };
    let mut entries: Vec<(u16, u16, u64, Made)> = fields(&node.written.fields, Made::Written)
        .into_iter()
        .chain(fields(&node.dir.fields, Made::Field))
        .filter(|e| tags.insert(e.0))
        .collect();
    entries.extend(made);
    entries.sort_by_key(|e| e.0);
    entries
}

/// Writes the value `made` (of type `kind`) of one of `node`'s entries
/// into `w`: a field's bytes, or numbers the writer made, in `order`.
#[allow(clippy::too_many_arguments)]
fn value(
    r: &mut dyn In,
    w: &mut dyn Write,
    order: Order,
    node: &Node,
    kind: u16,
    made: &Made,
    ats: &[u64],
    buf: &mut [u8],
) -> Result<(), Error> {
    let size = type_size(kind).unwrap_or(8) as usize;
    let source = node.dir.image.iter().flat_map(|image| &image.blocks);
    let numbers: &mut dyn Iterator<Item = u64> = match (made, &node.written.lens) {
        (Made::Field(n), _) => return field(r, w, &node.dir.fields[*n], buf),
        (Made::Written(n), _) => return field(r, w, &node.written.fields[*n], buf),
        (Made::Offsets, _) => &mut node.written.offsets.iter().copied(),
        (Made::Counts, Some(lens)) => &mut lens.iter().copied(),
        (Made::Counts, None) => &mut source.map(|b| b.len),
        (Made::Ifds(ifds), _) => &mut ifds.iter().map(|&n| ats[n]),
    };
    for v in numbers {
        w.write_all(&order.bytes(v, size)).map_err(Error::Write)?;
    }
    Ok(())
}

/// Writes `field`'s value into `w`: its bytes, or copied from the source
/// `r` through `buf`.
fn field(r: &mut dyn In, w: &mut dyn Write, field: &Field, buf: &mut [u8]) -> Result<(), Error> {
    match &field.value {
        Data::Bytes(bytes) => w.write_all(bytes).map_err(Error::Write),
        Data::Shared(bytes) => w.write_all(bytes).map_err(Error::Write),
        Data::Source { offset, len } => copy(r, w, *offset, *len, buf),
    }
}

/// Copies `len` bytes at `offset` in `r` into `w`, through `buf`: only
/// they are read ([`read_at`]), however far from what was read before.
pub(crate) fn copy(
    r: &mut dyn In,
    w: &mut dyn Write,
    offset: u64,
    len: u64,
    buf: &mut [u8],
) -> Result<(), Error> {
    let mut done = 0;
    while done < len {
        let n = buf
            .len()
            .min(usize::try_from(len - done).unwrap_or(usize::MAX));
        let step = &mut buf[..n];
        read_at(r, offset + done, step)?;
        w.write_all(step).map_err(Error::Write)?;
        done += step.len() as u64;
    }
    Ok(())
}

/// The output, counting where the next byte goes.
pub(crate) struct Sink<'w> {
    w: &'w mut dyn Out,
    at: u64,
}

impl Sink<'_> {
    /// Where the next byte goes.
    pub(crate) fn at(&self) -> u64 {
        self.at
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.write_all(bytes).map_err(Error::Write)
    }

    /// Pads to an even offset.
    pub(crate) fn align(&mut self) -> Result<(), Error> {
        let to = self.at + self.at % 2;
        self.pad_to(to)
    }

    /// Goes back to `to`, before where the next byte goes: what's written
    /// from there takes the place of what was, and the caller cuts the file
    /// at the size [`write_with`] gives.
    pub(crate) fn rewind(&mut self, to: u64) -> Result<(), Error> {
        self.w.seek(SeekFrom::Start(to)).map_err(Error::Write)?;
        self.at = to;
        Ok(())
    }

    /// Pads with zeros up to `to`.
    fn pad_to(&mut self, to: u64) -> Result<(), Error> {
        while self.at < to {
            let n = usize::try_from(to - self.at).map_or(8, |n| n.min(8));
            self.put(&[0; 8][..n])?;
        }
        Ok(())
    }
}

impl Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.w.write(buf)?;
        self.at += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use super::super::read::{read_value, Ifd, Meta, Tiff};
    use super::super::tests::{gray, set_entry, shared_meta, Builder, Page, B, V};
    use super::*;

    fn read(bytes: &[u8]) -> Tiff {
        Tiff::read(&mut Cursor::new(bytes), bytes.len() as u64).unwrap()
    }

    /// `meta` with every entry it has, and its Interop IFD.
    fn meta_dir(meta: &Meta) -> Dir {
        let interop = meta.interop.as_deref().map(|i| (40965, meta_dir(i)));
        Dir {
            fields: meta.entries.iter().filter_map(Field::copy).collect(),
            image: None,
            children: interop.into_iter().collect(),
        }
    }

    /// `ifd` with every entry it has, as Compress keeps them.
    fn everything(ifd: &Ifd) -> Dir {
        let mut children = Vec::new();
        if let Some(exif) = &ifd.exif {
            children.push((34665, meta_dir(exif)));
        }
        if let Some(gps) = &ifd.gps {
            children.push((34853, meta_dir(gps)));
        }
        Dir {
            fields: ifd.entries.iter().filter_map(Field::copy).collect(),
            image: Some(Image {
                blocks: ifd.blocks.clone(),
                tiled: ifd.tiled,
                subs: ifd
                    .subs
                    .iter()
                    .map(|chain| chain.iter().map(everything).collect())
                    .collect(),
            }),
            children,
        }
    }

    /// `file` read and written back with every entry.
    fn rewrite(file: &[u8]) -> Vec<u8> {
        let tiff = read(file);
        let pages: Vec<Dir> = tiff.pages.iter().map(everything).collect();
        let mut out = Cursor::new(Vec::new());
        let mut r = Cursor::new(file);
        write(&mut r, &mut out, tiff.layout, &pages, &CancelToken::new()).unwrap();
        out.into_inner()
    }

    /// Each entry's tag, type, count and value bytes.
    fn values(file: &[u8], entries: &[Entry]) -> Vec<(u16, u16, u64, Vec<u8>)> {
        let mut r = Cursor::new(file);
        let mut all: Vec<_> = entries
            .iter()
            .filter(|e| !matches!(e.tag, 273 | 279 | 324 | 325 | 330 | 34665 | 34853 | 40965))
            .map(|e| {
                let value = read_value(&mut r, e, 1 << 20).unwrap().unwrap();
                (e.tag, e.kind, e.count, value)
            })
            .collect();
        all.sort();
        all
    }

    /// Checks that `a` and `b` (files `fa` and `fb`) hold the same IFDs,
    /// entries and block bytes.
    fn same(fa: &[u8], a: &Ifd, fb: &[u8], b: &Ifd) {
        assert_eq!(values(fa, &a.entries), values(fb, &b.entries));
        let bytes = |f: &[u8], ifd: &Ifd| -> Vec<Vec<u8>> {
            ifd.blocks
                .iter()
                .map(|b| f[b.offset as usize..(b.offset + b.len) as usize].to_vec())
                .collect()
        };
        assert_eq!(bytes(fa, a), bytes(fb, b));
        assert_eq!(a.tiled, b.tiled);
        for (ma, mb) in [(&a.exif, &b.exif), (&a.gps, &b.gps)] {
            assert_eq!(ma.is_some(), mb.is_some());
            if let (Some(ma), Some(mb)) = (ma, mb) {
                assert_eq!(values(fa, &ma.entries), values(fb, &mb.entries));
                let ia = ma.interop.as_ref().map(|i| values(fa, &i.entries));
                let ib = mb.interop.as_ref().map(|i| values(fb, &i.entries));
                assert_eq!(ia, ib);
            }
        }
        assert_eq!(a.subs.len(), b.subs.len());
        for (ca, cb) in a.subs.iter().zip(&b.subs) {
            assert_eq!(ca.len(), cb.len());
            for (sa, sb) in ca.iter().zip(cb) {
                same(fa, sa, fb, sb);
            }
        }
    }

    /// Two pages: the first with SubIFD chains, Exif, Interop and GPS, a
    /// text value and blocks shared, empty and out of order, the second
    /// tiled; and bytes nothing points at.
    fn sample(order: u8, big: bool) -> Builder {
        let mut first = gray(
            4,
            4,
            1,
            vec![B::data(b"abcd"), B::Same(0), B::Empty, B::data(b"efg")],
        );
        first
            .tags
            .push((270, V::Ascii("a description long enough")));
        first
            .tags
            .push((33723, V::Bytes(4, b"\x1c\x02\x05\0\x04Grape\0\0".to_vec())));
        let mut nested = gray(2, 1, 1, vec![B::data(b"ij")]);
        nested.subs = vec![vec![gray(1, 1, 1, vec![B::data(b"k")])]];
        first.subs = vec![
            vec![nested, gray(1, 1, 1, vec![B::data(b"l")])],
            vec![gray(1, 1, 1, vec![B::data(b"m")])],
        ];
        first.exif = Some(vec![
            (0x9003, V::Ascii("2024:05:01 10:20:30")),
            (0xA001, V::Short(vec![1])),
        ]);
        first.interop = Some(vec![(1, V::Ascii("R98"))]);
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
        Builder {
            reversed: true,
            tail: b"orphaned bytes".to_vec(),
            ..Builder::new(order, big, vec![first, second])
        }
    }

    #[test]
    fn a_tiff_written_back_reads_the_same_in_every_order_and_kind() {
        for (order, big) in [(b'I', false), (b'M', false), (b'I', true), (b'M', true)] {
            let file = sample(order, big).build();
            let out = rewrite(&file);
            let (a, b) = (read(&file), read(&out));
            assert_eq!(
                (b.layout.order, b.layout.big),
                (a.layout.order, a.layout.big)
            );
            assert_eq!(a.pages.len(), b.pages.len());
            for (pa, pb) in a.pages.iter().zip(&b.pages) {
                same(&file, pa, &out, pb);
            }
            // The shared block is written once, the empty one stays empty,
            // and what nothing pointed at is gone.
            let blocks = &b.pages[0].blocks;
            assert_eq!(blocks[0], blocks[1]);
            assert_eq!(b.shared(), [blocks[0]]);
            assert_eq!(blocks[2], Block { offset: 0, len: 0 });
            assert!(!out.windows(8).any(|w| w == b"orphaned"));
            assert!(out.len() < file.len());
            // Blocks come first, in IFD order; IFDs and values are even.
            assert!(blocks[0].offset < blocks[3].offset);
            for ifd in b.ifds() {
                assert_eq!(ifd.at % 2, 0);
                assert!(ifd.at > b.pages[1].blocks[1].offset);
                for block in ifd.blocks.iter().filter(|b| b.len > 0) {
                    assert_eq!(block.offset % 2, 0);
                }
            }
            // Written again, it's the same file.
            assert_eq!(rewrite(&out), out);
        }
    }

    /// A page's samples as bytes.
    fn pixels(page: tiff::decoder::DecodingResult) -> Vec<u8> {
        use tiff::decoder::DecodingResult::*;
        match page {
            U8(v) => v,
            U16(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            _ => panic!("another sample type"),
        }
    }

    #[test]
    fn the_tiff_crate_decodes_every_page_written_back() {
        use tiff::decoder::Decoder;
        use tiff::encoder::{colortype, Compression, DeflateLevel, TiffEncoder, TiffKind};
        /// Three pages, LZW in strips of 7 rows, Deflate and PackBits: RGB,
        /// 16-bit gray and float gray.
        fn pages<K: TiffKind>(mut e: TiffEncoder<&mut Cursor<Vec<u8>>, K>) {
            let rgb: Vec<u8> = (0..40 * 30 * 3).map(|i| (i * 7 % 251) as u8).collect();
            let gray16: Vec<u16> = (0..40 * 30).map(|i| (i * 977 % 65_521) as u16).collect();
            let float: Vec<f32> = (0..40 * 30).map(|i| i as f32 / 7.0).collect();
            e = e.with_compression(Compression::Lzw);
            let mut lzw = e.new_image::<colortype::RGB8>(40, 30).unwrap();
            lzw.rows_per_strip(7).unwrap();
            lzw.write_data(&rgb).unwrap();
            e = e.with_compression(Compression::Deflate(DeflateLevel::Best));
            e.write_image::<colortype::Gray16>(40, 30, &gray16).unwrap();
            e = e.with_compression(Compression::Packbits);
            e.write_image::<colortype::Gray32Float>(40, 30, &float)
                .unwrap();
        }
        for big in [false, true] {
            let mut file = Cursor::new(Vec::new());
            if big {
                pages(TiffEncoder::new_big(&mut file).unwrap());
            } else {
                pages(TiffEncoder::new(&mut file).unwrap());
            }
            let file = file.into_inner();
            let out = rewrite(&file);
            let mut a = Decoder::new(Cursor::new(&file)).unwrap();
            let mut b = Decoder::new(Cursor::new(&out)).unwrap();
            for page in 0..3 {
                if page > 0 {
                    a.next_image().unwrap();
                    b.next_image().unwrap();
                }
                assert_eq!(a.colortype().unwrap(), b.colortype().unwrap());
                let (pa, pb) = (
                    pixels(a.read_image().unwrap()),
                    pixels(b.read_image().unwrap()),
                );
                assert_eq!(pa, pb, "page {page}");
                assert_eq!(pa.len(), [40 * 30 * 3, 40 * 30 * 2, 40 * 30 * 4][page]);
            }
            assert!(!b.more_images());
        }
    }

    #[test]
    fn values_fit_in_their_entries_as_the_kind_allows() {
        // 8 bytes are inline in a BigTIFF and out of line in a classic one.
        for big in [false, true] {
            let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
            page.tags.push((282, V::Rational(vec![(300, 1)])));
            let file = Builder::new(b'I', big, vec![page]).build();
            let out = rewrite(&file);
            let tiff = read(&out);
            let entry = tiff.pages[0].get(282).unwrap();
            assert_eq!(matches!(entry.value, Value::Inline { .. }), big);
        }
    }

    /// Three pages whose ICC profiles are the first page's 4,000 bytes,
    /// read from `step` bytes further on for each page after it.
    fn one_profile(big: bool, step: u64) -> (Vec<u8>, Vec<u8>) {
        let profile: Vec<u8> = (0..4000).map(|i| (i % 251) as u8 + 1).collect();
        let mut first = gray(1, 1, 1, vec![B::data(b"x")]);
        first.tags.push((34675, V::Bytes(7, profile.clone())));
        let mut pages = vec![first];
        for _ in 0..2 {
            let mut page = gray(1, 1, 1, vec![B::data(b"y")]);
            page.tags.push((34675, V::Bytes(7, vec![0])));
            pages.push(page);
        }
        let builder = Builder {
            tail: vec![0; 64],
            ..Builder::new(b'I', big, pages)
        };
        let mut file = builder.build();
        let tiff = read(&file);
        let Value::At { offset, .. } = tiff.pages[0].get(34675).unwrap().value else {
            panic!("an inline profile")
        };
        for (n, page) in (1..).zip(&tiff.pages[1..]) {
            set_entry(&mut file, page.at, 34675, 4000, offset + n * step);
        }
        (file, profile)
    }

    #[test]
    fn a_value_many_ifds_point_at_is_written_once() {
        for big in [false, true] {
            let (file, profile) = one_profile(big, 0);
            let out = rewrite(&file);
            let back = read(&out);
            let places: Vec<Value> = back
                .pages
                .iter()
                .map(|page| page.get(34675).unwrap().value)
                .collect();
            assert_eq!(places.len(), 3);
            assert!(places.iter().all(|place| *place == places[0]), "{places:?}");
            for page in &back.pages {
                let entry = page.get(34675).unwrap();
                let value = read_value(&mut Cursor::new(&out), entry, 1 << 20).unwrap();
                assert_eq!(value.unwrap(), profile);
            }
            assert!(out.len() < profile.len() + 1024, "{}", out.len());
        }
    }

    #[test]
    fn an_output_that_would_copy_more_than_the_source_holds_is_too_large() {
        // Each page's profile starts 8 bytes after the one before: three
        // values, so 12,000 bytes to copy from a file of about 4,500.
        for big in [false, true] {
            let (file, _) = one_profile(big, 8);
            let tiff = read(&file);
            let pages: Vec<Dir> = tiff.pages.iter().map(everything).collect();
            let mut out = Cursor::new(Vec::new());
            let result = write(
                &mut Cursor::new(&file),
                &mut out,
                tiff.layout,
                &pages,
                &CancelToken::new(),
            );
            assert!(matches!(result, Err(Error::TooLarge(_))), "{result:?}");
            // Nothing past the blocks was written.
            assert!(out.into_inner().len() < 64);
        }
    }

    #[test]
    fn an_output_past_any_size_is_too_large() {
        // A value of almost 2^64 bytes: the layout's sums would wrap.
        for big in [false, true] {
            let file = Builder::new(b'I', big, vec![gray(1, 1, 1, vec![B::data(b"x")])]).build();
            let tiff = read(&file);
            let mut dir = everything(&tiff.pages[0]);
            dir.fields.push(Field {
                tag: 50000,
                kind: 7,
                count: u64::MAX - 4,
                value: Data::Source {
                    offset: 8,
                    len: u64::MAX - 4,
                },
            });
            let result = write(
                &mut Cursor::new(&file),
                &mut Cursor::new(Vec::new()),
                tiff.layout,
                &[dir],
                &CancelToken::new(),
            );
            assert!(matches!(result, Err(Error::TooLarge(_))), "{result:?}");
        }
    }

    #[test]
    fn a_shared_exif_ifd_is_written_for_each_page_that_points_at_it() {
        for (order, big) in [(b'I', false), (b'M', false), (b'I', true), (b'M', true)] {
            let file = shared_meta(order, big);
            let out = rewrite(&file);
            let (a, b) = (read(&file), read(&out));
            for (pa, pb) in a.pages.iter().zip(&b.pages) {
                same(&file, pa, &out, pb);
            }
            // Two Exif IFDs now, whose date is written once.
            let [first, second] = &b.pages[..] else {
                panic!("{} pages", b.pages.len())
            };
            let (e1, e2) = (first.exif.as_ref().unwrap(), second.exif.as_ref().unwrap());
            assert_ne!(e1.at, e2.at);
            assert_eq!(out.windows(7).filter(|w| *w == b"2024:05").count(), 1);
            assert_eq!(rewrite(&out), out);
        }
    }

    #[test]
    fn fields_the_writer_makes_or_repeats_are_left_out() {
        let tiff =
            read(&Builder::new(b'M', false, vec![gray(1, 1, 1, vec![B::data(b"x")])]).build());
        let mut dir = everything(&tiff.pages[0]);
        let stray = Field {
            tag: 330,
            kind: 4,
            count: 1,
            value: Data::Bytes(vec![0, 0, 0, 8]),
        };
        let twice = Field {
            tag: 256,
            kind: 3,
            count: 1,
            value: Data::Bytes(vec![0, 9]),
        };
        dir.fields.extend([stray, twice]);
        let mut out = Cursor::new(Vec::new());
        let file = Builder::new(b'M', false, vec![gray(1, 1, 1, vec![B::data(b"x")])]).build();
        write(
            &mut Cursor::new(&file),
            &mut out,
            tiff.layout,
            &[dir],
            &CancelToken::new(),
        )
        .unwrap();
        let back = read(&out.into_inner());
        let page = &back.pages[0];
        assert!(page.get(330).is_none());
        assert_eq!(page.uint(256, back.layout.order), Some(1));
        assert_eq!(page.entries.iter().filter(|e| e.tag == 256).count(), 1);
    }

    #[test]
    fn a_cancel_stops_before_the_next_block() {
        let file = sample(b'I', false).build();
        let tiff = read(&file);
        let pages: Vec<Dir> = tiff.pages.iter().map(everything).collect();
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut out = Cursor::new(Vec::new());
        let result = write(
            &mut Cursor::new(&file),
            &mut out,
            tiff.layout,
            &pages,
            &cancel,
        );
        assert!(matches!(result, Err(Error::Write(_))));
        assert_eq!(out.into_inner().len(), 8);
    }

    /// A [`Blocks`] that writes each block's bytes backwards and a `!`,
    /// gives each image IFD a Compression of 8, and notes the numbers it's
    /// given.
    struct Backwards {
        order: Order,
        seen: Vec<usize>,
        /// Leaves the last block out.
        short: bool,
    }

    impl Blocks for Backwards {
        fn write(
            &mut self,
            n: usize,
            r: &mut dyn In,
            out: &mut Sink,
            image: &Image,
        ) -> Result<Written, Error> {
            self.seen.push(n);
            let mut written = Written {
                lens: Some(Vec::new()),
                ..Written::default()
            };
            let lens = written.lens.get_or_insert_default();
            for block in &image.blocks {
                if block.len == 0 {
                    written.offsets.push(0);
                    lens.push(0);
                    continue;
                }
                let mut bytes = vec![0; block.len as usize];
                read_at(r, block.offset, &mut bytes)?;
                bytes.reverse();
                bytes.push(b'!');
                out.align()?;
                let offset = out.at();
                out.write_all(&bytes).map_err(Error::Write)?;
                written.offsets.push(offset);
                lens.push(bytes.len() as u64);
                written.taken += block.len;
            }
            if self.short {
                lens.pop();
            }
            written.fields.push(Field {
                tag: 259,
                kind: 3,
                count: 1,
                value: Data::Bytes(self.order.bytes(8, 2)),
            });
            Ok(written)
        }
    }

    #[test]
    fn a_tools_blocks_and_fields_are_written_in_place_of_the_sources() {
        for (order, big) in [(b'I', false), (b'M', true)] {
            let file = sample(order, big).build();
            let tiff = read(&file);
            let pages: Vec<Dir> = tiff.pages.iter().map(everything).collect();
            let mut blocks = Backwards {
                order: tiff.layout.order,
                seen: Vec::new(),
                short: false,
            };
            let mut out = Cursor::new(Vec::new());
            let mut r = Cursor::new(&file[..]);
            let len = write_with(&mut r, &mut out, tiff.layout, &pages, &mut blocks).unwrap();
            let out = out.into_inner();
            assert_eq!(len, out.len() as u64);
            let back = read(&out);
            let (was, now) = (tiff.ifds(), back.ifds());
            // Each image IFD in the order `Tiff::ifds` lists them.
            assert_eq!(blocks.seen, (0..was.len()).collect::<Vec<_>>());
            assert_eq!(was.len(), now.len());
            for (a, b) in was.iter().zip(&now) {
                assert_eq!(b.uint(259, back.layout.order), Some(8));
                for (ba, bb) in a.blocks.iter().zip(&b.blocks) {
                    let mut want = file[ba.offset as usize..][..ba.len as usize].to_vec();
                    want.reverse();
                    if ba.len > 0 {
                        want.push(b'!');
                    }
                    assert_eq!(out[bb.offset as usize..][..bb.len as usize], want);
                }
            }
            // A tool that writes too few blocks is caught.
            blocks.short = true;
            let result = write_with(
                &mut r,
                &mut Cursor::new(Vec::new()),
                tiff.layout,
                &pages,
                &mut blocks,
            );
            assert!(matches!(result, Err(Error::Broken(_))), "{result:?}");
        }
    }

    #[test]
    fn a_classic_file_past_4_gib_is_too_large() {
        let classic = Layout {
            order: Order::Big,
            big: false,
            len: 0,
        };
        assert!(fits(classic, u64::from(u32::MAX)).is_ok());
        assert!(matches!(
            fits(classic, u64::from(u32::MAX) + 1),
            Err(Error::TooLarge(_))
        ));
        let big = Layout {
            big: true,
            ..classic
        };
        assert!(fits(big, u64::MAX).is_ok());
    }
}
