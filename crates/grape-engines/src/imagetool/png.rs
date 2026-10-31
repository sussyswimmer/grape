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
//! - **Written back:** the minimal EXIF where the first EXIF was, when
//!   that was before the image data and the picture needs it, in its form
//!   ([`Form`]): an `eXIf`, bare or behind `Exif\0\0` as it was, or
//!   ImageMagick's raw profile in a `tEXt`.

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

/// How much of one compressed text is inflated to see whether it's blank:
/// text still blank this far in is reported by its keyword.
const BLANK_CHECK: u64 = 64 << 10;

/// The most text inflated in one file: past it, text is reported by its
/// keyword, uninflated, so the work between two reads (and cancel checks)
/// stays small.
const INFLATE_BUDGET: u64 = 16 << 20;

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

/// How the first EXIF before the image data was held, which the minimal
/// one takes, so that each reader turns the output as it turned the
/// source: the `image` crate (so Grape's conversions) reads an orientation
/// only from a bare `eXIf`, Pillow from all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    /// An `eXIf`, behind `Exif\0\0` or not.
    Exif { prefixed: bool },
    /// ImageMagick's raw profile of this name (`exif` or `APP1`), behind
    /// `Exif\0\0` or not.
    Profile { name: &'static str, prefixed: bool },
}

/// What a PNG's text chunk of `data` (its keyword first) names as a raw
/// EXIF profile, if it is one.
fn profile_name(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"Raw profile type exif\0") {
        Some("exif")
    } else if data.starts_with(b"Raw profile type APP1\0") {
        Some("APP1")
    } else {
        None
    }
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
    let mut first_exif: Option<(exif::Exif, Form)> = None;
    let mut budget = INFLATE_BUDGET;
    let mut exif_budget = MAX_READ as u64;
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
                let needs = first_exif.as_ref().and_then(|(e, form)| {
                    exif::minimal(e.orientation, e.dcf_adobe_rgb() && !color_chunk)
                        .map(|tiff| (tiff, *form))
                });
                for part in parts {
                    match part {
                        Part::Bytes(bytes) => write(w, &bytes)?,
                        Part::ExifSlot => {
                            if let Some((tiff, form)) = &needs {
                                write_minimal(w, tiff, *form)?;
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
                let (field, exif_blob) = text(&kind, &data, whole, &mut budget, &mut exif_budget);
                found.fields.extend(field);
                if let (Some(blob), Some(name)) = (exif_blob, profile_name(&data)) {
                    let form = |prefixed| Form::Profile { name, prefixed };
                    note_exif(&mut found, &mut first_exif, held.as_mut(), &blob, form);
                }
            }
            b"eXIf" => {
                if !whole {
                    found.fields.insert(MetaField::Other);
                }
                let form = |prefixed| Form::Exif { prefixed };
                note_exif(&mut found, &mut first_exif, held.as_mut(), &data, form);
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
/// fields and, for the first one before the image data, marks its place
/// and keeps it with its form (`form` of whether it was behind the
/// prefix).
fn note_exif(
    found: &mut Found,
    first: &mut Option<(exif::Exif, Form)>,
    held: Option<&mut Vec<Part>>,
    blob: &[u8],
    form: impl Fn(bool) -> Form,
) {
    let tiff = blob.strip_prefix(b"Exif\0\0").unwrap_or(blob);
    let walked = exif::walk(tiff);
    found.fields.extend(walked.fields.iter().copied());
    if first.is_none() {
        if let Some(parts) = held {
            parts.push(Part::ExifSlot);
            *first = Some((walked, form(tiff.len() < blob.len())));
        }
    }
}

/// Hex digits, as ImageMagick writes a raw profile's.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// Writes the minimal EXIF `tiff` in `form`. A raw profile is written as
/// ImageMagick writes one, uncompressed: a newline, its name, its length
/// in 8 columns, then its bytes in hex, 36 to a line.
fn write_minimal(w: &mut dyn Write, tiff: &[u8], form: Form) -> Result<(), Error> {
    let (Form::Exif { prefixed } | Form::Profile { prefixed, .. }) = form;
    let blob = if prefixed {
        [b"Exif\0\0".as_slice(), tiff].concat()
    } else {
        tiff.to_vec()
    };
    match form {
        Form::Exif { .. } => write_chunk(w, b"eXIf", &blob),
        Form::Profile { name, .. } => {
            let mut text = format!("Raw profile type {name}\0\n{name}\n{:8}", blob.len());
            for line in blob.chunks(36) {
                text.push('\n');
                for b in line {
                    text.push(char::from(HEX[usize::from(b >> 4)]));
                    text.push(char::from(HEX[usize::from(b & 15)]));
                }
            }
            text.push('\n');
            write_chunk(w, b"tEXt", text.as_bytes())
        }
    }
}

/// What a text chunk of `kind` holding `data` (all of it if `whole`)
/// says: its field, if its text isn't empty, and an EXIF blob for
/// ImageMagick's "Raw profile type exif" (or "APP1"). Inflates no more
/// than is left of `budget`, and takes what it inflates from it; a raw
/// EXIF profile takes from `exif_budget` first, then from `budget`, so
/// the text before it can't cost the picture its Orientation. A profile
/// cut short is reported as Other too, and one that holds no EXIF header
/// as Other alone.
fn text(
    kind: &[u8; 4],
    data: &[u8],
    whole: bool,
    budget: &mut u64,
    exif_budget: &mut u64,
) -> (Option<MetaField>, Option<Vec<u8>>) {
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
        let limit = exif_budget.saturating_add(*budget).min(MAX_READ as u64);
        let mut left = limit;
        let body = body.and_then(|(compressed, text)| {
            if compressed {
                inflate(text, whole, &mut left)
            } else {
                Some(text.to_vec())
            }
        });
        let used = limit - left;
        let from_exif = used.min(*exif_budget);
        *exif_budget -= from_exif;
        *budget -= used - from_exif;
        // Cut short by the read or the budget: more may have been there.
        let cut = !whole || (used == limit && limit < MAX_READ as u64);
        let other = cut.then_some(MetaField::Other);
        return match body.as_deref().and_then(unhex_profile) {
            Some(blob) if holds_exif(&blob) => (other, Some(blob)),
            _ => (Some(MetaField::Other), None),
        };
    }
    let blank = body.and_then(|(compressed, text)| {
        if compressed {
            inflates_blank(text, whole, budget)
        } else {
            // Text past what was read may not be blank.
            Some(whole && blank(text))
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

/// Whether zlib `data` (all of it if `whole`) inflates to blank text
/// ([`blank`]), inflating only as far as the first other byte, and at
/// most [`BLANK_CHECK`] or what's left of `budget`: text still blank there
/// isn't, nor is text cut short. `None` if it's broken.
fn inflates_blank(data: &[u8], whole: bool, budget: &mut u64) -> Option<bool> {
    let limit = BLANK_CHECK.min(*budget);
    let mut reader = flate2::read::ZlibDecoder::new(data).take(limit);
    let mut buf = [0; INFLATE_STEP];
    let mut read = 0;
    let found = loop {
        match reader.read(&mut buf) {
            Ok(0) => break Some(whole && read < limit),
            Ok(n) => {
                read += n as u64;
                if !blank(&buf[..n]) {
                    break Some(false);
                }
            }
            Err(_) if !whole && read > 0 => break Some(false),
            Err(_) => break None,
        }
    };
    *budget -= read;
    found
}

/// Inflates zlib data to at most [`MAX_READ`] bytes, or what's left of
/// `budget`; `None` if it's broken (or cut short, unless `whole`).
fn inflate(data: &[u8], whole: bool, budget: &mut u64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let reader = flate2::read::ZlibDecoder::new(data);
    let result = reader
        .take((MAX_READ as u64).min(*budget))
        .read_to_end(&mut out);
    *budget -= out.len() as u64;
    match result {
        Ok(_) => Some(out),
        Err(_) if !whole && !out.is_empty() => Some(out),
        Err(_) => None,
    }
}

/// The bytes of an ImageMagick raw profile: a newline, the profile's
/// name, its length, then the bytes in hex over many lines, up to a NUL if
/// there is one. Read in one pass; a profile cut short can end in half a
/// byte, which goes.
fn unhex_profile(body: &[u8]) -> Option<Vec<u8>> {
    let mut lines = body.splitn(4, |&b| b == b'\n');
    let _empty = lines.next()?;
    let _name = lines.next()?;
    let _length = lines.next()?;
    let hex = lines.next()?;
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut high = None;
    for &b in hex {
        let nibble = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ if b.is_ascii_whitespace() => continue,
            0 => break,
            _ => return None,
        };
        match high.take() {
            None => high = Some(nibble),
            Some(h) => out.push(h << 4 | nibble),
        }
    }
    Some(out)
}

/// Whether `blob` holds at least an EXIF header (a TIFF's byte order, 42
/// and IFD0's offset), after an optional `Exif\0\0`: less can't be the
/// picture's EXIF, so it doesn't take its place.
fn holds_exif(blob: &[u8]) -> bool {
    let tiff = blob.strip_prefix(b"Exif\0\0").unwrap_or(blob);
    tiff.len() >= 8 && (tiff.starts_with(b"II*\0") || tiff.starts_with(b"MM\0*"))
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
    fn the_orientation_comes_back_in_the_form_its_exif_had() {
        let t = tiff(
            b'I',
            &[&[(0x0112, V::Short(6)), (0x010F, V::Ascii("Sony"))]],
            None,
        );
        let minimal = exif::minimal(Some(6), false).unwrap();
        let behind = |tiff: &[u8]| [b"Exif\0\0".as_slice(), tiff].concat();
        let around = |exif: Vec<u8>| png(&[ihdr(), exif, idat(), chunk(b"IEND", &[])]);
        // An eXIf behind `Exif\0\0`, which the `image` crate reads no
        // orientation from and Pillow does: so is the minimal one.
        let (out, _) = run(&around(chunk(b"eXIf", &behind(&t)))).unwrap();
        assert_eq!(out, around(chunk(b"eXIf", &behind(&minimal))));
        // ImageMagick's raw profile, compressed, which only Pillow reads:
        // back as one, uncompressed, which reads back as the minimal EXIF.
        for (keyword, name) in [("exif", "exif"), ("APP1", "APP1")] {
            for blob in [behind(&t), t.clone()] {
                let mut raw = format!("\n{name}\n{:8}\n", blob.len()).into_bytes();
                for b in &blob {
                    raw.extend_from_slice(format!("{b:02x}").as_bytes());
                }
                let key = format!("Raw profile type {keyword}\0");
                let ztxt = [key.as_bytes(), b"\0", &zlib(&raw)].concat();
                let (out, _) = run(&around(chunk(b"zTXt", &ztxt))).unwrap();
                let prefixed = blob.len() > t.len();
                let back = if prefixed {
                    behind(&minimal)
                } else {
                    minimal.clone()
                };
                let mut text = format!("{key}\n{name}\n{:8}\n", back.len()).into_bytes();
                let hex: String = back.iter().map(|b| format!("{b:02x}")).collect();
                text.extend_from_slice(hex.as_bytes());
                text.push(b'\n');
                assert!(back.len() <= 36);
                assert_eq!(out, around(chunk(b"tEXt", &text)));
                let (field, read) = text_of(b"tEXt", &text, true);
                assert_eq!((field, read), (None, Some(back)));
            }
        }
    }

    #[test]
    fn a_long_raw_profile_is_written_36_bytes_to_a_line() {
        let mut out = Vec::new();
        let tiff: Vec<u8> = (0..80).collect();
        write_minimal(
            &mut out,
            &tiff,
            Form::Profile {
                name: "exif",
                prefixed: true,
            },
        )
        .unwrap();
        let text = &out[8..out.len() - 4];
        let lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
        assert_eq!(lines[0], b"Raw profile type exif\0");
        assert_eq!(lines[1], b"exif");
        assert_eq!(lines[2], b"      86");
        assert_eq!(lines[3].len(), 72);
        assert_eq!(lines[4].len(), 72);
        assert_eq!(lines[5].len(), 28);
        assert_eq!(lines[6], b"");
        assert_eq!(&lines[3][..12], b"457869660000");
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

    /// [`text`] with whole budgets.
    fn text_of(kind: &[u8; 4], data: &[u8], whole: bool) -> (Option<MetaField>, Option<Vec<u8>>) {
        text(
            kind,
            data,
            whole,
            &mut INFLATE_BUDGET.clone(),
            &mut (MAX_READ as u64),
        )
    }

    #[test]
    fn a_raw_exif_profile_is_read_when_the_text_budget_is_spent() {
        let t = tiff(
            b'I',
            &[&[(0x0112, V::Short(6)), (0x010F, V::Ascii("Canon"))]],
            None,
        );
        let mut raw = b"\nexif\n      40\n".to_vec();
        for b in [b"Exif\0\0".as_slice(), &t].concat() {
            raw.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        let ztxt = [b"Raw profile type exif\0\0".as_slice(), &zlib(&raw)].concat();
        let orientation = |blob: Option<Vec<u8>>| {
            let blob = blob.unwrap();
            exif::walk(blob.strip_prefix(b"Exif\0\0").unwrap_or(&blob)).orientation
        };
        // No text budget left: the profile has its own.
        let (mut budget, mut own) = (0, MAX_READ as u64);
        let (field, blob) = text(b"zTXt", &ztxt, true, &mut budget, &mut own);
        assert_eq!((field, orientation(blob)), (None, Some(6)));
        assert!(own < MAX_READ as u64);
        // An allowance spent on an earlier profile: the text budget pays.
        let (mut budget, mut own) = (INFLATE_BUDGET, 16);
        let (field, blob) = text(b"zTXt", &ztxt, true, &mut budget, &mut own);
        assert_eq!((field, orientation(blob)), (None, Some(6)));
        assert_eq!(own, 0);
        // Cut short mid-byte, past its first IFD: that IFD is still read,
        // and the rest is reported as Other.
        let header = b"\nexif\n      40\n".len();
        let cut = raw.len() - 7;
        assert_eq!((cut - header) % 2, 1, "an odd number of digits");
        let cut = cut as u64;
        let (field, blob) = text(b"zTXt", &ztxt, true, &mut 0, &mut { cut });
        assert_eq!(
            (field, orientation(blob)),
            (Some(MetaField::Other), Some(6))
        );
        // Cut before its EXIF header: Other, and no EXIF to take the place
        // of a later one.
        for keep in [16, 23] {
            let (field, blob) = text(b"zTXt", &ztxt, true, &mut 0, &mut { keep });
            assert_eq!((field, blob), (Some(MetaField::Other), None), "{keep}");
        }
        // A NUL after the hex ends it, as the old decoder's dropped odd
        // byte did.
        let mut ended = b"Raw profile type exif\0".to_vec();
        ended.extend_from_slice(&raw);
        ended.extend_from_slice(b"\n\0");
        let (field, blob) = text_of(b"tEXt", &ended, true);
        assert_eq!((field, orientation(blob)), (None, Some(6)));
        // Not hex at all.
        let mut bad = b"Raw profile type exif\0".to_vec();
        bad.extend_from_slice(b"\nexif\n      4\nzz");
        assert_eq!(text_of(b"tEXt", &bad, true), (Some(MetaField::Other), None));
    }

    #[test]
    fn compressed_text_is_inflated_only_until_it_isnt_blank() {
        let ztxt = |text: &[u8]| {
            let mut data = b"Comment\0\0".to_vec();
            data.extend(zlib(text));
            data
        };
        assert_eq!(text_of(b"zTXt", &ztxt(b"  \0 "), true), (None, None));
        assert_eq!(
            text_of(b"zTXt", &ztxt(b"  hi"), true),
            (Some(MetaField::Comment), None)
        );
        // Text past the first step is still found.
        let mut late = vec![b' '; 3 * INFLATE_STEP];
        late.push(b'x');
        assert_eq!(
            text_of(b"zTXt", &ztxt(&late), true),
            (Some(MetaField::Comment), None)
        );
        // Broken zlib: said by its keyword.
        assert_eq!(
            text_of(b"zTXt", b"Comment\0\0not zlib", true),
            (Some(MetaField::Comment), None)
        );
        let mut itxt = b"Title\0\x01\0en\0\0".to_vec();
        itxt.extend(zlib(b"\n\n"));
        assert_eq!(text_of(b"iTXt", &itxt, true), (None, None));
    }

    #[test]
    fn text_blank_as_far_as_it_was_looked_at_is_still_reported() {
        let ztxt = |text: &[u8]| {
            let mut data = b"Author\0\0".to_vec();
            data.extend(zlib(text));
            data
        };
        // Blank past the check, then a name.
        let mut late = vec![b' '; BLANK_CHECK as usize];
        late.extend_from_slice(b"Jane Doe");
        let author = (Some(MetaField::Author), None);
        assert_eq!(text_of(b"zTXt", &ztxt(&late), true), author);
        // Blank as far as it was read, the rest not read.
        let mut long = b"Author\0".to_vec();
        long.extend(vec![b' '; MAX_READ]);
        assert_eq!(text_of(b"tEXt", &long, false), author);
        assert_eq!(text_of(b"tEXt", &long, true), (None, None));
        // Each look takes from the file's budget; with none left, the
        // keyword is all that's said.
        let mut budget = INFLATE_STEP as u64;
        assert_eq!(
            text(b"zTXt", &ztxt(b"   "), true, &mut budget, &mut 0),
            (None, None)
        );
        assert_eq!(budget, INFLATE_STEP as u64 - 3);
        budget = 0;
        assert_eq!(
            text(b"zTXt", &ztxt(b"   "), true, &mut budget, &mut 0),
            author
        );
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
        assert_eq!(text_of(b"tEXt", &[b'a'; 100], true), (Some(Other), None));
    }
}
