//! Compress on HEIC's graft (D28): the source's metadata items put into
//! the HEIC Grape's image helper wrote (`out.heic`, in memory), in place of
//! any the helper wrote itself. Pure: [`Graft::new`] plans the file,
//! [`Graft::write`] streams it, and [`Graft::check`] reads what was written
//! again.
//!
//! - **What `out.heic` must be** (D33's invariants, checked on every one):
//!   `ftyp` first, naming a still HEIC (`heic`, `heix` or `mif1` among its
//!   brands: a sequence's `msf1` and `hevc` sniff as HEIC too), then one
//!   `meta`, `mdat`s and `free` or `skip` boxes in any order, and nothing
//!   after its last box; `meta` read by the [model](super::items); a
//!   primary item that's HEVC (`hvc1`, or a `grid` of `hvc1` tiles); every
//!   `iloc` entry an item's, stored in the file (construction method 0,
//!   each extent with a length, outside `meta`) or in `idat` (1), in this
//!   file; and no metadata item of its own in a reference but a `cdsc` (the
//!   rewrite would leave a grid a tile short, or a thumbnail of nothing).
//!   Anything else is broken ([`Error::Broken`]), which Compress reports as
//!   an encode failure: nothing is published.
//! - **The helper's own metadata goes:** every item that's metadata by its
//!   type (Exif, `mime`, `uri `, `iptc`), or that describes another (a
//!   `cdsc` from it) and isn't a picture, as Remove Metadata drops them:
//!   its `infe`, `iloc` entry, references, property associations and
//!   group memberships. Its bytes are zeroed where they're its own (in an
//!   `mdat` or `idat`, overlapping no kept item's).
//! - **The source's are added** ([`Added`]), in order: new ids after the
//!   largest item or group id there is, each with an `infe` (version 2, or
//!   3 for an id past 16 bits) of its type, name, hidden flag and content
//!   type and encoding (`mime`) or URI type (`uri `), an `iloc` entry of
//!   one extent in one new `mdat` at the end of the file, and a `cdsc` to
//!   the primary (in a new `iref` when there's none).
//! - **`meta` is written again** with the helper's `iloc` version and field
//!   sizes, and every other box, entry and property as it was (an `iinf`,
//!   `iloc`, `iref` or `ipma` count past what its version holds, or an id
//!   it can't hold, is broken). It grows or shrinks by Δ, so every extent
//!   stored at or after its end moves by Δ: in the base offset when every
//!   extent of the item moves and it has one, else in each extent's offset;
//!   one that a field can't hold (or has none of) is broken. `idat`'s
//!   content keeps its place in `meta`, so data there doesn't move.
//! - **The file** is what came before `meta`, the new `meta`, what came
//!   after it (a last box that ran to the end given its size), then the new
//!   `mdat`.
//! - **Checked** ([`Graft::check`]): the file is read again from disk, and
//!   must be the size planned, with the same primary, exactly the kept and
//!   added items, each kept item's data the same bytes as in `out.heic`, and
//!   each added one of its type, described as the primary, in the one
//!   extent the plan gave it; the new `mdat`'s data is then read once, in
//!   order, against the added items' (each item found by its id in a map:
//!   no step is a search of them all, so 60,000 items take as long as
//!   their bytes).
//! - **Memory:** `out.heic` (its size), its `meta` and model, and the new
//!   `meta`, written twice (the first time for its size); then, for the
//!   check, the written file's `meta` and model and two 64 KiB buffers.

use std::collections::{HashMap, HashSet};
use std::io::{self, Cursor, Write};
use std::ops::Range;

use super::super::heic::{end_box, merged, overlaps, put_id, start_box, within};
use super::super::strip::{read_at, Error, In};
use super::boxes::{load, Child, Form, Header, Top};
use super::items::{file_meta, name, Location, Meta, Role};

/// How much is written, or read and compared, at a time.
const STEP: usize = 64 * 1024;

/// A metadata item to add: its `infe` fields and its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Added {
    /// Its item type: `Exif`, `mime`, `uri `, `iptc`, or another.
    pub(crate) kind: [u8; 4],
    /// `infe`'s flag 1.
    pub(crate) hidden: bool,
    /// Its `item_name`, with no NUL.
    pub(crate) name: Vec<u8>,
    /// A `mime` item's content type, a `uri ` item's URI type.
    pub(crate) content_type: Vec<u8>,
    /// A `mime` item's content encoding (empty: none).
    pub(crate) encoding: Vec<u8>,
    /// Its data: never empty (an extent of length 0 runs to the end).
    pub(crate) data: Vec<u8>,
}

/// `iloc`'s version and field sizes, as `out.heic` has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fields {
    version: u8,
    offset: usize,
    length: usize,
    base: usize,
    /// 0 in version 0, which has no index fields.
    index: usize,
}

impl Fields {
    /// An item id's size in an entry.
    fn id(self) -> usize {
        if self.version < 2 {
            2
        } else {
            4
        }
    }

    /// Where an entry's base offset is, past its id, its construction
    /// method (versions 1 and 2) and its data reference.
    fn base_at(self) -> usize {
        self.id() + if self.version >= 1 { 2 } else { 0 } + 2
    }

    /// Where extent `k`'s offset is in an entry.
    fn offset_at(self, k: usize) -> usize {
        self.base_at() + self.base + 2 + k * (self.index + self.offset + self.length) + self.index
    }
}

/// `value` moved by `delta`, if a field of `size` bytes holds it.
fn shifted(value: u64, delta: i64, size: usize) -> Option<u64> {
    let moved = u64::try_from(i128::from(value) + i128::from(delta)).ok()?;
    match size {
        8 => Some(moved),
        0 => None,
        n => (moved >> (8 * n) == 0).then_some(moved),
    }
}

/// Writes `value` into `field` (its size), big-endian.
fn put_uint(field: &mut [u8], value: u64) {
    let n = field.len();
    field.copy_from_slice(&value.to_be_bytes()[8 - n..]);
}

/// Appends `value` in `size` bytes, if they hold it.
fn push_uint(out: &mut Vec<u8>, value: u64, size: usize, what: &str) -> Result<(), Error> {
    if size < 8 && value >> (8 * size) != 0 {
        return Err(Error::Broken(format!(
            "its iloc's {size}-byte {what} field can't hold {value}"
        )));
    }
    out.extend_from_slice(&value.to_be_bytes()[8 - size..]);
    Ok(())
}

/// What [`Graft::new`] read of `out.heic`, to write its `meta` again.
struct Plan<'a> {
    m: &'a Meta,
    fields: Fields,
    /// The helper's metadata items, which go.
    dropped: &'a HashSet<u32>,
    /// The ids given to the added items, in order.
    ids: &'a [u32],
    added: &'a [Added],
    /// Where the old `meta` box ends in `out.heic`.
    meta_end: u64,
    /// Where dropped data is in `idat`'s content, that no kept item's is.
    zero_idat: &'a [Range<u64>],
}

impl Plan<'_> {
    /// The `meta` box written again, Δ (`delta`) applied to the extents at
    /// or after its old end, the added items' data in an `mdat` whose
    /// content starts at `data_at`.
    fn meta(&self, form: Form, delta: i64, data_at: u64) -> Result<Vec<u8>, Error> {
        let m = self.m;
        let mut out = Vec::with_capacity(m.bytes.len() + 256 * self.added.len());
        let at = start_box(&mut out, b"meta", form);
        out.extend_from_slice(&m.bytes[..4]);
        for (i, child) in m.children.iter().enumerate() {
            if i == m.iinf.0 {
                self.iinf(child, &mut out)?;
            } else if i == m.iloc.0 {
                self.iloc(child, delta, data_at, &mut out)?;
            } else if m.iref.is_some_and(|(at, _)| at == i) {
                self.iref(child, &mut out)?;
            } else if m.iprp.as_ref().is_some_and(|p| p.child == i) {
                self.iprp(child, &mut out);
            } else if m.idat == Some(i) {
                let from = out.len() + child.form.head() as usize;
                copy_box(&mut out, &m.bytes[child.range.clone()]);
                for r in self.zero_idat {
                    out[from + r.start as usize..from + r.end as usize].fill(0);
                }
            } else if m.grpl == Some(i) {
                self.grpl(child, &mut out);
            } else {
                copy_box(&mut out, &m.bytes[child.range.clone()]);
            }
        }
        if m.iref.is_none() && !self.added.is_empty() {
            let wide = self.ids.iter().chain([&m.primary]).any(|&id| id > 0xFFFF);
            let b = start_box(&mut out, b"iref", Form::Small);
            out.extend_from_slice(&[u8::from(wide), 0, 0, 0]);
            self.cdsc(wide, &mut out);
            end_box(&mut out, b, Form::Small);
        }
        if u32::try_from(out.len() - at).is_err() && form != Form::Large {
            return Err(Error::Broken("its meta would pass 4 GiB".into()));
        }
        end_box(&mut out, at, form);
        Ok(out)
    }

    /// `iinf`: the kept `infe` boxes as they were, then the added items'.
    fn iinf(&self, child: &Child, out: &mut Vec<u8>) -> Result<(), Error> {
        let m = self.m;
        let start = child.content().start;
        let kept = || m.items.iter().filter(|i| !self.dropped.contains(&i.id));
        let n = kept().count() + self.added.len();
        let b = start_box(out, b"iinf", child.form);
        out.extend_from_slice(&m.bytes[start..start + 4]);
        if m.iinf.1 == 0 {
            let n = u16::try_from(n)
                .map_err(|_| Error::Broken(format!("its iinf v0 can't count {n} items")))?;
            out.extend_from_slice(&n.to_be_bytes());
        } else {
            out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
        }
        for item in kept() {
            copy_box(out, &m.bytes[item.range.clone()]);
        }
        for (&id, a) in self.ids.iter().zip(self.added) {
            let wide = id > 0xFFFF;
            let infe = start_box(out, b"infe", Form::Small);
            out.extend_from_slice(&[if wide { 3 } else { 2 }, 0, 0, u8::from(a.hidden)]);
            put_id(out, id, wide);
            out.extend_from_slice(&0u16.to_be_bytes());
            out.extend_from_slice(&a.kind);
            out.extend_from_slice(&a.name);
            out.push(0);
            match &a.kind {
                b"mime" => {
                    for text in [&a.content_type, &a.encoding] {
                        out.extend_from_slice(text);
                        out.push(0);
                    }
                }
                b"uri " => {
                    out.extend_from_slice(&a.content_type);
                    out.push(0);
                }
                _ => {}
            }
            end_box(out, infe, Form::Small);
        }
        end_box(out, b, child.form);
        Ok(())
    }

    /// `iloc`: each kept entry as it was, moved by `delta` where its data
    /// is after `meta`; then an entry for each added item, its data at
    /// `data_at` on, one after another.
    fn iloc(
        &self,
        child: &Child,
        delta: i64,
        data_at: u64,
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        let m = self.m;
        let f = self.fields;
        let c = child.content();
        let kept = || m.locations.iter().filter(|l| !self.dropped.contains(&l.id));
        let n = kept().count() + self.added.len();
        let b = start_box(out, b"iloc", child.form);
        // Its version, flags and field sizes.
        out.extend_from_slice(&m.bytes[c.start..c.start + 6]);
        if f.version < 2 {
            let n = u16::try_from(n)
                .map_err(|_| Error::Broken(format!("its iloc can't count {n} items")))?;
            out.extend_from_slice(&n.to_be_bytes());
        } else {
            out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
        }
        for loc in kept() {
            let at = out.len();
            out.extend_from_slice(&m.bytes[loc.range.clone()]);
            if loc.method == 0 && delta != 0 {
                self.shift(loc, delta, &mut out[at..])?;
            }
        }
        let mut at = data_at;
        for (&id, a) in self.ids.iter().zip(self.added) {
            if f.version < 2 && id > 0xFFFF {
                return Err(Error::Broken(format!(
                    "its iloc v{} can't hold item {id}",
                    f.version
                )));
            }
            put_id(out, id, f.version == 2);
            if f.version >= 1 {
                out.extend_from_slice(&0u16.to_be_bytes());
            }
            out.extend_from_slice(&0u16.to_be_bytes());
            // From the base offset when there's one, so an offset of 0.
            let (base, offset) = if f.base > 0 { (at, 0) } else { (0, at) };
            push_uint(out, base, f.base, "base offset")?;
            out.extend_from_slice(&1u16.to_be_bytes());
            push_uint(out, 0, f.index, "index")?;
            if f.offset == 0 && offset != 0 {
                return Err(Error::Broken("its iloc has no field for an offset".into()));
            }
            push_uint(out, offset, f.offset, "offset")?;
            let len = a.data.len() as u64;
            if f.length == 0 || len == 0 {
                return Err(Error::Broken("its iloc has no field for a length".into()));
            }
            push_uint(out, len, f.length, "length")?;
            at += len;
        }
        out.extend_from_slice(&m.bytes[m.iloc_end..c.end]);
        end_box(out, b, child.form);
        Ok(())
    }

    /// Moves the extents of `loc` (method 0) at or after `meta`'s old end
    /// by `delta`, in `entry` (its bytes as written): in the base offset
    /// when they all move and it holds the move, else in each one's offset.
    fn shift(&self, loc: &Location, delta: i64, entry: &mut [u8]) -> Result<(), Error> {
        let f = self.fields;
        let spans = self.m.spans(loc)?;
        let after: Vec<bool> = spans.iter().map(|s| s.start >= self.meta_end).collect();
        if !after.contains(&true) {
            return Ok(());
        }
        if !after.contains(&false) && f.base > 0 {
            if let Some(base) = shifted(loc.base, delta, f.base) {
                let at = f.base_at();
                put_uint(&mut entry[at..at + f.base], base);
                return Ok(());
            }
        }
        let extents = &self.m.extents[loc.extents.clone()];
        for (k, (e, _)) in extents
            .iter()
            .zip(&after)
            .enumerate()
            .filter(|(_, (_, &a))| a)
        {
            let Some(offset) = shifted(e.offset, delta, f.offset) else {
                return Err(Error::Broken(format!(
                    "item {}'s data can't move by {delta} in its iloc's fields",
                    loc.id
                )));
            };
            let at = f.offset_at(k);
            put_uint(&mut entry[at..at + f.offset], offset);
        }
        Ok(())
    }

    /// `iref` without the dropped items' references (a box left with none
    /// goes), then a `cdsc` from each added item to the primary; nothing
    /// when no box is left.
    fn iref(&self, child: &Child, out: &mut Vec<u8>) -> Result<(), Error> {
        let m = self.m;
        let wide = m.iref.is_some_and(|(_, v)| v == 1);
        if !wide && self.ids.iter().chain([&m.primary]).any(|&id| id > 0xFFFF) {
            return Err(Error::Broken(
                "its iref v0 can't hold the items added".into(),
            ));
        }
        let start = child.content().start;
        let at = start_box(out, b"iref", child.form);
        out.extend_from_slice(&m.bytes[start..start + 4]);
        let mut any = false;
        for r in m.refs.iter().filter(|r| !self.dropped.contains(&r.from)) {
            let to = m.to(r);
            let kept = || to.iter().copied().filter(|id| !self.dropped.contains(id));
            let n = kept().count();
            if n == to.len() {
                copy_box(out, &m.bytes[r.child.range.clone()]);
            } else if n > 0 {
                let b = start_box(out, &r.kind, r.child.form);
                put_id(out, r.from, wide);
                // Fewer than it had.
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
        self.cdsc(wide, out);
        if any || !self.added.is_empty() {
            end_box(out, at, child.form);
        } else {
            out.truncate(at);
        }
        Ok(())
    }

    /// A `cdsc` from each added item to the primary.
    fn cdsc(&self, wide: bool, out: &mut Vec<u8>) {
        for &id in self.ids {
            let b = start_box(out, b"cdsc", Form::Small);
            put_id(out, id, wide);
            out.extend_from_slice(&1u16.to_be_bytes());
            put_id(out, self.m.primary, wide);
            end_box(out, b, Form::Small);
        }
    }

    /// `iprp` as it was, without the dropped items' `ipma` entries.
    fn iprp(&self, child: &Child, out: &mut Vec<u8>) {
        let m = self.m;
        let touched = m
            .maps
            .iter()
            .any(|map| map.entries.iter().any(|e| self.dropped.contains(&e.item)));
        let Some(iprp) = m.iprp.as_ref().filter(|_| touched) else {
            copy_box(out, &m.bytes[child.range.clone()]);
            return;
        };
        let at = start_box(out, b"iprp", child.form);
        let mut maps = m.maps.iter().peekable();
        for (i, kid) in iprp.children.iter().enumerate() {
            let Some(map) = maps.next_if(|map| map.child == i) else {
                copy_box(out, &m.bytes[kid.range.clone()]);
                continue;
            };
            let start = kid.content().start;
            let b = start_box(out, b"ipma", kid.form);
            out.extend_from_slice(&m.bytes[start..start + 4]);
            let entries = || {
                map.entries
                    .iter()
                    .filter(|e| !self.dropped.contains(&e.item))
            };
            // Fewer than it had.
            out.extend_from_slice(&(entries().count() as u32).to_be_bytes());
            for entry in entries() {
                out.extend_from_slice(&m.bytes[entry.range.clone()]);
            }
            out.extend_from_slice(&m.bytes[map.end..kid.range.end]);
            end_box(out, b, kid.form);
        }
        end_box(out, at, child.form);
    }

    /// `grpl` without the dropped ids (a group left with none goes).
    fn grpl(&self, child: &Child, out: &mut Vec<u8>) {
        let m = self.m;
        let at = start_box(out, b"grpl", child.form);
        for g in &m.groups {
            let members = m.members(g);
            let kept = || {
                members
                    .iter()
                    .copied()
                    .filter(|id| !self.dropped.contains(id))
            };
            let n = kept().count();
            if n == members.len() {
                copy_box(out, &m.bytes[g.child.range.clone()]);
            } else if n > 0 {
                let start = g.child.content().start;
                let b = start_box(out, &g.child.kind, g.child.form);
                // Its version, flags and group id.
                out.extend_from_slice(&m.bytes[start..start + 8]);
                // Fewer than it had.
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
}

/// An item `out.heic` keeps with data, for the check: its id and its data,
/// in `out.heic`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    id: u32,
    pieces: Vec<Range<u64>>,
}

/// The file Compress writes: `out.heic` with the source's metadata
/// grafted in ([module docs](self)).
#[derive(Debug)]
pub(crate) struct Graft {
    /// `out.heic`, its dropped data zeroed and a last box that ran to the
    /// end given its size.
    file: Vec<u8>,
    /// Where its `meta` box is.
    old_meta: Range<usize>,
    /// The `meta` written in its place.
    meta: Vec<u8>,
    /// The new `mdat`'s header; empty when nothing is added.
    mdat: Vec<u8>,
    added: Vec<Added>,
    ids: Vec<u32>,
    primary: u32,
    /// Every item kept, in `iinf`'s order.
    items: Vec<u32>,
    /// Those with data.
    kept: Vec<Kept>,
}

impl Graft {
    /// Plans the graft of `added` into `file`, the helper's `out.heic`
    /// ([module docs](self)); [`Error::Broken`] says what in `file` it
    /// can't take.
    pub(crate) fn new(mut file: Vec<u8>, added: Vec<Added>) -> Result<Graft, Error> {
        let len = file.len() as u64;
        let top = Top::read(&mut Cursor::new(&file[..]), len)?;
        if top.tail != len {
            return Err(Error::Broken("bytes after its last box".into()));
        }
        if top.boxes.first().map(|b| &b.kind) != Some(b"ftyp") {
            return Err(Error::Broken("no ftyp box first".into()));
        }
        if let Some(b) = top.boxes[1..]
            .iter()
            .find(|b| !matches!(&b.kind, b"meta" | b"mdat" | b"free" | b"skip"))
        {
            return Err(Error::Broken(format!("a {} box", name(&b.kind))));
        }
        if !still_heic(&file, &top.boxes[0]) {
            return Err(Error::Broken("an ftyp that names no still HEIC".into()));
        }
        let meta_box = file_meta(&top)?;
        let m = Meta::parse(load(&mut Cursor::new(&file[..]), &meta_box)?)?;
        if m.item(m.primary).map(|i| i.role()) != Some(Role::Image) {
            return Err(Error::Broken("a primary item that isn't a picture".into()));
        }
        let describes: HashSet<u32> = m
            .refs
            .iter()
            .filter(|r| &r.kind == b"cdsc")
            .map(|r| r.from)
            .collect();
        let dropped: HashSet<u32> = m
            .items
            .iter()
            .filter(|i| {
                let role = i.role();
                role.metadata() || (role != Role::Image && describes.contains(&i.id))
            })
            .map(|i| i.id)
            .collect();
        if let Some((r, id)) = m.refs.iter().filter(|r| &r.kind != b"cdsc").find_map(|r| {
            let named = std::iter::once(&r.from).chain(m.to(r));
            named
                .copied()
                .find(|id| dropped.contains(id))
                .map(|id| (r, id))
        }) {
            return Err(Error::Broken(format!(
                "its metadata item {id} in a {} reference",
                name(&r.kind)
            )));
        }
        let kind = |id: u32| m.item(id).map(|i| i.kind);
        let hevc = match kind(m.primary) {
            Some(k) if &k == b"hvc1" => true,
            Some(k) if &k == b"grid" => {
                let tiles: Vec<u32> = m
                    .refs
                    .iter()
                    .filter(|r| &r.kind == b"dimg" && r.from == m.primary)
                    .flat_map(|r| m.to(r).iter().copied())
                    .collect();
                // Each looked up once: a grid has as many tiles as items.
                let kinds: HashMap<u32, [u8; 4]> = m.items.iter().map(|i| (i.id, i.kind)).collect();
                !tiles.is_empty() && tiles.iter().all(|t| kinds.get(t) == Some(b"hvc1"))
            }
            _ => false,
        };
        if !hevc {
            return Err(Error::Broken(
                "a primary item that isn't HEVC or a grid of it".into(),
            ));
        }
        let ids: HashSet<u32> = m.items.iter().map(|i| i.id).collect();
        if let Some(loc) = m.locations.iter().find(|l| !ids.contains(&l.id)) {
            return Err(Error::Broken(format!(
                "an iloc entry for item {}, which isn't there",
                loc.id
            )));
        }
        if added.iter().any(|a| a.data.is_empty()) {
            return Err(Error::Broken("an added item with no data".into()));
        }
        let fields = iloc_fields(&m);

        // Where every item's data is: kept data must be where the graft
        // can keep it; dropped data is zeroed where it's its own.
        let meta_range = meta_box.at..meta_box.end();
        let idat = m.idat().map(|r| r.len() as u64).unwrap_or(0);
        // Where idat's content is in `file`.
        let idat_at = m
            .idat()
            .map_or(0, |r| meta_box.content().start + r.start as u64);
        let mut kept = Vec::new();
        let (mut kept_file, mut kept_idat) = (Vec::new(), Vec::new());
        let (mut drop_file, mut drop_idat) = (Vec::new(), Vec::new());
        for loc in &m.locations {
            let drop = dropped.contains(&loc.id);
            if !drop && loc.method == 2 {
                return Err(Error::Broken(format!(
                    "item {} is stored in other items",
                    loc.id
                )));
            }
            if !drop && loc.data_ref != 0 {
                return Err(Error::Broken(format!("item {} is in another file", loc.id)));
            }
            if loc.method == 2 {
                continue;
            }
            let size = if loc.method == 0 { len } else { idat };
            let mut pieces = Vec::new();
            for span in m.spans(loc)? {
                let range = span.start..span.end(size);
                let bad = if range.start > range.end || range.end > size {
                    Some("runs past its end")
                } else if loc.method == 0 && span.len.is_none() {
                    Some("runs to the end of the file")
                } else if loc.method == 0 && overlaps(std::slice::from_ref(&meta_range), &range) {
                    Some("is in its meta")
                } else {
                    None
                };
                match bad {
                    // What can't be zeroed is left as it is.
                    Some(_) if drop => {}
                    Some(why) => {
                        return Err(Error::Broken(format!("item {}'s data {why}", loc.id)))
                    }
                    None => pieces.push(range),
                }
            }
            match (drop, loc.method) {
                (true, 0) => drop_file.extend(pieces),
                (true, _) => drop_idat.extend(pieces),
                (false, 0) => {
                    kept_file.extend(pieces.iter().cloned());
                    kept.push(Kept { id: loc.id, pieces });
                }
                (false, _) => {
                    kept_idat.extend(pieces.iter().cloned());
                    let pieces = pieces
                        .into_iter()
                        .map(|p| idat_at + p.start..idat_at + p.end)
                        .collect();
                    kept.push(Kept { id: loc.id, pieces });
                }
            }
        }
        let (kept_file, kept_idat) = (merged(kept_file), merged(kept_idat));
        let mdats: Vec<Range<u64>> = top.all(b"mdat").map(|b| b.content()).collect();
        for r in merged(drop_file) {
            let in_mdat = mdats.iter().any(|m| m.start <= r.start && r.end <= m.end);
            if in_mdat && !overlaps(&kept_file, &r) {
                // Within the file.
                file[r.start as usize..r.end as usize].fill(0);
            }
        }
        let zero_idat: Vec<Range<u64>> = merged(drop_idat)
            .into_iter()
            .filter(|r| !overlaps(&kept_idat, r))
            .collect();
        // A last box that ran to the end ends where it ends now.
        for b in top.boxes.iter().filter(|b| b.form == Form::ToEnd) {
            let size = u32::try_from(b.len).map_err(|_| {
                Error::Broken(format!("a {} box of {} bytes", name(&b.kind), b.len))
            })?;
            let at = b.at as usize;
            file[at..at + 4].copy_from_slice(&size.to_be_bytes());
        }

        // The added items' ids, after every id there is.
        let last = m
            .items
            .iter()
            .map(|i| i.id)
            .chain(m.groups.iter().map(|g| g.id))
            .max()
            .unwrap_or(0);
        let new_ids: Vec<u32> = (1..=added.len() as u64)
            .map(|k| u32::try_from(u64::from(last) + k))
            .collect::<Result<_, _>>()
            .map_err(|_| Error::Broken("no item id left to add".into()))?;
        let plan = Plan {
            m: &m,
            fields,
            dropped: &dropped,
            ids: &new_ids,
            added: &added,
            meta_end: meta_range.end,
            zero_idat: &zero_idat,
        };
        let form = match meta_box.form {
            Form::Large => Form::Large,
            Form::Small | Form::ToEnd => Form::Small,
        };
        // Its size first: every field has its size, whatever it holds.
        let size = plan.meta(form, 0, 0)?.len() as u64;
        let delta = i64::try_from(i128::from(size) - i128::from(meta_box.len))
            .map_err(|_| Error::Broken("its meta changes by more than any file".into()))?;
        let data: u64 = added.iter().map(|a| a.data.len() as u64).sum();
        let mdat = if added.is_empty() {
            Vec::new()
        } else {
            let form = if data + 8 > u64::from(u32::MAX) {
                Form::Large
            } else {
                Form::Small
            };
            Header::write(b"mdat", form, form.head() + data)
        };
        let data_at = u64::try_from(i128::from(len) + i128::from(delta))
            .map_err(|_| Error::Broken("its meta changes by more than the file".into()))?
            + mdat.len() as u64;
        let meta = plan.meta(form, delta, data_at)?;
        if meta.len() as u64 != size {
            return Err(Error::Broken("its meta changed size".into()));
        }
        let items = m
            .items
            .iter()
            .map(|i| i.id)
            .filter(|id| !dropped.contains(id))
            .collect();
        Ok(Graft {
            old_meta: meta_box.at as usize..meta_box.end() as usize,
            file,
            meta,
            mdat,
            added,
            ids: new_ids,
            primary: m.primary,
            items,
            kept,
        })
    }

    /// The size of the file [`Graft::write`] writes.
    pub(crate) fn len(&self) -> u64 {
        let data: u64 = self.added.iter().map(|a| a.data.len() as u64).sum();
        (self.file.len() - self.old_meta.len() + self.meta.len() + self.mdat.len()) as u64 + data
    }

    /// The added items back, for another graft of the same items (HEIC
    /// Compress's "Try Small" trial).
    pub(crate) fn into_added(self) -> Vec<Added> {
        self.added
    }

    /// The ids the added items have, in order.
    #[cfg(test)]
    pub(crate) fn ids(&self) -> &[u32] {
        &self.ids
    }

    /// Writes the file into `w`, [`STEP`] at a time.
    pub(crate) fn write(&self, w: &mut dyn Write) -> io::Result<()> {
        let parts = [
            &self.file[..self.old_meta.start],
            &self.meta[..],
            &self.file[self.old_meta.end..],
            &self.mdat[..],
        ];
        let data = self.added.iter().map(|a| &a.data[..]);
        for part in parts.into_iter().chain(data) {
            for chunk in part.chunks(STEP) {
                w.write_all(chunk)?;
            }
        }
        Ok(())
    }

    /// Reads the file written, `r` (`len` bytes), again ([module
    /// docs](self)); [`Error::Broken`] says what's wrong.
    pub(crate) fn check(&self, r: &mut dyn In, len: u64) -> Result<(), Error> {
        let wrong = |why: String| Err(Error::Broken(format!("the grafted file {why}")));
        if len != self.len() {
            return wrong(format!("is {len} bytes, not {}", self.len()));
        }
        let top = Top::read(r, len)?;
        if top.tail != len || top.boxes.first().map(|b| &b.kind) != Some(b"ftyp") {
            return wrong("isn't laid out as planned".into());
        }
        let meta_box = file_meta(&top)?;
        let m = Meta::parse(load(r, &meta_box)?)?;
        if m.primary != self.primary {
            return wrong(format!("has primary item {}", m.primary));
        }
        let items: Vec<u32> = m.items.iter().map(|i| i.id).collect();
        let planned: Vec<u32> = self.items.iter().chain(&self.ids).copied().collect();
        if items != planned {
            return wrong(format!("has items {items:?}, not {planned:?}"));
        }
        let locations: HashMap<u32, &Location> = m.locations.iter().map(|l| (l.id, l)).collect();
        if locations.len() != self.kept.len() + self.ids.len() {
            return wrong("has other item locations".into());
        }
        let idat_at = m.idat().map(|i| meta_box.content().start + i.start as u64);
        let idat = m.idat().map_or(0, |i| i.len() as u64);
        // An item's data in the written file.
        let pieces = |id: u32| -> Result<Vec<Range<u64>>, Error> {
            let Some(loc) = locations.get(&id) else {
                return Err(Error::Broken(format!("the grafted file lost item {id}")));
            };
            let (size, at) = match (loc.method, idat_at) {
                (0, _) => (len, 0),
                (1, Some(at)) => (idat, at),
                _ => return Err(Error::Broken(format!("item {id} moved out of reach"))),
            };
            m.spans(loc)?
                .into_iter()
                .map(|s| {
                    let range = s.start..s.end(size);
                    if range.start > range.end || range.end > size {
                        return Err(Error::Broken(format!("item {id}'s data runs past its end")));
                    }
                    Ok(at + range.start..at + range.end)
                })
                .collect()
        };
        let total = |pieces: &[Range<u64>]| pieces.iter().map(|p| p.end - p.start).sum::<u64>();
        let mut bufs = (vec![0; STEP], vec![0; STEP]);
        for kept in &self.kept {
            let found = pieces(kept.id)?;
            if total(&found) != total(&kept.pieces)
                || !same(r, &found, &mut bufs, |at, b: &mut [u8]| {
                    fill(&self.file, &kept.pieces, at, b)
                })?
            {
                return wrong(format!("changed item {}'s data", kept.id));
            }
        }
        let described: HashSet<u32> = m
            .refs
            .iter()
            .filter(|r| &r.kind == b"cdsc" && m.to(r) == [m.primary])
            .map(|r| r.from)
            .collect();
        let kinds: HashMap<u32, [u8; 4]> = m.items.iter().map(|i| (i.id, i.kind)).collect();
        // The added items' data is the new mdat's, one after another, to
        // the end of the file: each must be where the plan put it, and the
        // whole read once, in order.
        let data: u64 = self.added.iter().map(|a| a.data.len() as u64).sum();
        let mut at = len.saturating_sub(data);
        for (&id, added) in self.ids.iter().zip(&self.added) {
            let found = pieces(id)?;
            let end = at + added.data.len() as u64;
            let one = locations.get(&id).is_some_and(|l| l.method == 0)
                && found.len() == 1
                && found[0] == (at..end);
            if !one || kinds.get(&id) != Some(&added.kind) {
                return wrong(format!("doesn't hold item {id} as planned"));
            }
            if !described.contains(&id) {
                return wrong(format!("doesn't say item {id} describes the primary"));
            }
            at = end;
        }
        let whole = len.saturating_sub(data)..len;
        let mut items = self.added.iter().map(|a| &a.data[..]);
        let mut item: &[u8] = &[];
        let same_data = same(r, std::slice::from_ref(&whole), &mut bufs, |_, b| {
            // In order: each call fills on from where the last stopped.
            let mut n = 0;
            while n < b.len() {
                if item.is_empty() {
                    match items.next() {
                        Some(next) => item = next,
                        None => return,
                    }
                }
                let k = item.len().min(b.len() - n);
                b[n..n + k].copy_from_slice(&item[..k]);
                item = &item[k..];
                n += k;
            }
        })?;
        if !same_data {
            return wrong("changed an added item's data".into());
        }
        Ok(())
    }
}

/// Whether the `ftyp` box `ftyp` of `file` names a still HEIC: `heic`,
/// `heix` or `mif1` as its major or a compatible brand.
fn still_heic(file: &[u8], ftyp: &Header) -> bool {
    let content = ftyp.content();
    let brands = usize::try_from(content.start)
        .ok()
        .zip(usize::try_from(content.end).ok())
        .and_then(|(start, end)| file.get(start..end))
        .unwrap_or_default();
    // The major brand, the minor version (skipped), the compatible ones.
    let major = brands.first_chunk::<4>().into_iter();
    let compatible = brands.get(8..).unwrap_or_default().as_chunks::<4>().0;
    major
        .chain(compatible)
        .any(|b| b == b"heic" || b == b"heix" || b == b"mif1")
}

/// Copies the box `bytes` to `out`, giving one that ran to the end of its
/// parent (size 0) its size: something may follow it now.
fn copy_box(out: &mut Vec<u8>, bytes: &[u8]) {
    let at = out.len();
    out.extend_from_slice(bytes);
    if bytes.get(..4) == Some(&[0; 4]) {
        // Within meta.
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }
}

/// `iloc`'s version and field sizes in `m`.
fn iloc_fields(m: &Meta) -> Fields {
    let c = m.children[m.iloc.0].content();
    // The model read them: version, flags, then the sizes.
    let (a, b) = (m.bytes[c.start + 4], m.bytes[c.start + 5]);
    let version = m.iloc.1;
    Fields {
        version,
        offset: usize::from(a >> 4),
        length: usize::from(a & 15),
        base: usize::from(b >> 4),
        index: if version == 0 { 0 } else { usize::from(b & 15) },
    }
}

/// Fills `buf` with the bytes of `space` its `pieces` (end to end) hold
/// from `at` on.
fn fill(space: &[u8], pieces: &[Range<u64>], at: u64, buf: &mut [u8]) {
    let mut n = 0;
    for r in within(pieces, at, buf.len() as u64) {
        let len = (r.end - r.start) as usize;
        buf[n..n + len].copy_from_slice(&space[r.start as usize..r.end as usize]);
        n += len;
    }
}

/// Whether the bytes of `r` in `found` (end to end) are those `expected`
/// fills in, a buffer of `bufs` at a time (two of the same length, read
/// and expected): `expected(at, buf)` fills `buf` with what should be from
/// `at` on, called in order (the caller checked that it's as long in all).
fn same(
    r: &mut dyn In,
    found: &[Range<u64>],
    bufs: &mut (Vec<u8>, Vec<u8>),
    mut expected: impl FnMut(u64, &mut [u8]),
) -> Result<bool, Error> {
    let (a, b) = (&mut bufs.0, &mut bufs.1);
    let mut at = 0u64;
    for range in found {
        let mut from = range.start;
        while from < range.end {
            let n = (range.end - from).min(a.len() as u64) as usize;
            read_at(r, from, &mut a[..n])?;
            b[..n].fill(0);
            expected(at, &mut b[..n]);
            if a[..n] != b[..n] {
                return Ok(false);
            }
            from += n as u64;
            at += n as u64;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::scan::Scan;
    use super::super::tests::{
        auxc, bx, camera, hvcc, ispe, meta_at, nclx, prop, read, simple, wide, Builder, Item,
        Layout, Place, EXIF, SECRET_XMP,
    };
    use super::*;

    /// What Compress carries from a source: an Exif, an XMP, a named text
    /// item with an encoding, a `uri ` item, IPTC and a type no one knows.
    fn carried() -> Vec<Added> {
        let item = |kind: &[u8; 4], hidden, name: &str, content_type: &str, data: &[u8]| Added {
            kind: *kind,
            hidden,
            name: name.as_bytes().to_vec(),
            content_type: content_type.as_bytes().to_vec(),
            encoding: Vec::new(),
            data: data.to_vec(),
        };
        let exif = [&6u32.to_be_bytes()[..], b"Exif\0\0", &camera()].concat();
        let mut text = item(b"mime", true, "Note", "text/plain", b"SourceNote");
        text.encoding = b"gzip".to_vec();
        vec![
            item(b"Exif", true, "", "", &exif),
            item(
                b"mime",
                true,
                "XMP",
                "application/rdf+xml",
                SECRET_XMP.as_bytes(),
            ),
            text,
            item(b"uri ", false, "", "urn:example:region", b"SourceRegion"),
            item(b"iptc", true, "", "", b"\x1c\x02\x5aSourceCity"),
            item(b"E\xEAif", false, "Odd", "", b"SourceOdd"),
        ]
    }

    /// The helper's own Exif and XMP, which go.
    const HELPER_EXIF: &[u8] = b"HelperExifBytes";
    const HELPER_XMP: &str = "<x:xmpmeta>HelperXmp</x:xmpmeta>";

    /// A picture as libheif 1.23 writes `out.heic` (pillow-heif 1.8.1's
    /// files, walked): `meta` before `mdat`, no `dinf`, `iloc` v0 with
    /// 4-byte offsets, lengths and base offsets (each item's where its data
    /// starts), `iinf` v0 of `infe` v2, and `hvcC`, `colr` `nclx`, `ispe`,
    /// `clap` and `pixi` properties.
    fn libheif() -> Builder {
        let mut primary = Item::picture(1, 0xA1, 798);
        primary.props = vec![(1, true), (2, false), (3, false), (4, true), (5, false)];
        let mut b = Builder::new(vec![primary], 1);
        b.properties = vec![
            hvcc(8, 8),
            nclx(1, 13, 6, true),
            ispe(64, 48),
            bx(b"clap", &[0; 32]),
            prop(b"pixi", &[0, 0, 0, 0, 3, 8, 8, 8]),
        ];
        b.iloc_version = 0;
        b.sizes = [4, 4, 4, 0];
        b.dinf = false;
        b.iloc_first = true;
        b
    }

    /// [`libheif`]'s with an Exif and an XMP of its own, as libheif writes
    /// what it's given.
    fn libheif_with_metadata() -> Builder {
        let mut b = libheif();
        let mut exif = Item::new(2, *b"Exif", Place::Mdat, HELPER_EXIF.to_vec());
        exif.hidden = true;
        b.items.extend([exif, Item::xmp(3, HELPER_XMP)]);
        b.refs = vec![(*b"cdsc", 2, vec![1]), (*b"cdsc", 3, vec![1])];
        b
    }

    /// A picture as ImageIO writes one large enough to tile: `iloc` v1 with
    /// 4-byte offsets and lengths and no base offset, a grid (in `idat`) of
    /// four hidden tiles, and a minimal Exif of its own.
    fn imageio() -> Builder {
        let mut grid = Item::new(1, *b"grid", Place::Idat, vec![0, 0, 1, 1, 0, 64, 0, 48]);
        grid.props = vec![(2, false), (3, true)];
        let mut items = vec![grid];
        for id in 2..=5 {
            let mut tile = Item::picture(id, 0xB0 | id as u8, 100 + id as usize);
            tile.hidden = true;
            tile.props = vec![(1, true), (4, false)];
            items.push(tile);
        }
        let mut exif = Item::new(6, *b"Exif", Place::Mdat, HELPER_EXIF.to_vec());
        exif.hidden = true;
        items.push(exif);
        let mut b = Builder::new(items, 1);
        b.properties = vec![
            hvcc(8, 8),
            ispe(64, 48),
            prop(b"colr", b"prof\0\0\0\x10icc-profile-data"),
            ispe(32, 24),
        ];
        b.refs = vec![(*b"dimg", 1, vec![2, 3, 4, 5]), (*b"cdsc", 6, vec![1])];
        b.sizes = [4, 4, 0, 0];
        b
    }

    /// Every layout of `out.heic` the graft must take, by name.
    fn helpers() -> Vec<(&'static str, Builder)> {
        let mut all = vec![
            ("libheif", libheif()),
            ("libheif with metadata", libheif_with_metadata()),
            ("ImageIO", imageio()),
        ];
        let mut b = libheif();
        let mut alpha = Item::picture(2, 0xAA, 39);
        alpha.props = vec![(1, true), (3, false), (6, true)];
        b.items.push(alpha);
        b.properties
            .push(auxc("urn:mpeg:mpegB:cicp:systems:auxiliary:alpha"));
        b.refs = vec![(*b"auxl", 2, vec![1])];
        all.push(("libheif with alpha", b));
        let mut b = imageio();
        b.meta_after_mdat = true;
        all.push(("ImageIO, meta after mdat", b));
        let mut b = libheif_with_metadata();
        b.meta_after_mdat = true;
        all.push(("libheif, meta after mdat", b));
        all.push(("iloc v2, wide ids, 64-bit fields", wide()));
        for (name, sizes) in [
            ("64-bit offsets, no base", [8, 8, 0, 0]),
            ("base offsets only", [0, 4, 4, 0]),
            ("64-bit base offsets", [4, 4, 8, 0]),
            ("64-bit lengths and indices", [4, 8, 4, 4]),
            ("indices", [4, 4, 0, 4]),
        ] {
            let mut b = imageio();
            b.sizes = sizes;
            all.push((name, b));
        }
        let mut b = imageio();
        b.items[5].place = Place::Idat;
        all.push(("its Exif in idat", b));
        let mut b = imageio();
        b.items[1].extents = 3;
        b.items[2].extents = 2;
        all.push(("several extents", b));
        let mut b = libheif_with_metadata();
        b.items[0].extents = 3;
        all.push(("several extents from a base", b));
        let mut b = libheif();
        b.meta_form = Form::Large;
        all.push(("largesize meta", b));
        let mut b = libheif_with_metadata();
        b.mdat_form = Form::ToEnd;
        all.push(("size-0 mdat last", b));
        let mut b = imageio();
        b.meta_form = Form::ToEnd;
        b.meta_after_mdat = true;
        all.push(("size-0 meta last", b));
        let mut b = imageio();
        b.mdat_form = Form::Large;
        all.push(("largesize mdat", b));
        let mut b = libheif_with_metadata();
        b.before = vec![bx(b"free", b"before")];
        b.between = vec![bx(b"skip", b"between")];
        b.after = vec![bx(b"free", b"after")];
        all.push(("free and skip boxes", b));
        let mut b = imageio();
        b.groups = vec![(*b"altr", 50, vec![1, 6]), (*b"ster", 51, vec![6])];
        all.push(("its Exif in groups", b));
        all
    }

    /// `file`'s `meta`, parsed.
    fn meta(file: &[u8]) -> Meta {
        let top = Top::read(&mut Cursor::new(file), file.len() as u64).unwrap();
        let header = file_meta(&top).unwrap();
        Meta::parse(load(&mut Cursor::new(file), &header).unwrap()).unwrap()
    }

    /// `added` grafted into `file`, written, and checked.
    fn grafted(file: &[u8], added: Vec<Added>) -> (Graft, Vec<u8>) {
        let g = Graft::new(file.to_vec(), added).unwrap();
        let mut out = Vec::new();
        g.write(&mut out).unwrap();
        assert_eq!(out.len() as u64, g.len());
        g.check(&mut Cursor::new(&out), out.len() as u64).unwrap();
        (g, out)
    }

    fn broken(file: &[u8], added: Vec<Added>) -> String {
        match Graft::new(file.to_vec(), added) {
            Err(Error::Broken(why)) => why,
            other => panic!("{other:?}"),
        }
    }

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    /// Checks `out`, `b`'s file (`layout`) with `added` grafted in: every
    /// picture `b` has reads the same bytes, the helper's metadata is gone
    /// (its bytes too), and the added items read back with every field as
    /// Compress's scan reads a source's.
    fn assert_grafted(name: &str, b: &Builder, layout: &Layout, out: &[u8], added: &[Added]) {
        let m = meta(out);
        assert_eq!(m.primary, b.primary, "{name}");
        for item in b.items.iter().filter(|i| i.place != Place::Nowhere) {
            match &item.kind {
                b"Exif" | b"mime" => {
                    assert!(m.item(item.id).is_none(), "{name}: {}", item.id);
                }
                _ => assert_eq!(read(out, &m, item.id), layout.data(item.id), "{name}"),
            }
        }
        assert!(!has(out, HELPER_EXIF), "{name}");
        assert!(!has(out, HELPER_XMP.as_bytes()), "{name}");
        let mut s = Scan::read(&mut Cursor::new(out), out.len() as u64).unwrap();
        s.load(&mut Cursor::new(out)).unwrap();
        assert_eq!(s.carried.len(), added.len(), "{name}");
        for (c, a) in s.carried.iter().zip(added) {
            assert_eq!(
                (
                    c.kind,
                    c.hidden,
                    &c.name,
                    &c.content_type,
                    &c.encoding,
                    &c.data
                ),
                (
                    a.kind,
                    a.hidden,
                    &a.name,
                    &a.content_type,
                    &a.encoding,
                    &a.data
                ),
                "{name}"
            );
        }
        // The groups lost the helper's Exif, and kept the rest.
        for g in &m.groups {
            assert!(!m.members(g).contains(&6), "{name}");
        }
    }

    #[test]
    fn every_helper_layout_takes_the_metadata_and_keeps_its_pictures() {
        for (name, b) in helpers() {
            let (file, layout) = b.build();
            for added in [carried(), carried()[..1].to_vec(), Vec::new()] {
                let (g, out) = grafted(&file, added.clone());
                assert_grafted(name, &b, &layout, &out, &added);
                // After every id it had.
                let last = b
                    .items
                    .iter()
                    .map(|i| i.id)
                    .chain(b.groups.iter().map(|g| g.1));
                let last = last.max().unwrap();
                let ids: Vec<u32> = (1..=added.len() as u32).map(|k| last + k).collect();
                assert_eq!(g.ids(), ids, "{name}");
            }
        }
    }

    #[test]
    fn a_file_with_nothing_to_drop_or_add_is_written_as_it_was() {
        for (name, b) in [
            ("libheif", libheif()),
            ("several extents", {
                let mut b = libheif();
                b.items[0].extents = 4;
                b
            }),
        ] {
            let (file, _) = b.build();
            let (_, out) = grafted(&file, Vec::new());
            assert_eq!(out, file, "{name}");
        }
        // A last box that ran to the end is given its size, and the rest
        // is the same.
        let mut b = libheif();
        b.mdat_form = Form::ToEnd;
        let (file, _) = b.build();
        let (_, out) = grafted(&file, Vec::new());
        let top = Top::read(&mut Cursor::new(&out), out.len() as u64).unwrap();
        let mdat = top.boxes.last().unwrap();
        assert_eq!((mdat.form, mdat.end()), (Form::Small, out.len() as u64));
        assert_eq!(out[..mdat.at as usize], file[..mdat.at as usize]);
        assert_eq!(out[mdat.at as usize + 4..], file[mdat.at as usize + 4..]);
    }

    #[test]
    fn data_after_meta_moves_by_what_meta_gained_or_lost() {
        // libheif's own metadata and none added: meta shrinks, and the
        // base offsets go back by as much.
        let (file, layout) = libheif_with_metadata().build();
        let (_, out) = grafted(&file, Vec::new());
        let shrunk = file.len() - out.len();
        assert!(shrunk > 50, "{shrunk}");
        let m = meta(&out);
        assert_eq!(m.locations.len(), 1);
        let loc = &m.locations[0];
        assert_eq!(loc.base, layout.file_ranges(1)[0].start - shrunk as u64);
        assert_eq!(m.extents[loc.extents.clone()][0].offset, 0);
        assert_eq!(read(&out, &m, 1), layout.data(1));
        // ImageIO's, with no base offsets: each extent's offset moves.
        let (file, layout) = imageio().build();
        let (_, out) = grafted(&file, carried());
        let data: u64 = carried().iter().map(|a| a.data.len() as u64).sum();
        // The new mdat's header and data come after.
        let grew = out.len() as u64 - file.len() as u64 - data - 8;
        let m = meta(&out);
        for id in 2..=5u32 {
            let loc = m.locations.iter().find(|l| l.id == id).unwrap();
            assert_eq!(loc.base, 0);
            let first = m.extents[loc.extents.clone()][0].offset;
            assert_eq!(first, layout.file_ranges(id)[0].start + grew, "{id}");
        }
        // Meta after mdat: nothing moves.
        let mut b = imageio();
        b.meta_after_mdat = true;
        let (file, _) = b.build();
        let (_, out) = grafted(&file, carried());
        let before = meta(&file);
        let after = meta(&out);
        for id in 2..=5u32 {
            let at = |m: &Meta| {
                let loc = m.locations.iter().find(|l| l.id == id).unwrap();
                m.extents[loc.extents.clone()].to_vec()
            };
            assert_eq!(at(&after), at(&before), "{id}");
        }
    }

    #[test]
    fn an_item_with_data_before_and_after_meta_moves_only_what_is_after() {
        // A second mdat before meta, holding the primary's first half: its
        // first extent points there, its second after meta.
        let mut b = libheif();
        b.sizes = [4, 4, 0, 0];
        b.iloc_version = 1;
        b.items[0].extents = 2;
        let (_, layout) = b.build();
        let data = layout.data(1);
        let half = layout.file_ranges(1)[0].end - layout.file_ranges(1)[0].start;
        b.before = vec![bx(b"mdat", &data[..half as usize])];
        let (mut file, layout) = b.build();
        let m = meta(&file);
        let loc = &m.locations[0];
        // ftyp's 24 bytes and the mdat's header.
        let at = meta_at(&file) + loc.range.start + 2 + 2 + 2 + 2;
        file[at..at + 4].copy_from_slice(&(24u32 + 8).to_be_bytes());
        assert_eq!(read(&file, &meta(&file), 1), data);
        let (_, out) = grafted(&file, carried());
        let m = meta(&out);
        assert_eq!(read(&out, &m, 1), layout.data(1));
        let extents = &m.extents[m.locations[0].extents.clone()];
        assert_eq!(extents[0].offset, 32);
        assert!(extents[1].offset > layout.file_ranges(1)[1].start);
    }

    #[test]
    fn a_shift_no_field_holds_is_refused() {
        assert_eq!(shifted(10, -3, 4), Some(7));
        assert_eq!(
            shifted(u64::from(u32::MAX) - 3, 3, 4),
            Some(u64::from(u32::MAX))
        );
        assert_eq!(shifted(u64::from(u32::MAX) - 3, 4, 4), None);
        assert_eq!(shifted(u64::MAX - 1, 1, 8), Some(u64::MAX));
        assert_eq!(shifted(u64::MAX, 1, 8), None);
        assert_eq!(shifted(2, -3, 8), None);
        assert_eq!(shifted(0, 0, 0), None);
        let mut field = [0u8; 4];
        put_uint(&mut field, 0x0102_0304);
        assert_eq!(field, [1, 2, 3, 4]);
        let mut out = Vec::new();
        assert!(push_uint(&mut out, 1 << 32, 4, "offset").is_err());
        assert!(push_uint(&mut out, 1, 0, "offset").is_err());
        push_uint(&mut out, 0, 0, "index").unwrap();
        push_uint(&mut out, 0xFFFF, 2, "length").unwrap();
        assert_eq!(out, [0xFF, 0xFF]);
    }

    #[test]
    fn what_out_heic_must_be_is_checked() {
        let (file, _) = libheif_with_metadata().build();
        let with = |kind: &[u8; 4]| [&file[..], &bx(kind, b"x")].concat();
        for (why, bad) in [
            ("a sequence", with(b"moov")),
            ("a uuid box", with(b"uuid")),
            ("junk after it", [&file[..], b"\x01junk"].concat()),
            ("no ftyp first", file[24..].to_vec()),
            ("two metas", with(b"meta")),
            ("no meta", bx(b"ftyp", b"heic\0\0\0\0mif1heic")),
        ] {
            assert!(Graft::new(bad, carried()).is_err(), "{why}");
        }
        // The primary isn't a picture.
        let mut b = libheif_with_metadata();
        b.primary = 2;
        b.refs.clear();
        broken(&b.build().0, carried());
        // A kept item's data in other items, in another file, to the end of
        // the file, in meta, or past the file's end; an iloc entry with no
        // item. iloc v1: id, construction method, data reference, extent
        // count, offset, length.
        let mut b = libheif();
        b.iloc_version = 1;
        b.sizes = [4, 4, 0, 0];
        let (file, _) = b.build();
        let at = meta_at(&file) + meta(&file).locations[0].range.start;
        let patched = |at: usize, value: &[u8]| {
            let mut f = file.clone();
            f[at..at + value.len()].copy_from_slice(value);
            f
        };
        for (why, bad, says) in [
            ("in other items", patched(at + 2, &[0, 2]), "other items"),
            ("another file", patched(at + 4, &[0, 1]), "another file"),
            ("to the end", patched(at + 12, &[0; 4]), "to the end"),
            (
                "in meta",
                patched(at + 8, &30u32.to_be_bytes()),
                "in its meta",
            ),
            (
                "past the end",
                patched(at + 12, &[0, 1, 0, 0]),
                "past its end",
            ),
            ("no item", patched(at, &[0, 9]), "isn't there"),
        ] {
            let why_not = broken(&bad, Vec::new());
            assert!(why_not.contains(says), "{why}: {why_not}");
        }
        // Brands that don't name a still HEIC (a sequence's, msf1 with
        // hevc, sniffs as HEIC); a primary that isn't HEVC, or a grid of
        // what isn't.
        for (why, brands, ok) in [
            ("a sequence's brands", &b"msf1\0\0\0\0msf1hevc"[..], false),
            ("AVIF's", b"avif\0\0\0\0avifmiaf", false),
            ("mif1 alone", b"mif1\0\0\0\0mif1", true),
            ("heix", b"heix\0\0\0\0mif1heix", true),
            ("heic compatible only", b"miaf\0\0\0\0miafheic", true),
        ] {
            let mut b = libheif();
            b.brands = brands.to_vec();
            let file = b.build().0;
            match ok {
                true => drop(grafted(&file, carried())),
                false => assert!(broken(&file, carried()).contains("still HEIC"), "{why}"),
            }
        }
        for kind in [*b"av01", *b"iden", *b"jpeg"] {
            let mut b = libheif();
            b.items[0].kind = kind;
            assert!(broken(&b.build().0, carried()).contains("HEVC"), "{kind:?}");
            let mut b = imageio();
            b.items[2].kind = kind;
            assert!(
                broken(&b.build().0, carried()).contains("HEVC"),
                "grid of {kind:?}"
            );
        }
        let mut b = imageio();
        b.refs.retain(|r| &r.0 != b"dimg");
        assert!(broken(&b.build().0, carried()).contains("HEVC"));
        // The helper's own metadata in a reference that isn't a cdsc: the
        // rewrite would leave a grid a tile short, or a thumbnail of
        // nothing.
        for (kind, from, to) in [
            (*b"dimg", 1, vec![2, 3, 4, 5, 6]),
            (*b"thmb", 6, vec![1]),
            (*b"auxl", 6, vec![1]),
        ] {
            let mut b = imageio();
            b.refs.retain(|r| r.0 != kind || r.1 != from);
            b.refs.push((kind, from, to));
            let why = broken(&b.build().0, carried());
            assert!(why.contains("item 6"), "{kind:?}: {why}");
        }
        // The helper's own Exif may be anywhere: it goes.
        let mut b = libheif_with_metadata();
        b.iloc_version = 1;
        b.sizes = [4, 4, 0, 0];
        let (file, _) = b.build();
        let at = meta_at(&file) + meta(&file).locations[1].range.start;
        let mut odd = file.clone();
        odd[at + 2..at + 6].copy_from_slice(&[0, 2, 0, 1]);
        let (_, out) = grafted(&odd, carried());
        assert!(meta(&out).item(2).is_none());
    }

    #[test]
    fn fields_or_ids_that_cant_hold_what_is_added_are_refused() {
        // Only idat: no offset field, then no length field, for the new
        // mdat; nothing to add is fine.
        for (sizes, says) in [([0, 4, 0, 0], "offset"), ([4, 0, 0, 0], "length")] {
            let mut b = Builder::new(vec![Item::new(1, *b"hvc1", Place::Idat, vec![7; 40])], 1);
            b.properties = vec![hvcc(8, 8)];
            b.items[0].props = vec![(1, true)];
            b.sizes = sizes;
            let (file, _) = b.build();
            assert!(broken(&file, carried()).contains(says), "{sizes:?}");
            grafted(&file, Vec::new());
        }
        // 16-bit ids used up, in iloc v1.
        let mut b = simple();
        b.items[0].id = 0xFFFF;
        b.primary = 0xFFFF;
        b.refs = vec![(*b"cdsc", EXIF, vec![0xFFFF])];
        let (file, _) = b.build();
        assert!(broken(&file, carried()).contains("can't hold item 65536"));
        grafted(&file, Vec::new());
        // In iloc v2, a v0 iref can't name it.
        let mut b = simple();
        b.iloc_version = 2;
        b.items[0].id = 0xFFFF;
        b.primary = 0xFFFF;
        b.refs = vec![(*b"cdsc", EXIF, vec![0xFFFF])];
        let (file, _) = b.build();
        assert!(broken(&file, carried()).contains("iref v0"));
        // An item with no data can't be added.
        let mut empty = carried();
        empty[0].data.clear();
        assert!(broken(&simple().build().0, empty).contains("no data"));
    }

    #[test]
    fn a_new_iref_is_wide_when_an_id_is() {
        // 16-bit ids used up with no iref: the new one is v1.
        let mut b = libheif();
        b.iloc_version = 2;
        b.items[0].id = 0xFFFF;
        b.primary = 0xFFFF;
        let (file, layout) = b.build();
        let (_, out) = grafted(&file, carried());
        let m = meta(&out);
        assert_eq!(m.iref.map(|(_, v)| v), Some(1));
        assert_eq!(read(&out, &m, 0xFFFF), layout.data(0xFFFF));
        // libheif's, with none: v0.
        let (file, _) = libheif().build();
        let m = meta(&grafted(&file, carried()).1);
        assert_eq!(m.iref.map(|(_, v)| v), Some(0));
        let cdsc: Vec<_> = m.refs.iter().filter(|r| &r.kind == b"cdsc").collect();
        assert_eq!(cdsc.len(), carried().len());
    }

    /// A file in memory that counts its seeks: the check's reads, each
    /// out of the file's order.
    struct Counting<'a> {
        inner: Cursor<&'a [u8]>,
        seeks: usize,
    }

    impl io::Read for Counting<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl io::BufRead for Counting<'_> {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            self.inner.fill_buf()
        }

        fn consume(&mut self, n: usize) {
            self.inner.consume(n);
        }
    }

    impl io::Seek for Counting<'_> {
        fn seek(&mut self, to: io::SeekFrom) -> io::Result<u64> {
            self.seeks += 1;
            self.inner.seek(to)
        }
    }

    impl In for Counting<'_> {}

    #[test]
    fn the_check_reads_the_added_items_in_one_pass() {
        // 20,000 items of 4 bytes: the check reads them where they are, one
        // after another, not each with a seek of its own (and finds each
        // item once, not by a search of them all).
        let added: Vec<Added> = (0..20_000u32)
            .map(|i| Added {
                kind: *b"Exif",
                hidden: true,
                name: Vec::new(),
                content_type: Vec::new(),
                encoding: Vec::new(),
                data: i.to_be_bytes().to_vec(),
            })
            .collect();
        let (file, _) = libheif().build();
        let g = Graft::new(file, added).unwrap();
        let mut out = Vec::new();
        g.write(&mut out).unwrap();
        let mut r = Counting {
            inner: Cursor::new(&out),
            seeks: 0,
        };
        g.check(&mut r, out.len() as u64).unwrap();
        assert!(r.seeks < 100, "{} seeks", r.seeks);
        // A changed byte among them is still caught.
        let mut bad = out.clone();
        let at = bad.len() - 4 * 10_000 + 1;
        bad[at] ^= 1;
        assert!(g.check(&mut Cursor::new(&bad), bad.len() as u64).is_err());
    }

    #[test]
    fn the_check_catches_a_changed_byte() {
        let (file, _) = imageio().build();
        let (g, out) = grafted(&file, carried());
        let m = meta(&out);
        let kept = read(&out, &m, 3);
        let added = read(&out, &m, g.ids()[1]);
        let at = |needle: &[u8]| out.windows(needle.len()).position(|w| w == needle).unwrap();
        for at in [
            at(&kept),
            at(&added),
            at(&added) + added.len() - 1,
            0,
            out.len() - 1,
        ] {
            let mut bad = out.clone();
            bad[at] ^= 0x20;
            assert!(
                g.check(&mut Cursor::new(&bad), bad.len() as u64).is_err(),
                "{at}"
            );
        }
        assert!(g
            .check(&mut Cursor::new(&out[1..]), out.len() as u64 - 1)
            .is_err());
    }

    #[test]
    fn the_helpers_metadata_over_a_kept_items_data_stays_as_it_is() {
        // The helper's Exif pointed into the primary's data: the Exif goes,
        // and the primary's bytes stay.
        let mut b = libheif_with_metadata();
        b.iloc_version = 1;
        b.sizes = [4, 4, 0, 0];
        let (mut file, layout) = b.build();
        let m = meta(&file);
        let exif = m.locations.iter().find(|l| l.id == 2).unwrap();
        let at = meta_at(&file) + exif.range.start + 8;
        let inside = layout.file_ranges(1)[0].start as u32 + 10;
        file[at..at + 4].copy_from_slice(&inside.to_be_bytes());
        let (_, out) = grafted(&file, carried());
        assert_eq!(read(&out, &meta(&out), 1), layout.data(1));
    }

    #[test]
    fn junk_and_cuts_never_panic() {
        for b in [imageio(), libheif_with_metadata(), wide()] {
            let (file, _) = b.build();
            let at = meta_at(&file) - 12;
            let mut taken = 0;
            for i in at..file.len().min(at + 1200) {
                for v in [0, 1, 0x7F, 0xFF] {
                    let mut f = file.clone();
                    f[i] = v;
                    if let Ok(g) = Graft::new(f, carried()) {
                        let mut out = Vec::new();
                        g.write(&mut out).unwrap();
                        // What the graft takes, it writes as planned.
                        g.check(&mut Cursor::new(&out), out.len() as u64)
                            .unwrap_or_else(|e| panic!("byte {i} set to {v}: {e:?}"));
                        taken += 1;
                    }
                }
            }
            assert!(taken > 100, "{taken}");
            for n in 0..file.len() {
                let _ = Graft::new(file[..n].to_vec(), carried());
            }
        }
    }
}
