//! Finds the sidecar programs Grape ships (FFmpeg, ffprobe, bsdtar and,
//! on Windows, `grape-heif`), the PDFium library and the HEIC helper once
//! at startup, and hands a copy to every job as [`SidecarPaths`].
//!
//! The programs sit next to Grape's own executable, without the
//! target-triple suffix they have in `binaries/`: the installer puts them
//! beside `Grape.exe` on Windows, the bundler puts them in
//! `Grape.app/Contents/MacOS` on macOS (the executable's own folder), and
//! in dev tauri-build copies `bundle.externalBin` into `target/debug` the
//! same way when `tauri dev` gets the `tauri.sidecars.conf.json` overlay
//! (D25). Nothing else is searched: never `PATH`, so an FFmpeg the user
//! installed is never run, and never through the shell plugin.
//!
//! PDFium (D29) is a library, loaded only by the PDF worker, which is
//! Grape's own executable started again as `Grape --grape-pdf-worker`
//! (`main.rs`). With the `tauri.pdfium.windows.conf.json` overlay it is
//! `pdfium.dll` next to `Grape.exe` (a bundle resource: the install root,
//! or `target\debug` in dev). With `tauri.pdfium.macos.conf.json` it is
//! `libpdfium.dylib` in `Grape.app/Contents/Frameworks`, which is
//! `../Frameworks` from the executable's folder; in dev tauri-build copies
//! it to `target/Frameworks` (`target/<triple>/Frameworks` for a `--target`
//! build), the same `../Frameworks` from `target/debug`. Other systems ship
//! none.
//!
//! bsdtar (D27) reads RAR archives: `bsdtar.exe` next to `Grape.exe`,
//! `Grape.app/Contents/MacOS/bsdtar`, by the same rule as FFmpeg.
//!
//! The HEIC helper (D33, `SidecarPaths::heif`) is started as
//! `<helper> --grape-heif-worker` on both systems. On Windows it's
//! `grape-heif.exe` next to `Grape.exe`, Grape's own sidecar (libheif, a
//! GPL program of its own, built by CI from `tools/grape-heif`; the
//! `tauri.heif.windows.conf.json` overlay bundles it), by the same rule as
//! FFmpeg. On macOS it's Grape's own executable (ImageIO, `main.rs`), like
//! the PDF worker, so it's always there and isn't logged as a sidecar.
//! Other systems have none.
//!
//! A sidecar that isn't there stays `None`, and nothing looks for it
//! anywhere else.

use std::env::consts::EXE_SUFFIX;
use std::path::Path;

use grape_engines::SidecarPaths;
use tauri::{AppHandle, Manager};

const FFMPEG: &str = "ffmpeg";
const FFPROBE: &str = "ffprobe";
const GRAPE_HEIF: &str = "grape-heif";
const BSDTAR: &str = "bsdtar";
const PDFIUM: &str = "pdfium";

/// Where this system's HEIC helper is (D33), or `None` where there's none.
const HEIF_AT: Option<HeifAt> = if cfg!(windows) {
    Some(HeifAt::NextToExe)
} else if cfg!(target_os = "macos") {
    Some(HeifAt::OwnExe)
} else {
    None
};

/// The HEIC helper, from Grape's executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeifAt {
    /// The `grape-heif` sidecar in the executable's own folder (Windows):
    /// looked for and logged like FFmpeg.
    NextToExe,
    /// The executable itself, as `Grape --grape-heif-worker` (macOS,
    /// ImageIO): always there, and not logged.
    OwnExe,
}

impl HeifAt {
    /// True when `grape-heif` is a sidecar to look for, and to log.
    fn is_sidecar(at: Option<HeifAt>) -> bool {
        at == Some(HeifAt::NextToExe)
    }
}

/// Where this system's PDFium build lands, or `None` where none ships.
const PDFIUM_AT: Option<PdfiumAt> = if cfg!(windows) {
    Some(PdfiumAt::NextToExe("pdfium.dll"))
} else if cfg!(target_os = "macos") {
    Some(PdfiumAt::Frameworks("libpdfium.dylib"))
} else {
    None
};

/// Where PDFium sits, from the folder Grape's executable is in. The file
/// names are the ones the PDFium overlays give it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PdfiumAt {
    /// In the executable's own folder (Windows).
    NextToExe(&'static str),
    /// In `Frameworks` beside the executable's folder (macOS):
    /// `Contents/MacOS` → `Contents/Frameworks`.
    Frameworks(&'static str),
}

/// The sidecars found at startup, managed as app state.
#[derive(Debug)]
pub struct Sidecars {
    engines: SidecarPaths,
}

impl Sidecars {
    /// Looks next to Grape's executable, once, and logs which sidecars it
    /// found, by name only.
    pub fn find() -> Self {
        let found = match std::env::current_exe() {
            Ok(exe) => Self {
                engines: resolve_all(&exe, EXE_SUFFIX, HEIF_AT, PDFIUM_AT, Path::is_file),
            },
            Err(err) => {
                log::warn!("sidecars: can't tell which folder Grape runs from: {err}");
                Self {
                    engines: SidecarPaths::default(),
                }
            }
        };
        log::info!(
            "sidecars: {}",
            summary(&found.looked_for(HeifAt::is_sidecar(HEIF_AT), PDFIUM_AT.is_some()))
        );
        found
    }

    /// Each sidecar looked for, by name, and whether it was found:
    /// `grape-heif` only when `with_grape_heif`, PDFium only when
    /// `with_pdfium`.
    fn looked_for(&self, with_grape_heif: bool, with_pdfium: bool) -> Vec<(&'static str, bool)> {
        let mut names = vec![
            (FFMPEG, self.engines.ffmpeg.is_some()),
            (FFPROBE, self.engines.ffprobe.is_some()),
        ];
        if with_grape_heif {
            names.push((GRAPE_HEIF, self.engines.heif.is_some()));
        }
        names.push((BSDTAR, self.engines.bsdtar.is_some()));
        if with_pdfium {
            names.push((PDFIUM, self.engines.pdfium.is_some()));
        }
        names
    }
}

/// A copy of the sidecars for one drop's jobs, or none if setup hasn't
/// found them (it always has by the time a drop runs).
pub fn for_jobs(app: &AppHandle) -> SidecarPaths {
    app.try_state::<Sidecars>()
        .map(|sidecars| sidecars.engines.clone())
        .unwrap_or_default()
}

/// Every sidecar for the executable `exe`: the programs in its folder
/// ([`resolve`]), PDFium and the PDF worker ([`resolve_pdf`]), and the
/// HEIC helper where `heif_at` says.
fn resolve_all(
    exe: &Path,
    suffix: &str,
    heif_at: Option<HeifAt>,
    pdfium_at: Option<PdfiumAt>,
    is_file: impl Fn(&Path) -> bool,
) -> SidecarPaths {
    let mut programs = exe.parent().map_or_else(SidecarPaths::default, |dir| {
        resolve(dir, suffix, HeifAt::is_sidecar(heif_at), &is_file)
    });
    if heif_at == Some(HeifAt::OwnExe) {
        programs.heif = Some(exe.to_path_buf());
    }
    let pdf = resolve_pdf(exe, pdfium_at, &is_file);
    SidecarPaths {
        pdfium: pdf.pdfium,
        pdf_worker: pdf.pdf_worker,
        ..programs
    }
}

/// The sidecars in `dir`, each named `{name}{suffix}` and kept only when
/// `is_file` says it's a file. `grape-heif` is looked for only when
/// `with_grape_heif`, and goes in `heif`.
fn resolve(
    dir: &Path,
    suffix: &str,
    with_grape_heif: bool,
    is_file: impl Fn(&Path) -> bool,
) -> SidecarPaths {
    let find = |name: &str| {
        let path = dir.join(format!("{name}{suffix}"));
        is_file(&path).then_some(path)
    };
    SidecarPaths {
        ffmpeg: find(FFMPEG),
        ffprobe: find(FFPROBE),
        heif: if with_grape_heif {
            find(GRAPE_HEIF)
        } else {
            None
        },
        bsdtar: find(BSDTAR),
        ..SidecarPaths::default()
    }
}

/// The PDF engine's paths for the executable `exe`: PDFium where `at`
/// says, kept only when `is_file` says it's a file (and never when `at` is
/// `None`), and `exe` itself as the worker. Only `pdfium` and
/// `pdf_worker` are set.
fn resolve_pdf(exe: &Path, at: Option<PdfiumAt>, is_file: impl Fn(&Path) -> bool) -> SidecarPaths {
    let dir = exe.parent();
    let pdfium = match (at, dir) {
        (Some(PdfiumAt::NextToExe(name)), Some(dir)) => Some(dir.join(name)),
        // The parent of the executable's folder, not `..`: the same folder
        // `@executable_path/../Frameworks` names, without a `..` in the path.
        (Some(PdfiumAt::Frameworks(name)), Some(dir)) => {
            dir.parent().map(|up| up.join("Frameworks").join(name))
        }
        (None, _) | (_, None) => None,
    };
    SidecarPaths {
        pdfium: pdfium.filter(|path| is_file(path)),
        pdf_worker: Some(exe.to_path_buf()),
        ..SidecarPaths::default()
    }
}

/// What the log says: "found ffmpeg, ffprobe; missing grape-heif". Names
/// only, never paths.
fn summary(looked_for: &[(&str, bool)]) -> String {
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    for &(name, is_found) in looked_for {
        if is_found {
            found.push(name);
        } else {
            missing.push(name);
        }
    }
    match (found.as_slice(), missing.as_slice()) {
        ([], missing) => format!("none found (looked for {})", missing.join(", ")),
        (found, []) => format!("found {}", found.join(", ")),
        (found, missing) => format!("found {}; missing {}", found.join(", "), missing.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    /// An `is_file` that knows only `files`.
    fn only(files: &[PathBuf]) -> impl Fn(&Path) -> bool {
        let files: HashSet<PathBuf> = files.iter().cloned().collect();
        move |path| files.contains(path)
    }

    /// What [`Sidecars::find`] would log for these paths.
    fn summary_of(engines: &SidecarPaths, with_grape_heif: bool, with_pdfium: bool) -> String {
        let sidecars = Sidecars {
            engines: engines.clone(),
        };
        summary(&sidecars.looked_for(with_grape_heif, with_pdfium))
    }

    const WINDOWS: Option<PdfiumAt> = Some(PdfiumAt::NextToExe("pdfium.dll"));
    const MACOS: Option<PdfiumAt> = Some(PdfiumAt::Frameworks("libpdfium.dylib"));
    const WIN_HEIF: Option<HeifAt> = Some(HeifAt::NextToExe);
    const MAC_HEIF: Option<HeifAt> = Some(HeifAt::OwnExe);

    #[test]
    fn this_system_looks_for_its_own_pdfium_build() {
        let expected = if cfg!(windows) {
            WINDOWS
        } else if cfg!(target_os = "macos") {
            MACOS
        } else {
            None
        };
        assert_eq!(PDFIUM_AT, expected);
    }

    #[test]
    fn this_system_runs_its_own_heic_helper() {
        let expected = if cfg!(windows) {
            Some(HeifAt::NextToExe)
        } else if cfg!(target_os = "macos") {
            Some(HeifAt::OwnExe)
        } else {
            None
        };
        assert_eq!(HEIF_AT, expected);
    }

    #[test]
    fn windows_looks_for_four_exes_next_to_grape() {
        let dir = Path::new("install");
        let all = [
            dir.join("ffmpeg.exe"),
            dir.join("ffprobe.exe"),
            dir.join("grape-heif.exe"),
            dir.join("bsdtar.exe"),
        ];
        let paths = resolve(dir, ".exe", true, only(&all));
        assert_eq!(paths.ffmpeg, Some(dir.join("ffmpeg.exe")));
        assert_eq!(paths.ffprobe, Some(dir.join("ffprobe.exe")));
        assert_eq!(paths.heif, Some(dir.join("grape-heif.exe")));
        assert_eq!(paths.bsdtar, Some(dir.join("bsdtar.exe")));
        assert_eq!(
            summary_of(&paths, true, false),
            "found ffmpeg, ffprobe, grape-heif, bsdtar"
        );
    }

    #[test]
    fn macos_looks_for_plain_names_and_never_grape_heif() {
        let dir = Path::new("Grape.app").join("Contents").join("MacOS");
        let all = [
            dir.join("ffmpeg"),
            dir.join("ffprobe"),
            dir.join("grape-heif"),
            dir.join("bsdtar"),
        ];
        let paths = resolve(&dir, "", false, only(&all));
        assert_eq!(paths.ffmpeg, Some(dir.join("ffmpeg")));
        assert_eq!(paths.ffprobe, Some(dir.join("ffprobe")));
        assert_eq!(paths.heif, None);
        assert_eq!(paths.bsdtar, Some(dir.join("bsdtar")));
        assert_eq!(
            summary_of(&paths, false, false),
            "found ffmpeg, ffprobe, bsdtar"
        );
    }

    #[test]
    fn a_missing_sidecar_stays_none() {
        let dir = Path::new("install");
        let paths = resolve(dir, ".exe", true, only(&[dir.join("ffprobe.exe")]));
        assert_eq!(paths.ffmpeg, None);
        assert_eq!(paths.ffprobe, Some(dir.join("ffprobe.exe")));
        assert_eq!(paths.heif, None);
        assert_eq!(
            summary_of(&paths, true, false),
            "found ffprobe; missing ffmpeg, grape-heif, bsdtar"
        );
    }

    #[test]
    fn only_the_right_names_in_that_folder_count() {
        let dir = Path::new("install");
        let elsewhere = [
            // No suffix on Windows, the target-triple name from
            // `binaries/`, and the right name in another folder.
            dir.join("ffmpeg"),
            dir.join("ffmpeg-x86_64-pc-windows-msvc.exe"),
            dir.join("bsdtar-x86_64-pc-windows-msvc.exe"),
            dir.join("grape-heif-x86_64-pc-windows-msvc.exe"),
            // The tool it replaced (D33) never counts.
            dir.join("heif-enc.exe"),
            Path::new("bin").join("ffprobe.exe"),
        ];
        let paths = resolve(dir, ".exe", true, only(&elsewhere));
        assert_eq!(paths.ffmpeg, None);
        assert_eq!(paths.ffprobe, None);
        assert_eq!(paths.bsdtar, None);
        assert_eq!(paths.heif, None);
        assert_eq!(
            summary_of(&paths, true, false),
            "none found (looked for ffmpeg, ffprobe, grape-heif, bsdtar)"
        );
        assert_eq!(
            summary_of(&SidecarPaths::default(), false, false),
            "none found (looked for ffmpeg, ffprobe, bsdtar)"
        );
    }

    #[test]
    fn a_folder_with_a_sidecars_name_is_not_a_sidecar() {
        let dir = std::env::temp_dir().join(format!("grape-sidecars-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(format!("ffmpeg{EXE_SUFFIX}"))).expect("a temp folder");
        let ffprobe = dir.join(format!("ffprobe{EXE_SUFFIX}"));
        std::fs::write(&ffprobe, b"x").expect("a temp file");
        std::fs::create_dir_all(dir.join(format!("grape-heif{EXE_SUFFIX}")))
            .expect("a temp folder");
        let paths = resolve(&dir, EXE_SUFFIX, true, Path::is_file);
        assert_eq!(paths.ffmpeg, None);
        assert_eq!(paths.ffprobe, Some(ffprobe));
        assert_eq!(paths.heif, None);
        std::fs::remove_dir_all(&dir).expect("the temp folder removed");
    }

    #[test]
    fn windows_finds_pdfium_dll_next_to_grape_exe() {
        let dir = Path::new("install");
        let exe = dir.join("Grape.exe");
        let pdf = resolve_pdf(&exe, WINDOWS, only(&[dir.join("pdfium.dll")]));
        assert_eq!(pdf.pdfium, Some(dir.join("pdfium.dll")));
        assert_eq!(pdf.pdf_worker, Some(exe));
    }

    #[test]
    fn windows_dev_finds_the_dll_tauri_build_copied_to_target_debug() {
        let dir = Path::new("target").join("debug");
        let exe = dir.join("grape.exe");
        let pdf = resolve_pdf(&exe, WINDOWS, only(&[dir.join("pdfium.dll")]));
        assert_eq!(pdf.pdfium, Some(dir.join("pdfium.dll")));
    }

    #[test]
    fn macos_finds_the_dylib_in_contents_frameworks() {
        let contents = Path::new("Grape.app").join("Contents");
        let exe = contents.join("MacOS").join("Grape");
        let dylib = contents.join("Frameworks").join("libpdfium.dylib");
        let pdf = resolve_pdf(&exe, MACOS, only(std::slice::from_ref(&dylib)));
        assert_eq!(pdf.pdfium, Some(dylib));
        assert_eq!(pdf.pdf_worker, Some(exe));
    }

    #[test]
    fn macos_dev_finds_the_dylib_in_target_frameworks() {
        // `tauri dev`: target/debug/grape and target/Frameworks; a
        // `--target` build nests both one level down.
        for target in [
            PathBuf::from("target"),
            Path::new("target").join("x86_64-apple-darwin"),
        ] {
            let exe = target.join("debug").join("grape");
            let dylib = target.join("Frameworks").join("libpdfium.dylib");
            let pdf = resolve_pdf(&exe, MACOS, only(std::slice::from_ref(&dylib)));
            assert_eq!(pdf.pdfium, Some(dylib));
        }
    }

    #[test]
    fn pdfium_anywhere_else_doesnt_count() {
        let contents = Path::new("Grape.app").join("Contents");
        let mac_exe = contents.join("MacOS").join("Grape");
        let elsewhere = [
            // Next to the executable, in Resources, under the name in
            // `binaries/`, and with the other system's name.
            contents.join("MacOS").join("libpdfium.dylib"),
            contents.join("Resources").join("libpdfium.dylib"),
            contents
                .join("Frameworks")
                .join("libpdfium-universal-apple-darwin.dylib"),
            contents.join("Frameworks").join("pdfium.dll"),
        ];
        let pdf = resolve_pdf(&mac_exe, MACOS, only(&elsewhere));
        assert_eq!(pdf.pdfium, None);
        // The worker is Grape itself, found or not.
        assert_eq!(pdf.pdf_worker, Some(mac_exe));

        let dir = Path::new("install");
        let win_elsewhere = [
            dir.join("pdfium-x86_64-pc-windows-msvc.dll"),
            dir.join("binaries").join("pdfium.dll"),
            Path::new("Frameworks").join("pdfium.dll"),
        ];
        let pdf = resolve_pdf(&dir.join("Grape.exe"), WINDOWS, only(&win_elsewhere));
        assert_eq!(pdf.pdfium, None);
    }

    #[test]
    fn a_system_without_a_pdfium_build_never_finds_one() {
        let dir = Path::new("target").join("debug");
        let exe = dir.join("grape");
        let everywhere = [
            dir.join("pdfium.dll"),
            dir.join("libpdfium.so"),
            Path::new("target")
                .join("Frameworks")
                .join("libpdfium.dylib"),
        ];
        let pdf = resolve_pdf(&exe, None, only(&everywhere));
        assert_eq!(pdf.pdfium, None);
        assert_eq!(pdf.pdf_worker, Some(exe));
    }

    #[test]
    fn an_executable_at_the_root_has_no_frameworks_folder() {
        // `/Grape` has a folder, `/`, which has no parent.
        let root = Path::new(std::path::MAIN_SEPARATOR_STR);
        let exe = root.join("Grape");
        let pdf = resolve_pdf(&exe, MACOS, |_| true);
        assert_eq!(pdf.pdfium, None);
        assert_eq!(pdf.pdf_worker, Some(exe));
    }

    #[test]
    fn a_folder_named_like_pdfium_is_not_pdfium() {
        let base = std::env::temp_dir().join(format!("grape-pdfium-{}", std::process::id()));
        let exe_dir = base.join("MacOS");
        std::fs::create_dir_all(&exe_dir).expect("a temp folder");
        let exe = exe_dir.join("grape");
        std::fs::write(&exe, b"x").expect("a temp file");
        let dll = exe_dir.join("pdfium.dll");
        let dylib = base.join("Frameworks").join("libpdfium.dylib");
        std::fs::create_dir_all(&dll).expect("a folder named pdfium.dll");
        std::fs::create_dir_all(&dylib).expect("a folder named libpdfium.dylib");
        assert_eq!(resolve_pdf(&exe, WINDOWS, Path::is_file).pdfium, None);
        assert_eq!(resolve_pdf(&exe, MACOS, Path::is_file).pdfium, None);
        std::fs::remove_dir_all(&dll).expect("the folder removed");
        std::fs::remove_dir_all(&dylib).expect("the folder removed");
        std::fs::write(&dll, b"x").expect("a temp file");
        std::fs::write(&dylib, b"x").expect("a temp file");
        assert_eq!(resolve_pdf(&exe, WINDOWS, Path::is_file).pdfium, Some(dll));
        assert_eq!(resolve_pdf(&exe, MACOS, Path::is_file).pdfium, Some(dylib));
        std::fs::remove_dir_all(&base).expect("the temp folder removed");
    }

    #[test]
    fn the_log_names_pdfium_where_one_ships() {
        let engines = SidecarPaths {
            ffmpeg: Some(PathBuf::from("ffmpeg.exe")),
            ffprobe: Some(PathBuf::from("ffprobe.exe")),
            heif: None,
            ..SidecarPaths::default()
        };
        let found = SidecarPaths {
            pdfium: Some(PathBuf::from("pdfium.dll")),
            pdf_worker: Some(PathBuf::from("Grape.exe")),
            ..engines.clone()
        };
        let missing = SidecarPaths {
            pdfium: None,
            pdf_worker: Some(PathBuf::from("Grape.exe")),
            ..engines.clone()
        };
        assert_eq!(
            summary_of(&found, true, true),
            "found ffmpeg, ffprobe, pdfium; missing grape-heif, bsdtar"
        );
        assert_eq!(
            summary_of(&missing, false, true),
            "found ffmpeg, ffprobe; missing bsdtar, pdfium"
        );
        assert_eq!(
            summary_of(&SidecarPaths::default(), false, true),
            "none found (looked for ffmpeg, ffprobe, bsdtar, pdfium)"
        );
        // Linux: no PDFium build, so the log doesn't mention it.
        assert_eq!(
            summary_of(&missing, false, false),
            "found ffmpeg, ffprobe; missing bsdtar"
        );
    }

    #[test]
    fn every_field_is_filled_in_one_go_on_windows() {
        let dir = Path::new("install");
        let exe = dir.join("Grape.exe");
        let all = [
            dir.join("ffmpeg.exe"),
            dir.join("ffprobe.exe"),
            dir.join("grape-heif.exe"),
            dir.join("bsdtar.exe"),
            dir.join("pdfium.dll"),
        ];
        let paths = resolve_all(&exe, ".exe", WIN_HEIF, WINDOWS, only(&all));
        assert_eq!(paths.ffmpeg, Some(dir.join("ffmpeg.exe")));
        assert_eq!(paths.ffprobe, Some(dir.join("ffprobe.exe")));
        assert_eq!(paths.heif, Some(dir.join("grape-heif.exe")));
        assert_eq!(paths.bsdtar, Some(dir.join("bsdtar.exe")));
        assert_eq!(paths.pdfium, Some(dir.join("pdfium.dll")));
        assert_eq!(paths.pdf_worker, Some(exe));
        assert_eq!(
            summary_of(&paths, true, true),
            "found ffmpeg, ffprobe, grape-heif, bsdtar, pdfium"
        );
    }

    #[test]
    fn windows_without_grape_heif_has_no_heic_helper() {
        let dir = Path::new("install");
        let exe = dir.join("Grape.exe");
        // Grape.exe itself is never the HEIC helper on Windows.
        let paths = resolve_all(
            &exe,
            ".exe",
            WIN_HEIF,
            WINDOWS,
            only(&[exe.clone(), dir.join("pdfium.dll")]),
        );
        assert_eq!(paths.heif, None);
        assert_eq!(
            summary_of(&paths, true, true),
            "found pdfium; missing ffmpeg, ffprobe, grape-heif, bsdtar"
        );
    }

    #[test]
    fn macos_runs_grape_itself_as_the_heic_helper() {
        let contents = Path::new("Grape.app").join("Contents");
        let exe = contents.join("MacOS").join("Grape");
        // A grape-heif next to it is never looked at.
        let stray = contents.join("MacOS").join("grape-heif");
        let dylib = contents.join("Frameworks").join("libpdfium.dylib");
        let paths = resolve_all(&exe, "", MAC_HEIF, MACOS, only(&[stray, dylib]));
        assert_eq!(paths.heif, Some(exe.clone()));
        assert_eq!(paths.pdf_worker, Some(exe));
        // And it isn't a sidecar, so the log doesn't name it.
        assert_eq!(
            summary_of(&paths, HeifAt::is_sidecar(MAC_HEIF), true),
            "found pdfium; missing ffmpeg, ffprobe, bsdtar"
        );
    }

    #[test]
    fn a_system_without_a_heic_helper_never_finds_one() {
        let dir = Path::new("target").join("debug");
        let exe = dir.join("grape");
        let everywhere = [dir.join("grape-heif"), dir.join("grape-heif.exe")];
        let paths = resolve_all(&exe, "", None, None, only(&everywhere));
        assert_eq!(paths.heif, None);
        assert!(!HeifAt::is_sidecar(None));
    }

    #[test]
    fn macos_finds_bsdtar_in_contents_macos_only() {
        let contents = Path::new("Grape.app").join("Contents");
        let exe = contents.join("MacOS").join("Grape");
        let bsdtar = contents.join("MacOS").join("bsdtar");
        let paths = resolve_all(
            &exe,
            "",
            MAC_HEIF,
            MACOS,
            only(std::slice::from_ref(&bsdtar)),
        );
        assert_eq!(paths.bsdtar, Some(bsdtar));
        let elsewhere = [
            contents.join("Resources").join("bsdtar"),
            contents.join("Frameworks").join("bsdtar"),
            contents.join("MacOS").join("bsdtar-universal-apple-darwin"),
        ];
        let paths = resolve_all(&exe, "", MAC_HEIF, MACOS, only(&elsewhere));
        assert_eq!(paths.bsdtar, None);
        assert_eq!(paths.pdf_worker, Some(exe));
    }
}
