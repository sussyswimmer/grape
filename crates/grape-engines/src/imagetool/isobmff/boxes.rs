//! ISOBMFF boxes (D28): a file's top-level boxes, read by seeking, and the
//! boxes inside a box Grape holds in memory.
//!
//! - **Headers:** a 32-bit size and the type (8 bytes), or size 1 and a
//!   64-bit `largesize` after the type (16 bytes); size 0 runs to the end
//!   of the file, or of the parent box. A type is four printable ASCII
//!   characters (or `©`).
//! - **The top level** ([`Top::read`]): each box's header, never its
//!   content. Bytes that can't start a box (fewer than 8 left, a size too
//!   small for its header or past the file's end, a type that isn't one)
//!   end the walk: from there on is the file's tail, junk or a box cut
//!   short, for the tool to judge.
//! - **In memory** ([`children`]): inside a box, what isn't a box is
//!   [`Error::Broken`].
//! - **Caps:** [`MAX_BOXES`] top-level boxes, [`MAX_META`] for the one box
//!   a tool loads ([`load`]), [`MAX_CHILDREN`] boxes in a box that has no
//!   cap of its own. Over one, the file is too large ([`Error::TooLarge`]),
//!   never cut short.

use std::ops::Range;

use super::super::strip::{read_at, Error, In};

/// The most top-level boxes in a file.
pub(crate) const MAX_BOXES: usize = 4_096;

/// The most bytes of the box a tool loads whole (the file-level `meta`; an
/// iPhone's is a few KiB).
pub(crate) const MAX_META: u64 = 16 << 20;

/// The most boxes in a box held in memory that has no cap of its own.
pub(crate) const MAX_CHILDREN: usize = 65_536;

/// How a box's header gives its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    /// A 32-bit size: an 8-byte header.
    Small,
    /// Size 1 and a 64-bit `largesize`: a 16-byte header.
    Large,
    /// Size 0: to the end of the file or the parent, an 8-byte header.
    ToEnd,
}

impl Form {
    /// The header's size.
    pub(crate) fn head(self) -> u64 {
        match self {
            Form::Large => 16,
            Form::Small | Form::ToEnd => 8,
        }
    }
}

/// A box: its type, and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) kind: [u8; 4],
    /// Where it starts.
    pub(crate) at: u64,
    /// Its size, header included.
    pub(crate) len: u64,
    pub(crate) form: Form,
}

impl Header {
    /// Where it ends.
    pub(crate) fn end(&self) -> u64 {
        self.at + self.len
    }

    /// Where its content is: after its header, to its end.
    pub(crate) fn content(&self) -> Range<u64> {
        self.at + self.form.head()..self.end()
    }

    /// The header written for a box of this form and type, `len` bytes in
    /// all: an explicit size (a `largesize` when it had one) even for one
    /// that ran to the end.
    pub(crate) fn write(kind: &[u8; 4], form: Form, len: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        match (form, u32::try_from(len)) {
            (Form::Small | Form::ToEnd, Ok(small)) => {
                out.extend_from_slice(&small.to_be_bytes());
                out.extend_from_slice(kind);
            }
            _ => {
                out.extend_from_slice(&1u32.to_be_bytes());
                out.extend_from_slice(kind);
                out.extend_from_slice(&len.to_be_bytes());
            }
        }
        out
    }
}

/// Reads the box header at the start of `bytes`, a box at `at` with `left`
/// bytes from there to the end of its file or parent; `None` when the
/// bytes can't start a box ([module docs](self)).
pub(crate) fn header(bytes: &[u8], at: u64, left: u64) -> Option<Header> {
    let size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?);
    let kind: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
    if !kind
        .iter()
        .all(|&b| (0x20..=0x7E).contains(&b) || b == 0xA9)
    {
        return None;
    }
    let (len, form) = match size {
        0 => (left, Form::ToEnd),
        1 => (
            u64::from_be_bytes(bytes.get(8..16)?.try_into().ok()?),
            Form::Large,
        ),
        n => (u64::from(n), Form::Small),
    };
    (len >= form.head() && len <= left).then_some(Header {
        kind,
        at,
        len,
        form,
    })
}

/// A file's top-level boxes ([module docs](self)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Top {
    /// The file's size.
    pub(crate) len: u64,
    /// Its boxes, in order, end to end from offset 0.
    pub(crate) boxes: Vec<Header>,
    /// Where the bytes that don't start a box start: `len` when there are
    /// none.
    pub(crate) tail: u64,
}

impl Top {
    /// Walks the top-level boxes of `r`, `len` bytes long.
    pub(crate) fn read(r: &mut dyn In, len: u64) -> Result<Top, Error> {
        let mut boxes = Vec::new();
        let mut at = 0;
        loop {
            let left = len - at;
            if left < 8 {
                break;
            }
            let mut head = [0; 16];
            // At most 16.
            let head = &mut head[..left.min(16) as usize];
            read_at(r, at, head)?;
            let Some(header) = header(head, at, left) else {
                break;
            };
            if boxes.len() == MAX_BOXES {
                return Err(Error::TooLarge(format!("over {MAX_BOXES} top-level boxes")));
            }
            boxes.push(header);
            at = header.end();
        }
        Ok(Top {
            len,
            boxes,
            tail: at,
        })
    }

    /// The boxes of type `kind`.
    pub(crate) fn all<'a>(&'a self, kind: &'a [u8; 4]) -> impl Iterator<Item = &'a Header> + 'a {
        self.boxes.iter().filter(move |b| &b.kind == kind)
    }
}

/// Reads the content of the box `header` (after its header) from `r`, at
/// most [`MAX_META`] bytes.
pub(crate) fn load(r: &mut dyn In, header: &Header) -> Result<Vec<u8>, Error> {
    let content = header.content();
    let n = content.end - content.start;
    if n > MAX_META {
        return Err(Error::TooLarge(format!(
            "a {} box of {n} bytes",
            String::from_utf8_lossy(&header.kind)
        )));
    }
    let mut bytes = vec![0; usize::try_from(n).map_err(|_| Error::TooLarge("a box".into()))?];
    read_at(r, content.start, &mut bytes)?;
    Ok(bytes)
}

/// A box inside a box held in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Child {
    pub(crate) kind: [u8; 4],
    /// The whole box, in the bytes that hold it.
    pub(crate) range: Range<usize>,
    pub(crate) form: Form,
}

impl Child {
    /// Its content, after its header.
    pub(crate) fn content(&self) -> Range<usize> {
        self.range.start + self.form.head() as usize..self.range.end
    }
}

/// The boxes in `bytes[range]`, end to end, at most `max`: anything else
/// there is broken, and more than `max` too large.
pub(crate) fn children(bytes: &[u8], range: Range<usize>, max: usize) -> Result<Vec<Child>, Error> {
    let mut out = Vec::new();
    let mut at = range.start;
    while at < range.end {
        let left = range.end - at;
        let head = bytes.get(at..range.end).unwrap_or_default();
        let Some(h) = header(head, at as u64, left as u64) else {
            return Err(Error::Broken("a box that isn't one".into()));
        };
        if out.len() == max {
            return Err(Error::TooLarge(format!("over {max} boxes in a box")));
        }
        // Within `range`, so it fits a usize.
        let end = at + h.len as usize;
        out.push(Child {
            kind: h.kind,
            range: at..end,
            form: h.form,
        });
        at = end;
    }
    Ok(out)
}

/// Big-endian fields read off a box's content, each bounds-checked: past
/// the end is [`Error::Broken`].
#[derive(Debug, Clone)]
pub(crate) struct Fields<'a> {
    bytes: &'a [u8],
    /// Where the next field is, in `bytes`.
    pub(crate) at: usize,
    end: usize,
}

impl<'a> Fields<'a> {
    /// The fields in `bytes[range]`.
    pub(crate) fn new(bytes: &'a [u8], range: Range<usize>) -> Fields<'a> {
        Fields {
            bytes,
            at: range.start,
            end: range.end.min(bytes.len()),
        }
    }

    /// The next `n` bytes.
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.end)
            .ok_or_else(|| Error::Broken("a box ends early".into()))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    /// An unsigned number of `size` bytes (0 to 8; 0 reads nothing, and is
    /// 0).
    pub(crate) fn uint(&mut self, size: usize) -> Result<u64, Error> {
        if size > 8 {
            return Err(Error::Broken(format!("a field of {size} bytes")));
        }
        Ok(self
            .take(size)?
            .iter()
            .fold(0, |acc, &b| acc << 8 | u64::from(b)))
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A FullBox's version and flags.
    pub(crate) fn full(&mut self) -> Result<(u8, u32), Error> {
        let v = self.u32()?;
        Ok(((v >> 24) as u8, v & 0x00FF_FFFF))
    }

    /// An item id: 32 bits when `wide`, 16 otherwise.
    pub(crate) fn id(&mut self, wide: bool) -> Result<u32, Error> {
        if wide {
            self.u32()
        } else {
            self.u16().map(u32::from)
        }
    }

    /// A 4-character code.
    pub(crate) fn kind(&mut self) -> Result<[u8; 4], Error> {
        let b = self.take(4)?;
        Ok([b[0], b[1], b[2], b[3]])
    }

    /// A NUL-terminated string, without its NUL; one with no NUL runs to
    /// the end, as libheif reads it.
    pub(crate) fn text(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.at..self.end];
        match rest.iter().position(|&b| b == 0) {
            Some(n) => {
                self.at += n + 1;
                &rest[..n]
            }
            None => {
                self.at = self.end;
                rest
            }
        }
    }

    /// What's left.
    pub(crate) fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at..self.end]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use super::*;

    fn bx(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut out = Header::write(kind, Form::Small, 8 + content.len() as u64);
        out.extend_from_slice(content);
        out
    }

    fn top(file: &[u8]) -> Result<Top, Error> {
        Top::read(&mut Cursor::new(file), file.len() as u64)
    }

    #[test]
    fn the_three_header_forms_are_read() {
        let mut file = bx(b"ftyp", b"heic\0\0\0\0");
        let large_at = file.len() as u64;
        file.extend_from_slice(&Header::write(b"free", Form::Large, 20));
        file.extend_from_slice(b"abcd");
        let end_at = file.len() as u64;
        file.extend_from_slice(b"\0\0\0\0mdat0123");
        let t = top(&file).unwrap();
        assert_eq!(
            t.boxes,
            [
                Header {
                    kind: *b"ftyp",
                    at: 0,
                    len: 16,
                    form: Form::Small
                },
                Header {
                    kind: *b"free",
                    at: large_at,
                    len: 20,
                    form: Form::Large
                },
                Header {
                    kind: *b"mdat",
                    at: end_at,
                    len: 12,
                    form: Form::ToEnd
                },
            ]
        );
        assert_eq!(t.tail, file.len() as u64);
        assert_eq!(t.boxes[1].content(), large_at + 16..large_at + 20);
    }

    #[test]
    fn what_cant_start_a_box_is_the_tail() {
        let file = bx(b"ftyp", b"heic\0\0\0\0");
        for (tail, why) in [
            (&b"\0\0\0"[..], "under 8 bytes"),
            (b"\0\0\0\x07free", "a size under its header"),
            (b"\0\0\0\x20free", "past the end"),
            (b"\0\0\0\x01free\0\0\0\0\0\0\0\x0F", "a largesize under 16"),
            (b"\0\0\0\x08fr\0e", "a type with a NUL"),
            (b"\0\0\0\0\0\0\0\0\0\0", "zeros"),
        ] {
            let mut f = file.clone();
            f.extend_from_slice(tail);
            let t = top(&f).unwrap();
            assert_eq!(t.boxes.len(), 1, "{why}");
            assert_eq!(t.tail, 16, "{why}");
        }
    }

    #[test]
    fn over_the_box_cap_is_too_large() {
        let mut file = Vec::new();
        for _ in 0..MAX_BOXES {
            file.extend_from_slice(&bx(b"free", b""));
        }
        assert_eq!(top(&file).unwrap().boxes.len(), MAX_BOXES);
        file.extend_from_slice(&bx(b"free", b""));
        assert!(matches!(top(&file), Err(Error::TooLarge(_))));
    }

    #[test]
    fn a_box_is_loaded_up_to_the_cap() {
        let file = bx(b"meta", &[7; 100]);
        let t = top(&file).unwrap();
        assert_eq!(
            load(&mut Cursor::new(&file), &t.boxes[0]).unwrap(),
            [7; 100]
        );
        let big = Header {
            kind: *b"meta",
            at: 0,
            len: MAX_META + 9,
            form: Form::Small,
        };
        assert!(matches!(
            load(&mut Cursor::new(&file), &big),
            Err(Error::TooLarge(_))
        ));
        let cut = Header {
            kind: *b"meta",
            at: 0,
            len: 200,
            form: Form::Small,
        };
        assert!(matches!(
            load(&mut Cursor::new(&file), &cut),
            Err(Error::Broken(_))
        ));
    }

    #[test]
    fn children_fill_their_parent_or_are_broken() {
        let mut parent = bx(b"hdlr", b"xx");
        parent.extend_from_slice(&[0, 0, 0, 0, b'i', b'd', b'a', b't', 1, 2]);
        let all = children(&parent, 0..parent.len(), 8).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].form, Form::ToEnd);
        assert_eq!(all[1].content(), 18..20);
        assert!(matches!(
            children(&parent, 0..parent.len(), 1),
            Err(Error::TooLarge(_))
        ));
        let mut junk = bx(b"hdlr", b"xx");
        junk.extend_from_slice(b"\0\0\0");
        assert!(matches!(
            children(&junk, 0..junk.len(), 8),
            Err(Error::Broken(_))
        ));
    }

    #[test]
    fn fields_are_bounds_checked() {
        let bytes = [1, 2, 3, 4, 5, b'a', 0, b'b'];
        let mut f = Fields::new(&bytes, 0..bytes.len());
        assert_eq!(f.u16().unwrap(), 0x0102);
        assert_eq!(f.uint(0).unwrap(), 0);
        assert_eq!(f.uint(3).unwrap(), 0x030405);
        assert_eq!(f.text(), b"a");
        assert_eq!(f.text(), b"b");
        assert!(matches!(f.u8(), Err(Error::Broken(_))));
        assert!(matches!(
            Fields::new(&bytes, 0..8).uint(9),
            Err(Error::Broken(_))
        ));
        let mut f = Fields::new(&bytes, 0..8);
        assert_eq!(f.full().unwrap(), (1, 0x020304));
        assert_eq!(f.rest(), [5, b'a', 0, b'b']);
    }
}
