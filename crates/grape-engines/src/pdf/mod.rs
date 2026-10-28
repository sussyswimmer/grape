//! Grape's PDF engine, slice 1 (DECISIONS D29): PDF → JPG, PNG, TXT and
//! MD, and the Split PDF, Merge PDFs and Remove Metadata tools. And images
//! → PDF with the Create PDF tool (D37), which parse no PDF and so run in
//! Grape's own process ([`create`]).
//!
//! - **The PDF work runs in a worker process.** PDFium (C++, through
//!   `pdfium-render`) and lopdf parse untrusted files. A fault in either one
//!   ends the process it runs in, and PDFium's bindings are process-global.
//!   So each job starts `<worker> --grape-pdf-worker` ([`worker_main`]),
//!   writes one JSON request to its stdin and reads JSON lines from its
//!   stdout ([`protocol`]). The worker writes only into a hidden temp folder
//!   the parent made. The parent ([`run`]) kills it on a cancel or a
//!   timeout, then publishes the result without replacing anything and
//!   removes the temp folder on every path.
//! - **PDFium** renders pages ([`render`]) and extracts text ([`text`]). It
//!   is bound from the absolute path in the request, never searched for.
//! - **lopdf** runs the tools: Grape's own page copier ([`copy`]) for split
//!   and merge, and [`meta`] for Remove Metadata. Split and merge fall back
//!   to PDFium's page import when lopdf can't read a file.
//! - The worker bounds its own heap ([`heap`]): its executables install a
//!   global allocator that calls [`heap::reserve`] and [`heap::release`].
//! - Only the worker's entry point and the heap bookkeeping are public (the
//!   app and the test binary use them). Everything else is crate-private.

mod copy;
pub(crate) mod create;
mod flate;
pub mod heap;
mod jpeg;
mod meta;
mod protocol;
mod render;
mod run;
mod text;
mod worker;
mod writer;

#[cfg(feature = "pdf-it")]
pub mod it;

pub use worker::worker_main;

// What `run_job`'s dispatch calls (D29).
pub(crate) use run::{run, runs_tool, supports, PdfPaths};

/// The first argument that turns Grape's executable into the PDF worker:
/// `Grape --grape-pdf-worker`. The app checks for it before anything else
/// starts and then calls [`worker_main`].
pub const WORKER_FLAG: &str = "--grape-pdf-worker";
