//! The heap Remove Metadata on TIFF holds at its peak (D28): TIFFs of a
//! million strips, and of pages that share Photoshop's resources, through
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

/// A classic TIFF of one gray page, 1 × `n`, one 1-byte row a strip: `n`
/// strips, their offsets and counts LONG arrays; the strips in the file
/// last first if `reversed`.
fn strips(n: u32, reversed: bool) -> Vec<u8> {
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
    let at = |i: u32| if reversed { data + n - 1 - i } else { data + i };
    file.extend((0..n).flat_map(|i| at(i).to_le_bytes()));
    file.extend((0..n).flat_map(|_| 1u32.to_le_bytes()));
    file.extend((0..n).map(|i| (i % 251) as u8));
    file
}

#[test]
fn remove_metadata_on_a_tiff_of_a_million_strips_holds_little_past_two_models() {
    let _turn = turn();
    let n = 1 << 20;
    let dir = TestDir::new("tiff-heap");
    // Strips in the file's order, and last first, which the overlap check
    // sorts (as references).
    for (name, reversed) in [("scan.tif", false), ("reversed.tif", true)] {
        let (_, job) = plan_one(
            &dir,
            name,
            &strips(n, reversed),
            Operation::tool(ToolKey::RemoveMetadata),
        );
        let peak = peak(&job);
        // The model holds 16 bytes a strip. The self-check holds the
        // plan's and the output's, and little else.
        let model = 16 * n as usize;
        let models = peak as f64 / model as f64;
        assert!(peak < 2 * model + model / 2, "{name}: {models:.2} models");
    }
}

/// A classic TIFF of `pages` gray 8 × 8 pages, a 64-byte strip each, whose
/// Photoshop resources (34377) are one block they all point at: a path
/// (2000) of `path` bytes.
fn one_irb(pages: u32, path: u32) -> Vec<u8> {
    let ifd = 2 + 12 * 8 + 4;
    let strips = 8 + pages * ifd;
    let irb = strips + 64 * pages;
    let mut file = b"II*\0\x08\0\0\0".to_vec();
    for n in 0..pages {
        let entries: [(u16, u16, u32, u32); 8] = [
            (256, 4, 1, 8),
            (257, 4, 1, 8),
            (258, 3, 1, 8),
            (262, 3, 1, 1),
            (273, 4, 1, strips + 64 * n),
            (278, 4, 1, 8),
            (279, 4, 1, 64),
            (34377, 7, 12 + path, irb),
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
    file.extend(b"8BIM\x07\xD0\0\0");
    file.extend(path.to_be_bytes());
    file.extend((0..path).map(|i| (i % 251) as u8));
    file
}

#[test]
fn pages_that_share_photoshops_resources_hold_them_once() {
    // 32 pages, one 1 MiB path: the plan holds it once, and the self-check
    // reads it once more.
    let _turn = turn();
    let path = 1 << 20;
    let dir = TestDir::new("tiff-heap-irb");
    let (_, job) = plan_one(
        &dir,
        "paths.tif",
        &one_irb(32, path),
        Operation::tool(ToolKey::RemoveMetadata),
    );
    let peak = peak(&job);
    let paths = peak as f64 / f64::from(path);
    assert!(peak < 4 * path as usize, "{paths:.2} paths");
}
