//! Grape's SVG worker as an executable of its own, for the engine's tests
//! (DECISIONS D39): `tests/svg.rs` starts it as
//! `grape-svg-worker --grape-svg-worker`, as the app starts
//! `Grape --grape-svg-worker`. Both run `grape_engines::svg::worker_main`.
//!
//! Like the app, it installs [`WorkerHeap`] as its global allocator, so the
//! worker's heap bound (`grape_engines::pdf::heap`) holds here too. That is
//! the only `unsafe` code, and it is the same as in the app's `main.rs` and
//! in `src/bin/grape-pdf-worker.rs`: change all three together.

use std::alloc::{GlobalAlloc, Layout, System};

use grape_engines::pdf::heap;

/// The system allocator, with every change reported to the workers' heap
/// bookkeeping, which ends the worker past its bound. Until the worker
/// arms the bound, that costs one atomic load per call.
struct WorkerHeap;

// SAFETY: every method hands its arguments to `System` unchanged and
// returns what `System` returns, so every block is `System`'s, under
// `System`'s guarantees. The bookkeeping calls around it don't allocate,
// and `heap::reserve` ends the process instead of returning (it never
// unwinds).
unsafe impl GlobalAlloc for WorkerHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        heap::reserve(layout.size());
        // SAFETY: the caller upholds `alloc`'s contract for `layout`.
        let block = unsafe { System.alloc(layout) };
        if block.is_null() {
            heap::release(layout.size());
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        heap::reserve(layout.size());
        // SAFETY: the caller upholds `alloc_zeroed`'s contract for `layout`.
        let block = unsafe { System.alloc_zeroed(layout) };
        if block.is_null() {
            heap::release(layout.size());
        }
        block
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `dealloc`'s contract: `ptr` came from
        // this allocator, so from `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        heap::release(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old_size = layout.size();
        if new_size > old_size {
            heap::reserve(new_size - old_size);
        }
        // SAFETY: the caller upholds `realloc`'s contract: `ptr` came from
        // `System` with `layout`, and `new_size` is valid for it.
        let block = unsafe { System.realloc(ptr, layout, new_size) };
        if block.is_null() {
            // The old block is still there, at its old size.
            if new_size > old_size {
                heap::release(new_size - old_size);
            }
        } else if new_size < old_size {
            heap::release(old_size - new_size);
        }
        block
    }
}

#[global_allocator]
static ALLOCATOR: WorkerHeap = WorkerHeap;

/// Started as `grape-svg-worker --grape-svg-worker`, the app's flag, so
/// the tests launch it the way the engine launches Grape. Anything else is
/// bad usage (exit code 2), as for `Grape` with an unknown worker flag.
fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next()) {
        (Some(flag), None) if flag == grape_engines::svg::WORKER_FLAG => {
            grape_engines::svg::worker_main()
        }
        _ => std::process::ExitCode::from(2),
    }
}
