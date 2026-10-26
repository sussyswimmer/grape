//! The PDF worker's heap bound (DECISIONS D29).
//!
//! lopdf can be made to hold far more than a file's size: a few hundred
//! bytes of object stream whose entries all point at one huge array parse
//! that array once per entry, and parsed objects take about 30 to 60 times
//! the bytes they came from. No decompression bound stops that, so the
//! worker bounds its own heap instead.
//!
//! The worker's executables (the app's `main.rs`, and `grape-pdf-worker`
//! for the tests) install a global allocator that wraps the system one and
//! calls [`reserve`] before and [`release`] after every change. This module
//! is only the bookkeeping, so `grape-engines` stays free of `unsafe`.
//! Nothing is counted until [`arm`] runs, which only the worker does, once
//! it has its request: in the app's own process the allocator just passes
//! everything through. Past the bound the worker exits at once with
//! [`OVER_HEAP_EXIT`], which the parent reports as "needs too much memory".
//!
//! Only Rust's heap is counted (lopdf, Grape's buffers and bitmap copies).
//! PDFium allocates with its own `malloc`, which this can't see.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

/// The exit code of a worker that went past its heap bound. Nothing else
/// in the worker exits with it.
pub const OVER_HEAP_EXIT: i32 = 86;

/// The bookkeeping: off until armed.
pub(crate) struct HeapCap {
    /// The bound in bytes; 0 while off.
    limit: AtomicIsize,
    /// Bytes allocated since arming, less those freed. Memory allocated
    /// before arming and freed after it makes this go below zero, which
    /// only loosens the bound by that much.
    held: AtomicIsize,
    /// The bound was passed and the process is exiting: let everything
    /// through so the exit can finish.
    tripped: AtomicBool,
}

impl HeapCap {
    /// Off.
    pub(crate) const fn new() -> HeapCap {
        HeapCap {
            limit: AtomicIsize::new(0),
            held: AtomicIsize::new(0),
            tripped: AtomicBool::new(false),
        }
    }

    /// Starts counting from zero with a bound of `limit` bytes (at least 1).
    pub(crate) fn arm(&self, limit: u64) {
        let limit = isize::try_from(limit).unwrap_or(isize::MAX).max(1);
        self.held.store(0, Ordering::Relaxed);
        self.limit.store(limit, Ordering::Release);
    }

    /// Counts `bytes` more, unless that passes the bound: then nothing is
    /// counted and the answer is false. Always true while off or tripped.
    pub(crate) fn try_reserve(&self, bytes: usize) -> bool {
        let limit = self.limit.load(Ordering::Acquire);
        if limit == 0 || self.tripped.load(Ordering::Relaxed) {
            return true;
        }
        let Ok(bytes) = isize::try_from(bytes) else {
            return false;
        };
        self.held
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                held.checked_add(bytes).filter(|&next| next <= limit)
            })
            .is_ok()
    }

    /// Counts `bytes` as freed (nothing while off).
    pub(crate) fn release(&self, bytes: usize) {
        if self.limit.load(Ordering::Acquire) == 0 {
            return;
        }
        let bytes = isize::try_from(bytes).unwrap_or(isize::MAX);
        self.held.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// The bytes counted now (0 while off).
    pub(crate) fn held(&self) -> isize {
        self.held.load(Ordering::Relaxed)
    }

    /// Marks the bound as passed: from now on everything goes through.
    fn trip(&self) {
        self.tripped.store(true, Ordering::Relaxed);
    }
}

/// The process's bookkeeping.
static HEAP: HeapCap = HeapCap::new();

/// Arms the process's heap bound at `limit` bytes, counting from zero: the
/// worker does this once it has read its request (and the app's test that
/// its allocator is installed does it with no real bound).
pub fn arm(limit: u64) {
    HEAP.arm(limit);
}

/// For the worker executables' global allocator, before it allocates or
/// grows a block by `bytes`: counts them, or, past the bound, ends the
/// process with [`OVER_HEAP_EXIT`] and never returns. It doesn't allocate
/// (the exit may: those allocations go through).
pub fn reserve(bytes: usize) {
    if !HEAP.try_reserve(bytes) {
        HEAP.trip();
        std::process::exit(OVER_HEAP_EXIT);
    }
}

/// For the worker executables' global allocator, after it frees or shrinks
/// a block by `bytes`, or when an allocation [`reserve`]d for failed.
pub fn release(bytes: usize) {
    HEAP.release(bytes);
}

/// The bytes the process's bound counts now (0 until armed).
pub fn held() -> isize {
    HEAP.held()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_counted_until_armed() {
        let cap = HeapCap::new();
        assert!(cap.try_reserve(usize::MAX));
        cap.release(1 << 20);
        assert_eq!(cap.held(), 0);
    }

    #[test]
    fn the_bound_holds_and_frees_make_room() {
        let cap = HeapCap::new();
        cap.arm(100);
        assert!(cap.try_reserve(60));
        assert!(cap.try_reserve(40));
        assert_eq!(cap.held(), 100);
        // One byte more is refused and not counted.
        assert!(!cap.try_reserve(1));
        assert_eq!(cap.held(), 100);
        cap.release(50);
        assert!(cap.try_reserve(50));
        assert!(!cap.try_reserve(usize::MAX));
        assert_eq!(cap.held(), 100);
    }

    #[test]
    fn frees_of_older_blocks_only_loosen_the_bound() {
        let cap = HeapCap::new();
        cap.arm(10);
        // Allocated before arming, freed after.
        cap.release(5);
        assert_eq!(cap.held(), -5);
        assert!(cap.try_reserve(15));
        assert!(!cap.try_reserve(1));
    }

    #[test]
    fn a_tripped_bound_lets_everything_through() {
        let cap = HeapCap::new();
        cap.arm(10);
        assert!(!cap.try_reserve(11));
        cap.trip();
        assert!(cap.try_reserve(1 << 30));
    }

    #[test]
    fn arming_clamps_the_bound() {
        let cap = HeapCap::new();
        cap.arm(0);
        assert!(!cap.try_reserve(2));
        assert!(cap.try_reserve(1));
        let huge = HeapCap::new();
        huge.arm(u64::MAX);
        assert!(huge.try_reserve(1 << 40));
    }
}
