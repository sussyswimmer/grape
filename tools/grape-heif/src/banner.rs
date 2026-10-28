//! The version banner, in FFmpeg's shape, for Grape's bundling gate (D25)
//! and CI (heic-spec §5.3):
//!
//! ```text
//! grape-heif version 1.23.7-grape.2 Copyright (c) 2026 the Grape contributors
//! configuration: libheif=1.23.7 libde265=1.1.3 x265=4.3 plugins=disabled crt=static protocol=2
//! license: GPL-3.0-or-later (libheif and libde265 LGPL-3.0-or-later, x265 GPL-2.0-or-later)
//! ```
//!
//! Every value on the configuration line is the running build's own answer,
//! never a constant: CI compares them with the pins in `vcpkg.lock.json`.

use std::ffi::CStr;

use libheif_rs::{CompressionFormat, LibHeif};
use libheif_sys as lh;

/// This program's version: libheif's, then Grape's build number.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the linked libraries say about themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Components {
    /// `heif_get_version()`.
    pub libheif: String,
    /// From libheif's HEVC decoder's name, when it has one.
    pub libde265: Option<String>,
    /// From libheif's HEVC encoder's name, when it has one.
    pub x265: Option<String>,
    /// libheif's own answer: no plugin directory at all.
    pub plugins_disabled: bool,
}

impl Components {
    /// Asks the linked libheif. `lib` is `None` when libheif didn't start.
    pub fn read(lib: Option<&LibHeif>) -> Components {
        let Some(lib) = lib else {
            return Components {
                libheif: libheif_version(),
                libde265: None,
                x265: None,
                plugins_disabled: plugins_disabled(),
            };
        };
        let libde265 = lib
            .decoder_descriptors(16, Some(CompressionFormat::Hevc))
            .iter()
            .find_map(|d| libde265_version(&d.name()));
        let x265 = lib
            .encoder_descriptors(16, Some(CompressionFormat::Hevc), None)
            .iter()
            .find_map(|d| x265_version(&d.name()));
        Components {
            libheif: libheif_version(),
            libde265,
            x265,
            plugins_disabled: plugins_disabled(),
        }
    }

    /// The codec's name in `probe`'s caps: "libheif 1.23.7 (libde265 1.1.3, x265 4.3)".
    pub fn codec_name(&self) -> String {
        format!(
            "libheif {} (libde265 {}, x265 {})",
            self.libheif,
            self.libde265.as_deref().unwrap_or("none"),
            self.x265.as_deref().unwrap_or("none")
        )
    }
}

/// The three banner lines, each ending in a newline.
pub fn banner(c: &Components) -> String {
    format!(
        "grape-heif version {VERSION} Copyright (c) 2026 the Grape contributors\n\
         configuration: libheif={} libde265={} x265={} plugins={} crt={} protocol={}\n\
         license: GPL-3.0-or-later (libheif and libde265 LGPL-3.0-or-later, x265 GPL-2.0-or-later)\n",
        c.libheif,
        c.libde265.as_deref().unwrap_or("none"),
        c.x265.as_deref().unwrap_or("none"),
        if c.plugins_disabled { "disabled" } else { "enabled" },
        if cfg!(target_feature = "crt-static") { "static" } else { "dynamic" },
        grape_heif_proto::PROTOCOL,
    )
}

/// The libheif this build was made for: the first three parts of
/// [`VERSION`] (`1.23.7-grape.2` → `[1, 23, 7]`).
pub fn expected_libheif() -> Option<[u8; 3]> {
    Some([
        env!("CARGO_PKG_VERSION_MAJOR").parse().ok()?,
        env!("CARGO_PKG_VERSION_MINOR").parse().ok()?,
        env!("CARGO_PKG_VERSION_PATCH").parse().ok()?,
    ])
}

/// libde265's version from its plugin's name, "libde265 HEVC decoder, version 1.1.3".
pub fn libde265_version(decoder_name: &str) -> Option<String> {
    leading_version(decoder_name.strip_prefix("libde265 HEVC decoder, version ")?)
}

/// x265's version from its plugin's name, "x265 HEVC encoder (4.3+1-e9b8812)":
/// up to the first character that isn't part of the version.
pub fn x265_version(encoder_name: &str) -> Option<String> {
    leading_version(encoder_name.strip_prefix("x265 HEVC encoder (")?)
}

/// The digits and dots at the start of `text`, when they make a version.
fn leading_version(text: &str) -> Option<String> {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let version = text[..end].trim_end_matches('.');
    (version.starts_with(|c: char| c.is_ascii_digit())).then(|| version.to_string())
}

fn libheif_version() -> String {
    // SAFETY: heif_get_version returns a pointer to a static, NUL-terminated
    // string (LIBHEIF_VERSION), or null; it needs no initialization.
    let ptr = unsafe { lh::heif_get_version() };
    if ptr.is_null() {
        return "unknown".to_string();
    }
    // SAFETY: not null, and libheif's static string lives as long as the
    // program.
    let text = unsafe { CStr::from_ptr(ptr) };
    text.to_str().unwrap_or("unknown").to_string()
}

/// libheif's own test, as in heif-enc (`examples/common.cc`): with plugin
/// loading compiled out it reports no plugin directory at all, even when
/// `LIBHEIF_PLUGIN_PATH` is set.
fn plugins_disabled() -> bool {
    // SAFETY: heif_get_plugin_directories returns a new array of C strings,
    // ended by a null entry, which is read here and then handed back to its
    // own free function exactly once. A null array (never returned today)
    // counts as "enabled", so CI fails rather than passes.
    unsafe {
        let dirs = lh::heif_get_plugin_directories();
        if dirs.is_null() {
            return false;
        }
        let none = (*dirs).is_null();
        lh::heif_free_plugin_directories(dirs);
        none
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_come_from_the_plugins_names() {
        assert_eq!(
            libde265_version("libde265 HEVC decoder, version 1.1.3").as_deref(),
            Some("1.1.3")
        );
        assert_eq!(
            x265_version("x265 HEVC encoder (4.3+1-e9b8812)").as_deref(),
            Some("4.3")
        );
        assert_eq!(
            x265_version("x265 HEVC encoder (4.3)").as_deref(),
            Some("4.3")
        );
        assert_eq!(x265_version("x265 HEVC encoder (null)"), None);
        assert_eq!(x265_version("kvazaar HEVC encoder (2.3.2)"), None);
        assert_eq!(libde265_version("libde265 HEVC decoder"), None);
        assert_eq!(leading_version("4.3."), Some("4.3".to_string()));
        assert_eq!(leading_version(".4"), None);
    }

    #[test]
    fn the_version_names_libheif() {
        assert_eq!(expected_libheif(), Some([1, 23, 7]));
        assert!(VERSION.starts_with("1.23.7-grape."));
    }

    #[test]
    fn the_banner_has_the_gates_shape() {
        let c = Components {
            libheif: "1.23.7".to_string(),
            libde265: Some("1.1.3".to_string()),
            x265: Some("4.3".to_string()),
            plugins_disabled: true,
        };
        let text = banner(&c);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        // The gate's /^grape-heif version \S*-grape(?:\.\d+)?(?:\s|$)/.
        let rest = lines[0].strip_prefix("grape-heif version ").unwrap_or("");
        let version = rest.split(' ').next().unwrap_or("");
        let (_, build) = version.split_once("-grape").unwrap_or(("", "x"));
        assert!(
            build.is_empty()
                || build
                    .strip_prefix('.')
                    .is_some_and(|n| n.parse::<u32>().is_ok())
        );
        assert!(lines[0].ends_with(" Copyright (c) 2026 the Grape contributors"));
        let crt = if cfg!(target_feature = "crt-static") {
            "static"
        } else {
            "dynamic"
        };
        assert_eq!(
            lines[1],
            format!("configuration: libheif=1.23.7 libde265=1.1.3 x265=4.3 plugins=disabled crt={crt} protocol=2")
        );
        assert!(lines[2].starts_with("license: GPL-3.0-or-later "));
        assert_eq!(c.codec_name(), "libheif 1.23.7 (libde265 1.1.3, x265 4.3)");
    }

    /// The real build: what CI's banner check also reads.
    #[test]
    fn the_linked_libraries_are_the_pinned_ones() {
        let lib = LibHeif::new_checked().ok();
        let c = Components::read(lib.as_ref());
        assert_eq!(c.libheif, "1.23.7");
        assert_eq!(c.libde265.as_deref(), Some("1.1.3"));
        assert_eq!(c.x265.as_deref(), Some("4.3"));
        assert!(
            c.plugins_disabled,
            "libheif was built with plugin loading on"
        );
    }
}
