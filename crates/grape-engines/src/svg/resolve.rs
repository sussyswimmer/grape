//! What an SVG may pull in (D39): `data:` only, within a job-wide budget.
//!
//! usvg's own resolver treats every `<image>` and `<feImage>` href that
//! isn't `data:` as a file path, absolute or relative to the current
//! folder, and reads it whole: a private picture baked into a shared PNG,
//! `/dev/zero` read until memory runs out, and on Windows a UNC path that
//! talks SMB (a network call in the conversion path, golden rule 3). So
//! Grape's resolver ([`Budget::resolver`]) never resolves a string, and
//! admits embedded data only within [`Budget`]'s bounds, which every nested
//! tree shares (usvg's `from_data_nested` keeps the outer `resolve_data`
//! and forces `resolve_string` off). usvg caches nothing: every reference,
//! every `<use>` copy included, asks again and is counted again.
//!
//! What is refused is counted once per distinct reference: an external
//! href by a hash of its text, refused data by its length and a hash of
//! its first 4 KiB ([`Budget::left_out`]). This is what browsers do with an
//! SVG used as an image ("secure static mode").

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::Hasher;
use std::sync::{Arc, Mutex, PoisonError};

use resvg::usvg::{self, ImageHrefResolver, ImageKind};

use super::protocol::Limits;

/// How much of refused data is hashed to tell one from another.
const HASHED_BYTES: usize = 4096;

/// A gzip stream's first bytes.
const GZIP: [u8; 2] = [0x1f, 0x8b];

/// What a job's embedded data may use, and what it left out.
#[derive(Debug)]
pub(crate) struct Budget {
    /// The most nested SVGs.
    max_nested_svgs: u32,
    /// The most decoded bytes admitted, in all.
    max_data_bytes: u64,
    /// An embedded raster's longest side.
    max_side: u32,
    /// An embedded raster's pixels.
    max_pixels: u64,
    /// What was admitted and refused so far.
    spent: Mutex<Spent>,
}

/// [`Budget`]'s running totals.
#[derive(Debug, Default)]
struct Spent {
    /// Nested SVGs admitted.
    nested: u32,
    /// Decoded bytes admitted.
    bytes: u64,
    /// One key per distinct refusal.
    refused: HashSet<u64>,
}

/// How usvg is told what embedded raster data is.
type RasterKind = fn(Arc<Vec<u8>>) -> ImageKind;

/// What a `data:` reference's MIME type says it is.
enum Embedded {
    /// PNG, JPEG, GIF or WebP.
    Raster,
    /// An SVG, drawn as a nested tree.
    Svg,
}

impl Budget {
    /// An empty budget with `limits`' bounds.
    pub(crate) fn new(limits: &Limits) -> Budget {
        Budget {
            max_nested_svgs: limits.max_nested_svgs,
            max_data_bytes: limits.max_data_bytes,
            max_side: limits.max_embedded_side,
            max_pixels: limits.max_embedded_pixels,
            spent: Mutex::new(Spent::default()),
        }
    }

    /// usvg's resolver: `data:` through [`Budget::admit`], every other
    /// href refused and counted.
    pub(crate) fn resolver(&self) -> ImageHrefResolver<'_> {
        ImageHrefResolver {
            resolve_data: Box::new(move |mime, data, options| self.admit(mime, data, options)),
            resolve_string: Box::new(move |href, _| {
                self.note_href(href);
                None
            }),
        }
    }

    /// How many distinct references were refused.
    pub(crate) fn left_out(&self) -> u32 {
        let spent = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        u32::try_from(spent.refused.len()).unwrap_or(u32::MAX)
    }

    /// Embedded data of type `mime`, decoded: a PNG, JPEG, GIF or WebP
    /// whose header gives a size within the bounds (its kind taken from
    /// its content, as a browser sniffs it), or an SVG while nested SVGs
    /// are within theirs, and either only while the job's decoded bytes
    /// stay within theirs. Anything else is refused and counted.
    pub(crate) fn admit(
        &self,
        mime: &str,
        data: Arc<Vec<u8>>,
        options: &usvg::Options,
    ) -> Option<ImageKind> {
        let embedded = match mime {
            "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp" => {
                Embedded::Raster
            }
            "image/svg+xml" => Embedded::Svg,
            _ => return self.refuse_data(&data),
        };
        match embedded {
            Embedded::Raster => {
                let Some(kind) = self.raster_kind(&data) else {
                    return self.refuse_data(&data);
                };
                if !self.take(&data, false) {
                    return self.refuse_data(&data);
                }
                Some(kind(data))
            }
            Embedded::Svg => {
                // usvg's `svgz` feature is off, so it would refuse this
                // anyway; Grape never inflates embedded data.
                if data.starts_with(&GZIP) || !self.take(&data, true) {
                    return self.refuse_data(&data);
                }
                usvg::Tree::from_data_nested(&data, options)
                    .ok()
                    .map(ImageKind::SVG)
            }
        }
    }

    /// The kind a raster is drawn as, from its content, if its header's
    /// size is known and within the bounds.
    fn raster_kind(&self, data: &[u8]) -> Option<RasterKind> {
        let kind: RasterKind = match imagesize::image_type(data).ok()? {
            imagesize::ImageType::Png => ImageKind::PNG,
            imagesize::ImageType::Jpeg => ImageKind::JPEG,
            imagesize::ImageType::Gif => ImageKind::GIF,
            imagesize::ImageType::Webp => ImageKind::WEBP,
            _ => return None,
        };
        let size = imagesize::blob_size(data).ok()?;
        let (width, height) = (
            u64::try_from(size.width).ok()?,
            u64::try_from(size.height).ok()?,
        );
        let side = u64::from(self.max_side);
        let fits = width <= side
            && height <= side
            && width
                .checked_mul(height)
                .is_some_and(|pixels| pixels <= self.max_pixels);
        fits.then_some(kind)
    }

    /// Counts `data` against the byte budget (and, for an SVG, the nested
    /// SVGs), or changes nothing and says no.
    fn take(&self, data: &[u8], svg: bool) -> bool {
        let mut spent = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        let bytes = spent.bytes.saturating_add(data.len() as u64);
        if bytes > self.max_data_bytes || (svg && spent.nested >= self.max_nested_svgs) {
            return false;
        }
        spent.bytes = bytes;
        if svg {
            spent.nested += 1;
        }
        true
    }

    /// Counts refused data once: by its length and its first bytes.
    fn refuse_data(&self, data: &[u8]) -> Option<ImageKind> {
        let mut hasher = DefaultHasher::new();
        hasher.write_u8(b'd');
        hasher.write_usize(data.len());
        hasher.write(&data[..data.len().min(HASHED_BYTES)]);
        self.refuse(hasher.finish());
        None
    }

    /// Counts an external href once, by its text. An empty one is no
    /// reference at all.
    fn note_href(&self, href: &str) {
        if href.trim().is_empty() {
            return;
        }
        let mut hasher = DefaultHasher::new();
        hasher.write_u8(b'h');
        hasher.write(href.as_bytes());
        self.refuse(hasher.finish());
    }

    fn refuse(&self, key: u64) {
        let mut spent = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        spent.refused.insert(key);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::{Cursor, Write};

    use ::image::{DynamicImage, ImageFormat, RgbImage};

    use super::*;

    fn encoded(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::new(width, height));
        let mut bytes = Cursor::new(Vec::new());
        img.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    /// A GIF header (logical screen `width` × `height`): `imagesize` reads
    /// no further, so the rest needn't be valid.
    fn gif(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes
    }

    /// A PNG header claiming `width` × `height`.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
        bytes
    }

    fn svg(body: &str) -> Vec<u8> {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">{body}</svg>"#)
            .into_bytes()
    }

    fn admit(budget: &Budget, mime: &str, data: Vec<u8>) -> Option<ImageKind> {
        budget.admit(mime, Arc::new(data), &usvg::Options::default())
    }

    fn kind_name(kind: Option<ImageKind>) -> &'static str {
        match kind {
            Some(ImageKind::PNG(_)) => "png",
            Some(ImageKind::JPEG(_)) => "jpeg",
            Some(ImageKind::GIF(_)) => "gif",
            Some(ImageKind::WEBP(_)) => "webp",
            Some(ImageKind::SVG(_)) => "svg",
            None => "none",
        }
    }

    #[test]
    fn rasters_of_the_four_kinds_are_admitted_by_their_content() {
        let budget = Budget::new(&Limits::DEFAULT);
        let png = encoded(ImageFormat::Png, 3, 2);
        let jpeg = encoded(ImageFormat::Jpeg, 3, 2);
        let webp = encoded(ImageFormat::WebP, 3, 2);
        assert_eq!(kind_name(admit(&budget, "image/png", png.clone())), "png");
        assert_eq!(
            kind_name(admit(&budget, "image/jpeg", jpeg.clone())),
            "jpeg"
        );
        assert_eq!(kind_name(admit(&budget, "image/jpg", jpeg.clone())), "jpeg");
        assert_eq!(kind_name(admit(&budget, "image/gif", gif(3, 2))), "gif");
        assert_eq!(kind_name(admit(&budget, "image/webp", webp)), "webp");
        // Labeled one kind, holding another: drawn as what it holds.
        assert_eq!(kind_name(admit(&budget, "image/png", jpeg)), "jpeg");
        assert_eq!(budget.left_out(), 0);
        // Other types, and data with no readable header, are refused.
        for (mime, data) in [
            ("text/plain", png.clone()),
            ("image/bmp", encoded(ImageFormat::Bmp, 3, 2)),
            ("image/png", encoded(ImageFormat::Bmp, 4, 2)),
            ("image/png", b"not an image".to_vec()),
            ("image/tiff", encoded(ImageFormat::Tiff, 3, 2)),
        ] {
            assert!(admit(&budget, mime, data).is_none(), "{mime}");
        }
        assert_eq!(budget.left_out(), 5);
    }

    #[test]
    fn rasters_over_a_side_or_the_pixels_are_refused() {
        let budget = Budget::new(&Limits::DEFAULT);
        assert!(admit(&budget, "image/png", png_header(16_384, 4096)).is_some());
        assert!(admit(&budget, "image/png", png_header(16_385, 1)).is_none());
        assert!(admit(&budget, "image/png", png_header(1, 16_385)).is_none());
        // 64 MP is the most: 8192 × 8193 is over it.
        assert!(admit(&budget, "image/png", png_header(8192, 8192)).is_some());
        assert!(admit(&budget, "image/png", png_header(8192, 8193)).is_none());
        assert!(admit(&budget, "image/gif", gif(65_535, 65_535)).is_none());
        assert_eq!(budget.left_out(), 4);
    }

    #[test]
    fn the_129th_mib_is_refused() {
        let limits = Limits::DEFAULT;
        let budget = Budget::new(&limits);
        let mib = 1024 * 1024;
        // 128 one-MiB PNGs (a header and padding) fit; the next doesn't.
        for i in 0..128 {
            let mut data = png_header(4, 4);
            data.resize(mib, 0);
            assert!(admit(&budget, "image/png", data).is_some(), "{i}");
        }
        let mut data = png_header(4, 4);
        data.resize(mib, 0);
        assert!(admit(&budget, "image/png", data.clone()).is_none());
        // A smaller one still fits in what's left: nothing.
        assert!(admit(&budget, "image/png", png_header(4, 4)).is_none());
        assert_eq!(budget.left_out(), 2);
        assert_eq!(
            budget.spent.lock().unwrap().bytes,
            limits.max_data_bytes,
            "a refusal spends nothing"
        );
    }

    #[test]
    fn the_65th_nested_svg_and_gzip_are_refused() {
        let budget = Budget::new(&Limits::DEFAULT);
        for i in 0..64 {
            let data = svg(&format!(r#"<rect width="{}" height="1"/>"#, i + 1));
            assert_eq!(
                kind_name(admit(&budget, "image/svg+xml", data)),
                "svg",
                "{i}"
            );
        }
        assert!(admit(&budget, "image/svg+xml", svg("<rect/>")).is_none());
        assert_eq!(budget.left_out(), 1);

        let fresh = Budget::new(&Limits::DEFAULT);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&svg("")).unwrap();
        assert!(admit(&fresh, "image/svg+xml", gz.finish().unwrap()).is_none());
        assert_eq!(fresh.left_out(), 1);
        assert_eq!(fresh.spent.lock().unwrap().nested, 0);
    }

    #[test]
    fn no_string_is_ever_resolved() {
        let budget = Budget::new(&Limits::DEFAULT);
        let resolver = budget.resolver();
        let options = usvg::Options::default();
        let here = std::env::current_exe().unwrap();
        let absolute = here.to_string_lossy().into_owned();
        for href in [
            absolute.as_str(),
            "secret.png",
            "../private/secret.png",
            r"\\host\share\x.png",
            r"\\host@SSL\share\x.png",
            "//host/share/x.png",
            "file:///etc/passwd",
            "http://example.com/x.png",
            "https://example.com/x.svg",
            "#local",
        ] {
            assert!(
                (resolver.resolve_string)(href, &options).is_none(),
                "{href}"
            );
        }
        assert_eq!(budget.left_out(), 10);
        // An empty href is no reference.
        assert!((resolver.resolve_string)("  ", &options).is_none());
        assert_eq!(budget.left_out(), 10);
    }

    #[test]
    fn a_reference_used_many_times_counts_once() {
        let budget = Budget::new(&Limits::DEFAULT);
        let resolver = budget.resolver();
        let options = usvg::Options::default();
        for _ in 0..1000 {
            assert!((resolver.resolve_string)("photo.png", &options).is_none());
            assert!(
                (resolver.resolve_data)("text/plain", Arc::new(b"same".to_vec()), &options)
                    .is_none()
            );
        }
        assert_eq!(budget.left_out(), 2);
        // Data that differs past its first 4 KiB, by length, still counts
        // apart; the same length and start count once.
        let mut long = vec![7u8; 5000];
        assert!(admit(&budget, "text/plain", long.clone()).is_none());
        long.push(1);
        assert!(admit(&budget, "text/plain", long).is_none());
        assert_eq!(budget.left_out(), 4);
    }

    #[test]
    fn feimage_hrefs_go_through_the_same_resolver() {
        // A whole document through usvg: an <image> and an <feImage>, each
        // with a file href, and a data: PNG. Only the PNG is admitted.
        let png = encoded(ImageFormat::Png, 2, 2);
        let data = base64_png(&png);
        let text = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="8" height="8">
                 <filter id="f"><feImage href="secret.png"/></filter>
                 <rect width="8" height="8" filter="url(#f)"/>
                 <image xlink:href="/etc/secret.png" width="4" height="4"/>
                 <image href="data:image/png;base64,{data}" width="4" height="4"/>
               </svg>"#
        );
        let budget = Budget::new(&Limits::DEFAULT);
        let options = usvg::Options {
            image_href_resolver: budget.resolver(),
            ..usvg::Options::default()
        };
        let tree = usvg::Tree::from_str(&text, &options).unwrap();
        drop(tree);
        drop(options);
        assert_eq!(budget.left_out(), 2);
        assert_eq!(budget.spent.lock().unwrap().bytes, png.len() as u64);
    }

    /// Standard base64, for data: URLs in tests.
    pub(crate) fn base64_png(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}
