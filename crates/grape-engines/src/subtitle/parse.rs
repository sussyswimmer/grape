//! Subtitle text to cues, for SRT and WebVTT alike. Real files are messy,
//! so the reader is lenient and only a file without one usable cue fails.
//!
//! - **Lines** split at `\r\n`, `\n` or a lone `\r`, and lose their
//!   trailing whitespace. `\r\r\n` is one break too, never a blank line:
//!   it is what a CRLF file's line ends become when converted again. The
//!   output keeps the first line break's kind (`\r\r\n` counts as `\r\n`).
//!   A text of more than [`MAX_LINES`] lines, or with a line longer than
//!   [`MAX_LINE_BYTES`], is too large. A line that a WebVTT `&#10;` or
//!   `&#13;` breaks counts once for each cue line it makes.
//! - **WebVTT** is recognized by content: the text starts with `WEBVTT`
//!   alone or followed by whitespace. Its header (up to the first blank
//!   line, or up to a line with `-->`) is skipped, and so are its `NOTE`,
//!   `STYLE` and `REGION` blocks, up to their blank line or to a line with
//!   `-->`, which starts a cue.
//! - **Cues** start at every valid timing line (`<ts> --> <ts>`, cue
//!   settings and SRT coordinates dropped), even without a blank line
//!   before it. A line of digits just before one is its index, not text;
//!   after a blank line, everything before the timing line is the cue's
//!   identifier. A line with `-->` that isn't a valid timing line drops its
//!   cue; in SRT, only one whose text before the arrow starts with a digit
//!   (after spaces, invisible marks and one `-`) is taken for a timing line,
//!   so an arrow in SRT text stays text. A block
//!   without a timing line continues the previous cue's text, unless it is
//!   only digits and that cue already has text: then it is a stray index.
//! - **Text** keeps italic, bold and underline from HTML-style, WebVTT and
//!   ASS tags; other known tags are dropped with their text kept, `<rt>`
//!   ruby text is put in parentheses, and unknown markup stays literal.
//!   Character references are decoded in WebVTT only (see [`cue_lines`]);
//!   SRT text is raw.
//! - **Normalization:** cues are sorted by start, a cue that doesn't end
//!   after it starts lasts 1 ms, and lines without visible text (and cues
//!   left without lines) are dropped.

use super::decode::CP1252_80_9F;
use super::entities;
use super::{Cancelled, Ticker};

/// The most lines a subtitle text may have, counting a line that a
/// WebVTT `&#10;` or `&#13;` breaks once for each cue line it makes. Real
/// files have tens of thousands. With [`MAX_LINE_BYTES`] this bounds the
/// lines, spans and text a parsed document holds.
pub(crate) const MAX_LINES: usize = 1_000_000;

/// The longest line a subtitle text may have, in bytes of UTF-8. Real
/// subtitle lines are under a few hundred bytes; this bounds the work
/// and memory one line takes.
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;

/// What separates the two timestamps of a timing line.
const ARROW: &str = "-->";

/// The line ending a document uses; outputs keep it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Newline {
    /// `\n` (also used for a text whose first line break is a lone `\r`).
    Lf,
    /// `\r\n`.
    CrLf,
}

impl Newline {
    /// The line ending itself.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Newline::Lf => "\n",
            Newline::CrLf => "\r\n",
        }
    }
}

/// A text style both SRT and WebVTT can express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    /// `<i>`.
    Italic,
    /// `<b>`.
    Bold,
    /// `<u>`.
    Underline,
}

/// A piece of one line of cue text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Span {
    /// Literal text, never containing a line break. Adjacent text is one span.
    Text(String),
    /// A style starts here.
    Open(Style),
    /// A style ends here. Tags as read may be unbalanced; writers balance them.
    Close(Style),
}

/// One subtitle: when it shows and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cue {
    /// When it appears, in milliseconds from the start.
    pub(crate) start_ms: u64,
    /// When it disappears; after `start_ms` once normalized.
    pub(crate) end_ms: u64,
    /// Its lines of text, each with some visible text once normalized.
    pub(crate) lines: Vec<Vec<Span>>,
}

/// A parsed, normalized subtitle file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Document {
    /// The cues, sorted by start time. Never empty.
    pub(crate) cues: Vec<Cue>,
    /// The source's line ending.
    pub(crate) newline: Newline,
    /// True when the source was WebVTT (by content, not by file name).
    pub(crate) webvtt: bool,
}

/// Why a text gave no document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseError {
    /// The job was cancelled.
    Cancelled,
    /// Not one cue had a valid timing line and visible text.
    NoCues,
    /// The text has more than [`MAX_LINES`] lines, or a line longer than
    /// [`MAX_LINE_BYTES`].
    TooLarge,
}

impl From<Cancelled> for ParseError {
    fn from(_: Cancelled) -> Self {
        ParseError::Cancelled
    }
}

/// Reads `text` (SRT or WebVTT) into a normalized [`Document`], as the
/// module docs describe. `is_cancelled` is asked before the first line,
/// as the work goes on and after the sort. Every line read, looked at or
/// turned into spans counts as work by its length (see [`Ticker::line`]),
/// and so does every cue, and every extra cue line a line break makes.
pub(crate) fn parse(
    text: &str,
    is_cancelled: &mut dyn FnMut() -> bool,
) -> Result<Document, ParseError> {
    // `decode` strips the BOM; a second one would hide the WEBVTT signature.
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let newline = newline(text);
    let webvtt = is_webvtt(text);

    let mut ticker = Ticker::new(is_cancelled);
    let mut reader = Reader {
        webvtt,
        cues: Vec::new(),
        continues: false,
        lines: 0,
    };
    // The lines since the last blank line. It is reused, so only the
    // longest block is ever held as lines.
    let mut block: Vec<&str> = Vec::new();
    // A WebVTT header runs to the first blank line, or to a line with
    // `-->`, which starts the first cue.
    let mut in_header = webvtt;
    for line in split_lines(text) {
        count_lines(&mut reader.lines, 1)?;
        if line.len() > MAX_LINE_BYTES {
            return Err(ParseError::TooLarge);
        }
        ticker.line(line)?;
        let line = line.trim_end();
        if in_header {
            if line.is_empty() || line.contains(ARROW) {
                in_header = false;
            } else {
                continue;
            }
        }
        if !line.is_empty() {
            block.push(line);
        } else if !block.is_empty() {
            reader.block(&block, &mut ticker)?;
            block.clear();
        }
    }
    if !block.is_empty() {
        reader.block(&block, &mut ticker)?;
    }
    let cues = normalize(reader.cues, &mut ticker)?;
    Ok(Document {
        cues,
        newline,
        webvtt,
    })
}

/// Adds `more` to `lines`, the lines counted toward [`MAX_LINES`]; past
/// it, the text is too large.
fn count_lines(lines: &mut usize, more: usize) -> Result<(), ParseError> {
    *lines = lines.saturating_add(more);
    if *lines > MAX_LINES {
        Err(ParseError::TooLarge)
    } else {
        Ok(())
    }
}

/// True when `line` has some text that isn't whitespace: what every line
/// of a normalized cue has.
pub(crate) fn has_text(line: &[Span]) -> bool {
    line.iter()
        .any(|span| matches!(span, Span::Text(text) if !text.trim().is_empty()))
}

/// `CrLf` if the first line break is `\r\n` or `\r\r\n`, else `Lf`.
fn newline(text: &str) -> Newline {
    match text.find(['\r', '\n']) {
        Some(at) if text[at..].starts_with("\r\n") || text[at..].starts_with("\r\r\n") => {
            Newline::CrLf
        }
        _ => Newline::Lf,
    }
}

/// The lines of `text`, split at `\r\n`, `\r\r\n`, `\n` or a lone `\r`. A
/// line break at the very end ends the last line; it doesn't start another.
fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_terminator('\n').flat_map(|line| {
        line.strip_suffix("\r\r")
            .or_else(|| line.strip_suffix('\r'))
            .unwrap_or(line)
            .split('\r')
    })
}

/// True for `WEBVTT` followed by the end of the text, a space, a tab or a
/// line break.
fn is_webvtt(text: &str) -> bool {
    text.strip_prefix("WEBVTT")
        .is_some_and(|rest| matches!(rest.chars().next(), None | Some(' ' | '\t' | '\r' | '\n')))
}

/// Reads blocks (runs of non-blank lines) into cues, their text already
/// in spans and without lines that have no visible text.
struct Reader {
    webvtt: bool,
    cues: Vec<Cue>,
    /// True when a block without a timing line continues the last cue:
    /// false before the first cue and after a dropped one, whose text that
    /// block would be.
    continues: bool,
    /// The lines counted toward [`MAX_LINES`] so far: every line of the
    /// text, and each extra cue line that a `&#10;` or `&#13;` makes.
    lines: usize,
}

impl Reader {
    /// Reads one block. Every line it looks at or turns into spans counts
    /// as work for `ticker`, by its length.
    fn block(&mut self, mut block: &[&str], ticker: &mut Ticker) -> Result<(), ParseError> {
        let webvtt = self.webvtt;
        let is_timing = |line: &str| looks_like_timing(line, webvtt);
        if self.webvtt && block.first().is_some_and(|line| starts_non_cue_block(line)) {
            // The comment, style or region ends at a timing line: a cue can
            // follow it without a blank line.
            match find(block, ticker, is_timing)? {
                None => return Ok(()),
                Some(at) => block = &block[at..],
            }
        }
        let Some(first) = find(block, ticker, is_timing)? else {
            // Text the blank line cut off from its cue, unless it is only
            // digits and the cue already has text: then it is a stray index.
            if !self.continues {
                return Ok(());
            }
            if let Some(cue) = self.cues.last_mut() {
                let stray_index = !cue.lines.is_empty()
                    && find(block, ticker, |line| !is_digits(line))?.is_none();
                if !stray_index {
                    push_lines(&mut cue.lines, block, webvtt, &mut self.lines, ticker)?;
                }
            }
            return Ok(());
        };
        // The lines before the first timing line are its cue's identifier.
        let mut rest = &block[first..];
        while let Some((timing, after)) = rest.split_first() {
            let next = find(after, ticker, is_timing)?.unwrap_or(after.len());
            let mut text = &after[..next];
            // A line of digits right before the next timing line is its index.
            if next < after.len() {
                if let Some((last, init)) = text.split_last() {
                    if is_digits(last) {
                        text = init;
                    }
                }
            }
            match timing_line(timing) {
                Some((start_ms, end_ms)) => {
                    // Most cues keep every line, so this is usually exact.
                    let mut lines = Vec::with_capacity(text.len());
                    push_lines(&mut lines, text, webvtt, &mut self.lines, ticker)?;
                    self.cues.push(Cue {
                        start_ms,
                        end_ms,
                        lines,
                    });
                    self.continues = true;
                }
                None => self.continues = false,
            }
            rest = &after[next..];
        }
        Ok(())
    }
}

/// The index of the first of `lines` that `pred` holds for; each line
/// looked at counts as work, by its length.
fn find(
    lines: &[&str],
    ticker: &mut Ticker,
    pred: impl Fn(&str) -> bool,
) -> Result<Option<usize>, Cancelled> {
    for (i, line) in lines.iter().enumerate() {
        ticker.line(line)?;
        if pred(line) {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// Appends `text`'s lines to a cue's `lines` as spans, leaving out those
/// without visible text. Each is shrunk to fit; most already are. A line
/// that makes more than one cue line adds the extra ones to `counted`,
/// and counts them as work.
fn push_lines(
    lines: &mut Vec<Vec<Span>>,
    text: &[&str],
    webvtt: bool,
    counted: &mut usize,
    ticker: &mut Ticker,
) -> Result<(), ParseError> {
    for line in text {
        ticker.line(line)?;
        let parts = cue_lines(line, webvtt);
        let extra = parts.len().saturating_sub(1);
        count_lines(counted, extra)?;
        ticker.work(extra)?;
        for mut line in parts {
            if has_text(&line) {
                line.shrink_to_fit();
                lines.push(line);
            }
        }
    }
    Ok(())
}

/// True for a line that is read as a timing line, valid or not: one with
/// `-->`. WebVTT text can't hold one, so there every such line counts. In
/// SRT, only one whose text before the arrow starts with a digit once
/// spaces, [invisible marks](is_invisible_mark) and one `-` are skipped:
/// an arrow in SRT text stays text, while a negative timing line (a
/// shifted SRT has them) or one after a stray BOM is still a timing line,
/// so its invalid timing drops its cue instead of joining the cue before.
fn looks_like_timing(line: &str, webvtt: bool) -> bool {
    line.split_once(ARROW).is_some_and(|(before, _)| {
        if webvtt {
            return true;
        }
        let before = before.trim_start_matches(|c: char| c.is_whitespace() || is_invisible_mark(c));
        before
            .strip_prefix('-')
            .unwrap_or(before)
            .starts_with(|c: char| c.is_ascii_digit())
    })
}

/// True for a character that shows nothing and that editors or joined
/// files leave at the start of a line: the BOM (U+FEFF), zero-width
/// spaces, joiners and direction marks (U+200B to U+200F), direction
/// embeddings and overrides (U+202A to U+202E) and the word joiner
/// (U+2060).
fn is_invisible_mark(c: char) -> bool {
    matches!(
        c,
        '\u{FEFF}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'
    )
}

/// True for the first line of a WebVTT comment, style or region block:
/// `NOTE`, `STYLE` or `REGION`, alone or followed by a space or tab.
fn starts_non_cue_block(line: &str) -> bool {
    ["NOTE", "STYLE", "REGION"].iter().any(|word| {
        line.strip_prefix(word)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    })
}

/// True for a non-empty line of ASCII digits only.
fn is_digits(line: &str) -> bool {
    !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit())
}

/// The start and end of a timing line, or `None` if it isn't one. Anything
/// after the end timestamp (cue settings, SRT coordinates) is ignored.
fn timing_line(line: &str) -> Option<(u64, u64)> {
    let (start, rest) = line.split_once(ARROW)?;
    let end = rest.split_whitespace().next()?;
    Some((timestamp(start.trim())?, timestamp(end)?))
}

/// Milliseconds from `[hours:]minutes:seconds[(,|.)fraction]`. Minutes and
/// seconds are one or two digits up to 59; hours any number of digits;
/// the fraction is decimal, read to the millisecond (",5" is 500 ms).
/// `None` for anything else, or for a time that overflows.
fn timestamp(text: &str) -> Option<u64> {
    let (clock, fraction) = match text.split_once([',', '.']) {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (text, None),
    };
    let mut parts = clock.rsplit(':');
    let seconds = sexagesimal(parts.next()?)?;
    let minutes = sexagesimal(parts.next()?)?;
    let hours = match parts.next() {
        Some(hours) => number(hours)?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    let millis = match fraction {
        Some(fraction) => millis(fraction)?,
        None => 0,
    };
    hours
        .checked_mul(60)?
        .checked_add(minutes)?
        .checked_mul(60)?
        .checked_add(seconds)?
        .checked_mul(1000)?
        .checked_add(millis)
}

/// One or more ASCII digits as a number; `None` for anything else or on
/// overflow.
fn number(digits: &str) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    digits.bytes().try_fold(0u64, |n, b| {
        if b.is_ascii_digit() {
            n.checked_mul(10)?.checked_add(u64::from(b - b'0'))
        } else {
            None
        }
    })
}

/// Minutes or seconds: one or two digits, at most 59.
fn sexagesimal(digits: &str) -> Option<u64> {
    if digits.len() > 2 {
        return None;
    }
    number(digits).filter(|&n| n <= 59)
}

/// A fraction of a second's digits in milliseconds: padded on the right
/// to three digits, and cut after three.
fn millis(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = digits.as_bytes();
    Some((0..3).fold(0, |ms, i| {
        ms * 10 + digits.get(i).map_or(0, |&d| u64::from(d - b'0'))
    }))
}

/// Normalizes the cues as read: sorted by start (stable), each lasting at
/// least 1 ms, and without cues that have no line of visible text. Each cue
/// counts as work, and so does sorting them (see [`sort_by_start`]).
fn normalize(mut cues: Vec<Cue>, ticker: &mut Ticker) -> Result<Vec<Cue>, ParseError> {
    for cue in &mut cues {
        ticker.tick()?;
        if cue.end_ms <= cue.start_ms {
            cue.end_ms = cue.start_ms.saturating_add(1);
        }
        cue.lines.shrink_to_fit();
    }
    cues.retain(|cue| !cue.lines.is_empty());
    sort_by_start(&mut cues, ticker)?;
    if cues.is_empty() {
        Err(ParseError::NoCues)
    } else {
        Ok(cues)
    }
}

/// Sorts `cues` by start, keeping the file order of cues that start
/// together. `ticker` checks before the sort, which most files (already in
/// order) end at, and after sorting (start, index) keys, the one step it
/// can't check inside: for a million cues in random order that takes
/// about 35 ms. Moving the cues into place then counts as work.
fn sort_by_start(cues: &mut [Cue], ticker: &mut Ticker) -> Result<(), Cancelled> {
    ticker.check()?;
    if cues.is_sorted_by_key(|cue| cue.start_ms) {
        return Ok(());
    }
    let mut order: Vec<(u64, usize)> = cues
        .iter()
        .enumerate()
        .map(|(i, cue)| (cue.start_ms, i))
        .collect();
    // The keys are all different, so an unstable sort keeps ties in order.
    order.sort_unstable();
    ticker.check()?;
    // order[i].1 is where the cue that belongs at i is now. Follow each
    // cycle of that permutation, a swap putting one cue in its place, and
    // mark each place done by pointing it at itself.
    for start in 0..order.len() {
        let mut at = start;
        loop {
            ticker.tick()?;
            let from = order[at].1;
            order[at].1 = at;
            if from == start {
                break;
            }
            cues.swap(at, from);
            at = from;
        }
    }
    Ok(())
}

/// The cue lines one line of text reads as, in spans: one, or more where a
/// WebVTT `&#10;` or `&#13;` breaks it. Only these are markup (tag names
/// in any case); everything else is literal text:
/// - `<i>`, `<b>`, `<u>` (also with WebVTT classes, `<i.loud>`) and their
///   end tags, and ASS `{\i1}`/`{\i0}` and the same for `b` and `u`, open
///   and close styles. Other ASS overrides (`{\an8}`, `{\pos(10,20)}`) are
///   dropped.
/// - `<font …>`, WebVTT `<c…>`, `<v…>`, `<lang…>`, `<ruby>` and their end
///   tags, and inline timestamps (`<00:00:01.000>`), are dropped and their
///   text kept. `<rt>` and `</rt>` become "(" and ")", so ruby text reads
///   "漢(かん)"; an `<rt>` still open at `</ruby>` or at the end of the line
///   gets its ")".
/// - In WebVTT only, character references: by name from [`entities`]
///   (case-sensitive, so `&Eacute;` is É and `&EACUTE;` stays text), or by
///   number (`&#233;`, `&#xE9;`). As in HTML, a name of HTML 3.2 (see
///   [`entities::legacy`]) needs no `;` when what follows it isn't a letter
///   or digit: `&copy 2024` is "© 2024", `&copy2024` stays text. `&#128;`
///   to `&#159;` are read as Windows-1252, as HTML does (see [`numeric`]).
///   `&#10;` and `&#13;` break the line, as a line of cue text can't hold
///   one; `&#0;` stays text.
fn cue_lines(line: &str, webvtt: bool) -> Vec<Vec<Span>> {
    let mut out = Spans::new();
    let mut rest = line;
    loop {
        let special = if webvtt {
            rest.find(['<', '{', '&'])
        } else {
            rest.find(['<', '{'])
        };
        let Some(at) = special else {
            out.text(rest);
            break;
        };
        out.text(&rest[..at]);
        rest = &rest[at..];
        let markup = if rest.starts_with('<') {
            tag(rest, &mut out)
        } else if rest.starts_with('{') {
            overrides(rest, &mut out)
        } else {
            reference(rest).map(|(len, reference)| {
                match reference {
                    Reference::Char(c) => out.text(c.encode_utf8(&mut [0; 4])),
                    Reference::LineBreak => out.line_break(),
                }
                len
            })
        };
        // Not markup: the character itself is text. All three are one byte.
        let len = markup.unwrap_or_else(|| {
            out.text(&rest[..1]);
            1
        });
        rest = &rest[len..];
    }
    out.line_break();
    out.lines
}

/// The spans of one line of text as they are built.
struct Spans {
    /// The cue lines already ended by a line break.
    lines: Vec<Vec<Span>>,
    /// The cue line being built.
    spans: Vec<Span>,
    /// Inside `<rt>`, whose ")" is still to come.
    ruby_text: bool,
}

impl Spans {
    /// Room for one line of one span, which is what most lines are, so
    /// they need no shrinking.
    fn new() -> Spans {
        Spans {
            lines: Vec::with_capacity(1),
            spans: Vec::with_capacity(1),
            ruby_text: false,
        }
    }

    /// Ends the cue line being built, closing an `<rt>` still open.
    fn line_break(&mut self) {
        if self.ruby_text {
            self.text(")");
            self.ruby_text = false;
        }
        self.lines.push(std::mem::take(&mut self.spans));
    }

    /// Appends text, merged into the text before it.
    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some(Span::Text(last)) => last.push_str(text),
            _ => self.spans.push(Span::Text(text.to_owned())),
        }
    }
}

/// What a recognized `<…>` tag does.
enum Tag {
    Open(Style),
    Close(Style),
    RubyTextStart,
    RubyTextEnd,
    RubyEnd,
    Drop,
}

/// Applies the tag at the start of `rest` and returns its length, or
/// `None` if it isn't a recognized tag (no `>` before the next `<`).
fn tag(rest: &str, out: &mut Spans) -> Option<usize> {
    let body = &rest[1..];
    let end = body.find(['<', '>'])?;
    if !body[end..].starts_with('>') {
        return None;
    }
    match classify(&body[..end])? {
        Tag::Open(style) => out.spans.push(Span::Open(style)),
        Tag::Close(style) => out.spans.push(Span::Close(style)),
        Tag::RubyTextStart => {
            if !out.ruby_text {
                out.text("(");
                out.ruby_text = true;
            }
        }
        Tag::RubyTextEnd | Tag::RubyEnd => {
            if out.ruby_text {
                out.text(")");
                out.ruby_text = false;
            }
        }
        Tag::Drop => {}
    }
    Some(end + 2)
}

/// What the tag with `content` between its `<` and `>` does, if Grape
/// knows it. A start tag's name may be followed by WebVTT classes
/// (`.loud`) or by whitespace and an annotation (`<v Roger>`).
fn classify(content: &str) -> Option<Tag> {
    if timestamp(content).is_some() {
        return Some(Tag::Drop);
    }
    if let Some(name) = content.strip_prefix('/') {
        let name = name.trim_end_matches(|c: char| c.is_ascii_whitespace());
        return match name.to_ascii_lowercase().as_str() {
            "i" => Some(Tag::Close(Style::Italic)),
            "b" => Some(Tag::Close(Style::Bold)),
            "u" => Some(Tag::Close(Style::Underline)),
            "rt" => Some(Tag::RubyTextEnd),
            "ruby" => Some(Tag::RubyEnd),
            "c" | "v" | "lang" | "font" => Some(Tag::Drop),
            _ => None,
        };
    }
    let name_len = content
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(content.len());
    let (name, rest) = content.split_at(name_len);
    if !(rest.is_empty()
        || rest.starts_with('.')
        || rest.starts_with(|c: char| c.is_ascii_whitespace()))
    {
        return None;
    }
    match name.to_ascii_lowercase().as_str() {
        "i" => Some(Tag::Open(Style::Italic)),
        "b" => Some(Tag::Open(Style::Bold)),
        "u" => Some(Tag::Open(Style::Underline)),
        "rt" => Some(Tag::RubyTextStart),
        "c" | "v" | "lang" | "ruby" | "font" => Some(Tag::Drop),
        _ => None,
    }
}

/// Applies the ASS override block (`{\…}`) at the start of `rest` and
/// returns its length, or `None` if it isn't one (no `}` before the next
/// `{`). Each override in it that sets italic, bold or underline on or off
/// becomes a span; the others are dropped.
fn overrides(rest: &str, out: &mut Spans) -> Option<usize> {
    let body = rest.strip_prefix("{\\")?;
    let end = body.find(['{', '}'])?;
    if !body[end..].starts_with('}') {
        return None;
    }
    for code in body[..end].split('\\') {
        let span = match code.trim().to_ascii_lowercase().as_str() {
            "i1" => Span::Open(Style::Italic),
            "i0" => Span::Close(Style::Italic),
            "b1" => Span::Open(Style::Bold),
            "b0" => Span::Close(Style::Bold),
            "u1" => Span::Open(Style::Underline),
            "u0" => Span::Close(Style::Underline),
            _ => continue,
        };
        out.spans.push(span);
    }
    Some(end + 3)
}

/// What a character reference stands for.
#[derive(Debug, PartialEq, Eq)]
enum Reference {
    Char(char),
    /// `&#10;` or `&#13;`.
    LineBreak,
}

/// The WebVTT character reference at the start of `rest` (at its `&`):
/// its length and what it stands for, or `None` if it isn't one Grape
/// decodes.
fn reference(rest: &str) -> Option<(usize, Reference)> {
    let body = &rest[1..];
    let (number, body) = match body.strip_prefix('#') {
        Some(digits) => (true, digits),
        None => (false, body),
    };
    let len = body
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(body.len());
    let (name, semicolon) = (&body[..len], body[len..].starts_with(';'));
    if number {
        if !semicolon {
            return None;
        }
        // `&`, `#`, the digits and `;`.
        return Some((len + 3, numeric(name)?));
    }
    if semicolon {
        Some((len + 2, Reference::Char(entities::named(name)?)))
    } else {
        // What follows the name isn't a letter or digit, or it would be
        // part of the name.
        Some((len + 1, Reference::Char(entities::legacy(name)?)))
    }
}

/// What the digits of a numeric reference (`65`, `x41` or `X41`) stand
/// for. As in HTML, 128 to 159 are the Windows-1252 bytes a page meant:
/// `&#150;` is "–", and the five bytes Windows-1252 leaves undefined stay
/// C1 controls. `None` for a number that isn't a Unicode scalar value, and
/// for NUL.
fn numeric(digits: &str) -> Option<Reference> {
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => in_radix(hex, 16),
        None => in_radix(digits, 10),
    }?;
    let windows_1252 = code
        .checked_sub(0x80)
        .and_then(|i| CP1252_80_9F.get(usize::try_from(i).ok()?));
    let c = match windows_1252 {
        Some(&c) => c,
        None => char::from_u32(code)?,
    };
    match c {
        '\0' => None,
        '\n' | '\r' => Some(Reference::LineBreak),
        c => Some(Reference::Char(c)),
    }
}

/// One or more digits in `radix` as a number; `None` otherwise or on
/// overflow.
fn in_radix(digits: &str, radix: u32) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    digits.chars().try_fold(0u32, |n, c| {
        n.checked_mul(radix)?.checked_add(c.to_digit(radix)?)
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::CHECK_EVERY;
    use super::*;

    fn parsed(text: &str) -> Document {
        parse(text, &mut || false).unwrap()
    }

    /// A check that finds the job cancelled from its second call on.
    fn cancelled_after_first() -> impl FnMut() -> bool {
        let mut calls = 0;
        move || {
            calls += 1;
            calls > 1
        }
    }

    fn text(s: &str) -> Span {
        Span::Text(s.to_owned())
    }

    /// The spans of a line of text that reads as one cue line.
    fn spans(line: &str, webvtt: bool) -> Vec<Span> {
        let mut lines = cue_lines(line, webvtt);
        assert_eq!(lines.len(), 1, "{line}");
        lines.remove(0)
    }

    /// Each cue as (start, end, its lines' text with tags left out).
    fn plain(doc: &Document) -> Vec<(u64, u64, Vec<String>)> {
        doc.cues
            .iter()
            .map(|cue| {
                let lines = cue
                    .lines
                    .iter()
                    .map(|line| {
                        line.iter()
                            .filter_map(|span| match span {
                                Span::Text(text) => Some(text.as_str()),
                                _ => None,
                            })
                            .collect()
                    })
                    .collect();
                (cue.start_ms, cue.end_ms, lines)
            })
            .collect()
    }

    fn cue(start: u64, end: u64, lines: &[&str]) -> (u64, u64, Vec<String>) {
        (start, end, lines.iter().map(|&l| l.to_owned()).collect())
    }

    // Rule 5: lines and line endings.

    #[test]
    fn lines_split_at_crlf_lf_and_a_lone_cr() {
        let lines: Vec<&str> = split_lines("a\r\nb\nc\rd\r\re").collect();
        assert_eq!(lines, ["a", "b", "c", "d", "", "e"]);
        let doc = parsed(
            "1\r00:00:01,000 --> 00:00:02,000\rOne\r\r2\n00:00:03,000 --> 00:00:04,000\r\nTwo",
        );
        assert_eq!(
            plain(&doc),
            [cue(1000, 2000, &["One"]), cue(3000, 4000, &["Two"])]
        );
    }

    #[test]
    fn a_cr_cr_lf_is_one_line_break() {
        let lines: Vec<&str> = split_lines("a\r\r\nb\r\r\n\r\r\nc\r\r\r\nd").collect();
        assert_eq!(lines, ["a", "b", "", "c", "", "d"]);
        // A CRLF SRT whose line ends were converted again: each cue keeps
        // its text, even text that is only digits.
        let srt = "1\r\r\n00:00:01,000 --> 00:00:02,000\r\r\n1984\r\r\n\r\r\n\
                   2\r\r\n00:00:03,000 --> 00:00:04,000\r\r\nNext\r\r\n";
        let doc = parsed(srt);
        assert_eq!(
            plain(&doc),
            [cue(1000, 2000, &["1984"]), cue(3000, 4000, &["Next"])]
        );
        assert_eq!(doc.newline, Newline::CrLf);
    }

    #[test]
    fn the_newline_is_crlf_only_when_the_first_line_break_is_crlf() {
        let srt = |nl: &str| format!("1{nl}00:00:01,000 --> 00:00:02,000{nl}Hi\n");
        assert_eq!(parsed(&srt("\r\n")).newline, Newline::CrLf);
        assert_eq!(parsed(&srt("\n")).newline, Newline::Lf);
        assert_eq!(parsed(&srt("\r")).newline, Newline::Lf);
        assert_eq!(Newline::CrLf.as_str(), "\r\n");
        assert_eq!(Newline::Lf.as_str(), "\n");
    }

    #[test]
    fn trailing_whitespace_is_trimmed_from_every_line() {
        let doc = parsed("1 \t\n00:00:01,000 --> 00:00:02,000   \n  Indented  \t\n \t\n2\n00:00:03,000 --> 00:00:04,000\nNext");
        assert_eq!(
            plain(&doc),
            [cue(1000, 2000, &["  Indented"]), cue(3000, 4000, &["Next"])]
        );
    }

    // Rule 6: WebVTT by content, and its header.

    #[test]
    fn webvtt_is_detected_by_its_signature() {
        let cue = "\n\n00:01.000 --> 00:02.000\nHi &amp; bye";
        for signature in ["WEBVTT", "WEBVTT - Title", "WEBVTT\tTitle"] {
            let doc = parsed(&format!("{signature}{cue}"));
            assert!(doc.webvtt, "{signature}");
            assert_eq!(plain(&doc), [self::cue(1000, 2000, &["Hi & bye"])]);
        }
        assert!(parsed("WEBVTT\r\n\r\n00:01.000 --> 00:02.000\r\nHi").webvtt);
        assert!(parsed("\u{FEFF}WEBVTT\n\n00:01.000 --> 00:02.000\nHi").webvtt);
        // Not WebVTT: the text stays raw.
        for text in [
            &format!("WEBVTTX{cue}"),
            &format!("webvtt{cue}"),
            &format!(" WEBVTT{cue}"),
        ] {
            let doc = parsed(text);
            assert!(!doc.webvtt, "{text}");
            assert_eq!(plain(&doc), [self::cue(1000, 2000, &["Hi &amp; bye"])]);
        }
    }

    #[test]
    fn the_webvtt_header_is_skipped_up_to_the_first_blank_line() {
        let doc =
            parsed("WEBVTT\nKind: captions\nLanguage: en\n\n00:00:01.000 --> 00:00:02.000\nHi");
        assert_eq!(plain(&doc), [cue(1000, 2000, &["Hi"])]);
    }

    #[test]
    fn a_header_line_with_an_arrow_ends_the_header_and_starts_a_cue() {
        let doc = parsed("WEBVTT\nKind: captions\n00:00:01.000 --> 00:00:02.000\nHi\n\n00:00:03.000 --> 00:00:04.000\nBye");
        assert_eq!(
            plain(&doc),
            [cue(1000, 2000, &["Hi"]), cue(3000, 4000, &["Bye"])]
        );
        let doc = parsed("WEBVTT\n00:01.000 --> 00:02.000\nRight away");
        assert_eq!(plain(&doc), [cue(1000, 2000, &["Right away"])]);
    }

    #[test]
    fn youtube_header_lines_and_the_timestamp_map_are_skipped() {
        let vtt = "WEBVTT\nKind: captions\nLanguage: en\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n\
                   00:00:00.320 --> 00:00:02.000 align:start position:0%\n \nhello<00:00:00.800><c> world</c>\n\n\
                   00:00:02.000 --> 00:00:02.010 align:start position:0%\nhello world\n \n";
        let doc = parsed(vtt);
        assert!(doc.webvtt);
        assert_eq!(
            plain(&doc),
            [
                cue(320, 2000, &["hello world"]),
                cue(2000, 2010, &["hello world"])
            ]
        );
    }

    // Rule 7: WebVTT blocks that aren't cues.

    #[test]
    fn webvtt_note_style_and_region_blocks_are_skipped() {
        let vtt = "WEBVTT\n\nNOTE\nA comment\nover two lines\n\nNOTE\tanother one\n\n\
                   STYLE\n::cue { color: yellow }\n\nREGION id:fred width:40%\n\n\
                   00:01.000 --> 00:02.000\nHi\n\nNOTE a comment between cues\n\n\
                   00:03.000 --> 00:04.000\nBye";
        assert_eq!(
            plain(&parsed(vtt)),
            [cue(1000, 2000, &["Hi"]), cue(3000, 4000, &["Bye"])]
        );
    }

    #[test]
    fn style_and_region_blocks_after_a_cue_are_skipped() {
        // After a cue, a block without a timing line would be its text.
        let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n\nSTYLE\n::cue { color: red }\n\n\
                   REGION id:x width:40%\n\n00:03.000 --> 00:04.000\nBye";
        assert_eq!(
            plain(&parsed(vtt)),
            [cue(1000, 2000, &["Hi"]), cue(3000, 4000, &["Bye"])]
        );
    }

    #[test]
    fn a_timing_line_ends_a_note_style_or_region_block() {
        // The arrow on the line after NOTE: the NOTE line is the cue's
        // identifier. Further down: the block ends before the arrow.
        let vtt = "WEBVTT\n\nNOTE This is a comment\n00:00:01.000 --> 00:00:02.000\nHello\n\n\
                   00:00:03.000 --> 00:00:04.000\nWorld\n";
        let both = [cue(1000, 2000, &["Hello"]), cue(3000, 4000, &["World"])];
        assert_eq!(plain(&parsed(vtt)), both);
        let vtt = "WEBVTT\n\nNOTE\nline two of note\n00:00:01.000 --> 00:00:02.000\nHello\n\
                   00:00:03.000 --> 00:00:04.000\nWorld\n";
        assert_eq!(plain(&parsed(vtt)), both);
        for block in ["STYLE\n::cue { color: red }", "REGION id:x width:40%"] {
            let vtt = format!("WEBVTT\n\n{block}\n00:00:01.000 --> 00:00:02.000\nHello\n");
            assert_eq!(
                plain(&parsed(&vtt)),
                [cue(1000, 2000, &["Hello"])],
                "{block}"
            );
        }
    }

    #[test]
    fn only_the_exact_block_words_start_a_non_cue_block() {
        // "NOTES" isn't a NOTE block, so it continues the cue before it.
        let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n\nNOTES on this\n\nSTYLEs too";
        assert_eq!(
            plain(&parsed(vtt)),
            [cue(1000, 2000, &["Hi", "NOTES on this", "STYLEs too"])]
        );
        // In an SRT, NOTE is just text.
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nHi\n\nNOTE this";
        assert_eq!(plain(&parsed(srt)), [cue(1000, 2000, &["Hi", "NOTE this"])]);
    }

    // Rule 8: timing lines and timestamps.

    #[test]
    fn timestamps_take_optional_hours_and_an_optional_fraction() {
        assert_eq!(timestamp("00:00:01,000"), Some(1000));
        assert_eq!(timestamp("01:02:03.456"), Some(3_723_456));
        assert_eq!(timestamp("1:2:3"), Some(3_723_000));
        assert_eq!(timestamp("02:03.4"), Some(123_400));
        assert_eq!(timestamp("0:0"), Some(0));
        assert_eq!(timestamp("123:00:00,000"), Some(442_800_000));
        assert_eq!(timestamp("0001:00:00,000"), Some(3_600_000));
    }

    #[test]
    fn a_fraction_is_read_as_a_decimal_fraction_of_a_second() {
        assert_eq!(timestamp("00:00:00,5"), Some(500));
        assert_eq!(timestamp("00:00:00,05"), Some(50));
        assert_eq!(timestamp("00:00:00.050"), Some(50));
        assert_eq!(timestamp("00:00:00,1234"), Some(123));
        assert_eq!(timestamp("00:00:00,9999999999999999999999"), Some(999));
        assert_eq!(timestamp("00:00:07"), Some(7000));
    }

    #[test]
    fn malformed_or_out_of_range_timestamps_are_rejected() {
        for bad in [
            "",
            "1",
            "00:60:00,000",
            "00:00:60,000",
            "00:000:01,000",
            "00:00:001,000",
            ":00:01",
            "1:2:3:4",
            "00:00:01,",
            "00:00:01,5a",
            "00:00:01.5.0",
            "+1:00:00",
            "-1:00:00",
            "00:0a:00",
            " 00:01",
            "00:01 ",
            "99999999999999999999:00:00",
            // Fits a u64 but overflows in milliseconds.
            "5124095576030432:00:00",
        ] {
            assert_eq!(timestamp(bad), None, "{bad:?}");
        }
        // The largest hours that still fit.
        let max_hours = u64::MAX / 3_600_000;
        assert_eq!(
            timestamp(&format!("{max_hours}:00:00")),
            Some(max_hours * 3_600_000)
        );
    }

    #[test]
    fn timing_lines_allow_any_spacing_and_drop_settings_after_the_end() {
        for line in [
            "00:00:01,000 --> 00:00:02,500",
            "00:00:01,000-->00:00:02,500",
            "  00:00:01,000\t-->   00:00:02,500",
            "00:00:01.000 --> 00:00:02.500 align:start position:10%",
            "00:00:01,000 --> 00:00:02,500  X1:100 X2:200 Y1:10 Y2:20",
        ] {
            assert_eq!(timing_line(line), Some((1000, 2500)), "{line:?}");
        }
        for line in [
            "00:00:01,000 -->",
            "00:00:01,000 --> soon",
            "1 00:00:01,000 --> 00:00:02,000",
            "00:00:01,000 --> 00:00:02,000,",
            "00:00:01,000 -> 00:00:02,000",
        ] {
            assert_eq!(timing_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn srt_with_dot_separators_and_two_digit_fractions_parses() {
        let doc = parsed("1\n00:00:01.50 --> 00:00:02.05\nHi\n\n2\n0:00:03.5 --> 0:00:04\nBye");
        assert_eq!(
            plain(&doc),
            [cue(1500, 2050, &["Hi"]), cue(3500, 4000, &["Bye"])]
        );
    }

    #[test]
    fn hours_past_99_parse() {
        let doc = parsed("1\n100:00:00,000 --> 1234:05:06,789\nLong film");
        assert_eq!(
            plain(&doc),
            [cue(360_000_000, 4_442_706_789, &["Long film"])]
        );
    }

    // Rule 9: invalid timing lines.

    #[test]
    fn a_line_with_an_arrow_that_isnt_a_timing_line_drops_its_cue() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nGood\n\n\
                   2\n00:00:0x,000 --> 00:00:04,000\nBad\nStill bad\n\nBad after a blank line\n\n\
                   3\n00:00:05,000 --> 00:00:06,000\nAlso good\n\
                   4\n00:61:00,000 --> 00:62:00,000\nBad without a blank line\n\
                   5\n00:00:07,000 --> 00:00:08,000\nGood again";
        assert_eq!(
            plain(&parsed(srt)),
            [
                cue(1000, 2000, &["Good"]),
                cue(5000, 6000, &["Also good"]),
                cue(7000, 8000, &["Good again"])
            ]
        );
    }

    #[test]
    fn an_arrow_in_srt_text_stays_text() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nGo this way -->\nnow\n\n\
                   2\n00:00:03,000 --> 00:00:04,000\nok\n";
        assert_eq!(
            plain(&parsed(srt)),
            [
                cue(1000, 2000, &["Go this way -->", "now"]),
                cue(3000, 4000, &["ok"])
            ]
        );
        assert_eq!(
            plain(&parsed("1\n00:00:01,000 --> 00:00:02,000\n--> this way")),
            [cue(1000, 2000, &["--> this way"])]
        );
        // A digit later in the line doesn't make it a timing line.
        assert_eq!(
            plain(&parsed(
                "1\n00:00:01,000 --> 00:00:02,000\nRoute 66 -->\nnow"
            )),
            [cue(1000, 2000, &["Route 66 -->", "now"])]
        );
        // In WebVTT a line with an arrow is a timing line, as the format has it.
        let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nGo this way -->\nnow\n\n\
                   00:03.000 --> 00:04.000\nok\n";
        assert_eq!(plain(&parsed(vtt)), [cue(3000, 4000, &["ok"])]);
    }

    #[test]
    fn an_srt_timing_line_may_start_with_spaces() {
        let srt =
            "1\n  00:00:01,000 --> 00:00:02,000\nHi\n\n2\n\t00:00:03,000 --> 00:00:04,000\nBye";
        assert_eq!(
            plain(&parsed(srt)),
            [cue(1000, 2000, &["Hi"]), cue(3000, 4000, &["Bye"])]
        );
    }

    #[test]
    fn a_negative_srt_timing_line_drops_its_cue() {
        // A shifted SRT can start a cue before zero. Its timing line is
        // invalid, so the cue is dropped, never added to the cue before.
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nFirst\n\n\
                   2\n-00:00:00,500 --> 00:00:00,900\nShifted before zero\n\n\
                   3\n00:00:03,000 --> 00:00:04,000\nThird\n";
        assert_eq!(
            plain(&parsed(srt)),
            [cue(1000, 2000, &["First"]), cue(3000, 4000, &["Third"])]
        );
    }

    #[test]
    fn an_srt_timing_line_after_an_invisible_mark_drops_its_cue() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nFirst\n\n\
                   \u{FEFF}00:00:03,000 --> 00:00:04,000\nSecond\n";
        assert_eq!(plain(&parsed(srt)), [cue(1000, 2000, &["First"])]);
        for mark in [
            '\u{FEFF}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{202A}',
            '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2060}',
        ] {
            let line = format!("{mark}00:00:03,000 --> 00:00:04,000");
            assert!(looks_like_timing(&line, false), "{mark:?}");
            let line = format!(" {mark} -00:00:03,000 --> 00:00:04,000");
            assert!(looks_like_timing(&line, false), "{mark:?}");
        }
        // Spaces, marks and one minus sign before the digit; nothing else.
        assert!(looks_like_timing("\u{A0}00:00:01,000 --> x", false));
        for text in [
            "\u{200A}x 1 -->",
            "\u{2010}00:00:01,000 -->",
            "\u{2061}00:00:01,000 -->",
            "--00:00:01,000 -->",
            "- 00:00:01,000 -->",
            "-\u{FEFF}00:00:01,000 -->",
            "00:00:01,000 -> x",
        ] {
            assert!(!looks_like_timing(text, false), "{text:?}");
        }
    }

    // Rule 10: cues without blank lines, indexes and identifiers.

    #[test]
    fn srt_cues_without_blank_lines_between_them_still_split() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nFirst\n2\n00:00:03,000 --> 00:00:04,000\nSecond\nline\n3\n00:00:05,000 --> 00:00:06,000\nThird";
        assert_eq!(
            plain(&parsed(srt)),
            [
                cue(1000, 2000, &["First"]),
                cue(3000, 4000, &["Second", "line"]),
                cue(5000, 6000, &["Third"])
            ]
        );
    }

    #[test]
    fn a_line_before_a_timing_line_stays_text_unless_it_is_digits() {
        let srt = "00:00:01,000 --> 00:00:02,000\nFirst\nnot an index\n00:00:03,000 --> 00:00:04,000\n2024\nSecond";
        assert_eq!(
            plain(&parsed(srt)),
            [
                cue(1000, 2000, &["First", "not an index"]),
                cue(3000, 4000, &["2024", "Second"])
            ]
        );
    }

    #[test]
    fn lines_before_a_timing_line_after_a_blank_line_are_its_identifier() {
        let vtt = "WEBVTT\n\nintro\n00:01.000 --> 00:02.000\nHi\n\nsome id\nanother id line\n00:03.000 --> 00:04.000\nBye";
        assert_eq!(
            plain(&parsed(vtt)),
            [cue(1000, 2000, &["Hi"]), cue(3000, 4000, &["Bye"])]
        );
        // The first block of an SRT starts after no blank line at all.
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nHi";
        assert_eq!(plain(&parsed(srt)), [cue(1000, 2000, &["Hi"])]);
    }

    // Rule 11: blocks without a timing line.

    #[test]
    fn a_block_of_digits_without_a_timing_line_is_dropped() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nHi\n\n42\n7\n\n2\n00:00:03,000 --> 00:00:04,000\nBye";
        assert_eq!(
            plain(&parsed(srt)),
            [cue(1000, 2000, &["Hi"]), cue(3000, 4000, &["Bye"])]
        );
    }

    #[test]
    fn a_block_of_digits_is_the_text_of_a_cue_without_any() {
        let srt =
            "1\n00:00:01,000 --> 00:00:02,000\n\n1984\n\n2\n00:00:03,000 --> 00:00:04,000\nNext";
        assert_eq!(
            plain(&parsed(srt)),
            [cue(1000, 2000, &["1984"]), cue(3000, 4000, &["Next"])]
        );
        // YouTube captions start a cue with a line of one space, which ends
        // the block once trimmed.
        let vtt = "WEBVTT\nKind: captions\nLanguage: en\n\n\
                   00:00:00.199 --> 00:00:02.270 align:start position:0%\n \n2019\n\n\
                   00:00:02.270 --> 00:00:02.280 align:start position:0%\n2019\n \n\n";
        assert_eq!(
            plain(&parsed(vtt)),
            [cue(199, 2270, &["2019"]), cue(2270, 2280, &["2019"])]
        );
    }

    #[test]
    fn a_text_block_without_a_timing_line_continues_the_previous_cue() {
        let srt = "Title before any cue\n\n1\n00:00:01,000 --> 00:00:02,000\nHi\n\nthere\n42\n\n\nfriend\n\n2\n00:00:03,000 --> 00:00:04,000\nBye";
        assert_eq!(
            plain(&parsed(srt)),
            [
                cue(1000, 2000, &["Hi", "there", "42", "friend"]),
                cue(3000, 4000, &["Bye"])
            ]
        );
    }

    // Rule 12: cue text to spans.

    #[test]
    fn html_style_tags_become_styles_in_any_case_and_with_classes() {
        assert_eq!(
            spans(
                "<i>a</i> <B>b</B> <u.x>c</U> <i.loud>d</i> <b.a.b>e</b>",
                false
            ),
            [
                Span::Open(Style::Italic),
                text("a"),
                Span::Close(Style::Italic),
                text(" "),
                Span::Open(Style::Bold),
                text("b"),
                Span::Close(Style::Bold),
                text(" "),
                Span::Open(Style::Underline),
                text("c"),
                Span::Close(Style::Underline),
                text(" "),
                Span::Open(Style::Italic),
                text("d"),
                Span::Close(Style::Italic),
                text(" "),
                Span::Open(Style::Bold),
                text("e"),
                Span::Close(Style::Bold),
            ]
        );
    }

    #[test]
    fn ass_style_overrides_become_styles_and_others_are_dropped() {
        assert_eq!(
            spans(
                "{\\an8}{\\i1}a{\\i0} {\\B1}b{\\b0} {\\u1}c{\\U0}{\\pos(10,20)}d",
                false
            ),
            [
                Span::Open(Style::Italic),
                text("a"),
                Span::Close(Style::Italic),
                text(" "),
                Span::Open(Style::Bold),
                text("b"),
                Span::Close(Style::Bold),
                text(" "),
                Span::Open(Style::Underline),
                text("c"),
                Span::Close(Style::Underline),
                text("d"),
            ]
        );
        // Several overrides in one block each count.
        assert_eq!(
            spans("{\\an8\\i1}x", false),
            [Span::Open(Style::Italic), text("x")]
        );
        // Braces without a backslash, or without a closing brace, are text.
        assert_eq!(spans("{note} {\\i1 x", false), [text("{note} {\\i1 x")]);
    }

    #[test]
    fn font_and_webvtt_tags_are_dropped_and_their_text_kept() {
        assert_eq!(
            spans(
                "<font color=\"#ff0000\">red</font> <FONT face=x>f</FONT> <font>g</font>",
                false
            ),
            [text("red f g")]
        );
        assert_eq!(
            spans(
                "<c>a</c><c.yellow.bg_blue>b</c> <v Roger Bingham>c</v> <v.loud Esme>d</v> <V>e</V> <lang en-GB>f</lang> <lang.x fr>g</lang>",
                true
            ),
            [text("ab c d e f g")]
        );
    }

    #[test]
    fn ruby_text_is_put_in_parentheses() {
        assert_eq!(
            spans("<ruby>漢<rt>かん</rt></ruby>字", true),
            [text("漢(かん)字")]
        );
        // An <rt> still open at </ruby> or at the end of the line is closed.
        assert_eq!(
            spans("<ruby>漢<rt>かん</ruby>字", true),
            [text("漢(かん)字")]
        );
        assert_eq!(spans("<ruby>漢<rt>かん", true), [text("漢(かん)")]);
        // A stray </rt> or a second <rt> adds nothing.
        assert_eq!(spans("a</rt><rt>b<rt>c</rt>", true), [text("a(bc)")]);
    }

    #[test]
    fn inline_timestamps_are_dropped() {
        assert_eq!(
            spans("Hello <00:00:01.000>world<01:02.500>!", true),
            [text("Hello world!")]
        );
    }

    #[test]
    fn webvtt_character_references_are_decoded() {
        assert_eq!(
            spans(
                "&amp; &lt;b&gt; a&nbsp;b &lrm;&rlm; &#65;&#x42;&#X43;&#x1F600; &AMP; &amp;lt;",
                true
            ),
            [text("& <b> a\u{A0}b \u{200E}\u{200F} ABC😀 & &lt;")]
        );
        assert_eq!(
            spans(
                "He said &quot;hi&quot; &apos;ok&apos; &hellip; caf&eacute; &Eacute;t&eacute; \
                 &copy; &euro; &mdash; &alpha;&Omega; &LT;&GT;&QUOT;",
                true
            ),
            [text("He said \"hi\" 'ok' … café Été © € — αΩ <>\"")]
        );
    }

    #[test]
    fn the_named_references_are_the_html4_set_sorted_for_lookup() {
        assert_eq!(entities::NAMED.len(), 252 + 7);
        assert!(entities::NAMED.windows(2).all(|w| w[0].0 < w[1].0));
        for (name, c) in entities::NAMED {
            assert_eq!(entities::named(name), Some(c), "{name}");
        }
        assert_eq!(entities::named("Eacute"), Some('É'));
        assert_eq!(entities::named("eacute"), Some('é'));
        for name in ["EACUTE", "Amp", "Quot", "apos;", "", "bogus"] {
            assert_eq!(entities::named(name), None, "{name}");
        }
    }

    #[test]
    fn the_legacy_names_are_html_3_2s() {
        let legacy: Vec<&str> = entities::NAMED
            .iter()
            .map(|&(name, _)| name)
            .filter(|name| entities::legacy(name).is_some())
            .collect();
        assert_eq!(legacy.len(), 106);
        for name in [
            "amp", "AMP", "lt", "LT", "gt", "GT", "quot", "QUOT", "copy", "COPY",
        ] {
            assert!(legacy.contains(&name), "{name}");
        }
        for name in [
            "reg", "REG", "nbsp", "iexcl", "AElig", "eacute", "yuml", "uml", "shy",
        ] {
            assert!(legacy.contains(&name), "{name}");
        }
        for name in ["apos", "euro", "hellip", "OElig", "Yuml", "trade", "fnof"] {
            assert_eq!(entities::legacy(name), None, "{name}");
        }
        assert_eq!(entities::legacy("Yacute"), Some('Ý'));
        // Computed apart from this code, from Python's table of HTML's
        // names: sum((i + 1) * ord(html5[k]) for i, k in enumerate(the
        // names without ';', sorted bytewise)).
        let checksum: u64 = legacy
            .iter()
            .zip(1..)
            .map(|(name, i)| i * u64::from(entities::legacy(name).unwrap()))
            .sum();
        assert_eq!(checksum, 1_128_350);
    }

    #[test]
    fn legacy_names_need_no_semicolon_before_a_non_alphanumeric() {
        assert_eq!(
            spans(
                "&amp &lt;b&gt &copy 2024 caf&eacute, &Eacute.t&eacute &nbsp|&AMP&LT&GT&QUOT &COPY&REG",
                true
            ),
            [text("& <b> © 2024 café, É.té \u{A0}|&<>\" ©®")]
        );
        // At the end of the line, and before `#` or another reference.
        assert_eq!(spans("&amp#38; &reg&copy;&amp", true), [text("&#38; ®©&")]);
        // HTML 4 names that HTML 3.2 didn't have need their `;`.
        assert_eq!(
            spans("&euro &hellip &apos &mdash; &COPY;&REG;", true),
            [text("&euro &hellip &apos — ©®")]
        );
    }

    #[test]
    fn numbers_from_128_to_159_are_read_as_windows_1252() {
        // HTML's table, the five undefined ones (0x81, 0x8D, 0x8F, 0x90 and
        // 0x9D) left as they are.
        let html = [
            0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160,
            0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022,
            0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
        ];
        for (n, want) in (128u32..=159).zip(html) {
            let want = char::from_u32(want).unwrap().to_string();
            assert_eq!(spans(&format!("&#{n};"), true), [text(&want)], "{n}");
            assert_eq!(spans(&format!("&#x{n:X};"), true), [text(&want)], "{n}");
        }
        assert_eq!(
            spans("&#127;&#150;&#160;&#x9f;", true),
            [text("\u{7F}–\u{A0}Ÿ")]
        );
    }

    #[test]
    fn invalid_or_unknown_references_stay_literal() {
        let literal = "&#xD800; &#1114112; &#99999999999; &#; &#x; &#xZZ; &#0; &#x0; \
                       &bogus; &Amp; &EACUTE; &Quot; &; & &#65 &#x41 &ampx &copy2024 &Amp &notin \
                       &#&amp1;";
        assert_eq!(spans(literal, true), [text(literal)]);
    }

    #[test]
    fn a_numeric_line_break_breaks_the_line() {
        assert_eq!(
            cue_lines("a&#10;b&#13;c&#xA;d&#XD;e", true),
            [["a"], ["b"], ["c"], ["d"], ["e"]].map(|[t]| vec![text(t)])
        );
        // Styles carry on to the next line; an open <rt> is closed.
        assert_eq!(
            cue_lines("<i>a&#10;b</i> <ruby>漢<rt>か&#10;ん</rt></ruby>", true),
            [
                vec![Span::Open(Style::Italic), text("a")],
                vec![text("b"), Span::Close(Style::Italic), text(" 漢(か)")],
                vec![text("ん")],
            ]
        );
        // The lines without text that a break leaves are dropped.
        let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nOne&#13;&#10;Two&#10;\n";
        assert_eq!(plain(&parsed(vtt)), [cue(1000, 2000, &["One", "Two"])]);
        // SRT text is raw.
        assert_eq!(spans("One&#10;Two", false), [text("One&#10;Two")]);
    }

    #[test]
    fn srt_text_is_raw() {
        assert_eq!(
            spans("Tom &amp; Jerry &lt;3 &#65;", false),
            [text("Tom &amp; Jerry &lt;3 &#65;")]
        );
    }

    #[test]
    fn unknown_markup_stays_literal() {
        for literal in [
            "<br> <p>x</p> <span class=a>",
            "<b",
            "a < b > c",
            "1 <2> 3",
            "<i<b>",
            "</ i> </x> <bx> <i-x> <>",
        ] {
            let spans = spans(literal, true);
            let rendered: String = spans
                .iter()
                .map(|span| match span {
                    Span::Text(text) => text.clone(),
                    Span::Open(_) => "<OPEN>".into(),
                    Span::Close(_) => "<CLOSE>".into(),
                })
                .collect();
            let expected = if literal == "<i<b>" {
                "<i<OPEN>".to_owned()
            } else {
                literal.to_owned()
            };
            assert_eq!(rendered, expected, "{literal}");
        }
    }

    #[test]
    fn adjacent_text_is_one_span() {
        assert_eq!(
            spans("a<c>b</c>{\\an8}c<00:01.000>d&amp;e", true),
            [text("abcd&e")]
        );
    }

    // Rule 13: normalization.

    #[test]
    fn out_of_order_cues_come_out_sorted_and_ties_keep_their_order() {
        let srt = "1\n00:00:05,000 --> 00:00:06,000\nThird\n\n2\n00:00:01,000 --> 00:00:02,000\nFirst\n\n\
                   3\n00:00:03,000 --> 00:00:04,000\nSecond a\n\n4\n00:00:03,000 --> 00:00:03,500\nSecond b";
        assert_eq!(
            plain(&parsed(srt)),
            [
                cue(1000, 2000, &["First"]),
                cue(3000, 4000, &["Second a"]),
                cue(3000, 3500, &["Second b"]),
                cue(5000, 6000, &["Third"])
            ]
        );
    }

    #[test]
    fn many_ties_keep_their_order() {
        // Past the length up to which even an unstable sort keeps ties in
        // order: cue i starts at second (i * 7) % 20, three cues to a start.
        let srt: String = (0..60u64)
            .map(|i| {
                let s = (i * 7) % 20;
                format!("{}\n00:00:{s:02},000 --> 00:00:{s:02},500\nc{i}\n\n", i + 1)
            })
            .collect();
        let got: Vec<String> = plain(&parsed(&srt))
            .into_iter()
            .map(|(_, _, lines)| lines[0].clone())
            .collect();
        let mut want: Vec<(u64, u64)> = (0..60u64).map(|i| ((i * 7) % 20, i)).collect();
        want.sort();
        let want: Vec<String> = want.into_iter().map(|(_, i)| format!("c{i}")).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn cues_in_any_order_sort_as_a_stable_sort_would() {
        // Pseudo-random starts with many ties, a reversed run, and one
        // long cycle (each cue belongs one place to the left).
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let random: Vec<u64> = (0..10_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x % 3_000
            })
            .collect();
        let reversed: Vec<u64> = (0..1_000).rev().collect();
        let rotated: Vec<u64> = (0..1_000).map(|i| (i + 1) % 1_000).collect();
        for starts in [random, reversed, rotated] {
            // Each cue's text is its index in the file.
            let mut cues: Vec<Cue> = starts
                .iter()
                .enumerate()
                .map(|(i, &start_ms)| Cue {
                    start_ms,
                    end_ms: start_ms + 1,
                    lines: vec![vec![text(&i.to_string())]],
                })
                .collect();
            let mut want: Vec<(u64, usize)> = starts.iter().copied().zip(0..).collect();
            want.sort_by_key(|&(start, _)| start);
            sort_by_start(&mut cues, &mut Ticker::new(&mut || false)).unwrap();
            let got: Vec<(u64, usize)> = cues
                .iter()
                .map(|cue| match &cue.lines[0][0] {
                    Span::Text(i) => (cue.start_ms, i.parse().unwrap()),
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(got, want);
        }
    }

    #[test]
    fn a_cue_that_doesnt_end_after_it_starts_lasts_one_millisecond() {
        let srt =
            "1\n00:00:02,000 --> 00:00:01,000\nBackwards\n\n2\n00:00:03,000 --> 00:00:03,000\nZero";
        assert_eq!(
            plain(&parsed(srt)),
            [cue(2000, 2001, &["Backwards"]), cue(3000, 3001, &["Zero"])]
        );
        let max = u64::MAX;
        let raw = vec![Cue {
            start_ms: max,
            end_ms: max,
            lines: vec![vec![text("End of time")]],
        }];
        let cues = normalize(raw, &mut Ticker::new(&mut || false)).unwrap();
        assert_eq!((cues[0].start_ms, cues[0].end_ms), (max, max));
    }

    #[test]
    fn lines_without_visible_text_and_cues_left_empty_are_dropped() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n<i>\nHello</i>\n{\\an8}\n&nbsp;\n\n\
                   2\n00:00:03,000 --> 00:00:04,000\n<b></b>\n\n\
                   3\n00:00:05,000 --> 00:00:06,000\n\n\
                   4\n00:00:07,000 --> 00:00:08,000\nLast";
        let doc = parsed(srt);
        assert_eq!(
            doc.cues,
            [
                Cue {
                    start_ms: 1000,
                    end_ms: 2000,
                    lines: vec![
                        vec![text("Hello"), Span::Close(Style::Italic)],
                        // SRT text is raw, so this line has visible text.
                        vec![text("&nbsp;")],
                    ],
                },
                Cue {
                    start_ms: 7000,
                    end_ms: 8000,
                    lines: vec![vec![text("Last")]],
                },
            ]
        );
        let vtt =
            "WEBVTT\n\n00:01.000 --> 00:02.000\n&nbsp;\n<c> </c>\n\n00:03.000 --> 00:04.000\nKept";
        assert_eq!(plain(&parsed(vtt)), [cue(3000, 4000, &["Kept"])]);
    }

    #[test]
    fn a_text_without_cues_is_an_error() {
        for text in [
            "",
            "\n\n",
            "WEBVTT",
            "WEBVTT\n\nNOTE nothing here",
            "Just some text\nwith no timing",
            "1\n00:00:01,000 --> 00:00:02,000\n\n",
            "1\nnot --> timing\nText",
        ] {
            assert_eq!(
                parse(text, &mut || false),
                Err(ParseError::NoCues),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_text_over_the_line_limit_is_too_large() {
        // Three lines of cue, then blank lines up to exactly the limit.
        let mut srt = "1\n00:00:01,000 --> 00:00:02,000\nHi\n".to_owned();
        srt.push_str(&"\n".repeat(MAX_LINES - 3));
        assert_eq!(split_lines(&srt).count(), MAX_LINES);
        assert_eq!(plain(&parsed(&srt)), [cue(1000, 2000, &["Hi"])]);
        srt.push('\n');
        assert_eq!(parse(&srt, &mut || false), Err(ParseError::TooLarge));
        // A line without a line break after it counts too.
        srt.pop();
        srt.push('x');
        assert_eq!(parse(&srt, &mut || false), Err(ParseError::TooLarge));
    }

    #[test]
    fn each_cue_line_a_numeric_line_break_makes_counts_toward_the_limit() {
        // One line that `&#10;` breaks into 10,001 cue lines (the last one
        // empty), then blank lines up to exactly the limit.
        let broken = "x&#10;".repeat(10_000);
        let mut vtt = format!("WEBVTT\n\n00:01.000 --> 00:02.000\n{broken}\n");
        vtt.push_str(&"\n".repeat(MAX_LINES - 4 - 10_000));
        let doc = parsed(&vtt);
        assert_eq!(doc.cues[0].lines.len(), 10_000);
        vtt.push('\n');
        assert_eq!(parse(&vtt, &mut || false), Err(ParseError::TooLarge));
        // In SRT the reference is text: the line is one line.
        let mut srt = format!("1\n00:00:01,000 --> 00:00:02,000\n{broken}\n");
        srt.push_str(&"\n".repeat(MAX_LINES - 3));
        assert_eq!(parsed(&srt).cues[0].lines.len(), 1);
        // push_lines counts them, for a cue's first block of text and for
        // text after a blank line alike.
        let mut lines = 0;
        let mut never = || false;
        let mut ticker = Ticker::new(&mut never);
        push_lines(&mut Vec::new(), &[&broken], true, &mut lines, &mut ticker).unwrap();
        assert_eq!(lines, 10_000);
        let mut lines = MAX_LINES - 9_999;
        assert_eq!(
            push_lines(&mut Vec::new(), &[&broken], true, &mut lines, &mut ticker),
            Err(ParseError::TooLarge)
        );
    }

    #[test]
    fn a_text_with_a_line_over_the_limit_is_too_large() {
        let srt = |line: &str, nl: &str| {
            format!("1{nl}00:00:01,000 --> 00:00:02,000{nl}{line}{nl}{nl}2{nl}00:00:03,000 --> 00:00:04,000{nl}Bye{nl}")
        };
        // The line break isn't part of the line.
        let longest = "x".repeat(MAX_LINE_BYTES);
        for nl in ["\n", "\r\n", "\r"] {
            let doc = parsed(&srt(&longest, nl));
            assert_eq!(doc.cues[0].lines, [vec![text(&longest)]]);
        }
        // One byte more, anywhere in the text: in a cue, a block that isn't
        // read or a WebVTT header. Trailing spaces count too.
        let over = "x".repeat(MAX_LINE_BYTES + 1);
        let spaces = format!("x{}", " ".repeat(MAX_LINE_BYTES));
        let note = format!("WEBVTT\n\nNOTE {over}\n\n00:01.000 --> 00:02.000\nHi\n");
        let header = format!("WEBVTT {over}\n\n00:01.000 --> 00:02.000\nHi\n");
        for text in [srt(&over, "\n"), srt(&spaces, "\r\n"), note, header] {
            assert_eq!(parse(&text, &mut || false), Err(ParseError::TooLarge));
        }
        // Counted in bytes: 32K two-byte characters are 64 KiB.
        let wide = "é".repeat(MAX_LINE_BYTES / 2);
        assert_eq!(parsed(&srt(&wide, "\n")).cues.len(), 2);
        let wider = format!("{wide}x");
        assert_eq!(
            parse(&srt(&wider, "\n"), &mut || false),
            Err(ParseError::TooLarge)
        );
    }

    // Cancellation.

    #[test]
    fn a_job_cancelled_before_parsing_is_cancelled() {
        for text in ["1\n00:00:01,000 --> 00:00:02,000\nHi", ""] {
            assert_eq!(
                parse(text, &mut || true),
                Err(ParseError::Cancelled),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_cancel_is_noticed_while_lines_are_read() {
        // Blank lines only, so no other step counts any work.
        let blank = "\n".repeat(CHECK_EVERY + 1);
        assert_eq!(
            parse(&blank, &mut cancelled_after_first()),
            Err(ParseError::Cancelled)
        );
    }

    #[test]
    fn parsing_checks_every_16k_lines() {
        // A fixed size, so a change to how often parsing checks shows:
        // 100,000 blank lines are checked at lines 0, 16,384, 32,768,
        // 49,152, 65,536, 81,920 and 98,304, then once before the sort.
        let blank = "\n".repeat(100_000);
        let mut calls = 0;
        let result = parse(&blank, &mut || {
            calls += 1;
            false
        });
        assert_eq!(result, Err(ParseError::NoCues));
        assert_eq!(calls, 8);
    }

    #[test]
    fn a_cancel_is_noticed_inside_a_long_block() {
        let text = vec!["x"; CHECK_EVERY];
        // A valid timing line keeps the text after it; a bad one drops it.
        for timing in [
            "00:00:01,000 --> 00:00:02,000",
            "00:00:0x,000 --> 00:00:02,000",
        ] {
            let block = [&[timing][..], &text].concat();
            let mut reader = Reader {
                webvtt: false,
                cues: Vec::new(),
                continues: false,
                lines: 0,
            };
            let mut is_cancelled = cancelled_after_first();
            let mut ticker = Ticker::new(&mut is_cancelled);
            assert_eq!(
                reader.block(&block, &mut ticker),
                Err(ParseError::Cancelled),
                "{timing}"
            );
        }
        // Turning lines into spans counts on its own too.
        let text = vec!["x"; CHECK_EVERY + 1];
        let mut is_cancelled = cancelled_after_first();
        let mut ticker = Ticker::new(&mut is_cancelled);
        assert_eq!(
            push_lines(&mut Vec::new(), &text, false, &mut 0, &mut ticker),
            Err(ParseError::Cancelled)
        );
    }

    /// How many times `f` asks its ticker's check, which never finds a
    /// cancel.
    fn checks(f: impl FnOnce(&mut Ticker)) -> usize {
        let mut calls = 0;
        let mut is_cancelled = || {
            calls += 1;
            false
        };
        f(&mut Ticker::new(&mut is_cancelled));
        calls
    }

    #[test]
    fn long_lines_count_as_work_by_their_length_in_every_step() {
        // Four lines of 32 KiB are 4 * (1 + 4096) units, past CHECK_EVERY,
        // so the piece of work after them is checked: two checks in all.
        let long = "x".repeat(32 * 1024);
        let lines = [long.as_str(); 4];
        let found = checks(|ticker| {
            assert_eq!(find(&lines, ticker, |_| false), Ok(None));
            ticker.tick().unwrap();
        });
        assert_eq!(found, 2);
        let pushed = checks(|ticker| {
            push_lines(&mut Vec::new(), &lines, false, &mut 0, ticker).unwrap();
            ticker.tick().unwrap();
        });
        assert_eq!(pushed, 2);
        // A line that breaks into 10,001 cue lines is 1 + 60,000 / 8 units,
        // and one more for each extra cue line.
        let broken = "x&#10;".repeat(10_000);
        let pushed = checks(|ticker| {
            push_lines(&mut Vec::new(), &[&broken], true, &mut 0, ticker).unwrap();
            ticker.tick().unwrap();
        });
        assert_eq!(pushed, 2);
        // Lines read count by their length too: four blank lines of 32 KiB
        // of spaces bring one more check than four short ones.
        let srt = |blank: &str| {
            format!("{blank}\n{blank}\n{blank}\n{blank}\n1\n00:00:01,000 --> 00:00:02,000\nHi\n")
        };
        let parsed_with = |text: &str| {
            let mut calls = 0;
            parse(text, &mut || {
                calls += 1;
                false
            })
            .unwrap();
            calls
        };
        let spaces = " ".repeat(32 * 1024);
        assert_eq!(parsed_with(&srt(&spaces)), parsed_with(&srt(" ")) + 1);
    }

    #[test]
    fn a_cancel_is_noticed_while_cues_are_normalized() {
        let cues = (0..=CHECK_EVERY as u64)
            .map(|i| Cue {
                start_ms: i,
                end_ms: i + 1,
                lines: vec![vec![text("x")]],
            })
            .collect();
        let mut is_cancelled = cancelled_after_first();
        assert_eq!(
            normalize(cues, &mut Ticker::new(&mut is_cancelled)),
            Err(ParseError::Cancelled)
        );
    }

    #[test]
    fn a_cancel_is_noticed_before_and_after_the_sort() {
        let cue = |start_ms| Cue {
            start_ms,
            end_ms: start_ms + 1,
            lines: vec![vec![text("x")]],
        };
        // Checks at the first cue, before the sort, and after it: a cancel
        // found at the last one leaves the cues where they were.
        for (cancelled_from, starts) in [(2, [1000, 2000]), (2, [2000, 1000]), (3, [2000, 1000])] {
            let mut calls = 0;
            let mut is_cancelled = || {
                calls += 1;
                calls >= cancelled_from
            };
            let mut ticker = Ticker::new(&mut is_cancelled);
            ticker.tick().unwrap();
            let mut cues = starts.map(cue);
            assert_eq!(sort_by_start(&mut cues, &mut ticker), Err(Cancelled));
            assert_eq!(cues.map(|cue| cue.start_ms), starts);
        }
        // Moving the cues into place counts as work: one long cycle of
        // 3 * CHECK_EVERY cues is checked at least five times more.
        let n = 3 * CHECK_EVERY as u64;
        let mut cues: Vec<Cue> = (0..n).map(|i| cue((i + 1) % n)).collect();
        let mut calls = 0;
        let mut count = || {
            calls += 1;
            false
        };
        let mut ticker = Ticker::new(&mut count);
        ticker.tick().unwrap();
        sort_by_start(&mut cues, &mut ticker).unwrap();
        assert!(calls >= 3 + 5, "{calls}");
        assert!(cues.iter().zip(0..).all(|(cue, i)| cue.start_ms == i));
        // In normalize, the check before the sort comes after the one at
        // the first cue.
        let cues = [2000, 1000].map(cue).to_vec();
        let mut is_cancelled = cancelled_after_first();
        assert_eq!(
            normalize(cues, &mut Ticker::new(&mut is_cancelled)),
            Err(ParseError::Cancelled)
        );
        // So a short text still reports a cancel after its first line.
        assert_eq!(
            parse(
                "1\n00:00:01,000 --> 00:00:02,000\nHi",
                &mut cancelled_after_first()
            ),
            Err(ParseError::Cancelled)
        );
    }

    #[test]
    fn many_cues_parse_across_cancel_checks() {
        let srt: String = (0..20_000u64)
            .map(|i| {
                format!(
                    "{}\n00:00:{:02},000 --> 00:00:{:02},500\nCue {i}\n\n",
                    i + 1,
                    i % 60,
                    i % 60
                )
            })
            .collect();
        let doc = parsed(&srt);
        assert_eq!(doc.cues.len(), 20_000);
        assert!(doc.cues.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
    }
}
