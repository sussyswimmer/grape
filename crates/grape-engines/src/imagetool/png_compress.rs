//! Compress on PNG (D26): lossless at every preset. The pixels are
//! decoded, stored in the smallest exact form, and written again with the
//! `png` crate's best compression, the metadata carried. The output must
//! be at most 95 % of the source's size.
//!
//! Exact reductions, in order ([`reduce`]):
//! 1. 16 → 8 bits a sample, when every sample is v · 257;
//! 2. no alpha, when every pixel is opaque;
//! 3. gray, when every pixel has R = G = B; at 1, 2 or 4 bits when every
//!    level is one of that depth's (0 and 255; multiples of 85; of 17);
//! 4. a palette of up to 256 colors (with `tRNS` for the transparent ones),
//!    at the fewest bits an index that holds it, for color or gray with
//!    alpha.
//!
//! Gray (from color) and the palette are skipped when the source has an
//! ICC profile, cICP, mDCV or cLLI, whose meaning depends on the color
//! type.
//!
//! Carried: the ICC profile (rather than sRGB when a source has both, as
//! PNG 3 ranks them), sRGB, gAMA, cHRM, cICP, mDCV, cLLI, `eXIf`, the text
//! chunks (those after the image data stay after it), pHYs and tIME.
//! Dropped: sBIT, bKGD, hIST, sPLT, the old PLTE and tRNS (made again),
//! `iDOT` and unknown chunks. An animated PNG is refused.
//!
//! The pixels are read [`SPAN`] at a time, never copied whole, and the
//! cancel token is checked before each span and between each
//! [`ENCODE_STEP`] the encoder is given. The encoder compresses a row
//! once it's whole, so a step runs on to the end of its row: wide noisy
//! rows answer a cancel tens of ms late, and rows of many megabytes only
//! after the row.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use grape_core::{Format, Job};
use image::{ColorType as ImageColor, DynamicImage, GenericImageView};
use png::{BitDepth, ColorType, Compression, Filter};

use super::strip::{read_exact, Error};
use crate::cancel::Cancellable;
use crate::headers::PNG_TEXT_ENTRY;
use crate::image::PNG_METADATA_LIMIT;
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome, Note};

/// A raw chunk: its type and data.
type Raw = ([u8; 4], Vec<u8>);

/// What the chunk walk found ([`walk`]).
#[derive(Debug, Default)]
struct Chunks {
    /// acTL before the image data: an animation.
    animated: bool,
    /// The color chunks that tie the color type: iCCP, cICP, mDCV, cLLI.
    color_bound: bool,
    /// Chunks the `png` crate doesn't write: cICP, mDCV, cLLI, tIME.
    raw: Vec<Raw>,
    /// Text and `eXIf` after the image data, which the `png` crate's
    /// header read stops before.
    after: Vec<Raw>,
    /// The bytes in `after`.
    after_bytes: u64,
    /// The bytes of the text chunks before the image data, which the
    /// `png` crate's header read holds, and how many there are.
    text_bytes: u64,
    text_chunks: u64,
    /// More text after the image data than `limit`: the walk stopped there.
    too_much: bool,
}

/// Walks the PNG's chunks, reading only the ones Compress carries itself,
/// and at most `limit` bytes of text after the image data.
fn walk(r: &mut dyn BufRead, limit: u64) -> Result<Chunks, Error> {
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
        let late = image_data && matches!(&kind, b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf");
        if small || late {
            if late {
                found.after_bytes = found.after_bytes.saturating_add(len as u64);
                if found.after_bytes > limit {
                    found.too_much = true;
                    return Ok(found);
                }
            }
            let mut data = vec![0; len];
            read_exact(r, &mut data)?;
            skip(r, 4)?;
            if late {
                found.after.push((kind, data));
            } else {
                found.raw.push((kind, data));
            }
        } else {
            skip(r, len.saturating_add(4))?;
        }
        if !image_data && matches!(&kind, b"tEXt" | b"zTXt" | b"iTXt") {
            found.text_bytes = found.text_bytes.saturating_add(len as u64);
            found.text_chunks += 1;
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

/// How a [`scan`] ended.
enum Scan {
    Done,
    /// The row callback stopped it.
    Stopped,
    Cancelled,
}

/// The most pixels [`scan`] reads at once: a row is read in spans of
/// this many, so its buffer stays small however wide the picture.
const SPAN: usize = 1 << 16;

/// The most bytes given to the encoder between two cancel checks: up to
/// about 25 ms of its best compression (noisy 8-bit gray, its slowest,
/// takes 160 to 205 ms for 256 KiB).
const ENCODE_STEP: usize = 32 * 1024;

/// What the deflater holds, about.
const DEFLATER: u64 = 1 << 20;

// The span's buffer is counted as the deflater's share ([`peak`]).
const _: () = assert!(SPAN * std::mem::size_of::<[u16; 4]>() <= DEFLATER as usize);

/// Calls `f` with each row of `img` as RGBA, 16 bits a sample, [`SPAN`]
/// pixels at most at a time with the column they start at, until it
/// returns false; checks `cancel` before each span.
fn scan(
    img: &DynamicImage,
    cancel: &CancelToken,
    mut f: impl FnMut(usize, &[[u16; 4]]) -> bool,
) -> Scan {
    let width = img.width() as usize;
    let mut span = Vec::with_capacity(width.min(SPAN));
    for y in 0..img.height() as usize {
        for x in (0..width).step_by(SPAN) {
            if cancel.is_cancelled() {
                return Scan::Cancelled;
            }
            span.clear();
            fill_span(img, y, width, x..(x + SPAN).min(width), &mut span);
            if !f(x, &span) {
                return Scan::Stopped;
            }
        }
    }
    Scan::Done
}

/// Columns `xs` of row `y` of `img` (`width` pixels wide) as RGBA, 16 bits
/// a sample.
fn fill_span(
    img: &DynamicImage,
    y: usize,
    width: usize,
    xs: std::ops::Range<usize>,
    row: &mut Vec<[u16; 4]>,
) {
    const OPAQUE: u16 = u16::MAX;
    let wide = |v: u8| u16::from(v) * 257;
    let line = |n: usize| (y * width + xs.start) * n..(y * width + xs.end) * n;
    match img {
        DynamicImage::ImageLuma8(b) => row.extend(b.as_raw()[line(1)].iter().map(|&v| {
            let v = wide(v);
            [v, v, v, OPAQUE]
        })),
        DynamicImage::ImageLumaA8(b) => {
            row.extend(b.as_raw()[line(2)].as_chunks::<2>().0.iter().map(|p| {
                let v = wide(p[0]);
                [v, v, v, wide(p[1])]
            }));
        }
        DynamicImage::ImageRgb8(b) => row.extend(
            b.as_raw()[line(3)]
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| [wide(p[0]), wide(p[1]), wide(p[2]), OPAQUE]),
        ),
        DynamicImage::ImageRgba8(b) => row.extend(
            b.as_raw()[line(4)]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| [wide(p[0]), wide(p[1]), wide(p[2]), wide(p[3])]),
        ),
        DynamicImage::ImageLuma16(b) => {
            row.extend(b.as_raw()[line(1)].iter().map(|&v| [v, v, v, OPAQUE]));
        }
        DynamicImage::ImageLumaA16(b) => row.extend(
            b.as_raw()[line(2)]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| [p[0], p[0], p[0], p[1]]),
        ),
        DynamicImage::ImageRgb16(b) => row.extend(
            b.as_raw()[line(3)]
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| [p[0], p[1], p[2], OPAQUE]),
        ),
        DynamicImage::ImageRgba16(b) => row.extend(
            b.as_raw()[line(4)]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| [p[0], p[1], p[2], p[3]]),
        ),
        // Floating point, which a PNG never decodes to.
        other => row.extend(xs.clone().map(|x| {
            let p = other.get_pixel(x as u32, y as u32).0;
            [wide(p[0]), wide(p[1]), wide(p[2]), wide(p[3])]
        })),
    }
}

/// The smallest exact form of `img` ([module docs](self)); gray from color
/// and a palette only if `free_color` (no color chunk ties the color
/// type). `None` if cancelled.
pub(super) fn reduce(img: &DynamicImage, free_color: bool, cancel: &CancelToken) -> Option<Packed> {
    let (width, height) = (img.width() as usize, img.height() as usize);
    let gray_source = matches!(
        img.color(),
        ImageColor::L8 | ImageColor::L16 | ImageColor::La8 | ImageColor::La16
    );
    let alpha_source = img.color().has_alpha();
    // One pass for what the pixels allow.
    let mut eight = true;
    let mut opaque = true;
    let mut all_gray = true;
    // Whether every gray level fits 1, 2 or 4 bits.
    let mut fits = [true; 3];
    let pass = scan(img, cancel, |_, span| {
        for p in span {
            eight &= p.iter().all(|&v| v % 257 == 0);
            opaque &= !alpha_source || p[3] == u16::MAX;
            all_gray &= p[0] == p[1] && p[1] == p[2];
            for (fit, step) in fits.iter_mut().zip(LOW_STEPS) {
                *fit &= (p[0] / 257) % step == 0;
            }
        }
        true
    });
    if let Scan::Cancelled = pass {
        return None;
    }
    let gray = gray_source || (free_color && all_gray);
    let has_alpha = !opaque;

    if gray && !has_alpha && eight {
        if let Some(bits) = [1, 2, 4]
            .into_iter()
            .zip(fits)
            .find_map(|(b, f)| f.then_some(b))
        {
            return low_gray(img, bits, cancel);
        }
    }
    // Gray without alpha stays gray: a palette is no smaller.
    if free_color && eight && !(gray && !has_alpha) {
        match palette(img, width, height, has_alpha, cancel) {
            Indexed::Packed(packed) => return Some(packed),
            Indexed::Cancelled => return None,
            Indexed::TooMany => {}
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
    let mut data = Vec::with_capacity(width * height * channels.len() * if eight { 1 } else { 2 });
    let pass = scan(img, cancel, |_, span| {
        for p in span {
            for &c in channels {
                if eight {
                    data.push((p[c] / 257) as u8);
                } else {
                    data.extend_from_slice(&p[c].to_be_bytes());
                }
            }
        }
        true
    });
    if let Scan::Cancelled = pass {
        return None;
    }
    Some(Packed {
        color,
        depth: if eight {
            BitDepth::Eight
        } else {
            BitDepth::Sixteen
        },
        data,
        palette: None,
        trns: None,
    })
}

/// The step between an 8-bit gray's levels at 1, 2 and 4 bits.
const LOW_STEPS: [u16; 3] = [255, 85, 17];

/// `img`, gray with every level a multiple of its step, at `bits` (1, 2
/// or 4) a pixel. `None` if cancelled.
fn low_gray(img: &DynamicImage, bits: usize, cancel: &CancelToken) -> Option<Packed> {
    let (width, height) = (img.width() as usize, img.height() as usize);
    let (step, depth) = match bits {
        1 => (255, BitDepth::One),
        2 => (85, BitDepth::Two),
        _ => (17, BitDepth::Four),
    };
    let row_bytes = (width * bits).div_ceil(8);
    let mut data = Vec::with_capacity(row_bytes * height);
    let pass = scan(img, cancel, |x0, span| {
        if x0 == 0 {
            data.resize(data.len() + row_bytes, 0);
        }
        let start = data.len() - row_bytes;
        for (x, p) in (x0..).zip(span) {
            let level = (p[0] / 257 / step) as u8;
            let bit = x * bits;
            data[start + bit / 8] |= level << (8 - bits - bit % 8);
        }
        true
    });
    if let Scan::Cancelled = pass {
        return None;
    }
    Some(Packed {
        color: ColorType::Grayscale,
        depth,
        data,
        palette: None,
        trns: None,
    })
}

/// What [`palette`] made.
enum Indexed {
    Packed(Packed),
    /// More than 256 colors.
    TooMany,
    Cancelled,
}

/// An indexed form of 8-bit `img` if it has at most 256 colors; the
/// transparent colors first, so `tRNS` is short.
fn palette(
    img: &DynamicImage,
    width: usize,
    height: usize,
    has_alpha: bool,
    cancel: &CancelToken,
) -> Indexed {
    let key = |p: &[u16; 4]| {
        let alpha = if has_alpha { p[3] } else { u16::MAX };
        [p[0], p[1], p[2], alpha].map(|v| (v / 257) as u8)
    };
    let mut colors: Vec<[u8; 4]> = Vec::new();
    let mut index = HashMap::new();
    let pass = scan(img, cancel, |_, span| {
        for p in span {
            let c = key(p);
            if let std::collections::hash_map::Entry::Vacant(slot) = index.entry(c) {
                if colors.len() == 256 {
                    return false;
                }
                slot.insert(0u8);
                colors.push(c);
            }
        }
        true
    });
    match pass {
        Scan::Done => {}
        Scan::Stopped => return Indexed::TooMany,
        Scan::Cancelled => return Indexed::Cancelled,
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
    let mut data = Vec::with_capacity(row_bytes * height);
    let pass = scan(img, cancel, |x0, span| {
        if x0 == 0 {
            data.resize(data.len() + row_bytes, 0);
        }
        let start = data.len() - row_bytes;
        for (x, p) in (x0..).zip(span) {
            let i = index[&key(p)];
            let bit = x * bits;
            data[start + bit / 8] |= i << (8 - bits - bit % 8);
        }
        true
    });
    if let Scan::Cancelled = pass {
        return Indexed::Cancelled;
    }
    let palette: Vec<u8> = colors.iter().flat_map(|c| [c[0], c[1], c[2]]).collect();
    let transparent = colors.iter().take_while(|c| c[3] != u8::MAX).count();
    let trns = (transparent > 0).then(|| colors[..transparent].iter().map(|c| c[3]).collect());
    Indexed::Packed(Packed {
        color: ColorType::Indexed,
        depth,
        data,
        palette: Some(palette),
        trns,
    })
}

/// What Compress holds at its peak for a decoded image of `decoded` bytes
/// in rows of `row` bytes, besides the decoder: the image and its packed
/// form (never bigger) with a [`SPAN`] of pixels, then the packed form and
/// the PNG written from it with the encoder's three rows and its deflater
/// (the bigger of the two extras), and `metadata` bytes throughout, twice:
/// as read, and in the PNG.
fn peak(decoded: u64, row: u64, metadata: u64) -> u64 {
    let encoder = row
        .saturating_add(1)
        .saturating_mul(3)
        .saturating_add(DEFLATER);
    decoded
        .saturating_mul(2)
        .saturating_add(encoder)
        .saturating_add(metadata.saturating_mul(2))
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
    let chunks = walk(
        &mut BufReader::new(Cancellable::new(file, cancel)),
        PNG_METADATA_LIMIT,
    )
    .map_err(broken)?;
    if chunks.too_much {
        return Err(names.too_large(format!(
            "{}: over {} MiB of text after the image data",
            src.display(),
            PNG_METADATA_LIMIT >> 20
        )));
    }
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
        bytes: PNG_METADATA_LIMIT as usize,
    };
    let reader =
        png::Decoder::new_with_limits(BufReader::new(Cancellable::new(open()?, cancel)), limits)
            .read_info()
            .map_err(|e| {
                let detail = format!("read {}: {e}", src.display());
                match e {
                    png::DecodingError::LimitsExceeded => names.too_large(detail),
                    _ => names.decode_failed(detail),
                }
            })
            .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    let info = reader.info().clone();
    drop(reader);
    progress(0.1);

    // The text before the image data, as the `png` crate holds it: twice
    // its bytes as strings (from Latin-1), an entry each, and while it's
    // written a Latin-1 copy and the chunk's buffer besides the PNG (so
    // 5 times its bytes with [`peak`]'s doubling).
    let size = |data: Option<&[u8]>| data.map_or(0, |d| d.len() as u64);
    let metadata = (chunks.text_bytes.saturating_mul(5) / 2)
        .saturating_add(chunks.text_chunks.saturating_mul(PNG_TEXT_ENTRY))
        .saturating_add(chunks.after_bytes)
        .saturating_add(size(info.icc_profile.as_deref()))
        .saturating_add(size(info.exif_metadata.as_deref()));
    let tool_peak = |width: u32, height: u32, color: ImageColor| {
        let row = u64::from(width).saturating_mul(u64::from(color.bytes_per_pixel()));
        peak(row.saturating_mul(u64::from(height)), row, metadata)
    };
    let (img, _icc) =
        crate::image::decode_stored(src, from, Format::Png, &names, cancel, &tool_peak)
            .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    progress(0.5);
    let packed = reduce(&img, !chunks.color_bound, cancel).ok_or_else(|| crate::cancelled(src))?;
    drop(img);
    progress(0.6);
    let encoded = encode(&packed, info, &chunks.raw, &chunks.after, cancel)
        .map_err(|e| names.encode_failed(format!("encode {}: {e}", src.display())))
        .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
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

/// Writes `packed` as a PNG with `source`'s metadata (moved, not copied),
/// the `raw` chunks the `png` crate doesn't write itself before the image
/// data, and the `after` ones after it; stops at a cancel.
fn encode(
    packed: &Packed,
    source: png::Info<'static>,
    raw: &[Raw],
    after: &[Raw],
    cancel: &CancelToken,
) -> Result<Vec<u8>, png::EncodingError> {
    let mut info = png::Info::with_size(source.width, source.height);
    info.color_type = packed.color;
    info.bit_depth = packed.depth;
    info.palette = packed.palette.clone().map(Into::into);
    info.trns = packed.trns.clone().map(Into::into);
    info.pixel_dims = source.pixel_dims;
    // The decoder sets only the chunk fields; the encoder reads the source
    // ones.
    info.source_gamma = source.gama_chunk.or(source.source_gamma);
    info.source_chromaticities = source.chrm_chunk.or(source.source_chromaticities);
    // The encoder writes sRGB or iCCP, not both; PNG 3 ranks the profile
    // first.
    info.srgb = if source.icc_profile.is_some() {
        None
    } else {
        source.srgb
    };
    info.icc_profile = source.icc_profile;
    info.exif_metadata = source.exif_metadata;
    info.uncompressed_latin1_text = source.uncompressed_latin1_text;
    info.compressed_latin1_text = source.compressed_latin1_text;
    info.utf8_text = source.utf8_text;
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::with_info(&mut out, info)?;
        encoder.set_compression(Compression::High);
        encoder.set_filter(Filter::Adaptive);
        let mut writer = encoder.write_header()?;
        for (kind, data) in raw {
            writer.write_chunk(png::chunk::ChunkType(*kind), data)?;
        }
        let mut stream = writer.stream_writer()?;
        for part in packed.data.chunks(ENCODE_STEP) {
            if cancel.is_cancelled() {
                return Err(io::Error::other("cancelled").into());
            }
            stream.write_all(part)?;
        }
        stream.finish()?;
        for (kind, data) in after {
            writer.write_chunk(png::chunk::ChunkType(*kind), data)?;
        }
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

    fn encoded(packed: &Packed, info: &png::Info<'static>, after: &[Raw]) -> Vec<u8> {
        encode(packed, info.clone(), &[], after, &CancelToken::default()).unwrap()
    }

    fn decoded(packed: &Packed, w: u32, h: u32) -> image::RgbaImage {
        let mut info = png::Info::with_size(w, h);
        info.color_type = packed.color;
        info.bit_depth = packed.depth;
        let png = encoded(packed, &info, &[]);
        image::load_from_memory(&png).unwrap().to_rgba8()
    }

    fn reduced(img: &DynamicImage, free_color: bool) -> Packed {
        reduce(img, free_color, &CancelToken::default()).unwrap()
    }

    /// The chunk types of a PNG, in order.
    fn kinds(png: &[u8]) -> Vec<String> {
        let mut at = 8;
        let mut out = Vec::new();
        while at + 8 <= png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            out.push(String::from_utf8_lossy(&png[at + 4..at + 8]).into_owned());
            at += 12 + len;
        }
        out
    }

    /// The `png` crate's view of a PNG's metadata.
    fn info_of(png: &[u8]) -> png::Info<'static> {
        png::Decoder::new(Cursor::new(png.to_vec()))
            .read_info()
            .unwrap()
            .info()
            .clone()
    }

    #[test]
    fn each_exact_reduction_keeps_every_pixel() {
        // 16-bit that's really 8-bit, opaque, gray: 8-bit gray.
        let img = DynamicImage::ImageRgba16(ImageBuffer::from_fn(5, 3, |x, y| {
            let v = u16::try_from(x * 40 + y).unwrap() * 257;
            Rgba([v, v, v, u16::MAX])
        }));
        let packed = reduced(&img, true);
        assert_eq!(
            (packed.color, packed.depth),
            (ColorType::Grayscale, BitDepth::Eight)
        );
        assert_eq!(decoded(&packed, 5, 3), img.to_rgba8());
        // With an ICC profile, the color type stays.
        let packed = reduced(&img, false);
        assert_eq!(
            (packed.color, packed.depth),
            (ColorType::Rgb, BitDepth::Eight)
        );
        // A real 16-bit sample stays 16-bit.
        let deep = DynamicImage::ImageRgb16(ImageBuffer::from_fn(4, 4, |x, _| {
            Rgb([u16::try_from(x).unwrap() * 1000 + 1, 0, 0])
        }));
        let packed = reduced(&deep, true);
        assert_eq!(packed.depth, BitDepth::Sixteen);
        // Three colors with one transparent: a 2-bit palette, tRNS first.
        let few = DynamicImage::ImageRgba8(RgbaImage::from_fn(7, 3, |x, _| match x % 3 {
            0 => Rgba([255, 0, 0, 255]),
            1 => Rgba([0, 255, 0, 128]),
            _ => Rgba([0, 0, 255, 255]),
        }));
        let packed = reduced(&few, true);
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
        assert_eq!(reduced(&many, true).color, ColorType::Rgb);
        // Gray stays gray (not a palette).
        let gray = DynamicImage::ImageLuma8(GrayImage::from_fn(9, 2, |x, _| {
            Luma([u8::try_from(x * 20 + 1).unwrap()])
        }));
        let packed = reduced(&gray, true);
        assert_eq!(
            (packed.color, packed.depth),
            (ColorType::Grayscale, BitDepth::Eight)
        );
        assert_eq!(decoded(&packed, 9, 2), gray.to_rgba8());
    }

    #[test]
    fn gray_with_few_even_levels_goes_down_to_their_bits() {
        for (levels, depth) in [
            (&[0u8, 255][..], BitDepth::One),
            (&[0, 85, 170, 255], BitDepth::Two),
            (&[0, 17, 34, 238, 255], BitDepth::Four),
            (&[0, 16, 255], BitDepth::Eight),
        ] {
            let gray = DynamicImage::ImageLuma8(GrayImage::from_fn(11, 3, |x, y| {
                Luma([levels[(x + y) as usize % levels.len()]])
            }));
            let packed = reduced(&gray, false);
            assert_eq!(
                (packed.color, packed.depth),
                (ColorType::Grayscale, depth),
                "{levels:?}"
            );
            assert_eq!(decoded(&packed, 11, 3), gray.to_rgba8(), "{levels:?}");
        }
        // A black-and-white RGB page, with no color chunk: 1-bit gray.
        let page = DynamicImage::ImageRgb8(RgbImage::from_fn(13, 2, |x, _| {
            if x % 3 == 0 {
                Rgb([0, 0, 0])
            } else {
                Rgb([255, 255, 255])
            }
        }));
        let packed = reduced(&page, true);
        assert_eq!(packed.depth, BitDepth::One);
        assert_eq!(decoded(&packed, 13, 2), page.to_rgba8());
    }

    #[test]
    fn a_cancel_stops_the_reduction_and_the_encode() {
        let cancel = CancelToken::default();
        cancel.cancel();
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(9, 200, |x, y| {
            Rgb([u8::try_from(x).unwrap(), u8::try_from(y).unwrap(), 0])
        }));
        assert!(reduce(&img, true, &cancel).is_none());
        let packed = reduced(&img, true);
        let mut info = png::Info::with_size(9, 200);
        info.color_type = packed.color;
        info.bit_depth = packed.depth;
        assert!(encode(&packed, info, &[], &[], &cancel).is_err());
    }

    #[test]
    fn the_color_chunks_and_late_text_come_along() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(4, 4, |x, y| {
            Rgb([
                u8::try_from(x * 60).unwrap(),
                u8::try_from(y * 60).unwrap(),
                9,
            ])
        }));
        let packed = reduced(&img, true);
        // gAMA and cHRM, without sRGB.
        let mut source = png::Info::with_size(4, 4);
        source.color_type = ColorType::Rgb;
        source.source_gamma = Some(png::ScaledFloat::from_scaled(100_000));
        source.source_chromaticities = Some(png::SourceChromaticities::new(
            (0.3127, 0.329),
            (0.64, 0.33),
            (0.3, 0.6),
            (0.15, 0.06),
        ));
        let late: Vec<Raw> = vec![(*b"tEXt", b"Comment\0late note".to_vec())];
        let src = encoded(&packed, &source, &late);
        let out = encoded(&packed, &info_of(&src), &late);
        let back = info_of(&out);
        assert_eq!(
            back.gama_chunk,
            Some(png::ScaledFloat::from_scaled(100_000))
        );
        assert!(back.chrm_chunk.is_some());
        let order = kinds(&out);
        let idat = order.iter().position(|k| k == "IDAT").unwrap();
        let text = order.iter().position(|k| k == "tEXt").unwrap();
        assert!(text > idat, "{order:?}");
        // sRGB with an ICC profile too: the profile is kept.
        let mut both = png::Info::with_size(4, 4);
        both.color_type = ColorType::Rgb;
        both.srgb = Some(png::SrgbRenderingIntent::Perceptual);
        both.icc_profile = Some(b"a profile".to_vec().into());
        let out = encoded(&packed, &both, &[]);
        let back = info_of(&out);
        assert!(back.icc_profile.is_some() && back.srgb.is_none());
    }

    #[test]
    fn the_walk_finds_animation_color_chunks_and_late_text() {
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
        let found = walk(&mut Cursor::new(apng), 100).unwrap();
        assert!(found.animated && !found.color_bound);
        let hdr = png(&[
            (b"IHDR", ihdr),
            (b"cICP", &[9, 16, 0, 1]),
            (b"tIME", &[7, 232, 5, 1, 0, 0, 0]),
            (b"tEXt", b"Title\0early"),
            (b"IDAT", &[1]),
            (b"acTL", &[0; 8]),
            (b"tEXt", b"Comment\0late"),
            (b"eXIf", b"MM\0*"),
            (b"IEND", &[]),
        ]);
        let found = walk(&mut Cursor::new(hdr.clone()), 100).unwrap();
        assert!(!found.animated && found.color_bound);
        assert_eq!(found.raw.len(), 2);
        let after: Vec<&[u8; 4]> = found.after.iter().map(|(k, _)| k).collect();
        assert_eq!(after, [b"tEXt", b"eXIf"]);
        assert_eq!(found.after_bytes, 16);
        // "Title\0early": the text before the image data.
        assert_eq!((found.text_bytes, found.text_chunks), (11, 1));
        assert!(!found.too_much);
        // Over the limit, the walk stops at the text that passes it.
        let found = walk(&mut Cursor::new(hdr), 15).unwrap();
        assert!(found.too_much);
        assert_eq!(found.after_bytes, 16);
        assert!(walk(&mut Cursor::new(b"\x89PNG\r\n\x1a\n\0\0".to_vec()), 100).is_err());
    }

    #[test]
    fn the_peak_is_the_image_its_packed_form_and_the_encoders_rows() {
        assert_eq!(peak(500, 7, 3), 1000 + 3 * 8 + DEFLATER + 2 * 3);
        // One row of 134 M gray pixels is held three more times while
        // it's written, so 128 MiB decoded counts as about 641 MiB.
        let row = 134_000_000;
        assert_eq!(peak(row, row, 0), 5 * row + 3 + DEFLATER);
        assert!(peak(row, row, 0) <= crate::image::WORKING_SET_LIMIT);
        assert!(peak(2 * row, 2 * row, 0) > crate::image::WORKING_SET_LIMIT);
    }

    #[test]
    fn rows_wider_than_a_span_pack_whole() {
        let width = SPAN as u32 * 2 + 3;
        let gray = GrayImage::from_fn(width, 2, |x, y| Luma([((x * 7 + y) % 4 * 85) as u8]));
        let rgb = RgbImage::from_fn(width, 2, |x, y| {
            Rgb([[9, 0, 0], [0, 9, 0], [0, 0, 9]][((x + y) % 3) as usize])
        });
        for img in [DynamicImage::ImageLuma8(gray), DynamicImage::ImageRgb8(rgb)] {
            let packed = reduced(&img, true);
            assert!(matches!(packed.depth, BitDepth::Two), "{:?}", packed.color);
            assert_eq!(decoded(&packed, width, 2), img.to_rgba8());
        }
    }

    #[test]
    fn a_scan_reads_spans_and_checks_the_cancel_before_each() {
        let width = SPAN as u32 * 2 + 5;
        let img =
            DynamicImage::ImageLuma8(GrayImage::from_fn(width, 2, |x, y| Luma([(x + y) as u8])));
        let mut seen = Vec::new();
        let pass = scan(&img, &CancelToken::default(), |x0, span| {
            assert_eq!(span[0][0] / 257, ((x0 + seen.len() / 3) % 256) as u16);
            seen.push((x0, span.len()));
            true
        });
        assert!(matches!(pass, Scan::Done));
        let row = [(0, SPAN), (SPAN, SPAN), (2 * SPAN, 5)];
        assert_eq!(seen, [row, row].concat());
        // A cancel inside one row stops it at the next span.
        let cancel = CancelToken::default();
        let mut calls = 0;
        let pass = scan(&img, &cancel, |_, _| {
            calls += 1;
            cancel.cancel();
            true
        });
        assert!(matches!(pass, Scan::Cancelled));
        assert_eq!(calls, 1);
    }
}
