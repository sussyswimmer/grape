//! What Grape and its image helper say to each other (DECISIONS D33, D41).
//!
//! Grape converts HEIC and AVIF ([`HeifFormat`]) in a separate process:
//! `grape-heif.exe` (libheif) on Windows, and `Grape --grape-heif-worker`
//! (ImageIO) on macOS. Both speak this protocol, version [`PROTOCOL`]:
//!
//! - The parent writes one [`Request`] as JSON to the helper's stdin and
//!   closes it.
//! - The helper answers with JSON lines on stdout, one [`Message`] each:
//!   `image` (before any pixel is written), `progress`, then exactly one of
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

mod format;
mod message;
mod pixels;
mod request;
mod serve;
mod sniff;

pub use format::HeifFormat;
pub use message::{Caps, Done, FailKind, Failure, FormatCaps, ImageInfo, Message};
pub use pixels::{
    read_pixels, PixelError, PixelHeader, PixelLimits, PixelWriter, Pixels, HEADER_BYTES, MAGIC,
};
pub use request::{Limits, Op, Request};
pub use serve::{serve, worker_main, Codec, PixelSink};
pub use sniff::{sniff, Sniff};

/// The protocol version. Any change to the wire types or the pixel file
/// bumps it, and `tools/grape-heif` is rebuilt in the same commit.
///
/// - 1: HEIC only (D33).
/// - 2: `encode` names its [`HeifFormat`], [`Caps`] answers per format,
///   AVIF writes `out.avif`, and [`Sniff::Avif`] (D41).
pub const PROTOCOL: u32 = 2;

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

/// 2 GiB: the most memory a helper may use. Not on the wire: the parent
/// gives it to the helper's container (the Job Object on Windows), and the
/// Mac worker bounds its own heap with it (D41).
pub const WORKER_HEAP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The files in the request's work folder. Each is created new
/// (`create_new`) by the side that writes it.
pub mod names {
    use crate::HeifFormat;

    /// Decode output, written by the helper: a raw pixel file.
    pub const PIXELS: &str = "pixels.rgba";
    /// Encode input, written by the parent: a raw pixel file.
    pub const INPUT: &str = "input.rgba";
    /// HEIC encode output, written by the helper.
    pub const OUTPUT_HEIC: &str = "out.heic";
    /// AVIF encode output, written by the helper.
    pub const OUTPUT_AVIF: &str = "out.avif";

    /// The encode output for `format`: [`OUTPUT_HEIC`] or [`OUTPUT_AVIF`].
    pub const fn output(format: HeifFormat) -> &'static str {
        match format {
            HeifFormat::Heic => OUTPUT_HEIC,
            HeifFormat::Avif => OUTPUT_AVIF,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_format_has_its_own_output() {
        assert_eq!(names::output(HeifFormat::Heic), "out.heic");
        assert_eq!(names::output(HeifFormat::Avif), "out.avif");
        assert_eq!(PROTOCOL, 2);
        assert_eq!(WORKER_HEAP_BYTES, 1 << 31);
    }
}
