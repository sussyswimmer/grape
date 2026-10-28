//! Remove Metadata on JPEG (D26): every segment the decoder needs is
//! copied as it is, the scans' entropy-coded data byte for byte; the rest
//! is dropped, and said what it was.
//!
//! - **Kept:** SOI; APP0 JFIF, without its thumbnail (16 bytes); APP2
//!   `ICC_PROFILE`; APP14 Adobe; the tables and frame (DQT, DHT, DAC,
//!   SOFn, DRI, DNL, and any other marker that isn't APPn or COM); each
//!   SOS with its data, also between progressive scans; EOI.
//! - **Dropped:** APP1 Exif (its fields, [`exif::walk`]), APP1 XMP and
//!   extended XMP, JFXX and a JFIF thumbnail, APP2 MPF and FlashPix,
//!   APP13 (IPTC, and its thumbnail), APP3 to APP12 and APP15 (C2PA
//!   included), COM, and everything after the first EOI (gain maps, depth
//!   maps, motion-photo video).
//! - **Written back:** the minimal EXIF ([`exif::minimal`]) where the
//!   first EXIF was, when the picture needs it: an orientation other than
//!   1, or Adobe RGB signalled the DCF way with no ICC profile.

use std::io::{BufRead, Write};

use super::exif;
use super::strip::{read_exact, read_u8, Error, Found};
use crate::MetaField;

/// The most segments before the first scan.
const MAX_HEADER_SEGMENTS: usize = 1024;

/// The most markers in the whole file (progressive JPEGs have a few dozen).
const MAX_MARKERS: usize = 65_536;

/// The most bytes skipped between segments: libjpeg skips stray bytes
/// before a marker, and so does Grape, up to this many.
const MAX_STRAY: usize = 1 << 20;

/// A JPEG starts with SOI and a marker.
pub(super) fn sniff(head: &[u8]) -> bool {
    head.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// A header segment, before the first scan: written as it is, or the
/// place of the first EXIF.
enum Part {
    Bytes(Vec<u8>),
    ExifSlot,
}

/// Strips the JPEG `r` into `w` ([module docs](self)).
pub(super) fn strip(r: &mut dyn BufRead, w: &mut dyn Write) -> Result<Found, Error> {
    let mut found = Found::default();
    let mut soi = [0; 2];
    read_exact(r, &mut soi)?;
    if soi != [0xFF, 0xD8] {
        return Err(Error::Broken("no SOI".into()));
    }
    let mut markers = 0;
    let mut parts = Vec::new();
    let mut first_exif: Option<exif::Exif> = None;
    let mut icc = false;
    // The header, up to the first scan, is held: the minimal EXIF depends
    // on whether an ICC profile comes after the EXIF.
    let sos = loop {
        let marker = next_marker(r, &mut markers)?;
        match marker {
            0xD8 => return Err(Error::Broken("a second SOI".into())),
            0xD9 => return Err(Error::Broken("EOI before any scan".into())),
            0xDA => break read_segment(r, marker)?,
            0x01 | 0xD0..=0xD7 => parts.push(Part::Bytes(vec![0xFF, marker])),
            _ => {
                let segment = read_segment(r, marker)?;
                let payload = &segment[4..];
                match fate(marker, payload) {
                    Fate::Keep => {
                        icc |= marker == 0xE2;
                        parts.push(Part::Bytes(segment));
                    }
                    Fate::Jfif { thumbnail } => {
                        if thumbnail {
                            found.fields.insert(MetaField::Thumbnail);
                        }
                        parts.push(Part::Bytes(jfif_without_thumbnail(&segment)));
                    }
                    Fate::Exif => {
                        let walked = exif::walk(&payload[6..]);
                        found.fields.extend(walked.fields.iter().copied());
                        if first_exif.is_none() {
                            first_exif = Some(walked);
                            parts.push(Part::ExifSlot);
                        }
                    }
                    Fate::Drop(fields) => found.fields.extend(fields),
                }
            }
        }
        if parts.len() > MAX_HEADER_SEGMENTS {
            return Err(Error::Broken("too many segments".into()));
        }
    };

    write(w, &[0xFF, 0xD8])?;
    for part in parts {
        match part {
            Part::Bytes(bytes) => write(w, &bytes)?,
            Part::ExifSlot => {
                let needs = first_exif
                    .as_ref()
                    .and_then(|e| exif::minimal(e.orientation, e.dcf_adobe_rgb() && !icc));
                if let Some(tiff) = needs {
                    write_app1_exif(w, &tiff)?;
                }
            }
        }
    }
    write(w, &sos)?;

    // The scans, and what comes between them, up to EOI.
    loop {
        let marker = copy_entropy(r, w, &mut markers)?;
        match marker {
            0xD9 => {
                write(w, &[0xFF, 0xD9])?;
                break;
            }
            0xD8 => return Err(Error::Broken("a second SOI".into())),
            0xDA => {
                let segment = read_segment(r, marker)?;
                write(w, &segment)?;
            }
            _ => {
                let segment = read_segment(r, marker)?;
                match fate(marker, &segment[4..]) {
                    Fate::Keep => write(w, &segment)?,
                    Fate::Jfif { .. } | Fate::Exif => {
                        // Metadata after a scan: dropped, said.
                        let walked = exif::walk(segment.get(10..).unwrap_or_default());
                        if marker == 0xE1 {
                            found.fields.extend(walked.fields);
                        } else {
                            found.fields.insert(MetaField::Other);
                        }
                    }
                    Fate::Drop(fields) => found.fields.extend(fields),
                }
            }
        }
    }
    // Anything but padding after EOI: a second image, a gain map, video.
    let rest = r.fill_buf().map_err(Error::Read)?;
    if rest.iter().any(|&b| b != 0x00 && b != 0xFF) {
        found.fields.insert(MetaField::Embedded);
    }
    Ok(found)
}

/// What becomes of a segment.
enum Fate {
    Keep,
    /// APP0 JFIF: kept without its thumbnail; whether it had one.
    Jfif {
        thumbnail: bool,
    },
    /// APP1 Exif: dropped, walked.
    Exif,
    /// Dropped, holding these kinds of metadata.
    Drop(Vec<MetaField>),
}

const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const XMP_EXTENSION: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";

/// The fate of a segment with `marker` and `payload` (after its length).
fn fate(marker: u8, payload: &[u8]) -> Fate {
    use MetaField::*;
    match marker {
        0xE0 if payload.starts_with(b"JFIF\0") && payload.len() >= 14 => Fate::Jfif {
            thumbnail: payload[12] != 0 || payload[13] != 0,
        },
        // A JFIF too short to have a thumbnail is kept as it is.
        0xE0 if payload.starts_with(b"JFIF\0") => Fate::Keep,
        0xE0 if payload.starts_with(b"JFXX\0") => Fate::Drop(vec![Thumbnail]),
        0xE1 if payload.starts_with(b"Exif\0") && payload.len() >= 6 => Fate::Exif,
        0xE1 if payload.starts_with(XMP) || payload.starts_with(XMP_EXTENSION) => {
            Fate::Drop(vec![Xmp])
        }
        0xE2 if payload.starts_with(b"ICC_PROFILE\0") => Fate::Keep,
        0xE2 if payload.starts_with(b"MPF\0") || payload.starts_with(b"FPXR\0") => {
            Fate::Drop(vec![Embedded])
        }
        0xED => {
            let mut fields = vec![Iptc];
            if photoshop_thumbnail(payload) {
                fields.push(Thumbnail);
            }
            Fate::Drop(fields)
        }
        0xEE if payload.starts_with(b"Adobe") => Fate::Keep,
        0xFE if payload.iter().all(|&b| b == 0 || b.is_ascii_whitespace()) => Fate::Drop(vec![]),
        0xFE => Fate::Drop(vec![Comment]),
        0xE0..=0xEF => Fate::Drop(vec![Other]),
        _ => Fate::Keep,
    }
}

/// Whether an APP13 "Photoshop 3.0" segment holds a thumbnail resource
/// (0x040C, or the old 0x0409).
fn photoshop_thumbnail(payload: &[u8]) -> bool {
    let Some(mut rest) = payload.strip_prefix(b"Photoshop 3.0\0") else {
        return false;
    };
    // 8BIM, id (2), a Pascal name padded to even, size (4), data padded
    // to even.
    while rest.len() >= 12 && rest.starts_with(b"8BIM") {
        let id = u16::from_be_bytes([rest[4], rest[5]]);
        if id == 0x040C || id == 0x0409 {
            return true;
        }
        let name = 1 + usize::from(rest[6]);
        let at = 6 + name + name % 2;
        let Some(size) = rest.get(at..at + 4) else {
            return false;
        };
        let size = u32::from_be_bytes([size[0], size[1], size[2], size[3]]);
        let Ok(size) = usize::try_from(size) else {
            return false;
        };
        let next = (at + 4).saturating_add(size).saturating_add(size % 2);
        rest = rest.get(next..).unwrap_or_default();
    }
    false
}

/// An APP0 JFIF segment with its thumbnail removed: the 14 bytes up to the
/// density, and a 0×0 thumbnail.
fn jfif_without_thumbnail(segment: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, 0xE0, 0x00, 0x10];
    out.extend_from_slice(&segment[4..16]);
    out.extend_from_slice(&[0, 0]);
    out
}

/// Writes `tiff` as an APP1 Exif segment.
fn write_app1_exif(w: &mut dyn Write, tiff: &[u8]) -> Result<(), Error> {
    let len =
        u16::try_from(2 + 6 + tiff.len()).map_err(|_| Error::Broken("EXIF too big".into()))?;
    write(w, &[0xFF, 0xE1])?;
    write(w, &len.to_be_bytes())?;
    write(w, b"Exif\0\0")?;
    write(w, tiff)
}

fn write(w: &mut dyn Write, bytes: &[u8]) -> Result<(), Error> {
    w.write_all(bytes).map_err(Error::Write)
}

/// The next marker: stray bytes before it skipped (at most
/// [`MAX_STRAY`]), and fill bytes (`FF`) too.
fn next_marker(r: &mut dyn BufRead, markers: &mut usize) -> Result<u8, Error> {
    let mut stray = 0;
    loop {
        let byte = read_u8(r)?;
        if byte != 0xFF {
            stray += 1;
            if stray > MAX_STRAY {
                return Err(Error::Broken("no marker".into()));
            }
            continue;
        }
        let mut marker = read_u8(r)?;
        while marker == 0xFF {
            marker = read_u8(r)?;
        }
        if marker == 0x00 {
            // A stuffed zero outside a scan: stray.
            continue;
        }
        *markers += 1;
        if *markers > MAX_MARKERS {
            return Err(Error::Broken("too many markers".into()));
        }
        return Ok(marker);
    }
}

/// A segment with a length: the marker, the length and the payload, as
/// in the file.
fn read_segment(r: &mut dyn BufRead, marker: u8) -> Result<Vec<u8>, Error> {
    let mut len = [0; 2];
    read_exact(r, &mut len)?;
    let size = usize::from(u16::from_be_bytes(len));
    if size < 2 {
        return Err(Error::Broken(format!(
            "segment {marker:#04X} of length {size}"
        )));
    }
    let mut segment = vec![0xFF, marker, len[0], len[1]];
    segment.resize(2 + size, 0);
    read_exact(r, &mut segment[4..])?;
    Ok(segment)
}

/// Copies a scan's entropy-coded data, with its stuffed zeros and restart
/// markers, up to the marker that ends it, which it returns unwritten.
fn copy_entropy(r: &mut dyn BufRead, w: &mut dyn Write, markers: &mut usize) -> Result<u8, Error> {
    loop {
        let buf = r.fill_buf().map_err(Error::Read)?;
        if buf.is_empty() {
            return Err(Error::Broken("the image data ends without EOI".into()));
        }
        let Some(at) = buf.iter().position(|&b| b == 0xFF) else {
            let n = buf.len();
            write(w, buf)?;
            r.consume(n);
            continue;
        };
        write(w, &buf[..at])?;
        r.consume(at + 1);
        let mut next = read_u8(r)?;
        // Fill bytes before a marker.
        while next == 0xFF {
            next = read_u8(r)?;
        }
        match next {
            0x00 | 0xD0..=0xD7 => write(w, &[0xFF, next])?,
            marker => {
                *markers += 1;
                if *markers > MAX_MARKERS {
                    return Err(Error::Broken("too many markers".into()));
                }
                return Ok(marker);
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeSet;
    use std::io::Cursor;

    use super::*;
    use crate::imagetool::exif::tests::{tiff, V};

    /// A segment with `marker` and `payload`.
    pub(in crate::imagetool) fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
        let len = u16::try_from(payload.len() + 2).unwrap();
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&len.to_be_bytes());
        s.extend_from_slice(payload);
        s
    }

    /// The JFIF APP0 with a thumbnail of `w`×`h` (its pixels left out).
    fn jfif(w: u8, h: u8) -> Vec<u8> {
        let mut p = b"JFIF\0\x01\x02\x01\x00\x48\x00\x48".to_vec();
        p.extend_from_slice(&[w, h]);
        p.extend(std::iter::repeat_n(7, usize::from(w) * usize::from(h) * 3));
        seg(0xE0, &p)
    }

    /// The decoder's segments: DQT, SOF0, DHT, then SOS and scan data
    /// with a stuffed zero, a restart marker and fill bytes.
    fn frame() -> (Vec<u8>, Vec<u8>) {
        let mut tables = seg(0xDB, &[0; 65]);
        tables.extend(seg(0xC0, &[8, 0, 16, 0, 16, 1, 1, 0x11, 0]));
        tables.extend(seg(0xC4, &[0; 20]));
        let mut scan = seg(0xDA, &[1, 1, 0, 0, 63, 0]);
        scan.extend_from_slice(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0x78]);
        (tables, scan)
    }

    /// A JPEG: SOI, `head`, the frame, then `tail` before EOI and `after`
    /// after it.
    fn jpeg(head: &[Vec<u8>], tail: &[u8], after: &[u8]) -> Vec<u8> {
        let (tables, scan) = frame();
        let mut j = vec![0xFF, 0xD8];
        for s in head {
            j.extend_from_slice(s);
        }
        j.extend(tables);
        j.extend(scan);
        j.extend_from_slice(tail);
        j.extend_from_slice(&[0xFF, 0xD9]);
        j.extend_from_slice(after);
        j
    }

    fn run(input: &[u8]) -> Result<(Vec<u8>, Found), Error> {
        let mut out = Vec::new();
        let found = strip(&mut Cursor::new(input), &mut out)?;
        Ok((out, found))
    }

    fn fields(list: &[MetaField]) -> BTreeSet<MetaField> {
        list.iter().copied().collect()
    }

    fn exif_seg(t: &[u8]) -> Vec<u8> {
        let mut p = b"Exif\0\0".to_vec();
        p.extend_from_slice(t);
        seg(0xE1, &p)
    }

    #[test]
    fn a_plain_jpeg_comes_out_byte_for_byte() {
        let icc = seg(0xE2, b"ICC_PROFILE\0\x01\x01profile");
        let adobe = seg(0xEE, b"Adobe\0\x64\0\0\0\0\x01");
        let src = jpeg(&[jfif(0, 0), icc, adobe], &[], &[]);
        let (out, found) = run(&src).unwrap();
        assert_eq!(out, src);
        assert!(found.fields.is_empty());
    }

    #[test]
    fn metadata_goes_and_says_what_it_was() {
        use MetaField::*;
        let camera = tiff(
            b'I',
            &[
                &[(0x010F, V::Ascii("Canon")), (0x8825, V::Ifd(1))],
                &[(0x0002, V::Bytes(5, &[0, 0, 0, 48, 0, 0, 0, 1]))],
            ],
            None,
        );
        let mut xmp = XMP.to_vec();
        xmp.extend_from_slice(b"<x:xmpmeta/>");
        let iptc = b"Photoshop 3.0\08BIM\x04\x0c\0\0\0\0\0\x02ab";
        let head = [
            jfif(2, 1),
            exif_seg(&camera),
            seg(0xE1, &xmp),
            seg(0xE2, b"MPF\0\0\0"),
            seg(0xED, iptc),
            seg(0xEB, b"JP\0\0c2pa"),
            seg(0xFE, b"made with love"),
            seg(0xE0, b"JFXX\0\x10"),
        ];
        let src = jpeg(&head, &[], b"\xFF\xD8 a gain map");
        let (out, found) = run(&src).unwrap();
        assert_eq!(
            found.fields,
            fields(&[Location, Camera, Comment, Xmp, Iptc, Thumbnail, Embedded, Other])
        );
        // What's left: SOI, the JFIF without its thumbnail, the frame, EOI.
        assert_eq!(out, jpeg(&[jfif(0, 0)], &[], &[]));
    }

    #[test]
    fn the_orientation_stays_where_the_exif_was() {
        let t = tiff(
            b'M',
            &[&[(0x010F, V::Ascii("Apple")), (0x0112, V::Short(6))]],
            None,
        );
        let src = jpeg(&[jfif(0, 0), exif_seg(&t), seg(0xFE, b"hi")], &[], &[]);
        let (out, found) = run(&src).unwrap();
        assert_eq!(
            found.fields,
            fields(&[MetaField::Camera, MetaField::Comment])
        );
        let minimal = exif::minimal(Some(6), false).unwrap();
        assert_eq!(out, jpeg(&[jfif(0, 0), exif_seg(&minimal)], &[], &[]));
        // Upright: nothing written back.
        let t = tiff(b'M', &[&[(0x0112, V::Short(1))]], None);
        let (out, _) = run(&jpeg(&[exif_seg(&t)], &[], &[])).unwrap();
        assert_eq!(out, jpeg(&[], &[], &[]));
    }

    #[test]
    fn adobe_rgb_the_dcf_way_is_written_back_only_without_a_profile() {
        let t = tiff(
            b'I',
            &[
                &[(0x8769, V::Ifd(1))],
                &[(0xA001, V::Short(0xFFFF)), (0xA005, V::Ifd(2))],
                &[(0x0001, V::Ascii("R03"))],
            ],
            None,
        );
        let (out, _) = run(&jpeg(&[exif_seg(&t)], &[], &[])).unwrap();
        let minimal = exif::minimal(None, true).unwrap();
        assert_eq!(out, jpeg(&[exif_seg(&minimal)], &[], &[]));
        let icc = seg(0xE2, b"ICC_PROFILE\0\x01\x01profile");
        let (out, _) = run(&jpeg(&[exif_seg(&t), icc.clone()], &[], &[])).unwrap();
        assert_eq!(out, jpeg(&[icc], &[], &[]));
    }

    #[test]
    fn progressive_scans_and_tables_between_them_are_kept() {
        let mut between = seg(0xC4, &[1; 20]);
        between.extend(seg(0xDA, &[1, 1, 0, 1, 63, 0]));
        between.extend_from_slice(&[0x9A, 0xFF, 0x00]);
        between.extend(seg(0xFE, b"late comment"));
        between.extend(seg(0xDD, &[0, 8]));
        let src = jpeg(&[], &between, &[]);
        let (out, found) = run(&src).unwrap();
        assert_eq!(found.fields, fields(&[MetaField::Comment]));
        let mut kept = seg(0xC4, &[1; 20]);
        kept.extend(seg(0xDA, &[1, 1, 0, 1, 63, 0]));
        kept.extend_from_slice(&[0x9A, 0xFF, 0x00]);
        kept.extend(seg(0xDD, &[0, 8]));
        assert_eq!(out, jpeg(&[], &kept, &[]));
    }

    #[test]
    fn padding_after_eoi_isnt_embedded_data() {
        let (_, found) = run(&jpeg(&[], &[], &[0, 0, 0xFF, 0])).unwrap();
        assert!(found.fields.is_empty());
    }

    #[test]
    fn broken_jpegs_are_refused() {
        let good = jpeg(&[jfif(0, 0)], &[], &[]);
        for n in 0..good.len() - 2 {
            assert!(
                matches!(run(&good[..n]), Err(Error::Broken(_) | Error::Read(_))),
                "cut at {n}"
            );
        }
        for bad in [
            &b"\xFF\xD9"[..],
            b"\xFF\xD8\xFF\xD8",
            b"\xFF\xD8\xFF\xD9",
            b"\xFF\xD8\xFF\xE1\x00\x01",
        ] {
            assert!(
                matches!(run(bad), Err(Error::Broken(_) | Error::Read(_))),
                "{bad:?}"
            );
        }
        // Too many segments before the scan.
        let many: Vec<Vec<u8>> = (0..=MAX_HEADER_SEGMENTS)
            .map(|_| seg(0xDB, &[0; 3]))
            .collect();
        assert!(matches!(run(&jpeg(&many, &[], &[])), Err(Error::Broken(_))));
    }

    #[test]
    fn stray_bytes_before_a_marker_are_skipped() {
        let mut src = vec![0xFF, 0xD8, 0x00, 0x12];
        let (tables, scan) = frame();
        src.extend(tables);
        src.extend(scan);
        src.extend_from_slice(&[0xFF, 0xD9]);
        let (out, _) = run(&src).unwrap();
        assert_eq!(out, jpeg(&[], &[], &[]));
    }
}
