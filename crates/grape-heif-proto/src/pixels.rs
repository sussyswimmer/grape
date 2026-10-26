//! The raw pixel file: `pixels.rgba` (decode output) and `input.rgba`
//! (encode input).
//!
//! ```text
//! 0..8    magic    b"GRAPEPX\x01"
//! 8..12   width    u32 LE, ≥ 1
//! 12..16  height   u32 LE, ≥ 1
//! 16..20  channels u32 LE, 3 (RGB) or 4 (RGBA, straight alpha)
//! 20..24  icc_len  u32 LE, 0 = none
//! 24..    icc_len bytes of ICC profile (it describes the samples; none = sRGB)
//! then    height rows of width × channels bytes, top row first, no padding
//! ```
//!
//! The file's length is exactly 24 + icc_len + width × height × channels.
//! The reader is given the length and refuses anything else.

use std::fmt;
use std::io::{self, Read, Write};

/// The file's first 8 bytes.
pub const MAGIC: [u8; 8] = *b"GRAPEPX\x01";

/// The header's size: the magic and four `u32`s.
pub const HEADER_BYTES: usize = 24;

/// How much [`read_pixels`] reads at a time, so a reader that checks for a
/// cancel on each read gets to check often.
const READ_STEP: usize = 1 << 20;

/// What the header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelHeader {
    /// Width in pixels, ≥ 1.
    pub width: u32,
    /// Height in pixels, ≥ 1.
    pub height: u32,
    /// 3 (RGB) or 4 (RGBA, straight alpha).
    pub channels: u8,
    /// The ICC profile's size, 0 for none.
    pub icc_len: u32,
}

impl PixelHeader {
    /// The bytes in one row (width × channels), or `None` when that
    /// doesn't fit in memory.
    pub fn row_len(&self) -> Option<usize> {
        usize::try_from(self.width)
            .ok()?
            .checked_mul(usize::from(self.channels))
    }

    /// The bytes of all the rows, or `None` on overflow.
    fn data_len(&self) -> Option<u64> {
        u64::from(self.width)
            .checked_mul(u64::from(self.height))?
            .checked_mul(u64::from(self.channels))
    }

    /// The whole file's length (24 + icc_len + the rows), or `None` on
    /// overflow.
    pub fn file_len(&self) -> Option<u64> {
        (HEADER_BYTES as u64)
            .checked_add(u64::from(self.icc_len))?
            .checked_add(self.data_len()?)
    }

    fn to_bytes(self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0u8; HEADER_BYTES];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..12].copy_from_slice(&self.width.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.height.to_le_bytes());
        bytes[16..20].copy_from_slice(&u32::from(self.channels).to_le_bytes());
        bytes[20..24].copy_from_slice(&self.icc_len.to_le_bytes());
        bytes
    }

    fn from_bytes(bytes: &[u8; HEADER_BYTES]) -> Result<PixelHeader, PixelError> {
        if bytes[..8] != MAGIC {
            return Err(PixelError::Malformed("this isn't a Grape pixel file"));
        }
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let (width, height, channels, icc_len) = (word(8), word(12), word(16), word(20));
        if width == 0 || height == 0 {
            return Err(PixelError::Malformed("the pixel file's image is empty"));
        }
        let channels = match channels {
            3 => 3,
            4 => 4,
            _ => {
                return Err(PixelError::Malformed(
                    "the pixel file's channels aren't 3 or 4",
                ))
            }
        };
        Ok(PixelHeader {
            width,
            height,
            channels,
            icc_len,
        })
    }
}

/// What [`read_pixels`] accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelLimits {
    /// The most pixels (width × height).
    pub max_pixels: u64,
    /// The longest side.
    pub max_side: u32,
    /// The largest ICC profile.
    pub max_icc_bytes: u32,
}

/// A whole pixel file in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixels {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// 3 (RGB) or 4 (RGBA, straight alpha).
    pub channels: u8,
    /// `height` rows of `width × channels` bytes, top row first.
    pub data: Vec<u8>,
    /// The ICC profile, if the file has one.
    pub icc: Option<Vec<u8>>,
}

/// Why a pixel file was refused.
#[derive(Debug)]
pub enum PixelError {
    /// Reading failed.
    Io(io::Error),
    /// The file isn't a valid pixel file (or its length is wrong).
    Malformed(&'static str),
    /// The file is valid but over a [`PixelLimits`] value. Never holds a
    /// path.
    TooLarge(String),
}

impl fmt::Display for PixelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PixelError::Io(e) => write!(f, "reading the pixel file: {:?}", e.kind()),
            PixelError::Malformed(why) => f.write_str(why),
            PixelError::TooLarge(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for PixelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PixelError::Io(e) => Some(e),
            PixelError::Malformed(_) | PixelError::TooLarge(_) => None,
        }
    }
}

/// Writes a pixel file row by row: [`begin`](Self::begin) writes the
/// header and the ICC profile, [`row`](Self::row) one row, and
/// [`finish`](Self::finish) checks that every row was written. It doesn't
/// buffer: give it a `BufWriter`.
#[derive(Debug)]
pub struct PixelWriter<W: Write> {
    out: W,
    row_len: usize,
    rows_left: u32,
}

impl<W: Write> PixelWriter<W> {
    /// Writes the header and `icc`. `header.icc_len` must be `icc`'s length
    /// (0 for `None`), the size at least 1×1, and channels 3 or 4; otherwise
    /// it fails with `InvalidInput` and writes nothing.
    pub fn begin(mut out: W, header: &PixelHeader, icc: Option<&[u8]>) -> io::Result<Self> {
        let invalid = |why: &'static str| io::Error::new(io::ErrorKind::InvalidInput, why);
        if header.width == 0 || header.height == 0 {
            return Err(invalid("the image is empty"));
        }
        if !matches!(header.channels, 3 | 4) {
            return Err(invalid("channels must be 3 or 4"));
        }
        let icc = icc.unwrap_or_default();
        if u32::try_from(icc.len()).ok() != Some(header.icc_len) {
            return Err(invalid("icc_len isn't the ICC profile's length"));
        }
        let row_len = match (header.row_len(), header.file_len()) {
            (Some(row_len), Some(_)) => row_len,
            _ => return Err(invalid("the image is too large")),
        };
        out.write_all(&header.to_bytes())?;
        out.write_all(icc)?;
        Ok(PixelWriter {
            out,
            row_len,
            rows_left: header.height,
        })
    }

    /// Writes the next row: exactly width × channels bytes. A row of
    /// another length, or one past the height, fails with `InvalidInput`.
    pub fn row(&mut self, row: &[u8]) -> io::Result<()> {
        if self.rows_left == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "more rows than the height",
            ));
        }
        if row.len() != self.row_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a row isn't width × channels bytes",
            ));
        }
        self.out.write_all(row)?;
        self.rows_left -= 1;
        Ok(())
    }

    /// Flushes and returns the writer once every row is written; otherwise
    /// fails with `InvalidData`.
    pub fn finish(mut self) -> io::Result<W> {
        if self.rows_left != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fewer rows than the height",
            ));
        }
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Reads a whole pixel file of `file_len` bytes. The header is checked
/// against `limits` and the length before anything else is allocated.
pub fn read_pixels(
    mut input: impl Read,
    file_len: u64,
    limits: &PixelLimits,
) -> Result<Pixels, PixelError> {
    if file_len < HEADER_BYTES as u64 {
        return Err(PixelError::Malformed(
            "the pixel file is shorter than its header",
        ));
    }
    let mut bytes = [0u8; HEADER_BYTES];
    input.read_exact(&mut bytes).map_err(early_end)?;
    let header = PixelHeader::from_bytes(&bytes)?;
    if header.icc_len > limits.max_icc_bytes {
        return Err(PixelError::TooLarge(format!(
            "an ICC profile of {} bytes is over {}",
            header.icc_len, limits.max_icc_bytes
        )));
    }
    if header.width > limits.max_side || header.height > limits.max_side {
        return Err(PixelError::TooLarge(format!(
            "{}x{} has a side over {} pixels",
            header.width, header.height, limits.max_side
        )));
    }
    let pixels = u64::from(header.width) * u64::from(header.height);
    if pixels > limits.max_pixels {
        return Err(PixelError::TooLarge(format!(
            "{}x{} is over {} pixels",
            header.width, header.height, limits.max_pixels
        )));
    }
    let (data_len, expected) = match (header.data_len(), header.file_len()) {
        (Some(data_len), Some(expected)) => (data_len, expected),
        _ => return Err(PixelError::Malformed("the pixel file's size overflows")),
    };
    if expected != file_len {
        return Err(PixelError::Malformed(
            "the pixel file's length doesn't match its header",
        ));
    }
    let icc = match header.icc_len {
        0 => None,
        len => Some(read_bytes(&mut input, u64::from(len))?),
    };
    let data = read_bytes(&mut input, data_len)?;
    if !at_end(&mut input)? {
        return Err(PixelError::Malformed(
            "the pixel file is longer than its header says",
        ));
    }
    Ok(Pixels {
        width: header.width,
        height: header.height,
        channels: header.channels,
        data,
        icc,
    })
}

/// Reads exactly `len` bytes into a buffer reserved up front, in steps of
/// [`READ_STEP`].
fn read_bytes(input: &mut impl Read, len: u64) -> Result<Vec<u8>, PixelError> {
    let len = usize::try_from(len)
        .map_err(|_| PixelError::TooLarge(format!("{len} bytes don't fit in memory")))?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|_| PixelError::Io(io::ErrorKind::OutOfMemory.into()))?;
    buf.resize(len, 0);
    for chunk in buf.chunks_mut(READ_STEP) {
        input.read_exact(chunk).map_err(early_end)?;
    }
    Ok(buf)
}

/// True when `input` has no byte left.
fn at_end(input: &mut impl Read) -> Result<bool, PixelError> {
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(n) => return Ok(n == 0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(PixelError::Io(e)),
        }
    }
}

/// An early end means the length the caller gave was wrong.
fn early_end(e: io::Error) -> PixelError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        PixelError::Malformed("the pixel file ends early")
    } else {
        PixelError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use super::*;

    const ANY: PixelLimits = PixelLimits {
        max_pixels: u64::MAX,
        max_side: u32::MAX,
        max_icc_bytes: u32::MAX,
    };

    fn header(width: u32, height: u32, channels: u8, icc_len: u32) -> PixelHeader {
        PixelHeader {
            width,
            height,
            channels,
            icc_len,
        }
    }

    /// A file with the given rows, through the writer.
    fn file(width: u32, height: u32, channels: u8, icc: Option<&[u8]>) -> Vec<u8> {
        let icc_len = icc.map_or(0, |p| p.len() as u32);
        let mut writer =
            PixelWriter::begin(Vec::new(), &header(width, height, channels, icc_len), icc).unwrap();
        let row_len = width as usize * channels as usize;
        for y in 0..height {
            let row: Vec<u8> = (0..row_len)
                .map(|x| (x as u32 * 7 + y * 13) as u8)
                .collect();
            writer.row(&row).unwrap();
        }
        writer.finish().unwrap()
    }

    /// A header followed by `rest`, written by hand.
    fn raw(width: u32, height: u32, channels: u32, icc_len: u32, rest: usize) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        for word in [width, height, channels, icc_len] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.resize(bytes.len() + rest, 9);
        bytes
    }

    fn read(bytes: &[u8], limits: &PixelLimits) -> Result<Pixels, PixelError> {
        read_pixels(Cursor::new(bytes), bytes.len() as u64, limits)
    }

    #[test]
    fn the_header_is_the_documented_layout() {
        let bytes = file(2, 1, 4, Some(b"icc!"));
        assert_eq!(&bytes[..8], b"GRAPEPX\x01");
        assert_eq!(&bytes[8..12], &[2, 0, 0, 0]);
        assert_eq!(&bytes[12..16], &[1, 0, 0, 0]);
        assert_eq!(&bytes[16..20], &[4, 0, 0, 0]);
        assert_eq!(&bytes[20..24], &[4, 0, 0, 0]);
        assert_eq!(&bytes[24..28], b"icc!");
        assert_eq!(bytes.len(), 24 + 4 + 2 * 4);
        assert_eq!(header(2, 1, 4, 4).file_len(), Some(36));
        assert_eq!(header(2, 1, 4, 4).row_len(), Some(8));
    }

    #[test]
    fn files_round_trip() {
        for (width, height, channels, icc) in [
            (1, 1, 3, None),
            (3, 5, 3, None),
            (5, 3, 4, None),
            (64, 2, 4, Some(&[1u8, 2, 3][..])),
            (7, 9, 3, Some(&[0xAAu8; 600][..])),
        ] {
            let bytes = file(width, height, channels, icc);
            let pixels = read(&bytes, &ANY).unwrap();
            assert_eq!(
                (pixels.width, pixels.height, pixels.channels),
                (width, height, channels)
            );
            assert_eq!(pixels.icc.as_deref(), icc);
            let start = 24 + icc.map_or(0, <[u8]>::len);
            assert_eq!(pixels.data, bytes[start..]);
            assert_eq!(
                pixels.data.len(),
                width as usize * height as usize * channels as usize
            );
        }
    }

    #[test]
    fn a_large_file_reads_in_steps() {
        // Over READ_STEP, so the data takes several reads.
        let bytes = file(1024, 300, 4, None);
        let pixels = read(&bytes, &ANY).unwrap();
        assert_eq!(pixels.data.len(), 1024 * 300 * 4);
        assert_eq!(pixels.data, bytes[24..]);
    }

    #[test]
    fn the_length_must_be_exact() {
        let bytes = file(3, 2, 3, Some(b"ab"));
        let len = bytes.len() as u64;
        // The caller's length one short or one over.
        assert!(matches!(
            read_pixels(Cursor::new(&bytes), len - 1, &ANY),
            Err(PixelError::Malformed(_))
        ));
        assert!(matches!(
            read_pixels(Cursor::new(&bytes), len + 1, &ANY),
            Err(PixelError::Malformed(_))
        ));
        // The file itself one short (the length given matches the header).
        let short = &bytes[..bytes.len() - 1];
        assert!(matches!(
            read_pixels(Cursor::new(short), len, &ANY),
            Err(PixelError::Malformed("the pixel file ends early"))
        ));
        // The file one over, read with the header's length.
        let mut long = bytes.clone();
        long.push(0);
        assert!(matches!(
            read_pixels(Cursor::new(&long), len, &ANY),
            Err(PixelError::Malformed(
                "the pixel file is longer than its header says"
            ))
        ));
        // Both one over or one short: the header disagrees.
        assert!(matches!(read(&long, &ANY), Err(PixelError::Malformed(_))));
        assert!(matches!(read(short, &ANY), Err(PixelError::Malformed(_))));
        assert!(matches!(
            read(&bytes[..23], &ANY),
            Err(PixelError::Malformed(
                "the pixel file is shorter than its header"
            ))
        ));
        assert!(matches!(read(&[], &ANY), Err(PixelError::Malformed(_))));
    }

    #[test]
    fn bad_headers_are_malformed() {
        let mut bad_magic = raw(1, 1, 3, 0, 3);
        bad_magic[7] = 2;
        for bytes in [
            bad_magic,
            raw(0, 1, 3, 0, 0),
            raw(1, 0, 3, 0, 0),
            raw(1, 1, 2, 0, 2),
            raw(1, 1, 5, 0, 5),
            raw(1, 1, 0, 0, 0),
            raw(1, 1, 1 << 8 | 3, 0, 3),
        ] {
            assert!(
                matches!(read(&bytes, &ANY), Err(PixelError::Malformed(_))),
                "accepted {:?}",
                &bytes[..24]
            );
        }
        // A well-formed header still reads.
        assert!(read(&raw(1, 1, 3, 0, 3), &ANY).is_ok());
    }

    #[test]
    fn a_size_that_overflows_is_refused() {
        let bytes = raw(u32::MAX, u32::MAX, 4, 0, 0);
        assert!(matches!(
            read(&bytes, &ANY),
            Err(PixelError::Malformed("the pixel file's size overflows"))
        ));
        let bytes = raw(u32::MAX, u32::MAX, 3, u32::MAX, 0);
        assert!(matches!(read(&bytes, &ANY), Err(PixelError::Malformed(_))));
        assert_eq!(header(u32::MAX, u32::MAX, 4, 0).file_len(), None);
    }

    #[test]
    fn limits_are_checked_before_the_length() {
        let limits = PixelLimits {
            max_pixels: 100,
            max_side: 20,
            max_icc_bytes: 8,
        };
        // Exactly at each limit: accepted.
        assert!(read(&file(10, 10, 3, Some(&[1; 8])), &limits).is_ok());
        assert!(read(&file(20, 5, 4, None), &limits).is_ok());
        // One over: refused, even with a wrong length.
        for bytes in [
            raw(11, 10, 3, 0, 0),
            raw(21, 1, 3, 0, 0),
            raw(1, 21, 3, 0, 0),
            raw(1, 1, 3, 9, 0),
            file(10, 10, 3, Some(&[1; 9])),
        ] {
            assert!(
                matches!(read(&bytes, &limits), Err(PixelError::TooLarge(_))),
                "accepted {:?}",
                &bytes[..24]
            );
        }
        let err = read(&raw(11, 10, 3, 0, 0), &limits).unwrap_err();
        assert_eq!(err.to_string(), "11x10 is over 100 pixels");
    }

    #[test]
    fn read_errors_pass_through() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("cancelled"))
            }
        }
        let err = read_pixels(Broken, 100, &ANY).unwrap_err();
        assert!(matches!(&err, PixelError::Io(e) if e.kind() == io::ErrorKind::Other));
        assert_eq!(err.to_string(), "reading the pixel file: Other");
    }

    #[test]
    fn the_writer_checks_its_header() {
        let invalid = |result: io::Result<PixelWriter<Vec<u8>>>| matches!(result, Err(e) if e.kind() == io::ErrorKind::InvalidInput);
        assert!(invalid(PixelWriter::begin(
            Vec::new(),
            &header(0, 1, 3, 0),
            None
        )));
        assert!(invalid(PixelWriter::begin(
            Vec::new(),
            &header(1, 0, 3, 0),
            None
        )));
        assert!(invalid(PixelWriter::begin(
            Vec::new(),
            &header(1, 1, 2, 0),
            None
        )));
        assert!(invalid(PixelWriter::begin(
            Vec::new(),
            &header(1, 1, 5, 0),
            None
        )));
        assert!(invalid(PixelWriter::begin(
            Vec::new(),
            &header(1, 1, 3, 2),
            None
        )));
        assert!(invalid(PixelWriter::begin(
            Vec::new(),
            &header(1, 1, 3, 0),
            Some(b"ab")
        )));
        assert!(invalid(PixelWriter::begin(
            Vec::new(),
            &header(u32::MAX, u32::MAX, 4, 0),
            None
        )));
        // An empty profile is no profile.
        let bytes = PixelWriter::begin(Vec::new(), &header(1, 1, 3, 0), Some(&[]))
            .and_then(|mut w| w.row(&[1, 2, 3]).map(|()| w))
            .and_then(PixelWriter::finish)
            .unwrap();
        assert_eq!(read(&bytes, &ANY).unwrap().icc, None);
    }

    #[test]
    fn the_writer_checks_its_rows() {
        let mut writer = PixelWriter::begin(Vec::new(), &header(2, 2, 3, 0), None).unwrap();
        let wrong =
            |r: io::Result<()>| matches!(r, Err(e) if e.kind() == io::ErrorKind::InvalidInput);
        assert!(wrong(writer.row(&[0; 5])));
        assert!(wrong(writer.row(&[0; 7])));
        assert!(wrong(writer.row(&[0; 8])));
        writer.row(&[0; 6]).unwrap();
        // One row short.
        let short = writer.finish();
        assert!(matches!(short, Err(e) if e.kind() == io::ErrorKind::InvalidData));

        let mut writer = PixelWriter::begin(Vec::new(), &header(2, 2, 4, 0), None).unwrap();
        writer.row(&[0; 8]).unwrap();
        writer.row(&[0; 8]).unwrap();
        assert!(wrong(writer.row(&[0; 8])));
        assert_eq!(writer.finish().unwrap().len(), 24 + 16);
    }
}
