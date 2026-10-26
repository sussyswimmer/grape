//! What Grape and its HEIC helper say to each other (DECISIONS D33).
//!
//! Grape converts HEIC in a separate process: `grape-heif.exe` (libheif) on
//! Windows, and `Grape --grape-heif-worker` (ImageIO) on macOS. Both speak
//! this protocol, version [`PROTOCOL`]:
//!
//! - The parent writes one [`Request`] as JSON to the helper's stdin and
//!   closes it.
//! - The helper answers with JSON lines on stdout, one [`Message`] each:
//!   `image` (before any pixel is decoded), `progress`, then exactly one of
//!   `done` (exit code 0) or `error` (exit code 1).
//! - Pixels never go through the pipe. They go through raw pixel files
//!   ([`PixelWriter`], [`read_pixels`]) in the request's work folder, under
//!   the fixed [`names`].
//! - No message holds a path, a file name or file contents, and both sides
//!   refuse unknown fields.
//!
//! The wire types, [`sniff`] and the pixel file do no I/O of their own. The
//! worker side, [`serve`] and [`worker_main`], uses `std` only: it reads
//! stdin, writes new files in the work folder and runs the job on a thread.
//! The codec itself comes in through the [`Codec`] trait, so this crate has
//! no `unsafe` and no OS-specific dependency.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod message;
mod pixels;
mod request;
mod serve;
mod sniff;

pub use message::{Caps, Done, FailKind, Failure, ImageInfo, Message};
pub use pixels::{
    read_pixels, PixelError, PixelHeader, PixelLimits, PixelWriter, Pixels, HEADER_BYTES, MAGIC,
};
pub use request::{Limits, Op, Request};
pub use serve::{serve, worker_main, Codec, PixelSink};
pub use sniff::{sniff, Sniff};

/// The protocol version. Any change to the wire types or the pixel file
/// bumps it, and `tools/grape-heif` is rebuilt in the same commit.
pub const PROTOCOL: u32 = 1;

/// The argument that starts a helper: `grape-heif.exe --grape-heif-worker`
/// on Windows, `Grape --grape-heif-worker` on macOS.
pub const WORKER_FLAG: &str = "--grape-heif-worker";

/// The most bytes of one request; a longer one is refused.
pub const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// The longest line the parent reads from the helper. A longer one is not a
/// message and is skipped.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// How many bytes from the start of a file [`sniff`] looks at.
pub const SNIFF_BYTES: usize = 256;

/// The files in the request's work folder. Each is created new
/// (`create_new`) by the side that writes it.
pub mod names {
    /// Decode output, written by the helper: a raw pixel file.
    pub const PIXELS: &str = "pixels.rgba";
    /// Encode input, written by the parent: a raw pixel file.
    pub const INPUT: &str = "input.rgba";
    /// Encode output, written by the helper: the HEIC file.
    pub const OUTPUT: &str = "out.heic";
}
