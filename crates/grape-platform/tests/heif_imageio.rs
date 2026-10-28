//! ImageIO as Grape's HEIC codec on macOS (DECISIONS D33, heic-spec §14),
//! against the fixtures CI's Windows `grape-heif` job writes with libheif
//! (`grape-heif-fixtures`, heic-spec §5.6).
//!
//! Run with `cargo test -p grape-platform --features heif-it` and
//! `GRAPE_TEST_HEIF_FIXTURES` set to that folder. Without it these tests
//! fail rather than skip, so CI's `heif-it-macos` job can't pass by
//! accident. Encoding needs a hardware HEVC encoder, which GitHub's macOS
//! VMs don't have: there the encode test checks for "noEncoder" and says
//! so; a real Mac encodes and decodes back.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use grape_heif_proto::{
    serve, Codec, FailKind, Failure, HeifFormat, ImageInfo, Limits, Message, Op, PixelSink, Pixels,
    Request, PROTOCOL,
};
use grape_platform::macos::heif::ImageIo;

/// The fixtures folder, or a failure that says how to get it.
fn fixtures() -> PathBuf {
    let dir = std::env::var_os("GRAPE_TEST_HEIF_FIXTURES").expect(
        "GRAPE_TEST_HEIF_FIXTURES: the folder grape-heif-fixtures wrote (CI's heif-fixtures artifact)",
    );
    PathBuf::from(dir)
}

fn fixture(name: &str) -> PathBuf {
    let path = fixtures().join(name);
    assert!(path.is_file(), "the fixture {name} is missing");
    path
}

/// A sink that keeps everything.
#[derive(Default)]
struct Collect {
    info: Option<ImageInfo>,
    icc: Option<Vec<u8>>,
    rows: Vec<Vec<u8>>,
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
        assert_eq!(row.len(), (info.width * u32::from(info.channels)) as usize);
        self.rows.push(row.to_vec());
        Ok(())
    }
}

impl Collect {
    fn size(&self) -> (u32, u32, u8) {
        let info = self.info.expect("an image");
        (info.width, info.height, info.channels)
    }

    fn pixel(&self, x: u32, y: u32) -> &[u8] {
        let channels = usize::from(self.size().2);
        let start = x as usize * channels;
        &self.rows[y as usize][start..start + channels]
    }
}

fn decode(path: &Path) -> Result<Collect, Failure> {
    let mut sink = Collect::default();
    ImageIo.decode(path, HeifFormat::Heic, &Limits::DEFAULT, &mut sink)?;
    let (_, height, _) = sink.size();
    assert_eq!(sink.rows.len(), height as usize, "every row");
    Ok(sink)
}

/// Each channel of `got` within `tolerance` of `want`.
fn near(got: &[u8], want: &[u8], tolerance: u8, at: &str) {
    for (g, w) in got.iter().zip(want) {
        assert!(
            g.abs_diff(*w) <= tolerance,
            "{at}: {got:?}, expected {want:?} ± {tolerance}"
        );
    }
}

#[test]
fn this_mac_decodes_heic() {
    let caps = ImageIo.caps();
    assert!(caps.heic.decode);
    assert_eq!(caps.codec, "ImageIO");
}

#[test]
fn opaque_blocks_keep_their_colors() {
    let image = decode(&fixture("opaque.heic")).unwrap();
    assert_eq!(image.size(), (128, 96, 3));
    // A 4×3 grid of 32×32 blocks, row by row.
    let blocks: [[u8; 3]; 12] = [
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
    for (i, want) in blocks.iter().enumerate() {
        let (x, y) = ((i % 4) as u32 * 32 + 16, (i / 4) as u32 * 32 + 16);
        near(image.pixel(x, y), want, 10, &format!("block {i}"));
    }
}

#[test]
fn alpha_comes_back_straight() {
    let image = decode(&fixture("alpha.heic")).unwrap();
    assert_eq!(image.size(), (64, 64, 4));
    near(image.pixel(16, 32), &[255, 0, 0, 255], 12, "left half");
    let right = image.pixel(48, 32);
    near(&right[3..], &[128], 6, "right half's alpha");
    // Straight, not premultiplied: still red, not dark red.
    near(&right[..3], &[255, 0, 0], 12, "right half's color");
}

#[test]
fn irot_is_applied_so_the_image_is_upright() {
    let image = decode(&fixture("rotate90.heic")).unwrap();
    // Stored 96×64, turned 90° clockwise.
    assert_eq!(image.size(), (64, 96, 3));
    near(image.pixel(56, 8), &[255, 0, 0], 12, "red at the top right");
    near(
        image.pixel(8, 8),
        &[255, 255, 255],
        12,
        "white at the top left",
    );
    near(
        image.pixel(8, 88),
        &[255, 255, 255],
        12,
        "white at the bottom left",
    );
}

#[test]
fn a_grid_image_decodes_whole() {
    let image = decode(&fixture("grid.heic")).unwrap();
    assert_eq!(image.size(), (128, 128, 3));
    near(image.pixel(32, 32), &[255, 0, 0], 10, "top left tile");
    near(image.pixel(96, 32), &[0, 255, 0], 10, "top right tile");
    near(image.pixel(32, 96), &[0, 0, 255], 10, "bottom left tile");
    near(
        image.pixel(96, 96),
        &[255, 255, 255],
        10,
        "bottom right tile",
    );
}

#[test]
fn odd_sizes_keep_their_size() {
    let image = decode(&fixture("odd.heic")).unwrap();
    assert_eq!(image.size(), (3, 5, 3));
    near(image.pixel(1, 2), &[0, 200, 0], 12, "the middle");
}

#[test]
fn an_rgb_icc_profile_is_kept() {
    let image = decode(&fixture("icc.heic")).unwrap();
    assert_eq!(image.size().0, 64);
    let icc = image.icc.as_deref().expect("the ICC profile");
    assert!(icc.len() > 20);
    assert_eq!(&icc[16..20], b"RGB ");
    assert_eq!(image.info.unwrap().icc_bytes as usize, icc.len());
}

#[test]
fn a_truncated_heic_is_damaged() {
    let err = decode(&fixture("truncated.heic")).err().expect("an error");
    assert_eq!(err.kind, FailKind::Damaged, "{}", err.detail);
}

/// One request through `serve`, as Grape sends it: the lines that came
/// back, and whether it ended with `done`.
fn served(op: Op, work_dir: &Path) -> (Vec<Message>, bool) {
    let request = Request {
        protocol: PROTOCOL,
        work_dir: work_dir.to_path_buf(),
        op,
        limits: Limits::DEFAULT,
    };
    let input = serde_json::to_vec(&request).unwrap();
    let mut output = Vec::new();
    let done = serve(&ImageIo, input.as_slice(), &mut output);
    let lines = String::from_utf8(output).unwrap();
    let messages = lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (messages, done)
}

fn work_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("grape-heif-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_avci_brand_is_refused_before_imageio_sees_it() {
    let dir = work_dir("avci");
    let (messages, done) = served(
        Op::Decode {
            source: fixture("avci-brand.heic"),
        },
        &dir,
    );
    assert!(!done);
    match messages.last() {
        Some(Message::Error(failure)) => assert!(
            matches!(failure.kind, FailKind::Unsupported | FailKind::NotHeif),
            "{failure:?}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(!dir.join(grape_heif_proto::names::PIXELS).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_served_decode_writes_the_pixel_file() {
    let dir = work_dir("decode");
    let (messages, done) = served(
        Op::Decode {
            source: fixture("opaque.heic"),
        },
        &dir,
    );
    assert!(done, "{messages:?}");
    assert!(matches!(
        messages.first(),
        Some(Message::Image(ImageInfo {
            width: 128,
            height: 96,
            channels: 3,
            ..
        }))
    ));
    let path = dir.join(grape_heif_proto::names::PIXELS);
    let len = std::fs::metadata(&path).unwrap().len();
    let limits = grape_heif_proto::PixelLimits {
        max_pixels: Limits::DEFAULT.max_pixels,
        max_side: u32::MAX,
        max_icc_bytes: Limits::DEFAULT.max_icc_bytes,
    };
    let pixels =
        grape_heif_proto::read_pixels(std::fs::File::open(&path).unwrap(), len, &limits).unwrap();
    assert_eq!((pixels.width, pixels.height, pixels.channels), (128, 96, 3));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_served_probe_says_it_decodes() {
    let dir = work_dir("probe");
    let (messages, done) = served(Op::Probe {}, &dir);
    assert!(done);
    match messages.last() {
        Some(Message::Done(done)) => {
            let caps = done.caps.as_ref().expect("caps");
            assert!(caps.heic.decode);
            println!("ImageIO encodes HEIC here: {}", caps.heic.encode);
        }
        other => panic!("{other:?}"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A smooth RGB gradient, or RGBA with half alpha on the right.
fn gradient(width: u32, height: u32, channels: u8) -> Pixels {
    let mut data = Vec::new();
    for y in 0..height {
        for x in 0..width {
            data.push((x * 255 / (width - 1)) as u8);
            data.push((y * 255 / (height - 1)) as u8);
            data.push(128);
            if channels == 4 {
                data.push(if x < width / 2 { 255 } else { 128 });
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

#[test]
fn encode_round_trips_or_says_this_mac_cant() {
    let dir = work_dir("encode");
    for channels in [3, 4] {
        let pixels = gradient(64, 48, channels);
        let heic = match ImageIo.encode(&pixels, HeifFormat::Heic, None, &Limits::DEFAULT) {
            Ok(heic) => heic,
            Err(failure) if failure.kind == FailKind::NoEncoder => {
                println!("this Mac can't write HEIC: {}", failure.detail);
                // Not listed: the list check said so. Listed (GitHub's VMs
                // and Macs without the hardware): no destination, or no
                // finish.
                let listed = ImageIo.caps().heic.encode;
                let why = failure.detail.as_str();
                let expected = if listed {
                    why == "ImageIO made no HEIC destination"
                        || why.starts_with("ImageIO couldn't finish the HEIC")
                } else {
                    why == "public.heic isn't an image destination type"
                };
                assert!(expected, "listed {listed}: {why}");
                std::fs::remove_dir_all(&dir).unwrap();
                return;
            }
            Err(failure) => panic!("{failure:?}"),
        };
        assert_eq!(
            grape_heif_proto::sniff(&heic[..heic.len().min(grape_heif_proto::SNIFF_BYTES)]),
            grape_heif_proto::Sniff::Heic
        );
        let path = dir.join(format!("round-trip-{channels}.heic"));
        std::fs::write(&path, &heic).unwrap();
        let back = decode(&path).unwrap();
        let (width, height, got) = back.size();
        assert_eq!((width, height), (64, 48));
        if channels == 4 {
            assert_eq!(got, 4, "alpha kept");
            near(&back.pixel(48, 24)[3..], &[128], 8, "alpha");
        }
        // Mean absolute error over the color channels.
        let mut total = 0u64;
        for y in 0..height {
            for x in 0..width {
                let want = &pixels.data[((y * width + x) * u32::from(channels)) as usize..][..3];
                let have = &back.pixel(x, y)[..3];
                total += want
                    .iter()
                    .zip(have)
                    .map(|(a, b)| u64::from(a.abs_diff(*b)))
                    .sum::<u64>();
            }
        }
        let mean = total as f64 / f64::from(width * height * 3);
        assert!(mean < 4.0, "{channels} channels: mean error {mean}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
