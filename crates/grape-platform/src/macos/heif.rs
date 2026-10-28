//! The HEIC worker's codec: ImageIO behind `grape_heif_proto`'s [`Codec`]
//! (DECISIONS D33).
//!
//! Only the worker process runs this: `Grape --grape-heif-worker` (the
//! app's `main.rs`) and the `grape-heif-worker` test bin. Grape's own
//! process never decodes a HEIC, so a crash in ImageIO can't take the tray
//! and the drag with it.
//!
//! - **Decode** takes the primary image, upright, as ImageIO's full-size
//!   "thumbnail" built from the image itself with its transform applied.
//!   Its size and bit depth come from the image's properties first, so a
//!   picture over the pixel limit, or whose decode wouldn't fit the
//!   request's decoder budget (`max_decoder_memory`, as libheif's on
//!   Windows), is refused before anything is decoded. The `image` line
//!   goes out once ImageIO has decoded it (only then is the upright size
//!   certain) and before Grape's own bitmap is made, so the parent can
//!   still stop it there. It's drawn into an 8-bit bitmap in the image's
//!   own RGB color space (with that space's ICC profile), Display P3 for PQ
//!   or HLG, or sRGB for anything else; ImageIO's picture is let go once
//!   it's drawn, and alpha is un-premultiplied row by row.
//! - **Encode** checks that this Mac can write `public.heic` at all
//!   (Macs without a hardware HEVC encoder, and GitHub's macOS VMs, can't),
//!   then writes one picture at ImageIO's lossy quality 0.8 by default. A
//!   finish that fails is the same "this Mac can't write HEIC": that's how
//!   ImageIO fails on hardware without an encoder that still lists the type.
//! - Before anything else it asks of ImageIO (its caps, a decode or an
//!   encode) the worker restricts ImageIO to the HEIF types with
//!   `CGImageSourceSetAllowableTypes` on macOS 14.2 and later, looked up
//!   with `dlsym`: a bound import would stop Grape from launching on
//!   macOS 13.
//!
//! Every op runs in its own autorelease pool, nothing logs, and no failure
//! detail holds a path.

use std::ffi::c_void;
use std::path::Path;
use std::sync::Once;

use grape_heif_proto::{Caps, Codec, FailKind, Failure, ImageInfo, Limits, PixelSink, Pixels};
use objc2::rc::autoreleasepool;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFMutableData, CFNumber, CFRetained, CFString,
    CFType, CGPoint, CGRect, CGSize, CFURL,
};
use objc2_core_graphics::{
    kCGColorSpaceDisplayP3, kCGColorSpaceSRGB, CGBitmapContextCreate, CGBitmapInfo,
    CGColorRenderingIntent, CGColorSpace, CGColorSpaceModel, CGContext, CGDataProvider, CGImage,
    CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_image_io::{
    kCGImageDestinationLossyCompressionQuality, kCGImagePropertyDepth, kCGImagePropertyOrientation,
    kCGImagePropertyPixelHeight, kCGImagePropertyPixelWidth,
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailWithTransform,
    kCGImageSourceShouldAllowFloat, kCGImageSourceShouldCache,
    kCGImageSourceShouldCacheImmediately, kCGImageSourceThumbnailMaxPixelSize,
    kCGImageSourceTypeIdentifierHint, CGImageDestination, CGImageSource, CGImageSourceStatus,
};

/// The HEIC types ImageIO may read in the worker, and the one it writes.
const HEIC: &str = "public.heic";
const HEIF: &str = "public.heif";
const HEICS: &str = "public.heics";

/// ImageIO's lossy quality when the request has none (heic-spec J27).
/// Apple doesn't document ImageIO's own default; 0.8 keeps a HEIC from
/// looking worse than the JPEG 90 it usually replaces.
const DEFAULT_QUALITY: f64 = 0.8;

/// Upright, the only orientation Grape writes: the pixels already are.
const UPRIGHT: i32 = 1;

/// ImageIO on this Mac, as a HEIC [`Codec`] for the worker
/// (`grape_heif_proto::worker_main`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageIo;

impl Codec for ImageIo {
    fn caps(&self) -> Caps {
        // Always before anything else ImageIO does in this process.
        restrict_to_heif();
        autoreleasepool(|_| Caps {
            decode: can_read_heic(),
            encode: can_write_heic(),
            codec: "ImageIO".to_owned(),
        })
    }

    fn decode(
        &self,
        source: &Path,
        limits: &Limits,
        out: &mut dyn PixelSink,
    ) -> Result<(), Failure> {
        restrict_to_heif();
        autoreleasepool(|_| decode(source, limits, out))
    }

    fn encode(
        &self,
        pixels: &Pixels,
        quality: Option<u8>,
        _limits: &Limits,
    ) -> Result<Vec<u8>, Failure> {
        restrict_to_heif();
        autoreleasepool(|_| encode(pixels, quality))
    }
}

/// True when ImageIO can open `public.heic` on this Mac.
fn can_read_heic() -> bool {
    // SAFETY: CGImageSourceCopyTypeIdentifiers takes nothing and returns a
    // new array (the Copy rule), which the binding owns and releases.
    let types = unsafe { CGImageSource::type_identifiers() };
    has_type(&types, HEIC)
}

/// True when ImageIO can write `public.heic` on this Mac: false without a
/// hardware HEVC encoder, and on GitHub's macOS VMs.
fn can_write_heic() -> bool {
    // SAFETY: as in `can_read_heic`, for CGImageDestinationCopyTypeIdentifiers.
    let types = unsafe { CGImageDestination::type_identifiers() };
    has_type(&types, HEIC)
}

/// True when the array of type identifiers holds `uti`.
fn has_type(types: &CFArray, uti: &str) -> bool {
    let wanted = CFString::from_str(uti);
    // SAFETY: every element of a CFArray of CF objects is a CFType, which
    // is all this view claims; each one is checked to be a CFString.
    let types: &CFArray<CFType> = unsafe { types.cast_unchecked() };
    (0..types.len())
        .filter_map(|index| types.get(index))
        .any(|item| {
            item.downcast_ref::<CFString>()
                .is_some_and(|name| *name == *wanted)
        })
}

/// Restricts ImageIO in this process to the HEIF types (heic-spec J16),
/// once, on macOS 14.2 and later. It's looked up with `dlsym`, never
/// imported: objc2-image-io binds `CGImageSourceSetAllowableTypes` as an
/// ordinary import, which dyld refuses on macOS 13, so Grape wouldn't
/// launch there. Older systems skip it. Its `OSStatus` is ignored: the
/// decode works either way, only with a wider attack surface.
fn restrict_to_heif() {
    static RESTRICT: Once = Once::new();
    RESTRICT.call_once(|| {
        // SAFETY: `RTLD_DEFAULT` searches every loaded image, and the name
        // is a NUL-terminated C string; dlsym only looks the symbol up.
        let symbol = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                c"CGImageSourceSetAllowableTypes".as_ptr(),
            )
        };
        if symbol.is_null() {
            return;
        }
        type SetAllowableTypes = unsafe extern "C" fn(*const c_void) -> i32;
        // SAFETY: the symbol is ImageIO's
        // `OSStatus CGImageSourceSetAllowableTypes(CFArrayRef)` (macOS
        // 14.2+, `CGImageSource.h`): one pointer in, an `i32` out.
        let set = unsafe { std::mem::transmute::<*mut c_void, SetAllowableTypes>(symbol) };
        let types = [
            CFString::from_static_str(HEIC),
            CFString::from_static_str(HEIF),
            CFString::from_static_str(HEICS),
        ];
        let array = CFArray::from_retained_objects(&types);
        let array: *const CFArray<CFString> = CFRetained::as_ptr(&array).as_ptr();
        // SAFETY: `array` is a valid CFArray of CFStrings for the whole
        // call (it's released only at the end of this closure), which is
        // the argument the function documents; ImageIO keeps its own copy.
        let _status = unsafe { set(array.cast()) };
    });
}

/// A failure of `kind` with a detail for the log.
fn fail(kind: FailKind, detail: impl Into<String>) -> Failure {
    Failure::new(kind, detail)
}

/// An options dictionary from ImageIO's keys and their values.
fn options(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<&CFString> = pairs.iter().map(|(key, _)| *key).collect();
    let values: Vec<&CFType> = pairs.iter().map(|(_, value)| *value).collect();
    CFDictionary::from_slices(&keys, &values)
}

/// A whole number from a properties dictionary.
fn number(properties: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<i64> {
    properties.get(key)?.downcast_ref::<CFNumber>()?.as_i64()
}

/// The color space Grape draws the image into, and the ICC profile that
/// then describes the pixels (`None`: sRGB).
struct Target {
    space: CFRetained<CGColorSpace>,
    icc: Option<Vec<u8>>,
}

impl Target {
    /// sRGB with no profile: what a missing profile means downstream.
    fn srgb() -> Option<Target> {
        // SAFETY: kCGColorSpaceSRGB is a constant CFString CoreGraphics
        // exports.
        let name = unsafe { kCGColorSpaceSRGB };
        CGColorSpace::with_name(Some(name)).map(|space| Target { space, icc: None })
    }

    /// A named space with its own ICC profile, if CoreGraphics has one
    /// that fits under `max_icc`.
    fn named(name: &CFString, max_icc: usize) -> Option<Target> {
        let space = CGColorSpace::with_name(Some(name))?;
        let icc = icc_of(&space, max_icc)?;
        Some(Target {
            space,
            icc: Some(icc),
        })
    }

    /// Where the image `image` is drawn (heic-spec §7 step 6): its own
    /// space when that's an RGB output space with a profile that fits;
    /// Display P3 for PQ or HLG (J18); otherwise sRGB with no profile
    /// (J19), so the pixels always match what the profile says.
    fn for_image(image: &CGImage, max_icc: usize) -> Option<Target> {
        if let Some(space) = CGImage::color_space(Some(image)) {
            if space.uses_itur_2100_tf() {
                // SAFETY: kCGColorSpaceDisplayP3 is a constant CFString
                // CoreGraphics exports.
                let p3 = unsafe { kCGColorSpaceDisplayP3 };
                if let Some(target) = Target::named(p3, max_icc) {
                    return Some(target);
                }
            } else if CGColorSpace::model(Some(&*space)) == CGColorSpaceModel::RGB
                && space.supports_output()
            {
                if let Some(icc) = icc_of(&space, max_icc) {
                    return Some(Target {
                        space,
                        icc: Some(icc),
                    });
                }
            }
        }
        Target::srgb()
    }
}

/// `space`'s ICC profile, when it has one of 1 to `max_icc` bytes.
fn icc_of(space: &CGColorSpace, max_icc: usize) -> Option<Vec<u8>> {
    let data = CGColorSpace::icc_data(Some(space))?;
    (!data.is_empty() && data.len() <= max_icc).then(|| data.to_vec())
}

/// How the pixels sit in the bitmap Grape draws into: 4 bytes a pixel,
/// R G B then alpha (premultiplied: the only alpha a bitmap context takes)
/// or a skipped byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// RGBX: the image has no alpha. Sent as 3 channels.
    Opaque,
    /// Premultiplied RGBA. Sent as 4 channels, straight alpha.
    Alpha,
}

impl Layout {
    fn of(image: &CGImage) -> Layout {
        match CGImage::alpha_info(Some(image)) {
            CGImageAlphaInfo::None
            | CGImageAlphaInfo::NoneSkipFirst
            | CGImageAlphaInfo::NoneSkipLast => Layout::Opaque,
            _ => Layout::Alpha,
        }
    }

    fn channels(self) -> u8 {
        match self {
            Layout::Opaque => 3,
            Layout::Alpha => 4,
        }
    }

    fn bitmap_info(self) -> u32 {
        let alpha = match self {
            Layout::Opaque => CGImageAlphaInfo::NoneSkipLast,
            Layout::Alpha => CGImageAlphaInfo::PremultipliedLast,
        };
        alpha.0 | CGImageByteOrderInfo::Order32Big.0
    }
}

/// heic-spec §7, decode.
fn decode(source: &Path, limits: &Limits, out: &mut dyn PixelSink) -> Result<(), Failure> {
    let url = CFURL::from_file_path(source)
        .ok_or_else(|| fail(FailKind::Read, "the source path isn't a file URL"))?;
    // SAFETY: the keys are constant CFStrings ImageIO exports.
    let (should_cache, hint) =
        unsafe { (kCGImageSourceShouldCache, kCGImageSourceTypeIdentifierHint) };
    let heic = CFString::from_static_str(HEIC);
    let source_options = options(&[(should_cache, CFBoolean::new(false)), (hint, &heic)]);
    // SAFETY: the options' keys are CFStrings and their values the types
    // ImageIO documents for them (a CFBoolean, a CFString).
    let src = unsafe { CGImageSource::with_url(&url, Some(source_options.as_opaque())) }
        .ok_or_else(|| fail(FailKind::Damaged, "ImageIO couldn't open the source"))?;

    // SAFETY: `src` is a valid image source for these four calls.
    let (kind, status, count, primary) = unsafe {
        (
            src.r#type(),
            src.status(),
            src.count(),
            src.primary_image_index(),
        )
    };
    let heif_types = [HEIC, HEIF, HEICS].map(CFString::from_static_str);
    match kind {
        Some(kind) if heif_types.iter().any(|t| **t == *kind) => {}
        Some(_) => {
            return Err(fail(
                FailKind::Unsupported,
                "ImageIO reads it as another image type",
            ))
        }
        None => return Err(fail(FailKind::NotHeif, "ImageIO doesn't know the type")),
    }
    if status != CGImageSourceStatus::StatusComplete {
        return Err(fail(
            FailKind::Damaged,
            format!("ImageIO's source status is {}", status.0),
        ));
    }
    if count == 0 {
        return Err(fail(FailKind::NoImage, "no images"));
    }
    if primary >= count {
        return Err(fail(FailKind::NoImage, "no primary image"));
    }
    // SAFETY: `src` is valid and `primary` is below its count.
    let image_status = unsafe { src.status_at_index(primary) };
    if image_status != CGImageSourceStatus::StatusComplete {
        // A file cut short: ImageIO could still draw what's there.
        return Err(fail(
            FailKind::Damaged,
            format!("ImageIO's image status is {}", image_status.0),
        ));
    }

    // The size first, from the properties: nothing is decoded yet.
    // SAFETY: `src` is valid and `primary` is below its count; no options.
    let properties = unsafe { src.properties_at_index(primary, None) }
        .ok_or_else(|| fail(FailKind::Damaged, "the image has no properties"))?;
    // SAFETY: an image's properties dictionary has CFString keys and CF
    // objects as values (`CGImageProperties.h`); each value is checked to
    // be a CFNumber before it's read.
    let properties: &CFDictionary<CFString, CFType> = unsafe { properties.cast_unchecked() };
    // SAFETY: the keys are constant CFStrings ImageIO exports.
    let (width_key, height_key) =
        unsafe { (kCGImagePropertyPixelWidth, kCGImagePropertyPixelHeight) };
    let side = |key: &CFString| {
        number(properties, key)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
    };
    let (Some(width), Some(height)) = (side(width_key), side(height_key)) else {
        return Err(fail(FailKind::Damaged, "the image has no pixel size"));
    };
    if u64::from(width) * u64::from(height) > limits.max_pixels {
        return Err(fail(
            FailKind::TooLarge,
            format!("{width}x{height} is over {} pixels", limits.max_pixels),
        ));
    }
    // ImageIO's decode has to fit the decoder budget, as libheif's does on
    // Windows: nothing caps this process's memory.
    // SAFETY: the key is a constant CFString ImageIO exports.
    let depth = number(properties, unsafe { kCGImagePropertyDepth });
    let needs = crate::mac_rules::heif_decode_bytes(width, height, depth);
    if needs > limits.max_decoder_memory {
        return Err(fail(
            FailKind::TooLarge,
            format!(
                "decoding {width}x{height} at {} bits needs about {} MiB, over {} MiB",
                depth.unwrap_or(8),
                needs >> 20,
                limits.max_decoder_memory >> 20
            ),
        ));
    }

    let image = full_size_upright(&src, primary, width.max(height))?;
    let (w, h) = (CGImage::width(Some(&image)), CGImage::height(Some(&image)));
    let (width, height) = (width as usize, height as usize);
    // J17: never a scaled-down picture.
    if (w, h) != (width, height) && (w, h) != (height, width) {
        return Err(fail(
            FailKind::Internal,
            format!("ImageIO gave {w}x{h} for a {width}x{height} image"),
        ));
    }
    draw_rows(image, w, h, limits, out)
}

/// The primary image, decoded at full size with its transform (irot, imir)
/// applied, so it's upright.
fn full_size_upright(
    src: &CGImageSource,
    index: usize,
    longest_side: u32,
) -> Result<CFRetained<CGImage>, Failure> {
    // SAFETY: the keys are constant CFStrings ImageIO exports.
    let (always, transform, max_size, cache_now, allow_float) = unsafe {
        (
            kCGImageSourceCreateThumbnailFromImageAlways,
            kCGImageSourceCreateThumbnailWithTransform,
            kCGImageSourceThumbnailMaxPixelSize,
            kCGImageSourceShouldCacheImmediately,
            kCGImageSourceShouldAllowFloat,
        )
    };
    let longest = CFNumber::new_i64(i64::from(longest_side));
    let thumbnail_options = options(&[
        (always, CFBoolean::new(true)),
        (transform, CFBoolean::new(true)),
        (max_size, &longest),
        (cache_now, CFBoolean::new(true)),
        (allow_float, CFBoolean::new(false)),
    ]);
    // SAFETY: `src` is valid, `index` is below its count, and the options'
    // values are the CFBooleans and the CFNumber ImageIO documents for
    // these keys.
    unsafe { src.thumbnail_at_index(index, Some(thumbnail_options.as_opaque())) }
        .ok_or_else(|| fail(FailKind::Damaged, "ImageIO couldn't decode the image"))
}

/// Draws `image` (`w` × `h`) into an 8-bit bitmap and hands it to `out`,
/// one row at a time, with straight alpha. `out` hears of the image before
/// the bitmap is made, and `image` is let go once it's drawn.
fn draw_rows(
    image: CFRetained<CGImage>,
    w: usize,
    h: usize,
    limits: &Limits,
    out: &mut dyn PixelSink,
) -> Result<(), Failure> {
    let max_icc = usize::try_from(limits.max_icc_bytes).unwrap_or(usize::MAX);
    let layout = Layout::of(&image);
    let target = Target::for_image(&image, max_icc)
        .ok_or_else(|| fail(FailKind::Internal, "no sRGB color space"))?;
    // CoreGraphics takes a space and a layout or not, whatever the size: a
    // one-pixel try settles the target before `begin` names its profile.
    let target = if bitmap_takes(&target, layout) {
        target
    } else {
        // Once more in sRGB, without a profile.
        let srgb = Target::srgb().ok_or_else(|| fail(FailKind::Internal, "no sRGB color space"))?;
        if !bitmap_takes(&srgb, layout) {
            return Err(fail(FailKind::Internal, "no bitmap context"));
        }
        srgb
    };

    let (width, height) = (
        u32::try_from(w).map_err(|_| fail(FailKind::Internal, "the width is over u32"))?,
        u32::try_from(h).map_err(|_| fail(FailKind::Internal, "the height is over u32"))?,
    );
    let icc_bytes = target
        .icc
        .as_ref()
        .map_or(0, |icc| u32::try_from(icc.len()).unwrap_or(0));
    out.begin(
        &ImageInfo {
            width,
            height,
            channels: layout.channels(),
            icc_bytes,
        },
        target.icc.as_deref(),
    )?;

    let stride = w
        .checked_mul(4)
        .ok_or_else(|| fail(FailKind::TooLarge, "the row doesn't fit in memory"))?;
    let len = stride
        .checked_mul(h)
        .ok_or_else(|| fail(FailKind::TooLarge, "the image doesn't fit in memory"))?;
    let mut buffer: Vec<u8> = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| fail(FailKind::TooLarge, "the image doesn't fit in memory"))?;
    buffer.resize(len, 0);
    let context = bitmap(&mut buffer, w, h, stride, &target, layout)
        .ok_or_else(|| fail(FailKind::Internal, "no bitmap context"))?;

    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    CGContext::draw_image(Some(&*context), rect, Some(&*image));
    // The context writes into `buffer`; it's gone before `buffer` is read.
    drop(context);
    // ImageIO's picture isn't needed once it's drawn.
    drop(image);

    // Row 0 of the bitmap is the image's top row.
    let mut row = Vec::with_capacity(w * usize::from(layout.channels()));
    for line in buffer.chunks_exact(stride) {
        row.clear();
        match layout {
            Layout::Opaque => rgbx_to_rgb(line, &mut row),
            Layout::Alpha => unpremultiply(line, &mut row),
        }
        out.row(&row)?;
    }
    Ok(())
}

/// A bitmap context drawing into `buffer` (`h` rows of `stride` bytes) in
/// `target`'s space, or `None` when CoreGraphics refuses that combination.
fn bitmap(
    buffer: &mut [u8],
    w: usize,
    h: usize,
    stride: usize,
    target: &Target,
    layout: Layout,
) -> Option<CFRetained<CGContext>> {
    if buffer.len() < stride.checked_mul(h)? {
        return None;
    }
    // SAFETY: `buffer` holds `h` rows of `stride` (4 × `w`) bytes, checked
    // above, which is what 8 bits per component and 4 components a pixel
    // need. It outlives the context: the caller drops the context before
    // it reads or frees `buffer`, and nothing else touches `buffer` while
    // the context lives.
    unsafe {
        CGBitmapContextCreate(
            buffer.as_mut_ptr().cast(),
            w,
            h,
            8,
            stride,
            Some(&*target.space),
            layout.bitmap_info(),
        )
    }
}

/// True when CoreGraphics makes a bitmap context in `target`'s space with
/// `layout` (tried on one pixel).
fn bitmap_takes(target: &Target, layout: Layout) -> bool {
    let mut pixel = [0u8; 4];
    bitmap(&mut pixel, 1, 1, 4, target, layout).is_some()
}

/// RGBX → RGB: drops every fourth byte.
fn rgbx_to_rgb(line: &[u8], row: &mut Vec<u8>) {
    for pixel in line.as_chunks::<4>().0 {
        row.extend_from_slice(&pixel[..3]);
    }
}

/// Premultiplied RGBA → straight RGBA. A fully transparent pixel is
/// 0,0,0,0; otherwise each color is c × 255 / a, rounded, at most 255.
fn unpremultiply(line: &[u8], row: &mut Vec<u8>) {
    for pixel in line.as_chunks::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            row.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        for &color in &pixel[..3] {
            let straight = (u32::from(color) * 255 + alpha / 2) / alpha;
            row.push(u8::try_from(straight.min(255)).unwrap_or(u8::MAX));
        }
        row.push(pixel[3]);
    }
}

/// ImageIO's quality for the request's: 1..=100 as q / 100, or 0.8.
fn lossy_quality(quality: Option<u8>) -> f64 {
    quality.map_or(DEFAULT_QUALITY, |q| f64::from(q) / 100.0)
}

/// heic-spec §7, encode.
fn encode(pixels: &Pixels, quality: Option<u8>) -> Result<Vec<u8>, Failure> {
    if !can_write_heic() {
        return Err(fail(
            FailKind::NoEncoder,
            "public.heic isn't an image destination type",
        ));
    }
    let channels = usize::from(pixels.channels);
    let alpha = match pixels.channels {
        3 => CGImageAlphaInfo::None,
        4 => CGImageAlphaInfo::Last,
        other => return Err(fail(FailKind::Internal, format!("{other} channels"))),
    };
    let (w, h) = (pixels.width as usize, pixels.height as usize);
    let stride = w
        .checked_mul(channels)
        .ok_or_else(|| fail(FailKind::TooLarge, "the row doesn't fit in memory"))?;
    if stride.checked_mul(h) != Some(pixels.data.len()) {
        return Err(fail(
            FailKind::Internal,
            "the pixels don't match their size",
        ));
    }

    let space = encode_space(pixels.icc.as_deref())
        .ok_or_else(|| fail(FailKind::Encode, "no color space for the pixels"))?;
    let data = CFData::from_bytes(&pixels.data);
    let provider = CGDataProvider::with_cf_data(Some(&*data))
        .ok_or_else(|| fail(FailKind::Encode, "no data provider"))?;
    // SAFETY: the provider holds exactly `h` rows of `stride` bytes (checked
    // above): 8 bits per component, `channels` components a pixel, straight
    // alpha last when there are 4. `decode` is null, which means none.
    let image = unsafe {
        CGImage::new(
            w,
            h,
            8,
            8 * channels,
            stride,
            Some(&*space),
            CGBitmapInfo(alpha.0 | CGImageByteOrderInfo::OrderDefault.0),
            Some(&*provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
    .ok_or_else(|| fail(FailKind::Encode, "CoreGraphics refused the pixels"))?;

    let out = CFMutableData::new(None, 0)
        .ok_or_else(|| fail(FailKind::Internal, "no memory for the output"))?;
    let heic = CFString::from_static_str(HEIC);
    // SAFETY: `out` is a valid mutable data, the type a CFString, one image
    // and no options.
    let destination = unsafe { CGImageDestination::with_data(&out, &heic, 1, None) }
        // A Mac whose list says yes but can't make one, or a VM.
        .ok_or_else(|| fail(FailKind::NoEncoder, "ImageIO made no HEIC destination"))?;
    // SAFETY: the keys are constant CFStrings ImageIO exports.
    let (quality_key, orientation_key) = unsafe {
        (
            kCGImageDestinationLossyCompressionQuality,
            kCGImagePropertyOrientation,
        )
    };
    let quality = CFNumber::new_f64(lossy_quality(quality));
    let upright = CFNumber::new_i32(UPRIGHT);
    let properties = options(&[(quality_key, &quality), (orientation_key, &upright)]);
    // SAFETY: `destination` and `image` are valid, and the properties'
    // values are the CFNumbers ImageIO documents for these keys.
    let finished = unsafe {
        destination.add_image(&image, Some(properties.as_opaque()));
        destination.finalize()
    };
    if !finished {
        return Err(finish_failed());
    }
    drop(destination);
    Ok(out.to_vec())
}

/// A `CGImageDestinationFinalize` that returned false for `public.heic`.
/// That's how ImageIO fails on a Mac or VM without a HEVC encoder that
/// still lists the type and makes the destination ("CGImageDestinationFinalize
/// failed for output type 'public.heic'"), so it's "this Mac can't write
/// HEIC files", not a failed write.
fn finish_failed() -> Failure {
    fail(
        FailKind::NoEncoder,
        "ImageIO couldn't finish the HEIC (no HEVC encoder?)",
    )
}

/// The space the pixels are in: their ICC profile's when it's an RGB one,
/// else sRGB.
fn encode_space(icc: Option<&[u8]>) -> Option<CFRetained<CGColorSpace>> {
    if let Some(icc) = icc {
        let data = CFData::from_bytes(icc);
        let data: &CFType = &data;
        // SAFETY: CGColorSpaceCreateWithICCData takes a CFData (or a data
        // provider), and this is a CFData.
        if let Some(space) = unsafe { CGColorSpace::with_icc_data(Some(data)) } {
            if CGColorSpace::model(Some(&*space)) == CGColorSpaceModel::RGB {
                return Some(space);
            }
        }
    }
    // SAFETY: kCGColorSpaceSRGB is a constant CFString CoreGraphics exports.
    let name = unsafe { kCGColorSpaceSRGB };
    CGColorSpace::with_name(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgbx_drops_the_fourth_byte() {
        let mut row = Vec::new();
        rgbx_to_rgb(&[1, 2, 3, 255, 4, 5, 6, 0], &mut row);
        assert_eq!(row, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn unpremultiply_gives_straight_alpha() {
        let mut row = Vec::new();
        unpremultiply(
            &[
                255, 0, 0, 255, // opaque red stays
                128, 0, 0, 128, // half-transparent red is red
                0, 0, 0, 0, // transparent is all zero
                9, 9, 9, 0, // even with junk in it
                64, 32, 0, 64, // c × 255 / a
                200, 0, 0, 100, // never over 255
            ],
            &mut row,
        );
        assert_eq!(
            row,
            [
                255, 0, 0, 255, //
                255, 0, 0, 128, //
                0, 0, 0, 0, //
                0, 0, 0, 0, //
                255, 128, 0, 64, //
                255, 0, 0, 100,
            ]
        );
    }

    #[test]
    fn unpremultiply_rounds_to_nearest() {
        // 1 × 255 / 3 = 85; 2 × 255 / 3 = 170; 1 × 255 / 2 = 127.5 → 128.
        let mut row = Vec::new();
        unpremultiply(&[1, 2, 0, 3, 1, 0, 0, 2], &mut row);
        assert_eq!(row, [85, 170, 0, 3, 128, 0, 0, 2]);
    }

    #[test]
    fn a_finish_that_fails_means_this_mac_cant_write_heic() {
        let failure = finish_failed();
        assert_eq!(failure.kind, FailKind::NoEncoder);
        assert!(failure
            .detail
            .starts_with("ImageIO couldn't finish the HEIC"));
    }

    #[test]
    fn quality_is_a_fraction_or_the_default() {
        assert_eq!(lossy_quality(None), 0.8);
        assert_eq!(lossy_quality(Some(100)), 1.0);
        assert_eq!(lossy_quality(Some(50)), 0.5);
        assert_eq!(lossy_quality(Some(1)), 0.01);
    }

    #[test]
    fn layouts_send_three_or_four_channels() {
        assert_eq!(Layout::Opaque.channels(), 3);
        assert_eq!(Layout::Alpha.channels(), 4);
        assert_eq!(
            Layout::Opaque.bitmap_info(),
            CGImageAlphaInfo::NoneSkipLast.0 | CGImageByteOrderInfo::Order32Big.0
        );
        assert_eq!(
            Layout::Alpha.bitmap_info(),
            CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0
        );
    }

    #[test]
    fn this_mac_reads_heic() {
        // Every macOS since 10.13 reads HEIC; writing depends on the
        // hardware, so it's only reported.
        let caps = ImageIo.caps();
        assert!(caps.decode);
        assert_eq!(caps.codec, "ImageIO");
        println!("ImageIO encodes HEIC here: {}", caps.encode);
    }

    #[test]
    fn a_png_isnt_read_as_heic() {
        let dir = std::env::temp_dir().join(format!("grape-imageio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp folder");
        let png = dir.join("one.png");
        // A 1×1 opaque PNG.
        std::fs::write(
            &png,
            [
                0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44,
                0x52, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0, 0x90, 0x77, 0x53, 0xde, 0, 0, 0, 0x0c,
                0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0, 0, 0x03, 0x01, 0x01,
                0, 0x18, 0xdd, 0x8d, 0xb0, 0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60,
                0x82,
            ],
        )
        .expect("a temp file");

        struct Nothing;
        impl PixelSink for Nothing {
            fn begin(&mut self, _: &ImageInfo, _: Option<&[u8]>) -> Result<(), Failure> {
                Ok(())
            }
            fn row(&mut self, _: &[u8]) -> Result<(), Failure> {
                Ok(())
            }
        }
        // `serve` sniffs first; called directly, ImageIO's own type check
        // still refuses it (with the hint, the restriction, or by type).
        let result = autoreleasepool(|_| decode(&png, &Limits::DEFAULT, &mut Nothing));
        let kind = result.expect_err("a PNG isn't HEIC").kind;
        assert!(
            matches!(
                kind,
                FailKind::Unsupported | FailKind::NotHeif | FailKind::Damaged
            ),
            "{kind:?}"
        );
        std::fs::remove_dir_all(&dir).expect("the temp folder removed");
    }
}
