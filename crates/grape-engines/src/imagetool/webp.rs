//! Remove Metadata on WebP (D26): the source is walked first as every
//! conversion walks it ([`Plan::new`]: libwebp's demuxer's reading), and
//! what that refuses is refused; then its image chunks are copied as they
//! are, padding included (a last chunk's missing pad byte is written, as
//! libwebp refuses a file without it), the rest dropped and said what they
//! were, and the file written is walked again before it's published
//! ([`Plan::check`]).
//!
//! - **Kept:** in an extended file, `VP8X` (the first chunk's; any other
//!   goes: libwebp's still decoder and image-webp pass over it, its
//!   demuxer refuses the file, and its flags would be patched instead of
//!   the ones readers go by), `ALPH`, `VP8 `, `VP8L`, `ANIM` and `ICCP`,
//!   and each `ANMF` frame that has an image, with its header and its
//!   `ALPH` and `VP8 ` or `VP8L`; an animation's first `ANIM` as the 6
//!   bytes the demuxer reads of it, which image-webp reads at that size
//!   only; in a simple file, its image, and the `ALPH` chunk after it if
//!   there is one, which the demuxer takes for the image's frame.
//! - **Dropped:** `EXIF` ([`exif::walk`], after an optional `Exif\0\0`),
//!   `XMP `, unknown chunks, an `ANMF` with no image (the demuxer passes
//!   over it, and refuses a file that ends with an empty one), what the
//!   demuxer reads as top-level chunks after a frame's image inside its
//!   `ANMF` (an `ANMF` is walked by its size padded, as the demuxer walks
//!   it), everything in a simple file from the chunk that ends its image's
//!   frame on, which no reader reads (whole chunks said by their kind, the
//!   rest as other metadata), and anything after the RIFF's end.
//! - **Refused:** what a conversion refuses before decoding ([`reading`]),
//!   and an `ANMF` or `ICCP` chunk the demuxer reads at the top level
//!   inside an `ANMF`, after its frame: the `image` crate trusts the
//!   `ANMF`'s size and reads neither, where the demuxer shows the one as a
//!   frame and may apply the other, so readers disagree on the file. (An
//!   `ANIM` there, a second one, both pass over: it goes.)
//! - **Patched afterwards:** the RIFF size, each frame's size, and VP8X's
//!   flags: XMP off, and EXIF on only when the minimal EXIF
//!   ([`exif::minimal`]) went back where the old one was. Without a VP8X
//!   (a simple WebP) there's no flag to set, so none goes back.
//! - **Checked:** the file written must be walked as the source was, with
//!   the same frames, image and canvas, or nothing is published: Grape
//!   never writes a WebP it would refuse itself.
//!
//! Compress on WebP ([`super::webp_compress`], D28) reads a file's chunks
//! with [`scan`] and puts the source's metadata into libwebp's file with
//! [`assemble`].

use std::io::{BufRead, Cursor, SeekFrom};
use std::sync::Arc;

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
const VP8X_FLAGS_AT: u64 = 20;

/// The largest `EXIF` or `XMP ` chunk Compress carries (D28). Compress
/// keeps metadata, so a file with a larger one is refused rather than
/// written without it.
const MAX_CARRIED: u64 = 16 << 20;

pub(super) fn sniff(head: &[u8]) -> bool {
    head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP"
}

/// Remove Metadata on a WebP ([module docs](self)): what the walk every
/// conversion reads the source with found ([`reading`]), which the file
/// written is checked against.
#[derive(Debug)]
pub(super) struct Plan {
    scan: Scan,
    reading: Reading,
}

impl Plan {
    /// Walks the WebP `r` as a conversion does before decoding it: what
    /// that refuses (readers disagree on it, or libwebp's demuxer refuses
    /// it) is [`Error::Broken`], before anything is written.
    pub(super) fn new(r: &mut dyn In) -> Result<Plan, Error> {
        let (scan, reading) = walked(r)?;
        Ok(Plan { scan, reading })
    }

    /// Whether a conversion decides on the source only once it's decoded:
    /// a `VP8L` whose own alpha bit says none under VP8X's alpha flag,
    /// refused when its pixels hold transparency ([`Reading::opaque`]).
    pub(super) fn decides_by_pixels(&self) -> bool {
        self.reading.opaque
    }

    /// Strips the WebP `r` into `w` ([module docs](self)).
    pub(super) fn write(&self, r: &mut dyn In, w: &mut dyn Out) -> Result<Found, Error> {
        let mut found = Found::default();
        let mut header = [0; 12];
        read_at(r, 0, &mut header)?;
        let end = 12 + riff(&header)?;
        // The RIFF must be whole: libwebp's still decoder and its demuxer
        // refuse a file shorter than it says, which the walk doesn't read
        // to its end.
        read_at(r, end - 1, &mut [0])?;
        let mut written = 0u64;
        write(w, &header, &mut written)?;
        let flags = match self.scan.vp8x {
            None => {
                simple(r, w, end, &mut found, &mut written)?;
                None
            }
            Some(vp8x) => Some(extended(r, w, end, vp8x.canvas, &mut found, &mut written)?),
        };
        // `r` is at the RIFF's end.
        if more_after(r, &[0])? {
            found.fields.insert(MetaField::Other);
        }
        // The RIFF's size, and VP8X's flags.
        let size = u32::try_from(written - 8).map_err(|_| Error::Broken("too big".into()))?;
        seek_write(w, 4, &size.to_le_bytes())?;
        if let Some(Flags { at, flags, exif }) = flags {
            let mut new = flags & !XMP_FLAG & !EXIF_FLAG;
            if exif {
                new |= EXIF_FLAG;
            }
            seek_write(w, at, &[new])?;
        }
        w.seek(SeekFrom::End(0)).map_err(Error::Write)?;
        Ok(found)
    }

    /// Walks the file Remove Metadata wrote, `r`, as the source was walked
    /// ([`Plan::new`]): it must be read, still or animated as the source
    /// is, with as many frames, the same image (its size, kind and alpha)
    /// and the same canvas and flags but EXIF's and XMP's. Anything else
    /// is [`Error::Broken`], saying what differs.
    pub(super) fn check(&self, r: &mut dyn In) -> Result<(), Error> {
        let (scan, reading) =
            walked(r).map_err(|e| Error::Broken(format!("the file written: {}", reason(e))))?;
        // What a reader shows: the metadata's flags aside.
        let view = |scan: &Scan, reading: &Reading| {
            let vp8x = scan.vp8x.map(|vp8x| Vp8x {
                flags: vp8x.flags & !(EXIF_FLAG | XMP_FLAG),
                ..vp8x
            });
            let image = (scan.size, scan.lossless, scan.alpha, scan.late_alpha);
            (vp8x, image, scan.animated, scan.frames, reading.opaque)
        };
        if view(&scan, &reading) != view(&self.scan, &self.reading) {
            return Err(Error::Broken(format!(
                "{scan:?} written for {:?}",
                self.scan
            )));
        }
        Ok(())
    }
}

/// What an error says, for a message.
fn reason(e: Error) -> String {
    match e {
        Error::Broken(why) | Error::TooLarge(why) | Error::Refused(why) => why,
        Error::Read(e) | Error::Write(e) => e.to_string(),
    }
}

/// Where an extended file's VP8X flags were written, what they were, and
/// whether a minimal EXIF went back ([`extended`]).
struct Flags {
    at: u64,
    flags: u8,
    exif: bool,
}

/// Copies a simple file's image chunk, from `r`'s 12th byte, and the
/// `ALPH` chunk after it if there is one (libwebp's demuxer takes it for
/// the image's frame and reads on: [`after_simple_image`]), into `w`, and
/// drops the rest of the RIFF, to its `end`, from the chunk that ends that
/// frame on: no reader reads it ([`drop_rest`]).
fn simple(
    r: &mut dyn In,
    w: &mut dyn Out,
    end: u64,
    found: &mut Found,
    written: &mut u64,
) -> Result<(), Error> {
    let mut at = 12;
    let mut chunks = 0;
    for first in [true, false] {
        if at == end {
            break;
        }
        let mut raw = [0; 8];
        read_at(r, at, &mut raw)?;
        let head = next(raw, &mut (end - at), &mut chunks)?;
        if !first && &head.fourcc != b"ALPH" {
            break;
        }
        copy_chunk(r, w, at, &head, written)?;
        at += 8 + head.body;
    }
    drop_rest(r, at, end, found)
}

/// Drops the bytes of `r` from `at` to `end`, which no reader reads, saying
/// what they held: each whole chunk by its kind (an EXIF walked, an XMP; a
/// `VP8X` holds none), and whatever isn't one, or follows what isn't, as
/// other metadata. Leaves `r` at `end`.
fn drop_rest(r: &mut dyn In, mut at: u64, end: u64, found: &mut Found) -> Result<(), Error> {
    let mut chunks = 0;
    while at < end {
        let mut raw = [0; 8];
        let head = if end - at < 8 {
            None
        } else {
            read_at(r, at, &mut raw)?;
            next(raw, &mut (end - at), &mut chunks).ok()
        };
        let Some(head) = head else {
            found.fields.insert(MetaField::Other);
            break;
        };
        match &head.fourcc {
            b"EXIF" => {
                read_exif(r, head.size, found)?;
            }
            b"XMP " => {
                found.fields.insert(MetaField::Xmp);
            }
            b"VP8X" => {}
            _ => {
                found.fields.insert(MetaField::Other);
            }
        }
        at += 8 + head.body;
    }
    read_at(r, end - 1, &mut [0])
}

/// Copies an extended file's kept chunks, from `r`'s 12th byte to the
/// RIFF's `end`, into `w`, walking them as the shared walk does ([`walk`],
/// [`animation`]): each frame from its `ANMF` header to its image, written
/// as an `ANMF` of its own, and what the demuxer reads after it inside the
/// `ANMF` as the top-level chunks it reads them as. `canvas` is VP8X's.
/// Says where VP8X's flags went ([`Flags`]).
fn extended(
    r: &mut dyn In,
    w: &mut dyn Out,
    end: u64,
    canvas: (u32, u32),
    found: &mut Found,
    written: &mut u64,
) -> Result<Flags, Error> {
    let mut flags = Flags {
        at: 0,
        flags: 0,
        exif: false,
    };
    let mut at = 12;
    let mut chunks = 0;
    let mut frames = 0;
    let mut seen_exif = false;
    let mut seen_anim = false;
    // The end of the last `ANMF` read: a chunk that starts before it, the
    // demuxer reads at the top level, and the `image` crate as the frame's.
    let mut frame_end = 0;
    while at < end {
        let mut raw = [0; 8];
        read_at(r, at, &mut raw)?;
        let head = next(raw, &mut (end - at), &mut chunks)?;
        let body = at + 8;
        let mut then = body + head.body;
        let in_frame = at < frame_end;
        match &head.fourcc {
            b"VP8X" if chunks == 1 => {
                if head.size < 10 {
                    return Err(Error::Broken("a short VP8X".into()));
                }
                write(w, &raw, written)?;
                flags.at = *written;
                // The format's 10 bytes; anything a longer chunk claims is
                // copied as it comes, never held.
                let mut data = [0; 10];
                read_exact(r, &mut data)?;
                flags.flags = data[0];
                write(w, &data, written)?;
                copy(r, w, head.body - 10, written)?;
                pad(w, head.size, head.body, written)?;
            }
            // Not the first chunk: readers go by the first. Nor a second
            // `ANIM` inside a frame, which the demuxer passes over and the
            // `image` crate never reads.
            b"VP8X" => {}
            b"ANIM" if in_frame => {}
            // A frame the demuxer shows and the `image` crate doesn't, or
            // a profile it may apply: readers disagree on the file.
            b"ANMF" | b"ICCP" if in_frame => {
                return Err(Error::Broken(format!(
                    "a {} chunk inside an ANMF",
                    name(&head.fourcc)
                )));
            }
            b"ANMF" => {
                let frame = animation_frame(r, body, &head, end, canvas, &mut frames)?;
                if let Some((image_at, image)) = &frame.image {
                    write(w, b"ANMF\0\0\0\0", written)?;
                    let size_at = *written - 4;
                    write(w, &frame.header, written)?;
                    if let Some((alpha_at, alpha)) = &frame.alpha {
                        copy_chunk(r, w, *alpha_at, alpha, written)?;
                    }
                    copy_chunk(r, w, *image_at, image, written)?;
                    // Even: its header and whole chunks.
                    let kept = u32::try_from(*written - size_at - 4)
                        .map_err(|_| Error::Broken("a frame too big".into()))?;
                    seek_write(w, size_at, &kept.to_le_bytes())?;
                    w.seek(SeekFrom::End(0)).map_err(Error::Write)?;
                }
                // With no image, the demuxer passes over it (D26: nothing
                // of it is kept), and reads what it holds as top-level
                // chunks, said below.
                frame_end = body + head.body;
                then = frame.resume;
            }
            b"ANIM" if !seen_anim && flags.flags & ANIMATION_FLAG != 0 => {
                // An animation's first, which readers go by, as the demuxer
                // reads it: the first 6 bytes of its size padded (a 5-byte
                // one's pad byte the loop count's high byte), which
                // image-webp reads at that size only.
                let mut data = [0; ANIM_SIZE as usize];
                read_at(r, body, &mut data)?;
                write_chunk(w, b"ANIM", &data, written)?;
                seen_anim = true;
            }
            b"VP8 " | b"VP8L" | b"ALPH" | b"ANIM" | b"ICCP" => {
                copy_chunk(r, w, at, &head, written)?;
            }
            b"EXIF" => {
                let walked = read_exif(r, head.size, found)?;
                // Inside a frame, it's dropped alone.
                if !seen_exif && !in_frame {
                    let needs = exif::minimal(
                        walked.orientation,
                        walked.dcf_adobe_rgb() && flags.flags & ICC_FLAG == 0,
                    );
                    if let Some(tiff) = needs {
                        write_chunk(w, b"EXIF", &tiff, written)?;
                        flags.exif = true;
                    }
                    seen_exif = true;
                }
            }
            b"XMP " => {
                found.fields.insert(MetaField::Xmp);
            }
            _ => {
                found.fields.insert(MetaField::Other);
            }
        }
        at = then;
    }
    read_at(r, end - 1, &mut [0])?;
    Ok(flags)
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
#[derive(Debug, Clone, Copy)]
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

/// Copies the chunk `head` that starts at `at` in `r` into `w`, padded.
fn copy_chunk(
    r: &mut dyn In,
    w: &mut dyn Out,
    at: u64,
    head: &Head,
    written: &mut u64,
) -> Result<(), Error> {
    let mut raw = [0; 8];
    read_at(r, at, &mut raw)?;
    write(w, &raw, written)?;
    copy(r, w, head.body, written)?;
    pad(w, head.size, head.body, written)
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
    /// VP8X's animation flag: its chunks are walked as libwebp's demuxer
    /// walks them ([`animation`]), and nothing is carried.
    pub(super) animated: bool,
    /// An animation's frames, as the demuxer counts them; 0 for a still.
    pub(super) frames: u64,
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
/// - an extended file (`VP8X` first) to the RIFF's end: its image's own
///   chunks ([`Own`]: a `VP8 ` or `VP8L`, and an `ALPH`), and its first
///   `ICCP`, `EXIF` and `XMP `, up to [`crate::image::MAX_ICC`] bytes for
///   the profile and [`MAX_CARRIED`] for each of the others (more is
///   [`Error::TooLarge`]: Compress keeps metadata). An animation (VP8X's
///   flag) is walked by [`animation`], nothing of it carried. In a still
///   file, an `ALPH`, `VP8 ` or `VP8L` past the image's own chunks is
///   broken: libwebp's demuxer (Chrome's, Pillow's) refuses the file, its
///   still decoder shows the first image and image-webp its first `VP8 `
///   or `VP8L` with the first `ALPH` (a second image of the same kind went
///   through; both kinds image-webp refuses). So are an `ANMF` chunk
///   (image-webp took the frame's picture or its `ALPH` for the still
///   one's, where libwebp's still decoder skips it and its demuxer refuses
///   the file, but for an `ANMF` after an `ANIM` after the image); an
///   `ANIM` before the image or of fewer than 6 bytes padded, which the
///   demuxer refuses (an `ANIM` after the image both decoders read past);
///   an `ALPH` before a `VP8L` (libwebp's still decoder then shows alpha,
///   which image-webp reads by VP8X's flag and the `VP8L`'s own bit, and
///   its demuxer refuses the file) or after one VP8X flags alpha for (the
///   demuxer refuses it); and a canvas other than the image's size
///   (libwebp refuses it). Nothing past the RIFF's end is read, by this
///   walk or by the decoder (which `crate::image` ends there: image-webp's
///   own walk would go on, as far again as the RIFF's size less 12 bytes
///   from the end of `VP8X`);
/// - a simple file to its first chunk, `VP8 ` or `VP8L`, and on as far as
///   libwebp's demuxer reads after it ([`after_simple_image`]: 1 to 7
///   bytes left in the RIFF after the image, say, are broken). Nothing
///   after the image is carried: libwebp's still decoder and image-webp
///   read nothing after it.
pub(super) fn scan(r: &mut dyn In) -> Result<Scan, Error> {
    walk(r, true)
}

/// [`scan`], carrying the metadata only if `carry`: a decode reads none of
/// it, so none of it can be too large for one.
fn walk(r: &mut dyn In, carry: bool) -> Result<Scan, Error> {
    let mut header = [0; 12];
    read_at(r, 0, &mut header)?;
    let mut left = riff(&header)?;
    let mut at = 12;
    let mut chunks = 0;
    let mut found = Scan::default();
    let mut image = None;
    let mut own = Own::Before;
    while left > 0 {
        let mut raw = [0; 8];
        read_at(r, at, &mut raw)?;
        let head = next(raw, &mut left, &mut chunks)?;
        let body = at + 8;
        at = body + head.body;
        let first = chunks == 1;
        if !first {
            own = own.then(&head.fourcc)?;
        }
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
                    let canvas = (side(&data[4..7]), side(&data[7..10]));
                    found.frames = animation(r, at, at + left, canvas, &mut chunks)?;
                    return Ok(found);
                }
            }
            // The image's own: a second is past them ([`Own`]).
            b"VP8 " | b"VP8L" => {
                if found.alpha && &head.fourcc == b"VP8L" {
                    // libwebp's still decoder shows alpha whenever there's
                    // one, and its demuxer refuses it.
                    return Err(Error::Broken("an ALPH chunk before its VP8L".into()));
                }
                image = Some(head.fourcc);
                bitstream(r, body, &head, &mut found)?;
                if first {
                    after_simple_image(r, at, left)?;
                    break;
                }
            }
            _ if first => {
                return Err(Error::Broken(format!(
                    "a {} chunk first",
                    name(&head.fourcc)
                )));
            }
            // The image's own: one before it, or one after it ([`Own`]).
            b"ALPH" => match image {
                None => found.alpha = true,
                Some(fourcc) if &fourcc == b"VP8L" && flags_alpha(&found) => {
                    // libwebp's demuxer wants a flagged one before its
                    // image, and a VP8L has its own.
                    return Err(Error::Broken("an ALPH chunk after its VP8L".into()));
                }
                Some(_) => found.late_alpha = true,
            },
            // After the image, in a still file, libwebp's still decoder and
            // its demuxer both read past it, which then refuses any image
            // chunk after it ([`Own`]). The demuxer goes by its size
            // padded.
            b"ANIM" if image.is_some() && head.body >= ANIM_SIZE => {}
            b"ANIM" if image.is_some() => {
                return Err(Error::Broken(format!(
                    "an ANIM chunk of {} bytes",
                    head.size
                )));
            }
            b"ANIM" => {
                // The still decoder skips it; the demuxer refuses the image
                // after it.
                return Err(Error::Broken(
                    "an ANIM chunk before the image in a still file".into(),
                ));
            }
            b"ANMF" => {
                // image-webp takes its picture or its alpha for the still
                // one's, which libwebp's decoders never do.
                return Err(Error::Broken("an ANMF chunk in a still file".into()));
            }
            b"ICCP" if carry => {
                take(r, body, &head, MAX_ICC as u64, &mut found.carried.iccp)?;
            }
            b"EXIF" if carry => take(r, body, &head, MAX_CARRIED, &mut found.carried.exif)?,
            b"XMP " if carry => take(r, body, &head, MAX_CARRIED, &mut found.carried.xmp)?,
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
    }
    Ok(found)
}

/// Reads what follows a simple file's image, `left` bytes of the RIFF from
/// `at`, as libwebp's demuxer (Chrome's, Pillow's) reads it
/// (`StoreFrame`): the RIFF's end, or a whole chunk, which ends the image's
/// frame, but for a first `ALPH`, which it takes for the frame's and reads
/// on after. It refuses the file for 1 to 7 bytes left (too few for a
/// chunk header), a chunk past the RIFF's end and a `VP8L` after that
/// `ALPH`, where its still decoder shows the image, so they're
/// [`Error::Broken`]; it reads nothing after the chunk that ends the frame.
/// A last chunk whose only missing byte is its pad is read, as in an
/// extended file ([`next`]).
fn after_simple_image(r: &mut dyn In, mut at: u64, mut left: u64) -> Result<(), Error> {
    let mut alpha = false;
    let mut chunks = 1;
    loop {
        if left == 0 {
            return Ok(());
        }
        if left < 8 {
            return Err(Error::Broken(format!(
                "{left} bytes after the image, too few for a chunk"
            )));
        }
        let mut raw = [0; 8];
        read_at(r, at, &mut raw)?;
        let head = next(raw, &mut left, &mut chunks)?;
        match &head.fourcc {
            b"ALPH" if !alpha => {
                alpha = true;
                at += 8 + head.body;
            }
            b"VP8L" if alpha => {
                return Err(Error::Broken("a VP8L chunk after an ALPH chunk".into()));
            }
            _ => return Ok(()),
        }
    }
}

/// Where a still file's walk is in its image's own chunks: what libwebp's
/// demuxer reads as the image's frame (`StoreFrame`), from the first
/// `ALPH`, `VP8 ` or `VP8L` on, at most one `ALPH` and one image in either
/// order, up to the first chunk of another kind or a second of one. Any
/// `ALPH`, `VP8 ` or `VP8L` past them, it refuses the file for
/// (`ParseSingleImage`), where its still decoder shows the first image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Own {
    Before,
    In { alpha: bool, image: bool },
    Past,
}

impl Own {
    /// Where the walk is once the chunk `fourcc` is read, or
    /// [`Error::Broken`] for an image chunk past the image's own.
    fn then(self, fourcc: &[u8; 4]) -> Result<Own, Error> {
        let is_alph = fourcc == b"ALPH";
        let is_image = matches!(fourcc, b"VP8 " | b"VP8L");
        Ok(match self {
            Own::Before if is_alph || is_image => Own::In {
                alpha: is_alph,
                image: is_image,
            },
            Own::In {
                alpha: false,
                image,
            } if is_alph => Own::In { alpha: true, image },
            Own::In {
                alpha,
                image: false,
            } if is_image => Own::In { alpha, image: true },
            _ if is_alph || is_image => {
                return Err(Error::Broken(format!(
                    "a {} chunk past the image's own",
                    name(fourcc)
                )));
            }
            Own::In { .. } => Own::Past,
            own => own,
        })
    }
}

/// Whether `found`'s VP8X flags alpha.
fn flags_alpha(found: &Scan) -> bool {
    found.vp8x.is_some_and(|vp8x| vp8x.flags & ALPHA_FLAG != 0)
}

/// An `ANIM` chunk's size: libwebp's demuxer refuses one whose size,
/// padded to even, is less (a 5-byte one it reads, its pad byte the
/// loop count's high byte).
const ANIM_SIZE: u64 = 6;

/// The most pixels libwebp's demuxer takes on an animation's canvas or in
/// a frame (`MAX_IMAGE_AREA`).
const MAX_AREA: u64 = 1 << 32;

/// Walks an animation's chunks, from `at` to the RIFF's `end`, as libwebp's
/// demuxer (Chrome's, Pillow's) reads them, and refuses what it refuses
/// (D16): libwebp's still decoder refuses every animation, and image-webp
/// would show a first frame of a file no libwebp reader shows. So a second
/// `VP8X`, an `ALPH`, `VP8 ` or `VP8L` chunk outside a frame, an `ANMF`
/// before the first `ANIM`, an `ANIM` of fewer than [`ANIM_SIZE`] bytes
/// padded, a frame that's broken ([`animation_frame`]) and an animation
/// with no frame are all [`Error::Broken`]; unknown chunks, metadata and a
/// second `ANIM` are passed over. The canvas, `canvas`, must be under
/// [`MAX_AREA`]. Gives the frames it read.
fn animation(
    r: &mut dyn In,
    mut at: u64,
    end: u64,
    canvas: (u32, u32),
    chunks: &mut usize,
) -> Result<u64, Error> {
    if u64::from(canvas.0) * u64::from(canvas.1) >= MAX_AREA {
        let (w, h) = canvas;
        return Err(Error::Broken(format!("a {w}x{h} canvas")));
    }
    let mut anim = false;
    let mut frames = 0u64;
    while at < end {
        let mut raw = [0; 8];
        read_at(r, at, &mut raw)?;
        let head = next(raw, &mut (end - at), chunks)?;
        let body = at + 8;
        at = body + head.body;
        match &head.fourcc {
            // Its size padded, as the demuxer reads it ([`ANIM_SIZE`]).
            b"ANIM" if head.body < ANIM_SIZE => {
                return Err(Error::Broken(format!(
                    "an ANIM chunk of {} bytes",
                    head.size
                )));
            }
            b"ANIM" => anim = true,
            b"ANMF" if !anim => {
                return Err(Error::Broken("an ANMF chunk before the ANIM chunk".into()));
            }
            b"ANMF" => at = animation_frame(r, body, &head, end, canvas, &mut frames)?.resume,
            b"VP8X" | b"ALPH" | b"VP8 " | b"VP8L" => {
                return Err(Error::Broken(format!(
                    "a {} chunk outside the animation's frames",
                    name(&head.fourcc)
                )));
            }
            _ => {}
        }
    }
    if frames == 0 {
        return Err(Error::Broken("an animation with no frame".into()));
    }
    Ok(frames)
}

/// An `ANMF` chunk as libwebp's demuxer reads it ([`animation_frame`]).
struct Frame {
    /// Its 16-byte header.
    header: [u8; FRAME_HEADER as usize],
    /// Its `ALPH` chunk, if it has one: where it starts, and its header.
    alpha: Option<(u64, Head)>,
    /// Its image chunk, if it has one: where it starts, and its header.
    image: Option<(u64, Head)>,
    /// Where the demuxer reads on, as top-level chunks: after the image,
    /// or after the header when there's none.
    resume: u64,
}

/// Reads the `ANMF` chunk `head` (its body at `at`) of an animation on a
/// `canvas`, the RIFF ending at `end`, as libwebp's demuxer does: after
/// the 16-byte frame header, its frame is an optional `ALPH` and a `VP8 `
/// or `VP8L`, and the first other chunk ends it. Gives where the demuxer
/// reads on, as top-level chunks: after the frame's image, inside the
/// `ANMF` if anything follows it there. With no image chunk it isn't a
/// frame; with fewer than 8 bytes after its header to the RIFF's end (an
/// empty last frame too: the demuxer wants a chunk header there), an
/// `ALPH` and no image, an `ALPH` after the image or before a `VP8L`, an
/// image past the `ANMF`'s end, an image whose bitstream has no header, or
/// a frame past the canvas, it's [`Error::Broken`]. Counts the frames in
/// `frames`, and says where the frame's chunks are ([`Frame`]).
fn animation_frame(
    r: &mut dyn In,
    at: u64,
    head: &Head,
    end: u64,
    canvas: (u32, u32),
    frames: &mut u64,
) -> Result<Frame, Error> {
    // Its size padded, as the demuxer reads it: a 15-byte one's pad byte
    // is its header's last.
    if head.body < FRAME_HEADER {
        return Err(Error::Broken("a short ANMF".into()));
    }
    let mut header = [0; FRAME_HEADER as usize];
    read_at(r, at, &mut header)?;
    let u24 = |b: &[u8]| u64::from(u32::from_le_bytes([b[0], b[1], b[2], 0]));
    let (x, y) = (2 * u24(&header[0..3]), 2 * u24(&header[3..6]));
    if (u24(&header[6..9]) + 1) * (u24(&header[9..12]) + 1) >= MAX_AREA {
        return Err(Error::Broken("a frame too large".into()));
    }
    let mut p = at + FRAME_HEADER;
    if end.saturating_sub(p) < 8 {
        // The demuxer wants a chunk header after the frame's, even for an
        // empty frame ("more data" in a whole file: refused).
        return Err(Error::Broken(
            "an ANMF frame with too few bytes after its header to the RIFF's end".into(),
        ));
    }
    let (mut alpha, mut image) = (None, None);
    while p < end {
        let mut raw = [0; 8];
        read_at(r, p, &mut raw)?;
        let mut left = end - p;
        let mut counted = 0;
        let chunk = next(raw, &mut left, &mut counted)?;
        match &chunk.fourcc {
            b"ALPH" if alpha.is_none() => alpha = Some((p, chunk)),
            b"VP8L" if alpha.is_some() => {
                return Err(Error::Broken("an ALPH chunk before a frame's VP8L".into()));
            }
            b"VP8 " | b"VP8L" if image.is_none() => {
                let mut bitstream_of = Scan::default();
                bitstream(r, p + 8, &chunk, &mut bitstream_of)?;
                image = Some((p, chunk, bitstream_of.size));
            }
            _ => break,
        }
        p += 8 + chunk.body;
    }
    if p > at + head.body {
        return Err(Error::Broken("a frame's image past its ANMF's end".into()));
    }
    match (alpha, image) {
        (None, None) => {}
        (Some(_), None) => {
            return Err(Error::Broken("a frame with alpha and no image".into()));
        }
        (Some((a, _)), Some((i, _, _))) if a > i => {
            return Err(Error::Broken("a frame's ALPH chunk after its image".into()));
        }
        (_, Some((_, _, (w, h)))) => {
            if x + u64::from(w) > u64::from(canvas.0) || y + u64::from(h) > u64::from(canvas.1) {
                return Err(Error::Broken(format!(
                    "a {w}x{h} frame at ({x}, {y}) past a {}x{} canvas",
                    canvas.0, canvas.1
                )));
            }
            *frames += 1;
        }
    }
    Ok(Frame {
        header,
        alpha,
        image: image.map(|(at, head, _)| (at, head)),
        resume: p,
    })
}

/// How image-webp, which goes by a file's VP8X flags, must read a still
/// WebP to show the picture libwebp shows ([`reading`]), for every decode
/// of one: conversions and Compress alike (D16, D28).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reading {
    /// The byte to read the file's VP8X flags as, and where they are, when
    /// they aren't the file's own ([`decode_flags`]).
    pub(crate) patch: Option<(u64, u8)>,
    /// VP8X flags alpha over a `VP8L` whose own bit says none: libwebp
    /// shows the picture opaque, and image-webp (by the flag) with the
    /// alpha its pixels hold, so a picture whose pixels hold some is shown
    /// differently by each and must be refused once decoded.
    pub(crate) opaque: bool,
}

/// Walks the WebP `r` as [`scan`] does, its metadata unread, and says how
/// a decode must read it ([`Reading`]). What the scan refuses is
/// [`Error::Broken`], and so is a file whose alpha libwebp and image-webp
/// read differently ([`alpha_disagrees`]): no decode could show what both
/// show. A decode of an animation takes its first frame, as image-webp
/// decodes it: its alpha isn't compared.
pub(crate) fn reading(r: &mut dyn In) -> Result<Reading, Error> {
    walked(r).map(|(_, reading)| reading)
}

/// [`reading`], with what the walk found.
fn walked(r: &mut dyn In) -> Result<(Scan, Reading), Error> {
    let scan = walk(r, false)?;
    if let Some(why) = alpha_disagrees(&scan) {
        return Err(Error::Broken(why.into()));
    }
    let flagged = scan.vp8x.is_some_and(|vp8x| vp8x.flags & ALPHA_FLAG != 0);
    let reading = Reading {
        patch: decode_flags(&scan).map(|flags| (VP8X_FLAGS_AT, flags)),
        opaque: !scan.animated && scan.lossless && flagged && !scan.alpha,
    };
    Ok((scan, reading))
}

/// Why `scan`'s alpha reads differently in libwebp (by the chunks) and
/// image-webp (by VP8X's flag, then the first `ALPH` in the RIFF), if it
/// does: a decode would show one of them something else. A `VP8L`'s alpha
/// bit under no flag doesn't: libwebp's still decoder and its demuxer both
/// show its alpha, and image-webp does with the flag on ([`decode_flags`]).
fn alpha_disagrees(scan: &Scan) -> Option<&'static str> {
    if scan.animated {
        return None;
    }
    let flagged = scan.vp8x?.flags & ALPHA_FLAG != 0;
    if !flagged && scan.alpha && !scan.lossless {
        // libwebp's still decoder applies it, its demuxer (Pillow's,
        // Chrome's) drops it.
        Some("VP8X says no alpha, and its VP8 has an ALPH chunk")
    } else if flagged && !scan.lossless && !scan.alpha && scan.late_alpha {
        Some("its only ALPH chunk comes after its VP8, where libwebp never reads it")
    } else {
        None
    }
}

/// VP8X's flags as image-webp should read `scan`'s file to see what
/// libwebp does, when they aren't the file's own: the EXIF and XMP flags
/// off, which change no pixel; for a still lossy image with no `ALPH`
/// chunk before it, alpha off (one only after it, with the flag, is
/// refused: [`alpha_disagrees`]); and for a still `VP8L` whose own alpha
/// bit is set, alpha on. image-webp refuses a flag whose chunk is missing
/// ("An expected chunk was missing"), and libwebp, Pillow and ImageMagick
/// read the file; and it goes by the flag alone for a `VP8L`'s alpha,
/// which libwebp's still decoder and its demuxer read by the bit.
fn decode_flags(scan: &Scan) -> Option<u8> {
    let flags = scan.vp8x?.flags;
    let mut view = flags & !(EXIF_FLAG | XMP_FLAG);
    if !scan.animated && !scan.lossless && !scan.alpha {
        view &= !ALPHA_FLAG;
    }
    if !scan.animated && scan.lossless && scan.alpha {
        view |= ALPHA_FLAG;
    }
    (view != flags).then_some(view)
}

/// The WebP `data` as image-webp must read it to show what libwebp shows
/// ([`reading`]): cut at its RIFF's end, where libwebp stops, its VP8X
/// flags patched; any other data as it is. For a decoder that reads from
/// memory, such as resvg's for an SVG's embedded image (lane A's
/// `svg::resolve`, through `imagetool::webp_readable`, before it admits
/// the data). An error says why it isn't a readable WebP: what a
/// conversion refuses before decoding, and a `VP8L` whose own alpha bit
/// says none under VP8X's alpha flag, whose pixels such a decoder can't be
/// asked to check.
#[allow(dead_code)] // For lane A's `svg::resolve` (D28).
pub(crate) fn readable(data: &Arc<Vec<u8>>) -> Result<Arc<Vec<u8>>, String> {
    let Some(riff) = data.get(4..8).filter(|_| sniff(data)) else {
        return Ok(Arc::clone(data));
    };
    let riff = u32::from_le_bytes([riff[0], riff[1], riff[2], riff[3]]);
    let end = usize::try_from(u64::from(riff) + 8).map_or(data.len(), |end| end.min(data.len()));
    let bytes = data.get(..end).unwrap_or_default();
    let reading = reading(&mut Cursor::new(bytes)).map_err(reason)?;
    if reading.opaque {
        return Err("VP8X says alpha, and its VP8L says none".into());
    }
    if reading.patch.is_none() && end == data.len() {
        return Ok(Arc::clone(data));
    }
    let mut out = bytes.to_vec();
    if let Some((at, byte)) = reading.patch {
        if let Some(slot) = usize::try_from(at).ok().and_then(|at| out.get_mut(at)) {
            *slot = byte;
        }
    }
    Ok(Arc::new(out))
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
    }
    Ok(())
}

/// Reads the chunk `head` (its body at `at`) into `slot`, unless an
/// earlier one is there: at most `max` bytes, or [`Error::TooLarge`].
fn take(
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

    /// Remove Metadata on `input` as `strip::run` runs it: planned, written
    /// and the file written checked against the plan.
    fn run(input: &[u8]) -> Result<(Vec<u8>, BTreeSet<MetaField>), Error> {
        let plan = Plan::new(&mut Cursor::new(input))?;
        let mut out = Cursor::new(Vec::new());
        let found = plan.write(&mut Cursor::new(input), &mut out)?;
        let out = out.into_inner();
        plan.check(&mut Cursor::new(&out))?;
        Ok((out, found.fields))
    }

    /// A `VP8 ` chunk the size of [`vp8x`]'s canvas.
    fn image() -> Vec<u8> {
        vp8(2, 2)
    }

    #[test]
    fn a_plain_webp_comes_out_byte_for_byte() {
        for src in [
            webp(&[image()]),
            webp(&[vp8x(0x10), chunk(b"ALPH", b"al"), image()]),
            webp(&[
                vp8x(0x22),
                chunk(b"ICCP", b"prof"),
                chunk(b"ANIM", &[0; 6]),
                frame(0, 0, &[chunk(b"ALPH", b"a"), image()]),
                frame(0, 0, &[vp8l(2, 2, false)]),
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
            image(),
            chunk(b"EXIF", &[b"Exif\0\0".as_slice(), &camera].concat()),
            chunk(b"XMP ", b"<x:xmpmeta/>"),
            chunk(b"JUNK", b"x"),
        ]);
        let (out, fields) = run(&[src, b"after".to_vec()].concat()).unwrap();
        assert_eq!(fields, [Camera, Xmp, Other].into());
        let minimal = exif::minimal(Some(3), false).unwrap();
        assert_eq!(out, webp(&[vp8x(0x08), image(), chunk(b"EXIF", &minimal)]));
        // Upright: the EXIF flag goes too.
        let t = tiff(b'I', &[&[(0x010F, V::Ascii("Nikon"))]], None);
        let src = webp(&[vp8x(0x08), image(), chunk(b"EXIF", &t)]);
        let (out, _) = run(&src).unwrap();
        assert_eq!(out, webp(&[vp8x(0x00), image()]));
    }

    #[test]
    fn only_the_first_chunks_vp8x_is_kept_and_patched() {
        use MetaField::*;
        // A second VP8X, which libwebp's still decoder and image-webp pass
        // over and its demuxer refuses: its flags were patched, and the
        // first's (the one readers go by) still flagged the XMP that went.
        let six = tiff(b'I', &[&[(0x0112, V::Short(6))]], None);
        let minimal = exif::minimal(Some(6), false).unwrap();
        let src = webp(&[
            vp8x(0x0C),
            image(),
            vp8x(0x00),
            chunk(b"EXIF", &six),
            chunk(b"XMP ", b"<x:xmpmeta/>"),
        ]);
        let (out, fields) = run(&src).unwrap();
        assert_eq!(out, webp(&[vp8x(0x08), image(), chunk(b"EXIF", &minimal)]));
        assert_eq!(fields, [Xmp].into());
        // Before the image too, and after a simple file's image.
        let src = webp(&[vp8x(0x0C), vp8x(0x10), image()]);
        let (out, _) = run(&src).unwrap();
        assert_eq!(out, webp(&[vp8x(0x00), image()]));
        let src = webp(&[image(), vp8x(0x10)]);
        let (out, _) = run(&src).unwrap();
        assert_eq!(out, webp(&[image()]));
    }

    /// An animation on [`vp8x`]'s canvas: VP8X flagging `flags` and an
    /// animation, an ANIM, then `chunks`.
    fn animation(flags: u8, chunks: &[Vec<u8>]) -> Vec<u8> {
        webp(&[&[vp8x(0x02 | flags), chunk(b"ANIM", &[0; 6])], chunks].concat())
    }

    #[test]
    fn chunks_inside_a_frame_go_too() {
        use MetaField::*;
        let src = animation(
            0,
            &[
                frame(
                    0,
                    0,
                    &[
                        vp8l(2, 2, false),
                        chunk(b"XMP ", b"<x:xmpmeta>Zoe</x:xmpmeta>"),
                        chunk(b"ABCD", b"GPS"),
                    ],
                ),
                frame(0, 0, &[chunk(b"ALPH", b"al"), image()]),
            ],
        );
        let (out, fields) = run(&src).unwrap();
        assert_eq!(fields, [Xmp, Other].into());
        assert_eq!(
            out,
            animation(
                0,
                &[
                    frame(0, 0, &[vp8l(2, 2, false)]),
                    frame(0, 0, &[chunk(b"ALPH", b"al"), image()]),
                ]
            )
        );
        // A frame too short for its header, or with a chunk past its end,
        // which the demuxer refuses as conversions do.
        for bad in [
            animation(0, &[chunk(b"ANMF", &[0; 14]), frame(0, 0, &[image()])]),
            animation(
                0,
                &[chunk(
                    b"ANMF",
                    &[[0; 16].as_slice(), b"VP8 \x09\0\0\0ab"].concat(),
                )],
            ),
        ] {
            assert!(matches!(run(&bad), Err(Error::Broken(_))), "{bad:?}");
        }
    }

    #[test]
    fn a_frame_with_no_image_goes_whole() {
        use MetaField::*;
        let red = || frame(0, 0, &[image()]);
        let blue = || frame(0, 0, &[vp8l(2, 2, false)]);
        let camera = tiff(b'I', &[&[(0x010F, V::Ascii("Nikon"))]], None);
        // The review's: a last ANMF holding only an unknown chunk, an EXIF,
        // an empty chunk, or nothing but a header followed by chunks that
        // go. Kept as it was, empty, it ended the file, which libwebp's
        // demuxer, Pillow and every conversion refuse; the demuxer passes
        // over such a frame, so it goes whole, and what it held is said.
        for (tail, said) in [
            (
                vec![chunk(
                    b"ANMF",
                    &[[0; 16].as_slice(), b"JUNK\x03\0\0\0abc\0"].concat(),
                )],
                vec![Other],
            ),
            (vec![chunk(b"ANMF", &[0; 24])], vec![Other]),
            (vec![chunk(b"ANMF", &[0; 32])], vec![Other]),
            (
                vec![chunk(
                    b"ANMF",
                    &[[0; 16].as_slice(), &chunk(b"EXIF", &camera)].concat(),
                )],
                vec![Camera],
            ),
            (
                vec![chunk(b"ANMF", &[0; 15]), chunk(b"JUNK", b"abc")],
                vec![Other],
            ),
            (
                vec![chunk(b"ANMF", &[0; 16]), chunk(b"JUNK", b"abc")],
                vec![Other],
            ),
            (
                vec![chunk(b"ANMF", &[0; 24]), chunk(b"JUNK", b"abc")],
                vec![Other],
            ),
            (
                vec![chunk(b"ANMF", &[0; 16]), chunk(b"EXIF", &camera)],
                vec![Camera],
            ),
            (
                vec![chunk(b"ANMF", &[0; 16]), chunk(b"XMP ", b"<x/>")],
                vec![Xmp],
            ),
            // Before a frame too, and an empty one alone, which held nothing.
            (vec![chunk(b"ANMF", &[0; 40]), blue()], vec![Other]),
            (vec![chunk(b"ANMF", &[0; 16]), blue()], vec![]),
        ] {
            let src = animation(0, &[&[red()], tail.as_slice()].concat());
            let (out, fields) = run(&src).unwrap_or_else(|e| panic!("{src:?}: {e:?}"));
            let frames = if tail.len() == 2 && tail[1] == blue() {
                vec![red(), blue()]
            } else {
                vec![red()]
            };
            assert_eq!(out, animation(0, &frames), "{src:?}");
            assert_eq!(fields, said.into_iter().collect(), "{src:?}");
        }
        // An empty last frame with nothing after it the demuxer refuses,
        // and conversions too, so Remove Metadata does.
        let src = animation(0, &[red(), chunk(b"ANMF", &[0; 16])]);
        assert!(matches!(run(&src), Err(Error::Broken(_))));
    }

    #[test]
    fn an_animations_anim_is_written_as_the_demuxer_reads_it() {
        let red = || frame(0, 0, &[image()]);
        let with = |anim: &[u8]| webp(&[vp8x(0x02), anim.to_vec(), red(), chunk(b"ANIM", &[9; 7])]);
        // The 6 bytes of its size padded the demuxer reads (its pad byte
        // too), which image-webp reads only at 6: one of 5, 7 or 16 bytes
        // (Pillow shows them) made every conversion refuse the file. A
        // second one, which both pass over, goes as it is.
        for (anim, want) in [
            (b"ANIM   ".to_vec(), [1, 2, 3, 4, 5, 7]),
            (chunk(b"ANIM", &[1, 2, 3, 4, 5, 6, 7]), [1, 2, 3, 4, 5, 6]),
            (
                chunk(b"ANIM", &[[1, 2, 3, 4, 5, 6].as_slice(), &[8; 10]].concat()),
                [1, 2, 3, 4, 5, 6],
            ),
            (chunk(b"ANIM", &[1, 2, 3, 4, 5, 6]), [1, 2, 3, 4, 5, 6]),
        ] {
            let (out, fields) = run(&with(&anim)).unwrap();
            assert_eq!(out, with(&chunk(b"ANIM", &want)));
            assert!(fields.is_empty());
        }
    }

    #[test]
    fn a_frame_is_walked_by_its_size_padded() {
        use MetaField::*;
        let red = || frame(0, 0, &[image()]);
        let blue = || frame(0, 0, &[vp8l(2, 2, false)]);
        // An odd ANMF whose pad byte completes a chunk header inside it, as
        // libwebp's demuxer reads it: a 31-byte empty frame, last or before
        // another, and a frame's image and 7 or 15 bytes after it. They
        // were refused as "a chunk cut short in an ANMF".
        let after_image = |n: usize| {
            let f = frame(0, 0, &[image(), vec![0; n]]);
            assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()) % 2, 1);
            f
        };
        for (chunks, want) in [
            (vec![red(), chunk(b"ANMF", &[0; 31])], vec![red()]),
            (
                vec![red(), chunk(b"ANMF", &[0; 31]), blue()],
                vec![red(), blue()],
            ),
            (vec![after_image(7)], vec![red()]),
            (vec![after_image(7), blue()], vec![red(), blue()]),
            (vec![after_image(15)], vec![red()]),
        ] {
            let src = animation(0, &chunks);
            let (out, fields) = run(&src).unwrap_or_else(|e| panic!("{src:?}: {e:?}"));
            assert_eq!(out, animation(0, &want), "{src:?}");
            assert_eq!(fields, [Other].into(), "{src:?}");
        }
    }

    #[test]
    fn a_frame_holding_a_frame_or_a_profile_is_refused() {
        // libwebp ends a frame at its image and reads the rest as top-level
        // chunks, a frame it shows or a profile it may apply, where the
        // `image` crate trusts the size and reads neither: readers disagree.
        for inner in [
            frame(0, 0, &[vp8l(2, 2, false)]),
            chunk(b"ICCP", b"profile"),
        ] {
            let src = animation(0x20, &[frame(0, 0, &[image(), inner])]);
            let got = run(&src);
            assert!(
                matches!(&got, Err(Error::Broken(why)) if why.contains("inside an ANMF")),
                "{got:?}"
            );
        }
        // A VP8X there the demuxer refuses, as conversions do.
        let src = animation(0, &[frame(0, 0, &[image(), vp8x(0x02)])]);
        assert!(matches!(run(&src), Err(Error::Broken(_))));
        // A second ANIM, which both pass over: it goes, unsaid.
        let src = animation(0, &[frame(0, 0, &[image(), chunk(b"ANIM", &[0; 6])])]);
        let (out, fields) = run(&src).unwrap();
        assert_eq!(out, animation(0, &[frame(0, 0, &[image()])]));
        assert!(fields.is_empty());
    }

    #[test]
    fn a_simple_file_keeps_its_images_frame_and_drops_the_rest() {
        use MetaField::*;
        let alph = || chunk(b"ALPH", b"al");
        let raw = |fourcc: &[u8; 4], size: u32, data: &[u8]| {
            [fourcc.as_slice(), &size.to_le_bytes(), data].concat()
        };
        let camera = tiff(b'I', &[&[(0x010F, V::Ascii("Nikon"))]], None);
        // libwebp's demuxer reads a simple file's image, and an ALPH after
        // it, up to the chunk that ends that frame, and nothing after it;
        // its still decoder and image-webp only the image. The review's:
        // an ANMF of 8 to 23 bytes after it (an ALPH between), one holding
        // a frame and 3 stray bytes or an image claiming 100 bytes, and a
        // chunk past the RIFF's end after a whole one. They were refused.
        for (tail, kept, said) in [
            (vec![chunk(b"ANMF", &[0; 8])], vec![], vec![Other]),
            (
                vec![alph(), chunk(b"ANMF", &[0; 17])],
                vec![alph()],
                vec![Other],
            ),
            (vec![chunk(b"ANMF", &[0; 23])], vec![], vec![Other]),
            (
                vec![raw(
                    b"ANMF",
                    16 + 20 + 3,
                    &[[0; 16].as_slice(), &image(), &[0; 3]].concat(),
                )],
                vec![],
                vec![Other],
            ),
            (
                vec![raw(
                    b"ANMF",
                    26,
                    &[[0; 16].as_slice(), &raw(b"VP8 ", 100, &[0; 2])].concat(),
                )],
                vec![],
                vec![Other],
            ),
            (
                vec![chunk(b"JUNK", b"junk"), raw(b"JUNK", 1000, &[0; 4])],
                vec![],
                vec![Other],
            ),
            (
                vec![chunk(b"EXIF", &camera), raw(b"XMP ", 500, &[0; 4])],
                vec![],
                vec![Camera, Other],
            ),
            (
                vec![chunk(b"XMP ", b"<x/>"), vec![0; 3]],
                vec![],
                vec![Xmp, Other],
            ),
            (vec![alph(), image(), alph()], vec![alph()], vec![Other]),
            (vec![vp8x(0x10)], vec![], vec![]),
            (vec![alph()], vec![alph()], vec![]),
        ] {
            let src = webp(&[&[image()], tail.as_slice()].concat());
            let (out, fields) = run(&src).unwrap_or_else(|e| panic!("{src:?}: {e:?}"));
            assert_eq!(
                out,
                webp(&[&[image()], kept.as_slice()].concat()),
                "{src:?}"
            );
            assert_eq!(fields, said.into_iter().collect(), "{src:?}");
        }
    }

    #[test]
    fn what_a_conversion_refuses_is_refused() {
        // Readers disagree on these, or libwebp's demuxer refuses them:
        // Remove Metadata refuses them before writing, as conversions do.
        // It kept their image chunks as they were.
        for bad in [
            webp(&[image(), vec![0; 3]]),
            webp(&[vp8x(0), image(), image()]),
            webp(&[vp8x(0), alph_before(), image()]),
            webp(&[vp8x(0), image(), anmf(&[image()])]),
            webp(&[vp8x(0), chunk(b"ANIM", &[0; 6]), image()]),
            webp(&[canvas(0, 3, 2), image()]),
        ] {
            assert!(matches!(run(&bad), Err(Error::Broken(_))), "{bad:?}");
        }
        // A VP8L whose bit says opaque under VP8X's alpha flag: a
        // conversion decides on it once decoded, and so does `strip::run`.
        let plan = Plan::new(&mut Cursor::new(webp(&[vp8x(0x10), vp8l(2, 2, false)]))).unwrap();
        assert!(plan.decides_by_pixels());
        let plan = Plan::new(&mut Cursor::new(webp(&[vp8x(0x10), vp8l(2, 2, true)]))).unwrap();
        assert!(!plan.decides_by_pixels());
    }

    /// An `ALPH` chunk.
    fn alph_before() -> Vec<u8> {
        chunk(b"ALPH", b"a")
    }

    #[test]
    fn the_file_written_must_be_walked_as_the_source_was() {
        let check = |src: &[u8], out: &[u8]| {
            Plan::new(&mut Cursor::new(src))
                .unwrap()
                .check(&mut Cursor::new(out))
        };
        let red = || frame(0, 0, &[image()]);
        let two = animation(0, &[red(), red()]);
        // As the source, its metadata's flags aside.
        assert!(check(&two, &two).is_ok());
        let tagged = webp(&[vp8x(0x0C), image()]);
        assert!(check(&tagged, &webp(&[vp8x(0x08), image()])).is_ok());
        // Anything a reader would see otherwise is Grape's own failure: a
        // file the walk refuses, a frame lost, another image, another
        // canvas, alpha, or a still for an animation.
        for (src, out) in [
            (
                two.clone(),
                animation(0, &[red(), chunk(b"ANMF", &[0; 16])]),
            ),
            (two.clone(), animation(0, &[red()])),
            (webp(&[image()]), webp(&[vp8(2, 3)])),
            (webp(&[image()]), webp(&[vp8l(2, 2, false)])),
            (tagged.clone(), webp(&[canvas(0x08, 3, 2), vp8(3, 2)])),
            (tagged.clone(), webp(&[vp8x(0x18), alph_before(), image()])),
            (two.clone(), webp(&[vp8x(0), image()])),
        ] {
            let got = check(&src, &out);
            assert!(matches!(got, Err(Error::Broken(_))), "{out:?}: {got:?}");
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
        let src = webp(&[vp8x(0x08), image(), chunk(b"EXIF", &t)]);
        let (out, fields) = run(&src).unwrap();
        // The maker note is the camera's; the cut is Other.
        assert_eq!(fields, [MetaField::Camera, MetaField::Other].into());
        let minimal = exif::minimal(Some(3), false).unwrap();
        assert_eq!(out, webp(&[vp8x(0x08), image(), chunk(b"EXIF", &minimal)]));
    }

    #[test]
    fn data_after_the_riff_is_found_past_the_read_buffer() {
        let mut input = webp(&[image()]);
        input.extend(vec![0; 70_000]);
        input.extend_from_slice(b"more");
        let mut r = std::io::BufReader::with_capacity(8, Cursor::new(input));
        let plan = Plan::new(&mut r).unwrap();
        let found = plan.write(&mut r, &mut Cursor::new(Vec::new())).unwrap();
        assert_eq!(found.fields, [MetaField::Other].into());
    }

    #[test]
    fn broken_webps_are_refused() {
        let good = webp(&[vp8x(0), image()]);
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
        let odd = webp(&[image()]);
        assert_eq!(run(&unpadded(odd.clone())).unwrap().0, odd);
        let odd = webp(&[vp8x(0x10), chunk(b"ALPH", b"a"), image()]);
        assert_eq!(run(&unpadded(odd.clone())).unwrap().0, odd);
        // A last frame whose image lacks its pad, the ANMF's own size odd.
        let header = &frame(0, 0, &[])[8..24];
        let lossless = vp8l(2, 2, false);
        let short = [
            b"ANMF".as_slice(),
            &31u32.to_le_bytes(),
            header,
            &lossless[..15],
        ]
        .concat();
        let (out, _) = run(&animation(0, &[short])).unwrap();
        assert_eq!(out, animation(0, &[frame(0, 0, &[lossless])]));
        for bad in [
            webp(&[chunk(b"XMP ", b"no image")]),
            webp(&[chunk(b"VP8X", &[0; 4]), image()]),
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
            image(),
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

    /// An ANMF frame at (`x`, `y`) of `chunks` (its header's own size 1 × 1:
    /// libwebp's demuxer goes by the image's).
    fn frame(x: u32, y: u32, chunks: &[Vec<u8>]) -> Vec<u8> {
        let u24 = |v: u32| v.to_le_bytes()[..3].to_vec();
        let header = [u24(x / 2), u24(y / 2), u24(0), u24(0), u24(100), vec![0]].concat();
        chunk(b"ANMF", &[header, chunks.concat()].concat())
    }

    #[test]
    fn an_animation_is_walked_as_libwebps_demuxer_walks_it() {
        let anim = || chunk(b"ANIM", &[0; 6]);
        let alph = || chunk(b"ALPH", b"a");
        // As libwebp's demuxer reads them (Pillow, Chrome): metadata and
        // unknown chunks anywhere outside the frames, a second ANIM,
        // frames with alpha, lossless frames, one at an offset, and a
        // chunk after a frame's image inside its ANMF, which the demuxer
        // reads as a top-level chunk.
        for good in [
            webp(&[canvas(0x02, 9, 9), anim(), frame(0, 0, &[vp8(9, 9)])]),
            webp(&[
                canvas(0x3E, 9, 9),
                chunk(b"ICCP", b"p"),
                anim(),
                chunk(b"JUNK", b"j"),
                frame(0, 0, &[alph(), vp8(9, 9)]),
                anim(),
                frame(2, 4, &[vp8l(7, 5, true), chunk(b"XMP ", b"x")]),
                chunk(b"EXIF", b"MM"),
                chunk(b"XMP ", b"<x/>"),
            ]),
            // A frame with no image chunk isn't one; the next is the first.
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[chunk(b"JUNK", b"j")]),
                frame(0, 0, &[vp8(9, 9)]),
            ]),
            // An empty frame with a whole chunk after it, which ends it.
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[vp8(9, 9)]),
                chunk(b"ANMF", &[0; 16]),
                chunk(b"JUNK", b""),
            ]),
            // A 5-byte ANIM and a 15-byte ANMF: the demuxer goes by their
            // sizes padded, 6 and 16.
            webp(&[
                canvas(0x02, 9, 9),
                chunk(b"ANIM", &[0; 5]),
                frame(0, 0, &[vp8(9, 9)]),
            ]),
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                chunk(b"ANMF", &[0; 15]),
                frame(0, 0, &[vp8(9, 9)]),
            ]),
        ] {
            let found = scanned(&good).unwrap_or_else(|e| panic!("{good:?}: {e:?}"));
            assert!(found.animated && found.frames >= 1);
            assert!(!reading(&mut Cursor::new(&good)).unwrap().opaque);
        }
        // What the demuxer refuses, where image-webp would show the first
        // frame (or the file's top-level image).
        for bad in [
            // An image, or its alpha, outside the frames, before or after.
            webp(&[
                canvas(0x02, 9, 9),
                vp8(9, 9),
                anim(),
                frame(0, 0, &[vp8l(9, 9, false)]),
            ]),
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[vp8l(9, 9, false)]),
                vp8(9, 9),
            ]),
            webp(&[
                canvas(0x12, 9, 9),
                anim(),
                frame(0, 0, &[vp8(9, 9)]),
                alph(),
            ]),
            // Two images in a frame: the second is read as top-level.
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[vp8(9, 9), vp8l(9, 9, false)]),
            ]),
            // A chunk before a frame's image ends it there.
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[chunk(b"JUNK", b"j"), vp8(9, 9)]),
            ]),
            // ANMF before ANIM, or no ANIM; a short ANIM; no frame.
            webp(&[canvas(0x02, 9, 9), frame(0, 0, &[vp8(9, 9)]), anim()]),
            webp(&[canvas(0x02, 9, 9), frame(0, 0, &[vp8(9, 9)])]),
            webp(&[
                canvas(0x02, 9, 9),
                chunk(b"ANIM", &[0; 4]),
                frame(0, 0, &[vp8(9, 9)]),
            ]),
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                chunk(b"ANMF", &[0; 14]),
                frame(0, 0, &[vp8(9, 9)]),
            ]),
            webp(&[canvas(0x02, 9, 9), anim()]),
            webp(&[canvas(0x12, 9, 9), chunk(b"EXIF", &[0; 3])]),
            // A last frame with fewer than 8 bytes after its header: the
            // demuxer wants another chunk's header there, even after an
            // empty frame (15 bytes is 16 padded), and Pillow refuses it.
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[vp8(9, 9)]),
                chunk(b"ANMF", &[0; 15]),
            ]),
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[vp8(9, 9)]),
                chunk(b"ANMF", &[0; 16]),
            ]),
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[vp8(9, 9)]),
                chunk(b"ANMF", &[0; 22]),
            ]),
            // A second VP8X.
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[vp8(9, 9)]),
                canvas(0x02, 9, 9),
            ]),
            // Alpha and no image; alpha after the image; alpha before a
            // VP8L, whatever comes first.
            webp(&[
                canvas(0x12, 9, 9),
                anim(),
                frame(0, 0, &[alph()]),
                frame(0, 0, &[vp8(9, 9)]),
            ]),
            webp(&[
                canvas(0x12, 9, 9),
                anim(),
                frame(0, 0, &[vp8(9, 9), alph()]),
            ]),
            webp(&[
                canvas(0x12, 9, 9),
                anim(),
                frame(0, 0, &[alph(), vp8l(9, 9, true)]),
            ]),
            webp(&[
                canvas(0x12, 9, 9),
                anim(),
                frame(0, 0, &[vp8(9, 9), alph(), vp8l(9, 9, true)]),
            ]),
            // A frame past the canvas, by its image's size or its offset.
            webp(&[canvas(0x02, 9, 9), anim(), frame(0, 0, &[vp8(10, 9)])]),
            webp(&[canvas(0x02, 9, 9), anim(), frame(2, 0, &[vp8(8, 9)])]),
            // A frame's image without a header; a canvas the demuxer
            // takes as too large.
            webp(&[
                canvas(0x02, 9, 9),
                anim(),
                frame(0, 0, &[chunk(b"VP8 ", &[0; 10])]),
            ]),
            webp(&[
                canvas(0x02, 1 << 16, 1 << 16),
                anim(),
                frame(0, 0, &[vp8(9, 9)]),
            ]),
        ] {
            assert!(matches!(scanned(&bad), Err(Error::Broken(_))), "{bad:?}");
            assert!(reading(&mut Cursor::new(&bad)).is_err(), "{bad:?}");
        }
        // A frame's image past its ANMF's end: the demuxer reads a frame's
        // chunks on past it, and refuses what it took from there.
        let empty = frame(0, 0, &[]);
        let alpha = frame(0, 0, &[alph()]);
        for frame in [empty, alpha] {
            let file = webp(&[canvas(0x12, 9, 9), anim(), frame, vp8(9, 9)]);
            let found = scanned(&file);
            assert!(
                matches!(&found, Err(Error::Broken(why)) if why.contains("past its ANMF")),
                "{found:?}"
            );
        }
    }

    #[test]
    fn scan_reads_a_simple_file_to_its_image_only() {
        let found = scanned(&webp(&[vp8(300, 200)])).unwrap();
        assert_eq!(
            found,
            Scan {
                size: (300, 200),
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
    fn a_simple_files_image_is_read_on_as_libwebps_demuxer_reads_it() {
        let alph = || chunk(b"ALPH", b"a");
        // The RIFF's end, or a whole chunk after the image, which ends its
        // frame (libwebp's demuxer reads nothing after it, so 3 stray
        // bytes after it go unread); a first ALPH it takes for the frame's
        // and reads on after (Pillow shows the image). A last chunk short
        // of its pad byte alone is read, as in an extended file.
        for good in [
            webp(&[vp8(9, 9), chunk(b"JUNK", b"junk")]),
            webp(&[vp8(9, 9), chunk(b"JUNK", b"")]),
            webp(&[vp8(9, 9), chunk(b"EXIF", b"MM\0*\0\0\0\x08\0\0")]),
            webp(&[vp8(9, 9), chunk(b"XMP ", b"<x/>")]),
            webp(&[vp8(9, 9), chunk(b"ANIM", &[0; 6])]),
            webp(&[vp8(9, 9), vp8x(0x10)]),
            webp(&[vp8(9, 9), vp8(9, 9)]),
            webp(&[vp8(9, 9), alph()]),
            webp(&[vp8(9, 9), alph(), vp8(9, 9)]),
            webp(&[vp8(9, 9), alph(), alph()]),
            webp(&[vp8l(9, 9, true), alph()]),
            webp(&[vp8(9, 9), chunk(b"JUNK", b"junk"), vec![0; 3]]),
            webp(&[vp8(9, 9), b"JUNK\x03\0\0\0abc".to_vec()]),
        ] {
            let found = scanned(&good).unwrap_or_else(|e| panic!("{good:?}: {e:?}"));
            assert_eq!(found.size, (9, 9));
            assert_eq!(found.carried, Carried::default());
            assert!(reading(&mut Cursor::new(&good)).is_ok(), "{good:?}");
        }
        // What the demuxer refuses (Pillow too), where libwebp's still
        // decoder shows the image: 1 to 7 bytes after it, or after its
        // ALPH; a chunk past the RIFF's end; a VP8L after an ALPH.
        let mut bad: Vec<Vec<u8>> = (1..8)
            .flat_map(|n| {
                [
                    webp(&[vp8(9, 9), vec![0; n]]),
                    webp(&[vp8l(9, 9, false), vec![0; n]]),
                    webp(&[vp8(9, 9), alph(), vec![0; n]]),
                ]
            })
            .collect();
        bad.extend([
            webp(&[vp8(9, 9), b"JUNK\x64\0\0\0".to_vec(), vec![0; 10]]),
            webp(&[vp8(9, 9), alph(), vp8l(9, 9, false)]),
            webp(&[vp8l(9, 9, false), alph(), vp8l(9, 9, false)]),
        ]);
        for bad in bad {
            assert!(matches!(scanned(&bad), Err(Error::Broken(_))), "{bad:?}");
            assert!(reading(&mut Cursor::new(&bad)).is_err(), "{bad:?}");
            assert!(readable(&Arc::new(bad.clone())).is_err(), "{bad:?}");
        }
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
                frames: 0,
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
        // Past the RIFF's end, nothing is read: image-webp walked on, as
        // far again as the RIFF's size less 12 bytes from the end of VP8X,
        // till Grape's reader ended the file there for it.
        let file = webp(&[canvas(0x10, 9, 9), vp8(9, 9)]);
        let after = [&file[..], &chunk(b"ALPH", b"al")].concat();
        let found = scanned(&after).unwrap();
        assert!(!found.alpha && !found.late_alpha);
        // A lossless image has no late alpha.
        let lossless = webp(&[canvas(0, 9, 9), vp8l(9, 9, true), chunk(b"ALPH", b"al")]);
        assert!(!scanned(&lossless).unwrap().late_alpha);
    }

    #[test]
    fn a_still_files_image_chunks_are_read_as_libwebps_demuxer_reads_them() {
        let alph = || chunk(b"ALPH", b"a");
        let xmp = || chunk(b"XMP ", b"x");
        // The image's own chunks, as the demuxer reads them: an ALPH and an
        // image in either order, nothing between them; an ALPH after a
        // VP8L only without VP8X's alpha flag (with it, after a VP8, it's
        // refused once read: [`alpha_disagrees`]).
        for good in [
            webp(&[canvas(0x10, 9, 9), alph(), vp8(9, 9), xmp()]),
            webp(&[canvas(0, 9, 9), vp8(9, 9), alph(), xmp()]),
            webp(&[canvas(0x10, 9, 9), vp8(9, 9), alph()]),
            webp(&[canvas(0, 9, 9), vp8l(9, 9, true), alph()]),
        ] {
            scanned(&good).unwrap_or_else(|e| panic!("{good:?}: {e:?}"));
        }
        // Any ALPH, VP8 or VP8L past them: the demuxer (Pillow, Chrome)
        // refuses the file, libwebp's still decoder shows the first image
        // and image-webp its first VP8 or VP8L, with its first ALPH. The
        // review's six, and an ALPH apart from its image.
        for bad in [
            webp(&[canvas(0, 9, 9), vp8(9, 9), vp8(9, 9)]),
            webp(&[canvas(0x04, 9, 9), vp8(9, 9), xmp(), vp8(9, 9)]),
            webp(&[canvas(0, 9, 9), vp8(9, 9), alph(), alph()]),
            webp(&[canvas(0x10, 9, 9), vp8l(9, 9, true), alph()]),
            webp(&[canvas(0, 9, 9), vp8l(9, 9, false), vp8l(9, 9, true)]),
            webp(&[canvas(0x10, 9, 9), alph(), vp8(9, 9), alph()]),
            webp(&[canvas(0x10, 9, 9), alph(), xmp(), vp8(9, 9)]),
            webp(&[canvas(0, 9, 9), vp8(9, 9), xmp(), alph()]),
            webp(&[canvas(0, 9, 9), vp8l(9, 9, false), xmp(), alph()]),
        ] {
            assert!(matches!(scanned(&bad), Err(Error::Broken(_))), "{bad:?}");
            assert!(reading(&mut Cursor::new(&bad)).is_err(), "{bad:?}");
        }
        // A simple file is its first chunk alone, as before.
        let simple = webp(&[vp8(9, 9), vp8(9, 9), alph()]);
        assert_eq!(scanned(&simple).unwrap().size, (9, 9));
    }

    #[test]
    fn scan_stops_at_an_animation() {
        // Its flag set: walked as libwebp's demuxer walks it
        // ([`an_animation_is_walked_as_libwebps_demuxer_walks_it`]), its
        // metadata not carried (Compress refuses it).
        let file = webp(&[
            canvas(0x0A, 9, 9),
            chunk(b"ANIM", &[0; 6]),
            frame(0, 0, &[vp8(9, 9)]),
            chunk(b"EXIF", &[0; 3]),
        ]);
        let found = scanned(&file).unwrap();
        assert!(found.animated && found.carried.exif.is_none(), "{found:?}");
        // Without the flag, an ANMF frame anywhere (image-webp takes its
        // picture or its alpha), an ANIM before the image (libwebp's
        // demuxer refuses the image after it), a short one, and an image
        // chunk after one, which the demuxer refuses too.
        let anim = chunk(b"ANIM", &[0; 6]);
        for file in [
            webp(&[canvas(0, 9, 9), anim.clone(), vp8(9, 9)]),
            webp(&[canvas(0, 9, 9), anmf(&[vp8(9, 9)])]),
            webp(&[canvas(0, 9, 9), vp8(9, 9), anim.clone(), anmf(&[vp8(9, 9)])]),
            webp(&[canvas(0x10, 9, 9), vp8l(9, 9, true), anmf(&[vp8(9, 9)])]),
            webp(&[canvas(0, 9, 9), vp8(9, 9), chunk(b"ANIM", &[0; 4])]),
            webp(&[
                canvas(0x10, 9, 9),
                chunk(b"ALPH", b"a"),
                vp8(9, 9),
                anim.clone(),
                chunk(b"ALPH", b"b"),
            ]),
            webp(&[canvas(0, 9, 9), vp8(9, 9), anim.clone(), vp8(9, 9)]),
        ] {
            assert!(matches!(scanned(&file), Err(Error::Broken(_))), "{file:?}");
        }
        // An ANIM after the image: libwebp's still decoder and its demuxer
        // both show the still picture, a 5-byte one too (6 padded).
        for file in [
            webp(&[canvas(0, 9, 9), vp8(9, 9), anim.clone()]),
            webp(&[canvas(0, 9, 9), vp8(9, 9), chunk(b"ANIM", &[0; 5])]),
            webp(&[
                canvas(0x10, 9, 9),
                vp8l(9, 9, true),
                anim.clone(),
                anim.clone(),
            ]),
        ] {
            let found = scanned(&file).unwrap();
            assert!(!found.animated && found.size == (9, 9), "{file:?}");
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

    #[test]
    fn alpha_libwebp_and_image_webp_read_differently_is_refused() {
        let scan = |flags: u8, lossless: bool, alpha: bool, late_alpha: bool| Scan {
            vp8x: Some(Vp8x {
                flags,
                canvas: (9, 9),
            }),
            lossless,
            size: (9, 9),
            alpha,
            late_alpha,
            ..Scan::default()
        };
        // Agreed: none, or flagged and there.
        assert_eq!(alpha_disagrees(&scan(0, false, false, false)), None);
        assert_eq!(alpha_disagrees(&scan(0x10, false, true, false)), None);
        assert_eq!(alpha_disagrees(&scan(0x10, true, true, false)), None);
        // A late ALPH after one image-webp and libwebp both read.
        assert_eq!(alpha_disagrees(&scan(0x10, false, true, true)), None);
        // An unflagged ALPH chunk, which libwebp's still decoder applies
        // and its demuxer (Pillow's) drops.
        assert!(alpha_disagrees(&scan(0, false, true, false)).is_some());
        // An unflagged VP8L alpha bit: libwebp reads its alpha either way,
        // and image-webp does with the flag turned on ([`decode_flags`]).
        assert_eq!(alpha_disagrees(&scan(0, true, true, false)), None);
        // Only a late ALPH: image-webp reads it, libwebp doesn't.
        assert!(alpha_disagrees(&scan(0x10, false, false, true)).is_some());
        // An animation's alpha isn't compared.
        let animated = Scan {
            animated: true,
            ..scan(0, false, true, false)
        };
        assert_eq!(alpha_disagrees(&animated), None);
        // A simple file has no flag to disagree with.
        let simple = Scan {
            vp8x: None,
            ..scan(0, true, true, false)
        };
        assert_eq!(alpha_disagrees(&simple), None);
    }

    #[test]
    fn image_webp_reads_the_flags_the_chunks_bear_out() {
        let scan = |flags: u8, lossless: bool, alpha: bool| Scan {
            vp8x: Some(Vp8x {
                flags,
                canvas: (9, 9),
            }),
            lossless,
            size: (9, 9),
            alpha,
            ..Scan::default()
        };
        // As they are.
        assert_eq!(decode_flags(&scan(0x30, false, true)), None);
        // An animation's alpha flag stays, its EXIF and XMP flags go.
        let animated = |flags: u8| Scan {
            animated: true,
            ..scan(flags, false, false)
        };
        assert_eq!(decode_flags(&animated(0x12)), None);
        assert_eq!(decode_flags(&animated(0x1E)), Some(0x12));
        assert_eq!(decode_flags(&scan(0x20, false, false)), None);
        assert_eq!(decode_flags(&scan(0x10, true, false)), None);
        assert_eq!(decode_flags(&Scan::default()), None);
        // Alpha with no ALPH chunk before a lossy image; EXIF and XMP.
        assert_eq!(decode_flags(&scan(0x30, false, false)), Some(0x20));
        assert_eq!(decode_flags(&scan(0x3C, false, true)), Some(0x30));
        assert_eq!(decode_flags(&scan(0x1C, true, true)), Some(0x10));
        // A VP8L whose alpha bit VP8X doesn't flag: libwebp shows its
        // alpha, so image-webp reads the flag on. Not in an animation,
        // whose frames aren't read here.
        assert_eq!(decode_flags(&scan(0, true, true)), Some(0x10));
        assert_eq!(decode_flags(&scan(0x28, true, true)), Some(0x30));
        assert_eq!(decode_flags(&scan(0, true, false)), None);
        let animated = Scan {
            animated: true,
            ..scan(0, true, true)
        };
        assert_eq!(decode_flags(&animated), None);
    }

    #[test]
    fn a_decode_reads_a_webp_as_libwebp_does_or_not_at_all() {
        let read = |file: &[u8]| reading(&mut Cursor::new(file));
        // As it is.
        assert_eq!(read(&webp(&[vp8(9, 9)])).unwrap(), Reading::default());
        // Flags patched, at VP8X's flags.
        let tagged = webp(&[canvas(0x18, 9, 9), vp8(9, 9)]);
        assert_eq!(
            read(&tagged).unwrap(),
            Reading {
                patch: Some((20, 0)),
                opaque: false
            }
        );
        assert_eq!(tagged[20], 0x18);
        // VP8X's alpha flag over a VP8L whose bit says none.
        let opaque = read(&webp(&[canvas(0x10, 9, 9), vp8l(9, 9, false)])).unwrap();
        assert!(opaque.opaque && opaque.patch.is_none());
        assert!(
            !read(&webp(&[canvas(0x10, 9, 9), vp8l(9, 9, true)]))
                .unwrap()
                .opaque
        );
        // An animation: its first frame, as it is.
        let anim = webp(&[
            canvas(0x12, 9, 9),
            chunk(b"ANIM", &[0; 6]),
            frame(0, 0, &[vp8(9, 9)]),
        ]);
        assert_eq!(read(&anim).unwrap(), Reading::default());
        // Refused: what the scan refuses, and alpha read differently.
        for bad in [
            webp(&[canvas(0, 9, 9), vp8(9, 9), anmf(&[vp8l(9, 9, false)])]),
            webp(&[canvas(0, 9, 9), anmf(&[vp8l(9, 9, false)]), vp8(9, 9)]),
            webp(&[canvas(0, 9, 9), chunk(b"ALPH", b"a"), vp8l(9, 9, false)]),
            webp(&[canvas(0, 9, 9), chunk(b"ALPH", b"a"), vp8(9, 9)]),
            webp(&[canvas(0x10, 9, 9), vp8(9, 9), chunk(b"ALPH", b"a")]),
        ] {
            assert!(matches!(read(&bad), Err(Error::Broken(_))), "{bad:?}");
        }
        // A VP8L's alpha bit under no flag: read with the flag on, as
        // libwebp and Pillow show it.
        assert_eq!(
            read(&webp(&[canvas(0, 9, 9), vp8l(9, 9, true)])).unwrap(),
            Reading {
                patch: Some((20, 0x10)),
                opaque: false
            }
        );
        // Metadata isn't read, so it's never too large to decode.
        let mut big = webp(&[canvas(0x08, 9, 9), vp8(9, 9)]);
        let size = u32::try_from(MAX_CARRIED + 2).unwrap();
        big.extend_from_slice(b"EXIF");
        big.extend_from_slice(&size.to_le_bytes());
        big.resize(big.len() + size as usize, 0);
        let riff = u32::try_from(big.len() - 8).unwrap();
        big[4..8].copy_from_slice(&riff.to_le_bytes());
        assert!(matches!(scanned(&big), Err(Error::TooLarge(_))));
        assert!(read(&big).is_ok());
    }

    #[test]
    fn readable_gives_a_decoder_from_memory_the_bytes_libwebp_reads() {
        let arc = |v: Vec<u8>| Arc::new(v);
        // Not a WebP, or one read as it is: the same bytes.
        for data in [b"not a webp at all".to_vec(), webp(&[vp8(9, 9)])] {
            let data = arc(data);
            assert!(Arc::ptr_eq(&readable(&data).unwrap(), &data));
        }
        // Cut at the RIFF's end, its flags patched.
        let file = webp(&[canvas(0x08, 9, 9), vp8(9, 9)]);
        let after = arc([file.clone(), chunk(b"EXIF", b"MM\0*")].concat());
        let mut want = file;
        want[20] = 0;
        assert_eq!(*readable(&after).unwrap(), want);
        // Refused: an ANMF frame in a still file, and VP8L's bit against
        // VP8X's flag, whose pixels can't be checked from here.
        for bad in [
            webp(&[canvas(0, 9, 9), vp8(9, 9), anmf(&[vp8l(9, 9, false)])]),
            webp(&[canvas(0x10, 9, 9), vp8l(9, 9, false)]),
        ] {
            assert!(readable(&arc(bad)).is_err());
        }
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
