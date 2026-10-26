//! Grape's own subtitle engine: SRT ⇄ WebVTT, and either to plain text
//! (`docs/ENGINES.md`). Everything here is a pure function over bytes or
//! text, so every rule is unit-tested in its file; reading the source and
//! publishing the output are the convert step's job.
//!
//! 1. [`decode`] turns the file's bytes into text: a BOM picks UTF-8 or
//!    UTF-16; without one, valid UTF-8 is UTF-8 and anything else is
//!    Windows-1252.
//! 2. [`parse`] reads the cues leniently, as SRT or WebVTT by content (not
//!    by file name), and normalizes them: sorted by start, never
//!    zero-length, no blank lines.
//! 3. [`write`] writes WebVTT, SRT or plain text with the source's line
//!    endings, cues renumbered from 1, and italic, bold and underline tags
//!    balanced per cue.

pub(crate) mod decode;
pub(crate) mod parse;
pub(crate) mod write;
