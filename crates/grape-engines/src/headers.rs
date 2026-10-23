//! What a source's headers say about how much its decoder will hold, read
//! before decoding so the memory estimate in [`crate::image`] can count it.
//!
//! The readers open the source read-only, read only headers (segment and
//! chunk bodies are skipped with seeks), and answer with the larger case
//! when the headers can't be read or look unusual: a wrong guess may refuse
//! an image, never let one past the limits.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// How a JPEG's frame is coded, from its markers up to the first scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JpegLayout {
    /// Color components in the frame (1 gray, 3 YCbCr, 4 CMYK).
    pub(crate) components: u8,
    /// True when zune-jpeg keeps 16-bit coefficients for every component of
    /// the whole image while decoding: a progressive frame, or a sequential
    /// one whose first scan doesn't carry every component. Otherwise it
    /// decodes one row of blocks at a time.
    pub(crate) whole_image: bool,
}

impl JpegLayout {
    /// What to assume when the markers can't be read: the most a JPEG
    /// decoder could keep.
    pub(crate) const WORST: JpegLayout = JpegLayout {
        components: 4,
        whole_image: true,
    };
}

/// Markers read before giving up: a JPEG's headers hold a few dozen
/// segments (up to 255 for a large ICC profile).
const MAX_JPEG_SEGMENTS: usize = 4096;

/// Stray and fill bytes allowed before one marker.
const MAX_JPEG_FILL: usize = 64 * 1024;

/// The layout of the JPEG at `src`, or [`JpegLayout::WORST`] if its markers
/// can't be read up to the first scan.
pub(crate) fn jpeg_layout(src: &Path) -> JpegLayout {
    File::open(src)
        .and_then(|file| read_jpeg_layout(&mut BufReader::new(file)))
        .ok()
        .flatten()
        .unwrap_or(JpegLayout::WORST)
}

/// Walks the markers from SOI to the first SOS. `None` when the stream
/// isn't laid out as expected.
fn read_jpeg_layout<R: Read + Seek>(r: &mut BufReader<R>) -> io::Result<Option<JpegLayout>> {
    if read_u16(r)? != 0xFFD8 {
        return Ok(None);
    }
    // (components, progressive) from the frame header.
    let mut frame: Option<(u8, bool)> = None;
    for _ in 0..MAX_JPEG_SEGMENTS {
        // Like zune-jpeg, skip stray bytes before the next marker, fill
        // bytes (`FF`) within it, and `FF 00`, which isn't a marker.
        let mut skipped = 0;
        let marker = loop {
            if skipped > MAX_JPEG_FILL {
                return Ok(None);
            }
            skipped += 1;
            if read_u8(r)? != 0xFF {
                continue;
            }
            let mut byte = read_u8(r)?;
            while byte == 0xFF {
                if skipped > MAX_JPEG_FILL {
                    return Ok(None);
                }
                skipped += 1;
                byte = read_u8(r)?;
            }
            if byte != 0x00 {
                break byte;
            }
        };
        match marker {
            // TEM and RSTn stand alone.
            0x01 | 0xD0..=0xD7 => continue,
            // Another SOI, or EOI before any scan.
            0xD8 | 0xD9 => return Ok(None),
            _ => {}
        }
        let body = i64::from(read_u16(r)?) - 2;
        if body < 0 {
            return Ok(None);
        }
        match marker {
            // SOFn (C4, C8 and CC are DHT, JPG and DAC): precision (1),
            // height (2), width (2), then the component count.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                if body < 6 || frame.is_some() {
                    return Ok(None);
                }
                r.seek_relative(5)?;
                let components = read_u8(r)?;
                // Only SOF0 and SOF1 are sequential; zune-jpeg decodes SOF2
                // progressively and refuses the rest.
                frame = Some((components, !matches!(marker, 0xC0 | 0xC1)));
                r.seek_relative(body - 6)?;
            }
            // SOS: the number of components in the first scan.
            0xDA => {
                let Some((components, progressive)) = frame else {
                    return Ok(None);
                };
                if body < 1 {
                    return Ok(None);
                }
                let in_first_scan = read_u8(r)?;
                return Ok(Some(JpegLayout {
                    components,
                    whole_image: progressive || in_first_scan < components,
                }));
            }
            _ => r.seek_relative(body)?,
        }
    }
    Ok(None)
}

/// How a WebP is laid out, from its first chunk header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebpLayout {
    /// A simple file: one `VP8 ` (lossy) or `VP8L` (lossless) image.
    Simple,
    /// An extended (`VP8X`) still image, with or without alpha. A lossy
    /// image with alpha carries it in an `ALPH` chunk.
    Still { alpha: bool },
    /// An extended (`VP8X`) animation.
    Animated,
}

/// The layout of the WebP at `src`. A header that can't be read counts as
/// [`WebpLayout::Animated`], whose decoder keeps the most.
pub(crate) fn webp_layout(src: &Path) -> WebpLayout {
    let mut header = [0u8; 21];
    let read = File::open(src).and_then(|mut file| file.read_exact(&mut header));
    if read.is_err() || &header[..4] != b"RIFF" || &header[8..12] != b"WEBP" {
        return WebpLayout::Animated;
    }
    match &header[12..16] {
        b"VP8 " | b"VP8L" => WebpLayout::Simple,
        // The flags byte: 0x02 animation, 0x10 alpha.
        b"VP8X" if header[20] & 0x02 == 0 => WebpLayout::Still {
            alpha: header[20] & 0x10 != 0,
        },
        _ => WebpLayout::Animated,
    }
}

/// Chunks ahead of the image data that the PNG walk reads before giving up.
/// Real files have a few dozen.
const MAX_PNG_CHUNKS: u32 = 1 << 20;

/// What the PNG decoder keeps for each text chunk besides its text: the
/// parsed entry, its keyword and their share of the list.
const PNG_TEXT_ENTRY: u64 = 128;

/// About how many bytes of metadata the PNG decoder will keep while it
/// decodes the PNG at `src`: everything ahead of the first image data,
/// which it reads before any pixel. That is the buffer it reads chunks
/// into (as large as the largest chunk), each text chunk's entry and text
/// (`tEXt`'s Latin-1 can double in size as UTF-8), and two copies of an
/// `eXIf` chunk (the decoder's, and the `image` crate's while it reads the
/// orientation). The inflated ICC profile isn't counted here: the decoder
/// holds it to its own limit. The walk stops where the decoder will fail
/// anyway (no PNG signature, a chunk that runs past the end of the file),
/// so a broken file is reported as unreadable, not as too large. `None`
/// when there are more than [`MAX_PNG_CHUNKS`] chunks ahead of the image
/// data.
pub(crate) fn png_metadata(src: &Path) -> Option<u64> {
    match File::open(src) {
        Ok(file) => read_png_metadata(&mut BufReader::new(file)),
        // The decoder will fail the same way.
        Err(_) => Some(0),
    }
}

fn read_png_metadata<R: Read + Seek>(r: &mut BufReader<R>) -> Option<u64> {
    let mut largest = 0u64;
    let mut kept = 0u64;
    // A read error, the end of the file or a broken chunk stops the walk:
    // the decoder will fail there too.
    let mut signature = [0u8; 8];
    let Ok(end) = r.seek(SeekFrom::End(0)) else {
        return Some(0);
    };
    if r.rewind().is_err()
        || r.read_exact(&mut signature).is_err()
        || &signature != b"\x89PNG\r\n\x1a\n"
    {
        return Some(0);
    }
    let mut at = 8u64;
    for _ in 0..MAX_PNG_CHUNKS {
        let mut header = [0u8; 8];
        if r.read_exact(&mut header).is_err() {
            return Some(largest.saturating_add(kept));
        }
        let len = u64::from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]));
        let kind = &header[4..];
        // Length, type, data and CRC.
        at = at.saturating_add(12).saturating_add(len);
        if matches!(kind, b"IDAT" | b"fdAT" | b"IEND") || at > end {
            return Some(largest.saturating_add(kept));
        }
        largest = largest.max(len);
        kept = kept.saturating_add(match kind {
            b"tEXt" => len.saturating_mul(2).saturating_add(PNG_TEXT_ENTRY),
            b"zTXt" | b"iTXt" => len.saturating_add(PNG_TEXT_ENTRY),
            b"eXIf" => len.saturating_mul(2),
            _ => 0,
        });
        // The chunk's data and its CRC.
        let skip = i64::try_from(len.saturating_add(4)).unwrap_or(i64::MAX);
        if r.seek_relative(skip).is_err() {
            return Some(largest.saturating_add(kept));
        }
    }
    None
}

fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut byte = [0u8; 1];
    r.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn read_u16<R: Read>(r: &mut R) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    r.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use ::image::codecs::jpeg::JpegEncoder;
    use ::image::{DynamicImage, ImageEncoder};

    use super::*;

    fn layout(bytes: &[u8]) -> Option<JpegLayout> {
        read_jpeg_layout(&mut BufReader::new(Cursor::new(bytes))).unwrap_or(None)
    }

    /// SOI, an APP0 segment, then a frame header (`sof`) with `components`
    /// and a first scan carrying `in_scan` of them.
    fn jpeg(sof: u8, components: u8, in_scan: u8) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8];
        bytes.extend([0xFF, 0xE0, 0x00, 0x06, b'J', b'F', b'I', b'F']);
        // Fill bytes before a marker are allowed.
        bytes.extend([0xFF, 0xFF, sof]);
        let len = 8 + 3 * u16::from(components);
        bytes.extend(len.to_be_bytes());
        bytes.extend([8, 0, 16, 0, 16, components]);
        for id in 1..=components {
            bytes.extend([id, 0x11, 0]);
        }
        bytes.extend([0xFF, 0xDA]);
        bytes.extend((6 + 2 * u16::from(in_scan)).to_be_bytes());
        bytes.push(in_scan);
        for id in 1..=in_scan {
            bytes.extend([id, 0]);
        }
        bytes.extend([0, 63, 0]);
        bytes
    }

    #[test]
    fn sequential_jpegs_with_every_component_in_the_first_scan_decode_by_rows() {
        let row_by_row = |components| JpegLayout {
            components,
            whole_image: false,
        };
        assert_eq!(layout(&jpeg(0xC0, 3, 3)), Some(row_by_row(3)));
        assert_eq!(layout(&jpeg(0xC1, 1, 1)), Some(row_by_row(1)));
        assert_eq!(layout(&jpeg(0xC0, 4, 4)), Some(row_by_row(4)));

        // What the `image` crate itself writes.
        let mut real = Vec::new();
        JpegEncoder::new(&mut real)
            .write_image(
                DynamicImage::new_rgb8(40, 24).as_bytes(),
                40,
                24,
                ::image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        assert_eq!(layout(&real), Some(row_by_row(3)));
    }

    #[test]
    fn progressive_or_split_scans_keep_the_whole_image() {
        let whole = |components| JpegLayout {
            components,
            whole_image: true,
        };
        assert_eq!(layout(&jpeg(0xC2, 3, 3)), Some(whole(3)));
        assert_eq!(layout(&jpeg(0xC2, 4, 1)), Some(whole(4)));
        // Sequential, but the first scan carries only the luma.
        assert_eq!(layout(&jpeg(0xC0, 3, 1)), Some(whole(3)));
        // Frames zune-jpeg doesn't decode sequentially count as whole-image.
        assert_eq!(layout(&jpeg(0xC9, 3, 3)), Some(whole(3)));
    }

    #[test]
    fn unreadable_markers_give_no_layout() {
        let good = jpeg(0xC0, 3, 3);
        // The frame header starts after SOI and APP0; the scan's 14 bytes
        // end the stream.
        let (frame, scan) = (10, good.len() - 14);
        assert_eq!(&good[scan..scan + 2], [0xFF, 0xDA]);
        // Not a JPEG; cut off in the scan header; a scan before any frame.
        assert_eq!(layout(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(layout(&good[..scan + 2]), None);
        assert_eq!(layout(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08, 0x01]), None);
        // Stray bytes and then the end; EOI before the scan.
        assert_eq!(layout(&[0xFF, 0xD8, 0x00, 0xC0]), None);
        assert_eq!(layout(&[0xFF, 0xD8, 0xFF, 0xD9]), None);
        // A segment length under 2; two frame headers.
        assert_eq!(layout(&[0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x01]), None);
        let mut twice = good[..scan].to_vec();
        twice.extend_from_slice(&good[frame..]);
        assert_eq!(layout(&twice), None);
        // From a path: a missing file gives the worst case.
        assert_eq!(
            jpeg_layout(Path::new("/no/such/grape.jpg")),
            JpegLayout::WORST
        );
    }

    #[test]
    fn stray_bytes_before_a_marker_are_skipped_like_the_decoder_does() {
        let good = jpeg(0xC0, 3, 3);
        let row_by_row = Some(JpegLayout {
            components: 3,
            whole_image: false,
        });
        // Two stray bytes after APP0, and `FF 00` before the frame header.
        let mut stray = good[..10].to_vec();
        stray.extend([0x00, 0x00, 0xFF, 0x00]);
        stray.extend_from_slice(&good[10..]);
        assert_eq!(layout(&stray), row_by_row);
        // Past the cap, the walk gives up.
        let mut junk = good[..10].to_vec();
        junk.resize(10 + MAX_JPEG_FILL + 2, 0x00);
        junk.extend_from_slice(&good[10..]);
        assert_eq!(layout(&junk), None);
    }

    /// A scratch folder for files the readers open by path.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "grape-headers-unit-{}",
                crate::output::unique_suffix()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
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

    #[test]
    fn the_vp8x_flags_give_the_webp_layout() {
        let dir = Scratch::new();
        let vp8x = |flags: u8| {
            let mut bytes = b"RIFF\x1e\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00".to_vec();
            bytes.extend([flags, 0, 0, 0, 15, 0, 0, 15, 0, 0]);
            bytes
        };
        let layout = |name: &str, bytes: &[u8]| webp_layout(&dir.file(name, bytes));
        assert_eq!(layout("anim.webp", &vp8x(0x12)), WebpLayout::Animated);
        assert_eq!(
            layout("alpha.webp", &vp8x(0x10)),
            WebpLayout::Still { alpha: true }
        );
        assert_eq!(
            layout("icc.webp", &vp8x(0x20)),
            WebpLayout::Still { alpha: false }
        );
        for simple in [&b"VP8L"[..], b"VP8 "] {
            let mut bytes = b"RIFF\x1e\x00\x00\x00WEBP".to_vec();
            bytes.extend_from_slice(simple);
            bytes.extend([0u8; 18]);
            assert_eq!(layout("simple.webp", &bytes), WebpLayout::Simple);
        }
        // Unreadable or not a WebP: assume the larger case.
        assert_eq!(layout("short.webp", b"RIFF"), WebpLayout::Animated);
        assert_eq!(layout("png.webp", &[0x89; 32]), WebpLayout::Animated);
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

    fn metadata(chunks: &[Vec<u8>]) -> Option<u64> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(chunk(b"IHDR", &[0; 13]));
        for c in chunks {
            png.extend_from_slice(c);
        }
        read_png_metadata(&mut BufReader::new(Cursor::new(png)))
    }

    #[test]
    fn png_metadata_counts_what_the_decoder_keeps_ahead_of_the_image() {
        let idat = chunk(b"IDAT", &[0; 4]);
        assert_eq!(metadata(std::slice::from_ref(&idat)), Some(13));
        let chunks = [
            chunk(b"tEXt", &[b'a'; 10]),
            chunk(b"eXIf", &[0; 20]),
            chunk(b"zTXt", &[0; 5]),
            chunk(b"iTXt", &[0; 7]),
            chunk(b"pHYs", &[0; 9]),
            idat.clone(),
            // After the image data: the decoder doesn't read it up front.
            chunk(b"tEXt", &[b'a'; 1000]),
        ];
        // The largest chunk read (20) plus each one's share.
        let text = 2 * 10 + PNG_TEXT_ENTRY;
        let exif = 2 * 20;
        let compressed = 5 + PNG_TEXT_ENTRY + 7 + PNG_TEXT_ENTRY;
        assert_eq!(metadata(&chunks), Some(20 + text + exif + compressed));
        // Cut off before the image data: what was read so far (IHDR's 13
        // bytes are the largest chunk then).
        assert_eq!(metadata(&chunks[..1]), Some(13 + text));
        // A chunk that claims to run past the end isn't counted: the
        // decoder will find the file broken.
        let mut broken = chunk(b"tEXt", &[b'a'; 10]);
        broken[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(metadata(&[broken]), Some(13));
        // Not a PNG at all, even if what follows looks like chunks.
        let mut not_png = b"GIF89a\0\0".to_vec();
        not_png.extend(chunk(b"tEXt", &[b'a'; 10]));
        assert_eq!(
            read_png_metadata(&mut BufReader::new(Cursor::new(not_png))),
            Some(0)
        );
    }

    #[test]
    fn a_png_with_too_many_chunks_ahead_of_the_image_gives_no_count() {
        let empty = chunk(b"tEXt", &[]);
        let mut many: Vec<Vec<u8>> = vec![empty; MAX_PNG_CHUNKS as usize];
        many.push(chunk(b"IDAT", &[0; 4]));
        assert_eq!(metadata(&many), None);
        // With IHDR and IDAT, the walk reads exactly its limit of chunks.
        many.drain(..2);
        let count = u64::from(MAX_PNG_CHUNKS) - 2;
        assert_eq!(metadata(&many), Some(13 + count * PNG_TEXT_ENTRY));
    }
}
