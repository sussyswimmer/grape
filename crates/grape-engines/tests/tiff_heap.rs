//! The heap Remove Metadata and Compress on TIFF hold at their peak (D28):
//! TIFFs of a million strips, of 65,536 pages, and of pages with
//! Photoshop's resources, through `run_job`, under an allocator that
//! counts. The tests take turns, so nothing else allocates meanwhile.
#![allow(clippy::unwrap_used)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use common::{plan_one, TestDir};
use grape_core::{CompressPreset, Job, Operation, ToolKey, ToolParams};
use grape_engines::{run_job, EngineCtx, ErrorKind, GrapeError, JobOutcome};

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

/// The most heap `job` held over what was held before it ran.
fn peak(job: &Job) -> usize {
    let (peak, result) = peak_of(job);
    result.unwrap();
    peak
}

/// Which row strip `i` of `n` is.
type At = fn(u32, u32) -> u32;

/// A classic TIFF of one gray page, 1 × `n`, one 1-byte row a strip: `n`
/// strips, their offsets and counts LONG arrays, then `n` bytes, of which
/// strip `i` is byte `at(i, n)`.
fn strips(n: u32, at: At) -> Vec<u8> {
    rows(n, 1, at)
}

/// A classic TIFF of one gray page, `width` × `n`, one row a strip: `n`
/// strips, their offsets and counts LONG arrays, then `n` rows, each of one
/// value, of which strip `i` is row `at(i, n)`.
fn rows(n: u32, width: u32, at: At) -> Vec<u8> {
    let entries: [(u16, u16, u32, u32); 7] = [
        (256, 4, 1, width),
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
    file.extend((0..n).flat_map(|i| (data + at(i, n) * width).to_le_bytes()));
    file.extend((0..n).flat_map(|_| width.to_le_bytes()));
    let row = |i: u32| std::iter::repeat_n((i % 251) as u8, width as usize);
    file.extend((0..n).flat_map(row));
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

/// A classic TIFF of `ifds` gray 8 × 8 IFDs, a 64-byte strip each, then
/// the bytes `irb`: IFD `n`'s Photoshop resources (34377) are the bytes
/// `at(n)` gives, as an offset into `irb` and a length. The IFDs are a
/// chain of pages, or with `subs`, a page and its SubIFDs (330).
fn photoshop(ifds: u32, irb: &[u8], subs: bool, at: impl Fn(u32) -> (u32, u32)) -> Vec<u8> {
    let ifd = 2 + 12 * 8 + 4;
    // The page's SubIFDs entry, and where each IFD is.
    let extra = if subs { 12 } else { 0 };
    let place = |n: u32| if n == 0 { 8 } else { 8 + n * ifd + extra };
    let array = place(ifds);
    let strips = array + if subs { 4 * (ifds - 1) } else { 0 };
    let base = strips + 64 * ifds;
    let mut file = b"II*\0\x08\0\0\0".to_vec();
    for n in 0..ifds {
        let (offset, len) = at(n);
        let mut entries = vec![
            (256, 4, 1, 8),
            (257, 4, 1, 8),
            (258, 3, 1, 8),
            (262, 3, 1, 1),
            (273, 4, 1, strips + 64 * n),
            (278, 4, 1, 8),
            (279, 4, 1, 64),
        ];
        if subs && n == 0 {
            let heads = if ifds == 2 { place(1) } else { array };
            entries.push((330, 4, ifds - 1, heads));
        }
        entries.push((34377, 7, len, base + offset));
        file.extend((entries.len() as u16).to_le_bytes());
        for (tag, kind, count, value) in entries {
            file.extend(u16::to_le_bytes(tag));
            file.extend(u16::to_le_bytes(kind));
            file.extend(u32::to_le_bytes(count));
            file.extend(u32::to_le_bytes(value));
        }
        let next = if !subs && n + 1 < ifds {
            place(n + 1)
        } else {
            0
        };
        file.extend(next.to_le_bytes());
    }
    if subs && ifds > 2 {
        file.extend((1..ifds).flat_map(|n| place(n).to_le_bytes()));
    }
    file.extend((0..64 * ifds).map(|i| (i % 251) as u8));
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
        &photoshop(32, &path(size), false, |_| (0, 12 + size)),
        Operation::tool(ToolKey::RemoveMetadata),
    );
    let peak = peak(&job);
    let paths = peak as f64 / f64::from(size);
    assert!(peak < 4 * size as usize, "{paths:.2} paths");
}

#[test]
fn the_self_check_holds_no_photoshop_resources_of_its_own() {
    // Four IFDs, each with its own 3 MiB block: the plan holds the four,
    // and the self-check reads and filters one at a time beside them,
    // whether they're pages or a page and its SubIFDs, which the
    // self-check walks with their page.
    let _turn = turn();
    let (ifds, size) = (4, 3 << 20);
    let irb: Vec<u8> = (0..ifds).flat_map(|_| path(size - 12)).collect();
    let dir = TestDir::new("tiff-heap-irbs");
    for subs in [false, true] {
        let (_, job) = plan_one(
            &dir,
            &format!("ifds-{subs}.tif"),
            &photoshop(ifds, &irb, subs, |n| (n * size, size)),
            Operation::tool(ToolKey::RemoveMetadata),
        );
        let blocks = peak(&job) as f64 / f64::from(size);
        assert!(blocks < 6.5, "{subs}: {blocks:.2} blocks");
    }
}

#[test]
fn pages_that_name_photoshops_resources_each_their_own_way_hold_16_mib_at_most() {
    // 40 pages that name one block of a 1 MiB path, each 2 bytes more of
    // it (the padding after it): each is read and filtered on its own, so
    // the second already reads more than the file holds, and the file is
    // too large.
    let _turn = turn();
    let (pages, size) = (40, 1 << 20);
    let irb = [path(size), vec![0; 2 * pages as usize]].concat();
    let dir = TestDir::new("tiff-heap-irb-lens");
    let (_, job) = plan_one(
        &dir,
        "lengths.tif",
        &photoshop(pages, &irb, false, |n| (0, 12 + size + 2 * n)),
        Operation::tool(ToolKey::RemoveMetadata),
    );
    let (peak, result) = peak_of(&job);
    let err = result.unwrap_err();
    assert!(
        err.user_message.ends_with("is too large to convert."),
        "{err:?}"
    );
    let mib = peak as f64 / f64::from(1 << 20);
    assert!(mib < 18.0, "{mib:.2} MiB");
}

/// Compress at Balanced.
fn compress() -> Operation {
    Operation::Tool {
        tool: ToolKey::Compress,
        params: ToolParams {
            compress: CompressPreset::Balanced,
            ..ToolParams::DEFAULT
        },
    }
}

#[test]
fn compress_on_a_tiff_of_many_strips_holds_two_models_and_40_bytes_a_repeat() {
    let _turn = turn();
    // Fewer strips than Remove Metadata's test, as a debug build deflates
    // each one in about 140 µs: the heap that two strips take (the codecs'
    // state and buffers, about 1.1 MB) is taken off. Up to 4,096 blocks
    // waiting to be read back stay in the count (0.1 model here; a million
    // strips measured 2.09, 2.09 and 3.34 models).
    let n = (1 << 17) + 2;
    let dir = TestDir::new("tiff-heap-compress");
    let (_, job) = plan_one(&dir, "two.tif", &rows(2, 32, |i, _| i), compress());
    let fixed = peak(&job);
    // The plan holds the model (16 bytes a strip) and the writer where each
    // strip went and its size (16 more); a block listed twice, its place
    // (24 bytes) beside the list of them (16); the source's strips out of
    // order, sorted references to them (8), but not at once.
    let cases: [(&str, At, f64); 3] = [
        ("scan.tif", |i, _| i, 2.5),
        ("reversed.tif", |i, n| n - 1 - i, 2.5),
        // Each row two strips in a row: half of them listed twice.
        ("pairs.tif", |i, _| i / 2, 3.75),
    ];
    for (name, at, most) in cases {
        let (_, job) = plan_one(&dir, name, &rows(n, 32, at), compress());
        let models = peak(&job).saturating_sub(fixed) as f64 / (16 * n) as f64;
        assert!(models < most, "{name}: {models:.2} models");
    }
}

/// A classic TIFF of `n` gray 32 × 8 pages, a chain, one 256-byte strip
/// each, of one value.
fn pages(n: u32) -> Vec<u8> {
    let ifd = 2 + 12 * 7 + 4;
    let strips = 8 + n * ifd;
    let mut file = b"II*\0\x08\0\0\0".to_vec();
    for k in 0..n {
        let entries: [(u16, u16, u32, u32); 7] = [
            (256, 4, 1, 32),
            (257, 4, 1, 8),
            (258, 3, 1, 8),
            (262, 3, 1, 1),
            (273, 4, 1, strips + 256 * k),
            (278, 4, 1, 8),
            (279, 4, 1, 256),
        ];
        file.extend(7u16.to_le_bytes());
        for (tag, kind, count, value) in entries {
            file.extend(tag.to_le_bytes());
            file.extend(kind.to_le_bytes());
            file.extend(count.to_le_bytes());
            file.extend(value.to_le_bytes());
        }
        let next = if k + 1 < n { 8 + (k + 1) * ifd } else { 0 };
        file.extend(next.to_le_bytes());
    }
    file.extend((0..n).flat_map(|k| std::iter::repeat_n((k % 251) as u8, 256)));
    file
}

#[test]
fn compress_on_a_tiff_of_65536_pages_holds_112_mib_at_most() {
    // About 1.7 KB a page: its entries, fields and plan, and the writer's
    // (1.9 KB, 119.6 MiB, while the writer grew each IFD's entries by
    // doubling and looked up every value out of line in a map).
    let _turn = turn();
    let dir = TestDir::new("tiff-heap-compress-pages");
    let (_, job) = plan_one(&dir, "pages.tif", &pages(65_536), compress());
    let mib = peak(&job) as f64 / f64::from(1 << 20);
    assert!(mib < 112.0, "{mib:.2} MiB");
}

/// A classic TIFF of `n` gray 8 × 8 pages, a chain, one 64-byte strip
/// each, and `extra` RATIONAL tags each (from 1000 on), every one out of
/// line after its IFD.
fn rationals(n: u32, extra: u32) -> Vec<u8> {
    let ifd = 2 + 12 * (7 + extra) + 4;
    let page = ifd + 8 * extra + 64;
    let mut file = b"II*\0\x08\0\0\0".to_vec();
    for k in 0..n {
        let at = 8 + k * page;
        let values = at + ifd;
        let strip = values + 8 * extra;
        let mut entries: Vec<(u16, u16, u32, u32)> = vec![
            (256, 4, 1, 8),
            (257, 4, 1, 8),
            (258, 3, 1, 8),
            (262, 3, 1, 1),
            (273, 4, 1, strip),
            (278, 4, 1, 8),
            (279, 4, 1, 64),
        ];
        let tag = |i: u32| u16::try_from(1_000 + i).unwrap();
        entries.extend((0..extra).map(|i| (tag(i), 5, 1, values + 8 * i)));
        file.extend(u16::try_from(entries.len()).unwrap().to_le_bytes());
        for (tag, kind, count, value) in entries {
            file.extend(tag.to_le_bytes());
            file.extend(kind.to_le_bytes());
            file.extend(count.to_le_bytes());
            file.extend(value.to_le_bytes());
        }
        let next = if k + 1 < n { at + page } else { 0 };
        file.extend(next.to_le_bytes());
        for i in 0..extra {
            file.extend((i + 1).to_le_bytes());
            file.extend(7u32.to_le_bytes());
        }
        file.extend((0..64).map(|i| (i * 3 + k) as u8));
    }
    file
}

#[test]
fn compress_on_a_tiff_of_a_million_entries_holds_about_100_bytes_an_entry() {
    // 256 pages of 4,096 entries, the cap, 1,046,784 of them RATIONALs
    // out of line, which Compress copies as they are, so it ends NoChange:
    // each entry is held as a field to write, the writer's entry and where
    // its value goes. Looking up every value out of line in a map, and
    // growing each IFD's entries by doubling, it held 234 MiB.
    let _turn = turn();
    let dir = TestDir::new("tiff-heap-entries");
    let (_, job) = plan_one(&dir, "entries.tif", &rationals(256, 4_089), compress());
    let (peak, result) = peak_of(&job);
    let err = result.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoChange, "{}", err.detail);
    let mib = peak as f64 / f64::from(1 << 20);
    assert!(mib < 112.0, "{mib:.2} MiB");
}
