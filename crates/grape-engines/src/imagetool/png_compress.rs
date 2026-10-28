//! Compress on PNG (D26): lossless at every preset. The pixels are
//! decoded, stored in the smallest exact form, and written again with the
//! `png` crate's best compression, the metadata carried. The output must
//! be at most 95 % of the source's size.
//!
//! Exact reductions, in order ([`reduce`]):
//! 1. 16 → 8 bits a sample, when every sample is v · 257;
//! 2. no alpha, when every pixel is opaque;
//! 3. gray, when every pixel has R = G = B;
//! 4. a palette of up to 256 colors (with `tRNS` for the transparent ones),
//!    at the fewest bits an index that holds it.
//!
//! Gray and the palette are skipped when the source has an ICC profile,
//! cICP, mDCV or cLLI, whose meaning depends on the color type.
//!
//! Carried: the ICC profile, sRGB, gAMA, cHRM, cICP, mDCV, cLLI, `eXIf`,
//! the text chunks, pHYs and tIME. Dropped: sBIT, bKGD, hIST, sPLT, the old
//! PLTE and tRNS (made again), `iDOT` and unknown chunks. An animated PNG
//! is refused.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use grape_core::{Format, Job};
use image::DynamicImage;
use png::{BitDepth, ColorType, Compression, Filter};

use super::strip::{read_exact, Error};
use crate::cancel::Cancellable;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{EngineCtx, ErrorKind, GrapeError, JobOutcome, Note};

/// What the chunk walk found ([`walk`]).
#[derive(Debug, Default)]
struct Chunks {
    /// acTL before the image data: an animation.
    animated: bool,
    /// The color chunks that tie the color type: iCCP, cICP, mDCV, cLLI.
    color_bound: bool,
    /// Chunks the `png` crate doesn't write: cICP, mDCV, cLLI, tIME, as
    /// (type, data).
    raw: Vec<([u8; 4], Vec<u8>)>,
}

/// Walks the PNG's chunks, reading only the small ones Compress carries
/// itself.
fn walk(r: &mut dyn BufRead) -> Result<Chunks, Error> {
    let mut found = Chunks::default();
    let mut signature = [0; 8];
    read_exact(r, &mut signature)?;
    if &signature != b"\x89PNG\r\n\x1a\n" {
        return Err(Error::Broken("no PNG signature".into()));
    }
    let mut image_data = false;
    for _ in 0..(1 << 20) {
        let mut header = [0; 8];
        read_exact(r, &mut header)?;
        let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let kind = [header[4], header[5], header[6], header[7]];
        let len = usize::try_from(len).map_err(|_| Error::Broken("a huge chunk".into()))?;
        let small = matches!(&kind, b"cICP" | b"mDCV" | b"cLLI" | b"tIME") && len <= 64;
        if small {
            let mut data = vec![0; len];
            read_exact(r, &mut data)?;
            skip(r, 4)?;
            found.raw.push((kind, data));
        } else {
            skip(r, len.saturating_add(4))?;
        }
        match &kind {
            b"acTL" if !image_data => found.animated = true,
            b"iCCP" | b"cICP" | b"mDCV" | b"cLLI" => found.color_bound = true,
            b"IDAT" => image_data = true,
            b"IEND" => return Ok(found),
            _ => {}
        }
    }
    Err(Error::Broken("too many chunks".into()))
}

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

/// Pixels in the form the PNG is written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Packed {
    pub(super) color: ColorType,
    pub(super) depth: BitDepth,
    /// The rows, each starting on a byte, 16-bit samples big-endian.
    pub(super) data: Vec<u8>,
    pub(super) palette: Option<Vec<u8>>,
    pub(super) trns: Option<Vec<u8>>,
}

/// The smallest exact form of `img` ([module docs](self)); gray and a
/// palette only if `free_color` (no color chunk ties the color type).
pub(super) fn reduce(img: &DynamicImage, free_color: bool) -> Packed {
    let (width, height) = (img.width() as usize, img.height() as usize);
    // Every pixel as RGBA, 16 bits a sample.
    let rgba: Vec<[u16; 4]> = img.to_rgba16().pixels().map(|p| p.0).collect();
    let gray_source = matches!(
        img.color(),
        image::ColorType::L8
            | image::ColorType::L16
            | image::ColorType::La8
            | image::ColorType::La16
    );
    let wide_source = matches!(
        img.color(),
        image::ColorType::L16
            | image::ColorType::La16
            | image::ColorType::Rgb16
            | image::ColorType::Rgba16
    );
    let alpha_source = img.color().has_alpha();
    let eight = !wide_source || rgba.iter().all(|p| p.iter().all(|&v| v % 257 == 0));
    let opaque = !alpha_source || rgba.iter().all(|p| p[3] == u16::MAX);
    let gray = gray_source || (free_color && rgba.iter().all(|p| p[0] == p[1] && p[1] == p[2]));
    let has_alpha = !opaque;

    if free_color && eight {
        if let Some(packed) = palette(&rgba, width, height, gray, has_alpha) {
            return packed;
        }
    }
    let channels: &[usize] = match (gray, has_alpha) {
        (true, false) => &[0],
        (true, true) => &[0, 3],
        (false, false) => &[0, 1, 2],
        (false, true) => &[0, 1, 2, 3],
    };
    let color = match (gray, has_alpha) {
        (true, false) => ColorType::Grayscale,
        (true, true) => ColorType::GrayscaleAlpha,
        (false, false) => ColorType::Rgb,
        (false, true) => ColorType::Rgba,
    };
    let mut data = Vec::with_capacity(rgba.len() * channels.len() * if eight { 1 } else { 2 });
    for p in &rgba {
        for &c in channels {
            if eight {
                data.push((p[c] / 257) as u8);
            } else {
                data.extend_from_slice(&p[c].to_be_bytes());
            }
        }
    }
    Packed {
        color,
        depth: if eight {
            BitDepth::Eight
        } else {
            BitDepth::Sixteen
        },
        data,
        palette: None,
        trns: None,
    }
}

/// An indexed form of 8-bit `rgba` if it has at most 256 colors, or a
/// gray one at fewer bits if every gray level fits them (gray without
/// alpha only); the transparent colors first, so `tRNS` is short.
fn palette(
    rgba: &[[u16; 4]],
    width: usize,
    height: usize,
    gray: bool,
    has_alpha: bool,
) -> Option<Packed> {
    let mut colors: Vec<[u8; 4]> = Vec::new();
    let mut index = std::collections::HashMap::new();
    for p in rgba {
        let c = [p[0] / 257, p[1] / 257, p[2] / 257, p[3] / 257].map(|v| v as u8);
        if let std::collections::hash_map::Entry::Vacant(slot) = index.entry(c) {
            if colors.len() == 256 {
                return None;
            }
            slot.insert(0u8);
            colors.push(c);
        }
    }
    // A gray picture of up to 16 levels can be gray at 1, 2 or 4 bits
    // only if its levels are the evenly spaced ones; the palette is
    // simpler and as small, so gray stays 8-bit gray and the palette is
    // used only for color, or gray with alpha.
    if gray && !has_alpha {
        return None;
    }
    colors.sort_by_key(|c| c[3] == u8::MAX);
    for (i, c) in colors.iter().enumerate() {
        index.insert(*c, i as u8);
    }
    let depth = match colors.len() {
        0..=2 => BitDepth::One,
        3..=4 => BitDepth::Two,
        5..=16 => BitDepth::Four,
        _ => BitDepth::Eight,
    };
    let bits = depth as usize;
    let row_bytes = (width * bits).div_ceil(8);
    let mut data = vec![0u8; row_bytes * height];
    for (n, p) in rgba.iter().enumerate() {
        let c = [p[0] / 257, p[1] / 257, p[2] / 257, p[3] / 257].map(|v| v as u8);
        let i = index[&c];
        let (y, x) = (n / width, n % width);
        let bit = x * bits;
        let byte = y * row_bytes + bit / 8;
        let shift = 8 - bits - bit % 8;
        data[byte] |= i << shift;
    }
    let palette: Vec<u8> = colors.iter().flat_map(|c| [c[0], c[1], c[2]]).collect();
    let transparent = colors.iter().take_while(|c| c[3] != u8::MAX).count();
    let trns = (transparent > 0).then(|| colors[..transparent].iter().map(|c| c[3]).collect());
    Some(Packed {
        color: ColorType::Indexed,
        depth,
        data,
        palette: Some(palette),
        trns,
    })
}

/// Runs Compress on the PNG `src` (named as `from`) into `dest`.
pub(crate) fn run(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, from);
    let dir = publish::dest_dir(dest, &names)?;
    let cancel = &ctx.cancel;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let open =
        || File::open(src).map_err(|e| names.read_failed(format!("open {}: {e}", src.display())));
    let file = open()?;
    let before = file
        .metadata()
        .map_err(|e| names.read_failed(format!("stat {}: {e}", src.display())))?
        .len();
    let broken = |e: Error| {
        let e = match e {
            Error::Broken(why) => names.decode_failed(format!("{}: {why}", src.display())),
            Error::Read(e) | Error::Write(e) => {
                names.read_failed(format!("read {}: {e}", src.display()))
            }
        };
        publish::or_cancelled(cancel, src, e)
    };
    let chunks = walk(&mut BufReader::new(Cancellable::new(file, cancel))).map_err(broken)?;
    if chunks.animated {
        return Err(GrapeError::new(
            ErrorKind::NotYetSupported,
            format!(
                "{} is animated. Compress works on still PNGs for now.",
                names.source
            ),
            format!("acTL in {}", src.display()),
        ));
    }
    // The metadata, as the `png` crate reads it.
    let limits = png::Limits {
        bytes: crate::image::PNG_METADATA_LIMIT as usize,
    };
    let reader =
        png::Decoder::new_with_limits(BufReader::new(Cancellable::new(open()?, cancel)), limits)
            .read_info()
            .map_err(|e| names.decode_failed(format!("read {}: {e}", src.display())))
            .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    let info = reader.info().clone();
    drop(reader);
    progress(0.1);

    let (img, _icc) = crate::image::decode_stored(src, from, Format::Png, &names, cancel)?;
    progress(0.5);
    let packed = reduce(&img, !chunks.color_bound);
    drop(img);
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let encoded = encode(&packed, &info, &chunks.raw)
        .map_err(|e| names.encode_failed(format!("encode {}: {e}", src.display())))?;
    progress(0.9);
    let after = encoded.len() as u64;
    if after.saturating_mul(20) > before.saturating_mul(19) {
        let detail = format!("{after} bytes from {before} in {}", src.display());
        return Err(names.not_smaller(false, detail));
    }

    publish::ensure_dir(job, ctx, dir, &names)?;
    let (temp, mut out) = TempFile::create(dir, from.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    out.write_all(&encoded)
        .and_then(|()| out.sync_all())
        .map_err(|e| names.write_failed(format!("write {}: {e}", temp.path().display())))?;
    drop(out);
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let mut outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    progress(1.0);
    outcome.notes.push(Note::Compressed { before, after });
    Ok(outcome)
}

/// Writes `packed` as a PNG with `source`'s metadata, and the `raw` chunks
/// the `png` crate doesn't write itself.
fn encode(
    packed: &Packed,
    source: &png::Info<'static>,
    raw: &[([u8; 4], Vec<u8>)],
) -> Result<Vec<u8>, png::EncodingError> {
    let mut info = png::Info::with_size(source.width, source.height);
    info.color_type = packed.color;
    info.bit_depth = packed.depth;
    info.palette = packed.palette.clone().map(Into::into);
    info.trns = packed.trns.clone().map(Into::into);
    info.pixel_dims = source.pixel_dims;
    info.srgb = source.srgb;
    info.source_gamma = source.source_gamma;
    info.source_chromaticities = source.source_chromaticities;
    info.icc_profile = source.icc_profile.clone();
    info.exif_metadata = source.exif_metadata.clone();
    info.uncompressed_latin1_text = source.uncompressed_latin1_text.clone();
    info.compressed_latin1_text = source.compressed_latin1_text.clone();
    info.utf8_text = source.utf8_text.clone();
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::with_info(&mut out, info)?;
        encoder.set_compression(Compression::High);
        encoder.set_filter(Filter::Adaptive);
        let mut writer = encoder.write_header()?;
        for (kind, data) in raw {
            writer.write_chunk(png::chunk::ChunkType(*kind), data)?;
        }
        writer.write_image_data(&packed.data)?;
        writer.finish()?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use image::{GrayImage, ImageBuffer, Luma, Rgb, RgbImage, Rgba, RgbaImage};

    use super::*;

    fn decoded(packed: &Packed, w: u32, h: u32) -> image::RgbaImage {
        let mut info = png::Info::with_size(w, h);
        info.color_type = packed.color;
        info.bit_depth = packed.depth;
        let raw: Vec<([u8; 4], Vec<u8>)> = Vec::new();
        let png = encode(packed, &info, &raw).unwrap();
        image::load_from_memory(&png).unwrap().to_rgba8()
    }

    #[test]
    fn each_exact_reduction_keeps_every_pixel() {
        // 16-bit that's really 8-bit, opaque, gray: 8-bit gray.
        let img = DynamicImage::ImageRgba16(ImageBuffer::from_fn(5, 3, |x, y| {
            let v = u16::try_from(x * 40 + y).unwrap() * 257;
            Rgba([v, v, v, u16::MAX])
        }));
        let packed = reduce(&img, true);
        assert_eq!(
            (packed.color, packed.depth),
            (ColorType::Grayscale, BitDepth::Eight)
        );
        assert_eq!(decoded(&packed, 5, 3), img.to_rgba8());
        // With an ICC profile, the color type stays.
        let packed = reduce(&img, false);
        assert_eq!(
            (packed.color, packed.depth),
            (ColorType::Rgb, BitDepth::Eight)
        );
        // A real 16-bit sample stays 16-bit.
        let deep = DynamicImage::ImageRgb16(ImageBuffer::from_fn(4, 4, |x, _| {
            Rgb([u16::try_from(x).unwrap() * 1000 + 1, 0, 0])
        }));
        let packed = reduce(&deep, true);
        assert_eq!(packed.depth, BitDepth::Sixteen);
        // Three colors with one transparent: a 2-bit palette, tRNS first.
        let few = DynamicImage::ImageRgba8(RgbaImage::from_fn(7, 3, |x, _| match x % 3 {
            0 => Rgba([255, 0, 0, 255]),
            1 => Rgba([0, 255, 0, 128]),
            _ => Rgba([0, 0, 255, 255]),
        }));
        let packed = reduce(&few, true);
        assert_eq!(
            (packed.color, packed.depth),
            (ColorType::Indexed, BitDepth::Two)
        );
        assert_eq!(packed.trns, Some(vec![128]));
        assert_eq!(decoded(&packed, 7, 3), few.to_rgba8());
        // 300 colors: no palette.
        let many = DynamicImage::ImageRgb8(RgbImage::from_fn(300, 1, |x, _| {
            Rgb([
                u8::try_from(x % 256).unwrap(),
                u8::try_from(x / 256).unwrap(),
                0,
            ])
        }));
        assert_eq!(reduce(&many, true).color, ColorType::Rgb);
        // Gray stays gray (not a palette).
        let gray = DynamicImage::ImageLuma8(GrayImage::from_fn(9, 2, |x, _| {
            Luma([u8::try_from(x * 20).unwrap()])
        }));
        let packed = reduce(&gray, true);
        assert_eq!(packed.color, ColorType::Grayscale);
        assert_eq!(decoded(&packed, 9, 2), gray.to_rgba8());
    }

    #[test]
    fn the_walk_finds_animation_and_color_chunks() {
        let png = |chunks: &[(&[u8; 4], &[u8])]| {
            let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
            for (kind, data) in chunks {
                out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
                out.extend_from_slice(*kind);
                out.extend_from_slice(data);
                out.extend_from_slice(&[0; 4]);
            }
            out
        };
        let ihdr: &[u8] = &[0; 13];
        let apng = png(&[
            (b"IHDR", ihdr),
            (b"acTL", &[0; 8]),
            (b"IDAT", &[1]),
            (b"IEND", &[]),
        ]);
        let found = walk(&mut Cursor::new(apng)).unwrap();
        assert!(found.animated && !found.color_bound);
        let hdr = png(&[
            (b"IHDR", ihdr),
            (b"cICP", &[9, 16, 0, 1]),
            (b"tIME", &[7, 232, 5, 1, 0, 0, 0]),
            (b"IDAT", &[1]),
            (b"acTL", &[0; 8]),
            (b"IEND", &[]),
        ]);
        let found = walk(&mut Cursor::new(hdr)).unwrap();
        assert!(!found.animated && found.color_bound);
        assert_eq!(found.raw.len(), 2);
        assert!(walk(&mut Cursor::new(b"\x89PNG\r\n\x1a\n\0\0".to_vec())).is_err());
    }
}
