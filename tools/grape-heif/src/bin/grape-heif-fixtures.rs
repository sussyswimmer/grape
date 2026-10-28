//! `grape-heif-fixtures <out-dir> [--icc <file>]`: writes the HEIC test
//! fixtures of heic-spec §5.6 with libheif directly, at quality 90 and 4:2:0.
//! CI's grape-heif workflow runs it and uploads the folder for the heif-it
//! tests on Windows and macOS. It never ships.
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

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use libheif_rs::{
    color_profile_types, Channel, ColorProfileRaw, ColorSpace, CompressionFormat,
    EncoderParameterValue, EncoderQuality, EncodingOptions, HeifContext, Image, ImageOrientation,
    LibHeif, RgbChroma,
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

    let opaque = encode(
        &lib,
        &rgb(128, 96, |x, y| BLOCKS[(y / 32 * 4 + x / 32) as usize]),
        None,
        None,
    )?;
    write(&out, "opaque.heic", &opaque)?;

    let alpha = rgba(64, 64, |x, _| {
        if x < 32 {
            [255, 0, 0, 255]
        } else {
            [255, 0, 0, 128]
        }
    });
    write(&out, "alpha.heic", &encode(&lib, &alpha, None, None)?)?;

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
        &encode(&lib, &stored, None, Some(ImageOrientation::Rotate90Cw))?,
    )?;

    write(&out, "grid.heic", &grid(&lib)?)?;
    write(
        &out,
        "odd.heic",
        &encode(&lib, &rgb(3, 5, |_, _| [0, 200, 0]), None, None)?,
    )?;

    match &icc {
        Some(profile) => {
            let gray = rgb(64, 64, |_, _| [128, 128, 128]);
            write(&out, "icc.heic", &encode(&lib, &gray, Some(profile), None)?)?;
        }
        None => eprintln!("grape-heif-fixtures: no --icc, so no icc.heic"),
    }

    write(&out, "truncated.heic", &opaque[..opaque.len() * 6 / 10])?;

    let mut avci = Vec::with_capacity(32);
    avci.extend_from_slice(&24u32.to_be_bytes());
    avci.extend_from_slice(b"ftypavci");
    avci.extend_from_slice(&0u32.to_be_bytes());
    avci.extend_from_slice(b"mif1avci");
    avci.resize(32, 0);
    write(&out, "avci-brand.heic", &avci)?;
    Ok(())
}

/// A new file only: a fixture folder is never written over.
fn write(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = dir.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("creating {}: {e}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    println!("{name}: {} bytes", bytes.len());
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

fn encoder(lib: &LibHeif) -> Result<libheif_rs::Encoder<'_>> {
    let mut encoder = lib
        .encoder_for_format(CompressionFormat::Hevc)
        .map_err(|e| e.to_string())?;
    encoder
        .set_quality(EncoderQuality::Lossy(QUALITY))
        .map_err(|e| e.to_string())?;
    encoder
        .set_parameter_value("chroma", EncoderParameterValue::String("420".to_string()))
        .map_err(|e| e.to_string())?;
    Ok(encoder)
}

fn encode(
    lib: &LibHeif,
    p: &Picture,
    icc: Option<&[u8]>,
    orientation: Option<ImageOrientation>,
) -> Result<Vec<u8>> {
    let image = image(p, icc)?;
    let mut encoder = encoder(lib)?;
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

fn grid(lib: &LibHeif) -> Result<Vec<u8>> {
    let tiles = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]]
        .into_iter()
        .map(|color| image(&rgb(64, 64, |_, _| color), None))
        .collect::<Result<Vec<_>>>()?;
    let mut encoder = encoder(lib)?;
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
