//! The TIFF writer both TIFF tools share (D28): a compact file, in the
//! source's byte order and kind, of the IFDs a tool chose ([`Dir`]).
//!
//! 1. The header, with a placeholder for the first IFD's offset.
//! 2. Every image IFD's blocks, in IFD order, each on an even offset,
//!    copied from the source. A block listed more than once is written
//!    once; an empty one stays 0 bytes at 0.
//! 3. Every IFD, each followed by its values that don't fit in their
//!    entries, on even offsets: the entries sorted by tag, with the block
//!    offsets and counts, the SubIFDs (330) and the pointers to the Exif,
//!    GPS and Interop IFDs written fresh. The region is laid out before
//!    it's written, so every pointer is known when it's written.
//! 4. A seek back to the header for the first IFD's offset.
//!
//! Nothing else from the source reaches the output. A classic file that
//! would need an offset past 4 GiB is [`Error::TooLarge`]. Reads and
//! writes go through the caller's `Cancellable` source and temp file, and
//! the token is checked before each block.

use std::collections::{HashMap, HashSet};
use std::io::{self, SeekFrom, Write};

use super::super::strip::{read_exact, Error, In, Out};
use super::read::{seek, type_size, Block, Entry, Layout, Order, Value};
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

/// An entry's value, as the writer makes it.
enum Made {
    /// The IFD's field number n.
    Field(usize),
    /// The output's block offsets.
    Offsets,
    /// The output's block byte counts.
    Counts,
    /// The offsets of these IFDs (by their number among all of them).
    Ifds(Vec<usize>),
}

/// One IFD as it's laid out.
struct Node<'d> {
    dir: &'d Dir,
    /// (tag, type, count, value), sorted by tag.
    entries: Vec<(u16, u16, u64, Made)>,
    /// The next IFD in its chain.
    next: Option<usize>,
    /// Where it goes.
    at: u64,
    /// Where each entry's value goes, when it doesn't fit in the entry.
    values: Vec<Option<u64>>,
    /// Its blocks' places in the output.
    blocks: Vec<Block>,
}

/// Writes `pages`, the main chain, into `w` (a new, empty file) as a TIFF
/// of `layout`'s byte order and kind, copying values and blocks from the
/// source `r` ([module docs](self)). `shared` holds the source's blocks
/// that are listed more than once.
pub(crate) fn write(
    r: &mut dyn In,
    w: &mut dyn Out,
    layout: Layout,
    shared: &HashSet<Block>,
    pages: &[Dir],
    cancel: &CancelToken,
) -> Result<(), Error> {
    let mut nodes = Vec::new();
    let Some(first) = chain(pages, &mut nodes, layout) else {
        return Err(Error::Broken("no page to write".into()));
    };
    let o = layout.order;
    let word = layout.word() as usize;
    let mut out = Counted { w, at: 0 };
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
    let mut placed: HashMap<Block, u64> = HashMap::new();
    for node in &mut nodes {
        let Some(image) = &node.dir.image else {
            continue;
        };
        let mut blocks = Vec::with_capacity(image.blocks.len());
        for &block in &image.blocks {
            if cancel.is_cancelled() {
                return Err(Error::Write(io::Error::other("cancelled")));
            }
            let at = if block.len == 0 {
                0
            } else if let Some(&at) = placed.get(&block) {
                at
            } else {
                out.align()?;
                let at = out.at;
                copy(r, &mut out, block.offset, block.len, &mut buf)?;
                fits(layout, out.at)?;
                if shared.contains(&block) {
                    placed.insert(block, at);
                }
                at
            };
            blocks.push(Block {
                offset: at,
                len: block.len,
            });
        }
        node.blocks = blocks;
    }

    // The IFDs, laid out before they're written.
    let mut pos = out.at;
    for node in &mut nodes {
        pos += pos % 2;
        node.at = pos;
        pos += layout.ifd_len(node.entries.len() as u64);
        let mut values = Vec::with_capacity(node.entries.len());
        for (_, kind, count, made) in &node.entries {
            let len = match made {
                Made::Field(n) => node.dir.fields[*n].len(),
                _ => type_size(*kind).unwrap_or(8) * count,
            };
            if len > layout.word() {
                pos += pos % 2;
                values.push(Some(pos));
                pos += len;
            } else {
                values.push(None);
            }
        }
        node.values = values;
    }
    fits(layout, pos)?;
    let ats: Vec<u64> = nodes.iter().map(|node| node.at).collect();
    for node in &nodes {
        out.pad_to(node.at)?;
        out.put(&o.bytes(node.entries.len() as u64, layout.count_len() as usize))?;
        for ((tag, kind, count, made), place) in node.entries.iter().zip(&node.values) {
            out.put(&o.bytes(u64::from(*tag), 2))?;
            out.put(&o.bytes(u64::from(*kind), 2))?;
            out.put(&o.bytes(*count, word))?;
            match place {
                Some(place) => out.put(&o.bytes(*place, word))?,
                None => {
                    let mut field = Vec::with_capacity(word);
                    value(r, &mut field, o, node, *kind, made, &ats, &mut buf)?;
                    field.resize(word, 0);
                    out.put(&field)?;
                }
            }
        }
        out.put(&o.bytes(node.next.map_or(0, |n| ats[n]), word))?;
        for ((_, kind, _, made), place) in node.entries.iter().zip(&node.values) {
            if let Some(place) = place {
                out.pad_to(*place)?;
                value(r, &mut out, o, node, *kind, made, &ats, &mut buf)?;
            }
        }
    }

    // The first IFD's offset, in the header.
    out.w
        .seek(SeekFrom::Start(layout.header() - layout.word()))
        .map_err(Error::Write)?;
    out.w
        .write_all(&o.bytes(ats[first], word))
        .map_err(Error::Write)
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
        entries: Vec::new(),
        next: None,
        at: 0,
        values: Vec::new(),
        blocks: Vec::new(),
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
    let mut tags: HashSet<u16> = [273, 279, 324, 325, 330].into();
    tags.extend(made.iter().map(|m| m.0));
    let mut entries: Vec<(u16, u16, u64, Made)> = dir
        .fields
        .iter()
        .enumerate()
        .filter(|(_, f)| tags.insert(f.tag))
        .map(|(n, f)| (f.tag, f.kind, f.count, Made::Field(n)))
        .collect();
    entries.extend(made);
    entries.sort_by_key(|e| e.0);
    nodes[at].entries = entries;
    at
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
    let mut numbers = |numbers: &mut dyn Iterator<Item = u64>| {
        for v in numbers {
            w.write_all(&order.bytes(v, size)).map_err(Error::Write)?;
        }
        Ok(())
    };
    match made {
        Made::Field(n) => match &node.dir.fields[*n].value {
            Data::Bytes(bytes) => w.write_all(bytes).map_err(Error::Write),
            Data::Source { offset, len } => copy(r, w, *offset, *len, buf),
        },
        Made::Offsets => numbers(&mut node.blocks.iter().map(|b| b.offset)),
        Made::Counts => numbers(&mut node.blocks.iter().map(|b| b.len)),
        Made::Ifds(ifds) => numbers(&mut ifds.iter().map(|&n| ats[n])),
    }
}

/// Copies `len` bytes at `offset` in `r` into `w`, through `buf`.
fn copy(
    r: &mut dyn In,
    w: &mut dyn Write,
    offset: u64,
    len: u64,
    buf: &mut [u8],
) -> Result<(), Error> {
    seek(r, offset)?;
    let mut left = len;
    while left > 0 {
        let n = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let step = &mut buf[..n];
        read_exact(r, step)?;
        w.write_all(step).map_err(Error::Write)?;
        left -= step.len() as u64;
    }
    Ok(())
}

/// The output, counting where the next byte goes.
struct Counted<'w> {
    w: &'w mut dyn Out,
    at: u64,
}

impl Counted<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.write_all(bytes).map_err(Error::Write)
    }

    /// Pads to an even offset.
    fn align(&mut self) -> Result<(), Error> {
        let to = self.at + self.at % 2;
        self.pad_to(to)
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

impl Write for Counted<'_> {
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
    use super::super::tests::{gray, Builder, Page, B, V};
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
        write(
            &mut r,
            &mut out,
            tiff.layout,
            &tiff.shared,
            &pages,
            &CancelToken::new(),
        )
        .unwrap();
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
            assert_eq!(b.shared, [blocks[0]].into());
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
            &tiff.shared,
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
            &tiff.shared,
            &pages,
            &cancel,
        );
        assert!(matches!(result, Err(Error::Write(_))));
        assert_eq!(out.into_inner().len(), 8);
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
