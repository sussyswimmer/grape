//! Gzip input: every member in turn, with zero padding at the end allowed.
//!
//! `flate2`'s `MultiGzDecoder` refuses trailing zero bytes ("invalid gzip
//! header"), which some tools and tape blocking leave after the last
//! member, so Grape loops over the members itself with `bufread::GzDecoder`.
//! Each member's CRC-32 and length are checked as it ends.

use std::io::{self, BufRead, Read};

use flate2::bufread::GzDecoder;

/// What the first member's header says, for a plain (not tar) `.gz`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GzInfo {
    /// FNAME: the original file name, raw bytes (Latin-1 per RFC 1952).
    pub(crate) name: Option<Vec<u8>>,
    /// MTIME in Unix seconds; 0 means none.
    pub(crate) mtime: u32,
}

/// A gzip file's decompressed data, all members in a row.
pub(crate) struct GzMembers<R: BufRead> {
    current: Option<GzDecoder<R>>,
}

impl<R: BufRead> GzMembers<R> {
    /// Starts on the first member; also returns its header, if it parsed
    /// (if not, the first read fails).
    pub(crate) fn new(inner: R) -> (GzMembers<R>, Option<GzInfo>) {
        let decoder = GzDecoder::new(inner);
        let info = decoder.header().map(|header| GzInfo {
            name: header.filename().map(<[u8]>::to_vec),
            mtime: header.mtime(),
        });
        (
            GzMembers {
                current: Some(decoder),
            },
            info,
        )
    }
}

impl<R: BufRead> Read for GzMembers<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(decoder) = self.current.as_mut() else {
                return Ok(0);
            };
            let n = decoder.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            // The member ended and its trailer checked out.
            let Some(decoder) = self.current.take() else {
                return Ok(0);
            };
            let mut inner = decoder.into_inner();
            if another_member(&mut inner)? {
                self.current = Some(GzDecoder::new(inner));
            }
        }
    }
}

/// After a member: true if another one starts, false at the end of the
/// file or of zero padding, an error for anything else.
fn another_member<R: BufRead>(inner: &mut R) -> io::Result<bool> {
    match inner.fill_buf()?.first() {
        None => Ok(false),
        Some(0x1F) => Ok(true),
        Some(0) => loop {
            let buf = inner.fill_buf()?;
            if buf.is_empty() {
                return Ok(false);
            }
            if buf.iter().any(|&b| b != 0) {
                return Err(trailing());
            }
            let n = buf.len();
            inner.consume(n);
        },
        Some(_) => Err(trailing()),
    }
}

fn trailing() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "data after the last gzip member",
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Write;

    use flate2::{write::GzEncoder, Compression, GzBuilder};

    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn decode(bytes: &[u8]) -> io::Result<Vec<u8>> {
        let (mut members, _) = GzMembers::new(bytes);
        let mut out = Vec::new();
        members.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn reads_every_member_and_zero_padding() {
        let mut bytes = gzip(b"part one\n");
        bytes.extend(gzip(b"part two\n"));
        assert_eq!(decode(&bytes).unwrap(), b"part one\npart two\n");
        bytes.extend([0u8; 512]);
        assert_eq!(decode(&bytes).unwrap(), b"part one\npart two\n");
        // An empty member in the middle.
        let mut bytes = gzip(b"a");
        bytes.extend(gzip(b""));
        bytes.extend(gzip(b"b"));
        assert_eq!(decode(&bytes).unwrap(), b"ab");
    }

    #[test]
    fn refuses_garbage_bad_checksums_and_truncation() {
        let mut garbage = gzip(b"data");
        garbage.extend(b"garbage");
        assert!(decode(&garbage).is_err());
        let mut zeros_then_data = gzip(b"data");
        zeros_then_data.extend([0, 0, 1]);
        assert!(decode(&zeros_then_data).is_err());
        let mut bad_crc = gzip(b"some data to check");
        let at = bad_crc.len() - 6;
        bad_crc[at] ^= 0xFF;
        assert!(decode(&bad_crc).is_err());
        let whole = gzip(b"truncated data");
        assert!(decode(&whole[..whole.len() - 3]).is_err());
        assert!(decode(b"\x1f\x8bnot really").is_err());
    }

    #[test]
    fn keeps_the_first_header() {
        let mut encoder = GzBuilder::new()
            .filename("notes.txt")
            .mtime(1_700_000_000)
            .write(Vec::new(), Compression::default());
        encoder.write_all(b"x").unwrap();
        let bytes = encoder.finish().unwrap();
        let (_, info) = GzMembers::new(&bytes[..]);
        assert_eq!(
            info,
            Some(GzInfo {
                name: Some(b"notes.txt".to_vec()),
                mtime: 1_700_000_000
            })
        );
        let (_, info) = GzMembers::new(&gzip(b"x")[..]);
        assert_eq!(info.unwrap().name, None);
    }
}
