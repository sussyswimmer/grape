//! Audio and video conversions with the bundled FFmpeg sidecar: the pure
//! half, which builds every argv and reads `ffprobe`'s answer.
//!
//! - [`probe`] asks `ffprobe` for the container, the duration (for
//!   progress) and the few stream fields the planner needs, and parses its
//!   `-of flat` output into a [`probe::Probe`].
//! - [`args`] turns the probe and the target format into the `ffmpeg`
//!   passes: one per conversion, two for GIF (palette, then the GIF).
//!
//! **Offline and contained.** Both tools run with `-protocol_whitelist
//! file` and `-format_whitelist` [`args::DEMUXERS`], and every path goes in
//! as a `file:` URL ([`args::file_url`]). So a source can't make FFmpeg
//! reach the network or read other files (an HLS, DASH or `ffconcat`
//! playlist renamed to `.mp4` is refused), and no file name is ever read as
//! an option or a protocol.
//!
//! **Only what the user sees.** A conversion keeps the first video stream
//! that isn't cover art and the first audio stream. Subtitles, data
//! streams, further audio tracks and cover art are dropped.
//!
//! The source is only ever an input; the output is always the temp path the
//! caller passes, never the planned final name.

pub(crate) mod args;
pub(crate) mod probe;
