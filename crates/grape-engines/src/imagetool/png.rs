//! Remove Metadata on PNG (D26): chunks the picture needs are copied as
//! they are, CRCs included; the rest are dropped and said what they were.
//!
//! - **Kept:** IHDR, PLTE, IDAT, IEND, tRNS, cHRM, gAMA, iCCP, sBIT, sRGB,
//!   cICP, mDCV, cLLI, bKGD, pHYs, hIST, sPLT, oFFs, sCAL, APNG's acTL,
//!   fcTL and fdAT (it stays animated), and any unknown critical chunk.
//! - **Dropped:** tEXt, zTXt and iTXt (by keyword: [`text_field`]),
//!   `eXIf` ([`exif::walk`]), tIME, other ancillary chunks, and anything
//!   after IEND. Apple's `iDOT` goes unreported: it's a decoding hint
//!   whose offsets the strip makes wrong.
//! - **Written back:** the minimal EXIF as `eXIf` where the first EXIF
//!   was, when that was before the image data and the picture needs it.

use std::io::{BufRead, Read, Write};

use super::exif;
use super::strip::{more_after, read_exact, Error, Found};
use crate::MetaField;

/// A PNG's signature.
const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// The most chunks in a file (a big PNG has an IDAT every 8 KiB or so).
const MAX_CHUNKS: usize = 1 << 20;

/// The most bytes of kept chunks held before the image data.
const MAX_HELD: usize = 64 << 20;

/// The most bytes of a dropped chunk read to classify it, decompressed
/// text included: of a bigger EXIF, only this much is walked (its first
/// IFD is at the start), and the rest is reported as Other.
const MAX_READ: usize = 1 << 20;

/// How much compressed text is inflated at a time to see whether it's
/// blank.
const INFLATE_STEP: usize = 4096;

pub(super) fn sniff(head: &[u8]) -> bool {
    head.starts_with(SIGNATURE)
}

/// The chunks a PNG decoder may need, kept by type.
const KEPT: [&[u8; 4]; 21] = [
    b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"cHRM", b"gAMA", b"iCCP", b"sBIT", b"sRGB",
    b"cICP", b"mDCV", b"cLLI", b"bKGD", b"pHYs", b"hIST", b"sPLT", b"oFFs", b"sCAL", b"acTL",
    b"fcTL",
];

/// Whether a chunk of type `kind` is kept: a listed one, APNG's `fdAT`,
/// or an unknown critical one (an uppercase first letter).
fn kept(kind: &[u8; 4]) -> bool {
    KEPT.contains(&kind) || kind == b"fdAT" || kind[0].is_ascii_uppercase()
}

/// A part of the output before the image data.
enum Part {
    Bytes(Vec<u8>),
    ExifSlot,
}

/// Strips the PNG `r` into `w` ([module docs](self)).
pub(super) fn strip(r: &mut dyn BufRead, w: &mut dyn Write) -> Result<Found, Error> {
    let mut found = Found::default();
    let mut signature = [0; 8];
    read_exact(r, &mut signature)?;
    if &signature != SIGNATURE {
        return Err(Error::Broken("no PNG signature".into()));
    }
    write(w, SIGNATURE)?;
    let mut held: Option<Vec<Part>> = Some(Vec::new());
    let mut held_bytes = 0;
    let mut first_exif: Option<exif::Exif> = None;
    let mut color_chunk = false;
    let mut chunks = 0;
    loop {
        let mut header = [0; 8];
        read_exact(r, &mut header)?;
        let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let kind = [header[4], header[5], header[6], header[7]];
        if len > i32::MAX as u32 || !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(Error::Broken(format!("a bad chunk header {header:?}")));
        }
        chunks += 1;
        if chunks > MAX_CHUNKS {
            return Err(Error::Broken("too many chunks".into()));
        }
        let len = len as usize;
        if chunks == 1 && &kind != b"IHDR" {
            return Err(Error::Broken("no IHDR first".into()));
        }
        if kept(&kind) {
            color_chunk |= &kind == b"iCCP" || &kind == b"sRGB";
            let image_data = &kind == b"IDAT" || &kind == b"IEND";
            if let Some(parts) = held.as_mut().filter(|_| !image_data) {
                held_bytes += len + 12;
                if held_bytes > MAX_HELD {
                    return Err(Error::Broken("too much before the image data".into()));
                }
                let mut chunk = header.to_vec();
                chunk.resize(12 + len, 0);
                read_exact(r, &mut chunk[8..])?;
                parts.push(Part::Bytes(chunk));
                continue;
            }
            if let Some(parts) = held.take() {
                if &kind == b"IEND" {
                    return Err(Error::Broken("no image data".into()));
                }
                let needs = first_exif
                    .as_ref()
                    .and_then(|e| exif::minimal(e.orientation, e.dcf_adobe_rgb() && !color_chunk));
                for part in parts {
                    match part {
                        Part::Bytes(bytes) => write(w, &bytes)?,
                        Part::ExifSlot => {
                            if let Some(tiff) = &needs {
                                write_chunk(w, b"eXIf", tiff)?;
                            }
                        }
                    }
                }
            }
            write(w, &header)?;
            copy(r, w, len + 4)?;
            if &kind == b"IEND" {
                break;
            }
            continue;
        }
        // Dropped: read what's needed to say what it was.
        let take = len.min(MAX_READ);
        let mut data = vec![0; take];
        read_exact(r, &mut data)?;
        skip(r, len - take + 4)?;
        let whole = take == len;
        match &kind {
            b"tEXt" | b"zTXt" | b"iTXt" => {
                let (field, exif_blob) = text(&kind, &data, whole);
                found.fields.extend(field);
                if let Some(blob) = exif_blob {
                    note_exif(&mut found, &mut first_exif, held.as_mut(), &blob);
                }
            }
            b"eXIf" => {
                if !whole {
                    found.fields.insert(MetaField::Other);
                }
                note_exif(&mut found, &mut first_exif, held.as_mut(), &data);
            }
            b"tIME" => {
                found.fields.insert(MetaField::ModDate);
            }
            b"iDOT" => {}
            _ => {
                found.fields.insert(MetaField::Other);
            }
        }
    }
    // Anything but padding after IEND.
    if more_after(r, &[0])? {
        found.fields.insert(MetaField::Other);
    }
    Ok(found)
}

/// Walks an EXIF blob (a TIFF, or one behind `Exif\0\0`), reports its
/// fields and, for the first one before the image data, marks its place.
fn note_exif(
    found: &mut Found,
    first: &mut Option<exif::Exif>,
    held: Option<&mut Vec<Part>>,
    blob: &[u8],
) {
    let tiff = blob.strip_prefix(b"Exif\0\0").unwrap_or(blob);
    let walked = exif::walk(tiff);
    found.fields.extend(walked.fields.iter().copied());
    if first.is_none() {
        if let Some(parts) = held {
            parts.push(Part::ExifSlot);
            *first = Some(walked);
        }
    }
}

/// What a text chunk of `kind` holding `data` (all of it if `whole`)
/// says: its field, if its text isn't empty, and an EXIF blob for
/// ImageMagick's "Raw profile type exif" (or "APP1").
fn text(kind: &[u8; 4], data: &[u8], whole: bool) -> (Option<MetaField>, Option<Vec<u8>>) {
    let Some(nul) = data.iter().take(80).position(|&b| b == 0) else {
        return (Some(MetaField::Other), None);
    };
    let keyword = &data[..nul];
    let rest = &data[nul + 1..];
    // The text, and whether it's compressed.
    let body = match kind {
        b"tEXt" => Some((false, rest)),
        b"zTXt" => rest.get(1..).map(|z| (true, z)),
        _ => itxt(rest),
    };
    let field = text_field(keyword);
    if matches!(keyword, b"Raw profile type exif" | b"Raw profile type APP1") {
        let body = body.and_then(|(compressed, text)| {
            if compressed {
                inflate(text, whole)
            } else {
                Some(text.to_vec())
            }
        });
        return match body.as_deref().and_then(unhex_profile) {
            Some(blob) => (None, Some(blob)),
            None => (Some(MetaField::Other), None),
        };
    }
    let blank = body.and_then(|(compressed, text)| {
        if compressed {
            inflates_blank(text, whole)
        } else {
            Some(blank(text))
        }
    });
    match blank {
        // Blank, or unreadable: reported by its keyword.
        Some(true) => (None, None),
        Some(false) | None => (Some(field), None),
    }
}

/// The text of an iTXt chunk after its keyword, and whether it's
/// compressed.
fn itxt(rest: &[u8]) -> Option<(bool, &[u8])> {
    let (&compressed, rest) = rest.split_first()?;
    let rest = rest.get(1..)?;
    let language = rest.iter().position(|&b| b == 0)?;
    let rest = &rest[language + 1..];
    let translated = rest.iter().position(|&b| b == 0)?;
    Some((compressed == 1, &rest[translated + 1..]))
}

/// Whether text is only NULs and white space.
fn blank(text: &[u8]) -> bool {
    text.iter().all(|&b| b == 0 || b.is_ascii_whitespace())
}

/// Whether zlib `data` inflates to blank text ([`blank`]), inflating only
/// as far as the first other byte and at most [`MAX_READ`]; `None` if it's
/// broken (or cut short before any text, unless `whole`).
fn inflates_blank(data: &[u8], whole: bool) -> Option<bool> {
    let mut reader = flate2::read::ZlibDecoder::new(data).take(MAX_READ as u64);
    let mut buf = [0; INFLATE_STEP];
    let mut read_any = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Some(true),
            Ok(n) if !blank(&buf[..n]) => return Some(false),
            Ok(_) => read_any = true,
            Err(_) if !whole && read_any => return Some(true),
            Err(_) => return None,
        }
    }
}

/// Inflates zlib data to at most [`MAX_READ`] bytes; `None` if it's
/// broken (or cut short, unless `whole`).
fn inflate(data: &[u8], whole: bool) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let reader = flate2::read::ZlibDecoder::new(data);
    match reader.take(MAX_READ as u64).read_to_end(&mut out) {
        Ok(_) => Some(out),
        Err(_) if !whole && !out.is_empty() => Some(out),
        Err(_) => None,
    }
}

/// The bytes of an ImageMagick raw profile: a newline, the profile's
/// name, its length, then the bytes in hex over many lines.
fn unhex_profile(body: &[u8]) -> Option<Vec<u8>> {
    let mut lines = body.splitn(4, |&b| b == b'\n');
    let _empty = lines.next()?;
    let _name = lines.next()?;
    let _length = lines.next()?;
    let hex = lines.next()?;
    let digits: Vec<u8> = hex
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}

/// The field a PNG text keyword names (D26's table).
fn text_field(keyword: &[u8]) -> MetaField {
    use MetaField::*;
    match keyword {
        b"Title" => Title,
        b"Author" => Author,
        b"Description" | b"Comment" => Comment,
        b"Copyright" => Copyright,
        b"Creation Time" => CreationDate,
        b"Software" => Software,
        b"Source" => Camera,
        b"XML:com.adobe.xmp" | b"Raw profile type xmp" => Xmp,
        b"Raw profile type iptc" => Iptc,
        _ => Other,
    }
}

fn write(w: &mut dyn Write, bytes: &[u8]) -> Result<(), Error> {
    w.write_all(bytes).map_err(Error::Write)
}

/// Writes a chunk of `kind` and `data`, with its CRC.
fn write_chunk(w: &mut dyn Write, kind: &[u8; 4], data: &[u8]) -> Result<(), Error> {
    let len = u32::try_from(data.len()).map_err(|_| Error::Broken("chunk too big".into()))?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    write(w, &len.to_be_bytes())?;
    write(w, kind)?;
    write(w, data)?;
    write(w, &crc.finalize().to_be_bytes())
}

/// Copies `n` bytes from `r` to `w`.
fn copy(r: &mut dyn BufRead, w: &mut dyn Write, mut n: usize) -> Result<(), Error> {
    while n > 0 {
        let buf = r.fill_buf().map_err(Error::Read)?;
        if buf.is_empty() {
            return Err(Error::Broken("the file ends inside a chunk".into()));
        }
        let take = buf.len().min(n);
        write(w, &buf[..take])?;
        r.consume(take);
        n -= take;
    }
    Ok(())
}

/// Skips `n` bytes of `r`.
fn skip(r: &mut dyn BufRead, mut n: usize) -> Result<(), Error> {
    while n > 0 {
        let buf = r.fill_buf().map_err(Error::Read)?;
        if buf.is_empty() {
            return Err(Error::Broken("the file ends inside a chunk".into()));
        }
        let take = buf.len().min(n);
        r.consume(take);
        n -= take;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeSet;
    use std::io::Cursor;

    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    use super::*;
    use crate::imagetool::exif::tests::{tiff, V};

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_chunk(&mut out, kind, data).unwrap();
        out
    }

    fn ihdr() -> Vec<u8> {
        chunk(b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0])
    }

    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        for c in chunks {
            out.extend_from_slice(c);
        }
        out
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn run(input: &[u8]) -> Result<(Vec<u8>, BTreeSet<MetaField>), Error> {
        let mut out = Vec::new();
        let found = strip(&mut Cursor::new(input), &mut out)?;
        Ok((out, found.fields))
    }

    fn idat() -> Vec<u8> {
        chunk(b"IDAT", &zlib(&[0; 14]))
    }

    #[test]
    fn a_plain_png_comes_out_byte_for_byte() {
        let src = png(&[
            ihdr(),
            chunk(b"sRGB", &[0]),
            chunk(b"pHYs", &[0, 0, 11, 19, 0, 0, 11, 19, 1]),
            chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]),
            idat(),
            chunk(b"fdAT", &[0, 0, 0, 1, 7]),
            chunk(b"IEND", &[]),
        ]);
        let (out, fields) = run(&src).unwrap();
        assert_eq!(out, src);
        assert!(fields.is_empty());
    }

    #[test]
    fn text_exif_time_and_unknown_chunks_go_and_say_what_they_were() {
        use MetaField::*;
        let camera = tiff(b'M', &[&[(0x010F, V::Ascii("Sony"))]], None);
        let mut raw = b"\nexif\n      40\n".to_vec();
        for b in [
            b"Exif\0\0".as_slice(),
            &tiff(b'I', &[&[(0x013B, V::Ascii("Zoe"))]], None),
        ]
        .concat()
        {
            raw.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        let mut ztxt = b"Copyright\0\0".to_vec();
        ztxt.extend(zlib(b"(c) 2024"));
        let mut itxt = b"XML:com.adobe.xmp\0\x01\0en\0\0".to_vec();
        itxt.extend(zlib(b"<x:xmpmeta/>"));
        let src = png(&[
            ihdr(),
            chunk(b"tEXt", b"Title\0Holiday"),
            chunk(b"tEXt", b"Software\0   "),
            chunk(b"zTXt", &ztxt),
            chunk(b"iTXt", &itxt),
            chunk(
                b"zTXt",
                &[b"Raw profile type exif\0\0".as_slice(), &zlib(&raw)].concat(),
            ),
            chunk(b"eXIf", &camera),
            chunk(b"tIME", &[7, 232, 5, 1, 10, 20, 30]),
            chunk(b"iDOT", &[0; 28]),
            chunk(b"caBX", b"c2pa"),
            idat(),
            chunk(b"tEXt", b"Comment\0late"),
            chunk(b"IEND", &[]),
        ]);
        let (out, fields) = run(&[src, b"trailing".to_vec()].concat()).unwrap();
        let want: BTreeSet<MetaField> = [
            Camera, Author, Title, Comment, Copyright, ModDate, Xmp, Other,
        ]
        .into();
        assert_eq!(fields, want);
        assert_eq!(out, png(&[ihdr(), idat(), chunk(b"IEND", &[])]));
    }

    #[test]
    fn the_orientation_comes_back_as_exif_before_the_image_data() {
        let t = tiff(
            b'I',
            &[&[(0x0112, V::Short(8)), (0x0110, V::Ascii("Pixel"))]],
            None,
        );
        let src = png(&[ihdr(), chunk(b"eXIf", &t), idat(), chunk(b"IEND", &[])]);
        let (out, fields) = run(&src).unwrap();
        assert_eq!(fields, [MetaField::Camera].into());
        let minimal = exif::minimal(Some(8), false).unwrap();
        assert_eq!(
            out,
            png(&[
                ihdr(),
                chunk(b"eXIf", &minimal),
                idat(),
                chunk(b"IEND", &[])
            ])
        );
        // After the image data, it's dropped and not written back.
        let src = png(&[ihdr(), idat(), chunk(b"eXIf", &t), chunk(b"IEND", &[])]);
        let (out, _) = run(&src).unwrap();
        assert_eq!(out, png(&[ihdr(), idat(), chunk(b"IEND", &[])]));
    }

    #[test]
    fn broken_pngs_are_refused() {
        let good = png(&[ihdr(), idat(), chunk(b"IEND", &[])]);
        for n in 0..good.len() {
            assert!(run(&good[..n]).is_err(), "cut at {n}");
        }
        for bad in [
            png(&[idat(), chunk(b"IEND", &[])]),
            png(&[ihdr(), chunk(b"IEND", &[])]),
            png(&[ihdr(), b"\xff\xff\xff\xffIDAT".to_vec()]),
            png(&[ihdr(), b"\0\0\0\x01ID\0T\0\0\0\0\0".to_vec()]),
        ] {
            assert!(matches!(run(&bad), Err(Error::Broken(_))), "{bad:?}");
        }
    }

    #[test]
    fn a_big_exif_is_walked_as_far_as_it_was_read() {
        use MetaField::*;
        // Orientation 6 and a GPS IFD up front, then over 1 MiB of a maker
        // note: the orientation still comes back, the location is said.
        let big = vec![7; MAX_READ];
        let t = tiff(
            b'M',
            &[
                &[
                    (0x0112, V::Short(6)),
                    (0x8825, V::Ifd(1)),
                    (0x927C, V::Bytes(7, &big)),
                ],
                &[(0x0002, V::Bytes(5, &[0, 0, 0, 48, 0, 0, 0, 1]))],
            ],
            None,
        );
        assert!(t.len() > MAX_READ);
        let src = png(&[ihdr(), chunk(b"eXIf", &t), idat(), chunk(b"IEND", &[])]);
        let (out, fields) = run(&src).unwrap();
        assert!(
            fields.contains(&Location) && fields.contains(&Other),
            "{fields:?}"
        );
        let minimal = exif::minimal(Some(6), false).unwrap();
        assert_eq!(
            out,
            png(&[
                ihdr(),
                chunk(b"eXIf", &minimal),
                idat(),
                chunk(b"IEND", &[])
            ])
        );
    }

    #[test]
    fn compressed_text_is_inflated_only_until_it_isnt_blank() {
        let ztxt = |text: &[u8]| {
            let mut data = b"Comment\0\0".to_vec();
            data.extend(zlib(text));
            data
        };
        assert_eq!(text(b"zTXt", &ztxt(b"  \0 "), true), (None, None));
        assert_eq!(
            text(b"zTXt", &ztxt(b"  hi"), true),
            (Some(MetaField::Comment), None)
        );
        // Text past the first step is still found.
        let mut late = vec![b' '; 3 * INFLATE_STEP];
        late.push(b'x');
        assert_eq!(
            text(b"zTXt", &ztxt(&late), true),
            (Some(MetaField::Comment), None)
        );
        // Broken zlib: said by its keyword.
        assert_eq!(
            text(b"zTXt", b"Comment\0\0not zlib", true),
            (Some(MetaField::Comment), None)
        );
        let mut itxt = b"Title\0\x01\0en\0\0".to_vec();
        itxt.extend(zlib(b"\n\n"));
        assert_eq!(text(b"iTXt", &itxt, true), (None, None));
    }

    #[test]
    fn data_after_iend_is_found_past_the_read_buffer() {
        let src = png(&[ihdr(), idat(), chunk(b"IEND", &[])]);
        let mut after = vec![0; 70_000];
        after.extend_from_slice(b"video");
        let input = [src.clone(), after].concat();
        let mut out = Vec::new();
        let found = strip(
            &mut std::io::BufReader::with_capacity(8, Cursor::new(input)),
            &mut out,
        )
        .unwrap();
        assert_eq!(found.fields, [MetaField::Other].into());
        assert_eq!(out, src);
        // Only zeros: padding.
        let (_, fields) = run(&[src, vec![0; 70_000]].concat()).unwrap();
        assert!(fields.is_empty());
    }

    #[test]
    fn keywords_land_in_their_fields() {
        use MetaField::*;
        for (keyword, field) in [
            (&b"Author"[..], Author),
            (b"Description", Comment),
            (b"Creation Time", CreationDate),
            (b"Source", Camera),
            (b"Raw profile type xmp", Xmp),
            (b"Raw profile type iptc", Iptc),
            (b"Disclaimer", Other),
        ] {
            assert_eq!(text_field(keyword), field);
        }
        // No NUL within 80 bytes: not a keyword.
        assert_eq!(text(b"tEXt", &[b'a'; 100], true), (Some(Other), None));
    }
}
