//! libheif behind grape-heif-proto's [`Codec`] (heic-spec §5.4).
//!
//! `serve` (in grape-heif-proto) reads the request, checks the paths and
//! limits, sniffs the source and writes the pixel files. This module only
//! turns a HEIC file into upright 8-bit rows, and pixels into HEIC bytes.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;

use grape_heif_proto::{Caps, Codec, FailKind, Failure, ImageInfo, Limits, PixelSink, Pixels};
use libheif_rs::{
    color_profile_types, Channel, ColorProfile, ColorProfileRaw, ColorSpace, CompressionFormat,
    DecodingOptions, EncoderParameterValue, EncoderQuality, EncodingOptions, HeifContext,
    HeifError, HeifErrorCode, HeifErrorSubCode, Image, LibHeif, RgbChroma, SecurityLimits,
    StreamReader,
};

use crate::banner::{self, Components};

/// libheif's quality when a request has none: heif-enc's default, which the
/// x265 plugin turns into CRF 25 with its `slow` preset and `ssim` tune
/// (heic-spec J27).
pub const DEFAULT_QUALITY: u8 = 50;

/// libheif's largest single allocation while reading. In 1.23.7 it's also
/// the NAL size limit libheif hands libde265.
const MAX_MEMORY_BLOCK: u64 = 512 * 1024 * 1024;

/// The longest libheif message a failure carries.
const MAX_DETAIL: usize = 300;

/// libheif, libde265 and x265, linked statically.
pub struct LibheifCodec {
    lib: Option<LibHeif>,
    /// Why it can't run: libheif didn't start, or it isn't the version this
    /// build was made for. Then every op fails with `internal`, and the
    /// banner shows the real version, which fails CI.
    problem: Option<String>,
    components: Components,
}

impl LibheifCodec {
    /// Starts libheif and checks it's the version in this program's version.
    pub fn new() -> LibheifCodec {
        let (lib, mut problem) = match LibHeif::new_checked() {
            Ok(lib) => (Some(lib), None),
            Err(e) => (None, Some(format!("libheif didn't start: {}", detail(&e)))),
        };
        if let Some(lib) = &lib {
            let [a, b, c] = lib.version();
            if banner::expected_libheif() != Some([a, b, c]) {
                problem = Some(format!(
                    "grape-heif {} needs its own libheif, but libheif {a}.{b}.{c} is linked",
                    banner::VERSION
                ));
            }
        }
        let components = Components::read(lib.as_ref());
        LibheifCodec {
            lib,
            problem,
            components,
        }
    }

    /// What the linked libraries say about themselves (the banner).
    pub fn components(&self) -> Components {
        self.components.clone()
    }

    fn ready(&self) -> Result<&LibHeif, Failure> {
        match (&self.lib, &self.problem) {
            (Some(lib), None) => Ok(lib),
            (_, Some(problem)) => Err(Failure::new(FailKind::Internal, problem.clone())),
            (None, None) => Err(Failure::new(FailKind::Internal, "libheif didn't start")),
        }
    }
}

impl Codec for LibheifCodec {
    fn caps(&self) -> Caps {
        let ok = self.ready().is_ok();
        Caps {
            decode: ok,
            encode: ok,
            codec: self.components.codec_name(),
        }
    }

    fn decode(
        &self,
        source: &Path,
        limits: &Limits,
        out: &mut dyn PixelSink,
    ) -> Result<(), Failure> {
        let lib = self.ready()?;
        let file = File::open(source).map_err(|e| io_failure("opening the source", &e))?;
        let len = file
            .metadata()
            .map_err(|e| io_failure("reading the source's size", &e))?
            .len();
        let mut ctx = new_context(limits)?;
        // Never read_from_file: libheif opens that narrow path with an
        // ifstream, which can't open a non-ANSI path on Windows ("ảnh
        // chụp.heic"). Rust's File uses the wide API.
        ctx.read_reader(Box::new(StreamReader::new(BufReader::new(file), len)))
            .map_err(decode_failure)?;
        let handle = ctx.primary_image_handle().map_err(|e| {
            let failure = decode_failure(e);
            if failure.kind == FailKind::TooLarge {
                failure
            } else {
                Failure::new(FailKind::NoImage, failure.detail)
            }
        })?;
        // The size after libheif's transformations (irot, imir, clap), which
        // the decode below applies.
        let width = handle.width();
        let height = handle.height();
        let alpha = handle.has_alpha_channel();
        let channels: u8 = if alpha { 4 } else { 3 };
        // An nclx-only HEIC has no ICC and reads as sRGB (heic-spec J11).
        let icc = handle
            .color_profile_raw()
            .filter(|p| {
                let kind = p.profile_type();
                kind == color_profile_types::PROF || kind == color_profile_types::R_ICC
            })
            .map(|p| p.data);
        let info = ImageInfo {
            width,
            height,
            channels,
            icc_bytes: icc
                .as_ref()
                .map_or(0, |p| u32::try_from(p.len()).unwrap_or(u32::MAX)),
        };
        // Before any pixel is decoded: serve's sink checks the size here.
        out.begin(&info, icc.as_deref())?;

        let mut options = DecodingOptions::new()
            .ok_or_else(|| internal("libheif couldn't make its decoding options"))?;
        // libheif keeps 10 bits by default.
        options.set_convert_hdr_to_8bit(true);
        // Rotation, mirroring and the crop applied: upright pixels.
        options.set_ignore_transformations(false);
        let chroma = if alpha {
            RgbChroma::Rgba
        } else {
            RgbChroma::Rgb
        };
        let image = lib
            .decode(&handle, ColorSpace::Rgb(chroma), Some(options))
            .map_err(decode_failure)?;
        stream_rows(&image, width, height, channels, out)
    }

    fn encode(
        &self,
        pixels: &Pixels,
        quality: Option<u8>,
        _limits: &Limits,
    ) -> Result<Vec<u8>, Failure> {
        // serve has checked the size against the limits, and libheif's
        // security limits apply to reading, not to encoding.
        let lib = self.ready()?;
        let chroma = match pixels.channels {
            3 => RgbChroma::Rgb,
            4 => RgbChroma::Rgba,
            n => return Err(internal(format!("{n} channels to encode"))),
        };
        let mut image = Image::new(pixels.width, pixels.height, ColorSpace::Rgb(chroma))
            .map_err(encode_failure)?;
        image
            .create_plane(Channel::Interleaved, pixels.width, pixels.height, 8)
            .map_err(encode_failure)?;
        copy_rows(&mut image, pixels)?;
        if let Some(icc) = &pixels.icc {
            // A prof box; libheif then writes no nclx box beside it (its
            // macOS workaround, on by default).
            image
                .set_color_profile_raw(&ColorProfileRaw::new(
                    color_profile_types::PROF,
                    icc.clone(),
                ))
                .map_err(encode_failure)?;
        }

        let mut encoder = lib
            .encoder_for_format(CompressionFormat::Hevc)
            .map_err(encode_failure)?;
        encoder
            .set_quality(EncoderQuality::Lossy(quality.unwrap_or(DEFAULT_QUALITY)))
            .map_err(encode_failure)?;
        // 4:2:0, the plugin's default too; preset `slow` and tune `ssim` stay
        // its defaults.
        encoder
            .set_parameter_value("chroma", EncoderParameterValue::String("420".to_string()))
            .map_err(encode_failure)?;
        // One still picture gains nothing from x265's frame threads, which it
        // otherwise sizes by core count (memory, heic-spec J24).
        encoder
            .set_parameter_value(
                "x265:frame-threads",
                EncoderParameterValue::String("1".to_string()),
            )
            .map_err(encode_failure)?;
        let mut options = EncodingOptions::new().map_err(encode_failure)?;
        options.set_save_alpha_channel(pixels.channels == 4);

        let mut ctx = HeifContext::new().map_err(encode_failure)?;
        ctx.encode_image(&image, &mut encoder, Some(options))
            .map_err(encode_failure)?;
        ctx.write_to_bytes().map_err(encode_failure)
    }
}

/// libheif's security limits for one decode: its compiled-in defaults
/// (`heif_get_global_security_limits`), with Grape's four on top.
///
/// Never start from a new context's own limits: with
/// `LIBHEIF_SECURITY_LIMITS=off` in the environment they're all disabled,
/// and `set_security_limits` copies the whole set (heic-spec J38). Grape
/// also removes the variable when it starts the helper.
pub fn security_limits(limits: &Limits) -> SecurityLimits {
    let mut s = SecurityLimits::new();
    s.set_max_image_size_pixels(limits.max_pixels);
    s.set_max_memory_block_size(MAX_MEMORY_BLOCK.min(limits.max_decoder_memory));
    s.set_max_total_memory(limits.max_decoder_memory);
    s.set_max_color_profile_size(limits.max_icc_bytes);
    s
}

/// A new context with [`security_limits`], set before anything is read.
fn new_context(limits: &Limits) -> Result<HeifContext<'static>, Failure> {
    let mut ctx = HeifContext::new().map_err(decode_failure)?;
    ctx.set_security_limits(&security_limits(limits))
        .map_err(decode_failure)?;
    Ok(ctx)
}

/// Hands the decoded rows to the sink, top first, straight alpha.
fn stream_rows(
    image: &Image,
    width: u32,
    height: u32,
    channels: u8,
    out: &mut dyn PixelSink,
) -> Result<(), Failure> {
    if image.width() != width || image.height() != height {
        return Err(internal(format!(
            "libheif decoded {}x{}, not the {width}x{height} it reported",
            image.width(),
            image.height()
        )));
    }
    let planes = image.planes();
    let plane = planes
        .interleaved
        .ok_or_else(|| internal("libheif gave no interleaved RGB plane"))?;
    if plane.bits_per_pixel != 8 {
        return Err(internal(format!(
            "libheif gave {} bits per sample, not 8",
            plane.bits_per_pixel
        )));
    }
    let row_len = row_len(width, channels)?;
    if plane.width != width || plane.height != height || plane.stride < row_len {
        return Err(internal("libheif's plane doesn't match the image"));
    }
    let premultiplied = channels == 4 && image.is_premultiplied_alpha();
    let mut scratch = if premultiplied {
        vec![0u8; row_len]
    } else {
        Vec::new()
    };
    for y in 0..plane.height as usize {
        let row = y
            .checked_mul(plane.stride)
            .and_then(|start| plane.data.get(start..start.checked_add(row_len)?))
            .ok_or_else(|| internal("libheif's plane is shorter than the image"))?;
        if premultiplied {
            scratch.copy_from_slice(row);
            unpremultiply(&mut scratch);
            out.row(&scratch)?;
        } else {
            out.row(row)?;
        }
    }
    Ok(())
}

/// Copies the pixel file's rows into libheif's plane, which may be padded.
fn copy_rows(image: &mut Image, pixels: &Pixels) -> Result<(), Failure> {
    let row_len = row_len(pixels.width, pixels.channels)?;
    let mut planes = image.planes_mut();
    let plane = planes
        .interleaved
        .as_mut()
        .ok_or_else(|| internal("libheif made no interleaved RGB plane"))?;
    if plane.stride < row_len {
        return Err(internal("libheif's plane is narrower than the image"));
    }
    let rows = pixels.data.chunks_exact(row_len);
    if rows.len() != pixels.height as usize || !rows.remainder().is_empty() {
        return Err(internal("the pixels don't match their size"));
    }
    for (y, src) in rows.enumerate() {
        let dest = y
            .checked_mul(plane.stride)
            .and_then(|start| plane.data.get_mut(start..start.checked_add(row_len)?))
            .ok_or_else(|| internal("libheif's plane is shorter than the image"))?;
        dest.copy_from_slice(src);
    }
    Ok(())
}

fn row_len(width: u32, channels: u8) -> Result<usize, Failure> {
    usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(usize::from(channels)))
        .filter(|&len| len > 0)
        .ok_or_else(|| internal("the image's rows don't fit in memory"))
}

/// Premultiplied RGBA to straight alpha, rounding to nearest: a = 0 gives
/// 0,0,0,0, else c = min(255, (c×255 + a/2)/a).
pub fn unpremultiply(rgba: &mut [u8]) {
    let (pixels, _) = rgba.as_chunks_mut::<4>();
    for px in pixels {
        let a = u32::from(px[3]);
        match a {
            0 => *px = [0; 4],
            255 => {}
            _ => {
                for c in &mut px[..3] {
                    let straight = (u32::from(*c) * 255 + a / 2) / a;
                    *c = u8::try_from(straight).unwrap_or(u8::MAX);
                }
            }
        }
    }
}

fn internal(detail: impl Into<String>) -> Failure {
    Failure::new(FailKind::Internal, detail)
}

fn io_failure(what: &str, e: &io::Error) -> Failure {
    Failure::new(FailKind::Read, format!("{what}: {:?}", e.kind()))
}

/// libheif's error as a detail: its codes and its message, which never
/// holds a path, cut to [`MAX_DETAIL`] characters.
fn detail(e: &HeifError) -> String {
    let mut text = format!("{:?}/{:?}: {}", e.code, e.sub_code, e.message);
    if let Some((end, _)) = text.char_indices().nth(MAX_DETAIL) {
        text.truncate(end);
    }
    text
}

fn is_over_a_limit(e: &HeifError) -> bool {
    matches!(e.code, HeifErrorCode::MemoryAllocationError)
        || matches!(e.sub_code, HeifErrorSubCode::SecurityLimitExceeded)
}

/// heic-spec §5.4 step 7.
fn decode_failure(e: HeifError) -> Failure {
    let kind = if is_over_a_limit(&e) {
        FailKind::TooLarge
    } else if matches!(e.code, HeifErrorCode::UnsupportedFileType) {
        FailKind::NotHeif
    } else if matches!(e.code, HeifErrorCode::UnsupportedFeature)
        || matches!(
            e.sub_code,
            HeifErrorSubCode::UnsupportedCodec | HeifErrorSubCode::NoMatchingDecoderInstalled
        )
    {
        FailKind::Unsupported
    } else {
        // Invalid_input, the decoder plugin's errors and the rest: bytes
        // that looked like HEIC but didn't decode.
        FailKind::Damaged
    };
    Failure::new(kind, detail(&e))
}

fn encode_failure(e: HeifError) -> Failure {
    let kind = if is_over_a_limit(&e) {
        FailKind::TooLarge
    } else {
        FailKind::Encode
    };
    Failure::new(kind, detail(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    use grape_heif_proto::{sniff, Sniff};

    /// A new, empty folder under the OS temp folder, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("grape-heif-test-{}-{n}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Collects what a decode hands its sink.
    #[derive(Default)]
    struct Collect {
        info: Option<ImageInfo>,
        icc: Option<Vec<u8>>,
        data: Vec<u8>,
        rows: u32,
    }

    impl PixelSink for Collect {
        fn begin(&mut self, info: &ImageInfo, icc: Option<&[u8]>) -> Result<(), Failure> {
            assert!(self.info.is_none(), "begin twice");
            self.info = Some(*info);
            self.icc = icc.map(<[u8]>::to_vec);
            Ok(())
        }

        fn row(&mut self, row: &[u8]) -> Result<(), Failure> {
            let info = self.info.expect("a row before begin");
            assert_eq!(row.len(), info.width as usize * info.channels as usize);
            self.data.extend_from_slice(row);
            self.rows += 1;
            Ok(())
        }
    }

    fn gradient(width: u32, height: u32, channels: u8) -> Pixels {
        let mut data = Vec::new();
        for y in 0..height {
            for x in 0..width {
                data.push((x * 255 / (width - 1)) as u8);
                data.push((y * 255 / (height - 1)) as u8);
                data.push(128);
                if channels == 4 {
                    data.push(200);
                }
            }
        }
        Pixels {
            width,
            height,
            channels,
            data,
            icc: None,
        }
    }

    fn mean_abs_error(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len());
        let sum: u64 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| u64::from(x.abs_diff(y)))
            .sum();
        sum as f64 / a.len() as f64
    }

    fn round_trip(pixels: &Pixels) -> Collect {
        let codec = LibheifCodec::new();
        let bytes = codec.encode(pixels, None, &Limits::DEFAULT).unwrap();
        assert_eq!(sniff(&bytes), Sniff::Heic);
        let dir = TempDir::new();
        let file = dir.0.join("round trip ảnh.heic");
        std::fs::write(&file, &bytes).unwrap();
        let mut sink = Collect::default();
        codec.decode(&file, &Limits::DEFAULT, &mut sink).unwrap();
        sink
    }

    #[test]
    fn the_codec_runs_against_its_own_libheif() {
        let codec = LibheifCodec::new();
        assert_eq!(codec.problem, None);
        let caps = codec.caps();
        assert!(caps.decode && caps.encode);
        assert_eq!(caps.codec, "libheif 1.23.7 (libde265 1.1.3, x265 4.3)");
    }

    #[test]
    fn a_gradient_survives_a_round_trip() {
        let pixels = gradient(64, 48, 3);
        let sink = round_trip(&pixels);
        let info = sink.info.unwrap();
        assert_eq!((info.width, info.height, info.channels), (64, 48, 3));
        assert_eq!(sink.rows, 48);
        assert!(sink.icc.is_none());
        let mae = mean_abs_error(&pixels.data, &sink.data);
        assert!(mae < 4.0, "mean absolute error {mae}");
    }

    #[test]
    fn alpha_and_icc_survive_a_round_trip() {
        let mut pixels = gradient(64, 48, 4);
        // A made-up profile: libheif stores the bytes as they are.
        let mut icc = vec![0u8; 132];
        icc[12..16].copy_from_slice(b"mntr");
        icc[16..20].copy_from_slice(b"RGB ");
        icc[36..40].copy_from_slice(b"acsp");
        pixels.icc = Some(icc.clone());
        let sink = round_trip(&pixels);
        let info = sink.info.unwrap();
        assert_eq!(info.channels, 4);
        assert_eq!(sink.icc.as_deref(), Some(&icc[..]));
        let alpha: Vec<u8> = sink
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| px[3])
            .collect();
        assert!(alpha.iter().all(|&a| a.abs_diff(200) <= 6), "alpha drifted");
    }

    #[test]
    fn odd_and_tiny_sizes_round_trip() {
        for (w, h) in [(3, 5), (2, 2), (17, 1)] {
            let pixels = Pixels {
                width: w,
                height: h,
                channels: 3,
                data: [0u8, 200, 0].repeat((w * h) as usize),
                icc: None,
            };
            let sink = round_trip(&pixels);
            let info = sink.info.unwrap();
            assert_eq!((info.width, info.height), (w, h));
            assert!(mean_abs_error(&pixels.data, &sink.data) < 8.0, "{w}x{h}");
        }
    }

    #[test]
    fn not_heic_bytes_are_refused_as_not_heif_or_damaged() {
        let dir = TempDir::new();
        let file = dir.0.join("x.heic");
        std::fs::write(&file, b"\0\0\0\x18ftypheic\0\0\0\0mif1heic garbage").unwrap();
        let err = LibheifCodec::new()
            .decode(&file, &Limits::DEFAULT, &mut Collect::default())
            .unwrap_err();
        assert!(
            matches!(
                err.kind,
                FailKind::Damaged | FailKind::NotHeif | FailKind::NoImage
            ),
            "{err:?}"
        );
        assert!(!err.detail.contains("x.heic"));
    }

    #[test]
    fn premultiplied_alpha_is_undone_with_rounding() {
        let mut px = [
            0u8, 0, 0, 0, 10, 20, 30, 0, 64, 32, 0, 128, 255, 255, 255, 255, 200, 1, 0, 100,
        ];
        unpremultiply(&mut px);
        assert_eq!(
            px,
            [0, 0, 0, 0, 0, 0, 0, 0, 128, 64, 0, 128, 255, 255, 255, 255, 255, 3, 0, 100]
        );
    }

    /// libheif's plugin loader is compiled out (`ENABLE_PLUGIN_LOADING=OFF`,
    /// heic-spec §5.5): it answers from `init.cc`'s stub.
    #[test]
    fn plugins_cant_be_loaded() {
        let lib = LibHeif::new_checked().unwrap();
        let dir = TempDir::new();
        let err = lib.load_plugins(&dir.0).unwrap_err();
        assert_eq!(err.message, "Plugins are not supported");
    }

    fn assert_grape_limits(actual: &SecurityLimits) {
        let mut expected = SecurityLimits::new();
        expected.set_max_image_size_pixels(Limits::DEFAULT.max_pixels);
        expected.set_max_memory_block_size(MAX_MEMORY_BLOCK);
        expected.set_max_total_memory(Limits::DEFAULT.max_decoder_memory);
        expected.set_max_color_profile_size(Limits::DEFAULT.max_icc_bytes);
        let fields = |s: &SecurityLimits| {
            [
                s.max_image_size_pixels(),
                s.max_number_of_tiles(),
                u64::from(s.max_bayer_pattern_pixels()),
                u64::from(s.max_items()),
                u64::from(s.max_color_profile_size()),
                s.max_memory_block_size(),
                u64::from(s.max_components()),
                u64::from(s.max_iloc_extents_per_item()),
                u64::from(s.max_size_entity_group()),
                u64::from(s.max_children_per_box()),
                s.max_total_memory(),
                u64::from(s.max_sample_description_box_entries()),
                u64::from(s.max_sample_group_description_box_entries()),
                u64::from(s.max_sequence_frames()),
                u64::from(s.max_number_of_file_brands()),
                u64::from(s.max_bad_pixels()),
                u64::from(s.max_iso23001_17_pixel_size_bytes()),
            ]
        };
        assert_eq!(fields(actual), fields(&expected));
        assert!(
            fields(&expected).iter().all(|&v| v > 0),
            "a default limit is off"
        );
    }

    /// The child half of the next test: run with LIBHEIF_SECURITY_LIMITS=off
    /// in its environment from the start, because the static CRT's getenv,
    /// which libheif reads, never sees a variable set later in-process.
    #[test]
    #[ignore = "run by security_limits_ignore_an_inherited_off"]
    fn security_limits_child() {
        assert_eq!(
            std::env::var("LIBHEIF_SECURITY_LIMITS").as_deref(),
            Ok("off")
        );
        let _lib = LibHeif::new_checked().unwrap();
        // libheif read the variable: a plain new context has no limits.
        let plain = HeifContext::new().unwrap();
        assert_eq!(plain.security_limits().max_image_size_pixels(), 0);
        let ctx = new_context(&Limits::DEFAULT).unwrap();
        assert_grape_limits(&ctx.security_limits());
        println!("grape-heif: security limits ok");
    }

    #[test]
    fn security_limits_ignore_an_inherited_off() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "codec::tests::security_limits_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LIBHEIF_SECURITY_LIMITS", "off")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("grape-heif: security limits ok"),
            "the child failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // And in this process, without the variable.
        let _lib = LibHeif::new_checked().unwrap();
        assert_grape_limits(&new_context(&Limits::DEFAULT).unwrap().security_limits());
    }
}
