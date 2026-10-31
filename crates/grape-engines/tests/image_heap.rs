//! The heap a conversion holds at its peak (D16), through `run_job`, under
//! an allocator that counts, against what the model in `image.rs` admits
//! it at. The tests take turns, so nothing else allocates meanwhile.
#![allow(clippy::unwrap_used)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use common::{plan_one, TestDir};
use grape_core::{Format, Job, Operation};
use grape_engines::{run_job, EngineCtx, GrapeError, JobOutcome};
use image::ImageEncoder;

/// The bytes held now, and the most held since the last reset.
static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(bytes: usize) {
    let now = HELD.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn shrink(bytes: usize) {
    HELD.fetch_sub(bytes, Ordering::Relaxed);
}

/// The system allocator, counting what it holds.
struct Counting;

// SAFETY: every method hands its arguments to `System` unchanged and
// returns what `System` returns, so every block is `System`'s, under
// `System`'s guarantees. The counting around it doesn't allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract for `layout`.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() {
            grow(layout.size());
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc_zeroed`'s contract for `layout`.
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() {
            grow(layout.size());
        }
        block
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `dealloc`'s contract: `ptr` came from
        // this allocator, so from `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        shrink(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `realloc`'s contract: `ptr` came from
        // `System` with `layout`, and `new_size` is valid for it.
        let block = unsafe { System.realloc(ptr, layout, new_size) };
        if !block.is_null() {
            // Counted as both while it moves.
            grow(new_size);
            shrink(layout.size());
        }
        block
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Each test's turn, held for all of it: what it builds counts too.
fn turn() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// The most heap `job` held over what was held before it ran, and how it
/// ended.
fn peak_of(job: &Job) -> (usize, Result<JobOutcome, GrapeError>) {
    let before = HELD.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let result = run_job(job, &EngineCtx::default(), &mut |_| {});
    (PEAK.load(Ordering::Relaxed) - before, result)
}

/// A RIFF chunk, padded to even.
fn chunk(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let size = u32::try_from(data.len()).unwrap().to_le_bytes();
    let pad: &[u8] = if data.len() % 2 == 1 { &[0] } else { &[] };
    [fourcc.as_slice(), &size, data, pad].concat()
}

fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    let body = chunks.concat();
    let size = u32::try_from(body.len() + 4).unwrap().to_le_bytes();
    [b"RIFF".as_slice(), &size, b"WEBP", &body].concat()
}

/// A bare little-endian TIFF of one entry, an EXIF orientation.
fn orientation(o: u16) -> Vec<u8> {
    let o = o.to_le_bytes();
    [
        b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".as_slice(),
        &[o[0], o[1], 0, 0],
        &[0; 4],
    ]
    .concat()
}

#[test]
fn an_opaque_webp_whose_vp8l_says_no_alpha_is_turned_as_rgb() {
    let _turn = turn();
    let dir = TestDir::new("heap-webp-turn");
    // An opaque lossless 1024 × 512 picture whose VP8L bit says no alpha,
    // under no VP8X alpha flag (only EXIF's): decoded with its alpha to
    // see, 4 bytes a pixel, and handed on as RGB, 3, it peaks at the 7 the
    // model admits it at for BMP. Turned by its EXIF orientation as RGBA,
    // two copies of it, it held 8; turned as RGB, 7, as upright.
    let (w, h) = (1024u32, 512u32);
    let pixels = (w * h) as usize;
    let picture = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 256) as u8, 255])
    });
    let mut file = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut file)
        .write_image(picture.as_raw(), w, h, image::ExtendedColorType::Rgba8)
        .unwrap();
    let mut vp8l = file[12..].to_vec();
    assert_eq!(&vp8l[..4], b"VP8L");
    vp8l[12] &= !0x10;
    let (cw, ch) = ((w - 1).to_le_bytes(), (h - 1).to_le_bytes());
    let vp8x = chunk(
        b"VP8X",
        &[0x08, 0, 0, 0, cw[0], cw[1], cw[2], ch[0], ch[1], ch[2]],
    );
    for (o, size) in [(1, (w, h)), (6, (h, w)), (8, (h, w))] {
        let src = riff(&[vp8x.clone(), vp8l.clone(), chunk(b"EXIF", &orientation(o))]);
        let name = format!("o{o}.webp");
        let (_, job) = plan_one(&dir, &name, &src, Operation::convert(Format::Bmp));
        drop(src);
        let (peak, result) = peak_of(&job);
        let outcome = result.unwrap();
        let out = image::open(&outcome.outputs[0]).unwrap();
        assert_eq!((out.width(), out.height()), size, "{o}");
        assert_eq!(out.color(), image::ColorType::Rgb8, "{o}");
        assert!(
            peak <= 7 * pixels + pixels / 16,
            "orientation {o}: {peak} bytes, {:.2} a pixel",
            peak as f64 / pixels as f64
        );
        fs::remove_file(&outcome.outputs[0]).unwrap();
    }
}
