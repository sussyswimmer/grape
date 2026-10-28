//! A TIFF builder for the tests, grown from `exif::tests::tiff`: pages,
//! SubIFD chains, Exif, GPS and Interop IFDs, strips or tiles, in either
//! byte order, classic or BigTIFF.
//!
//! The file is laid out as the header, every IFD (each page, then its
//! SubIFDs depth first, then its Exif, Interop and GPS IFDs), the values
//! that don't fit in their entries, then the blocks, so that cutting the
//! file anywhere cuts a block. IFDs and values start on even offsets;
//! blocks follow each other with no gap. Entries are sorted by tag.

#![allow(clippy::unwrap_used)]

use super::read::Order;

/// An entry's value; its type and count follow from it.
#[derive(Debug, Clone)]
pub(crate) enum V {
    Short(Vec<u16>),
    Long(Vec<u32>),
    Rational(Vec<(u32, u32)>),
    /// Text, with its NUL added.
    Ascii(&'static str),
    /// Bytes of this type as they are, never byte-swapped (Photoshop's
    /// IPTC typed as LONG); the count is their length over the type's size.
    Bytes(u16, Vec<u8>),
}

/// A strip or a tile.
#[derive(Debug, Clone)]
pub(crate) enum B {
    Data(Vec<u8>),
    /// The same bytes as this IFD's block n: one block, listed twice.
    Same(usize),
    /// 0 bytes at offset 0: a sparse tile.
    Empty,
}

impl B {
    pub(crate) fn data(bytes: &[u8]) -> B {
        B::Data(bytes.to_vec())
    }
}

/// An image IFD: a page or a SubIFD.
#[derive(Debug, Clone, Default)]
pub(crate) struct Page {
    /// Its tags, besides the ones the builder writes: the block offsets
    /// and counts, 330, 34665, 34853 and 40965.
    pub(crate) tags: Vec<(u16, V)>,
    pub(crate) blocks: Vec<B>,
    /// Tiles (324, 325) rather than strips (273, 279).
    pub(crate) tiled: bool,
    /// Its SubIFD chains.
    pub(crate) subs: Vec<Vec<Page>>,
    pub(crate) exif: Option<Vec<(u16, V)>>,
    /// The Exif IFD's Interop IFD.
    pub(crate) interop: Option<Vec<(u16, V)>>,
    pub(crate) gps: Option<Vec<(u16, V)>>,
    /// The offsets to write instead of the blocks' own.
    pub(crate) offsets: Option<Vec<u64>>,
    /// The byte counts to write instead of the blocks' own; empty leaves
    /// the tag out.
    pub(crate) counts: Option<Vec<u64>>,
}

/// A gray 8-bit page `w` × `h`, `rows` rows a strip, with these strips.
pub(crate) fn gray(w: u32, h: u32, rows: u32, strips: Vec<B>) -> Page {
    Page {
        tags: vec![
            (256, V::Long(vec![w])),
            (257, V::Long(vec![h])),
            (258, V::Short(vec![8])),
            (262, V::Short(vec![1])),
            (278, V::Long(vec![rows])),
        ],
        blocks: strips,
        ..Page::default()
    }
}

/// A TIFF to build.
#[derive(Debug, Clone)]
pub(crate) struct Builder {
    /// b'I' or b'M'.
    pub(crate) order: u8,
    pub(crate) big: bool,
    pub(crate) pages: Vec<Page>,
    /// The last page's next offset, instead of 0.
    pub(crate) last_next: Option<u64>,
    /// The blocks written last first.
    pub(crate) reversed: bool,
    /// Bytes after everything, that nothing points at.
    pub(crate) tail: Vec<u8>,
}

/// An entry's value once laid out: numbers that wait for positions, or
/// bytes.
enum Src {
    Bytes(Vec<u8>),
    /// IFD `node`'s block offsets (or its override).
    Offsets(usize),
    /// IFD `node`'s block counts (or its override).
    Counts(usize),
    /// The offsets of these IFDs.
    Ifds(Vec<usize>),
}

/// One IFD to write.
struct Node<'a> {
    /// (tag, type, count, value), sorted by tag.
    entries: Vec<(u16, u16, u64, Src)>,
    /// Its page, for an image IFD.
    page: Option<&'a Page>,
    /// The next IFD in its chain.
    next: Option<usize>,
}

impl Builder {
    pub(crate) fn new(order: u8, big: bool, pages: Vec<Page>) -> Builder {
        Builder {
            order,
            big,
            pages,
            last_next: None,
            reversed: false,
            tail: Vec::new(),
        }
    }

    /// The header's size: where the first page is.
    pub(crate) fn header(&self) -> u64 {
        if self.big {
            16
        } else {
            8
        }
    }

    fn order(&self) -> Order {
        if self.order == b'M' {
            Order::Big
        } else {
            Order::Little
        }
    }

    fn word(&self) -> u64 {
        if self.big {
            8
        } else {
            4
        }
    }

    /// The type of a block offset or count.
    fn number(&self) -> u16 {
        if self.big {
            16
        } else {
            4
        }
    }

    /// `value` as (type, count, bytes in the file's order).
    fn value(&self, value: &V) -> (u16, u64, Vec<u8>) {
        let o = self.order();
        let n = |len: usize| len as u64;
        match value {
            V::Short(v) => (
                3,
                n(v.len()),
                v.iter().flat_map(|&x| o.bytes(x.into(), 2)).collect(),
            ),
            V::Long(v) => (
                4,
                n(v.len()),
                v.iter().flat_map(|&x| o.bytes(x.into(), 4)).collect(),
            ),
            V::Rational(v) => (
                5,
                n(v.len()),
                v.iter()
                    .flat_map(|&(a, b)| [o.bytes(a.into(), 4), o.bytes(b.into(), 4)].concat())
                    .collect(),
            ),
            V::Ascii(s) => (2, n(s.len() + 1), [s.as_bytes(), &[0]].concat()),
            V::Bytes(kind, b) => {
                let size = super::read::type_size(*kind).unwrap_or(1) as usize;
                (*kind, n(b.len() / size), b.clone())
            }
        }
    }

    /// Adds `tags` as an IFD, and the IFDs under it, to `nodes`; gives its
    /// index.
    fn add<'a>(
        &self,
        nodes: &mut Vec<Node<'a>>,
        tags: &[(u16, V)],
        page: Option<&'a Page>,
    ) -> usize {
        let at = nodes.len();
        let mut entries: Vec<(u16, u16, u64, Src)> = tags
            .iter()
            .map(|(tag, value)| {
                let (kind, count, bytes) = self.value(value);
                (*tag, kind, count, Src::Bytes(bytes))
            })
            .collect();
        nodes.push(Node {
            entries: Vec::new(),
            page,
            next: None,
        });
        if let Some(page) = page {
            // A page with no blocks has no data tags at all.
            let (offsets, counts) = if page.tiled { (324, 325) } else { (273, 279) };
            let n = page.offsets.as_ref().map_or(page.blocks.len(), Vec::len);
            if n > 0 {
                entries.push((offsets, self.number(), n as u64, Src::Offsets(at)));
            }
            let counts_n = page.counts.as_ref().map_or(page.blocks.len(), Vec::len);
            if counts_n > 0 {
                entries.push((counts, self.number(), counts_n as u64, Src::Counts(at)));
            }
            let mut heads = Vec::new();
            for chain in &page.subs {
                let mut last: Option<usize> = None;
                for sub in chain {
                    let node = self.add(nodes, &sub.tags, Some(sub));
                    match last {
                        Some(prev) => nodes[prev].next = Some(node),
                        None => heads.push(node),
                    }
                    last = Some(node);
                }
            }
            if !heads.is_empty() {
                let kind = if self.big { 18 } else { 13 };
                entries.push((330, kind, heads.len() as u64, Src::Ifds(heads)));
            }
            let pointer = if self.big { 18 } else { 4 };
            if let Some(exif) = &page.exif {
                let node = self.add(nodes, exif, None);
                if let Some(interop) = &page.interop {
                    let inner = self.add(nodes, interop, None);
                    nodes[node]
                        .entries
                        .push((40965, pointer, 1, Src::Ifds(vec![inner])));
                    nodes[node].entries.sort_by_key(|e| e.0);
                }
                entries.push((34665, pointer, 1, Src::Ifds(vec![node])));
            }
            if let Some(gps) = &page.gps {
                let node = self.add(nodes, gps, None);
                entries.push((34853, pointer, 1, Src::Ifds(vec![node])));
            }
        }
        entries.sort_by_key(|e| e.0);
        nodes[at].entries = entries;
        at
    }

    /// The file.
    pub(crate) fn build(&self) -> Vec<u8> {
        let o = self.order();
        let word = self.word();
        let mut nodes = Vec::new();
        let mut pages: Vec<usize> = Vec::new();
        for page in &self.pages {
            let node = self.add(&mut nodes, &page.tags, Some(page));
            if let Some(&prev) = pages.last() {
                nodes[prev].next = Some(node);
            }
            pages.push(node);
        }
        let entry_len = if self.big { 20 } else { 12 };
        let count_len = if self.big { 8 } else { 2 };
        let even = |pos: u64| pos + pos % 2;
        // Where each IFD is, then each value that doesn't fit.
        let mut pos = self.header();
        let mut at = Vec::new();
        for node in &nodes {
            pos = even(pos);
            at.push(pos);
            pos += count_len + entry_len * node.entries.len() as u64 + word;
        }
        let size = |kind: u16, count: u64| super::read::type_size(kind).unwrap_or(1) * count;
        let mut value_at = Vec::new();
        for node in &nodes {
            let mut here = Vec::new();
            for &(_, kind, count, ref src) in &node.entries {
                let len = match src {
                    Src::Bytes(b) => b.len() as u64,
                    _ => size(kind, count),
                };
                if len > word {
                    pos = even(pos);
                    here.push(Some(pos));
                    pos += len;
                } else {
                    here.push(None);
                }
            }
            value_at.push(here);
        }
        // The blocks, after everything else.
        let mut data: Vec<(usize, usize)> = Vec::new();
        for (n, node) in nodes.iter().enumerate() {
            for (b, block) in node.page.map_or(&[][..], |p| &p.blocks).iter().enumerate() {
                if matches!(block, B::Data(_)) {
                    data.push((n, b));
                }
            }
        }
        if self.reversed {
            data.reverse();
        }
        let mut block_at = std::collections::HashMap::new();
        let mut blocks = Vec::new();
        for (n, b) in data {
            let Some(B::Data(bytes)) = nodes[n].page.map(|p| &p.blocks[b]) else {
                continue;
            };
            block_at.insert((n, b), pos);
            pos += bytes.len() as u64;
            blocks.extend_from_slice(bytes);
        }
        // Each image IFD's own offsets and counts.
        let own = |n: usize| -> (Vec<u64>, Vec<u64>) {
            let Some(page) = nodes[n].page else {
                return (Vec::new(), Vec::new());
            };
            let mut offsets = Vec::new();
            let mut counts = Vec::new();
            for (b, block) in page.blocks.iter().enumerate() {
                let (at, len) = match block {
                    B::Data(bytes) => (block_at[&(n, b)], bytes.len() as u64),
                    B::Same(i) => match &page.blocks[*i] {
                        B::Data(bytes) => (block_at[&(n, *i)], bytes.len() as u64),
                        _ => (0, 0),
                    },
                    B::Empty => (0, 0),
                };
                offsets.push(at);
                counts.push(len);
            }
            (
                page.offsets.clone().unwrap_or(offsets),
                page.counts.clone().unwrap_or(counts),
            )
        };
        let mut out = Vec::new();
        out.extend_from_slice(if self.order == b'M' { b"MM" } else { b"II" });
        if self.big {
            out.extend(o.bytes(43, 2));
            out.extend(o.bytes(8, 2));
            out.extend(o.bytes(0, 2));
        } else {
            out.extend(o.bytes(42, 2));
        }
        out.extend(o.bytes(at[0], word as usize));
        let pad = |out: &mut Vec<u8>, to: u64| out.resize(to as usize, 0);
        let mut values = Vec::new();
        for (n, node) in nodes.iter().enumerate() {
            pad(&mut out, at[n]);
            out.extend(o.bytes(node.entries.len() as u64, count_len as usize));
            for (e, (tag, kind, count, src)) in node.entries.iter().enumerate() {
                let bytes = match src {
                    Src::Bytes(b) => b.clone(),
                    Src::Offsets(i) | Src::Counts(i) => {
                        let (offsets, counts) = own(*i);
                        let numbers = if matches!(src, Src::Offsets(_)) {
                            offsets
                        } else {
                            counts
                        };
                        let size = super::read::type_size(*kind).unwrap_or(4) as usize;
                        numbers.iter().flat_map(|&v| o.bytes(v, size)).collect()
                    }
                    Src::Ifds(ifds) => {
                        let size = super::read::type_size(*kind).unwrap_or(4) as usize;
                        ifds.iter().flat_map(|&i| o.bytes(at[i], size)).collect()
                    }
                };
                out.extend(o.bytes((*tag).into(), 2));
                out.extend(o.bytes((*kind).into(), 2));
                out.extend(o.bytes(*count, word as usize));
                match value_at[n][e] {
                    Some(place) => {
                        out.extend(o.bytes(place, word as usize));
                        values.push((place, bytes));
                    }
                    None => {
                        let mut field = bytes;
                        field.resize(word as usize, 0);
                        out.extend(field);
                    }
                }
            }
            let next = match node.next {
                Some(next) => at[next],
                None if pages.last() == Some(&n) => self.last_next.unwrap_or(0),
                None => 0,
            };
            out.extend(o.bytes(next, word as usize));
        }
        for (place, bytes) in values {
            pad(&mut out, place);
            out.extend(bytes);
        }
        out.extend(blocks);
        out.extend_from_slice(&self.tail);
        out
    }
}
