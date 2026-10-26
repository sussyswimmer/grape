//! Subtitle text to cues, for SRT and WebVTT alike. Real files are messy,
//! so the reader is lenient and only a file without one usable cue fails.
//!
//! - **Lines** split at `\r\n`, `\n` or a lone `\r`, and lose their
//!   trailing whitespace. The output keeps the first line break's kind.
//! - **WebVTT** is recognized by content: the text starts with `WEBVTT`
//!   alone or followed by whitespace. Its header (up to the first blank
//!   line, or up to a line with `-->`) is skipped, and so are its `NOTE`,
//!   `STYLE` and `REGION` blocks.
//! - **Cues** start at every valid timing line (`<ts> --> <ts>`, cue
//!   settings and SRT coordinates dropped), even without a blank line
//!   before it. A line of digits just before one is its index, not text;
//!   after a blank line, everything before the timing line is the cue's
//!   identifier. A line with `-->` that isn't a valid timing line drops its
//!   cue. A block without a timing line continues the previous cue's text,
//!   unless it is only digits.
//! - **Text** keeps italic, bold and underline from HTML-style, WebVTT and
//!   ASS tags; other known tags are dropped with their text kept, `<rt>`
//!   ruby text is put in parentheses, and unknown markup stays literal.
//!   Character references are decoded in WebVTT only: SRT text is raw.
//! - **Normalization:** cues are sorted by start, a cue that doesn't end
//!   after it starts lasts 1 ms, and lines without visible text (and cues
//!   left without lines) are dropped.

use crate::CancelToken;

/// How many blocks and cues are read between cancel checks.
const CHECK_EVERY: usize = 256;

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
    /// The cancel token was set.
    Cancelled,
    /// Not one cue had a valid timing line and visible text.
    NoCues,
}

/// Reads `text` (SRT or WebVTT) into a normalized [`Document`], as the
/// module docs describe. `cancel` is checked before reading and every
/// [`CHECK_EVERY`] blocks or cues.
pub(crate) fn parse(text: &str, cancel: &CancelToken) -> Result<Document, ParseError> {
    if cancel.is_cancelled() {
        return Err(ParseError::Cancelled);
    }
    // `decode` strips the BOM; a second one would hide the WEBVTT signature.
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let newline = newline(text);
    let webvtt = is_webvtt(text);
    let lines: Vec<&str> = split_lines(text).map(str::trim_end).collect();
    let body = if webvtt {
        &lines[header_len(&lines)..]
    } else {
        &lines[..]
    };

    let mut ticker = Ticker { cancel, count: 0 };
    let mut reader = Reader {
        webvtt,
        cues: Vec::new(),
        continues: false,
    };
    for block in body.split(|line| line.is_empty()) {
        if !block.is_empty() {
            ticker.tick()?;
            reader.block(block, &mut ticker)?;
        }
    }
    let cues = normalize(reader.cues, webvtt, &mut ticker)?;
    Ok(Document {
        cues,
        newline,
        webvtt,
    })
}

/// True when `line` has some text that isn't whitespace: what every line
/// of a normalized cue has.
pub(crate) fn has_text(line: &[Span]) -> bool {
    line.iter()
        .any(|span| matches!(span, Span::Text(text) if !text.trim().is_empty()))
}

/// `CrLf` if the first line break is `\r\n`, else `Lf`.
fn newline(text: &str) -> Newline {
    match text.find(['\r', '\n']) {
        Some(at) if text[at..].starts_with("\r\n") => Newline::CrLf,
        _ => Newline::Lf,
    }
}

/// The lines of `text`, split at `\r\n`, `\n` or a lone `\r`.
fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n')
        .flat_map(|line| line.strip_suffix('\r').unwrap_or(line).split('\r'))
}

/// True for `WEBVTT` followed by the end of the text, a space, a tab or a
/// line break.
fn is_webvtt(text: &str) -> bool {
    text.strip_prefix("WEBVTT")
        .is_some_and(|rest| matches!(rest.chars().next(), None | Some(' ' | '\t' | '\r' | '\n')))
}

/// How many lines a WebVTT header takes: those before the first blank
/// line, or before a line with `-->`, which starts the first cue.
fn header_len(lines: &[&str]) -> usize {
    lines
        .iter()
        .position(|line| line.is_empty() || line.contains(ARROW))
        .unwrap_or(lines.len())
}

/// Checks the cancel token every [`CHECK_EVERY`] ticks, from the first.
struct Ticker<'a> {
    cancel: &'a CancelToken,
    count: usize,
}

impl Ticker<'_> {
    /// Counts one block or cue.
    fn tick(&mut self) -> Result<(), ParseError> {
        let due = self.count.is_multiple_of(CHECK_EVERY);
        self.count = self.count.wrapping_add(1);
        if due && self.cancel.is_cancelled() {
            Err(ParseError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// A cue as read: its timing and its raw text lines.
struct RawCue<'a> {
    start_ms: u64,
    end_ms: u64,
    lines: Vec<&'a str>,
}

/// Reads blocks (runs of non-blank lines) into cues.
struct Reader<'a> {
    webvtt: bool,
    cues: Vec<RawCue<'a>>,
    /// True when a block without a timing line continues the last cue:
    /// false before the first cue and after a dropped one, whose text that
    /// block would be.
    continues: bool,
}

impl<'a> Reader<'a> {
    fn block(&mut self, block: &[&'a str], ticker: &mut Ticker) -> Result<(), ParseError> {
        if self.webvtt && block.first().is_some_and(|line| starts_non_cue_block(line)) {
            return Ok(());
        }
        let Some(first) = block.iter().position(|line| line.contains(ARROW)) else {
            // A block of digits is a stray index; anything else is text the
            // blank line cut off from its cue.
            if !block.iter().all(|line| is_digits(line)) && self.continues {
                if let Some(cue) = self.cues.last_mut() {
                    cue.lines.extend_from_slice(block);
                }
            }
            return Ok(());
        };
        // The lines before the first timing line are its cue's identifier.
        let mut rest = &block[first..];
        while let Some((timing, after)) = rest.split_first() {
            ticker.tick()?;
            let next = after
                .iter()
                .position(|line| line.contains(ARROW))
                .unwrap_or(after.len());
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
                    self.cues.push(RawCue {
                        start_ms,
                        end_ms,
                        lines: text.to_vec(),
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

/// Turns the raw cues into spans and normalizes them: sorted by start
/// (stable), each lasting at least 1 ms, without lines that have no
/// visible text, and without cues left with no lines.
fn normalize(raw: Vec<RawCue>, webvtt: bool, ticker: &mut Ticker) -> Result<Vec<Cue>, ParseError> {
    let mut cues = Vec::with_capacity(raw.len());
    for cue in raw {
        ticker.tick()?;
        let lines: Vec<Vec<Span>> = cue
            .lines
            .iter()
            .map(|line| spans(line, webvtt))
            .filter(|line| has_text(line))
            .collect();
        if lines.is_empty() {
            continue;
        }
        let end_ms = if cue.end_ms <= cue.start_ms {
            cue.start_ms.saturating_add(1)
        } else {
            cue.end_ms
        };
        cues.push(Cue {
            start_ms: cue.start_ms,
            end_ms,
            lines,
        });
    }
    cues.sort_by_key(|cue| cue.start_ms);
    if cues.is_empty() {
        Err(ParseError::NoCues)
    } else {
        Ok(cues)
    }
}

/// One line of cue text as spans. Only these are markup (names in any
/// case); everything else is literal text:
/// - `<i>`, `<b>`, `<u>` (also with WebVTT classes, `<i.loud>`) and their
///   end tags, and ASS `{\i1}`/`{\i0}` and the same for `b` and `u`, open
///   and close styles. Other ASS overrides (`{\an8}`, `{\pos(10,20)}`) are
///   dropped.
/// - `<font …>`, WebVTT `<c…>`, `<v…>`, `<lang…>`, `<ruby>` and their end
///   tags, and inline timestamps (`<00:00:01.000>`), are dropped and their
///   text kept. `<rt>` and `</rt>` become "(" and ")", so ruby text reads
///   "漢(かん)"; an `<rt>` still open at `</ruby>` or at the end of the line
///   gets its ")".
/// - In WebVTT only: `&amp;`, `&lt;`, `&gt;`, `&nbsp;`, `&lrm;`, `&rlm;`
///   and numeric references.
fn spans(line: &str, webvtt: bool) -> Vec<Span> {
    let mut out = Spans::default();
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
            entity(rest).map(|(len, c)| {
                out.text(c.encode_utf8(&mut [0; 4]));
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
    if out.ruby_text {
        out.text(")");
    }
    out.spans
}

/// The spans of one line as they are built.
#[derive(Default)]
struct Spans {
    spans: Vec<Span>,
    /// Inside `<rt>`, whose ")" is still to come.
    ruby_text: bool,
}

impl Spans {
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

/// The WebVTT character reference at the start of `rest`: its length and
/// the character, or `None` if it isn't one Grape decodes.
fn entity(rest: &str) -> Option<(usize, char)> {
    let body = &rest[1..];
    let len = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
        .unwrap_or(body.len());
    if !body[len..].starts_with(';') {
        return None;
    }
    let c = match body[..len].to_ascii_lowercase().as_str() {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "nbsp" => '\u{A0}',
        "lrm" => '\u{200E}',
        "rlm" => '\u{200F}',
        name => numeric(name)?,
    };
    Some((len + 2, c))
}

/// The character a numeric reference (`#65` or `#x41`, lowercased) names.
/// `None` for a number that isn't a Unicode scalar value, and for NUL and
/// line breaks, which a line of cue text can't hold.
fn numeric(name: &str) -> Option<char> {
    let digits = name.strip_prefix('#')?;
    let code = match digits.strip_prefix('x') {
        Some(hex) => in_radix(hex, 16),
        None => in_radix(digits, 10),
    }?;
    char::from_u32(code).filter(|c| !matches!(c, '\0' | '\n' | '\r'))
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

    use super::*;

    fn parsed(text: &str) -> Document {
        parse(text, &CancelToken::new()).unwrap()
    }

    fn text(s: &str) -> Span {
        Span::Text(s.to_owned())
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
        let lines: Vec<&str> = split_lines("a\r\nb\nc\rd\r\r\ne").collect();
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
    }

    #[test]
    fn invalid_or_unknown_references_stay_literal() {
        let literal =
            "&#xD800; &#1114112; &#99999999999; &#; &#x; &#xZZ; &#0; &#10; &#xD; &bogus; &amp &;";
        assert_eq!(spans(literal, true), [text(literal)]);
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
    fn a_cue_that_doesnt_end_after_it_starts_lasts_one_millisecond() {
        let srt =
            "1\n00:00:02,000 --> 00:00:01,000\nBackwards\n\n2\n00:00:03,000 --> 00:00:03,000\nZero";
        assert_eq!(
            plain(&parsed(srt)),
            [cue(2000, 2001, &["Backwards"]), cue(3000, 3001, &["Zero"])]
        );
        let max = u64::MAX;
        let raw = vec![RawCue {
            start_ms: max,
            end_ms: max,
            lines: vec!["End of time"],
        }];
        let token = CancelToken::new();
        let mut ticker = Ticker {
            cancel: &token,
            count: 0,
        };
        let cues = normalize(raw, false, &mut ticker).unwrap();
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
        let token = CancelToken::new();
        for text in [
            "",
            "\n\n",
            "WEBVTT",
            "WEBVTT\n\nNOTE nothing here",
            "Just some text\nwith no timing",
            "1\n00:00:01,000 --> 00:00:02,000\n\n",
            "1\nnot --> timing\nText",
        ] {
            assert_eq!(parse(text, &token), Err(ParseError::NoCues), "{text:?}");
        }
    }

    #[test]
    fn a_cancelled_token_returns_cancelled() {
        let token = CancelToken::new();
        token.cancel();
        assert_eq!(
            parse("1\n00:00:01,000 --> 00:00:02,000\nHi", &token),
            Err(ParseError::Cancelled)
        );
        assert_eq!(parse("", &token), Err(ParseError::Cancelled));
        // The ticker itself stops at its next check.
        let token = CancelToken::new();
        let mut ticker = Ticker {
            cancel: &token,
            count: 1,
        };
        for _ in 1..CHECK_EVERY {
            ticker.tick().unwrap();
        }
        token.cancel();
        assert_eq!(ticker.tick(), Err(ParseError::Cancelled));
    }

    #[test]
    fn many_cues_parse_across_cancel_checks() {
        let srt: String = (0..1000u64)
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
        assert_eq!(doc.cues.len(), 1000);
        assert!(doc.cues.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
    }
}
