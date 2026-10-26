//! Finds the sidecar programs Grape ships (FFmpeg, ffprobe and, on
//! Windows, `heif-enc`) once at startup, and hands a copy to every job as
//! [`SidecarPaths`].
//!
//! They sit next to Grape's own executable, without the target-triple
//! suffix they have in `binaries/`: the installer puts them beside
//! `Grape.exe` on Windows, the bundler puts them in `Grape.app/Contents/MacOS`
//! on macOS (the executable's own folder), and in dev tauri-build copies
//! `bundle.externalBin` into `target/debug` the same way when `tauri dev`
//! gets the `tauri.sidecars.conf.json` overlay (D25). Nothing else is
//! searched: never `PATH`, so an FFmpeg the user installed is never run,
//! and never through the shell plugin.
//!
//! A sidecar that isn't there stays `None`, and nothing looks for it
//! anywhere else.

use std::env::consts::EXE_SUFFIX;
use std::path::Path;

use grape_engines::SidecarPaths;
use tauri::{AppHandle, Manager};

const FFMPEG: &str = "ffmpeg";
const FFPROBE: &str = "ffprobe";
const HEIF_ENC: &str = "heif-enc";

/// Only Windows ships `heif-enc`: macOS encodes HEIC with ImageIO
/// (`docs/ENGINES.md`).
const WITH_HEIF_ENC: bool = cfg!(windows);

/// The sidecars found at startup, managed as app state.
#[derive(Debug)]
pub struct Sidecars(SidecarPaths);

impl Sidecars {
    /// Looks next to Grape's executable, once, and logs which sidecars it
    /// found, by name only.
    pub fn find() -> Self {
        let paths = match std::env::current_exe() {
            Ok(exe) => exe.parent().map_or_else(SidecarPaths::default, |dir| {
                resolve(dir, EXE_SUFFIX, WITH_HEIF_ENC, Path::is_file)
            }),
            Err(err) => {
                log::warn!("sidecars: can't tell which folder Grape runs from: {err}");
                SidecarPaths::default()
            }
        };
        log::info!("sidecars: {}", summary(&paths, WITH_HEIF_ENC));
        Self(paths)
    }
}

/// A copy of the sidecars for one drop's jobs, or none if setup hasn't
/// found them (it always has by the time a drop runs).
pub fn for_jobs(app: &AppHandle) -> SidecarPaths {
    app.try_state::<Sidecars>()
        .map(|sidecars| sidecars.0.clone())
        .unwrap_or_default()
}

/// The sidecars in `dir`, each named `{name}{suffix}` and kept only when
/// `is_file` says it's a file. `heif-enc` is looked for only when
/// `with_heif_enc`.
fn resolve(
    dir: &Path,
    suffix: &str,
    with_heif_enc: bool,
    is_file: impl Fn(&Path) -> bool,
) -> SidecarPaths {
    let find = |name: &str| {
        let path = dir.join(format!("{name}{suffix}"));
        is_file(&path).then_some(path)
    };
    SidecarPaths {
        ffmpeg: find(FFMPEG),
        ffprobe: find(FFPROBE),
        heif_enc: if with_heif_enc { find(HEIF_ENC) } else { None },
    }
}

/// What the log says: "found ffmpeg, ffprobe; missing heif-enc". Names
/// only, never paths.
fn summary(paths: &SidecarPaths, with_heif_enc: bool) -> String {
    let heif_enc = with_heif_enc.then_some((HEIF_ENC, &paths.heif_enc));
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    for (name, path) in [(FFMPEG, &paths.ffmpeg), (FFPROBE, &paths.ffprobe)]
        .into_iter()
        .chain(heif_enc)
    {
        if path.is_some() {
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

    #[test]
    fn windows_looks_for_three_exes_next_to_grape() {
        let dir = Path::new("install");
        let all = [
            dir.join("ffmpeg.exe"),
            dir.join("ffprobe.exe"),
            dir.join("heif-enc.exe"),
        ];
        let paths = resolve(dir, ".exe", true, only(&all));
        assert_eq!(paths.ffmpeg, Some(dir.join("ffmpeg.exe")));
        assert_eq!(paths.ffprobe, Some(dir.join("ffprobe.exe")));
        assert_eq!(paths.heif_enc, Some(dir.join("heif-enc.exe")));
        assert_eq!(summary(&paths, true), "found ffmpeg, ffprobe, heif-enc");
    }

    #[test]
    fn macos_looks_for_two_plain_names_and_never_heif_enc() {
        let dir = Path::new("Grape.app").join("Contents").join("MacOS");
        let all = [
            dir.join("ffmpeg"),
            dir.join("ffprobe"),
            dir.join("heif-enc"),
        ];
        let paths = resolve(&dir, "", false, only(&all));
        assert_eq!(paths.ffmpeg, Some(dir.join("ffmpeg")));
        assert_eq!(paths.ffprobe, Some(dir.join("ffprobe")));
        assert_eq!(paths.heif_enc, None);
        assert_eq!(summary(&paths, false), "found ffmpeg, ffprobe");
    }

    #[test]
    fn a_missing_sidecar_stays_none() {
        let dir = Path::new("install");
        let paths = resolve(dir, ".exe", true, only(&[dir.join("ffprobe.exe")]));
        assert_eq!(paths.ffmpeg, None);
        assert_eq!(paths.ffprobe, Some(dir.join("ffprobe.exe")));
        assert_eq!(paths.heif_enc, None);
        assert_eq!(
            summary(&paths, true),
            "found ffprobe; missing ffmpeg, heif-enc"
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
            Path::new("bin").join("ffprobe.exe"),
        ];
        let paths = resolve(dir, ".exe", true, only(&elsewhere));
        assert_eq!(paths.ffmpeg, None);
        assert_eq!(paths.ffprobe, None);
        assert_eq!(
            summary(&paths, true),
            "none found (looked for ffmpeg, ffprobe, heif-enc)"
        );
        assert_eq!(
            summary(&SidecarPaths::default(), false),
            "none found (looked for ffmpeg, ffprobe)"
        );
    }

    #[test]
    fn a_folder_with_a_sidecars_name_is_not_a_sidecar() {
        let dir = std::env::temp_dir().join(format!("grape-sidecars-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(format!("ffmpeg{EXE_SUFFIX}"))).expect("a temp folder");
        let ffprobe = dir.join(format!("ffprobe{EXE_SUFFIX}"));
        std::fs::write(&ffprobe, b"x").expect("a temp file");
        let paths = resolve(&dir, EXE_SUFFIX, WITH_HEIF_ENC, Path::is_file);
        assert_eq!(paths.ffmpeg, None);
        assert_eq!(paths.ffprobe, Some(ffprobe));
        assert_eq!(paths.heif_enc, None);
        std::fs::remove_dir_all(&dir).expect("the temp folder removed");
    }
}
