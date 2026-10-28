//! ImageIO behind the HEIC protocol, as its own program (DECISIONS D33):
//! what `Grape --grape-heif-worker` runs, without the rest of Grape. CI's
//! `heif-it-macos` job points grape-engines' HEIC tests at it
//! (`GRAPE_TEST_HEIF`). Built only with the `heif-it` feature and never
//! shipped. Elsewhere it says "macOS only" and exits 2.

use std::process::ExitCode;

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    // As Grape does: exactly the flag, right after the program's name.
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next()) {
        (Some(flag), None) if flag == grape_heif_proto::WORKER_FLAG => {
            use grape_platform::macos::heif::ImageIo;

            ImageIo::restrict_process();
            grape_heif_proto::worker_main(&ImageIo)
        }
        _ => {
            eprintln!("usage: grape-heif-worker {}", grape_heif_proto::WORKER_FLAG);
            ExitCode::from(2)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("grape-heif-worker: macOS only (Windows uses grape-heif.exe)");
    ExitCode::from(2)
}
