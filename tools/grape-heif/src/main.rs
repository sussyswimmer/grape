//! `grape-heif.exe`, Grape's image helper on Windows: HEIC and AVIF
//! (DECISIONS D33, D41).
//!
//! Grape starts it as `grape-heif.exe --grape-heif-worker`, writes one
//! request to its stdin and reads JSON lines from its stdout: the protocol in
//! `crates/grape-heif-proto`. The codec is libheif with libde265 (HEIC
//! decode), x265 (HEIC encode), dav1d (AVIF decode) and libaom (AVIF encode),
//! linked statically from vcpkg by `build.ps1`, with plugin loading compiled
//! out. The program is GPL-3.0-or-later as a whole, so it only ever ships as
//! this separate executable (golden rule 6).
//!
//! `grape-heif.exe -hide_banner -version` prints the banner that Grape's
//! bundling gate (`scripts/check-sidecars.mjs`, D25) and CI read.

mod banner;
mod codec;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use codec::LibheifCodec;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// `--grape-heif-worker`: one request on stdin, the lines on stdout.
    Worker,
    /// `-version` or `--version`, with or without `-hide_banner`.
    Banner,
    /// Anything else.
    Usage,
}

fn mode(args: &[OsString]) -> Mode {
    if args.len() == 1 && args[0] == grape_heif_proto::WORKER_FLAG {
        return Mode::Worker;
    }
    let version = |a: &OsString| a == "-version" || a == "--version";
    if args.iter().any(version) && args.iter().all(|a| version(a) || a == "-hide_banner") {
        return Mode::Banner;
    }
    Mode::Usage
}

fn main() -> ExitCode {
    harden();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match mode(&args) {
        Mode::Worker => grape_heif_proto::worker_main(&LibheifCodec::new()),
        Mode::Banner => {
            let codec = LibheifCodec::new();
            let text = banner::banner(&codec.components());
            let mut out = std::io::stdout().lock();
            if out
                .write_all(text.as_bytes())
                .and_then(|()| out.flush())
                .is_ok()
            {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Mode::Usage => {
            eprintln!(
                "grape-heif: Grape's image helper (HEIC and AVIF). Grape runs it as `grape-heif {}`; `grape-heif -version` prints its version.",
                grape_heif_proto::WORKER_FLAG
            );
            ExitCode::from(2)
        }
    }
}

/// First thing on Windows (heic-spec J10): DLLs load only from System32, so
/// nothing planted next to the exe or in the working folder is ever loaded
/// (x265 looks up `libx265_main10.dll` for bit depths this build lacks), and
/// a crash or a missing drive never shows an error dialog.
#[cfg(windows)]
fn harden() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX,
    };
    use windows_sys::Win32::System::LibraryLoader::{
        SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    // SAFETY: both take plain flags and touch only this process's settings.
    // A failure leaves the defaults, which is no worse; there's nothing to
    // report it to before the protocol starts.
    unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    }
}

#[cfg(not(windows))]
fn harden() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_command_line_picks_one_mode() {
        assert_eq!(mode(&args(&["--grape-heif-worker"])), Mode::Worker);
        for banner in [
            &["-version"][..],
            &["--version"],
            &["-hide_banner", "-version"],
            &["-version", "-hide_banner"],
            &["-hide_banner", "--version"],
        ] {
            assert_eq!(mode(&args(banner)), Mode::Banner, "{banner:?}");
        }
        for usage in [
            &[][..],
            &["-hide_banner"],
            &["--grape-heif-worker", "x"],
            &["x", "--grape-heif-worker"],
            &["--grape-heif-worker", "-version"],
            &["-version", "x"],
            &["--help"],
        ] {
            assert_eq!(mode(&args(usage)), Mode::Usage, "{usage:?}");
        }
    }
}
