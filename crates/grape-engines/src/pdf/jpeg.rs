//! JPEG passthrough for images → PDF (D37): which JPEGs go into the PDF
//! as their original data (`/DCTDecode`, no generation loss), and which of
//! their bytes are kept.
//!
//! A JPEG passes through when its frame is baseline, extended-sequential or
//! progressive Huffman at 8 bits (SOF0, SOF1, SOF2) with 1 or 3 components,
//! and for 3 components the color transform is known the same way by
//! Grape's decoder (zune-jpeg) and by PDF readers:
//! - an Adobe APP14: transform 0 is RGB (`/ColorTransform 0`), 1 YCbCr;
//!   2 (YCCK) doesn't pass; nor does transform 0 beside a JFIF APP0
//!   (libjpeg-style readers take JFIF for YCbCr), nor transform 1 with
//!   component ids `R G B` (zune-jpeg reads those as RGB);
//! - else a JFIF APP0 (YCbCr), unless the ids are `R G B`;
//! - else component ids 1, 2, 3 (YCbCr).
//!
//! What is kept: SOI; the JFIF APP0 (unless it holds a thumbnail), the
//! `ICC_PROFILE` APP2s and the Adobe APP14; every DQT, DHT, DRI and the SOF
//! before the first SOS; and from the first SOS on, everything up to and
//! including EOI, verbatim. What is dropped: APP1 (EXIF, XMP), APP3–APP13,
//! APP15, COM, other APP0/APP2/APP14 segments (JFXX thumbnails, MPF,
//! FlashPix), fill bytes before a kept marker, and everything after EOI.
//! So no GPS, camera, thumbnail or XMP data reaches the PDF. EXIF
//! orientation is read from the APP1 before it's dropped, as Grape's
//! decoder reads it: the last `Exif` APP1 wins.
//!
//! Anything else, a marker this module doesn't know before the first SOS,
//! an APPn or COM between scans, or a malformed structure (a length past
//! the end, no SOS, no EOI) doesn't pass: the decoder accepted the file,
//! so the caller re-encodes its pixels instead.

use std::ops::Range;

use ::image::metadata::Orientation;

/// A JPEG that goes into the PDF as is, minus its metadata.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Passthrough {
    /// The source's bytes to keep, in order.
    pub(crate) keep: Vec<Range<usize>>,
    /// The stored image's width (before orientation).
    pub(crate) width: u32,
    /// The stored image's height.
    pub(crate) height: u32,
    /// 1 (gray) or 3 (color).
    pub(crate) components: u8,
    /// True for RGB data (an Adobe APP14 with transform 0): the image
    /// XObject gets `/ColorTransform 0`.
    pub(crate) rgb: bool,
    /// The EXIF orientation, applied by the page's matrix.
    pub(crate) orientation: Orientation,
}

impl Passthrough {
    /// The kept bytes of `bytes`, the source this was read from, in order.
    pub(crate) fn parts<'a>(&self, bytes: &'a [u8]) -> Vec<&'a [u8]> {
        self.keep
            .iter()
            .filter_map(|range| bytes.get(range.clone()))
            .collect()
    }

    /// How many bytes are kept.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.keep.iter().map(|range| range.len()).sum()
    }
}

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;
const DHT: u8 = 0xC4;
const DRI: u8 = 0xDD;
const APP0: u8 = 0xE0;
const APP1: u8 = 0xE1;
const APP2: u8 = 0xE2;
const APP14: u8 = 0xEE;
const APP15: u8 = 0xEF;
const COM: u8 = 0xFE;

/// The frame header's facts.
struct Frame {
    width: u16,
    height: u16,
    ids: Vec<u8>,
}

/// Whether `bytes`, a JPEG Grape's decoder accepted, can pass through, and
/// what of it is kept. `None` sends it to the re-encode path.
pub(crate) fn passthrough(bytes: &[u8]) -> Option<Passthrough> {
    if bytes.get(..2)? != [0xFF, SOI] {
        return None;
    }
    // SOI first.
    let mut keep: Vec<Range<usize>> = Vec::with_capacity(8);
    keep.push(0..2);
    let mut at = 2;
    let mut frame: Option<Frame> = None;
    let mut jfif = false;
    let mut adobe: Option<u8> = None;
    let mut orientation = Orientation::NoTransforms;
    let sos_at = loop {
        let (marker_at, code) = marker(bytes, at)?;
        if code == SOS {
            break marker_at;
        }
        let end = segment_end(bytes, marker_at)?;
        let payload = &bytes[marker_at + 4..end];
        let whole = marker_at..end;
        match code {
            APP0 => {
                if let Some(thumbnail) = jfif_thumbnail(payload) {
                    jfif = true;
                    if !thumbnail {
                        keep.push(whole);
                    }
                }
            }
            APP1 => {
                // As zune-jpeg reads it, for the decoder's own orientation:
                // a payload over 6 bytes that starts `Exif\0\0`; the last
                // one wins.
                if payload.len() > 6 && payload.starts_with(b"Exif\0\0") {
                    orientation = Orientation::from_exif_chunk(&payload[6..])
                        .unwrap_or(Orientation::NoTransforms);
                }
            }
            APP2 => {
                if payload.starts_with(b"ICC_PROFILE\0") {
                    keep.push(whole);
                }
            }
            APP14 => {
                if payload.len() >= 12 && payload.starts_with(b"Adobe") {
                    adobe = Some(payload[11]);
                    keep.push(whole);
                }
            }
            0xE3..=0xED | APP15 | COM => {}
            0xC0..=0xC2 => {
                if frame.is_some() {
                    return None;
                }
                frame = Some(read_frame(payload)?);
                keep.push(whole);
            }
            DQT | DHT | DRI => keep.push(whole),
            // Lossless, hierarchical and arithmetic frames, DAC, DNL, the
            // reserved JPGn and anything else.
            _ => return None,
        }
        at = end;
    };
    let frame = frame?;
    let eoi_end = scans_end(bytes, sos_at)?;
    keep.push(sos_at..eoi_end);
    let rgb = match frame.ids.as_slice() {
        [_] => false,
        ids @ [_, _, _] => color_is_rgb(ids, jfif, adobe)?,
        _ => return None,
    };
    Some(Passthrough {
        keep,
        width: u32::from(frame.width),
        height: u32::from(frame.height),
        components: u8::try_from(frame.ids.len()).ok()?,
        rgb,
        orientation,
    })
}

/// For 3 components: `Some(true)` for RGB data, `Some(false)` for YCbCr,
/// `None` when the markers leave it unknown or readers would disagree.
fn color_is_rgb(ids: &[u8], jfif: bool, adobe: Option<u8>) -> Option<bool> {
    let rgb_ids = ids == b"RGB";
    match adobe {
        Some(0) if !jfif => Some(true),
        Some(1) if !rgb_ids => Some(false),
        Some(_) => None,
        None if jfif && !rgb_ids => Some(false),
        None if !jfif && ids == [1, 2, 3] => Some(false),
        None => None,
    }
}

/// The marker at `at`, skipping fill bytes: where its last `0xFF` is, and
/// its code. `None` if there is no marker there.
fn marker(bytes: &[u8], at: usize) -> Option<(usize, u8)> {
    if *bytes.get(at)? != 0xFF {
        return None;
    }
    let mut code_at = at + 1;
    while *bytes.get(code_at)? == 0xFF {
        code_at += 1;
    }
    let code = bytes[code_at];
    if code == 0 {
        return None;
    }
    Some((code_at - 1, code))
}

/// The end of the segment whose marker is at `marker_at`: its 2-byte
/// length counts itself, and must fit in `bytes`.
fn segment_end(bytes: &[u8], marker_at: usize) -> Option<usize> {
    let len = usize::from(u16::from_be_bytes([
        *bytes.get(marker_at + 2)?,
        *bytes.get(marker_at + 3)?,
    ]));
    if len < 2 {
        return None;
    }
    let end = marker_at + 2 + len;
    (end <= bytes.len()).then_some(end)
}

/// `Some(has_thumbnail)` for a JFIF APP0 payload, `None` for any other
/// APP0 (JFXX, AVI1, …).
fn jfif_thumbnail(payload: &[u8]) -> Option<bool> {
    if !payload.starts_with(b"JFIF\0") || payload.len() < 14 {
        return None;
    }
    Some(payload[12] != 0 && payload[13] != 0)
}

/// An 8-bit frame header with a size and 1 or 3 components.
fn read_frame(payload: &[u8]) -> Option<Frame> {
    let (&precision, rest) = payload.split_first()?;
    let height = u16::from_be_bytes([*rest.first()?, *rest.get(1)?]);
    let width = u16::from_be_bytes([*rest.get(2)?, *rest.get(3)?]);
    let count = usize::from(*rest.get(4)?);
    let specs = rest.get(5..5 + 3 * count)?;
    if precision != 8 || width == 0 || height == 0 || !matches!(count, 1 | 3) {
        return None;
    }
    let ids = specs
        .as_chunks::<3>()
        .0
        .iter()
        .map(|spec| spec[0])
        .collect();
    Some(Frame { width, height, ids })
}

/// The end of the EOI that closes the scans starting at the SOS marker at
/// `sos_at`. Between scans only SOS, DHT, DQT and DRI may come.
fn scans_end(bytes: &[u8], sos_at: usize) -> Option<usize> {
    let mut marker_at = sos_at;
    let mut code = SOS;
    loop {
        match code {
            EOI => return Some(marker_at + 2),
            SOS => {
                let mut at = segment_end(bytes, marker_at)?;
                // Entropy-coded data: 0xFF is followed by a stuffed 0x00,
                // a restart marker, fill bytes or the next marker.
                loop {
                    let ff = at + bytes.get(at..)?.iter().position(|&b| b == 0xFF)?;
                    let mut next = ff + 1;
                    while *bytes.get(next)? == 0xFF {
                        next += 1;
                    }
                    let found = bytes[next];
                    if found == 0 || (0xD0..=0xD7).contains(&found) {
                        at = next + 1;
                        continue;
                    }
                    marker_at = next - 1;
                    code = found;
                    break;
                }
            }
            DHT | DQT | DRI => {
                let end = segment_end(bytes, marker_at)?;
                let (next_at, next) = marker(bytes, end)?;
                marker_at = next_at;
                code = next;
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A segment: marker, length, payload.
    pub(crate) fn segment(code: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xFF, code];
        bytes.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn jfif(thumbnail: (u8, u8)) -> Vec<u8> {
        let mut payload = b"JFIF\0\x01\x02\x00\x00\x01\x00\x01".to_vec();
        payload.extend_from_slice(&[thumbnail.0, thumbnail.1]);
        payload.extend(std::iter::repeat_n(
            7u8,
            3 * usize::from(thumbnail.0) * usize::from(thumbnail.1),
        ));
        segment(APP0, &payload)
    }

    fn frame(code: u8, precision: u8, ids: &[u8]) -> Vec<u8> {
        let mut payload = vec![precision, 0, 2, 0, 3];
        payload.push(u8::try_from(ids.len()).unwrap());
        for &id in ids {
            payload.extend_from_slice(&[id, 0x11, 0]);
        }
        segment(code, &payload)
    }

    fn adobe(transform: u8) -> Vec<u8> {
        segment(
            APP14,
            &[b'A', b'd', b'o', b'b', b'e', 0, 100, 0, 0, 0, 0, transform],
        )
    }

    /// SOS through EOI: a scan header, entropy data with a stuffed byte, a
    /// restart marker and fill bytes, a DHT between scans, a second scan,
    /// fill bytes before EOI.
    fn scans() -> Vec<u8> {
        let mut bytes = segment(SOS, &[1, 1, 0, 0, 63, 0]);
        bytes.extend_from_slice(&[
            0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0xFF, 0xD3, 0x78,
        ]);
        bytes.extend(segment(
            DHT,
            &[0x10, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5],
        ));
        bytes.extend(segment(SOS, &[1, 1, 0, 0, 63, 0]));
        bytes.extend_from_slice(&[0x9A, 0xBC, 0xFF, 0xFF, 0xFF, EOI]);
        bytes
    }

    fn exif(orientation: u16) -> Vec<u8> {
        let mut payload = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
        payload.extend_from_slice(&orientation.to_be_bytes());
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        segment(APP1, &payload)
    }

    /// A JPEG of these segments, then [`scans`], then `trailer`.
    fn jpeg(segments: &[Vec<u8>], trailer: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xFF, SOI];
        for segment in segments {
            bytes.extend_from_slice(segment);
        }
        bytes.extend(scans());
        bytes.extend_from_slice(trailer);
        bytes
    }

    fn kept(bytes: &[u8], pass: &Passthrough) -> Vec<u8> {
        pass.parts(bytes).concat()
    }

    #[test]
    fn metadata_is_dropped_and_the_rest_kept() {
        let icc = segment(APP2, b"ICC_PROFILE\0\x01\x01profile");
        let dqt = segment(DQT, &[0; 65]);
        let sof = frame(0xC0, 8, &[1, 2, 3]);
        let dht = segment(DHT, &[0; 17]);
        let dri = segment(DRI, &[0, 4]);
        let mut filled_dqt = vec![0xFF, 0xFF];
        filled_dqt.extend_from_slice(&dqt);
        let segments = [
            jfif((0, 0)),
            exif(6),
            segment(APP1, b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta GPS/>"),
            icc.clone(),
            segment(0xED, b"Photoshop 3.0\0IPTC"),
            segment(COM, b"a comment"),
            segment(APP15, b"app15"),
            segment(APP2, b"MPF\0pointers"),
            filled_dqt,
            sof.clone(),
            dht.clone(),
            dri.clone(),
        ];
        let bytes = jpeg(&segments, b"trailing data after EOI");
        let pass = passthrough(&bytes).unwrap();
        let mut expected = vec![0xFF, SOI];
        for segment in [jfif((0, 0)), icc, dqt, sof, dht, dri] {
            expected.extend(segment);
        }
        expected.extend(scans());
        assert_eq!(kept(&bytes, &pass), expected);
        assert_eq!(pass.len(), expected.len());
        // From SOS to EOI, byte for byte.
        let sos = bytes.windows(2).position(|w| w == [0xFF, SOS]).unwrap();
        assert!(expected.ends_with(&bytes[sos..bytes.len() - b"trailing data after EOI".len()]));
        assert_eq!((pass.width, pass.height, pass.components), (3, 2, 3));
        assert!(!pass.rgb);
        assert_eq!(pass.orientation, Orientation::Rotate90);
        let out = kept(&bytes, &pass);
        for gone in [
            &b"Exif"[..],
            b"xmpmeta",
            b"Photoshop",
            b"comment",
            b"app15",
            b"MPF",
            b"trailing",
        ] {
            assert!(!out.windows(gone.len()).any(|w| w == gone), "{gone:?}");
        }
    }

    #[test]
    fn the_last_exif_wins_as_in_the_decoder() {
        let sof = frame(0xC0, 8, &[1]);
        let bytes = jpeg(&[exif(6), exif(3), sof.clone()], b"");
        assert_eq!(
            passthrough(&bytes).unwrap().orientation,
            Orientation::Rotate180
        );
        // An EXIF block without an orientation counts as none.
        let empty = segment(APP1, b"Exif\0\0MM\0\x2a\0\0\0\x08\0\0");
        let bytes = jpeg(&[exif(6), empty, sof], b"");
        assert_eq!(
            passthrough(&bytes).unwrap().orientation,
            Orientation::NoTransforms
        );
    }

    #[test]
    fn thumbnails_in_app0_are_dropped() {
        let sof = frame(0xC0, 8, &[1, 2, 3]);
        let bytes = jpeg(&[jfif((2, 1)), sof.clone()], b"");
        let pass = passthrough(&bytes).unwrap();
        assert!(!kept(&bytes, &pass).windows(4).any(|w| w == b"JFIF"));
        // Still JFIF, so YCbCr.
        assert!(!pass.rgb);
        let jfxx = segment(APP0, b"JFXX\0\x10thumbnail");
        let bytes = jpeg(&[jfxx, sof], b"");
        let pass = passthrough(&bytes).unwrap();
        assert!(!kept(&bytes, &pass).windows(4).any(|w| w == b"JFXX"));
    }

    #[test]
    fn only_known_frames_pass() {
        for (code, precision, ids, passes) in [
            (0xC0, 8, &[1u8, 2, 3][..], true),
            (0xC1, 8, &[1, 2, 3], true),
            (0xC2, 8, &[1, 2, 3], true),
            (0xC0, 8, &[1], true),
            (0xC0, 12, &[1, 2, 3], false),
            (0xC1, 12, &[1], false),
            (0xC3, 8, &[1, 2, 3], false),
            (0xC5, 8, &[1, 2, 3], false),
            (0xC9, 8, &[1, 2, 3], false),
            (0xCA, 8, &[1, 2, 3], false),
            (0xCB, 8, &[1], false),
            (0xC0, 8, &[1, 2, 3, 4], false),
            (0xC0, 8, &[1, 2], false),
        ] {
            let bytes = jpeg(&[frame(code, precision, ids)], b"");
            assert_eq!(
                passthrough(&bytes).is_some(),
                passes,
                "SOF {code:#x} P {precision} {ids:?}"
            );
        }
        // Arithmetic conditioning and the reserved JPGn markers.
        let sof = frame(0xC0, 8, &[1]);
        assert!(passthrough(&jpeg(&[segment(0xCC, &[0, 1]), sof.clone()], b"")).is_none());
        assert!(passthrough(&jpeg(&[segment(0xF7, &[0, 1]), sof.clone()], b"")).is_none());
        // Two frames, or none.
        assert!(passthrough(&jpeg(&[sof.clone(), sof.clone()], b"")).is_none());
        assert!(passthrough(&jpeg(&[], b"")).is_none());
        // A frame with no height (DNL).
        let mut no_height = frame(0xC0, 8, &[1]);
        no_height[5..7].copy_from_slice(&[0, 0]);
        assert!(passthrough(&jpeg(&[no_height], b"")).is_none());
    }

    #[test]
    fn the_color_transform_must_be_known() {
        let sof = |ids: &[u8]| frame(0xC0, 8, ids);
        let rgb = |segments: &[Vec<u8>]| passthrough(&jpeg(segments, b"")).map(|p| p.rgb);
        // Adobe: 0 is RGB, 1 YCbCr, 2 YCCK doesn't pass.
        assert_eq!(rgb(&[adobe(0), sof(&[1, 2, 3])]), Some(true));
        assert_eq!(rgb(&[adobe(1), sof(&[1, 2, 3])]), Some(false));
        assert_eq!(rgb(&[adobe(2), sof(&[1, 2, 3])]), None);
        // Adobe 0 beside JFIF: readers disagree.
        assert_eq!(rgb(&[jfif((0, 0)), adobe(0), sof(&[1, 2, 3])]), None);
        assert_eq!(rgb(&[jfif((0, 0)), adobe(1), sof(&[1, 2, 3])]), Some(false));
        // R G B ids: RGB to zune-jpeg, so only with Adobe 0.
        assert_eq!(rgb(&[adobe(0), sof(b"RGB")]), Some(true));
        assert_eq!(rgb(&[adobe(1), sof(b"RGB")]), None);
        assert_eq!(rgb(&[jfif((0, 0)), sof(b"RGB")]), None);
        assert_eq!(rgb(&[sof(b"RGB")]), None);
        // JFIF alone is YCbCr; no markers, ids 1 2 3 are; other ids aren't
        // known.
        assert_eq!(rgb(&[jfif((0, 0)), sof(&[7, 8, 9])]), Some(false));
        assert_eq!(rgb(&[sof(&[1, 2, 3])]), Some(false));
        assert_eq!(rgb(&[sof(&[0, 1, 2])]), None);
        // Gray needs nothing.
        assert_eq!(rgb(&[sof(&[9])]), Some(false));
        // The Adobe segment is kept, its transform shown.
        let bytes = jpeg(&[adobe(0), sof(&[1, 2, 3])], b"");
        let pass = passthrough(&bytes).unwrap();
        assert!(kept(&bytes, &pass).windows(5).any(|w| w == b"Adobe"));
    }

    #[test]
    fn a_malformed_structure_takes_the_re_encode_path() {
        let sof = frame(0xC0, 8, &[1]);
        let good = jpeg(&[segment(DQT, &[0; 65]), sof.clone()], b"");
        assert!(passthrough(&good).is_some());
        // Not a JPEG.
        assert!(passthrough(b"\x89PNG\r\n").is_none());
        assert!(passthrough(&[0xFF]).is_none());
        // Truncated anywhere before EOI: in a segment, in a scan, the EOI.
        for cut in 3..good.len() - 1 {
            assert!(passthrough(&good[..cut]).is_none(), "cut at {cut}");
        }
        // A length past the end, or under 2.
        let mut over = good.clone();
        over[4..6].copy_from_slice(&[0xFF, 0xF0]);
        assert!(passthrough(&over).is_none());
        let mut under = good.clone();
        under[4..6].copy_from_slice(&[0, 1]);
        assert!(passthrough(&under).is_none());
        // Bytes that aren't a marker where one should be.
        let mut stray = vec![0xFF, SOI, 0x00];
        stray.extend_from_slice(&good[2..]);
        assert!(passthrough(&stray).is_none());
        // EOI before any scan; a second SOI.
        assert!(passthrough(&[0xFF, SOI, 0xFF, EOI]).is_none());
        let mut twice = vec![0xFF, SOI, 0xFF, SOI];
        twice.extend_from_slice(&good[2..]);
        assert!(passthrough(&twice).is_none());
        // A comment or APPn between scans.
        let mut between = vec![0xFF, SOI];
        between.extend(sof);
        between.extend(segment(SOS, &[1, 1, 0, 0, 63, 0]));
        between.extend_from_slice(&[0x11, 0x22]);
        between.extend(segment(COM, b"late"));
        between.extend_from_slice(&[0xFF, EOI]);
        assert!(passthrough(&between).is_none());
    }
}
