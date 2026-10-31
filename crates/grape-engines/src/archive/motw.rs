//! Mark of the Web on Windows: a downloaded archive carries a
//! `Zone.Identifier` alternate data stream, and Explorer copies it to every
//! file it extracts, so SmartScreen and Office's Protected View still know
//! where the content came from. Grape does the same for the files it
//! extracts and for a converted archive (DECISIONS D27). It is plain std
//! file I/O on the `{path}:Zone.Identifier` name.
//!
//! macOS's `com.apple.quarantine` needs xattr calls, which belong in
//! `grape-platform` (this crate forbids `unsafe`); that comes later.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The name of `path`'s `Zone.Identifier` stream.
pub(crate) fn zone_stream(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(":Zone.Identifier");
    PathBuf::from(name)
}

/// The source's mark, if it has one. Always none off Windows.
#[derive(Debug, Clone, Default)]
pub(crate) struct Mark(Option<Vec<u8>>);

/// The most bytes of a mark Grape copies. Real ones are about 100 bytes.
#[cfg(windows)]
const MAX_MARK: u64 = 64 * 1024;

impl Mark {
    /// Reads `src`'s `Zone.Identifier` stream. Any error (no stream, a
    /// volume without streams) means no mark; so does one over `MAX_MARK`.
    #[cfg(windows)]
    pub(crate) fn of(src: &Path) -> Mark {
        use std::io::Read;
        let Ok(file) = std::fs::File::open(zone_stream(src)) else {
            return Mark(None);
        };
        let mut bytes = Vec::new();
        match file.take(MAX_MARK + 1).read_to_end(&mut bytes) {
            Ok(n) if n as u64 <= MAX_MARK => Mark(Some(bytes)),
            _ => Mark(None),
        }
    }

    /// Off Windows there is no mark to read.
    #[cfg(not(windows))]
    pub(crate) fn of(_src: &Path) -> Mark {
        Mark(None)
    }

    /// A mark of these bytes, for tests on every OS.
    #[cfg(test)]
    pub(crate) fn with(bytes: &[u8]) -> Mark {
        Mark(Some(bytes.to_vec()))
    }

    /// Writes the mark onto `path`, a file Grape just wrote. A failure (a
    /// FAT32 or exFAT volume has no streams) is ignored: the file is fine
    /// without it. Off Windows there is never a mark ([`Mark::of`]), so
    /// this does nothing there outside tests (where a "stream" is a file
    /// next to `path`).
    pub(crate) fn apply(&self, path: &Path) {
        if let Some(bytes) = &self.0 {
            let _ = std::fs::write(zone_stream(path), bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_stream_after_the_whole_path() {
        assert_eq!(
            zone_stream(Path::new("dir/a b.zip")),
            PathBuf::from("dir/a b.zip:Zone.Identifier")
        );
        assert_eq!(
            zone_stream(Path::new(r"C:\Users\x\Downloads\f.tar.gz")),
            PathBuf::from(r"C:\Users\x\Downloads\f.tar.gz:Zone.Identifier")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn there_is_no_mark_off_windows() {
        let mark = Mark::of(Path::new("/nonexistent"));
        assert!(mark.0.is_none());
        mark.apply(Path::new("/nonexistent"));
    }
}
