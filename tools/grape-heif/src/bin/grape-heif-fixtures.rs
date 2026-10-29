//! `grape-heif-fixtures <out-dir> [--icc <file>]`: writes the HEIC and AVIF
//! test fixtures of heic-spec §5.6, D41 and D28 with libheif directly, at
//! quality 90 (`q95.heic` 95) and 4:2:0: x265 for HEIC, libaom for AVIF.
//! CI's grape-heif workflow runs it and uploads the folder for the heif-it
//! tests on Windows and macOS (where ImageIO reads the AVIF ones). It never
//! ships. Every file is sniffed (grape-heif-proto's `sniff`) before it's
//! written, so a fixture that isn't what its name says stops the run, and
//! D28's are read back with libheif first, so one that lacks what it's for
//! (its Exif, its thumbnail, its `nclx`) stops it too.
//!
//! | File | What |
//! |---|---|
//! | `opaque.heic` | 128×96, a 4×3 grid of 32×32 solid blocks ([`BLOCKS`]) |
//! | `alpha.heic` | 64×64; left half (255,0,0,255), right half (255,0,0,128) straight |
//! | `rotate90.heic` | 96×64 stored, top-left 16×16 red, rest white, rotated 90° clockwise (irot): decodes as 64×96 |
//! | `grid.heic` | a 2×2 grid of 64×64 tiles: red, green / blue, white |
//! | `odd.heic` | 3×5 solid (0,200,0) |
//! | `icc.heic` | 64×64 gray 128 with the ICC profile from `--icc` (only with `--icc`) |
//! | `truncated.heic` | the first 60% of `opaque.heic` |
//! | `avci-brand.heic` | 32 bytes: an `ftyp` with major `avci`, compatible `mif1 avci`, then zeros: AVC, a HEIF codec no helper reads (D41; it replaces `avif-brand.heic`, which protocol 2 sniffs as AVIF) |
//! | `opaque.avif` | as `opaque.heic`, in AV1 |
//! | `alpha.avif` | as `alpha.heic`: the alpha is an auxiliary AV1 image |
//! | `odd-3x5.avif` | 3×5 solid (0,200,0) |
//! | `deep.avif` | 64×48 in 10 bits: top half (1023,0,0), bottom half (512,512,512); about (255,0,0) and (128,128,128) once decoded to 8 bits |
//! | `grid.avif` | as `grid.heic`, with AV1 tiles; it sniffs as AVIF, or as HEIC when libheif gives the grid no codec brand (the run's log says which) |
//! | `icc.avif` | as `icc.heic` (only with `--icc`) |
//! | `damaged.avif` | the first half of `opaque.avif` |
//! | `exif-xmp.heic` | as `rotate90.heic` (decodes as 64×96), as a phone writes one: an Exif item (`Exif\0\0`, then big-endian TIFF: Make, Model, Orientation 6, an Exif IFD with ExifVersion 0232, DateTimeOriginal and PixelXDimension × PixelYDimension 96 × 64, the stored size; a GPS IFD) and an XMP item (`tiff:Orientation="6"`, Make, Model, DateTimeOriginal, GPS), for D28's metadata graft |
//! | `thumb.heic` | 320×240 gradients under a checkerboard, with a thumbnail item at most 64 a side (`heif_context_encode_thumbnail`) |
//! | `nclx-p3.heic` | 64×64 red, green and blue quarters and a white one, its color only an `nclx` box: primaries 12 (Display P3), transfer 13 (sRGB), matrix 6, full range; no ICC profile |
//! | `q95.heic` | 1600×1200 of fine detail (noise over gradients and a checkerboard) at quality 95, so Compress has something to shrink |

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use grape_heif_proto::{sniff, HeifFormat, Sniff};
use libheif_rs::{
    color_profile_types, Channel, ColorPrimaries, ColorProfileNCLX, ColorProfileRaw, ColorSpace,
    CompressionFormat, EncoderParameterValue, EncoderQuality, EncodingOptions, HeifContext, Image,
    ImageOrientation, LibHeif, RgbChroma, TransferCharacteristics,
};

/// `opaque.heic`'s blocks, row by row (4 across, 3 down).
const BLOCKS: [[u8; 3]; 12] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 255],
    [0, 0, 0],
    [128, 128, 128],
    [255, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [255, 128, 0],
    [128, 0, 255],
    [0, 128, 128],
];

const QUALITY: u8 = 90;

/// `q95.heic`'s quality (D28).
const QUALITY_95: u8 = 95;

/// The bounding box of `thumb.heic`'s thumbnail.
const THUMB_BOX: u32 = 64;

/// `exif-xmp.heic`'s Make.
const MAKE: &str = "Grape";

/// `exif-xmp.heic`'s Model.
const MODEL: &str = "Fixture Camera";

/// `exif-xmp.heic`'s stored size: its `ispe`, and its Exif's
/// PixelXDimension and PixelYDimension, as a phone writes them for a
/// picture it stores sideways (Orientation 6).
const STORED: (u32, u32) = (96, 64);

/// `exif-xmp.heic`'s XMP packet: what a phone writes beside its Exif, with
/// the same Orientation (6: turn 90° clockwise to show).
const XMP: &str = concat!(
    "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n",
    "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
    " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
    "  <rdf:Description rdf:about=\"\"\n",
    "    xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"\n",
    "    xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"\n",
    "    tiff:Orientation=\"6\"\n",
    "    tiff:Make=\"Grape\"\n",
    "    tiff:Model=\"Fixture Camera\"\n",
    "    exif:DateTimeOriginal=\"2026-10-28T12:34:56\"\n",
    "    exif:GPSLatitude=\"48,51.4N\"\n",
    "    exif:GPSLongitude=\"2,17.6667E\"/>\n",
    " </rdf:RDF>\n",
    "</x:xmpmeta>\n",
    "<?xpacket end=\"w\"?>",
);

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("grape-heif-fixtures: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<OsString>) -> Result<()> {
    let usage = "usage: grape-heif-fixtures <out-dir> [--icc <file>]";
    let mut args = args.into_iter();
    let out = PathBuf::from(args.next().ok_or(usage)?);
    let mut icc = None;
    while let Some(arg) = args.next() {
        if arg == "--icc" {
            let file = args.next().ok_or(usage)?;
            icc = Some(std::fs::read(&file).map_err(|e| format!("reading the ICC profile: {e}"))?);
        } else {
            return Err(usage.to_string());
        }
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("making {}: {e}", out.display()))?;
    let lib = LibHeif::new_checked().map_err(|e| e.to_string())?;

    let heic = HeifFormat::Heic;
    let avif = HeifFormat::Avif;
    let blocks = rgb(128, 96, |x, y| BLOCKS[(y / 32 * 4 + x / 32) as usize]);
    let opaque = encode(&lib, heic, &blocks, None, None)?;
    write(&out, "opaque.heic", &opaque, &[Sniff::Heic])?;

    let alpha = rgba(64, 64, |x, _| {
        if x < 32 {
            [255, 0, 0, 255]
        } else {
            [255, 0, 0, 128]
        }
    });
    write(
        &out,
        "alpha.heic",
        &encode(&lib, heic, &alpha, None, None)?,
        &[Sniff::Heic],
    )?;

    let stored = rgb(96, 64, |x, y| {
        if x < 16 && y < 16 {
            [255, 0, 0]
        } else {
            [255, 255, 255]
        }
    });
    write(
        &out,
        "rotate90.heic",
        &encode(
            &lib,
            heic,
            &stored,
            None,
            Some(ImageOrientation::Rotate90Cw),
        )?,
        &[Sniff::Heic],
    )?;

    write(&out, "grid.heic", &grid(&lib, heic)?, &[Sniff::Heic])?;
    let odd = rgb(3, 5, |_, _| [0, 200, 0]);
    write(
        &out,
        "odd.heic",
        &encode(&lib, heic, &odd, None, None)?,
        &[Sniff::Heic],
    )?;

    let gray = rgb(64, 64, |_, _| [128, 128, 128]);
    match &icc {
        Some(profile) => write(
            &out,
            "icc.heic",
            &encode(&lib, heic, &gray, Some(profile), None)?,
            &[Sniff::Heic],
        )?,
        None => eprintln!("grape-heif-fixtures: no --icc, so no icc.heic or icc.avif"),
    }

    write(
        &out,
        "truncated.heic",
        &opaque[..opaque.len() * 6 / 10],
        &[Sniff::Heic],
    )?;

    let mut avci = Vec::with_capacity(32);
    avci.extend_from_slice(&24u32.to_be_bytes());
    avci.extend_from_slice(b"ftypavci");
    avci.extend_from_slice(&0u32.to_be_bytes());
    avci.extend_from_slice(b"mif1avci");
    avci.resize(32, 0);
    write(&out, "avci-brand.heic", &avci, &[Sniff::OtherHeif])?;

    // AVIF (D41).
    let opaque = encode(&lib, avif, &blocks, None, None)?;
    write(&out, "opaque.avif", &opaque, &[Sniff::Avif])?;
    write(
        &out,
        "alpha.avif",
        &encode(&lib, avif, &alpha, None, None)?,
        &[Sniff::Avif],
    )?;
    write(
        &out,
        "odd-3x5.avif",
        &encode(&lib, avif, &odd, None, None)?,
        &[Sniff::Avif],
    )?;
    write(&out, "deep.avif", &deep(&lib)?, &[Sniff::Avif])?;
    // libheif 1.23 can write a grid with no codec brand (`mif1` + `[mif1,
    // miaf]`), which sniffs as HEIC and goes to the helper all the same
    // (D33's 2026-10-26 note).
    write(
        &out,
        "grid.avif",
        &grid(&lib, avif)?,
        &[Sniff::Avif, Sniff::Heic],
    )?;
    if let Some(profile) = &icc {
        write(
            &out,
            "icc.avif",
            &encode(&lib, avif, &gray, Some(profile), None)?,
            &[Sniff::Avif],
        )?;
    }
    write(
        &out,
        "damaged.avif",
        &opaque[..opaque.len() / 2],
        &[Sniff::Avif],
    )?;

    // HEIC Compress and Remove Metadata (D28).
    write(
        &out,
        "exif-xmp.heic",
        &exif_xmp(&lib, &stored)?,
        &[Sniff::Heic],
    )?;
    write(&out, "thumb.heic", &thumb(&lib)?, &[Sniff::Heic])?;
    write(&out, "nclx-p3.heic", &nclx_p3(&lib)?, &[Sniff::Heic])?;
    let detail = rgb(1600, 1200, |x, y| {
        // A hash of the position: noise of 0 to 63 in every channel.
        let hash =
            (x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77)).wrapping_mul(0xC2B2_AE3D);
        let noise = (hash >> 26) as u8;
        let checker = if (x / 8 + y / 8) % 2 == 0 { 128 } else { 0 };
        [
            (x * 127 / 1599) as u8 + noise,
            (y * 127 / 1199) as u8 + noise,
            checker + noise,
        ]
    });
    write(
        &out,
        "q95.heic",
        &encode_at(&lib, heic, &detail, None, None, QUALITY_95)?,
        &[Sniff::Heic],
    )?;
    Ok(())
}

/// A new file only: a fixture folder is never written over. `bytes` must
/// sniff as one of `kinds`, as Grape's helpers will take them; the log says
/// which.
fn write(dir: &Path, name: &str, bytes: &[u8], kinds: &[Sniff]) -> Result<()> {
    let found = sniff(bytes);
    if !kinds.contains(&found) {
        return Err(format!("{name} sniffs as {found:?}, not {kinds:?}"));
    }
    let path = dir.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("creating {}: {e}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    println!("{name}: {} bytes, sniffs as {found:?}", bytes.len());
    Ok(())
}

fn rgb(width: u32, height: u32, color: impl Fn(u32, u32) -> [u8; 3]) -> Picture {
    let mut data = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&color(x, y));
        }
    }
    Picture {
        width,
        height,
        channels: 3,
        data,
    }
}

fn rgba(width: u32, height: u32, color: impl Fn(u32, u32) -> [u8; 4]) -> Picture {
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&color(x, y));
        }
    }
    Picture {
        width,
        height,
        channels: 4,
        data,
    }
}

struct Picture {
    width: u32,
    height: u32,
    channels: u8,
    data: Vec<u8>,
}

fn image(p: &Picture, icc: Option<&[u8]>) -> Result<Image> {
    let chroma = if p.channels == 4 {
        RgbChroma::Rgba
    } else {
        RgbChroma::Rgb
    };
    let mut image =
        Image::new(p.width, p.height, ColorSpace::Rgb(chroma)).map_err(|e| e.to_string())?;
    image
        .create_plane(Channel::Interleaved, p.width, p.height, 8)
        .map_err(|e| e.to_string())?;
    let row_len = p.width as usize * usize::from(p.channels);
    {
        let mut planes = image.planes_mut();
        let plane = planes.interleaved.as_mut().ok_or("no interleaved plane")?;
        for (y, src) in p.data.chunks_exact(row_len).enumerate() {
            let start = y * plane.stride;
            plane
                .data
                .get_mut(start..start + row_len)
                .ok_or("the plane is too short")?
                .copy_from_slice(src);
        }
    }
    if let Some(icc) = icc {
        image
            .set_color_profile_raw(&ColorProfileRaw::new(
                color_profile_types::PROF,
                icc.to_vec(),
            ))
            .map_err(|e| e.to_string())?;
    }
    Ok(image)
}

fn encoder(lib: &LibHeif, format: HeifFormat, quality: u8) -> Result<libheif_rs::Encoder<'_>> {
    let compression = match format {
        HeifFormat::Heic => CompressionFormat::Hevc,
        HeifFormat::Avif => CompressionFormat::Av1,
    };
    let mut encoder = lib
        .encoder_for_format(compression)
        .map_err(|e| e.to_string())?;
    encoder
        .set_quality(EncoderQuality::Lossy(quality))
        .map_err(|e| e.to_string())?;
    encoder
        .set_parameter_value("chroma", EncoderParameterValue::String("420".to_string()))
        .map_err(|e| e.to_string())?;
    Ok(encoder)
}

fn encode(
    lib: &LibHeif,
    format: HeifFormat,
    p: &Picture,
    icc: Option<&[u8]>,
    orientation: Option<ImageOrientation>,
) -> Result<Vec<u8>> {
    encode_at(lib, format, p, icc, orientation, QUALITY)
}

fn encode_at(
    lib: &LibHeif,
    format: HeifFormat,
    p: &Picture,
    icc: Option<&[u8]>,
    orientation: Option<ImageOrientation>,
    quality: u8,
) -> Result<Vec<u8>> {
    let image = image(p, icc)?;
    let mut encoder = encoder(lib, format, quality)?;
    let mut options = EncodingOptions::new().map_err(|e| e.to_string())?;
    options.set_save_alpha_channel(p.channels == 4);
    if let Some(orientation) = orientation {
        options.set_image_orientation(orientation);
    }
    let mut ctx = HeifContext::new().map_err(|e| e.to_string())?;
    ctx.encode_image(&image, &mut encoder, Some(options))
        .map_err(|e| e.to_string())?;
    ctx.write_to_bytes().map_err(|e| e.to_string())
}

/// `deep.avif`: 10 bits a sample in, libheif's RRGGBB little-endian layout,
/// which libaom encodes as 10-bit 4:2:0.
fn deep(lib: &LibHeif) -> Result<Vec<u8>> {
    let (width, height) = (64u32, 48u32);
    let mut image = Image::new(width, height, ColorSpace::Rgb(RgbChroma::HdrRgbLe))
        .map_err(|e| e.to_string())?;
    image
        .create_plane(Channel::Interleaved, width, height, 10)
        .map_err(|e| e.to_string())?;
    {
        let mut planes = image.planes_mut();
        let plane = planes.interleaved.as_mut().ok_or("no interleaved plane")?;
        let row_len = width as usize * 6;
        for y in 0..height as usize {
            let color: [u16; 3] = if y < height as usize / 2 {
                [1023, 0, 0]
            } else {
                [512, 512, 512]
            };
            let start = y * plane.stride;
            let row = plane
                .data
                .get_mut(start..start + row_len)
                .ok_or("the plane is too short")?;
            for px in row.as_chunks_mut::<6>().0 {
                for (c, v) in color.iter().enumerate() {
                    px[c * 2..c * 2 + 2].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
    }
    let mut encoder = encoder(lib, HeifFormat::Avif, QUALITY)?;
    let mut ctx = HeifContext::new().map_err(|e| e.to_string())?;
    ctx.encode_image(&image, &mut encoder, None)
        .map_err(|e| e.to_string())?;
    ctx.write_to_bytes().map_err(|e| e.to_string())
}

fn grid(lib: &LibHeif, format: HeifFormat) -> Result<Vec<u8>> {
    let tiles = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]]
        .into_iter()
        .map(|color| image(&rgb(64, 64, |_, _| color), None))
        .collect::<Result<Vec<_>>>()?;
    let mut encoder = encoder(lib, format, QUALITY)?;
    let mut ctx = HeifContext::new().map_err(|e| e.to_string())?;
    let rows = NonZeroU16::new(2).ok_or("two rows")?;
    let mut handle = ctx
        .encode_grid(&tiles, rows, &mut encoder, None)
        .map_err(|e| e.to_string())?
        .ok_or("libheif made no grid")?;
    ctx.set_primary_image(&mut handle)
        .map_err(|e| e.to_string())?;
    ctx.write_to_bytes().map_err(|e| e.to_string())
}

/// `exif-xmp.heic`: `stored` (`rotate90.heic`'s picture) turned 90°
/// clockwise by `irot`, with an Exif item ([`exif`]) and an XMP item
/// ([`XMP`]) whose Orientation says the same, as a phone writes them. Read
/// back: one Exif and one XMP block on the primary image, stored 96×64,
/// the Exif block exactly [`exif`]'s behind libheif's offset to its TIFF
/// header, with ExifVersion and the stored size in its Exif IFD.
fn exif_xmp(lib: &LibHeif, stored: &Picture) -> Result<Vec<u8>> {
    let image = image(stored, None)?;
    let mut encoder = encoder(lib, HeifFormat::Heic, QUALITY)?;
    let mut options = EncodingOptions::new().map_err(|e| e.to_string())?;
    options.set_image_orientation(ImageOrientation::Rotate90Cw);
    let mut ctx = HeifContext::new().map_err(|e| e.to_string())?;
    let handle = ctx
        .encode_image(&image, &mut encoder, Some(options))
        .map_err(|e| e.to_string())?;
    ctx.add_exif_metadata(&handle, &exif())
        .map_err(|e| e.to_string())?;
    ctx.add_xmp_metadata(&handle, XMP.as_bytes())
        .map_err(|e| e.to_string())?;
    let bytes = ctx.write_to_bytes().map_err(|e| e.to_string())?;
    {
        let read = HeifContext::read_from_bytes(&bytes).map_err(|e| e.to_string())?;
        let primary = read.primary_image_handle().map_err(|e| e.to_string())?;
        let exif_blocks = primary.number_of_metadata_blocks(b"Exif");
        let xmp_blocks = primary.number_of_metadata_blocks(b"mime");
        if (exif_blocks, xmp_blocks) != (1, 1) {
            return Err(format!(
                "exif-xmp.heic has {exif_blocks} Exif and {xmp_blocks} XMP blocks, not one each"
            ));
        }
        let stored_size = (primary.ispe_width(), primary.ispe_height());
        if stored_size != (96, 64) {
            return Err(format!(
                "exif-xmp.heic is stored {stored_size:?}, not 96×64"
            ));
        }
        let mut ids = [0; 1];
        if primary.metadata_block_ids(&mut ids, b"Exif") != 1 {
            return Err("exif-xmp.heic's Exif block has no ID".to_string());
        }
        let block = primary.metadata(ids[0]).map_err(|e| e.to_string())?;
        check_exif_block(&block)?;
    }
    Ok(bytes)
}

/// `exif-xmp.heic`'s Exif block as libheif stored it: the big-endian
/// offset to the TIFF header (6, past `Exif\0\0`), then [`exif`] as it
/// was given, whose Exif IFD has ExifVersion 0232 and PixelXDimension and
/// PixelYDimension of [`STORED`], each LONG.
fn check_exif_block(block: &[u8]) -> Result<()> {
    let given = exif();
    if block.get(..4) != Some(&6u32.to_be_bytes()[..]) || block.get(4..) != Some(&given[..]) {
        return Err(format!(
            "exif-xmp.heic's Exif block is {} bytes, not the offset 6 and the {} bytes given",
            block.len(),
            given.len()
        ));
    }
    // The stored TIFF, past the offset and `Exif\0\0`.
    let tiff = &block[10..];
    let entries = |at: u32| -> Option<Vec<RawEntry>> {
        let at = usize::try_from(at).ok()?;
        let count = u16::from_be_bytes(tiff.get(at..at + 2)?.try_into().ok()?);
        (0..usize::from(count))
            .map(|n| {
                let e = tiff.get(at + 2 + 12 * n..at + 14 + 12 * n)?;
                Some((
                    u16::from_be_bytes([e[0], e[1]]),
                    u16::from_be_bytes([e[2], e[3]]),
                    u32::from_be_bytes([e[4], e[5], e[6], e[7]]),
                    [e[8], e[9], e[10], e[11]],
                ))
            })
            .collect()
    };
    let exif_ifd = entries(8)
        .and_then(|ifd0| ifd0.into_iter().find(|e| e.0 == 0x8769))
        .and_then(|pointer| entries(u32::from_be_bytes(pointer.3)))
        .ok_or("exif-xmp.heic's Exif has no Exif IFD")?;
    // Each entry as written: tag, type, count and the inline value.
    let wanted = [
        ("ExifVersion", (0x9000, UNDEFINED, 4, *b"0232")),
        ("PixelXDimension", (0xA002, LONG, 1, STORED.0.to_be_bytes())),
        ("PixelYDimension", (0xA003, LONG, 1, STORED.1.to_be_bytes())),
    ];
    for (name, entry) in wanted {
        if !exif_ifd.contains(&entry) {
            return Err(format!("exif-xmp.heic's Exif IFD has no {name} as written"));
        }
    }
    Ok(())
}

/// An IFD entry as read: tag, type, count and the 4 bytes of its value or
/// offset.
type RawEntry = (u16, u16, u32, [u8; 4]);

/// TIFF field types.
const BYTE: u16 = 1;
const ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const RATIONAL: u16 = 5;
const UNDEFINED: u16 = 7;

/// One IFD entry, its value big-endian.
struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    value: Vec<u8>,
}

fn ascii(tag: u16, text: &str) -> Entry {
    let mut value = text.as_bytes().to_vec();
    value.push(0);
    Entry {
        tag,
        kind: ASCII,
        count: value.len() as u32,
        value,
    }
}

fn short(tag: u16, n: u16) -> Entry {
    Entry {
        tag,
        kind: SHORT,
        count: 1,
        value: n.to_be_bytes().to_vec(),
    }
}

fn long(tag: u16, n: u32) -> Entry {
    Entry {
        tag,
        kind: LONG,
        count: 1,
        value: n.to_be_bytes().to_vec(),
    }
}

fn rationals(tag: u16, parts: &[(u32, u32)]) -> Entry {
    let mut value = Vec::with_capacity(parts.len() * 8);
    for (numerator, denominator) in parts {
        value.extend_from_slice(&numerator.to_be_bytes());
        value.extend_from_slice(&denominator.to_be_bytes());
    }
    Entry {
        tag,
        kind: RATIONAL,
        count: parts.len() as u32,
        value,
    }
}

/// A big-endian IFD that starts `at` bytes into its TIFF: the entries (in
/// tag order), no next IFD, then the values longer than 4 bytes, each at an
/// even offset.
fn ifd(entries: &[Entry], at: u32) -> Vec<u8> {
    let values_at = at + 2 + 12 * entries.len() as u32 + 4;
    let mut table = (entries.len() as u16).to_be_bytes().to_vec();
    let mut values = Vec::new();
    for entry in entries {
        table.extend_from_slice(&entry.tag.to_be_bytes());
        table.extend_from_slice(&entry.kind.to_be_bytes());
        table.extend_from_slice(&entry.count.to_be_bytes());
        if entry.value.len() <= 4 {
            let mut inline = entry.value.clone();
            inline.resize(4, 0);
            table.extend_from_slice(&inline);
        } else {
            table.extend_from_slice(&(values_at + values.len() as u32).to_be_bytes());
            values.extend_from_slice(&entry.value);
            if values.len() % 2 == 1 {
                values.push(0);
            }
        }
    }
    table.extend_from_slice(&0u32.to_be_bytes());
    table.extend_from_slice(&values);
    table
}

/// `exif-xmp.heic`'s Exif: `Exif\0\0`, then a big-endian TIFF, as an
/// iPhone's HEIC holds it. IFD0: Make, Model, Orientation 6 and the
/// pointers to the Exif IFD (ExifVersion 0232, DateTimeOriginal, and
/// PixelXDimension and PixelYDimension: the stored size, [`STORED`], as
/// phones write it, so D28's graft has dimensions to patch) and the GPS
/// IFD (version 2.3, 48°51'24" N, 2°17'40" E). libheif stores it after the
/// 4-byte offset of the TIFF header (6).
fn exif() -> Vec<u8> {
    let ifd0 = |exif_at: u32, gps_at: u32| {
        [
            ascii(0x010F, MAKE),
            ascii(0x0110, MODEL),
            short(0x0112, 6),
            long(0x8769, exif_at),
            long(0x8825, gps_at),
        ]
    };
    // The pointers are inline, so IFD0's length doesn't depend on them.
    let exif_at = 8 + ifd(&ifd0(0, 0), 8).len() as u32;
    let exif_ifd = ifd(
        &[
            Entry {
                tag: 0x9000,
                kind: UNDEFINED,
                count: 4,
                value: b"0232".to_vec(),
            },
            ascii(0x9003, "2026:10:28 12:34:56"),
            long(0xA002, STORED.0),
            long(0xA003, STORED.1),
        ],
        exif_at,
    );
    let gps_at = exif_at + exif_ifd.len() as u32;
    let gps_ifd = ifd(
        &[
            Entry {
                tag: 0x0000,
                kind: BYTE,
                count: 4,
                value: vec![2, 3, 0, 0],
            },
            ascii(0x0001, "N"),
            rationals(0x0002, &[(48, 1), (51, 1), (2400, 100)]),
            ascii(0x0003, "E"),
            rationals(0x0004, &[(2, 1), (17, 1), (4000, 100)]),
        ],
        gps_at,
    );
    let mut data = b"Exif\0\0MM\0\x2a".to_vec();
    data.extend_from_slice(&8u32.to_be_bytes());
    data.extend_from_slice(&ifd(&ifd0(exif_at, gps_at), 8));
    data.extend_from_slice(&exif_ifd);
    data.extend_from_slice(&gps_ifd);
    data
}

/// `thumb.heic`: 320×240 and a thumbnail item at most [`THUMB_BOX`] a
/// side, which libheif encodes and assigns to the primary image. Read
/// back: the primary image has one thumbnail.
fn thumb(lib: &LibHeif) -> Result<Vec<u8>> {
    let p = rgb(320, 240, |x, y| {
        [
            (x * 255 / 319) as u8,
            (y * 255 / 239) as u8,
            if (x / 40 + y / 40) % 2 == 0 { 255 } else { 0 },
        ]
    });
    let image = image(&p, None)?;
    let mut encoder = encoder(lib, HeifFormat::Heic, QUALITY)?;
    let options = EncodingOptions::new().map_err(|e| e.to_string())?;
    let mut ctx = HeifContext::new().map_err(|e| e.to_string())?;
    let handle = ctx
        .encode_image(&image, &mut encoder, Some(options))
        .map_err(|e| e.to_string())?;
    ctx.encode_thumbnail(&image, &handle, THUMB_BOX, &mut encoder, None)
        .map_err(|e| e.to_string())?;
    let bytes = ctx.write_to_bytes().map_err(|e| e.to_string())?;
    {
        let read = HeifContext::read_from_bytes(&bytes).map_err(|e| e.to_string())?;
        let primary = read.primary_image_handle().map_err(|e| e.to_string())?;
        let thumbnails = primary.number_of_thumbnails();
        if thumbnails != 1 {
            return Err(format!("thumb.heic has {thumbnails} thumbnails, not one"));
        }
    }
    Ok(bytes)
}

/// `nclx-p3.heic`: its color only an `nclx` profile, Display P3 primaries
/// (12) with the sRGB transfer (13) and libheif's defaults otherwise
/// (matrix 6, full range), and no ICC profile. libheif writes an image's
/// `nclx` as a `colr` box only when it gets encoding options. Read back: a
/// `colr nclx` with 12 and 13, and no ICC profile.
fn nclx_p3(lib: &LibHeif) -> Result<Vec<u8>> {
    let p = rgb(64, 64, |x, y| match (x < 32, y < 32) {
        (true, true) => [255, 0, 0],
        (false, true) => [0, 255, 0],
        (true, false) => [0, 0, 255],
        (false, false) => [255, 255, 255],
    });
    let mut image = image(&p, None)?;
    let mut nclx = ColorProfileNCLX::new().ok_or("libheif gave no nclx profile")?;
    nclx.set_color_primaries(ColorPrimaries::SMPTE_EG_432_1);
    // heif_nclx_color_profile_alloc's default; no setter for it.
    if nclx.transfer_characteristics() != TransferCharacteristics::IEC_61966_2_1 {
        return Err("libheif's nclx default transfer isn't sRGB (13)".to_string());
    }
    image
        .set_color_profile_nclx(&nclx)
        .map_err(|e| e.to_string())?;
    let mut encoder = encoder(lib, HeifFormat::Heic, QUALITY)?;
    let options = EncodingOptions::new().map_err(|e| e.to_string())?;
    let mut ctx = HeifContext::new().map_err(|e| e.to_string())?;
    ctx.encode_image(&image, &mut encoder, Some(options))
        .map_err(|e| e.to_string())?;
    let bytes = ctx.write_to_bytes().map_err(|e| e.to_string())?;
    {
        let read = HeifContext::read_from_bytes(&bytes).map_err(|e| e.to_string())?;
        let primary = read.primary_image_handle().map_err(|e| e.to_string())?;
        if primary.color_profile_raw().is_some() {
            return Err("nclx-p3.heic has an ICC profile".to_string());
        }
        let written = primary
            .color_profile_nclx()
            .ok_or("nclx-p3.heic has no colr nclx box")?;
        let found = (
            written.color_primaries(),
            written.transfer_characteristics(),
        );
        if found
            != (
                ColorPrimaries::SMPTE_EG_432_1,
                TransferCharacteristics::IEC_61966_2_1,
            )
        {
            return Err(format!("nclx-p3.heic's nclx is {found:?}, not P3 and sRGB"));
        }
    }
    Ok(bytes)
}
