// Release builds use the Windows GUI subsystem, so no console window opens
// next to the tray icon. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::OsString;
use std::process::ExitCode;

use grape_engines::pdf::{heap, worker_main, WORKER_FLAG};

/// The system allocator, with every change reported to the PDF worker's
/// heap bookkeeping (`grape_engines::pdf::heap`, DECISIONS D29), which ends
/// the worker past its bound. Only the worker arms the bound: in the app
/// itself this costs one atomic load per call. It is the same as in
/// `crates/grape-engines/src/bin/grape-pdf-worker.rs`: change both
/// together.
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

fn main() -> ExitCode {
    // `Grape --grape-pdf-worker` is the PDF worker (DECISIONS D29): one PDF
    // job, its request on stdin and its answers on stdout. It runs before
    // anything of the app starts (no Tauri, no window or tray, no
    // single-instance check, no logger) and exits when the job ends.
    // `args_os`, not `args`: a name that isn't valid Unicode mustn't panic.
    if is_pdf_worker(std::env::args_os()) {
        return worker_main();
    }
    if let Err(err) = grape_app::run() {
        // Release builds have no console, so stderr goes nowhere. The log
        // file catches it too, if the logger was already running.
        log::error!("Grape could not start: {err:#}");
        eprintln!("Grape could not start: {err:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// True when the first argument after the program's own name is
/// [`WORKER_FLAG`], exactly. Anything else, or the flag later on, starts
/// the app.
fn is_pdf_worker(mut args: impl Iterator<Item = OsString>) -> bool {
    args.nth(1).is_some_and(|first| first == WORKER_FLAG)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn the_flag_first_makes_the_pdf_worker() {
        assert_eq!(WORKER_FLAG, "--grape-pdf-worker");
        assert!(is_pdf_worker(args(&["Grape.exe", "--grape-pdf-worker"])));
        assert!(is_pdf_worker(args(&["grape", "--grape-pdf-worker", "x"])));
    }

    #[test]
    fn anything_else_starts_the_app() {
        assert!(!is_pdf_worker(args(&[])));
        assert!(!is_pdf_worker(args(&["Grape.exe"])));
        assert!(!is_pdf_worker(args(&["Grape.exe", "--at-login"])));
        assert!(!is_pdf_worker(args(&[
            "Grape.exe",
            "--at-login",
            "--grape-pdf-worker"
        ])));
        assert!(!is_pdf_worker(args(&["Grape.exe", "--grape-pdf-worker="])));
        assert!(!is_pdf_worker(args(&["Grape.exe", "--GRAPE-PDF-WORKER"])));
        // The program's own name is never the flag.
        assert!(!is_pdf_worker(args(&["--grape-pdf-worker"])));
    }

    #[test]
    fn the_pdf_workers_heap_bound_sees_every_allocation() {
        const MIB: isize = 1 << 20;
        // No real bound: this only checks the allocator reports to it. Other
        // tests allocate meanwhile, so the checks leave a wide margin.
        heap::arm(u64::MAX);
        let before = heap::held();
        let mut block: Vec<u8> = Vec::with_capacity(16);
        block.push(1);
        block.reserve_exact(64 << 20);
        let grown = heap::held();
        assert!(grown - before >= 32 * MIB, "{before} → {grown}");
        block.truncate(1);
        block.shrink_to_fit();
        let shrunk = heap::held();
        assert!(grown - shrunk >= 32 * MIB, "{grown} → {shrunk}");
        let zeroed = vec![0u8; 64 << 20];
        assert!(heap::held() - shrunk >= 32 * MIB);
        drop(zeroed);
        drop(block);
        assert!(heap::held() - shrunk < 32 * MIB);
    }

    #[cfg(unix)]
    #[test]
    fn an_argument_that_isnt_unicode_starts_the_app_without_a_panic() {
        use std::os::unix::ffi::OsStringExt;
        let odd = OsString::from_vec(vec![0x2d, 0x2d, 0xff]);
        let list = vec![OsString::from("grape"), odd];
        assert!(!is_pdf_worker(list.into_iter()));
    }
}
