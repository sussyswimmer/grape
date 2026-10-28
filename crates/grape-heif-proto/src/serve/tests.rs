#![allow(clippy::unwrap_used)]

use std::fs;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::{FormatCaps, MAX_LINE_BYTES};

/// What the fake codec does.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    /// Decode an RGB image (encode: a file of the format asked for).
    Rgb,
    /// Decode an RGBA image.
    Rgba,
    /// Decode RGB with an ICC profile of this many bytes.
    Icc(usize),
    /// Panic, with a path in the message.
    Panic,
    /// Fail with this kind.
    Fail(FailKind),
    /// Claim this many channels.
    Channels(u8),
    /// Make row 1 this many bytes longer (or shorter).
    RowLen(i32),
    /// Give this many rows instead of the height.
    Rows(u32),
    /// Return before `begin`.
    NoBegin,
    /// Call `begin` twice.
    BeginTwice,
    /// Give a row before `begin`.
    RowFirst,
    /// Ignore the sink's error and return `Ok`.
    IgnoreSink,
    /// Replace the sink's error with one of its own.
    OwnError,
    /// Recurse deeply before decoding.
    DeepStack,
    /// Encode: return bytes that aren't HEIF.
    NotHeic,
    /// Encode: return the other format's bytes.
    OtherFormat,
}

struct Fake {
    mode: Mode,
    width: u32,
    height: u32,
    calls: AtomicUsize,
    encoded: Mutex<Vec<(Pixels, HeifFormat, Option<u8>)>>,
    decoded: Mutex<Vec<HeifFormat>>,
    thread: Mutex<Option<String>>,
}

impl Fake {
    fn new(mode: Mode, width: u32, height: u32) -> Fake {
        Fake {
            mode,
            width,
            height,
            calls: AtomicUsize::new(0),
            encoded: Mutex::new(Vec::new()),
            decoded: Mutex::new(Vec::new()),
            thread: Mutex::new(None),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Codec for Fake {
    fn caps(&self) -> Caps {
        Caps {
            heic: FormatCaps {
                decode: true,
                encode: true,
            },
            avif: FormatCaps {
                decode: true,
                encode: false,
            },
            codec: "fake 1.0".to_string(),
        }
    }

    fn decode(
        &self,
        _source: &Path,
        format: HeifFormat,
        _limits: &Limits,
        out: &mut dyn PixelSink,
    ) -> Result<(), Failure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.decoded.lock().unwrap().push(format);
        *self.thread.lock().unwrap() = thread::current().name().map(str::to_string);
        let channels = match self.mode {
            Mode::Rgba => 4,
            Mode::Channels(n) => n,
            _ => 3,
        };
        let icc = match self.mode {
            Mode::Icc(n) => Some(vec![7u8; n]),
            _ => None,
        };
        match self.mode {
            Mode::Panic => panic!("decoding /secret/IMG_0001.HEIC"),
            Mode::Fail(kind) => return Err(Failure::new(kind, "from the codec")),
            Mode::NoBegin => return Ok(()),
            Mode::RowFirst => return out.row(&row(self.width, channels, 0)),
            Mode::DeepStack => {
                black_box(depth(16_000));
            }
            _ => {}
        }
        let info = ImageInfo {
            width: self.width,
            height: self.height,
            channels,
            // Ignored by the sink: the profile's own length counts.
            icc_bytes: 999,
        };
        let begun = out.begin(&info, icc.as_deref());
        match (self.mode, begun) {
            (Mode::IgnoreSink, _) => return Ok(()),
            (Mode::OwnError, Err(_)) => return Err(Failure::new(FailKind::Damaged, "mine")),
            (_, begun) => begun?,
        }
        if self.mode == Mode::BeginTwice {
            out.begin(&info, None)?;
        }
        let rows = match self.mode {
            Mode::Rows(n) => n,
            _ => self.height,
        };
        for y in 0..rows {
            let mut bytes = row(self.width, channels, y);
            if let (Mode::RowLen(delta), 1) = (self.mode, y) {
                let len = bytes.len() as i64 + i64::from(delta);
                bytes.resize(len as usize, 0);
            }
            out.row(&bytes)?;
        }
        Ok(())
    }

    fn encode(
        &self,
        pixels: &Pixels,
        format: HeifFormat,
        quality: Option<u8>,
        _limits: &Limits,
    ) -> Result<Vec<u8>, Failure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut written = format;
        match self.mode {
            Mode::Panic => panic!("encoding /secret/IMG_0001.HEIC"),
            Mode::Fail(kind) => return Err(Failure::new(kind, "from the codec")),
            Mode::NotHeic => return Ok(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec()),
            Mode::OtherFormat => {
                written = match format {
                    HeifFormat::Heic => HeifFormat::Avif,
                    HeifFormat::Avif => HeifFormat::Heic,
                }
            }
            _ => {}
        }
        self.encoded
            .lock()
            .unwrap()
            .push((pixels.clone(), format, quality));
        let mut bytes = head(written);
        bytes.extend_from_slice(&pixels.data[..pixels.data.len().min(16)]);
        Ok(bytes)
    }
}

/// Uses about 1 KiB of stack per level.
fn depth(n: u32) -> u64 {
    let pad = black_box([n as u8; 1024]);
    if n == 0 {
        u64::from(pad[0])
    } else {
        depth(n - 1) + u64::from(pad[1])
    }
}

/// Row `y` of the fake's image.
fn row(width: u32, channels: u8, y: u32) -> Vec<u8> {
    (0..width * u32::from(channels))
        .map(|x| (x * 31 + y * 7) as u8)
        .collect()
}

fn rows(width: u32, height: u32, channels: u8) -> Vec<u8> {
    (0..height).flat_map(|y| row(width, channels, y)).collect()
}

/// The start of an iPhone HEIC.
fn heic_head() -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 24];
    bytes.extend_from_slice(b"ftypheic\0\0\0\0mif1heic");
    bytes
}

/// The start of an AVIF as libavif writes it.
fn avif_head() -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 32];
    bytes.extend_from_slice(b"ftypavif\0\0\0\0avifmif1miafMA1B");
    bytes
}

fn head(format: HeifFormat) -> Vec<u8> {
    match format {
        HeifFormat::Heic => heic_head(),
        HeifFormat::Avif => avif_head(),
    }
}

/// A folder under the OS temp folder, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new() -> Temp {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "grape-heif-proto-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir(&path).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A work folder and a HEIC-looking source in a folder of its own.
struct Case {
    work: Temp,
    _inputs: Temp,
    source: PathBuf,
    limits: Limits,
}

impl Case {
    fn new() -> Case {
        Case::with_source(&[heic_head(), vec![0; 100]].concat())
    }

    fn with_source(bytes: &[u8]) -> Case {
        let inputs = Temp::new();
        let source = inputs.0.join("IMG_0001.heic");
        fs::write(&source, bytes).unwrap();
        Case {
            work: Temp::new(),
            _inputs: inputs,
            source,
            limits: Limits::DEFAULT,
        }
    }

    fn work(&self) -> &Path {
        &self.work.0
    }

    fn request(&self, op: Op) -> Request {
        Request {
            protocol: PROTOCOL,
            work_dir: self.work().to_path_buf(),
            op,
            limits: self.limits,
        }
    }

    fn decode(&self) -> Request {
        self.request(Op::Decode {
            source: self.source.clone(),
        })
    }

    fn encode(&self, quality: Option<u8>) -> Request {
        self.encode_to(HeifFormat::Heic, quality)
    }

    fn encode_to(&self, format: HeifFormat, quality: Option<u8>) -> Request {
        self.request(Op::Encode { format, quality })
    }

    /// Writes `input.rgba`.
    fn input(&self, width: u32, height: u32, channels: u8, icc: Option<&[u8]>) -> Vec<u8> {
        let header = PixelHeader {
            width,
            height,
            channels,
            icc_len: icc.map_or(0, |p| p.len() as u32),
        };
        let mut writer = PixelWriter::begin(Vec::new(), &header, icc).unwrap();
        for y in 0..height {
            writer.row(&row(width, channels, y)).unwrap();
        }
        let bytes = writer.finish().unwrap();
        fs::write(self.work().join(names::INPUT), &bytes).unwrap();
        bytes
    }

    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.work())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn pixels(&self) -> Pixels {
        let path = self.work().join(names::PIXELS);
        let len = fs::metadata(&path).unwrap().len();
        let any = PixelLimits {
            max_pixels: u64::MAX,
            max_side: u32::MAX,
            max_icc_bytes: u32::MAX,
        };
        read_pixels(File::open(&path).unwrap(), len, &any).unwrap()
    }
}

/// One run of `serve`, with the lines it wrote.
struct Run {
    done: bool,
    lines: Vec<Message>,
}

impl Run {
    fn failure(&self) -> &Failure {
        match self.lines.last() {
            Some(Message::Error(failure)) => failure,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    fn progress(&self) -> Vec<f32> {
        self.lines
            .iter()
            .filter_map(|m| match m {
                Message::Progress(p) => Some(*p),
                _ => None,
            })
            .collect()
    }
}

/// Runs `serve` on raw request bytes and checks the rules every run keeps:
/// every line a message under the cap, exactly one last line, and the
/// return value matching it.
fn serve_bytes(codec: &Fake, input: &[u8]) -> Run {
    let mut out = Vec::new();
    let done = serve(codec, input, &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.ends_with('\n'), "{text:?}");
    let lines: Vec<Message> = text
        .lines()
        .map(|line| {
            assert!(line.len() <= MAX_LINE_BYTES);
            serde_json::from_str(line).unwrap()
        })
        .collect();
    let last = lines
        .iter()
        .filter(|m| matches!(m, Message::Done(_) | Message::Error(_)))
        .count();
    assert_eq!(last, 1, "{lines:?}");
    assert!(matches!(
        lines.last(),
        Some(Message::Done(_) | Message::Error(_))
    ));
    assert_eq!(done, matches!(lines.last(), Some(Message::Done(_))));
    let images = lines
        .iter()
        .filter(|m| matches!(m, Message::Image(_)))
        .count();
    assert!(images <= 1);
    Run { done, lines }
}

fn serve_request(codec: &Fake, request: &Request) -> Run {
    serve_bytes(codec, &serde_json::to_vec(request).unwrap())
}

fn assert_internal(run: &Run, part: &str) {
    let failure = run.failure();
    assert_eq!(failure.kind, FailKind::Internal, "{failure:?}");
    assert!(failure.detail.contains(part), "{failure:?} lacks {part:?}");
    assert!(!run.done);
}

#[test]
fn probe_reports_the_caps_and_touches_nothing() {
    let case = Case::new();
    let fake = Fake::new(Mode::Rgb, 1, 1);
    let run = serve_request(&fake, &case.request(Op::Probe {}));
    assert!(run.done);
    assert_eq!(
        run.lines,
        vec![Message::Done(Done {
            caps: Some(fake.caps()),
            ..Done::default()
        })]
    );
    assert_eq!(fake.calls(), 0);
    assert!(case.entries().is_empty());
}

#[test]
fn decode_writes_the_pixel_file_and_reports_it() {
    for (mode, channels) in [(Mode::Rgb, 3), (Mode::Rgba, 4)] {
        let case = Case::new();
        let before = fs::read(&case.source).unwrap();
        let fake = Fake::new(mode, 5, 7);
        let run = serve_request(&fake, &case.decode());
        assert!(run.done);
        assert_eq!(
            run.lines[0],
            Message::Image(ImageInfo {
                width: 5,
                height: 7,
                channels,
                icc_bytes: 0,
            })
        );
        assert_eq!(
            run.lines.last(),
            Some(&Message::Done(Done {
                width: 5,
                height: 7,
                channels,
                ..Done::default()
            }))
        );
        let pixels = case.pixels();
        assert_eq!(
            (pixels.width, pixels.height, pixels.channels),
            (5, 7, channels)
        );
        assert_eq!(pixels.data, rows(5, 7, channels));
        assert_eq!(pixels.icc, None);
        assert_eq!(fs::read(&case.source).unwrap(), before);
        assert_eq!(case.entries(), vec![names::PIXELS]);
        assert_eq!(fake.thread.lock().unwrap().as_deref(), Some(JOB_THREAD));
    }
}

#[test]
fn decode_keeps_an_icc_profile_within_the_limit() {
    let mut case = Case::new();
    case.limits.max_icc_bytes = 536;
    let run = serve_request(&Fake::new(Mode::Icc(536), 2, 2), &case.decode());
    assert!(run.done);
    assert!(matches!(
        run.lines[0],
        Message::Image(ImageInfo { icc_bytes: 536, .. })
    ));
    assert!(matches!(
        run.lines.last(),
        Some(Message::Done(Done { icc_bytes: 536, .. }))
    ));
    let pixels = case.pixels();
    assert_eq!(pixels.icc, Some(vec![7; 536]));
    assert_eq!(pixels.data, rows(2, 2, 3));
}

#[test]
fn an_icc_profile_over_the_limit_or_empty_is_dropped() {
    for size in [537, 0] {
        let mut case = Case::new();
        case.limits.max_icc_bytes = 536;
        let run = serve_request(&Fake::new(Mode::Icc(size), 2, 2), &case.decode());
        assert!(run.done);
        assert!(matches!(
            run.lines[0],
            Message::Image(ImageInfo { icc_bytes: 0, .. })
        ));
        assert_eq!(case.pixels().icc, None);
        assert_eq!(case.pixels().data, rows(2, 2, 3));
    }
}

#[test]
fn progress_goes_forward_in_steps() {
    let case = Case::new();
    let run = serve_request(&Fake::new(Mode::Rgb, 3, 100), &case.decode());
    assert!(run.done);
    let progress = run.progress();
    assert_eq!(progress.len(), 20);
    assert!(progress.windows(2).all(|w| w[0] < w[1]), "{progress:?}");
    assert!(progress.iter().all(|p| *p > 0.0 && *p <= 1.0));
    assert_eq!(progress.last(), Some(&1.0));
    assert!(matches!(run.lines[0], Message::Image(_)));

    let case = Case::new();
    let run = serve_request(&Fake::new(Mode::Rgb, 3, 1), &case.decode());
    assert_eq!(run.progress(), vec![1.0]);
    assert_eq!(run.lines.len(), 3);
}

#[test]
fn an_image_over_the_pixel_limit_is_refused_before_any_pixel() {
    let mut case = Case::new();
    case.limits.max_pixels = 99;
    let fake = Fake::new(Mode::Rgb, 10, 10);
    let run = serve_request(&fake, &case.decode());
    assert_eq!(
        run.lines,
        vec![Message::Error(Failure::new(
            FailKind::TooLarge,
            "10x10 is over 99 pixels"
        ))]
    );
    assert!(case.entries().is_empty());

    case.limits.max_pixels = 100;
    assert!(serve_request(&fake, &case.decode()).done);
}

#[test]
fn the_sinks_failure_wins_over_the_codecs_answer() {
    for mode in [Mode::IgnoreSink, Mode::OwnError] {
        let mut case = Case::new();
        case.limits.max_pixels = 10;
        let run = serve_request(&Fake::new(mode, 10, 10), &case.decode());
        assert_eq!(run.failure().kind, FailKind::TooLarge, "{mode:?}");
        assert!(case.entries().is_empty());
    }
    // A codec that stops without its rows is caught too.
    let case = Case::new();
    let run = serve_request(&Fake::new(Mode::IgnoreSink, 4, 4), &case.decode());
    assert_internal(&run, "0 of 4 rows");
}

#[test]
fn bad_codec_output_is_internal() {
    for (mode, width, height, part) in [
        (Mode::Channels(2), 5, 3, "2 channels"),
        (Mode::Channels(5), 5, 3, "5 channels"),
        (Mode::Channels(0), 5, 3, "0 channels"),
        (Mode::RowLen(-1), 5, 3, "a row of 14 bytes, not 15"),
        (Mode::RowLen(1), 5, 3, "a row of 16 bytes, not 15"),
        (Mode::Rows(2), 5, 3, "2 of 3 rows"),
        (Mode::Rows(0), 5, 3, "0 of 3 rows"),
        (Mode::Rows(4), 5, 3, "more rows than the height"),
        (Mode::NoBegin, 5, 3, "no image"),
        (Mode::BeginTwice, 5, 3, "twice"),
        (Mode::RowFirst, 5, 3, "a row before the image"),
        (Mode::Rgb, 0, 3, "empty image"),
        (Mode::Rgb, 3, 0, "empty image"),
    ] {
        let case = Case::new();
        let run = serve_request(&Fake::new(mode, width, height), &case.decode());
        assert_internal(&run, part);
    }
}

#[test]
fn a_panic_is_internal_and_names_nothing() {
    let case = Case::new();
    let fake = Fake::new(Mode::Panic, 2, 2);
    for request in [case.decode(), case.encode(None)] {
        if matches!(request.op, Op::Encode { .. }) {
            case.input(2, 2, 3, None);
        }
        let run = serve_request(&fake, &request);
        assert_eq!(
            run.lines,
            vec![Message::Error(Failure::new(
                FailKind::Internal,
                "the job panicked"
            ))]
        );
    }
    assert_eq!(fake.calls(), 2);
}

#[test]
fn the_job_thread_has_a_large_stack() {
    let case = Case::new();
    assert!(serve_request(&Fake::new(Mode::DeepStack, 2, 2), &case.decode()).done);
}

#[test]
fn codec_failures_pass_through() {
    for kind in [
        FailKind::NotHeif,
        FailKind::Unsupported,
        FailKind::Damaged,
        FailKind::NoImage,
        FailKind::TooLarge,
        FailKind::NoEncoder,
        FailKind::Encode,
        FailKind::Read,
        FailKind::Write,
        FailKind::Protocol,
        FailKind::Internal,
    ] {
        let case = Case::new();
        let fake = Fake::new(Mode::Fail(kind), 2, 2);
        let expected = vec![Message::Error(Failure::new(kind, "from the codec"))];
        assert_eq!(serve_request(&fake, &case.decode()).lines, expected);
        case.input(2, 2, 4, None);
        assert_eq!(serve_request(&fake, &case.encode(None)).lines, expected);
        assert_eq!(case.entries(), vec![names::INPUT]);
    }
}

#[test]
fn the_sniffed_format_reaches_the_codec() {
    let mut old_avif_brand = vec![0, 0, 0, 24];
    old_avif_brand.extend_from_slice(b"ftypavif\0\0\0\0mif1avif");
    old_avif_brand.resize(32, 0);
    for (bytes, format) in [
        ([heic_head(), vec![0; 100]].concat(), HeifFormat::Heic),
        ([avif_head(), vec![0; 100]].concat(), HeifFormat::Avif),
        // The content decides, whatever the name (`IMG_0001.heic`).
        (old_avif_brand, HeifFormat::Avif),
    ] {
        let case = Case::with_source(&bytes);
        let fake = Fake::new(Mode::Rgb, 2, 2);
        let run = serve_request(&fake, &case.decode());
        assert!(run.done, "{format:?}");
        assert_eq!(*fake.decoded.lock().unwrap(), vec![format]);
        assert_eq!(case.entries(), vec![names::PIXELS]);
    }
}

#[test]
fn the_source_is_checked_before_the_codec_sees_it() {
    // grape-heif-fixtures' `avci-brand.heic`: AVC, another HEIF codec.
    let mut avci = vec![0, 0, 0, 24];
    avci.extend_from_slice(b"ftypavci\0\0\0\0mif1avci");
    avci.resize(32, 0);
    for (bytes, kind) in [
        (b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec(), FailKind::NotHeif),
        (Vec::new(), FailKind::NotHeif),
        (avci, FailKind::Unsupported),
    ] {
        let case = Case::with_source(&bytes);
        let fake = Fake::new(Mode::Rgb, 2, 2);
        let run = serve_request(&fake, &case.decode());
        assert_eq!(run.failure().kind, kind);
        if kind == FailKind::Unsupported {
            assert_eq!(run.failure().detail, "a HEIF image that isn't HEVC or AV1");
        }
        assert_eq!(run.lines.len(), 1);
        assert_eq!(fake.calls(), 0);
        assert!(case.entries().is_empty());
    }

    let mut case = Case::new();
    let len = fs::metadata(&case.source).unwrap().len();
    case.limits.max_source_bytes = len - 1;
    let fake = Fake::new(Mode::Rgb, 2, 2);
    let run = serve_request(&fake, &case.decode());
    assert_eq!(
        run.failure(),
        &Failure::new(
            FailKind::TooLarge,
            format!("the file is {len} bytes, over {}", len - 1)
        )
    );
    assert_eq!(fake.calls(), 0);
    case.limits.max_source_bytes = len;
    assert!(serve_request(&fake, &case.decode()).done);
}

#[test]
fn an_existing_pixel_file_is_never_replaced() {
    let case = Case::new();
    fs::write(case.work().join(names::PIXELS), b"keep").unwrap();
    let run = serve_request(&Fake::new(Mode::Rgb, 2, 2), &case.decode());
    assert_eq!(run.failure().kind, FailKind::Write);
    assert_eq!(fs::read(case.work().join(names::PIXELS)).unwrap(), b"keep");
}

#[test]
fn encode_reads_input_rgba_and_writes_out_heic() {
    // And out.avif for an AVIF: the format reaches the codec, and only
    // that format's output is made.
    for (format, channels, icc, quality) in [
        (HeifFormat::Heic, 3, None, None),
        (HeifFormat::Heic, 4, None, Some(70)),
        (HeifFormat::Heic, 3, Some(&b"profile"[..]), Some(1)),
        (HeifFormat::Heic, 4, Some(&[9u8; 300][..]), Some(100)),
        (HeifFormat::Avif, 3, None, None),
        (HeifFormat::Avif, 4, Some(&b"profile"[..]), Some(50)),
    ] {
        let case = Case::new();
        let input = case.input(6, 4, channels, icc);
        let fake = Fake::new(Mode::Rgb, 0, 0);
        let run = serve_request(&fake, &case.encode_to(format, quality));
        assert!(run.done);
        let output = fs::read(case.work().join(names::output(format))).unwrap();
        assert_eq!(
            run.lines,
            vec![Message::Done(Done {
                bytes: output.len() as u64,
                ..Done::default()
            })]
        );
        assert_eq!(sniff(&output).format(), Some(format));
        let encoded = fake.encoded.lock().unwrap();
        let (pixels, seen_format, seen_quality) = &encoded[0];
        assert_eq!(*seen_format, format);
        assert_eq!(*seen_quality, quality);
        assert_eq!(
            (pixels.width, pixels.height, pixels.channels),
            (6, 4, channels)
        );
        assert_eq!(pixels.data, rows(6, 4, channels));
        assert_eq!(pixels.icc.as_deref(), icc);
        assert_eq!(fs::read(case.work().join(names::INPUT)).unwrap(), input);
        assert_eq!(case.entries(), vec![names::INPUT, names::output(format)]);
    }
}

#[test]
fn encode_holds_its_limits_before_the_codec() {
    for (width, height, icc, limits) in [
        (
            10,
            10,
            None,
            Limits {
                max_encode_pixels: 99,
                ..Limits::DEFAULT
            },
        ),
        (
            10,
            1,
            None,
            Limits {
                max_encode_side: 9,
                ..Limits::DEFAULT
            },
        ),
        (
            1,
            10,
            None,
            Limits {
                max_encode_side: 9,
                ..Limits::DEFAULT
            },
        ),
        (
            1,
            1,
            Some(&[1u8; 4][..]),
            Limits {
                max_icc_bytes: 3,
                ..Limits::DEFAULT
            },
        ),
    ] {
        let mut case = Case::new();
        case.limits = limits;
        case.input(width, height, 3, icc);
        let fake = Fake::new(Mode::Rgb, 0, 0);
        let run = serve_request(&fake, &case.encode(None));
        assert_eq!(run.failure().kind, FailKind::TooLarge, "{width}x{height}");
        assert_eq!(fake.calls(), 0);
        assert!(!case.work().join(names::OUTPUT_HEIC).exists());
    }
    // At the limits: accepted.
    let mut case = Case::new();
    case.limits.max_encode_pixels = 100;
    case.limits.max_encode_side = 10;
    case.input(10, 10, 3, None);
    assert!(serve_request(&Fake::new(Mode::Rgb, 0, 0), &case.encode(None)).done);
}

#[test]
fn a_missing_or_malformed_input_rgba_is_refused() {
    let case = Case::new();
    let fake = Fake::new(Mode::Rgb, 0, 0);
    let run = serve_request(&fake, &case.encode(None));
    assert_eq!(run.failure().kind, FailKind::Read);

    let mut bytes = case.input(2, 2, 3, None);
    bytes[0] = b'X';
    fs::write(case.work().join(names::INPUT), &bytes).unwrap();
    let run = serve_request(&fake, &case.encode(None));
    assert_internal(&run, "input.rgba: this isn't a Grape pixel file");

    let mut bytes = case.input(2, 2, 3, None);
    bytes.push(0);
    fs::write(case.work().join(names::INPUT), &bytes).unwrap();
    let run = serve_request(&fake, &case.encode(None));
    assert_internal(&run, "doesn't match its header");
    assert_eq!(fake.calls(), 0);
}

#[test]
fn the_encoders_output_must_be_the_format_asked_for() {
    for (mode, format, part) in [
        (
            Mode::NotHeic,
            HeifFormat::Heic,
            "the encoder's output isn't HEIC",
        ),
        (
            Mode::NotHeic,
            HeifFormat::Avif,
            "the encoder's output isn't AVIF",
        ),
        // HEIC bytes for an AVIF, and AVIF bytes for a HEIC.
        (
            Mode::OtherFormat,
            HeifFormat::Avif,
            "the encoder's output isn't AVIF",
        ),
        (
            Mode::OtherFormat,
            HeifFormat::Heic,
            "the encoder's output isn't HEIC",
        ),
    ] {
        let case = Case::new();
        case.input(2, 2, 3, None);
        let run = serve_request(&Fake::new(mode, 0, 0), &case.encode_to(format, None));
        assert_internal(&run, part);
        assert_eq!(case.entries(), vec![names::INPUT], "{mode:?} {format:?}");
    }
}

#[test]
fn an_existing_output_is_never_replaced() {
    for format in HeifFormat::ALL {
        let case = Case::new();
        case.input(2, 2, 3, None);
        let out = case.work().join(names::output(format));
        fs::write(&out, b"keep").unwrap();
        let run = serve_request(&Fake::new(Mode::Rgb, 0, 0), &case.encode_to(format, None));
        assert_eq!(run.failure().kind, FailKind::Write);
        let name = names::output(format);
        assert_eq!(
            run.failure().detail,
            format!("creating {name}: AlreadyExists")
        );
        assert_eq!(fs::read(&out).unwrap(), b"keep");
    }
    // The other format's output doesn't stand in the way.
    let case = Case::new();
    case.input(2, 2, 3, None);
    fs::write(case.work().join(names::OUTPUT_HEIC), b"keep").unwrap();
    let run = serve_request(
        &Fake::new(Mode::Rgb, 0, 0),
        &case.encode_to(HeifFormat::Avif, None),
    );
    assert!(run.done);
    assert_eq!(
        fs::read(case.work().join(names::OUTPUT_HEIC)).unwrap(),
        b"keep"
    );
}

#[test]
fn quality_must_be_1_to_100() {
    for quality in [0, 101, 255] {
        let case = Case::new();
        case.input(2, 2, 3, None);
        let fake = Fake::new(Mode::Rgb, 0, 0);
        let run = serve_request(&fake, &case.encode(Some(quality)));
        assert_internal(&run, &format!("quality {quality} isn't in 1..=100"));
        assert_eq!(fake.calls(), 0);
    }
}

#[test]
fn paths_must_be_absolute_and_of_the_right_kind() {
    let case = Case::new();
    fs::write(case.work().join("a file"), b"").unwrap();
    let source_dir = case.source.parent().unwrap().to_path_buf();
    let fake = Fake::new(Mode::Rgb, 2, 2);
    let mut requests = Vec::new();
    for work_dir in [
        PathBuf::from("work"),
        case.work().join("gone"),
        case.work().join("a file"),
    ] {
        requests.push((
            Request {
                work_dir,
                ..case.request(Op::Probe {})
            },
            "the work folder",
        ));
    }
    for source in [
        PathBuf::from("IMG_0001.heic"),
        source_dir.join("gone.heic"),
        source_dir.clone(),
    ] {
        requests.push((case.request(Op::Decode { source }), "the source"));
    }
    for (request, part) in requests {
        let run = serve_request(&fake, &request);
        assert_internal(&run, part);
        assert!(!run.failure().detail.contains("IMG"));
        assert!(!run.failure().detail.contains("gone"));
    }
    assert_eq!(fake.calls(), 0);
}

#[test]
fn no_limit_may_be_zero() {
    let zeroed: [fn(&mut Limits); 6] = [
        |l| l.max_source_bytes = 0,
        |l| l.max_pixels = 0,
        |l| l.max_encode_pixels = 0,
        |l| l.max_encode_side = 0,
        |l| l.max_icc_bytes = 0,
        |l| l.max_decoder_memory = 0,
    ];
    for zero in zeroed {
        let mut case = Case::new();
        zero(&mut case.limits);
        let run = serve_request(&Fake::new(Mode::Rgb, 2, 2), &case.decode());
        assert_internal(&run, "a limit is 0");
    }
}

#[test]
fn another_protocol_is_refused_as_such() {
    let case = Case::new();
    let fake = Fake::new(Mode::Rgb, 2, 2);
    for protocol in [0, 1, 3, u32::MAX] {
        let run = serve_request(
            &fake,
            &Request {
                protocol,
                ..case.decode()
            },
        );
        assert_eq!(
            run.failure(),
            &Failure::new(
                FailKind::Protocol,
                format!("protocol {protocol}, but this helper speaks 2")
            )
        );
    }
    // A later version's request, with fields this one doesn't know.
    let later =
        br#"{"protocol":3,"workDir":"/t","op":{"kind":"thumbnail","size":256},"budget":{}}"#;
    assert_eq!(serve_bytes(&fake, later).failure().kind, FailKind::Protocol);
    // And protocol 1's encode, without a format: refused as protocol 1.
    let earlier = br#"{"protocol":1,"workDir":"/t","op":{"kind":"encode"},"limits":{}}"#;
    assert_eq!(
        serde_json::to_value(serve_bytes(&fake, earlier).failure()).unwrap(),
        serde_json::json!({"kind": "protocol", "detail": "protocol 1, but this helper speaks 2"})
    );
    assert_eq!(fake.calls(), 0);
}

#[test]
fn a_bad_request_is_internal_and_never_quoted() {
    let case = Case::new();
    let fake = Fake::new(Mode::Rgb, 2, 2);
    let good = serde_json::to_string(&case.decode()).unwrap();
    let inputs = [
        String::new(),
        "{".to_string(),
        "not json: /secret/IMG_0001.HEIC".to_string(),
        r#"{"protocol":2,"workDir":"/secret/IMG_0001.HEIC"}"#.to_string(),
        r#"{"protocol":"2","workDir":"/secret/IMG_0001.HEIC"}"#.to_string(),
        good.replace(r#""kind":"decode""#, r#""kind":"encode""#),
        good.replacen('{', r#"{"note":"/secret/IMG_0001.HEIC","#, 1),
        format!("{good} /secret/IMG_0001.HEIC"),
        format!("{good}{good}"),
        // serde reads a struct from a sequence too, so this one's version is
        // this helper's own.
        "[2]".to_string(),
    ];
    for input in inputs {
        let run = serve_bytes(&fake, input.as_bytes());
        assert_internal(&run, "bad request (");
        assert!(!run.failure().detail.contains("secret"), "{input}");
        assert!(!run.failure().detail.contains("IMG"), "{input}");
    }
    assert_eq!(
        serve_bytes(&fake, b"").failure().detail,
        "bad request (Eof at 1:0)"
    );
    assert_eq!(fake.calls(), 0);
}

#[test]
fn a_request_over_the_cap_is_refused() {
    let case = Case::new();
    let fake = Fake::new(Mode::Rgb, 2, 2);
    let mut request = serde_json::to_vec(&case.request(Op::Probe {})).unwrap();
    request.resize(MAX_REQUEST_BYTES as usize, b' ');
    assert!(serve_bytes(&fake, &request).done);
    request.push(b' ');
    let run = serve_bytes(&fake, &request);
    assert_internal(&run, "the request is too long");
}

#[test]
fn a_closed_output_ends_with_false() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let case = Case::new();
    let fake = Fake::new(Mode::Rgb, 2, 2);
    let probe = serde_json::to_vec(&case.request(Op::Probe {})).unwrap();
    assert!(!serve(&fake, probe.as_slice(), Closed));
    // The `image` line can't get out, so no pixel file is started.
    let decode = serde_json::to_vec(&case.decode()).unwrap();
    assert!(!serve(&fake, decode.as_slice(), Closed));
    assert!(case.entries().is_empty());
}

#[test]
fn the_exit_code_follows_the_last_line() {
    assert_eq!(exit_code(true), ExitCode::SUCCESS);
    assert_eq!(exit_code(false), ExitCode::FAILURE);
    assert_eq!(ExitCode::FAILURE, ExitCode::from(1));
}
