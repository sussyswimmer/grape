//! The PDF file Grape writes for images → PDF and Create PDF (D37), front
//! to back, one object at a time, so only the page being written is ever
//! in memory.
//!
//! - `%PDF-1.4`, then a binary comment line.
//! - Object 1 is the catalog, written first. Object 2 is the page tree: its
//!   number is reserved up front and it is written last, once every page
//!   is known.
//! - Each stream's bytes are complete in memory before its object is
//!   written, so `/Length` is a direct number.
//! - A classic cross-reference table (20 bytes an entry) and a trailer with
//!   `/Size` and `/Root` only: no Info dictionary, no `/ID`, no dates.
//!
//! [`Sink`] is the in-memory buffer a stream is built in: reserved once, at
//! its bound, and refusing to grow past it.

use std::fmt;
use std::io::{self, Write};

use crate::CancelToken;

/// The catalog's object number.
pub(crate) const CATALOG: u32 = 1;

/// The page tree's object number.
pub(crate) const PAGES: u32 = 2;

/// The first bytes of every file: the version, then a comment of four
/// bytes over 127 so tools treat the file as binary.
const HEADER: &[u8] = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n";

/// The largest offset a 10-digit cross-reference entry holds.
pub(crate) const MAX_OFFSET: u64 = 9_999_999_999;

/// A PDF being written to `out`.
#[derive(Debug)]
pub(crate) struct Writer<W: Write> {
    out: W,
    /// Bytes written so far: the next object's offset.
    at: u64,
    /// Where each object starts, by number less one; `None` until written.
    offsets: Vec<Option<u64>>,
    /// The page objects, in order.
    kids: Vec<u32>,
}

impl<W: Write> Writer<W> {
    /// Writes the header and the catalog; the page tree's number is
    /// reserved.
    pub(crate) fn begin(out: W) -> io::Result<Writer<W>> {
        let mut writer = Writer {
            out,
            at: 0,
            offsets: vec![None, None],
            kids: Vec::new(),
        };
        writer.put(HEADER)?;
        writer.object(CATALOG, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        Ok(writer)
    }

    /// A new object number, to be written later (exactly once).
    pub(crate) fn reserve(&mut self) -> u32 {
        self.offsets.push(None);
        // At most one object per stream and page, so far below u32::MAX.
        u32::try_from(self.offsets.len()).unwrap_or(u32::MAX)
    }

    /// Bytes written so far.
    pub(crate) fn position(&self) -> u64 {
        self.at
    }

    /// Writes object `n`: `body` between `n 0 obj` and `endobj`.
    pub(crate) fn object(&mut self, n: u32, body: &[u8]) -> io::Result<()> {
        self.start(n)?;
        self.put(body)?;
        self.put(b"\nendobj\n")
    }

    /// Writes object `n` as a stream of `data` with the dictionary entries
    /// `entries` (`/Length` is added).
    pub(crate) fn stream(&mut self, n: u32, entries: &str, data: &[u8]) -> io::Result<()> {
        self.stream_parts(n, entries, [data])
    }

    /// [`Writer::stream`] with the data in `parts`, written one after the
    /// other (gone through twice: once for the length, once to write).
    pub(crate) fn stream_parts<'a, I>(&mut self, n: u32, entries: &str, parts: I) -> io::Result<()>
    where
        I: IntoIterator<Item = &'a [u8]> + Clone,
    {
        let length: usize = parts.clone().into_iter().map(<[u8]>::len).sum();
        self.start(n)?;
        let dict = if entries.is_empty() {
            format!("<< /Length {length} >>\nstream\n")
        } else {
            format!("<< {entries} /Length {length} >>\nstream\n")
        };
        self.put(dict.as_bytes())?;
        for part in parts {
            self.put(part)?;
        }
        self.put(b"\nendstream\nendobj\n")
    }

    /// Adds the page object `page` (written or still to be) to the tree.
    pub(crate) fn add_page(&mut self, page: u32) {
        self.kids.push(page);
    }

    /// Writes the page tree, the cross-reference table and the trailer, and
    /// returns the output. Fails if a reserved object was never written or
    /// the file is too large for the table.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        let mut tree = String::from("<< /Type /Pages /Kids [");
        for (i, kid) in self.kids.iter().enumerate() {
            if i > 0 {
                tree.push(' ');
            }
            tree.push_str(&format!("{kid} 0 R"));
        }
        tree.push_str(&format!("] /Count {} >>", self.kids.len()));
        self.object(PAGES, tree.as_bytes())?;

        let xref_at = self.at;
        if xref_at > MAX_OFFSET {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the PDF is too large for its cross-reference table",
            ));
        }
        let mut offsets = Vec::with_capacity(self.offsets.len());
        for (i, offset) in self.offsets.iter().enumerate() {
            match offset {
                Some(offset) => offsets.push(*offset),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("object {} was reserved but never written", i + 1),
                    ))
                }
            }
        }
        let size = offsets.len() + 1;
        let mut table = format!("xref\n0 {size}\n0000000000 65535 f \n");
        for offset in offsets {
            table.push_str(&format!("{offset:010} 00000 n \n"));
        }
        table.push_str(&format!(
            "trailer\n<< /Size {size} /Root {CATALOG} 0 R >>\nstartxref\n{xref_at}\n%%EOF\n"
        ));
        self.put(table.as_bytes())?;
        Ok(self.out)
    }

    /// Records where object `n` starts and writes `n 0 obj`.
    fn start(&mut self, n: u32) -> io::Result<()> {
        let index = usize::try_from(n)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "object 0"))?;
        let at = self.at;
        match self.offsets.get_mut(index) {
            Some(slot @ None) => *slot = Some(at),
            Some(Some(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("object {n} written twice"),
                ))
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("object {n} was never reserved"),
                ))
            }
        }
        self.put(format!("{n} 0 obj\n").as_bytes())
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.at = self.at.saturating_add(bytes.len() as u64);
        Ok(())
    }
}

/// `v` as a PDF number: at most 4 decimals, no exponent, trailing zeros
/// trimmed (`612`, `0.5`, `0.3333`).
pub(crate) fn num(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let fixed = format!("{v:.4}");
    let trimmed = if fixed.contains('.') {
        fixed.trim_end_matches('0').trim_end_matches('.')
    } else {
        fixed.as_str()
    };
    match trimmed {
        "-0" | "" => "0".to_string(),
        other => other.to_string(),
    }
}

/// The error a full [`Sink`] fails a write with.
#[derive(Debug)]
struct Full;

impl fmt::Display for Full {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the stream is over its bound")
    }
}

impl std::error::Error for Full {}

/// The buffer a stream is built in: reserved once at `cap` bytes, so it
/// never reallocates, and refusing ([`Sink::is_full`]) a write that would
/// take it past `cap`. Every write fails once `cancel` is set, which stops
/// an encoder at its next write.
#[derive(Debug)]
pub(crate) struct Sink {
    buf: Vec<u8>,
    cap: usize,
    cancel: CancelToken,
}

impl Sink {
    /// An empty buffer of `cap` bytes. `Err` when the memory can't be had.
    pub(crate) fn with_cap(cap: usize, cancel: &CancelToken) -> Result<Sink, ()> {
        let mut buf = Vec::new();
        buf.try_reserve_exact(cap).map_err(|_| ())?;
        Ok(Sink {
            buf,
            cap,
            cancel: cancel.clone(),
        })
    }

    /// What was written.
    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    /// True when `e` is this sink refusing a write past its bound.
    #[cfg(test)]
    pub(crate) fn is_full(e: &io::Error) -> bool {
        e.get_ref().is_some_and(|inner| inner.is::<Full>())
    }
}

impl Write for Sink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        if data.len() > self.cap - self.buf.len() {
            return Err(io::Error::other(Full));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The offset each `N 0 obj` really starts at, by parsing the table.
    pub(crate) fn xref(pdf: &[u8]) -> (Vec<u64>, usize) {
        let key = b"startxref\n";
        let at = pdf.windows(key.len()).rposition(|w| w == key).unwrap();
        let tail = String::from_utf8(pdf[at + key.len()..].to_vec()).unwrap();
        let start: usize = tail.lines().next().unwrap().parse().unwrap();
        let table = String::from_utf8(pdf[start..].to_vec()).unwrap();
        // Every entry is 20 bytes: ten digits, five, a type, a space and
        // a line feed.
        assert!(table.split("trailer").next().unwrap().ends_with(" \n"));
        assert!(table.starts_with("xref\n0 "), "{table}");
        let mut lines = table.lines();
        lines.next();
        let size: usize = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(lines.next(), Some("0000000000 65535 f "));
        let offsets = (1..size)
            .map(|_| {
                let line = lines.next().unwrap();
                assert_eq!(line.len(), 19, "{line:?}");
                assert!(line.ends_with(" 00000 n "), "{line:?}");
                line[..10].parse().unwrap()
            })
            .collect();
        (offsets, size)
    }

    #[test]
    fn every_entry_points_at_its_object_and_the_tree_comes_last() {
        let mut writer = Writer::begin(Vec::new()).unwrap();
        let data = writer.reserve();
        let page = writer.reserve();
        writer.stream(data, "/Type /XObject", b"abc\ndef").unwrap();
        writer.object(page, b"<< /Type /Page >>").unwrap();
        writer.add_page(page);
        let pdf = writer.finish().unwrap();
        let (offsets, size) = xref(&pdf);
        assert_eq!(size, 5);
        for (i, offset) in offsets.iter().enumerate() {
            let at = usize::try_from(*offset).unwrap();
            let header = format!("{} 0 obj\n", i + 1);
            assert_eq!(
                &pdf[at..at + header.len()],
                header.as_bytes(),
                "object {}",
                i + 1
            );
        }
        assert!(pdf.starts_with(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n1 0 obj\n"));
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains(
            "3 0 obj\n<< /Type /XObject /Length 7 >>\nstream\nabc\ndef\nendstream\nendobj\n"
        ));
        assert!(text.contains("2 0 obj\n<< /Type /Pages /Kids [4 0 R] /Count 1 >>\nendobj\n"));
        assert!(text.contains("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n"));
        assert!(text.ends_with("%%EOF\n"));
        // The tree is the last object written.
        let tree_at = offsets[1];
        assert!(offsets.iter().all(|&o| o <= tree_at));
    }

    #[test]
    fn an_object_reserved_and_not_written_fails_the_file() {
        let mut writer = Writer::begin(Vec::new()).unwrap();
        writer.reserve();
        assert!(writer.finish().is_err());
        let mut writer = Writer::begin(Vec::new()).unwrap();
        assert!(writer.object(CATALOG, b"again").is_err());
        assert!(writer.object(0, b"zero").is_err());
        assert!(writer.object(9, b"unknown").is_err());
    }

    #[test]
    fn numbers_have_at_most_four_decimals_and_no_exponent() {
        assert_eq!(num(612.0), "612");
        assert_eq!(num(0.5), "0.5");
        assert_eq!(num(1.0 / 3.0), "0.3333");
        assert_eq!(num(2.0 / 3.0), "0.6667");
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(-0.00001), "0");
        assert_eq!(num(1e-5), "0");
        assert_eq!(num(-12.5), "-12.5");
        assert_eq!(num(14_400.0), "14400");
        assert_eq!(num(1e12), "1000000000000");
        assert_eq!(num(f64::NAN), "0");
    }

    #[test]
    fn a_sink_never_grows_past_its_bound() {
        let cancel = CancelToken::new();
        let mut sink = Sink::with_cap(4, &cancel).unwrap();
        sink.write_all(b"abc").unwrap();
        let err = sink.write_all(b"de").unwrap_err();
        assert!(Sink::is_full(&err));
        sink.write_all(b"d").unwrap();
        assert_eq!(sink.into_inner(), b"abcd");

        let mut sink = Sink::with_cap(4, &cancel).unwrap();
        cancel.cancel();
        let err = sink.write_all(b"a").unwrap_err();
        assert!(!Sink::is_full(&err));
    }
}
