//! Grape's PDF worker as an executable of its own, for the `pdf-it` tests
//! (DECISIONS D29). The app runs the same entry point as
//! `Grape --grape-pdf-worker`; this binary needs no argument.
//!
//! Like the app, it installs [`WorkerHeap`] as its global allocator, so the
//! worker's heap bound (`grape_engines::pdf::heap`) holds here too. That is
//! the only `unsafe` code, and it is the same as in the app's `main.rs`:
//! change both together.

use std::alloc::{GlobalAlloc, Layout, System};

use grape_engines::pdf::heap;

/// The system allocator, with every change reported to the PDF worker's
/// heap bookkeeping, which ends the worker past its bound. Until the
/// worker arms the bound, that costs one atomic load per call.
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

fn main() -> std::process::ExitCode {
    grape_engines::pdf::worker_main()
}
