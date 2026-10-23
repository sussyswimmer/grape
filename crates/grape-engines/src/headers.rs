//! What a source's headers say about how much its decoder will hold, read
//! before decoding so the memory estimate in [`crate::image`] can count it.
//!
//! Both readers open the source read-only, read only headers (segment
//! bodies are skipped with seeks), and answer with the larger case when the
//! headers can't be read or look unusual: a wrong guess may refuse an
//! image, never let one past the limits.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek};
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

/// Fill bytes (`FF`) allowed before one marker.
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
        if read_u8(r)? != 0xFF {
            return Ok(None);
        }
        let mut marker = read_u8(r)?;
        let mut fill = 0;
        while marker == 0xFF {
            fill += 1;
            if fill > MAX_JPEG_FILL {
                return Ok(None);
            }
            marker = read_u8(r)?;
        }
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

/// True when the WebP at `src` is animated: its `VP8X` header sets the
/// animation flag. The decoder then keeps an RGBA frame and an RGBA canvas.
/// A header that can't be read counts as animated.
pub(crate) fn webp_is_animated(src: &Path) -> bool {
    let mut header = [0u8; 21];
    let read = File::open(src).and_then(|mut file| file.read_exact(&mut header));
    if read.is_err() || &header[..4] != b"RIFF" || &header[8..12] != b"WEBP" {
        return true;
    }
    // A simple file (`VP8 `, `VP8L`) has no animation; `VP8X` has a flags
    // byte, with 0x02 for animation.
    &header[12..16] == b"VP8X" && header[20] & 0x02 != 0
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
        // A non-marker byte where a marker belongs; EOI before the scan.
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
    fn the_vp8x_animation_flag_marks_an_animated_webp() {
        let dir = std::env::temp_dir().join(format!(
            "grape-headers-unit-{}",
            crate::output::unique_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let vp8x = |flags: u8| {
            let mut bytes = b"RIFF\x1e\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00".to_vec();
            bytes.extend([flags, 0, 0, 0, 15, 0, 0, 15, 0, 0]);
            bytes
        };
        assert!(webp_is_animated(&file("anim.webp", &vp8x(0x12))));
        assert!(!webp_is_animated(&file("still.webp", &vp8x(0x10))));
        let mut lossless = b"RIFF\x1e\x00\x00\x00WEBPVP8L".to_vec();
        lossless.extend([0u8; 18]);
        assert!(!webp_is_animated(&file("simple.webp", &lossless)));
        // Unreadable or not a WebP: assume the larger case.
        assert!(webp_is_animated(&file("short.webp", b"RIFF")));
        assert!(webp_is_animated(&file("png.webp", &[0x89; 32])));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
