//! A door for the `pdf-it` integration tests (`tests/pdf.rs`) only: runs
//! a PDF job with the worker and the PDFium library they name. Built only
//! with the `pdf-it` feature, and not part of the crate's interface.

use std::path::{Path, PathBuf};

use grape_core::Job;

use super::protocol::MetaKey;
use super::run::{run_with, Launch, Limits, PdfPaths};
use crate::{EngineCtx, GrapeError};

/// What a PDF job wrote and reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The files or the folder written.
    pub outputs: Vec<PathBuf>,
    /// True when the planned name was taken and a " N" name was used.
    pub renamed: bool,
    /// Pages written or in the output.
    pub pages: u32,
    /// Pages (from 1) drawn below 300 DPI.
    pub scaled_pages: Vec<u32>,
    /// Remove Metadata: what was removed ("Title", …, "XMP").
    pub removed: Vec<String>,
    /// Split and merge: PDFium's page import did the job.
    pub used_pdfium: bool,
}

/// The bounds a test may lower; the defaults are Grape's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The most pixels one rendered page may have.
    pub max_page_pixels: u64,
    /// The most pages a render exports.
    pub max_pages: u32,
    /// The most pages Split PDF makes files of.
    pub max_split_pages: u32,
    /// The most bytes the worker's heap may hold.
    pub max_heap_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        let bounds = Limits::default().bounds;
        Options {
            max_page_pixels: bounds.max_page_pixels,
            max_pages: bounds.max_pages,
            max_split_pages: bounds.max_split_pages,
            max_heap_bytes: bounds.max_heap_bytes,
        }
    }
}

/// Runs `job` as `run_job` will, with `worker` started as
/// `<worker> --grape-pdf-worker` and `pdfium` as the library.
pub fn run(
    job: &Job,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    worker: &Path,
    pdfium: &Path,
    options: Options,
) -> Result<Outcome, GrapeError> {
    let paths = PdfPaths {
        worker: worker.to_path_buf(),
        pdfium: pdfium.to_path_buf(),
    };
    let mut limits = Limits::default();
    limits.bounds.max_page_pixels = options.max_page_pixels;
    limits.bounds.max_pages = options.max_pages;
    limits.bounds.max_split_pages = options.max_split_pages;
    limits.bounds.max_heap_bytes = options.max_heap_bytes;
    let outcome = run_with(job, ctx, progress, &Launch::worker(&paths), pdfium, &limits)?;
    Ok(Outcome {
        outputs: outcome.outcome.outputs,
        renamed: outcome.outcome.renamed,
        pages: outcome.pages,
        scaled_pages: outcome.scaled_pages,
        removed: outcome
            .removed
            .iter()
            .map(|key| key_name(*key).to_string())
            .collect(),
        used_pdfium: outcome.used_pdfium,
    })
}

fn key_name(key: MetaKey) -> &'static str {
    match key {
        MetaKey::Title => "Title",
        MetaKey::Author => "Author",
        MetaKey::Subject => "Subject",
        MetaKey::Keywords => "Keywords",
        MetaKey::Creator => "Creator",
        MetaKey::Producer => "Producer",
        MetaKey::CreationDate => "CreationDate",
        MetaKey::ModDate => "ModDate",
        MetaKey::Other => "Other",
        MetaKey::Xmp => "XMP",
    }
}
