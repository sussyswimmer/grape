//! Remove Metadata on GIF (D26): the images and what draws them are
//! copied as they are; the other extensions are dropped and said what
//! they were. (FFmpeg's `-c copy` would keep comments and XMP.)
//!
//! - **Kept:** the header, the logical screen and global palette; each
//!   image with its local palette and LZW data; graphic control (`F9`)
//!   and plain text (`01`) extensions; the first `NETSCAPE2.0` or
//!   `ANIMEXTS1.0` loop extension; an `ICCRGBG1012` profile.
//! - **Dropped:** comments (`FE`), `XMP DataXMP` (its "magic trailer" is
//!   ordinary sub-block bytes, so skipping sub-blocks is exact), Photoshop
//!   IPTC (`MGK8BIM0000`, `MGKIPTC0000`), other application and unknown
//!   extensions, later copies of the loop extension (FFmpeg writes those;
//!   unreported), and anything after the trailer.

use std::io::{BufRead, Write};

use super::strip::{more_after, read_exact, read_u8, Error, Found};
use crate::MetaField;

/// The most blocks (images and extensions) in a file.
const MAX_BLOCKS: usize = 1 << 22;

pub(super) fn sniff(head: &[u8]) -> bool {
    head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a")
}

/// Strips the GIF `r` into `w` ([module docs](self)).
pub(super) fn strip(r: &mut dyn BufRead, w: &mut dyn Write) -> Result<Found, Error> {
    let mut found = Found::default();
    let mut header = [0; 13];
    read_exact(r, &mut header)?;
    if !sniff(&header) {
        return Err(Error::Broken("no GIF header".into()));
    }
    write(w, &header)?;
    if header[10] & 0x80 != 0 {
        copy(r, w, palette_size(header[10]))?;
    }
    let mut looped = false;
    let mut images = 0;
    let mut blocks = 0;
    loop {
        blocks += 1;
        if blocks > MAX_BLOCKS {
            return Err(Error::Broken("too many blocks".into()));
        }
        match read_u8(r)? {
            0x2C => {
                let mut descriptor = [0; 9];
                read_exact(r, &mut descriptor)?;
                write(w, &[0x2C])?;
                write(w, &descriptor)?;
                if descriptor[8] & 0x80 != 0 {
                    copy(r, w, palette_size(descriptor[8]))?;
                }
                // The LZW minimum code size, then the data.
                copy(r, w, 1)?;
                copy_sub_blocks(r, w)?;
                images += 1;
            }
            0x21 => {
                let label = read_u8(r)?;
                match label {
                    0xF9 | 0x01 => {
                        write(w, &[0x21, label])?;
                        copy_sub_blocks(r, w)?;
                    }
                    0xFE => {
                        if !skip_sub_blocks(r)? {
                            found.fields.insert(MetaField::Comment);
                        }
                    }
                    0xFF => {
                        let first = read_sub_block(r)?;
                        if first.is_empty() {
                            // `21 FF 00`: an empty extension, already ended.
                            found.fields.insert(MetaField::Other);
                            continue;
                        }
                        let id = first.get(..11).unwrap_or_default();
                        let keep_loop = matches!(id, b"NETSCAPE2.0" | b"ANIMEXTS1.0");
                        if (keep_loop && !looped) || id == b"ICCRGBG1012" {
                            looped |= keep_loop;
                            write(w, &[0x21, 0xFF, u8::try_from(first.len()).unwrap_or(0)])?;
                            write(w, &first)?;
                            copy_sub_blocks(r, w)?;
                        } else {
                            skip_sub_blocks(r)?;
                            match id {
                                b"NETSCAPE2.0" | b"ANIMEXTS1.0" => {}
                                b"XMP DataXMP" => {
                                    found.fields.insert(MetaField::Xmp);
                                }
                                b"MGK8BIM0000" | b"MGKIPTC0000" => {
                                    found.fields.insert(MetaField::Iptc);
                                }
                                _ => {
                                    found.fields.insert(MetaField::Other);
                                }
                            }
                        }
                    }
                    _ => {
                        skip_sub_blocks(r)?;
                        found.fields.insert(MetaField::Other);
                    }
                }
            }
            0x3B => {
                if images == 0 {
                    return Err(Error::Broken("no image".into()));
                }
                write(w, &[0x3B])?;
                break;
            }
            other => return Err(Error::Broken(format!("an unknown block {other:#04X}"))),
        }
    }
    if more_after(r, &[0])? {
        found.fields.insert(MetaField::Other);
    }
    Ok(found)
}

/// The size of a palette from a packed field's low 3 bits.
fn palette_size(packed: u8) -> usize {
    3 << ((packed & 7) + 1)
}

fn write(w: &mut dyn Write, bytes: &[u8]) -> Result<(), Error> {
    w.write_all(bytes).map_err(Error::Write)
}

fn copy(r: &mut dyn BufRead, w: &mut dyn Write, n: usize) -> Result<(), Error> {
    let mut buf = vec![0; n];
    read_exact(r, &mut buf)?;
    write(w, &buf)
}

/// One sub-block's data; empty for the terminator.
fn read_sub_block(r: &mut dyn BufRead) -> Result<Vec<u8>, Error> {
    let len = read_u8(r)?;
    let mut data = vec![0; usize::from(len)];
    read_exact(r, &mut data)?;
    Ok(data)
}

/// Copies sub-blocks up to and including the terminator.
fn copy_sub_blocks(r: &mut dyn BufRead, w: &mut dyn Write) -> Result<(), Error> {
    loop {
        let data = read_sub_block(r)?;
        write(w, &[u8::try_from(data.len()).unwrap_or(0)])?;
        if data.is_empty() {
            return Ok(());
        }
        write(w, &data)?;
    }
}

/// Skips sub-blocks up to and including the terminator; whether they
/// held only NULs and spaces.
fn skip_sub_blocks(r: &mut dyn BufRead) -> Result<bool, Error> {
    let mut blank = true;
    loop {
        let data = read_sub_block(r)?;
        if data.is_empty() {
            return Ok(blank);
        }
        blank &= data.iter().all(|&b| b == 0 || b.is_ascii_whitespace());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeSet;
    use std::io::Cursor;

    use super::*;

    /// A 1×1 GIF89a with a 2-color global palette and `blocks` before
    /// its image.
    fn gif(blocks: &[Vec<u8>], after_image: &[Vec<u8>]) -> Vec<u8> {
        let mut g = b"GIF89a\x01\x00\x01\x00\x80\x00\x00".to_vec();
        g.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
        for b in blocks {
            g.extend_from_slice(b);
        }
        g.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 0x01, 0]);
        for b in after_image {
            g.extend_from_slice(b);
        }
        g.push(0x3B);
        g
    }

    /// An extension of `label` with these sub-blocks.
    fn ext(label: u8, blocks: &[&[u8]]) -> Vec<u8> {
        let mut e = vec![0x21, label];
        for b in blocks {
            e.push(u8::try_from(b.len()).unwrap());
            e.extend_from_slice(b);
        }
        e.push(0);
        e
    }

    fn run(input: &[u8]) -> Result<(Vec<u8>, BTreeSet<MetaField>), Error> {
        let mut out = Vec::new();
        let found = strip(&mut Cursor::new(input), &mut out)?;
        Ok((out, found.fields))
    }

    #[test]
    fn a_plain_animation_comes_out_byte_for_byte() {
        let looping = ext(0xFF, &[b"NETSCAPE2.0", &[1, 0, 0]]);
        let gce = ext(0xF9, &[&[4, 10, 0, 0]]);
        let icc = ext(0xFF, &[b"ICCRGBG1012", b"profile"]);
        let src = gif(&[looping, icc, gce.clone()], &[gce]);
        let (out, fields) = run(&src).unwrap();
        assert_eq!(out, src);
        assert!(fields.is_empty());
    }

    #[test]
    fn comments_xmp_and_unknown_extensions_go() {
        use MetaField::*;
        let looping = ext(0xFF, &[b"NETSCAPE2.0", &[1, 0, 0]]);
        // XMP's magic trailer: bytes 0xFF down to 0x00, then the
        // terminator, which as sub-blocks is exact.
        let mut trailer = vec![1u8];
        trailer.extend((0..=255u8).rev());
        let mut xmp = vec![0x21, 0xFF, 11];
        xmp.extend_from_slice(b"XMP DataXMP");
        xmp.extend_from_slice(b"<x:xmpmeta/>");
        xmp.extend_from_slice(&trailer[1..]);
        xmp.push(0);
        let src = gif(
            &[
                looping.clone(),
                ext(0xFE, &[b"made by me"]),
                xmp,
                ext(0xFF, &[b"MGK8BIM0000", b"iptc"]),
                ext(0xFF, &[b"SOMEAPP1.00", b"x"]),
                ext(0x99, &[b"what"]),
                ext(0xFE, &[b"   "]),
            ],
            std::slice::from_ref(&looping),
        );
        let (out, fields) = run(&[src, b"tail".to_vec()].concat()).unwrap();
        assert_eq!(fields, [Comment, Xmp, Iptc, Other].into());
        assert_eq!(out, gif(&[looping], &[]));
    }

    #[test]
    fn an_empty_application_extension_ends_where_it_starts() {
        // `21 FF 00`, then a comment whose text looks like a plain-text
        // extension, then the image: the comment goes whole, the image
        // stays.
        let tricky = ext(0xFE, &[b"\x21\x01\x05SECRET\x00"]);
        let src = gif(&[vec![0x21, 0xFF, 0x00], tricky], &[]);
        let (out, fields) = run(&src).unwrap();
        assert_eq!(fields, [MetaField::Comment, MetaField::Other].into());
        assert_eq!(out, gif(&[], &[]));
    }

    #[test]
    fn data_after_the_trailer_is_found_past_the_read_buffer() {
        let mut input = gif(&[], &[]);
        input.extend(vec![0; 70_000]);
        input.extend_from_slice(b"zip");
        let mut out = Vec::new();
        let found = strip(
            &mut std::io::BufReader::with_capacity(8, Cursor::new(input)),
            &mut out,
        )
        .unwrap();
        assert_eq!(found.fields, [MetaField::Other].into());
    }

    #[test]
    fn broken_gifs_are_refused() {
        let good = gif(&[], &[]);
        for n in 0..good.len() {
            assert!(run(&good[..n]).is_err(), "cut at {n}");
        }
        let mut stray = gif(&[], &[]);
        let at = stray.len() - 1;
        stray.insert(at, 0x42);
        let empty = b"GIF89a\x01\x00\x01\x00\x00\x00\x00\x3B".to_vec();
        for bad in [stray, empty] {
            assert!(matches!(run(&bad), Err(Error::Broken(_))), "{bad:?}");
        }
    }
}
