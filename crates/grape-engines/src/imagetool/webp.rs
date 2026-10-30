//! Remove Metadata on WebP (D26): the image chunks are copied as they
//! are, padding included (a last chunk's missing pad byte is written, as
//! libwebp refuses a file without it); the rest are dropped and said what
//! they were.
//!
//! - **Kept:** `VP8 `, `VP8L`, `VP8X`, `ALPH`, `ANIM` and `ICCP`, and each
//!   `ANMF` frame with its header and its `ALPH`, `VP8 ` or `VP8L`.
//! - **Dropped:** `EXIF` ([`exif::walk`], after an optional `Exif\0\0`),
//!   `XMP `, unknown chunks (inside a frame too), and anything after the
//!   RIFF's end.
//! - **Refused:** a frame holding `ANMF`, `VP8X`, `ANIM` or `ICCP`. libwebp
//!   ends such a frame at its image and reads the rest as top-level chunks,
//!   while the `image` crate trusts its size, so dropping them could lose a
//!   frame.
//! - **Patched afterwards:** the RIFF size, and VP8X's flags: XMP off, and
//!   EXIF on only when the minimal EXIF ([`exif::minimal`]) went back where
//!   the old one was. Without a VP8X (a simple WebP) there's no flag to
//!   set, so none goes back.
//!
//! Compress on WebP ([`super::webp_compress`], D28) reads a file's chunks
//! with [`scan`] and puts the source's metadata into libwebp's file with
//! [`assemble`].

use std::io::{BufRead, Cursor, SeekFrom};

use super::exif::{self, MAX_EXIF};
use super::strip::{more_after, read_at, read_exact, Error, Found, In, Out};
use crate::image::MAX_ICC;
use crate::MetaField;

/// The most chunks in a file (an animation has one per frame).
const MAX_CHUNKS: usize = 1 << 20;

/// An ANMF frame's header: its position, size, duration and flags.
const FRAME_HEADER: u64 = 16;

/// VP8X's flag for an animation.
const ANIMATION_FLAG: u8 = 0x02;

/// VP8X's flag for an XMP chunk.
pub(super) const XMP_FLAG: u8 = 0x04;

/// VP8X's flag for an EXIF chunk.
pub(super) const EXIF_FLAG: u8 = 0x08;

/// VP8X's flag for alpha.
pub(super) const ALPHA_FLAG: u8 = 0x10;

/// VP8X's flag for an ICC profile.
const ICC_FLAG: u8 = 0x20;

/// Where an extended file's VP8X flags are: [`scan`] wants `VP8X` first.
pub(super) const VP8X_FLAGS_AT: u64 = 20;

/// The most of a `VP8 ` frame's first partition [`quality`] reads: its
/// frame header up to the quantizer index takes at most 186 bits.
const VP8_HEADER: usize = 64;

/// The largest `EXIF` or `XMP ` chunk Compress carries (D28). Compress
/// keeps metadata, so a file with a larger one is refused rather than
/// written without it.
const MAX_CARRIED: u64 = 16 << 20;

pub(super) fn sniff(head: &[u8]) -> bool {
    head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP"
}

/// Strips the WebP `r` into `w` ([module docs](self)).
pub(super) fn strip(r: &mut dyn BufRead, w: &mut dyn Out) -> Result<Found, Error> {
    let mut found = Found::default();
    let mut header = [0; 12];
    read_exact(r, &mut header)?;
    let mut left = riff(&header)?;
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
        let Head { fourcc, size, body } = next(head, &mut left, &mut chunks)?;
        match &fourcc {
            b"VP8X" => {
                if size < 10 {
                    return Err(Error::Broken("a short VP8X".into()));
                }
                write(w, &head, &mut written)?;
                flags_at = Some(written);
                // The format's 10 bytes; anything a longer chunk claims is
                // copied as it comes, never held.
                let mut data = [0; 10];
                read_exact(r, &mut data)?;
                flags = data[0];
                write(w, &data, &mut written)?;
                copy(r, w, body - 10, &mut written)?;
                pad(w, size, body, &mut written)?;
            }
            b"VP8 " | b"VP8L" | b"ALPH" | b"ANIM" | b"ICCP" => {
                image |= matches!(&fourcc, b"VP8 " | b"VP8L");
                write(w, &head, &mut written)?;
                copy(r, w, body, &mut written)?;
                pad(w, size, body, &mut written)?;
            }
            b"ANMF" => {
                image = true;
                frame(r, w, size, &mut found, &mut written)?;
                skip(r, body - size)?;
            }
            b"EXIF" => {
                let walked = read_exif(r, size, &mut found)?;
                skip(r, body - size)?;
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
    if more_after(r, &[0])? {
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

/// What's left of the RIFF after "WEBP", from a file's first 12 bytes.
fn riff(header: &[u8; 12]) -> Result<u64, Error> {
    if !sniff(header) {
        return Err(Error::Broken("no RIFF WEBP header".into()));
    }
    let riff = u64::from(u32::from_le_bytes([
        header[4], header[5], header[6], header[7],
    ]));
    riff.checked_sub(4)
        .ok_or_else(|| Error::Broken("a RIFF too small".into()))
}

/// A top-level chunk, from its 8-byte header ([`next`]).
struct Head {
    fourcc: [u8; 4],
    /// The size the header gives.
    size: u64,
    /// What the chunk's body takes in the file: its size padded to even,
    /// or not, when it's the last chunk and the pad byte is missing.
    body: u64,
}

/// The top-level chunk whose header is `head`, `left` bytes of the RIFF
/// to go: takes it off `left` and counts it in `chunks`, at most
/// [`MAX_CHUNKS`].
fn next(head: [u8; 8], left: &mut u64, chunks: &mut usize) -> Result<Head, Error> {
    let fourcc = [head[0], head[1], head[2], head[3]];
    let size = u64::from(u32::from_le_bytes([head[4], head[5], head[6], head[7]]));
    let padded = size + (size & 1);
    // The last chunk's pad byte may be missing.
    let body = if *left >= 8 + padded {
        padded
    } else if *left == 8 + size {
        size
    } else {
        return Err(Error::Broken(format!(
            "a chunk of {size} bytes past the RIFF's end"
        )));
    };
    *left -= 8 + body;
    *chunks += 1;
    if *chunks > MAX_CHUNKS {
        return Err(Error::Broken("too many chunks".into()));
    }
    Ok(Head { fourcc, size, body })
}

/// Reads an EXIF chunk's `size` bytes, walking at most [`MAX_EXIF`] of
/// them (after an optional `Exif\0\0`), and says what it held.
fn read_exif(r: &mut dyn BufRead, size: u64, found: &mut Found) -> Result<exif::Exif, Error> {
    let take = usize_of(size)?.min(MAX_EXIF);
    let mut data = vec![0; take];
    read_exact(r, &mut data)?;
    skip(r, size - take as u64)?;
    if (take as u64) < size {
        found.fields.insert(MetaField::Other);
    }
    let tiff = data.strip_prefix(b"Exif\0\0").unwrap_or(&data);
    let walked = exif::walk(tiff);
    found.fields.extend(walked.fields.iter().copied());
    Ok(walked)
}

/// Copies an ANMF frame of `size` bytes: its header and its image
/// chunks, the rest dropped and said. Its size is written again after.
fn frame(
    r: &mut dyn BufRead,
    w: &mut dyn Out,
    size: u64,
    found: &mut Found,
    written: &mut u64,
) -> Result<(), Error> {
    if size < FRAME_HEADER {
        return Err(Error::Broken("a short ANMF".into()));
    }
    write(w, b"ANMF\0\0\0\0", written)?;
    let size_at = *written - 4;
    let mut header = [0; FRAME_HEADER as usize];
    read_exact(r, &mut header)?;
    write(w, &header, written)?;
    let mut left = size - FRAME_HEADER;
    while left > 0 {
        let mut head = [0; 8];
        if left < 8 {
            return Err(Error::Broken("a chunk cut short in an ANMF".into()));
        }
        read_exact(r, &mut head)?;
        let fourcc = [head[0], head[1], head[2], head[3]];
        let inner = u64::from(u32::from_le_bytes([head[4], head[5], head[6], head[7]]));
        let padded = inner + (inner & 1);
        // The frame's last chunk's pad byte may be missing.
        let body = if left >= 8 + padded {
            padded
        } else if left == 8 + inner {
            inner
        } else {
            return Err(Error::Broken(format!(
                "a chunk of {inner} bytes past its ANMF's end"
            )));
        };
        left -= 8 + body;
        match &fourcc {
            b"ALPH" | b"VP8 " | b"VP8L" => {
                write(w, &head, written)?;
                copy(r, w, body, written)?;
                pad(w, inner, body, written)?;
            }
            b"EXIF" => {
                read_exif(r, inner, found)?;
                skip(r, body - inner)?;
            }
            b"XMP " => {
                skip(r, body)?;
                found.fields.insert(MetaField::Xmp);
            }
            // Chunks only a file's top level holds: decoders disagree on
            // where such a frame ends, so it can't be kept for sure.
            b"ANMF" | b"VP8X" | b"ANIM" | b"ICCP" => {
                return Err(Error::Broken(format!(
                    "a {} chunk inside an ANMF",
                    String::from_utf8_lossy(&fourcc).trim_end()
                )));
            }
            _ => {
                skip(r, body)?;
                found.fields.insert(MetaField::Other);
            }
        }
    }
    let kept = u32::try_from(*written - size_at - 4)
        .map_err(|_| Error::Broken("a frame too big".into()))?;
    seek_write(w, size_at, &kept.to_le_bytes())?;
    w.seek(SeekFrom::End(0)).map_err(Error::Write)?;
    if kept % 2 == 1 {
        write(w, &[0], written)?;
    }
    Ok(())
}

/// What Compress needs to know of a WebP before it decodes it ([`scan`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Scan {
    /// An extended file's `VP8X`.
    pub(super) vp8x: Option<Vp8x>,
    /// The image chunk is `VP8L` (lossless), not `VP8 `.
    pub(super) lossless: bool,
    /// The size the image chunk's bitstream gives.
    pub(super) size: (u32, u32),
    /// Alpha as libwebp reads it, whatever VP8X's flag says: `VP8L`'s
    /// alpha bit, or an `ALPH` chunk before the `VP8 ` one.
    pub(super) alpha: bool,
    /// An `ALPH` chunk after the `VP8 ` one, which libwebp never reads and
    /// image-webp does when none comes before it.
    pub(super) late_alpha: bool,
    /// VP8X's animation flag: the walk stops there.
    pub(super) animated: bool,
    /// The quality, 0 to 100, a lossy image reads as on libwebp's scale,
    /// from its frame header ([`quality`]).
    pub(super) quality: Option<u8>,
    /// The size of the image chunk, `VP8 ` or `VP8L`.
    pub(super) image_bytes: u64,
    /// The size of the `ALPH` chunk libwebp reads with a `VP8 `.
    pub(super) alpha_bytes: u64,
    /// What Compress carries into its own file.
    pub(super) carried: Carried,
}

/// A `VP8X` chunk's flags and canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Vp8x {
    pub(super) flags: u8,
    pub(super) canvas: (u32, u32),
}

/// An extended file's first `ICCP`, `EXIF` and `XMP ` chunks, and VP8X's
/// flags for them, which go back as the source has them: a reader that
/// goes by the flags (libwebp's demuxer, so Chrome) and one that goes by
/// the chunks (image-webp) see the same in both files.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Carried {
    pub(super) flags: u8,
    pub(super) iccp: Option<Vec<u8>>,
    pub(super) exif: Option<Vec<u8>>,
    pub(super) xmp: Option<Vec<u8>>,
}

impl Carried {
    /// The bytes it holds.
    pub(super) fn len(&self) -> u64 {
        [&self.iccp, &self.exif, &self.xmp]
            .into_iter()
            .flatten()
            .map(|c| c.len() as u64)
            .sum()
    }
}

/// Walks a WebP's top-level chunks for Compress (D28), reading their
/// headers and seeking past their bodies, which the RIFF must hold whole,
/// as for Remove Metadata:
/// - an extended file (`VP8X` first) to the RIFF's end: its first `VP8 `
///   or `VP8L` (both is broken, as image-webp reads them), its `ALPH`
///   chunks, and its first `ICCP`, `EXIF` and `XMP `, up to
///   [`crate::image::MAX_ICC`] bytes for the profile and [`MAX_CARRIED`]
///   for each of the others (more is [`Error::TooLarge`]: Compress keeps
///   metadata). It stops at an animation (VP8X's flag). An `ANIM` or
///   `ANMF` chunk in a still file is broken, as are two `ALPH` chunks
///   before a `VP8 ` (libwebp reads the last, image-webp the first), any
///   before a `VP8L` (libwebp's still decoder then shows alpha, which
///   image-webp reads by VP8X's flag and the `VP8L`'s own bit, and its
///   demuxer refuses the file) and a canvas other than the image's size
///   (libwebp refuses it). Nothing past
///   the RIFF's end is read, by this walk or by the decoder (which
///   `crate::image` ends there: image-webp's own walk would go on, as far
///   again as the RIFF's size less 12 bytes from the end of `VP8X`);
/// - a simple file only to its first chunk, `VP8 ` or `VP8L`: libwebp and
///   image-webp read nothing after it, so nothing after it is carried.
pub(super) fn scan(r: &mut dyn In) -> Result<Scan, Error> {
    let mut header = [0; 12];
    read_at(r, 0, &mut header)?;
    let mut left = riff(&header)?;
    let mut at = 12;
    let mut chunks = 0;
    let mut found = Scan::default();
    let mut image = None;
    let mut alph = 0;
    while left > 0 {
        let mut raw = [0; 8];
        read_at(r, at, &mut raw)?;
        let head = next(raw, &mut left, &mut chunks)?;
        let body = at + 8;
        at = body + head.body;
        let first = chunks == 1;
        match &head.fourcc {
            b"VP8X" if first => {
                if head.size < 10 {
                    return Err(Error::Broken("a short VP8X".into()));
                }
                let mut data = [0; 10];
                read_at(r, body, &mut data)?;
                let side = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], 0]) + 1;
                let flags = data[0];
                found.vp8x = Some(Vp8x {
                    flags,
                    canvas: (side(&data[4..7]), side(&data[7..10])),
                });
                found.carried.flags = flags & (ICC_FLAG | EXIF_FLAG | XMP_FLAG);
                if flags & ANIMATION_FLAG != 0 {
                    found.animated = true;
                    return Ok(found);
                }
            }
            b"VP8 " | b"VP8L" => {
                match image {
                    None => {
                        if alph > 1 && &head.fourcc == b"VP8 " {
                            // libwebp reads the last, image-webp the first.
                            return Err(Error::Broken(format!(
                                "{alph} ALPH chunks before its VP8"
                            )));
                        }
                        if alph > 0 && &head.fourcc == b"VP8L" {
                            // libwebp's still decoder shows alpha whenever
                            // there's one, and its demuxer refuses it.
                            return Err(Error::Broken("an ALPH chunk before its VP8L".into()));
                        }
                        image = Some(head.fourcc);
                        found.image_bytes = head.size;
                        bitstream(r, body, &head, &mut found)?;
                    }
                    Some(fourcc) if fourcc != head.fourcc => {
                        return Err(Error::Broken("both a VP8 and a VP8L chunk".into()));
                    }
                    Some(_) => {}
                }
                if first {
                    break;
                }
            }
            _ if first => {
                return Err(Error::Broken(format!(
                    "a {} chunk first",
                    name(&head.fourcc)
                )));
            }
            b"ALPH" => {
                if image.is_none() {
                    found.alpha = true;
                    found.alpha_bytes = head.size;
                    alph += 1;
                } else {
                    found.late_alpha = true;
                }
            }
            b"ANIM" | b"ANMF" => {
                // In a still file: libwebp's still decoder skips it, and
                // its demuxer (Chrome's) refuses the file.
                return Err(Error::Broken(format!(
                    "an {} chunk in a still image",
                    name(&head.fourcc)
                )));
            }
            b"ICCP" => carry(r, body, &head, MAX_ICC as u64, &mut found.carried.iccp)?,
            b"EXIF" => carry(r, body, &head, MAX_CARRIED, &mut found.carried.exif)?,
            b"XMP " => carry(r, body, &head, MAX_CARRIED, &mut found.carried.xmp)?,
            _ => {}
        }
    }
    let Some(fourcc) = image else {
        return Err(Error::Broken("no image data".into()));
    };
    if let Some(vp8x) = found.vp8x.filter(|vp8x| vp8x.canvas != found.size) {
        // libwebp refuses such a still file, and image-webp decodes the
        // whole frame, whatever its size, before it compares them.
        let ((cw, ch), (w, h)) = (vp8x.canvas, found.size);
        return Err(Error::Broken(format!(
            "a {cw}x{ch} canvas over a {w}x{h} image"
        )));
    }
    if &fourcc == b"VP8L" {
        // An ALPH chunk is the lossy image's.
        found.late_alpha = false;
        found.alpha_bytes = 0;
    }
    Ok(found)
}

/// The size the bitstream of the image chunk `head` (its body at `at`)
/// gives, and for `VP8L` whether it's lossless with its alpha bit.
fn bitstream(r: &mut dyn In, at: u64, head: &Head, found: &mut Scan) -> Result<(), Error> {
    if &head.fourcc == b"VP8L" {
        // The signature, then 14 bits each of width and height less one,
        // the alpha bit and a 3-bit version.
        let mut b = [0; 5];
        if head.size < 5 {
            return Err(Error::Broken("a short VP8L".into()));
        }
        read_at(r, at, &mut b)?;
        let bits = u32::from_le_bytes([b[1], b[2], b[3], b[4]]);
        if b[0] != 0x2F || bits >> 29 != 0 {
            return Err(Error::Broken("a VP8L chunk with no VP8L header".into()));
        }
        found.lossless = true;
        found.size = ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1);
        // Alpha in a VP8L is its own: an ALPH chunk with it is ignored.
        found.alpha = bits >> 28 & 1 == 1;
    } else {
        // A key frame's tag, start code, then 14 bits each of width and
        // height and 2 of scale.
        let mut b = [0; 10];
        if head.size < 10 {
            return Err(Error::Broken("a short VP8".into()));
        }
        read_at(r, at, &mut b)?;
        if b[3..6] != [0x9D, 0x01, 0x2A] {
            return Err(Error::Broken("a VP8 chunk with no key frame".into()));
        }
        let side = |lo: u8, hi: u8| u32::from(u16::from_le_bytes([lo, hi]) & 0x3FFF);
        found.size = (side(b[6], b[7]), side(b[8], b[9]));
        // The frame header's first bytes, unread (None) when the file is
        // cut before they end.
        let take = usize::try_from(head.size - 10).map_or(VP8_HEADER, |n| n.min(VP8_HEADER));
        let mut header = vec![0; take];
        found.quality = match read_at(r, at + 10, &mut header) {
            Ok(()) => Some(quality(&header)),
            Err(Error::Broken(_)) => None,
            Err(e) => return Err(e),
        };
    }
    Ok(())
}

/// The quality, 0 to 100, a VP8 key frame reads as on libwebp's scale,
/// from the frame header that starts its first partition, `header` (RFC
/// 6386 9.3 to 9.6, 19.2). libwebp's encoder maps a quality q to a
/// quantizer index 127·(1 − c), where c is the cube root of q·2/3 below
/// 0.75 and of 2q − 1 above (`QualityToCompression`), and gives each of
/// its segments c raised to a power about 1 whose mean, weighted by the
/// segments' shares of the picture, is 1. So c is the weighted geometric
/// mean of the segments' (1 − index/127), the shares read from the
/// segment map's tree probabilities. Within about a point of the quality
/// libwebp was given, at any method, for photos and drawings alike.
pub(super) fn quality(header: &[u8]) -> u8 {
    let mut b = BoolReader::new(header);
    b.literal(2); // Color space, clamping.
    let mut segments = None;
    if b.flag() {
        let map = b.flag();
        let mut quant = [0; 4];
        let mut absolute = true;
        if b.flag() {
            absolute = b.flag();
            for q in &mut quant {
                *q = b.signed(7);
            }
            for _ in 0..4 {
                b.signed(6); // Loop filter levels.
            }
        }
        // A key frame's probabilities are 255 unless it sets them.
        let mut tree = [255; 3];
        if map {
            for p in &mut tree {
                if b.flag() {
                    *p = b.literal(8);
                }
            }
        }
        segments = Some((quant, absolute, tree));
    }
    b.literal(1 + 6 + 3); // Filter type, level, sharpness.
    if b.flag() && b.flag() {
        for _ in 0..8 {
            b.signed(6); // Loop filter deltas.
        }
    }
    b.literal(2); // Token partitions.
    let base = b.literal(7);
    let c = |index: i32| 1.0 - f64::from(index.clamp(0, 126)) / 127.0;
    let c = match segments {
        None => c(base),
        Some((quant, absolute, tree)) => {
            let [p0, p1, p2] = tree.map(|p| f64::from(p) / 255.0);
            let shares = [
                p0 * p1,
                p0 * (1.0 - p1),
                (1.0 - p0) * p2,
                (1.0 - p0) * (1.0 - p2),
            ];
            let log: f64 = quant
                .iter()
                .zip(shares)
                .map(|(&q, share)| share * c(if absolute { q } else { base + q }).ln())
                .sum();
            log.exp()
        }
    };
    let linear = c * c * c;
    let quality = if linear < 0.5 {
        linear * 1.5
    } else {
        (linear + 1.0) / 2.0
    };
    (quality * 100.0).round().clamp(0.0, 100.0) as u8
}

/// VP8's boolean decoder (RFC 6386 7.3), for fixed-probability bits only;
/// past the end of its data it reads zeros, as libwebp's does.
struct BoolReader<'a> {
    data: &'a [u8],
    at: usize,
    value: u32,
    range: u32,
    bits: u32,
}

impl<'a> BoolReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        let byte = |i: usize| u32::from(data.get(i).copied().unwrap_or(0));
        BoolReader {
            data,
            at: 2,
            value: byte(0) << 8 | byte(1),
            range: 255,
            bits: 0,
        }
    }

    fn bit(&mut self, probability: u32) -> bool {
        let split = 1 + (((self.range - 1) * probability) >> 8);
        let big = split << 8;
        let one = self.value >= big;
        if one {
            self.range -= split;
            self.value -= big;
        } else {
            self.range = split;
        }
        while self.range < 128 {
            self.value <<= 1;
            self.range <<= 1;
            self.bits += 1;
            if self.bits == 8 {
                self.bits = 0;
                self.value |= u32::from(self.data.get(self.at).copied().unwrap_or(0));
                self.at = self.at.saturating_add(1);
            }
        }
        one
    }

    fn flag(&mut self) -> bool {
        self.bit(128)
    }

    /// `n` bits, the first the highest.
    fn literal(&mut self, n: u32) -> i32 {
        (0..n).fold(0, |v, _| v << 1 | i32::from(self.flag()))
    }

    /// A flag, then if it's set `n` bits and a sign; 0 if not.
    fn signed(&mut self, n: u32) -> i32 {
        if !self.flag() {
            return 0;
        }
        let v = self.literal(n);
        if self.flag() {
            -v
        } else {
            v
        }
    }
}

/// Reads the chunk `head` (its body at `at`) into `slot`, unless an
/// earlier one is there: at most `max` bytes, or [`Error::TooLarge`].
fn carry(
    r: &mut dyn In,
    at: u64,
    head: &Head,
    max: u64,
    slot: &mut Option<Vec<u8>>,
) -> Result<(), Error> {
    if slot.is_some() {
        return Ok(());
    }
    if head.size > max {
        return Err(Error::TooLarge(format!(
            "a {} chunk of {} bytes, over the {max} Compress carries",
            name(&head.fourcc),
            head.size
        )));
    }
    let mut data = vec![0; usize_of(head.size)?];
    read_at(r, at, &mut data)?;
    *slot = Some(data);
    Ok(())
}

/// libwebp's `file`, a `width`×`height` picture, with `carried` put in it
/// (D28): `RIFF`, `VP8X`, `ICCP`, libwebp's `ALPH` and `VP8 ` or its
/// `VP8L`, `EXIF` and `XMP `, each padded to even. VP8X's canvas is the
/// new size, its alpha flag says whether libwebp wrote alpha (an `ALPH`
/// chunk, or `VP8L`'s alpha bit), and its ICC, EXIF and XMP flags are the
/// source's, for the chunks that go in. With nothing to carry, `file` as
/// it is. A chunk libwebp wouldn't write is [`Error::Broken`].
pub(super) fn assemble(
    file: Vec<u8>,
    width: u32,
    height: u32,
    carried: &Carried,
) -> Result<Vec<u8>, Error> {
    let Carried {
        flags,
        iccp,
        exif,
        xmp,
    } = carried;
    if iccp.is_none() && exif.is_none() && xmp.is_none() {
        return Ok(file);
    }
    let cut = || Error::Broken("libwebp's file is cut short".into());
    let header: [u8; 12] = file
        .get(..12)
        .and_then(|h| h.try_into().ok())
        .ok_or_else(cut)?;
    let mut left = riff(&header)?;
    let mut at = 12;
    let mut chunks = 0;
    let mut alph = None;
    let mut image = None;
    while left > 0 {
        let raw: [u8; 8] = file
            .get(at..at + 8)
            .and_then(|h| h.try_into().ok())
            .ok_or_else(cut)?;
        let head = next(raw, &mut left, &mut chunks)?;
        let body = file
            .get(at + 8..at + 8 + usize_of(head.size)?)
            .ok_or_else(cut)?;
        at += 8 + usize_of(head.body)?;
        match &head.fourcc {
            b"VP8X" => {}
            b"ALPH" if alph.is_none() => alph = Some(body),
            b"VP8 " | b"VP8L" if image.is_none() => image = Some((head.fourcc, body)),
            other => {
                return Err(Error::Broken(format!(
                    "libwebp wrote a {} chunk Grape didn't expect",
                    name(other)
                )));
            }
        }
    }
    let (fourcc, data) = image.ok_or_else(|| Error::Broken("libwebp wrote no image".into()))?;
    let alpha = if &fourcc == b"VP8L" {
        data.get(4).is_some_and(|b| b >> 4 & 1 == 1)
    } else {
        alph.is_some()
    };
    let side = |n: u32| {
        n.checked_sub(1)
            .filter(|&n| n < 1 << 24)
            .map(|n| n.to_le_bytes())
            .ok_or_else(|| Error::Broken(format!("a side of {n} px")))
    };
    let (w, h) = (side(width)?, side(height)?);
    let mut vp8x_flags = 0;
    for (chunk, flag) in [(iccp, ICC_FLAG), (exif, EXIF_FLAG), (xmp, XMP_FLAG)] {
        if chunk.is_some() {
            vp8x_flags |= flags & flag;
        }
    }
    if alpha {
        vp8x_flags |= ALPHA_FLAG;
    }
    let vp8x = [vp8x_flags, 0, 0, 0, w[0], w[1], w[2], h[0], h[1], h[2]];

    let mut out = Cursor::new(Vec::with_capacity(file.len() + 64 + carried.len() as usize));
    let mut written = 0;
    write(&mut out, b"RIFF\0\0\0\0WEBP", &mut written)?;
    write_chunk(&mut out, b"VP8X", &vp8x, &mut written)?;
    if let Some(iccp) = iccp {
        write_chunk(&mut out, b"ICCP", iccp, &mut written)?;
    }
    if let Some(alph) = alph.filter(|_| &fourcc == b"VP8 ") {
        write_chunk(&mut out, b"ALPH", alph, &mut written)?;
    }
    write_chunk(&mut out, &fourcc, data, &mut written)?;
    if let Some(exif) = exif {
        write_chunk(&mut out, b"EXIF", exif, &mut written)?;
    }
    if let Some(xmp) = xmp {
        write_chunk(&mut out, b"XMP ", xmp, &mut written)?;
    }
    let mut out = out.into_inner();
    let riff = u32::try_from(out.len() - 8).map_err(|_| Error::Broken("too big".into()))?;
    out[4..8].copy_from_slice(&riff.to_le_bytes());
    Ok(out)
}

/// A fourcc for a message, its trailing space trimmed.
fn name(fourcc: &[u8; 4]) -> String {
    String::from_utf8_lossy(fourcc).trim_end().to_owned()
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

/// Writes the pad byte a chunk of `size` bytes whose `body` was copied
/// lacked: the last chunk's may be missing in the source, but libwebp
/// refuses a file without it.
fn pad(w: &mut dyn Out, size: u64, body: u64, written: &mut u64) -> Result<(), Error> {
    if body < size + (size & 1) {
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

    /// An ANMF frame: a 16-byte header, then `chunks`.
    fn anmf(chunks: &[Vec<u8>]) -> Vec<u8> {
        chunk(b"ANMF", &[[3; 16].as_slice(), &chunks.concat()].concat())
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
                anmf(&[chunk(b"ALPH", b"a"), chunk(b"VP8 ", b"f1")]),
                anmf(&[chunk(b"VP8L", b"f2")]),
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
    fn chunks_inside_a_frame_go_too() {
        use MetaField::*;
        let src = webp(&[
            vp8x(0x02),
            chunk(b"ANIM", &[0; 6]),
            anmf(&[
                chunk(b"VP8L", b"frame one"),
                chunk(b"XMP ", b"<x:xmpmeta>Zoe</x:xmpmeta>"),
                chunk(b"ABCD", b"GPS"),
            ]),
            anmf(&[chunk(b"ALPH", b"al"), chunk(b"VP8 ", b"two")]),
        ]);
        let (out, fields) = run(&src).unwrap();
        assert_eq!(fields, [Xmp, Other].into());
        assert_eq!(
            out,
            webp(&[
                vp8x(0x02),
                chunk(b"ANIM", &[0; 6]),
                anmf(&[chunk(b"VP8L", b"frame one")]),
                anmf(&[chunk(b"ALPH", b"al"), chunk(b"VP8 ", b"two")]),
            ])
        );
        // A frame too short for its header, or with a chunk past its end.
        for bad in [
            webp(&[vp8x(0x02), chunk(b"ANMF", &[0; 15])]),
            webp(&[
                vp8x(0x02),
                chunk(b"ANMF", &[[0; 16].as_slice(), b"VP8 \x09\0\0\0ab"].concat()),
            ]),
        ] {
            assert!(matches!(run(&bad), Err(Error::Broken(_))), "{bad:?}");
        }
    }

    #[test]
    fn a_frame_holding_a_top_level_chunk_is_refused() {
        // libwebp ends a frame at its image and reads the next frame from
        // inside it; the `image` crate trusts the size and doesn't. Either
        // way, dropping it would lose a picture someone sees.
        for inner in [
            anmf(&[chunk(b"VP8L", b"two")]),
            vp8x(0x02),
            chunk(b"ANIM", &[0; 6]),
            chunk(b"ICCP", b"profile"),
        ] {
            let src = webp(&[
                vp8x(0x02),
                chunk(b"ANIM", &[0; 6]),
                anmf(&[chunk(b"VP8L", b"one"), inner]),
            ]);
            assert!(matches!(run(&src), Err(Error::Broken(_))), "{src:?}");
        }
    }

    #[test]
    fn a_big_exif_is_walked_as_far_as_it_was_read() {
        let big = vec![7; MAX_EXIF];
        let t = tiff(
            b'I',
            &[&[(0x0112, V::Short(3)), (0x927C, V::Bytes(7, &big))]],
            None,
        );
        let src = webp(&[vp8x(0x08), chunk(b"VP8 ", b"frame"), chunk(b"EXIF", &t)]);
        let (out, fields) = run(&src).unwrap();
        // The maker note is the camera's; the cut is Other.
        assert_eq!(fields, [MetaField::Camera, MetaField::Other].into());
        let minimal = exif::minimal(Some(3), false).unwrap();
        assert_eq!(
            out,
            webp(&[
                vp8x(0x08),
                chunk(b"VP8 ", b"frame"),
                chunk(b"EXIF", &minimal)
            ])
        );
    }

    #[test]
    fn data_after_the_riff_is_found_past_the_read_buffer() {
        let mut input = webp(&[chunk(b"VP8 ", b"frame")]);
        input.extend(vec![0; 70_000]);
        input.extend_from_slice(b"more");
        let mut out = Cursor::new(Vec::new());
        let found = strip(
            &mut std::io::BufReader::with_capacity(8, Cursor::new(input)),
            &mut out,
        )
        .unwrap();
        assert_eq!(found.fields, [MetaField::Other].into());
    }

    #[test]
    fn broken_webps_are_refused() {
        let good = webp(&[vp8x(0), chunk(b"VP8 ", b"frame!")]);
        for n in 0..good.len() {
            assert!(run(&good[..n]).is_err(), "cut at {n}");
        }
        // The last pad byte missing is fine, and it's written: libwebp
        // refuses a chunk without its pad. In a frame too.
        let unpadded = |file: Vec<u8>| {
            let mut short = file[..file.len() - 1].to_vec();
            let riff = u32::try_from(short.len() - 8).unwrap().to_le_bytes();
            short[4..8].copy_from_slice(&riff);
            short
        };
        let odd = webp(&[chunk(b"VP8 ", b"odd")]);
        assert_eq!(run(&unpadded(odd.clone())).unwrap().0, odd);
        let odd = webp(&[vp8x(0x10), chunk(b"ALPH", b"a"), chunk(b"VP8 ", b"odd")]);
        assert_eq!(run(&unpadded(odd.clone())).unwrap().0, odd);
        let frame = |inner: &[u8]| {
            let mut frame = b"ANMF".to_vec();
            frame.extend(u32::try_from(16 + inner.len()).unwrap().to_le_bytes());
            frame.extend([3; 16]);
            frame.extend(inner);
            frame
        };
        let anim = |frame: Vec<u8>| webp(&[vp8x(0x02), chunk(b"ANIM", &[0; 6]), frame]);
        // A frame whose image's pad is missing inside it, the frame's own
        // size even.
        let inner = b"VP8L\x03\0\0\0abc";
        let (out, _) = run(&anim(frame(inner))).unwrap();
        assert_eq!(out, anim(anmf(&[chunk(b"VP8L", b"abc")])));
        for bad in [
            webp(&[chunk(b"XMP ", b"no image")]),
            webp(&[chunk(b"VP8X", &[0; 4]), chunk(b"VP8 ", b"f")]),
            b"RIFF\x02\0\0\0WEBP".to_vec(),
        ] {
            assert!(matches!(run(&bad), Err(Error::Broken(_))), "{bad:?}");
        }
    }

    #[test]
    fn a_vp8x_is_never_held_whole() {
        // 30 bytes whose VP8X claims almost 4 GiB: refused as cut short,
        // having read only the format's 10 bytes into memory.
        let mut huge = b"RIFF\xff\xff\xff\xffWEBPVP8X\xf0\xff\xff\xff".to_vec();
        huge.extend_from_slice(&[0; 10]);
        assert!(matches!(run(&huge), Err(Error::Broken(_))));
        // A longer VP8X than the format's is copied whole.
        let long = webp(&[
            chunk(b"VP8X", &[0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 7, 7]),
            chunk(b"VP8 ", b"frame"),
        ]);
        assert_eq!(run(&long).unwrap().0, long);
    }

    /// A `VP8 ` chunk whose key frame says `w`×`h` (the rest isn't VP8).
    fn vp8(w: u16, h: u16) -> Vec<u8> {
        let [w0, w1] = w.to_le_bytes();
        let [h0, h1] = h.to_le_bytes();
        chunk(b"VP8 ", &[0x10, 2, 0, 0x9D, 1, 0x2A, w0, w1, h0, h1, 7])
    }

    /// A `VP8L` chunk whose header says `w`×`h` and `alpha`.
    fn vp8l(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let bits = (w - 1) | (h - 1) << 14 | u32::from(alpha) << 28;
        chunk(
            b"VP8L",
            &[[0x2F].as_slice(), &bits.to_le_bytes(), &[1, 2]].concat(),
        )
    }

    /// A VP8X chunk with `flags` and a `w`×`h` canvas.
    fn canvas(flags: u8, w: u32, h: u32) -> Vec<u8> {
        let (w, h) = ((w - 1).to_le_bytes(), (h - 1).to_le_bytes());
        chunk(
            b"VP8X",
            &[flags, 0, 0, 0, w[0], w[1], w[2], h[0], h[1], h[2]],
        )
    }

    fn scanned(file: &[u8]) -> Result<Scan, Error> {
        scan(&mut Cursor::new(file))
    }

    #[test]
    fn scan_reads_a_simple_file_to_its_image_only() {
        let found = scanned(&webp(&[vp8(300, 200)])).unwrap();
        assert_eq!(
            found,
            Scan {
                size: (300, 200),
                quality: Some(100),
                image_bytes: 11,
                ..Scan::default()
            }
        );
        let found = scanned(&webp(&[vp8l(5, 7, true)])).unwrap();
        assert!(found.lossless && found.alpha);
        assert_eq!(found.size, (5, 7));
        // Nothing after a simple file's image is read: libwebp and
        // image-webp don't, so none of it is carried.
        let found = scanned(&webp(&[
            vp8(9, 9),
            chunk(b"EXIF", b"MM\0*"),
            chunk(b"ALPH", b"a"),
            vp8l(1, 1, false),
        ]))
        .unwrap();
        assert_eq!(found.carried, Carried::default());
        assert!(!found.alpha && !found.late_alpha && !found.lossless);
    }

    #[test]
    fn scan_carries_an_extended_files_first_metadata_and_its_flags() {
        let file = webp(&[
            canvas(0x3C, 300, 200),
            chunk(b"ICCP", b"profile"),
            chunk(b"ALPH", b"al"),
            vp8(300, 200),
            chunk(b"EXIF", b"Exif\0\0II*\0"),
            chunk(b"XMP ", b"<x:xmpmeta/>"),
            chunk(b"EXIF", b"a second"),
            chunk(b"JUNK", b"x"),
        ]);
        let found = scanned(&file).unwrap();
        assert_eq!(
            found,
            Scan {
                vp8x: Some(Vp8x {
                    flags: 0x3C,
                    canvas: (300, 200)
                }),
                lossless: false,
                size: (300, 200),
                alpha: true,
                late_alpha: false,
                animated: false,
                quality: Some(100),
                image_bytes: 11,
                alpha_bytes: 2,
                carried: Carried {
                    flags: 0x2C,
                    iccp: Some(b"profile".to_vec()),
                    exif: Some(b"Exif\0\0II*\0".to_vec()),
                    xmp: Some(b"<x:xmpmeta/>".to_vec()),
                },
            }
        );
        assert_eq!(found.carried.len(), 7 + 10 + 12);
        // An ALPH chunk after the VP8: libwebp doesn't read it.
        let late = scanned(&webp(&[
            canvas(0x10, 9, 9),
            vp8(9, 9),
            chunk(b"ALPH", b"al"),
        ]))
        .unwrap();
        assert!(!late.alpha && late.late_alpha);
        // VP8L has its own alpha; an ALPH chunk after it is nothing (one
        // before it is refused).
        let lossless = scanned(&webp(&[
            canvas(0, 9, 9),
            vp8l(9, 9, false),
            chunk(b"ALPH", b"al"),
        ]))
        .unwrap();
        assert!(lossless.lossless && !lossless.alpha && !lossless.late_alpha);
        // Chunks without their flags are carried, and their flags stay off.
        let unflagged =
            scanned(&webp(&[canvas(0, 9, 9), vp8(9, 9), chunk(b"XMP ", b"x")])).unwrap();
        assert_eq!(unflagged.carried.flags, 0);
        assert_eq!(unflagged.carried.xmp.as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn scan_reads_alph_chunks_as_libwebp_and_image_webp_do() {
        // Two before the VP8: libwebp reads the last, image-webp the first.
        let two = webp(&[
            canvas(0x10, 9, 9),
            chunk(b"ALPH", b"a"),
            chunk(b"ALPH", b"b"),
            vp8(9, 9),
        ]);
        assert!(matches!(scanned(&two), Err(Error::Broken(_))));
        // Before a VP8L, one or more: libwebp's still decoder then shows
        // alpha whatever VP8X and the VP8L say, which image-webp goes by,
        // and its demuxer refuses the file.
        for alph in [
            &[chunk(b"ALPH", b"a")][..],
            &[chunk(b"ALPH", b"a"), chunk(b"ALPH", b"b")],
        ] {
            for alpha in [false, true] {
                let file = webp(&[&[canvas(0, 9, 9)], alph, &[vp8l(9, 9, alpha)]].concat());
                assert!(matches!(scanned(&file), Err(Error::Broken(_))), "{file:?}");
            }
        }
        // After it, neither reads it.
        let after = webp(&[canvas(0, 9, 9), vp8l(9, 9, false), chunk(b"ALPH", b"a")]);
        let found = scanned(&after).unwrap();
        assert!(!found.alpha && !found.late_alpha);
        // One before and one after: both read the one before.
        let both = scanned(&webp(&[
            canvas(0x10, 9, 9),
            chunk(b"ALPH", b"a"),
            vp8(9, 9),
            chunk(b"ALPH", b"b"),
        ]))
        .unwrap();
        assert!(both.alpha && both.late_alpha);
        // Past the RIFF's end, nothing is read: image-webp walked on, as
        // far again as the RIFF's size less 12 bytes from the end of VP8X,
        // till Grape's reader ended the file there for it.
        let file = webp(&[canvas(0x10, 9, 9), vp8(9, 9)]);
        let after = [&file[..], &chunk(b"ALPH", b"al")].concat();
        let found = scanned(&after).unwrap();
        assert!(!found.alpha && !found.late_alpha);
        // A lossless image has no late alpha.
        let lossless = webp(&[canvas(0x10, 9, 9), vp8l(9, 9, true), chunk(b"ALPH", b"al")]);
        assert!(!scanned(&lossless).unwrap().late_alpha);
    }

    #[test]
    fn scan_stops_at_an_animation() {
        for file in [
            webp(&[canvas(0x02, 9, 9), chunk(b"ANIM", &[0; 6])]),
            // Its flag set, and nothing after it read: an animation with
            // no frame is still an animation.
            webp(&[canvas(0x12, 9, 9), chunk(b"EXIF", &[0; 3])]),
        ] {
            assert!(scanned(&file).unwrap().animated, "{file:?}");
        }
        // Without the flag, an animation's chunks are a broken still file:
        // libwebp's still decoder skips them, its demuxer refuses the file.
        for file in [
            webp(&[canvas(0, 9, 9), chunk(b"ANIM", &[0; 6]), vp8(9, 9)]),
            webp(&[canvas(0, 9, 9), anmf(&[vp8(9, 9)])]),
            webp(&[canvas(0, 9, 9), vp8(9, 9), chunk(b"ANIM", &[0; 6])]),
            webp(&[canvas(0x10, 9, 9), vp8l(9, 9, true), anmf(&[vp8(9, 9)])]),
        ] {
            assert!(matches!(scanned(&file), Err(Error::Broken(_))), "{file:?}");
        }
    }

    #[test]
    fn scan_refuses_what_it_cant_read() {
        for bad in [
            webp(&[canvas(0, 9, 9), vp8(9, 9), vp8l(9, 9, false)]),
            webp(&[canvas(0, 9, 9), vp8l(9, 9, false), vp8(9, 9)]),
            webp(&[chunk(b"ICCP", b"profile"), vp8(9, 9)]),
            webp(&[canvas(0, 9, 9), chunk(b"EXIF", b"MM")]),
            webp(&[chunk(b"VP8L", &[0x2E, 0, 0, 0, 0])]),
            webp(&[chunk(b"VP8L", &[0x2F, 0, 0, 0, 0x20])]),
            webp(&[chunk(b"VP8 ", &[0x10, 2, 0, 0x9D, 1, 0x2B, 1, 0, 1, 0])]),
            webp(&[chunk(b"VP8 ", &[0x10, 2, 0, 0x9D, 1, 0x2A, 1, 0, 1])]),
            webp(&[chunk(b"VP8X", &[0; 9]), vp8(9, 9)]),
            // A canvas other than the image's size, which libwebp refuses:
            // image-webp would decode the whole frame before it checks.
            webp(&[canvas(0, 9, 9), vp8(8, 9)]),
            webp(&[canvas(0x10, 16_383, 16_383), vp8l(96, 64, true)]),
            webp(&[canvas(0, 1, 1), vp8(16_383, 16_383)]),
        ] {
            assert!(matches!(scanned(&bad), Err(Error::Broken(_))), "{bad:?}");
        }
        // Cut anywhere before its image's 10-byte header ends (the VP8
        // chunk's last data byte and its pad follow), a file is refused.
        let file = webp(&[canvas(0x08, 9, 9), chunk(b"EXIF", b"MM\0*"), vp8(9, 9)]);
        for n in 0..file.len() {
            assert_eq!(
                scanned(&file[..n]).is_ok(),
                n >= file.len() - 2,
                "cut at {n}"
            );
        }
        // Metadata over what Compress carries, refused before it's read.
        let mut big = webp(&[canvas(0x08, 9, 9), vp8(9, 9)]);
        let size = u32::try_from(MAX_CARRIED + 1).unwrap();
        big.extend_from_slice(b"EXIF");
        big.extend_from_slice(&size.to_le_bytes());
        let riff = u32::try_from(big.len() - 8).unwrap() + size + 1;
        big[4..8].copy_from_slice(&riff.to_le_bytes());
        assert!(matches!(scanned(&big), Err(Error::TooLarge(_))));
        // At the cap it's read (here, found cut short).
        let at_cap = u32::try_from(MAX_CARRIED).unwrap();
        let n = big.len();
        big[n - 4..].copy_from_slice(&at_cap.to_le_bytes());
        let riff = u32::try_from(n - 8).unwrap() + at_cap;
        big[4..8].copy_from_slice(&riff.to_le_bytes());
        assert!(matches!(scanned(&big), Err(Error::Broken(_))));
    }

    /// The fourcc of each top-level chunk of `file`, checking the RIFF size
    /// and the padding.
    fn fourccs(file: &[u8]) -> Vec<String> {
        let riff = u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff + 8, file.len());
        let mut at = 12;
        let mut out = Vec::new();
        while at < file.len() {
            let size = u32::from_le_bytes(file[at + 4..at + 8].try_into().unwrap()) as usize;
            out.push(name(&file[at..at + 4].try_into().unwrap()));
            at += 8 + size + (size & 1);
        }
        assert_eq!(at, file.len());
        out
    }

    /// `headers::webp_layout` of `file`, as the decode's memory check sees
    /// it.
    fn layout(file: &[u8]) -> crate::headers::WebpLayout {
        let path = std::env::temp_dir().join(format!(
            "grape-webp-unit-{}.webp",
            crate::output::unique_suffix()
        ));
        std::fs::write(&path, file).unwrap();
        let layout = crate::headers::webp_layout(&path);
        std::fs::remove_file(&path).unwrap();
        layout
    }

    #[test]
    fn assemble_puts_the_metadata_around_libwebps_chunks() {
        use crate::headers::WebpLayout;
        let carried = Carried {
            flags: 0x2C,
            iccp: Some(b"odd profile".to_vec()),
            exif: Some(b"Exif\0\0MM\0*".to_vec()),
            xmp: Some(b"<x/>".to_vec()),
        };
        // libwebp's lossy file with alpha: VP8X, ALPH, VP8.
        let lossy = webp(&[
            canvas(0x10, 300, 200),
            chunk(b"ALPH", b"alp"),
            vp8(300, 200),
        ]);
        let out = assemble(lossy, 300, 200, &carried).unwrap();
        assert_eq!(
            fourccs(&out),
            ["VP8X", "ICCP", "ALPH", "VP8", "EXIF", "XMP"]
        );
        assert_eq!(&out[20..30], &[0x3C, 0, 0, 0, 43, 1, 0, 199, 0, 0]);
        assert_eq!(layout(&out), WebpLayout::Still { lossy_alpha: true });
        let back = scanned(&out).unwrap();
        assert_eq!(back.carried, carried);
        assert!(back.alpha && !back.lossless);
        assert_eq!(back.size, (300, 200));
        // A lossless file: VP8L's alpha bit sets VP8X's flag.
        for alpha in [true, false] {
            let lossless = webp(&[vp8l(16_383, 1, alpha)]);
            let out = assemble(lossless, 16_383, 1, &carried).unwrap();
            assert_eq!(fourccs(&out), ["VP8X", "ICCP", "VP8L", "EXIF", "XMP"]);
            assert_eq!(out[20] & ALPHA_FLAG != 0, alpha);
            assert_eq!(&out[24..30], &[0xFE, 0x3F, 0, 0, 0, 0]);
            assert_eq!(layout(&out), WebpLayout::Still { lossy_alpha: false });
        }
        // The source's flags as they were: an XMP chunk without its flag.
        let quiet = Carried {
            flags: 0x20,
            iccp: None,
            exif: None,
            xmp: Some(b"x".to_vec()),
        };
        let out = assemble(webp(&[vp8(9, 9)]), 9, 9, &quiet).unwrap();
        assert_eq!(fourccs(&out), ["VP8X", "VP8", "XMP"]);
        assert_eq!(out[20], 0);
        // Nothing to carry: libwebp's file as it is.
        let plain = webp(&[vp8(9, 9)]);
        assert_eq!(
            assemble(plain.clone(), 9, 9, &Carried::default()).unwrap(),
            plain
        );
    }

    #[test]
    fn assemble_refuses_what_libwebp_wouldnt_write() {
        let carried = Carried {
            xmp: Some(b"x".to_vec()),
            ..Carried::default()
        };
        for bad in [
            webp(&[chunk(b"ICCP", b"p"), vp8(9, 9)]),
            webp(&[vp8(9, 9), vp8(9, 9)]),
            webp(&[chunk(b"ALPH", b"a")]),
            webp(&[vp8(9, 9)])[..20].to_vec(),
            b"RIFF".to_vec(),
        ] {
            assert!(
                matches!(assemble(bad.clone(), 9, 9, &carried), Err(Error::Broken(_))),
                "{bad:?}"
            );
        }
        assert!(assemble(webp(&[vp8(9, 9)]), 0, 9, &carried).is_err());
    }
}
