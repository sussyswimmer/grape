//! Shared helpers for the engine integration tests. Everything is generated
//! into a unique folder under `std::env::temp_dir()`; no binary fixtures.
#![allow(dead_code, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh folder under the system temp dir, removed (with its contents)
/// when dropped.
pub struct TestDir {
    pub path: PathBuf,
}

impl TestDir {
    pub fn new(tag: &str) -> TestDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "grape-engines-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        TestDir { path }
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// The names in this folder, sorted.
    pub fn names(&self) -> Vec<String> {
        names_in(&self.path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The names in `dir`, sorted.
pub fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Panics if a temp or probe file was left in `dir`.
pub fn assert_no_leftovers(dir: &Path) {
    let leftovers: Vec<String> = names_in(dir)
        .into_iter()
        .filter(|name| name.starts_with(".grape-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "left in {}: {leftovers:?}",
        dir.display()
    );
}
