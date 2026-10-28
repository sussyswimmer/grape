// Release builds use the Windows GUI subsystem, so no console window opens
// next to the tray icon. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::OsString;
use std::process::ExitCode;

use grape_engines::pdf::{heap, worker_main, WORKER_FLAG as PDF_WORKER_FLAG};
use grape_heif_proto::WORKER_FLAG as HEIF_WORKER_FLAG;

/// The system allocator, with every change reported to the workers' heap
/// bookkeeping (`grape_engines::pdf::heap`, DECISIONS D29), which ends a
/// worker past its bound. Only the workers arm the bound, the PDF worker
/// (D29) and the Mac image helper worker (D41): in the app itself this
/// costs one atomic load per call. It is the same as in
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
    if first_arg_is(std::env::args_os(), PDF_WORKER_FLAG) {
        return worker_main();
    }
    // `Grape --grape-heif-worker` is the image helper worker (D33, D41),
    // the same way: one HEIC or AVIF job through ImageIO (and rav1e for
    // AVIF output) on macOS, under a 2 GiB heap bound.
    if first_arg_is(std::env::args_os(), HEIF_WORKER_FLAG) {
        return heif_worker();
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

/// True when the first argument after the program's own name is `flag`,
/// exactly: [`PDF_WORKER_FLAG`] or [`HEIF_WORKER_FLAG`]. Anything else, or
/// the flag later on, starts the app.
fn first_arg_is(mut args: impl Iterator<Item = OsString>, flag: &str) -> bool {
    args.nth(1).is_some_and(|first| first == flag)
}

/// The image helper worker: ImageIO (and rav1e, `grape_platform::avif`,
/// for AVIF output) behind the image helper's protocol on macOS
/// (`grape_platform::macos::heif`), with the exit code it ends with.
#[cfg(target_os = "macos")]
fn heif_worker() -> ExitCode {
    use grape_platform::macos::heif::ImageIo;

    run_heif_worker(ImageIo::restrict_process, || {
        grape_heif_proto::worker_main(&ImageIo)
    })
}

/// The image helper worker's steps, in order, on every OS so that a test
/// runs the same path [`heif_worker`] does: `restrict` (on macOS ImageIO's
/// allowed types, before ImageIO does anything else and while the worker
/// has one thread), then the worker's memory cap, then `serve` (the
/// protocol's one job), whose exit code it returns.
///
/// The cap (D41) is the heap bound the PDF worker has, at
/// `grape_heif_proto::WORKER_HEAP_BYTES` (2 GiB), for every op. rav1e's
/// encoder and Grape's buffers are Rust's heap; ImageIO's own `malloc`
/// isn't counted. Past the bound the worker exits with
/// `heap::OVER_HEAP_EXIT`, which the parent reports as "needs too much
/// memory".
#[cfg(any(target_os = "macos", test))]
fn run_heif_worker(restrict: impl FnOnce(), serve: impl FnOnce() -> ExitCode) -> ExitCode {
    restrict();
    heap::arm(grape_heif_proto::WORKER_HEAP_BYTES);
    serve()
}

/// Elsewhere the image helper is `grape-heif.exe` (Windows) or none, so
/// `Grape --grape-heif-worker` exits 2 (bad usage) at once and never starts
/// a second tray app (heic-spec J20).
#[cfg(not(target_os = "macos"))]
fn heif_worker() -> ExitCode {
    ExitCode::from(2)
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

    fn is_pdf_worker(list: &[&str]) -> bool {
        first_arg_is(args(list), PDF_WORKER_FLAG)
    }

    fn is_heif_worker(list: &[&str]) -> bool {
        first_arg_is(args(list), HEIF_WORKER_FLAG)
    }

    #[test]
    fn the_flag_first_makes_the_pdf_worker() {
        assert_eq!(PDF_WORKER_FLAG, "--grape-pdf-worker");
        assert!(is_pdf_worker(&["Grape.exe", "--grape-pdf-worker"]));
        assert!(is_pdf_worker(&["grape", "--grape-pdf-worker", "x"]));
        // The image helper's flag isn't the PDF worker's.
        assert!(!is_pdf_worker(&["Grape", "--grape-heif-worker"]));
    }

    #[test]
    fn the_flag_first_makes_the_heif_worker() {
        assert_eq!(HEIF_WORKER_FLAG, "--grape-heif-worker");
        assert!(is_heif_worker(&["Grape", "--grape-heif-worker"]));
        assert!(is_heif_worker(&["Grape.exe", "--grape-heif-worker", "x"]));
        // The PDF flag isn't the image helper worker's.
        assert!(!is_heif_worker(&["Grape", "--grape-pdf-worker"]));
    }

    #[test]
    fn anything_else_starts_the_app() {
        for flag in ["--grape-pdf-worker", "--grape-heif-worker"] {
            let upper = flag.to_uppercase();
            let with_value = format!("{flag}=");
            let cases: [&[&str]; 7] = [
                &[],
                &["Grape.exe"],
                &["Grape.exe", "--at-login"],
                &["Grape.exe", "--at-login", flag],
                &["Grape.exe", &with_value],
                &["Grape.exe", &upper],
                // The program's own name is never the flag.
                &[flag],
            ];
            for case in cases {
                assert!(!first_arg_is(args(case), flag), "{case:?}");
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_heif_flag_exits_2_where_grape_isnt_the_image_helper() {
        assert_eq!(heif_worker(), ExitCode::from(2));
    }

    #[test]
    fn the_workers_heap_bound_sees_every_allocation() {
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

    /// The variable that makes [`heif_heap_child`] the child of
    /// [`the_mac_worker_arms_its_heap_bound`].
    const HEAP_CHILD: &str = "GRAPE_TEST_HEIF_HEAP_CHILD";

    /// Not a test: the child [`the_mac_worker_arms_its_heap_bound`] runs
    /// (`--exact … --ignored`). It runs the worker's own steps,
    /// [`run_heif_worker`], with a `serve` that asks for a block just under
    /// the bound, which it gets (only reserved, never touched), and one
    /// just over it, which ends the process with `OVER_HEAP_EXIT` before
    /// the system allocator sees it.
    #[test]
    #[ignore = "the heap bound's child process, run by the test below"]
    fn heif_heap_child() {
        if std::env::var_os(HEAP_CHILD).is_none() {
            return;
        }
        let code = run_heif_worker(
            || println!("restricted"),
            || {
                let bound = usize::try_from(grape_heif_proto::WORKER_HEAP_BYTES).unwrap();
                let under: Vec<u8> = Vec::with_capacity(bound - (64 << 20));
                drop(under);
                println!("under the bound");
                let over: Vec<u8> = Vec::with_capacity(bound + 1);
                drop(over);
                println!("over the bound");
                ExitCode::SUCCESS
            },
        );
        println!("served: {code:?}");
    }

    /// The Mac worker's heap bound (D41), armed by [`run_heif_worker`],
    /// which [`heif_worker`] runs: 2 GiB, after `restrict` and before
    /// `serve`, and past it the exit code the parent reads as "needs too
    /// much memory". The child is this test binary, so the bound never
    /// touches the other tests. (The worker itself is macOS only; its
    /// steps, the arm and the allocator are the same on every OS.)
    #[test]
    fn the_mac_worker_arms_its_heap_bound() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::heif_heap_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ])
            .env(HEAP_CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(heap::OVER_HEAP_EXIT), "{stdout}");
        let restricted = stdout.find("restricted").expect(&stdout);
        let under = stdout.find("under the bound").expect(&stdout);
        assert!(restricted < under, "{stdout}");
        assert!(!stdout.contains("over the bound"), "{stdout}");
        assert!(!stdout.contains("served"), "{stdout}");
    }

    #[cfg(unix)]
    #[test]
    fn an_argument_that_isnt_unicode_starts_the_app_without_a_panic() {
        use std::os::unix::ffi::OsStringExt;
        let odd = OsString::from_vec(vec![0x2d, 0x2d, 0xff]);
        let list = vec![OsString::from("grape"), odd];
        assert!(!first_arg_is(list.clone().into_iter(), PDF_WORKER_FLAG));
        assert!(!first_arg_is(list.into_iter(), HEIF_WORKER_FLAG));
    }
}
