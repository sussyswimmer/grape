//! What a source's headers say about how much its decoder will hold, read
//! before decoding so the memory estimate in [`crate::image`] can count it.
//!
//! Every reader opens the source read-only and reads only headers (chunk
//! bodies are skipped with seeks). A JPEG's headers are read by zune-jpeg
//! itself, the decoder that will decode it, so nothing here can read them
//! differently. The WebP and PNG walks follow their decoders' own rules for
//! where to look and where to stop.
//! - The JPEG and WebP readers answer with the larger case when the headers
//!   can't be read or look unusual: a wrong guess may refuse an image, never
//!   let one past the limits.
//! - The PNG walk instead stops where the decoder itself will fail, so a
//!   broken file is reported as unreadable, not as too large.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::cancel::Cancellable;
use crate::CancelToken;

/// How a JPEG's frame is coded, as far as the decoder's memory goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JpegLayout {
    /// Color components in the frame (1 gray, 3 YCbCr, 4 CMYK).
    pub(crate) components: u8,
    /// True when zune-jpeg keeps 16-bit coefficients for every component
    /// of the whole image while decoding: a progressive frame, or a
    /// sequential one whose first scan doesn't carry every component.
    /// Otherwise it decodes one row of blocks at a time.
    pub(crate) whole_image: bool,
}

impl JpegLayout {
    /// What to assume when the headers can't be read: the most a JPEG
    /// decoder could keep.
    pub(crate) const WORST: JpegLayout = JpegLayout {
        components: 4,
        whole_image: true,
    };
}

/// The layout of the JPEG at `src`, from its headers as zune-jpeg reads
/// them with the options the `image` crate decodes with, or
/// [`JpegLayout::WORST`] if they can't be read. The read goes through the
/// cancel token: a cancelled job ends it at the next read.
pub(crate) fn jpeg_layout(src: &Path, cancel: &CancelToken) -> JpegLayout {
    let Ok(file) = File::open(src) else {
        return JpegLayout::WORST;
    };
    let mut reader = BufReader::new(Cancellable::new(file, cancel));
    let options = DecoderOptions::default()
        .set_strict_mode(false)
        .set_max_width(usize::MAX)
        .set_max_height(usize::MAX);
    // A decoder panic on a malformed file must not take the worker down.
    let read = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut decoder = JpegDecoder::new_with_options(&mut reader, options);
        decoder.decode_headers().ok()?;
        decoder.info()
    }));
    let Ok(Some(info)) = read else {
        return JpegLayout::WORST;
    };
    let row_by_row =
        !info.sof.is_progressive() && first_scan_components(&mut reader) == Some(info.components);
    JpegLayout {
        components: info.components,
        whole_image: !row_by_row,
    }
}

/// How many components the first scan carries, read back from where
/// zune-jpeg's header decode stopped: right after the first SOS header,
/// whose length it requires to be exactly `6 + 2 × count` bytes. So the
/// real count always matches `[0, 6 + 2 × count, count]` at that distance
/// before the end. `None` unless exactly one count from 1 to 4 does: a
/// split first scan then never passes for a full one.
fn first_scan_components<R: Read + Seek>(r: &mut R) -> Option<u8> {
    let end = r.stream_position().ok()?;
    let mut found = None;
    for count in 1..=4u8 {
        let len = 6 + 2 * count;
        let Some(at) = end.checked_sub(u64::from(len)) else {
            continue;
        };
        let mut bytes = [0u8; 3];
        if r.seek(SeekFrom::Start(at)).is_err() || r.read_exact(&mut bytes).is_err() {
            return None;
        }
        if bytes == [0, len, count] {
            if found.is_some() {
                return None;
            }
            found = Some(count);
        }
    }
    found
}

/// The namespace that starts an extended-XMP segment's data.
const XMP_EXTENSION: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";

/// The most data one JPEG segment holds.
const JPEG_SEGMENT: u64 = 65_533;

/// What a read of a whole JPEG file finds about its metadata, before any
/// decoder sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct JpegScan {
    /// Places the extended-XMP namespace appears: at least as many as the
    /// decoder's extended-XMP segments.
    pub(crate) xmp_segments: u64,
    /// `FF` bytes: at least as many as the markers the decoder's header
    /// parse can meet.
    pub(crate) ff_bytes: u64,
}

impl JpegScan {
    /// What the JPEG decoder keeps besides pixels, for a file of `len`
    /// bytes: the file itself; its copies of the metadata segments it
    /// parses (ICC, EXIF, XMP), which can't exceed the file; and the
    /// extended XMP it reassembles from those copies.
    pub(crate) fn decoder_holds(&self, len: u64) -> u64 {
        let xmp = self.xmp_segments.saturating_mul(JPEG_SEGMENT).min(len);
        len.saturating_mul(2).saturating_add(xmp)
    }

    /// An upper bound on the work of the decoder's header parse: it
    /// re-checks every pending extended-XMP segment after every marker.
    pub(crate) fn header_work(&self) -> u64 {
        self.xmp_segments.saturating_mul(self.ff_bytes)
    }
}

/// Reads the JPEG at `src` once, through the cancel token, and counts what
/// [`JpegScan`] holds.
pub(crate) fn jpeg_scan(src: &Path, cancel: &CancelToken) -> io::Result<JpegScan> {
    let mut reader = Cancellable::new(File::open(src)?, cancel);
    let mut scan = JpegScan::default();
    // The end of the last read, so a namespace split between two reads is
    // found once.
    let keep = XMP_EXTENSION.len() - 1;
    let mut window = vec![0u8; keep + 64 * 1024];
    let mut held = 0;
    loop {
        let n = match reader.read(&mut window[held..]) {
            Ok(0) => return Ok(scan),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let filled = held + n;
        let fresh = &window[held..filled];
        scan.ff_bytes += fresh.iter().filter(|&&b| b == 0xFF).count() as u64;
        scan.xmp_segments += window[..filled]
            .windows(XMP_EXTENSION.len())
            .filter(|w| *w == XMP_EXTENSION)
            .count() as u64;
        held = keep.min(filled);
        window.copy_within(filled - held..filled, 0);
    }
}

/// How a WebP is laid out, as far as the decoder's memory goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebpLayout {
    /// A simple file: one `VP8 ` (lossy) or `VP8L` (lossless) image.
    Simple,
    /// An extended (`VP8X`) still image. `lossy_alpha` when it has alpha and
    /// no `VP8L` chunk, so the decoder decodes a `VP8 ` image and an `ALPH`
    /// chunk; then, too, when that can't be told.
    Still { lossy_alpha: bool },
    /// An extended (`VP8X`) animation.
    Animated,
}

/// Chunks the WebP walk reads before giving up: real files have a handful.
const MAX_WEBP_CHUNKS: u32 = 1 << 16;

/// The layout of the WebP at `src`. A header that can't be read counts as
/// [`WebpLayout::Animated`], whose decoder keeps the most.
pub(crate) fn webp_layout(src: &Path) -> WebpLayout {
    File::open(src)
        .ok()
        .and_then(|file| read_webp_layout(&mut BufReader::new(file)))
        .unwrap_or(WebpLayout::Animated)
}

fn read_webp_layout<R: Read + Seek>(r: &mut BufReader<R>) -> Option<WebpLayout> {
    let mut header = [0u8; 21];
    r.read_exact(&mut header).ok()?;
    if &header[..4] != b"RIFF" || &header[8..12] != b"WEBP" {
        return None;
    }
    match &header[12..16] {
        b"VP8 " | b"VP8L" => return Some(WebpLayout::Simple),
        b"VP8X" => {}
        _ => return None,
    }
    // The flags byte: 0x02 animation, 0x10 alpha.
    let flags = header[20];
    if flags & 0x02 != 0 {
        return Some(WebpLayout::Animated);
    }
    if flags & 0x10 == 0 {
        return Some(WebpLayout::Still { lossy_alpha: false });
    }
    // With alpha, image-webp decodes losslessly when a `VP8L` chunk is
    // anywhere in the range it scans: from the end of `VP8X` to that point
    // plus the RIFF size less 12, or the end of the file.
    let le = |b: &[u8]| u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let rounded = |size: u64| size + (size & 1);
    let mut at = 20 + rounded(le(&header[16..20]));
    let end = at.saturating_add(le(&header[4..8]).saturating_sub(12));
    for _ in 0..MAX_WEBP_CHUNKS {
        if at >= end {
            return Some(WebpLayout::Still { lossy_alpha: true });
        }
        let mut chunk = [0u8; 8];
        if r.seek(SeekFrom::Start(at)).is_err() || r.read_exact(&mut chunk).is_err() {
            return Some(WebpLayout::Still { lossy_alpha: true });
        }
        if &chunk[..4] == b"VP8L" {
            return Some(WebpLayout::Still { lossy_alpha: false });
        }
        at = at
            .saturating_add(8)
            .saturating_add(rounded(le(&chunk[4..])));
    }
    Some(WebpLayout::Still { lossy_alpha: true })
}

/// What [`png_metadata`] found ahead of a PNG's image data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PngMetadata {
    /// About how many bytes the decoder will keep for the text and EXIF
    /// chunks, and the buffer it reads chunks into.
    pub(crate) counted: u64,
    /// True when there is an ICC profile chunk: the decoder inflates and
    /// keeps the profile, held to its own limit.
    pub(crate) icc: bool,
}

/// Chunks ahead of the image data that the PNG walk reads before giving up.
/// Real files have a few dozen.
const MAX_PNG_CHUNKS: u32 = 1 << 20;

/// What the PNG decoder keeps for each text chunk besides its text: the
/// parsed entry, its keyword and their share of the list.
const PNG_TEXT_ENTRY: u64 = 128;

/// What the PNG decoder will keep of the metadata ahead of the first image
/// data, which it reads before any pixel: the buffer it reads chunks into
/// (as large as the largest chunk), each text chunk's entry and text
/// (`tEXt`'s Latin-1 can double in size as UTF-8), two copies of an `eXIf`
/// chunk (the decoder's, and the `image` crate's while it reads the
/// orientation), and whether there is an ICC profile.
///
/// The walk stops where the decoder will fail anyway: no PNG signature, a
/// first chunk other than `IHDR`, a critical chunk it doesn't know (as a
/// run of zeros reads), or a chunk running past the end of the file. So a
/// broken file is reported as unreadable, not as too large. `None` when
/// there are more than [`MAX_PNG_CHUNKS`] chunks ahead of the image data.
pub(crate) fn png_metadata(src: &Path) -> Option<PngMetadata> {
    match File::open(src) {
        Ok(file) => read_png_metadata(&mut BufReader::new(file)),
        // The decoder will fail the same way.
        Err(_) => Some(PngMetadata {
            counted: 0,
            icc: false,
        }),
    }
}

fn read_png_metadata<R: Read + Seek>(r: &mut BufReader<R>) -> Option<PngMetadata> {
    let mut largest = 0u64;
    let mut kept = 0u64;
    let mut icc = false;
    let found = |largest: u64, kept: u64, icc: bool| {
        Some(PngMetadata {
            counted: largest.saturating_add(kept),
            icc,
        })
    };
    let mut signature = [0u8; 8];
    let Ok(end) = r.seek(SeekFrom::End(0)) else {
        return found(0, 0, false);
    };
    if r.rewind().is_err()
        || r.read_exact(&mut signature).is_err()
        || &signature != b"\x89PNG\r\n\x1a\n"
    {
        return found(0, 0, false);
    }
    let mut at = 8u64;
    for chunk in 0..MAX_PNG_CHUNKS {
        let mut header = [0u8; 8];
        if r.read_exact(&mut header).is_err() {
            return found(largest, kept, icc);
        }
        let len = u64::from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]));
        let kind = &header[4..];
        // Length, type, data and CRC.
        at = at.saturating_add(12).saturating_add(len);
        // Bit 5 of the first byte clear marks a critical chunk.
        let critical = kind[0] & 0x20 == 0;
        let known = matches!(kind, b"IHDR" | b"PLTE");
        if matches!(kind, b"IDAT" | b"fdAT" | b"IEND")
            || at > end
            || (chunk == 0) != (kind == b"IHDR")
            || (critical && !known)
        {
            return found(largest, kept, icc);
        }
        largest = largest.max(len);
        icc |= kind == b"iCCP";
        kept = kept.saturating_add(match kind {
            b"tEXt" => len.saturating_mul(2).saturating_add(PNG_TEXT_ENTRY),
            b"zTXt" | b"iTXt" => len.saturating_add(PNG_TEXT_ENTRY),
            b"eXIf" => len.saturating_mul(2),
            _ => 0,
        });
        // The chunk's data and its CRC.
        let skip = i64::try_from(len.saturating_add(4)).unwrap_or(i64::MAX);
        if r.seek_relative(skip).is_err() {
            return found(largest, kept, icc);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;
    use std::path::PathBuf;

    use ::image::codecs::jpeg::JpegEncoder;
    use ::image::{ExtendedColorType, ImageEncoder};

    use super::*;

    /// A scratch folder for files the readers open by path.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "grape-headers-unit-{}",
                crate::output::unique_suffix()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A 40×24 image of `color` as a sequential JPEG from the `image`
    /// crate's encoder.
    fn jpeg(color: ExtendedColorType) -> Vec<u8> {
        let channels = if color == ExtendedColorType::L8 { 1 } else { 3 };
        let mut bytes = Vec::new();
        JpegEncoder::new(&mut bytes)
            .write_image(&vec![128; 40 * 24 * channels], 40, 24, color)
            .unwrap();
        bytes
    }

    /// `jpeg` with its frame header (SOF0) changed by `patch`.
    fn with_frame(mut jpeg: Vec<u8>, patch: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let sof = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        patch(&mut jpeg[sof..]);
        jpeg
    }

    /// `jpeg` with its first scan header replaced by one that carries only
    /// the first component.
    fn first_scan_of_one(mut jpeg: Vec<u8>) -> Vec<u8> {
        let sos = jpeg.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
        let len = usize::from(u16::from_be_bytes([jpeg[sos + 2], jpeg[sos + 3]]));
        jpeg.splice(
            sos..sos + 2 + len,
            [0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00],
        );
        jpeg
    }

    #[test]
    fn jpeg_layouts_come_from_the_decoders_own_headers() {
        let dir = Scratch::new();
        let cancel = CancelToken::new();
        let layout = |name: &str, bytes: &[u8]| jpeg_layout(&dir.file(name, bytes), &cancel);
        let gray = jpeg(ExtendedColorType::L8);
        let rgb = jpeg(ExtendedColorType::Rgb8);
        // Sequential frames whose first scan carries every component decode
        // row by row.
        let row_by_row = |components| JpegLayout {
            components,
            whole_image: false,
        };
        assert_eq!(layout("gray.jpg", &gray), row_by_row(1));
        assert_eq!(layout("rgb.jpg", &rgb), row_by_row(3));
        // A first scan with only the luma keeps the whole image.
        assert_eq!(
            layout("split.jpg", &first_scan_of_one(rgb.clone())),
            JpegLayout {
                components: 3,
                whole_image: true
            }
        );
        // A progressive frame (SOF2) keeps the whole image.
        let progressive = with_frame(gray.clone(), |sof| sof[1] = 0xC2);
        assert_eq!(
            layout("progressive.jpg", &progressive),
            JpegLayout {
                components: 1,
                whole_image: true
            }
        );
        // Wider than zune-jpeg's default limit, as the `image` crate allows.
        let wide = with_frame(gray.clone(), |sof| {
            sof[7..9].copy_from_slice(&20_000u16.to_be_bytes())
        });
        assert_eq!(layout("wide.jpg", &wide).components, 1);
        // Not a JPEG, or no file at all: the worst case.
        assert_eq!(layout("png.jpg", b"\x89PNG\r\n\x1a\n"), JpegLayout::WORST);
        assert_eq!(
            jpeg_layout(&dir.0.join("gone.jpg"), &cancel),
            JpegLayout::WORST
        );
        // Cancelled, the read stops: the worst case, and the job reports
        // the cancel.
        cancel.cancel();
        assert_eq!(layout("late.jpg", &rgb), JpegLayout::WORST);
    }

    #[test]
    fn the_first_scan_is_read_back_only_when_unambiguous() {
        let back = |bytes: &[u8]| {
            let mut r = Cursor::new(bytes.to_vec());
            r.seek(SeekFrom::End(0)).unwrap();
            first_scan_components(&mut r)
        };
        // A three-component scan header, ending at the end.
        let three = [
            0xFF, 0xDA, 0x00, 0x0C, 0x03, 1, 0, 2, 0x11, 3, 0x11, 0, 0x3F, 0,
        ];
        assert_eq!(back(&three), Some(3));
        // Selectors that also read as a one-component header: either could
        // be the real one, so neither is taken.
        let mut tricky = vec![0x00, 0x0C, 0x03, 1, 0x00, 0x08, 0x01, 3, 0x11, 0, 0x3F, 0];
        assert_eq!(back(&tricky), None);
        tricky[5] = 0x09;
        assert_eq!(back(&tricky), Some(3));
        // Too short to hold any header.
        assert_eq!(back(&[0x00, 0x08]), None);
    }

    #[test]
    fn the_jpeg_scan_counts_xmp_extensions_and_ff_bytes() {
        let dir = Scratch::new();
        let cancel = CancelToken::new();
        // One namespace split across the scan's 64 KiB reads, one whole,
        // and FF bytes around them.
        let mut bytes = vec![0x11; 64 * 1024 - 10];
        bytes.extend_from_slice(XMP_EXTENSION);
        bytes.extend([0xFF, 0xFF, 0x00]);
        bytes.extend_from_slice(XMP_EXTENSION);
        bytes.push(0xFF);
        let path = dir.file("xmp.jpg", &bytes);
        let scan = jpeg_scan(&path, &cancel).unwrap();
        assert_eq!(
            scan,
            JpegScan {
                xmp_segments: 2,
                ff_bytes: 3
            }
        );
        assert_eq!(scan.header_work(), 6);
        // The decoder holds the file, its metadata copies and the extended
        // XMP, which is at most the segments' data and at most the file.
        let len = bytes.len() as u64;
        assert_eq!(scan.decoder_holds(len), 3 * len);
        assert_eq!(scan.decoder_holds(1 << 30), (2 << 30) + 2 * JPEG_SEGMENT);
        assert_eq!(JpegScan::default().decoder_holds(len), 2 * len);
        cancel.cancel();
        assert!(jpeg_scan(&path, &cancel).is_err());
    }

    #[test]
    fn a_frame_the_decoder_skips_over_doesnt_count() {
        // After SOI, an RST marker with a length that covers a gray frame
        // and scan. zune-jpeg skips that length, as it does for any marker
        // it doesn't handle, and decodes the progressive color frame that
        // follows; reading RST as a lone marker would find the gray one.
        let real = with_frame(jpeg(ExtendedColorType::Rgb8), |sof| sof[1] = 0xC2);
        let hidden: &[u8] = &[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x10, 0x01, 0x01, 0x11, 0x00, //
            0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00,
        ];
        let len = u16::try_from(hidden.len() + 2).unwrap();
        let bytes = [
            &[0xFF, 0xD8, 0xFF, 0xD0][..],
            &len.to_be_bytes(),
            hidden,
            &real[2..],
        ]
        .concat();
        let dir = Scratch::new();
        assert_eq!(
            jpeg_layout(&dir.file("rst.jpg", &bytes), &CancelToken::new()),
            JpegLayout {
                components: 3,
                whole_image: true
            }
        );
    }

    /// A RIFF chunk: tag, little-endian size, data, padded to even size.
    fn riff(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = tag.to_vec();
        chunk.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        chunk.extend_from_slice(data);
        if data.len() % 2 == 1 {
            chunk.push(0);
        }
        chunk
    }

    /// An extended WebP with `flags` and `chunks` after `VP8X`.
    fn extended(flags: u8, chunks: &[Vec<u8>]) -> Vec<u8> {
        let vp8x = riff(b"VP8X", &[flags, 0, 0, 0, 15, 0, 0, 15, 0, 0]);
        let body = [b"WEBP".to_vec(), vp8x, chunks.concat()].concat();
        riff(b"RIFF", &body)
    }

    fn webp(bytes: &[u8]) -> Option<WebpLayout> {
        read_webp_layout(&mut BufReader::new(Cursor::new(bytes)))
    }

    #[test]
    fn webp_layouts_follow_the_decoders_rules() {
        let still = |lossy_alpha| Some(WebpLayout::Still { lossy_alpha });
        let vp8 = riff(b"VP8 ", &[0; 10]);
        let vp8l = riff(b"VP8L", &[0; 5]);
        let alph = riff(b"ALPH", &[0; 3]);
        let iccp = riff(b"ICCP", &[0; 7]);
        assert_eq!(webp(&extended(0x12, &[])), Some(WebpLayout::Animated));
        // Alpha with a lossy image, and with a lossless one after an ICC
        // profile.
        assert_eq!(
            webp(&extended(0x10, &[alph.clone(), vp8.clone()])),
            still(true)
        );
        assert_eq!(
            webp(&extended(0x30, &[iccp.clone(), vp8l.clone()])),
            still(false)
        );
        // No alpha: neither keeps an alpha plane.
        assert_eq!(webp(&extended(0x20, &[iccp, vp8.clone()])), still(false));
        // Alpha and no image chunk to tell: the larger case.
        assert_eq!(webp(&extended(0x10, &[])), still(true));
        for simple in [&b"VP8L"[..], b"VP8 "] {
            let body = [&b"WEBP"[..], simple, &[0; 18][..]].concat();
            assert_eq!(webp(&riff(b"RIFF", &body)), Some(WebpLayout::Simple));
        }
        // Not a WebP, or cut short.
        assert_eq!(webp(&[0x89; 32]), None);
        assert_eq!(webp(b"RIFF"), None);
        let dir = Scratch::new();
        assert_eq!(
            webp_layout(&dir.file("short.webp", b"RIFF")),
            WebpLayout::Animated
        );
    }

    #[test]
    fn a_vp8l_chunk_past_the_decoders_scan_doesnt_count() {
        // image-webp scans from the end of `VP8X` to that point plus the
        // RIFF size less 12: here 10 bytes past the RIFF data. A `VP8L`
        // chunk further on is never read, so the image decodes as `VP8 `
        // with its alpha chunk.
        let lossy = extended(0x10, &[riff(b"ALPH", &[0; 3]), riff(b"VP8 ", &[0; 10])]);
        let beyond = [lossy.clone(), vec![0; 16], riff(b"VP8L", &[0; 5])].concat();
        assert_eq!(webp(&beyond), Some(WebpLayout::Still { lossy_alpha: true }));
        // Within reach, it counts.
        let within = [lossy, riff(b"VP8L", &[0; 5])].concat();
        assert_eq!(
            webp(&within),
            Some(WebpLayout::Still { lossy_alpha: false })
        );
    }

    /// A PNG chunk: length, type, data and a (wrong) CRC, which the walk
    /// never checks.
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        bytes.extend([0; 4]);
        bytes
    }

    fn png(chunks: &[Vec<u8>]) -> Option<PngMetadata> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(chunk(b"IHDR", &[0; 13]));
        for c in chunks {
            png.extend_from_slice(c);
        }
        read_png_metadata(&mut BufReader::new(Cursor::new(png)))
    }

    fn counted(counted: u64, icc: bool) -> Option<PngMetadata> {
        Some(PngMetadata { counted, icc })
    }

    #[test]
    fn png_metadata_counts_what_the_decoder_keeps_ahead_of_the_image() {
        let idat = chunk(b"IDAT", &[0; 4]);
        assert_eq!(png(std::slice::from_ref(&idat)), counted(13, false));
        let chunks = [
            chunk(b"tEXt", &[b'a'; 10]),
            chunk(b"eXIf", &[0; 20]),
            chunk(b"zTXt", &[0; 5]),
            chunk(b"iTXt", &[0; 7]),
            chunk(b"iCCP", &[0; 9]),
            idat.clone(),
            // After the image data: the decoder doesn't read it up front.
            chunk(b"tEXt", &[b'a'; 1000]),
        ];
        // The largest chunk read (20) plus each one's share.
        let text = 2 * 10 + PNG_TEXT_ENTRY;
        let exif = 2 * 20;
        let compressed = 5 + PNG_TEXT_ENTRY + 7 + PNG_TEXT_ENTRY;
        assert_eq!(png(&chunks), counted(20 + text + exif + compressed, true));
        // Cut off before the image data: what was read so far (IHDR's 13
        // bytes are the largest chunk then).
        assert_eq!(png(&chunks[..1]), counted(13 + text, false));
    }

    #[test]
    fn the_png_walk_stops_where_the_decoder_fails() {
        let text = 2 * 10 + PNG_TEXT_ENTRY;
        // A chunk that claims to run past the end.
        let mut broken = chunk(b"tEXt", &[b'a'; 10]);
        broken[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(png(&[broken]), counted(13, false));
        // A critical chunk the decoder doesn't know, as a run of zeros
        // reads: more of them than the walk would count.
        let zeros = vec![0u8; 12 * (MAX_PNG_CHUNKS as usize + 1)];
        assert_eq!(png(&[zeros]), counted(13, false));
        let critical = chunk(b"ABCD", &[0; 4]);
        let comment = chunk(b"tEXt", &[b'a'; 10]);
        assert_eq!(
            png(&[comment.clone(), critical, comment]),
            counted(13 + text, false)
        );
        // A first chunk other than IHDR.
        let mut no_ihdr = b"\x89PNG\r\n\x1a\n".to_vec();
        no_ihdr.extend(chunk(b"tEXt", &[b'a'; 10]));
        assert_eq!(
            read_png_metadata(&mut BufReader::new(Cursor::new(no_ihdr))),
            counted(0, false)
        );
        // Not a PNG at all, even if what follows looks like chunks.
        let mut not_png = b"GIF89a\0\0".to_vec();
        not_png.extend(chunk(b"IHDR", &[0; 13]));
        not_png.extend(chunk(b"tEXt", &[b'a'; 10]));
        assert_eq!(
            read_png_metadata(&mut BufReader::new(Cursor::new(not_png))),
            counted(0, false)
        );
    }

    #[test]
    fn a_png_with_too_many_chunks_ahead_of_the_image_gives_no_count() {
        let empty = chunk(b"tEXt", &[]);
        let mut many: Vec<Vec<u8>> = vec![empty; MAX_PNG_CHUNKS as usize];
        many.push(chunk(b"IDAT", &[0; 4]));
        assert_eq!(png(&many), None);
        // With IHDR and IDAT, the walk reads exactly its limit of chunks.
        many.drain(..2);
        let count = u64::from(MAX_PNG_CHUNKS) - 2;
        assert_eq!(png(&many), counted(13 + count * PNG_TEXT_ENTRY, false));
    }
}
