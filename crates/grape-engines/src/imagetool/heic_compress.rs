//! Compress on HEIC (D28): Grape's image helper (D33) decodes the picture
//! upright, Grape scales it down, the helper encodes it again at the
//! preset's quality, and Grape grafts the source's metadata items into
//! what it wrote ([`graft`](super::isobmff::graft)).
//!
//! | Preset | Long side | libheif (Windows) | ImageIO (macOS, provisional) |
//! |---|---|---|---|
//! | Small | ≤ 2048 | q40 | 0.60 |
//! | Balanced | ≤ 4096 | q50 | 0.70 |
//! | High | as it is (one HEVC picture at most) | q60 | 0.80 |
//!
//! The helper is fixed per OS, so the table is too (`cfg!(target_os =
//! "macos")`; [`plan`]). ImageIO's numbers wait for QA's calibration: at
//! each preset, the median output of 10 iPhone photos within 10 % of the
//! PC's.
//!
//! 1. **Before anything starts** ([`scan::check`]): the file's content,
//!    boxes and `meta`, the refusals (AVIF, another codec, sequences, more
//!    than 8 bits, another picture, non-square pixels, High over one HEVC
//!    picture), the metadata items to carry (16 MiB at most), and last
//!    whether the helper can write HEIC at all (probed once a process).
//!    Progress 0–0.05.
//! 2. **Decode** ([`Decoder::decode_within`]): upright 8-bit RGB or RGBA
//!    and the ICC profile, the helper's `image` line held to what this job
//!    holds at its peak ([`peak`]) with the carried metadata and the
//!    profile, within D16's 1 GiB. 0.05–0.40.
//! 3. **Color:** with no profile back, a `colr` `nclx` that isn't sRGB-like
//!    (primaries 1 or 2, transfer 1, 2, 6 or 13) is refused ([`WIDE`]):
//!    encoded without it, its colors would shift. libheif hands back no
//!    profile for one (Windows: a P3 `nclx` is refused); ImageIO hands back
//!    its color space's (macOS: kept). One rule on both.
//! 4. **Scale** ([`scale::scale`]) to the preset's long side, as JPEG's
//!    Compress does. 0.40–0.50 (Lanczos3 itself can't be stopped).
//! 5. **Encode** ([`Encoder::encode`]) at the preset's quality, the rows
//!    streamed into the helper's `input.rgba` (0.50–0.55), its `out.heic`
//!    read back (0.55–0.90). The pixels go once it's back.
//! 6. **Early size check:** `out.heic`, the carried data and 256 bytes over
//!    90 % of the source end it `NoChange` ("… is already compact. Try
//!    Small." but at Small, "… is already as small as Grape makes it."):
//!    HEVC has no quality to read from a source the way a JPEG's tables
//!    give one, so this comes only after the decode and encode.
//! 7. **The metadata** ([`carried`]): each Exif item's dimensions become the
//!    new picture's. The helper turns the picture by its `irot` and `imir`
//!    ([`Scan::turns`]); then every Exif item's Orientation becomes 1 (its
//!    IFD1, stored as the picture was, unlinked when it said 2 to 8) and so
//!    does every XMP item's `tiff:Orientation`. A photo that turns only by
//!    its Exif (a Samsung portrait, its `irot` blanked; an iOS 11 photo)
//!    comes back as stored, as libheif decodes it, and keeps its
//!    Orientation, as JPEG Compress keeps a JPEG's. A helper that turned
//!    one by its Exif (ImageIO may) is told by the sides it hands back:
//!    `ispe`'s, swapped, for an Orientation of 5 to 8 ([`turned_by_exif`]);
//!    of 2 to 4 it can't be told (a known limit, QA's).
//! 8. **Graft** into `out.heic` ([`Graft`]), then the final rule: over 90 %
//!    of the source is `NoChange` as above, before a temp file is made.
//!    0.90–0.92.
//! 9. **Write** the temp file in the destination folder through
//!    [`Cancellable`], read it again ([`Graft::check`]), then publish it
//!    without replacing anything, with [`Note::Compressed`]. 0.92–0.98,
//!    then 1.0.
//!
//! - **Dropped,** as D26's JPEG Compress drops them: gain maps, `tmap`,
//!   depth and Portrait mattes, thumbnails, motion-photo video and C2PA;
//!   `udes`, `crtt` and `mdft` aren't carried (the date is in the Exif).
//! - **Memory:** at the helper's `image` line, [`peak`]'s figure for the
//!   picture (decoded; averaged and Lanczos3's steps; the scaled picture and
//!   `out.heic`, counted at 2 bytes a pixel, 3 with alpha), the carried
//!   metadata (≤ 16 MiB) and the ICC profile, within 1 GiB. The helper
//!   itself is held by its container (2 GiB on Windows). The graft holds
//!   `out.heic`, its `meta` twice and the carried data, the pixels gone.
//!   Measured with the tests' fake helper (Linux, release build): a 48 MP
//!   RGB picture peaked at 564 MiB at Balanced (Lanczos3's intermediate;
//!   554 MiB counted) and 185 MiB at Small (174 counted); 35.5 MP at High
//!   at 213 MiB, the fake's `out.heic` being the raw pixels (106 MB, where
//!   [`peak`] counts 71 MB of HEVC).
//! - **Cancel:** checked between steps; the helper is killed within D33's
//!   100 ms poll; the scale stops at its next row (but Lanczos3); the write
//!   and the check stop at their next 64 KiB.
//! - **Time:** the helper has D33's deadlines (120 s, plus 1 s a megapixel
//!   to decode and 3 s to encode). Grape's own share, for the same 48 MP
//!   picture: the scale took 2.5 s at Balanced and 1.1 s at Small, reading
//!   the decoded pixels 0.3 s, and the graft, write and check of a 36 MiB
//!   `out.heic` 0.06 s.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use grape_core::{CompressPreset, Format, Job};
use grape_heif_proto::HeifFormat;
use image::{ColorType, GenericImageView};

use super::exif::{self, MAX_EXIF};
use super::isobmff::graft::{Added, Graft};
use super::isobmff::items::Role;
use super::isobmff::scan::{self, Scan};
use super::scale;
use super::strip::{self, Error};
use crate::cancel::Cancellable;
use crate::heif::{Decoded, Decoder, Encoder};
use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{EngineCtx, ErrorKind, GrapeError, JobOutcome, Note};

/// Why a wide color space the helper hands back with no profile is
/// refused.
pub(crate) const WIDE: &str = "uses a wide color space that Compress can't keep yet.";

/// A preset's quality for the helper of this OS (`mac`: ImageIO's, q / 100;
/// otherwise libheif's) and the long side it scales down to.
fn plan(preset: CompressPreset, mac: bool) -> (u8, Option<u32>) {
    match (preset, mac) {
        (CompressPreset::Small, false) => (40, Some(2048)),
        (CompressPreset::Balanced, false) => (50, Some(4096)),
        (CompressPreset::High, false) => (60, None),
        (CompressPreset::Small, true) => (60, Some(2048)),
        (CompressPreset::Balanced, true) => (70, Some(4096)),
        (CompressPreset::High, true) => (80, None),
    }
}

/// What Compress holds at its peak for a `width`×`height` decoded picture
/// of `bpp` bytes a pixel scaled to fit `cap` ([`scale::peak`]): to encode,
/// the scaled picture, whose rows stream into the helper's `input.rgba`
/// with no copy, and `out.heic` read back beside it, counted at 2 bytes a
/// pixel (HEVC at 4:2:0 is 1.5 bytes a pixel uncompressed), 3 with alpha
/// (its plane, 1 more).
fn peak(width: u32, height: u32, bpp: u64, cap: Option<u32>) -> u64 {
    let encode = |w: u64, h: u64, bpp: u64| {
        let heic = if bpp == 4 { 3 } else { 2 };
        w.saturating_mul(h).saturating_mul(bpp + heic)
    };
    scale::peak(width, height, bpp, cap, &encode)
}

/// Runs Compress at `preset` on the HEIF `src` (named as `from`) into
/// `dest`, with the image helper in `ctx.sidecars`.
pub(crate) fn run(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    preset: CompressPreset,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    let sidecars = &ctx.sidecars;
    // `run_job` sends a HEIC here only with the helper; this is for HEIF
    // named as another format, which the content sends here (rule 3).
    match (
        Decoder::from_sidecars(sidecars),
        Encoder::from_sidecars(sidecars, HeifFormat::Heic),
    ) {
        (Some(decoder), Some(encoder)) => run_with(
            job, src, dest, from, preset, ctx, progress, &decoder, &encoder,
        ),
        _ => Err(GrapeError::new(
            ErrorKind::NotYetSupported,
            "Grape's image helper is missing.",
            "SidecarPaths::heif is None",
        )),
    }
}

/// Progress passed on only forwards.
struct Forward<'a> {
    last: f32,
    progress: &'a mut dyn FnMut(f32),
}

impl Forward<'_> {
    fn to(&mut self, fraction: f32) {
        if fraction > self.last {
            self.last = fraction;
            (self.progress)(fraction);
        }
    }
}

/// [`run`] with this decoder and encoder ([module docs](self)).
#[allow(clippy::too_many_arguments)] // `run`'s, plus the helper's two halves.
pub(crate) fn run_with(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    preset: CompressPreset,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    decoder: &Decoder,
    encoder: &Encoder,
) -> Result<JobOutcome, GrapeError> {
    let names = Names::new(src, dest, from, from);
    let dir = publish::dest_dir(dest, &names)?;
    let cancel = &ctx.cancel;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let before = std::fs::metadata(src)
        .map_err(|e| names.read_failed(format!("stat {}: {e}", src.display())))?
        .len();
    // Refused unread, as the helper would refuse it.
    if before > crate::image::MAX_SOURCE {
        return Err(names.too_large(format!(
            "compress {}: the file is {} MiB, over the {} MiB limit for a HEIC source",
            src.display(),
            before >> 20,
            crate::image::MAX_SOURCE >> 20
        )));
    }
    let mut forward = Forward {
        last: 0.0,
        progress,
    };
    let scan = scan::check(src, &names, preset, encoder, cancel)?;
    forward.to(0.05);

    let (quality, cap) = plan(preset, cfg!(target_os = "macos"));
    let held: u64 = scan.carried.iter().map(|c| c.data.len() as u64).sum();
    let tool_peak = |width: u32, height: u32, color: ColorType| {
        peak(width, height, u64::from(color.bytes_per_pixel()), cap).saturating_add(held)
    };
    let decoded = decoder
        .decode_within(src, &names, &tool_peak, cancel, (0.05, 0.40), &mut |f| {
            forward.to(f)
        })
        .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    let (img, icc) = match decoded {
        Decoded::Pixels(img, icc) => (img, icc),
        // The scan read it as HEIC.
        Decoded::NotHeif => {
            return Err(names.decode_failed(format!("{}: not a HEIF file", src.display())))
        }
    };
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    forward.to(0.40);
    if icc.is_none() && scan.nclx.is_some_and(|n| !n.srgb_like()) {
        return Err(strip::refused(&names, src, WIDE));
    }
    let upright = scan.turns() || turned_by_exif(&scan, img.dimensions());
    let img = scale::scale(img, cap, cancel).ok_or_else(|| crate::cancelled(src))?;
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    forward.to(0.50);
    let (width, height) = img.dimensions();
    let encoded = encoder
        .encode(
            &img,
            icc.as_deref(),
            Some(quality),
            &names,
            cancel,
            (0.55, 0.90),
            &mut |f| forward.to(f),
        )
        .map_err(|e| publish::or_cancelled(cancel, src, e))?;
    drop((img, icc));
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    forward.to(0.90);
    let try_small = preset != CompressPreset::Small;
    let early = (encoded.len() as u64)
        .saturating_add(held)
        .saturating_add(256);
    if early.saturating_mul(10) > before.saturating_mul(9) {
        let detail = format!(
            "{} bytes and {held} of metadata from {before} in {}",
            encoded.len(),
            src.display()
        );
        return Err(names.not_smaller(try_small, detail));
    }

    let graft = Graft::new(encoded, carried(scan, (width, height), upright))
        .map_err(|e| graft_failed(e, &names, src))?;
    let after = graft.len();
    if after.saturating_mul(10) > before.saturating_mul(9) {
        let detail = format!("{after} bytes from {before} in {}", src.display());
        return Err(names.not_smaller(try_small, detail));
    }
    forward.to(0.92);

    publish::ensure_dir(job, ctx, dir, &names)?;
    let (temp, file) = TempFile::create(dir, from.output_ext())
        .map_err(|e| names.write_failed(format!("create temp in {}: {e}", dir.display())))?;
    let written = {
        let mut w = BufWriter::with_capacity(1 << 20, Cancellable::new(file, cancel));
        graft
            .write(&mut w)
            .and_then(|()| w.into_inner().map_err(|e| e.into_error()))
            .and_then(|w| w.into_inner().sync_all())
    };
    written.map_err(|e| {
        let e = names.write_failed(format!("write {}: {e}", temp.path().display()));
        publish::or_cancelled(cancel, src, e)
    })?;
    forward.to(0.95);
    let file = File::open(temp.path())
        .map_err(|e| names.write_failed(format!("open {}: {e}", temp.path().display())))?;
    let mut r = BufReader::with_capacity(64 * 1024, Cancellable::new(file, cancel));
    graft
        .check(&mut r, after)
        .map_err(|e| publish::or_cancelled(cancel, src, graft_failed(e, &names, src)))?;
    drop((r, graft));
    forward.to(0.98);
    // A cancel during the last report is a cancel.
    if cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let mut outcome = publish::finalize(&temp, job, src, dest, dir, &names)?;
    forward.to(1.0);
    outcome.notes.push(Note::Compressed { before, after });
    Ok(outcome)
}

/// What the graft says went wrong, in Grape's words: what the helper wrote
/// isn't what the graft takes, or what was written isn't what it planned
/// ("Grape couldn't write a HEIC file."); or the temp file couldn't be read
/// again.
fn graft_failed(e: Error, names: &Names, src: &Path) -> GrapeError {
    match e {
        Error::Broken(why) | Error::TooLarge(why) | Error::Refused(why) => {
            names.encode_failed(format!("graft {}: {why}", src.display()))
        }
        Error::Read(e) | Error::Write(e) => {
            names.write_failed(format!("check {}: {e}", src.display()))
        }
    }
}

/// Whether the helper turned the picture by its Exif Orientation though
/// nothing in the file turns it ([module docs](self), step 7): an
/// Orientation of 5 to 8 and the pixels back `width`×`height` as `ispe`'s
/// sides swapped.
fn turned_by_exif(scan: &Scan, (width, height): (u32, u32)) -> bool {
    !scan.turns()
        && scan.orientation.is_some_and(|o| (5..=8).contains(&o))
        && scan
            .ispe
            .is_some_and(|(w, h)| w != h && (width, height) == (h, w))
}

/// The metadata items `scan` carries, to graft into a `width`×`height`
/// picture, `upright` when the helper turned it ([module docs](self), step
/// 7).
fn carried(scan: Scan, (width, height): (u32, u32), upright: bool) -> Vec<Added> {
    scan.carried
        .into_iter()
        .map(|c| {
            let mut data = c.data;
            match c.role {
                Role::Exif => {
                    if let Some(tiff) = exif_tiff(&mut data) {
                        upright_exif(tiff, width, height, upright);
                    }
                }
                Role::Xmp if upright => upright_xmp(&mut data),
                _ => {}
            }
            Added {
                kind: c.kind,
                hidden: c.hidden,
                name: c.name,
                content_type: c.content_type,
                encoding: c.encoding,
                data,
            }
        })
        .collect()
}

/// The TIFF structure of an Exif item's `data`: after its 4-byte header
/// offset, and after `Exif\0\0` when it's there, as the scan reads it.
fn exif_tiff(data: &mut [u8]) -> Option<&mut [u8]> {
    let offset = u32::from_be_bytes(*data.first_chunk::<4>()?);
    let skip = usize::try_from(offset).ok()?.checked_add(4)?;
    let tiff = data.get_mut(skip..)?;
    if tiff.starts_with(b"Exif\0\0") {
        tiff.get_mut(6..)
    } else {
        Some(tiff)
    }
}

/// An Exif `tiff` for pixels now `width`×`height`, in place: its
/// dimensions, and when they're `upright`, an Orientation of 1 and no IFD1
/// if it said 2 to 8.
fn upright_exif(tiff: &mut [u8], width: u32, height: u32, upright: bool) {
    exif::patch_dimensions(tiff, width, height);
    if upright {
        let turned = exif::walk(&tiff[..tiff.len().min(MAX_EXIF)])
            .orientation
            .is_some_and(|o| o != 1);
        exif::patch_orientation(tiff, 1);
        if turned {
            exif::unlink_ifd1(tiff);
        }
    }
}

/// An XMP packet's `tiff:Orientation` set to 1 in place, in both forms a
/// packet writes it: an attribute (`tiff:Orientation="6"`, or in single
/// quotes) and an element (`<tiff:Orientation>6</tiff:Orientation>`). A
/// value of more than one digit is left as it is.
fn upright_xmp(xmp: &mut [u8]) {
    const NAME: &[u8] = b"tiff:Orientation";
    let mut at = 0;
    while let Some(found) = xmp[at..].windows(NAME.len()).position(|w| w == NAME) {
        let i = at + found + NAME.len();
        let digit = match xmp.get(i..i + 4) {
            Some(&[b'=', q, d, end]) if (q == b'"' || q == b'\'') && end == q => {
                d.is_ascii_digit().then_some(i + 2)
            }
            Some(&[b'>', d, b'<', b'/']) => d.is_ascii_digit().then_some(i + 1),
            _ => None,
        };
        if let Some(d) = digit {
            xmp[d] = b'1';
        }
        at = i;
    }
}

#[cfg(test)]
mod tests {
    //! Compress on HEIC end to end with `heif.rs`'s fake helper, whose
    //! "HEIC" is real HEIF around raw pixels: the sources are built with
    //! the isobmff builder around its payload, and its encode records the
    //! quality it was asked for. Every run checks that the source is
    //! unchanged, that nothing but the output is left beside it, and that
    //! no work folder is left.

    #![allow(clippy::unwrap_used)]

    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;

    use grape_core::{plan as plan_jobs, JobOutput, Operation, Settings, ToolKey, ToolParams};

    use super::super::exif::tests::{tiff, V};
    use super::super::isobmff::tests::{
        bx, hvcc, icc as colr_icc, irot, ispe, nclx, pixi, Builder, Item, Place, EXIF, XMP,
    };
    use super::*;
    use crate::heif::tests::{fake, fake_heic, fake_quality, nowhere, parse_fake};
    use crate::heif::Limits;
    use crate::helper::Launch;
    use crate::output::unique_suffix;
    use crate::MetaField;
    use CompressPreset::{Balanced, High, Small};

    /// A camera's Exif, big-endian: Make, Orientation `o`, an Exif IFD with
    /// the date and PixelXDimension and PixelYDimension (`w`×`h`), GPS, and
    /// an IFD1 thumbnail.
    fn camera(o: u16, (w, h): (u32, u32)) -> Vec<u8> {
        tiff(
            b'M',
            &[
                &[
                    (0x010F, V::Ascii("SecretMake")),
                    (0x0112, V::Short(o)),
                    (0x8769, V::Ifd(1)),
                    (0x8825, V::Ifd(2)),
                ],
                &[
                    (0x9003, V::Ascii("2024:05:01 10:20:30")),
                    (0xA002, V::Long(w)),
                    (0xA003, V::Long(h)),
                ],
                &[
                    (0x0001, V::Ascii("N")),
                    (
                        0x0002,
                        V::Bytes(
                            5,
                            &[
                                0, 0, 0, 48, 0, 0, 0, 1, 0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0,
                                0, 1,
                            ],
                        ),
                    ),
                ],
                &[
                    (0x0103, V::Short(6)),
                    (0x0201, V::Long(4000)),
                    (0x0202, V::Long(10)),
                ],
            ],
            Some(3),
        )
    }

    /// The smallest ICC profile the helper keeps: an RGB one's header.
    fn rgb_profile() -> Vec<u8> {
        let mut p = vec![0; 128];
        p[..4].copy_from_slice(&128u32.to_be_bytes());
        p[16..20].copy_from_slice(b"RGB ");
        p
    }

    /// An XMP packet with both forms of `tiff:Orientation` at `o`.
    fn xmp(o: u16) -> String {
        format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description tiff:Orientation=\"{o}\" \
             exif:GPSLatitude='48,1N'><tiff:Orientation>{o}</tiff:Orientation>\
             </rdf:Description></x:xmpmeta>"
        )
    }

    /// A source to build: the fake helper's picture of `size` and
    /// `channels`, with `icc`, as the primary (`hvcC` 8-bit and `ispe`, or
    /// `ispe` given), more properties, its Exif and XMP, and `pad` bytes of
    /// a `free` box after it all (what Compress gains on).
    struct Source {
        size: (u32, u32),
        channels: u8,
        icc: Option<Vec<u8>>,
        ispe: Option<(u32, u32)>,
        props: Vec<Vec<u8>>,
        exif: Option<Vec<u8>>,
        xmp: Option<String>,
        pad: usize,
    }

    impl Source {
        fn new(size: (u32, u32)) -> Source {
            Source {
                size,
                channels: 3,
                icc: None,
                ispe: None,
                props: Vec::new(),
                exif: Some(camera(1, size)),
                xmp: Some(xmp(1)),
                pad: 64 * 1024,
            }
        }

        /// The picture's rows.
        fn rows(&self) -> Vec<u8> {
            let n = self.size.0 as usize * self.size.1 as usize * usize::from(self.channels);
            (0..n).map(|i| (i * 7 % 251) as u8).collect()
        }

        fn build(&self) -> Vec<u8> {
            self.builder().build().0
        }

        fn builder(&self) -> Builder {
            let rows = self.rows();
            let (w, h) = self.size;
            let fake = fake_heic(w, h, self.channels, &rows, self.icc.as_deref());
            // The fake's payload ends its file: magic, quality, size,
            // channels, the profile's length, the profile and the rows.
            let len = 18 + self.icc.as_ref().map_or(0, Vec::len) + rows.len();
            let payload = fake[fake.len() - len..].to_vec();
            let mut primary = Item::new(1, *b"hvc1", Place::Mdat, payload);
            let mut properties = vec![hvcc(8, 8), {
                let (w, h) = self.ispe.unwrap_or(self.size);
                ispe(w, h)
            }];
            properties.extend(self.props.iter().cloned());
            primary.props = (1..=properties.len() as u16).map(|i| (i, i == 1)).collect();
            let mut items = vec![primary];
            let mut refs = Vec::new();
            if let Some(exif) = &self.exif {
                items.push(Item::exif(EXIF, exif));
                refs.push((*b"cdsc", EXIF, vec![1]));
            }
            if let Some(xmp) = &self.xmp {
                items.push(Item::xmp(XMP, xmp));
                refs.push((*b"cdsc", XMP, vec![1]));
            }
            let mut b = Builder::new(items, 1);
            b.properties = properties;
            b.refs = refs;
            if self.pad > 0 {
                b.after = vec![bx(b"free", &vec![0; self.pad])];
            }
            b
        }
    }

    /// A folder for one test's files, removed with it.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            let path =
                std::env::temp_dir().join(format!("grape-heic-compress-unit-{}", unique_suffix()));
            fs::create_dir_all(path.join("work")).unwrap();
            TestDir(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// What a run gave: its result and its progress.
    struct Ran {
        result: Result<JobOutcome, GrapeError>,
        progress: Vec<f32>,
    }

    /// A source in its own folder, planned for Compress at a preset as the
    /// app plans it.
    struct Case {
        dir: TestDir,
        src: PathBuf,
        bytes: Vec<u8>,
        job: Job,
        preset: CompressPreset,
    }

    fn case(name: &str, bytes: &[u8], preset: CompressPreset) -> Case {
        let dir = TestDir::new();
        let src = dir.0.join(name);
        fs::write(&src, bytes).unwrap();
        let settings = Settings::new(dir.0.join("fallback"));
        let selection = crate::build_selection(std::slice::from_ref(&src), &settings);
        let op = Operation::Tool {
            tool: ToolKey::Compress,
            params: ToolParams {
                compress: preset,
                ..ToolParams::DEFAULT
            },
        };
        let mut jobs = plan_jobs(&selection, &op, &settings).unwrap().jobs;
        assert_eq!(jobs.len(), 1);
        Case {
            dir,
            src,
            bytes: bytes.to_vec(),
            job: jobs.remove(0),
            preset,
        }
    }

    impl Case {
        fn dest(&self) -> PathBuf {
            match &self.job.output {
                JobOutput::File(path) => path.clone(),
                other => panic!("{other:?}"),
            }
        }

        fn limits(&self) -> Limits {
            Limits {
                work_parent: self.dir.0.join("work"),
                ..Limits::default()
            }
        }

        /// Runs the job with the helper `launch` and `limits`, cancelling
        /// it once progress reaches `cancel_at`.
        fn run_with(&self, launch: Launch, limits: Limits, cancel_at: Option<f32>) -> Ran {
            let decoder = Decoder::new(launch.clone(), limits.clone());
            let encoder = Encoder::new(launch, limits, HeifFormat::Heic);
            let ctx = EngineCtx::default();
            let token = ctx.cancel.clone();
            let mut seen = Vec::new();
            let result = {
                let mut progress = |f: f32| {
                    seen.push(f);
                    if cancel_at.is_some_and(|at| f >= at) {
                        token.cancel();
                    }
                };
                run_with(
                    &self.job,
                    &self.src,
                    &self.dest(),
                    Format::Heic,
                    self.preset,
                    &ctx,
                    &mut progress,
                    &decoder,
                    &encoder,
                )
            };
            self.check(result.is_ok());
            Ran {
                result,
                progress: seen,
            }
        }

        fn run(&self, mode: &str) -> Ran {
            self.run_with(fake(mode), self.limits(), None)
        }

        /// The source is unchanged; beside it, only the output when there
        /// is one; no work folder is left.
        fn check(&self, published: bool) {
            assert_eq!(
                fs::read(&self.src).unwrap(),
                self.bytes,
                "the source changed"
            );
            let mut names: Vec<String> = fs::read_dir(&self.dir.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            let mut expected = vec![
                self.src.file_name().unwrap().to_string_lossy().into_owned(),
                "work".to_string(),
            ];
            if published {
                expected.push(
                    self.dest()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            expected.sort();
            assert_eq!(names, expected);
            assert_eq!(fs::read_dir(self.dir.0.join("work")).unwrap().count(), 0);
        }

        /// The output, and what it holds: the fake's picture, and the
        /// metadata items carried.
        fn output(&self, ran: &Ran) -> (Vec<u8>, Scan) {
            let outcome = ran.result.as_ref().unwrap();
            assert_eq!(outcome.outputs, [self.dest()]);
            let out = fs::read(self.dest()).unwrap();
            let note = Note::Compressed {
                before: self.bytes.len() as u64,
                after: out.len() as u64,
            };
            assert_eq!(outcome.notes, [note]);
            assert!(out.len() * 10 <= self.bytes.len() * 9);
            let mut s = Scan::read(&mut Cursor::new(&out), out.len() as u64).unwrap();
            s.load(&mut Cursor::new(&out)).unwrap();
            // Progress only goes forward, and ends at 1.
            assert!(
                ran.progress.windows(2).all(|w| w[0] < w[1]),
                "{:?}",
                ran.progress
            );
            assert_eq!(ran.progress.last(), Some(&1.0));
            (out, s)
        }

        fn refused<'a>(&self, ran: &'a Ran) -> &'a GrapeError {
            ran.result.as_ref().unwrap_err()
        }
    }

    /// The Exif's IFD0 Orientation, PixelXDimension and PixelYDimension
    /// (LONG), and whether it has an IFD1, from a carried Exif item.
    fn exif_of(data: &[u8]) -> (Option<u16>, Option<(u32, u32)>, bool) {
        let tiff = &data[10..];
        let u16_at = |at: usize| u16::from_be_bytes([tiff[at], tiff[at + 1]]);
        let u32_at = |at: usize| u32::from_be_bytes(tiff[at..at + 4].try_into().unwrap());
        let entries = |at: usize| (0..usize::from(u16_at(at))).map(move |n| at + 2 + n * 12);
        let ifd0 = u32_at(4) as usize;
        let exif_ifd = entries(ifd0)
            .find(|&e| u16_at(e) == 0x8769)
            .map(|e| u32_at(e + 8) as usize);
        let dims = exif_ifd.map(|at| {
            let get = |tag| {
                entries(at)
                    .find(|&e| u16_at(e) == tag)
                    .map(|e| u32_at(e + 8))
                    .unwrap()
            };
            (get(0xA002), get(0xA003))
        });
        let walked = exif::walk(tiff);
        let ifd1 = walked.fields.contains(&MetaField::Thumbnail);
        (walked.orientation, dims, ifd1)
    }

    #[test]
    fn each_preset_sends_its_quality_and_the_metadata_is_carried() {
        // 4200 × 30: Small and Balanced scale it, High doesn't.
        for (preset, size) in [
            (Small, (2048, 15)),
            (Balanced, (4096, 29)),
            (High, (4200, 30)),
        ] {
            let mut source = Source::new((4200, 30));
            source.pad = 500_000;
            let c = case("photo.heic", &source.build(), preset);
            let ran = c.run("ok");
            let (out, s) = c.output(&ran);
            let (quality, _) = plan(preset, false);
            assert_eq!(fake_quality(&out), Some(quality), "{preset:?}");
            let picture = parse_fake(&out).unwrap();
            assert_eq!((picture.width, picture.height), size, "{preset:?}");
            // The Exif and the XMP, the Exif's sides patched; nothing
            // turned it, so its Orientation and IFD1 stay.
            assert_eq!(s.carried.len(), 2);
            let (o, dims, ifd1) = exif_of(&s.carried[0].data);
            assert_eq!((o, dims, ifd1), (Some(1), Some(size), true), "{preset:?}");
            assert_eq!(s.carried[1].data, xmp(1).as_bytes());
            assert_eq!(s.carried[1].content_type, b"application/rdf+xml");
            assert!(ran.progress.contains(&0.05) && ran.progress.contains(&0.98));
        }
        // The Mac's numbers.
        assert_eq!(plan(Small, true), (60, Some(2048)));
        assert_eq!(plan(Balanced, true), (70, Some(4096)));
        assert_eq!(plan(High, true), (80, None));
    }

    #[test]
    fn alpha_is_kept_scaled_or_not() {
        for (preset, size) in [(Small, (2048, 15)), (High, (4200, 30))] {
            let mut source = Source::new((4200, 30));
            source.channels = 4;
            source.pad = 600_000;
            let c = case("photo.heic", &source.build(), preset);
            let (out, _) = c.output(&c.run("ok"));
            let picture = parse_fake(&out).unwrap();
            assert_eq!((picture.width, picture.height), size, "{preset:?}");
            assert_eq!(picture.channels, 4, "{preset:?}");
            if preset == High {
                assert_eq!(picture.rows, source.rows());
            }
        }
    }

    #[test]
    fn a_picture_the_helper_turned_says_orientation_1() {
        // An irot: the helper hands it back upright.
        for angle in [1, 2, 3] {
            let mut source = Source::new((64, 48));
            source.props = vec![irot(angle)];
            source.exif = Some(camera(6, (64, 48)));
            source.xmp = Some(xmp(6));
            let c = case("photo.heic", &source.build(), Balanced);
            let (_, s) = c.output(&c.run("ok"));
            let (o, dims, ifd1) = exif_of(&s.carried[0].data);
            assert_eq!((o, dims, ifd1), (Some(1), Some((64, 48)), false), "{angle}");
            assert_eq!(s.carried[1].data, xmp(1).as_bytes());
        }
        // Orientation 1 already, with an irot: no IFD1 goes.
        let mut source = Source::new((64, 48));
        source.props = vec![irot(1)];
        let c = case("photo.heic", &source.build(), Balanced);
        let (_, s) = c.output(&c.run("ok"));
        assert_eq!(exif_of(&s.carried[0].data), (Some(1), Some((64, 48)), true));
    }

    #[test]
    fn a_picture_that_turns_only_by_its_exif_keeps_it() {
        let mut source = Source::new((64, 48));
        source.exif = Some(camera(6, (64, 48)));
        source.xmp = Some(xmp(6));
        let c = case("photo.heic", &source.build(), Balanced);
        let (_, s) = c.output(&c.run("ok"));
        assert_eq!(exif_of(&s.carried[0].data), (Some(6), Some((64, 48)), true));
        assert_eq!(s.carried[1].data, xmp(6).as_bytes());
        // A helper that turned it by its Exif after all (ImageIO may): its
        // sides come back swapped, so it's upright now.
        for (o, turned) in [(6, true), (8, true), (5, true), (3, false), (1, false)] {
            let mut source = Source::new((48, 64));
            source.ispe = Some((64, 48));
            source.exif = Some(camera(o, (64, 48)));
            let c = case("photo.heic", &source.build(), Balanced);
            let (_, s) = c.output(&c.run("ok"));
            let (found, dims, ifd1) = exif_of(&s.carried[0].data);
            assert_eq!(dims, Some((48, 64)));
            let expected = if turned {
                (Some(1), false)
            } else {
                (Some(o), true)
            };
            assert_eq!((found, ifd1), expected, "{o}");
        }
        // Square: nothing to tell by.
        let mut source = Source::new((48, 48));
        source.exif = Some(camera(6, (48, 48)));
        let c = case("photo.heic", &source.build(), Balanced);
        let (_, s) = c.output(&c.run("ok"));
        assert_eq!(exif_of(&s.carried[0].data).0, Some(6));
    }

    #[test]
    fn a_wide_color_space_with_no_profile_back_is_refused() {
        for (primaries, transfer, profile, refused) in [
            (12, 13, false, true),
            (9, 16, false, true),
            (12, 13, true, false),
            (1, 13, false, false),
            (2, 2, false, false),
        ] {
            let mut source = Source::new((64, 48));
            source.props = vec![nclx(primaries, transfer, 6, true)];
            source.icc = profile.then(rgb_profile);
            let c = case("photo.heic", &source.build(), Balanced);
            let ran = c.run("ok");
            if refused {
                let e = c.refused(&ran);
                assert_eq!(e.kind, ErrorKind::NotYetSupported);
                assert_eq!(e.user_message, format!("photo.heic {WIDE}"));
            } else {
                let (out, _) = c.output(&ran);
                assert_eq!(parse_fake(&out).unwrap().icc, source.icc, "{primaries}");
            }
        }
    }

    #[test]
    fn refusals_come_before_the_helper_decodes() {
        // At High over one HEVC picture, by the helper's own bounds: before
        // the helper starts at all.
        let source = Source::new((64, 48));
        let c = case("photo.heic", &source.build(), High);
        let limits = Limits {
            helper: grape_heif_proto::Limits {
                max_encode_pixels: 64 * 48 - 1,
                ..grape_heif_proto::Limits::DEFAULT
            },
            ..c.limits()
        };
        let ran = c.run_with(nowhere(), limits, None);
        assert_eq!(
            c.refused(&ran).user_message,
            "photo.heic is too large to compress at High yet. Try Balanced."
        );
        assert!(ran.progress.is_empty());
        // 10 bits.
        let mut source = Source::new((64, 48));
        source.props = vec![pixi(&[10, 10, 10])];
        let c = case("photo.heic", &source.build(), Small);
        let ran = c.run_with(nowhere(), c.limits(), None);
        assert_eq!(
            c.refused(&ran).user_message,
            "photo.heic is a 10-bit (HDR) photo, which Compress can't keep yet."
        );
        // A helper that can't write HEIC: D33's words, before any decode.
        let c = case("photo.heic", &Source::new((64, 48)).build(), Balanced);
        let ran = c.run("no-heic-encoder");
        let e = c.refused(&ran);
        assert_eq!(e.kind, ErrorKind::NotYetSupported);
        assert!(e.user_message.contains("can't write HEIC files"), "{e:?}");
        assert!(ran.progress.is_empty());
        // One whose decode fails says so, as a conversion would.
        let c = case("photo.heic", &Source::new((64, 48)).build(), Balanced);
        let e = c.run("error-damaged").result.unwrap_err();
        assert_eq!(e.kind, ErrorKind::DecodeFailed, "{e:?}");
    }

    #[test]
    fn what_isnt_smaller_enough_is_not_written() {
        // No padding: the fake writes the pixels as they are, so the file
        // can't shrink by a tenth. Small says so; the others say Try Small.
        for (preset, words) in [
            (Small, "is already as small as Grape makes it."),
            (Balanced, "is already compact. Try Small."),
            (High, "is already compact. Try Small."),
        ] {
            let mut source = Source::new((64, 48));
            source.pad = 0;
            let c = case("photo.heic", &source.build(), preset);
            let ran = c.run("ok");
            let e = c.refused(&ran);
            assert_eq!(e.kind, ErrorKind::NoChange);
            assert_eq!(e.user_message, format!("photo.heic {words}"));
        }
    }

    #[test]
    fn the_last_size_check_counts_what_the_graft_adds() {
        // Twelve items with long names and a byte each: the graft writes
        // their names again, past the 256 bytes the early check counts.
        let build = |pad: usize| {
            let mut source = Source::new((64, 48));
            (source.exif, source.xmp, source.pad) = (None, None, pad);
            let mut b = source.builder();
            for i in 0..12u32 {
                let name = format!("{i}{}", "n".repeat(60));
                let mut item = Item::mime(100 + i, "text/plain", b"x");
                item.name = Some(Box::leak(name.into_boxed_str()));
                b.items.push(item);
                b.refs.push((*b"cdsc", 100 + i, vec![1]));
            }
            b.build().0
        };
        // Its output's size, from a run with room to spare.
        let c = case("photo.heic", &build(100_000), Balanced);
        let (out, s) = c.output(&c.run("ok"));
        assert_eq!(s.carried.len(), 12);
        let after = out.len();
        let plain = build(0).len();
        // A source just short of making that 90 %: the early check passes,
        // the last doesn't.
        let before = (after * 10).div_ceil(9) - 1;
        let source = build(before - plain - 8);
        assert_eq!(source.len(), before);
        let c = case("photo.heic", &source, Balanced);
        let ran = c.run("ok");
        let e = c.refused(&ran);
        assert_eq!(e.kind, ErrorKind::NoChange);
        assert!(
            e.detail
                .starts_with(&format!("{after} bytes from {before}")),
            "{e:?}"
        );
    }

    #[test]
    fn a_cancel_at_any_report_saves_nothing() {
        let source = Source::new((64, 48));
        let c = case("photo.heic", &source.build(), Balanced);
        let reports = c.run("ok").progress;
        fs::remove_file(c.dest()).unwrap();
        assert!(reports.len() >= 6, "{reports:?}");
        for &at in reports.iter().filter(|&&f| f < 1.0) {
            let ran = c.run_with(fake("ok"), c.limits(), Some(at));
            let e = ran.result.unwrap_err();
            assert_eq!(e.kind, ErrorKind::Cancelled, "at {at}: {e:?}");
            assert_eq!(ran.progress.last(), Some(&at));
        }
        // Cancelled before it starts.
        let ctx = EngineCtx::default();
        ctx.cancel.cancel();
        let decoder = Decoder::new(nowhere(), c.limits());
        let encoder = Encoder::new(nowhere(), c.limits(), HeifFormat::Heic);
        let e = run_with(
            &c.job,
            &c.src,
            &c.dest(),
            Format::Heic,
            Balanced,
            &ctx,
            &mut |_| {},
            &decoder,
            &encoder,
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled);
        c.check(false);
    }

    #[test]
    fn with_no_helper_it_says_so() {
        let c = case("photo.heic", &Source::new((64, 48)).build(), Balanced);
        let e = run(
            &c.job,
            &c.src,
            &c.dest(),
            Format::Heic,
            Balanced,
            &EngineCtx::default(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotYetSupported);
        assert_eq!(e.user_message, "Grape's image helper is missing.");
        c.check(false);
    }

    #[test]
    fn the_peak_counts_each_step() {
        let limit = crate::image::WORKING_SET_LIMIT;
        // A 48 MP photo at Balanced and High, a 200 MP one at Small and
        // Balanced, with alpha too: within 1 GiB.
        for (w, h, bpp, preset) in [
            (8064, 6048, 3, Balanced),
            (6880, 5160, 3, High),
            (6880, 5160, 4, High),
            (16_320, 12_240, 3, Small),
            (16_320, 12_240, 3, Balanced),
        ] {
            let p = peak(w, h, bpp, plan(preset, false).1);
            assert!(p <= limit, "{w}x{h}x{bpp} {preset:?}: {p}");
        }
        // At High, the picture, and out.heic beside it.
        assert_eq!(peak(100, 10, 3, None), 100 * 10 * (3 + 2));
        assert_eq!(peak(100, 10, 4, None), 100 * 10 * (4 + 3));
        // Scaled: what scale.rs counts, with the scaled picture's encode.
        assert_eq!(
            peak(4097, 1024, 3, Some(4096)),
            4097 * 1024 * 3 + 4097 * 1024 * 16 + 4096 * 1024 * 3
        );
        // 16 bytes a pixel past it, refused at the image line.
        assert!(peak(20_000, 15_000, 4, None) > limit);
    }

    #[test]
    fn xmp_orientation_is_set_in_both_forms_and_nothing_else() {
        for (before, after) in [
            ("tiff:Orientation=\"6\"", "tiff:Orientation=\"1\""),
            ("tiff:Orientation='8'", "tiff:Orientation='1'"),
            (
                "<tiff:Orientation>3</tiff:Orientation>",
                "<tiff:Orientation>1</tiff:Orientation>",
            ),
            ("tiff:Orientation=\"16\"", "tiff:Orientation=\"16\""),
            ("tiff:Orientation=\"6'", "tiff:Orientation=\"6'"),
            ("tiff:Orientation = \"6\"", "tiff:Orientation = \"6\""),
            (
                "<tiff:Orientation>x</tiff:Orientation>",
                "<tiff:Orientation>x</tiff:Orientation>",
            ),
            ("tiff:Orientation", "tiff:Orientation"),
            ("tiff:Orientation=\"", "tiff:Orientation=\""),
        ] {
            let mut x = before.as_bytes().to_vec();
            upright_xmp(&mut x);
            assert_eq!(x, after.as_bytes(), "{before}");
        }
        let mut both = xmp(6).into_bytes();
        upright_xmp(&mut both);
        assert_eq!(both, xmp(1).as_bytes());
    }

    #[test]
    fn exif_items_of_any_form_are_patched_where_they_can_be() {
        // Its TIFF right after its offset; behind Exif\0\0; an offset past
        // its end; too short for one.
        let tiff = camera(6, (64, 48));
        for (data, at) in [
            ([&0u32.to_be_bytes()[..], &tiff].concat(), Some(4)),
            (
                [&6u32.to_be_bytes()[..], b"Exif\0\0", &tiff].concat(),
                Some(10),
            ),
            ([&2u32.to_be_bytes()[..], b"xx", &tiff].concat(), Some(6)),
            ([&99u32.to_be_bytes()[..], &tiff[..10]].concat(), None),
            (vec![0, 0], None),
        ] {
            let mut d = data.clone();
            if let Some(t) = exif_tiff(&mut d) {
                upright_exif(t, 32, 24, true);
            }
            match at {
                Some(at) => {
                    let walked = exif::walk(&d[at..]);
                    assert_eq!(walked.orientation, Some(1));
                    assert!(!walked.fields.contains(&MetaField::Thumbnail));
                }
                None => assert_eq!(d, data),
            }
        }
    }

    #[test]
    fn colr_profiles_count_as_a_profile_in_the_source_only_through_the_helper() {
        // A source with an ICC profile in its colr and a wide nclx: what
        // decides is the profile the helper hands back (the fake: none).
        let mut source = Source::new((64, 48));
        source.props = vec![colr_icc(b"prof", b"profile"), nclx(12, 13, 0, true)];
        let c = case("photo.heic", &source.build(), Balanced);
        let e = c.run("ok").result.unwrap_err();
        assert_eq!(e.user_message, format!("photo.heic {WIDE}"));
    }
}
