//! Remove Metadata on WebP (D26): the image chunks are copied as they
//! are, padding included; the rest are dropped and said what they were.
//!
//! - **Kept:** `VP8 `, `VP8L`, `VP8X`, `ALPH`, `ANIM`, `ANMF` and `ICCP`.
//! - **Dropped:** `EXIF` ([`exif::walk`], after an optional `Exif\0\0`),
//!   `XMP `, unknown chunks, and anything after the RIFF's end.
//! - **Patched afterwards:** the RIFF size, and VP8X's flags: XMP off, and
//!   EXIF on only when the minimal EXIF ([`exif::minimal`]) went back where
//!   the old one was. Without a VP8X (a simple WebP) there's no flag to
//!   set, so none goes back.

use std::io::{BufRead, SeekFrom};

use super::exif;
use super::strip::{read_exact, Error, Found, Out};
use crate::MetaField;

/// The most chunks in a file (an animation has one per frame).
const MAX_CHUNKS: usize = 1 << 20;

/// The most bytes of an EXIF chunk read; a bigger one is reported as
/// Other.
const MAX_EXIF: usize = 1 << 20;

/// VP8X's flag for an XMP chunk.
const XMP_FLAG: u8 = 0x04;

/// VP8X's flag for an EXIF chunk.
const EXIF_FLAG: u8 = 0x08;

pub(super) fn sniff(head: &[u8]) -> bool {
    head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP"
}

/// Strips the WebP `r` into `w` ([module docs](self)).
pub(super) fn strip(r: &mut dyn BufRead, w: &mut dyn Out) -> Result<Found, Error> {
    let mut found = Found::default();
    let mut header = [0; 12];
    read_exact(r, &mut header)?;
    if !sniff(&header) {
        return Err(Error::Broken("no RIFF WEBP header".into()));
    }
    let riff = u64::from(u32::from_le_bytes([
        header[4], header[5], header[6], header[7],
    ]));
    // What's left of the RIFF after "WEBP".
    let mut left = riff
        .checked_sub(4)
        .ok_or_else(|| Error::Broken("a RIFF too small".into()))?;
    let mut written = 0u64;
    write(w, &header, &mut written)?;
    let mut flags_at = None;
    let mut flags = 0u8;
    let mut wrote_exif = false;
    let mut seen_exif = false;
    let mut chunks = 0;
    let mut image = false;
    while left > 0 {
        let mut head = [0; 8];
        read_exact(r, &mut head)?;
        let fourcc = [head[0], head[1], head[2], head[3]];
        let size = u64::from(u32::from_le_bytes([head[4], head[5], head[6], head[7]]));
        let padded = size + (size & 1);
        // The last chunk's pad byte may be missing.
        let body = if left >= 8 + padded {
            padded
        } else if left == 8 + size {
            size
        } else {
            return Err(Error::Broken(format!(
                "a chunk of {size} bytes past the RIFF's end"
            )));
        };
        left -= 8 + body;
        chunks += 1;
        if chunks > MAX_CHUNKS {
            return Err(Error::Broken("too many chunks".into()));
        }
        match &fourcc {
            b"VP8X" => {
                if size < 10 {
                    return Err(Error::Broken("a short VP8X".into()));
                }
                write(w, &head, &mut written)?;
                flags_at = Some(written);
                let mut data = vec![0; usize_of(body)?];
                read_exact(r, &mut data)?;
                flags = data[0];
                write(w, &data, &mut written)?;
            }
            b"VP8 " | b"VP8L" | b"ALPH" | b"ANIM" | b"ANMF" | b"ICCP" => {
                image |= matches!(&fourcc, b"VP8 " | b"VP8L" | b"ANMF");
                write(w, &head, &mut written)?;
                copy(r, w, body, &mut written)?;
            }
            b"EXIF" => {
                let take = usize_of(size)?.min(MAX_EXIF);
                let mut data = vec![0; take];
                read_exact(r, &mut data)?;
                skip(r, body - take as u64)?;
                if take as u64 == size {
                    let tiff = data.strip_prefix(b"Exif\0\0").unwrap_or(&data);
                    let walked = exif::walk(tiff);
                    found.fields.extend(walked.fields.iter().copied());
                    if !seen_exif && flags_at.is_some() {
                        let needs = exif::minimal(
                            walked.orientation,
                            walked.dcf_adobe_rgb() && flags & 0x20 == 0,
                        );
                        if let Some(tiff) = needs {
                            write_chunk(w, b"EXIF", &tiff, &mut written)?;
                            wrote_exif = true;
                        }
                    }
                } else {
                    found.fields.insert(MetaField::Other);
                }
                seen_exif = true;
            }
            b"XMP " => {
                skip(r, body)?;
                found.fields.insert(MetaField::Xmp);
            }
            _ => {
                skip(r, body)?;
                found.fields.insert(MetaField::Other);
            }
        }
    }
    if !image {
        return Err(Error::Broken("no image data".into()));
    }
    let rest = r.fill_buf().map_err(Error::Read)?;
    if rest.iter().any(|&b| b != 0) {
        found.fields.insert(MetaField::Other);
    }
    // The RIFF's size, and VP8X's flags.
    let size = u32::try_from(written - 8).map_err(|_| Error::Broken("too big".into()))?;
    seek_write(w, 4, &size.to_le_bytes())?;
    if let Some(at) = flags_at {
        let mut new = flags & !XMP_FLAG & !EXIF_FLAG;
        if wrote_exif {
            new |= EXIF_FLAG;
        }
        seek_write(w, at, &[new])?;
    }
    w.seek(SeekFrom::End(0)).map_err(Error::Write)?;
    Ok(found)
}

fn usize_of(n: u64) -> Result<usize, Error> {
    usize::try_from(n).map_err(|_| Error::Broken("a chunk too big".into()))
}

fn write(w: &mut dyn Out, bytes: &[u8], written: &mut u64) -> Result<(), Error> {
    w.write_all(bytes).map_err(Error::Write)?;
    *written += bytes.len() as u64;
    Ok(())
}

/// Writes a chunk of `fourcc` and `data`, padded to even.
fn write_chunk(
    w: &mut dyn Out,
    fourcc: &[u8; 4],
    data: &[u8],
    written: &mut u64,
) -> Result<(), Error> {
    let size = u32::try_from(data.len()).map_err(|_| Error::Broken("chunk too big".into()))?;
    write(w, fourcc, written)?;
    write(w, &size.to_le_bytes(), written)?;
    write(w, data, written)?;
    if data.len() % 2 == 1 {
        write(w, &[0], written)?;
    }
    Ok(())
}

fn seek_write(w: &mut dyn Out, at: u64, bytes: &[u8]) -> Result<(), Error> {
    w.seek(SeekFrom::Start(at)).map_err(Error::Write)?;
    w.write_all(bytes).map_err(Error::Write)
}

fn copy(r: &mut dyn BufRead, w: &mut dyn Out, mut n: u64, written: &mut u64) -> Result<(), Error> {
    while n > 0 {
        let buf = r.fill_buf().map_err(Error::Read)?;
        if buf.is_empty() {
            return Err(Error::Broken("the file ends inside a chunk".into()));
        }
        let take = buf.len().min(usize::try_from(n).unwrap_or(usize::MAX));
        write(w, &buf[..take], written)?;
        r.consume(take);
        n -= take as u64;
    }
    Ok(())
}

fn skip(r: &mut dyn BufRead, mut n: u64) -> Result<(), Error> {
    while n > 0 {
        let buf = r.fill_buf().map_err(Error::Read)?;
        if buf.is_empty() {
            return Err(Error::Broken("the file ends inside a chunk".into()));
        }
        let take = buf.len().min(usize::try_from(n).unwrap_or(usize::MAX));
        r.consume(take);
        n -= take as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeSet;
    use std::io::Cursor;

    use super::*;
    use crate::imagetool::exif::tests::{tiff, V};

    fn chunk(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut n = 0;
        write_chunk(&mut Cursor::new(&mut out), fourcc, data, &mut n).unwrap();
        out
    }

    fn vp8x(flags: u8) -> Vec<u8> {
        chunk(b"VP8X", &[flags, 0, 0, 0, 1, 0, 0, 1, 0, 0])
    }

    fn webp(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend(body);
        out
    }

    fn run(input: &[u8]) -> Result<(Vec<u8>, BTreeSet<MetaField>), Error> {
        let mut out = Cursor::new(Vec::new());
        let found = strip(&mut Cursor::new(input), &mut out)?;
        Ok((out.into_inner(), found.fields))
    }

    #[test]
    fn a_plain_webp_comes_out_byte_for_byte() {
        for src in [
            webp(&[chunk(b"VP8 ", b"frame")]),
            webp(&[
                vp8x(0x10),
                chunk(b"ALPH", b"al"),
                chunk(b"VP8L", b"lossless"),
            ]),
            webp(&[
                vp8x(0x22),
                chunk(b"ICCP", b"prof"),
                chunk(b"ANIM", &[0; 6]),
                chunk(b"ANMF", b"f1"),
            ]),
        ] {
            let (out, fields) = run(&src).unwrap();
            assert_eq!(out, src);
            assert!(fields.is_empty());
        }
    }

    #[test]
    fn exif_xmp_and_unknown_chunks_go_and_the_flags_follow() {
        use MetaField::*;
        let camera = tiff(
            b'I',
            &[&[(0x010F, V::Ascii("Nikon")), (0x0112, V::Short(3))]],
            None,
        );
        let src = webp(&[
            vp8x(0x0C),
            chunk(b"VP8 ", b"frame"),
            chunk(b"EXIF", &[b"Exif\0\0".as_slice(), &camera].concat()),
            chunk(b"XMP ", b"<x:xmpmeta/>"),
            chunk(b"JUNK", b"x"),
        ]);
        let (out, fields) = run(&[src, b"after".to_vec()].concat()).unwrap();
        assert_eq!(fields, [Camera, Xmp, Other].into());
        let minimal = exif::minimal(Some(3), false).unwrap();
        assert_eq!(
            out,
            webp(&[
                vp8x(0x08),
                chunk(b"VP8 ", b"frame"),
                chunk(b"EXIF", &minimal)
            ])
        );
        // Upright: the EXIF flag goes too.
        let t = tiff(b'I', &[&[(0x010F, V::Ascii("Nikon"))]], None);
        let src = webp(&[vp8x(0x08), chunk(b"VP8 ", b"frame"), chunk(b"EXIF", &t)]);
        let (out, _) = run(&src).unwrap();
        assert_eq!(out, webp(&[vp8x(0x00), chunk(b"VP8 ", b"frame")]));
    }

    #[test]
    fn broken_webps_are_refused() {
        let good = webp(&[vp8x(0), chunk(b"VP8 ", b"frame!")]);
        for n in 0..good.len() {
            assert!(run(&good[..n]).is_err(), "cut at {n}");
        }
        // The last pad byte missing is fine.
        let odd = webp(&[chunk(b"VP8 ", b"odd")]);
        let mut short = odd[..odd.len() - 1].to_vec();
        let riff = u32::try_from(short.len() - 8).unwrap().to_le_bytes();
        short[4..8].copy_from_slice(&riff);
        assert!(run(&short).is_ok());
        for bad in [
            webp(&[chunk(b"XMP ", b"no image")]),
            webp(&[chunk(b"VP8X", &[0; 4]), chunk(b"VP8 ", b"f")]),
            b"RIFF\x02\0\0\0WEBP".to_vec(),
        ] {
            assert!(matches!(run(&bad), Err(Error::Broken(_))), "{bad:?}");
        }
    }
}
