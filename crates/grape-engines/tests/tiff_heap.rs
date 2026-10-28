//! The heap Remove Metadata on TIFF holds at its peak (D28): TIFFs of a
//! million strips, and of pages with Photoshop's resources, through
//! `run_job`, under an allocator that counts. The tests take turns, so
//! nothing else allocates meanwhile.
#![allow(clippy::unwrap_used)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use common::{plan_one, TestDir};
use grape_core::{Job, Operation, ToolKey};
use grape_engines::{run_job, EngineCtx};

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

/// The most heap `job` held over what was held before it ran.
fn peak(job: &Job) -> usize {
    let before = HELD.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    run_job(job, &EngineCtx::default(), &mut |_| {}).unwrap();
    PEAK.load(Ordering::Relaxed) - before
}

/// Which byte strip `i` of `n` is.
type At = fn(u32, u32) -> u32;

/// A classic TIFF of one gray page, 1 × `n`, one 1-byte row a strip: `n`
/// strips, their offsets and counts LONG arrays, then `n` bytes, of which
/// strip `i` is byte `at(i, n)`.
fn strips(n: u32, at: At) -> Vec<u8> {
    let entries: [(u16, u16, u32, u32); 7] = [
        (256, 4, 1, 1),
        (257, 4, 1, n),
        (258, 3, 1, 8),
        (262, 3, 1, 1),
        (273, 4, n, 0),
        (278, 4, 1, 1),
        (279, 4, n, 0),
    ];
    let offsets = 8 + 2 + 12 * 7 + 4;
    let counts = offsets + 4 * n;
    let data = counts + 4 * n;
    let mut file = b"II*\0\x08\0\0\0\x07\0".to_vec();
    for (tag, kind, count, value) in entries {
        let value = match tag {
            273 => offsets,
            279 => counts,
            _ => value,
        };
        file.extend(tag.to_le_bytes());
        file.extend(kind.to_le_bytes());
        file.extend(count.to_le_bytes());
        file.extend(value.to_le_bytes());
    }
    file.extend([0; 4]);
    file.extend((0..n).flat_map(|i| (data + at(i, n)).to_le_bytes()));
    file.extend((0..n).flat_map(|_| 1u32.to_le_bytes()));
    file.extend((0..n).map(|i| (i % 251) as u8));
    file
}

#[test]
fn remove_metadata_on_a_tiff_of_a_million_strips_holds_two_models_and_a_half_at_most() {
    let _turn = turn();
    // Just past a power of two, where a list grown by doubling would
    // hold twice what it needs.
    let n = (1 << 20) + 2;
    let dir = TestDir::new("tiff-heap");
    // The model holds 16 bytes a strip. The self-check holds the plan's
    // and the output's, and little else; when the output's strips are out
    // of the file's order, the overlap check's sorted references to them
    // too (half a model).
    let cases: [(&str, At, f64); 6] = [
        ("scan.tif", |i, _| i, 2.25),
        // Last first: the source's strips are sorted, the output's aren't.
        ("reversed.tif", |i, n| n - 1 - i, 2.25),
        // Every strip one byte, written once.
        ("one.tif", |_, _| 0, 2.25),
        // Each byte two strips in a row: half a million repeats.
        ("pairs.tif", |i, _| i / 2, 2.75),
        // Two bytes in turn: the output's strips are out of order too.
        ("turns.tif", |i, _| i % 2, 2.75),
        // Each byte two strips, half the file apart: half a million
        // repeats, and the output's strips out of order.
        ("twice.tif", |i, n| i % (n / 2), 2.75),
    ];
    for (name, at, most) in cases {
        let (_, job) = plan_one(
            &dir,
            name,
            &strips(n, at),
            Operation::tool(ToolKey::RemoveMetadata),
        );
        let models = peak(&job) as f64 / (16 * n) as f64;
        assert!(models < most, "{name}: {models:.2} models");
    }
}

/// Photoshop's resources (34377): one path (2000) of `size` bytes.
fn path(size: u32) -> Vec<u8> {
    let mut block = b"8BIM\x07\xD0\0\0".to_vec();
    block.extend(size.to_be_bytes());
    block.extend((0..size).map(|i| (i % 251) as u8));
    block
}

/// A classic TIFF of `pages` gray 8 × 8 pages, a 64-byte strip each, then
/// the bytes `irb`: page `n`'s Photoshop resources (34377) are the bytes
/// `at(n)` gives, as an offset into `irb` and a length.
fn photoshop(pages: u32, irb: &[u8], at: impl Fn(u32) -> (u32, u32)) -> Vec<u8> {
    let ifd = 2 + 12 * 8 + 4;
    let strips = 8 + pages * ifd;
    let base = strips + 64 * pages;
    let mut file = b"II*\0\x08\0\0\0".to_vec();
    for n in 0..pages {
        let (offset, len) = at(n);
        let entries: [(u16, u16, u32, u32); 8] = [
            (256, 4, 1, 8),
            (257, 4, 1, 8),
            (258, 3, 1, 8),
            (262, 3, 1, 1),
            (273, 4, 1, strips + 64 * n),
            (278, 4, 1, 8),
            (279, 4, 1, 64),
            (34377, 7, len, base + offset),
        ];
        file.extend(8u16.to_le_bytes());
        for (tag, kind, count, value) in entries {
            file.extend(tag.to_le_bytes());
            file.extend(kind.to_le_bytes());
            file.extend(count.to_le_bytes());
            file.extend(value.to_le_bytes());
        }
        let next = if n + 1 < pages { 8 + (n + 1) * ifd } else { 0 };
        file.extend(next.to_le_bytes());
    }
    file.extend((0..64 * pages).map(|i| (i % 251) as u8));
    file.extend_from_slice(irb);
    file
}

#[test]
fn pages_that_share_photoshops_resources_hold_them_once() {
    // 32 pages, one 1 MiB path: the plan holds it once, and the self-check
    // reads it once more.
    let _turn = turn();
    let size = 1 << 20;
    let dir = TestDir::new("tiff-heap-irb");
    let (_, job) = plan_one(
        &dir,
        "paths.tif",
        &photoshop(32, &path(size), |_| (0, 12 + size)),
        Operation::tool(ToolKey::RemoveMetadata),
    );
    let peak = peak(&job);
    let paths = peak as f64 / f64::from(size);
    assert!(peak < 4 * size as usize, "{paths:.2} paths");
}

#[test]
fn the_self_check_holds_no_photoshop_resources_of_its_own() {
    // Four pages, each with its own 3 MiB block: the plan holds the four,
    // and the self-check reads and filters one at a time beside them.
    let _turn = turn();
    let (pages, size) = (4, 3 << 20);
    let irb: Vec<u8> = (0..pages).flat_map(|_| path(size - 12)).collect();
    let dir = TestDir::new("tiff-heap-irbs");
    let (_, job) = plan_one(
        &dir,
        "pages.tif",
        &photoshop(pages, &irb, |n| (n * size, size)),
        Operation::tool(ToolKey::RemoveMetadata),
    );
    let blocks = peak(&job) as f64 / f64::from(size);
    assert!(blocks < 6.5, "{blocks:.2} blocks");
}
