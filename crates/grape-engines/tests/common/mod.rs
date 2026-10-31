//! Shared helpers for the engine integration tests. Everything is generated
//! into a unique folder under `std::env::temp_dir()`; no binary fixtures.
#![allow(dead_code, clippy::unwrap_used)]

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::Hasher;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use grape_core::{plan, Job, JobOutput, Operation, Settings};
use grape_engines::build_selection;

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

/// The source's length, content hash and mtime, to prove it was never
/// touched.
pub struct SourceState {
    pub path: PathBuf,
    pub content: (u64, u64),
    pub modified: SystemTime,
}

/// The length and a hash of a file's bytes, read in chunks so a large
/// source isn't held in memory.
pub fn content_digest(path: &Path) -> (u64, u64) {
    let mut file = File::open(path).unwrap();
    let mut hasher = DefaultHasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf).unwrap();
        if n == 0 {
            return (len, hasher.finish());
        }
        hasher.write(&buf[..n]);
        len += n as u64;
    }
}

impl SourceState {
    pub fn of(path: &Path) -> SourceState {
        SourceState {
            path: path.to_path_buf(),
            content: content_digest(path),
            modified: fs::metadata(path).unwrap().modified().unwrap(),
        }
    }

    pub fn assert_unchanged(&self) {
        assert_eq!(
            content_digest(&self.path),
            self.content,
            "source bytes changed: {}",
            self.path.display()
        );
        assert_eq!(
            fs::metadata(&self.path).unwrap().modified().unwrap(),
            self.modified,
            "source mtime changed: {}",
            self.path.display()
        );
    }
}

/// Writes `bytes` as `name` in `dir` and plans `op` for it the way the app
/// does (`build_selection`, outputs next to the source, fallback
/// `dir/fallback`). Returns the source and the job.
pub fn plan_one(dir: &TestDir, name: &str, bytes: &[u8], op: Operation) -> (PathBuf, Job) {
    let src = dir.join(name);
    fs::write(&src, bytes).unwrap();
    let settings = Settings::new(dir.join("fallback"));
    let sel = build_selection(std::slice::from_ref(&src), &settings);
    let mut plan = plan(&sel, &op, &settings).unwrap();
    assert_eq!(plan.jobs.len(), 1);
    (src, plan.jobs.remove(0))
}

pub fn planned_file(job: &Job) -> PathBuf {
    match &job.output {
        JobOutput::File(path) => path.clone(),
        other => panic!("expected a file output, got {other:?}"),
    }
}
