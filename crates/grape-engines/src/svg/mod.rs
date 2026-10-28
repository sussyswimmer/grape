//! SVG input (DECISIONS D39), drawn by resvg in Grape's SVG worker, a
//! separate process. Lane A's module, by agreement with lane B, who owns
//! this crate.
//!
//! - **Every SVG is drawn in the worker,** `Grape --grape-svg-worker`
//!   ([`worker_main`]): resvg and usvg have no cancel, progress or budget
//!   hook, their time and memory aren't bounded by the file's size, and a
//!   stack overflow or a failed allocation ends the process it runs in.
//!   The worker holds itself to the request's limits and a heap bound
//!   ([`crate::pdf::heap`]); a kill ends the rest.
//! - **References are `data:` only** (`resolve`): no file is ever read for
//!   an `<image>` or `<feImage>` href, and embedded data is held to a
//!   job-wide budget. What was left out is counted.
//! - **Text** is drawn with the system's fonts, loaded only when the
//!   document can draw text.
//! - **The size rule** (`size`): the SVG's own size, twice when it's under
//!   256 px, or 300 DPI for a PDF page, fitted into the target's caps.
//! - **The protocol** (`protocol`): one JSON request on stdin, JSON lines
//!   on stdout, the pixels in `pixels.rgba` in a private work folder.

mod protocol;
mod resolve;
mod size;
mod worker;

pub use worker::worker_main;

/// The first argument that turns Grape's executable into the SVG worker:
/// `Grape --grape-svg-worker`. The app checks for it before anything else
/// starts and then calls [`worker_main`].
pub const WORKER_FLAG: &str = "--grape-svg-worker";
