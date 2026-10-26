//! Documents to WebVTT, SRT or plain text.
//!
//! - **Layout:** every line ends with the document's newline, the file
//!   ends with one, and cues are separated by exactly one blank line,
//!   never with one inside a cue. Cues are numbered 1 to n in order.
//! - **Timestamps** are `HH:MM:SS.mmm` (WebVTT) or `HH:MM:SS,mmm` (SRT),
//!   with more hour digits when needed.
//! - **Tags:** only `<i>`, `<b>` and `<u>` are written, balanced per cue:
//!   see [`tagged_lines`]. Each line is written as soon as it is rendered,
//!   so a cue is never held whole as text.
//! - **Text that would read back as structure** is made safe: WebVTT
//!   escapes `&`, `<` and `>`, and SRT writes a `-->` in text as `->`, so
//!   a text line can never become a timing line.
//! - **Cancellation:** `is_cancelled` is asked before anything is written
//!   and then every [`CHECK_EVERY`](super::CHECK_EVERY) lines; once it
//!   says so, the writer
//!   fails with an error.
//!
//! A line without visible text, and a cue without such lines, is left out,
//! so even a document that wasn't normalized can't produce a blank line in
//! a cue.

use std::io::{self, Write};

use super::parse::{has_text, Cue, Document, Span, Style};
use super::{Cancelled, Ticker};

impl From<Cancelled> for io::Error {
    fn from(_: Cancelled) -> Self {
        io::Error::other("cancelled")
    }
}

/// Writes `doc` as WebVTT: the signature, a blank line, then each cue's
/// number, timing line and escaped text. No BOM.
pub(crate) fn write_vtt(
    doc: &Document,
    out: &mut impl Write,
    is_cancelled: &mut dyn FnMut() -> bool,
) -> io::Result<()> {
    let nl = doc.newline.as_str();
    let mut ticker = Ticker::new(is_cancelled);
    ticker.tick()?;
    write!(out, "WEBVTT{nl}")?;
    for (i, cue) in visible_cues(doc).enumerate() {
        let (start, end) = (timestamp(cue.start_ms, '.'), timestamp(cue.end_ms, '.'));
        write!(out, "{nl}{}{nl}{start} --> {end}{nl}", i + 1)?;
        tagged_lines(cue, escape_vtt, &mut ticker, |line| {
            write!(out, "{line}{nl}")
        })?;
    }
    Ok(())
}

/// Writes `doc` as SRT, starting with a UTF-8 BOM if `bom`: each cue's
/// index, timing line and raw text.
pub(crate) fn write_srt(
    doc: &Document,
    out: &mut impl Write,
    bom: bool,
    is_cancelled: &mut dyn FnMut() -> bool,
) -> io::Result<()> {
    let nl = doc.newline.as_str();
    let mut ticker = Ticker::new(is_cancelled);
    ticker.tick()?;
    if bom {
        out.write_all(b"\xEF\xBB\xBF")?;
    }
    for (i, cue) in visible_cues(doc).enumerate() {
        if i > 0 {
            out.write_all(nl.as_bytes())?;
        }
        let (start, end) = (timestamp(cue.start_ms, ','), timestamp(cue.end_ms, ','));
        write!(out, "{}{nl}{start} --> {end}{nl}", i + 1)?;
        tagged_lines(cue, String::push_str, &mut ticker, |line| {
            write!(out, "{}{nl}", defuse_arrows(line))
        })?;
    }
    Ok(())
}

/// Writes the text of `doc`'s lines, one per line, without timestamps,
/// tags or blank lines. A line that repeats the line written before it
/// (compared trimmed) is left out, as rolling captions repeat each line.
pub(crate) fn write_txt(
    doc: &Document,
    out: &mut impl Write,
    is_cancelled: &mut dyn FnMut() -> bool,
) -> io::Result<()> {
    let nl = doc.newline.as_str();
    let mut ticker = Ticker::new(is_cancelled);
    ticker.tick()?;
    let mut previous = String::new();
    let mut text = String::new();
    for cue in &doc.cues {
        for line in &cue.lines {
            ticker.tick()?;
            text.clear();
            for span in line {
                if let Span::Text(t) = span {
                    text.push_str(t);
                }
            }
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed == previous {
                continue;
            }
            write!(out, "{text}{nl}")?;
            previous.clear();
            previous.push_str(trimmed);
        }
    }
    Ok(())
}

/// The cues with at least one line of visible text.
fn visible_cues(doc: &Document) -> impl Iterator<Item = &Cue> {
    doc.cues
        .iter()
        .filter(|cue| cue.lines.iter().any(|line| has_text(line)))
}

/// `ms` as `HH:MM:SS` and three-digit milliseconds after `separator`.
fn timestamp(ms: u64, separator: char) -> String {
    let hours = ms / 3_600_000;
    let rest = ms % 3_600_000;
    format!(
        "{hours:02}:{:02}:{:02}{separator}{:03}",
        rest / 60_000,
        rest / 1000 % 60,
        rest % 1000
    )
}

/// Renders the cue's lines with visible text one at a time and hands each
/// to `emit`, each line counted as work for `ticker`. `text` writes each
/// text span, and `<i>`, `<b>` and `<u>` tags are balanced across the cue:
/// - opening a style that is already open, or closing one that isn't,
///   writes nothing;
/// - closing a style that isn't the innermost closes the inner ones, then
///   it, then reopens the inner ones;
/// - styles still open at the end are closed, innermost first, at the end
///   of the last line.
fn tagged_lines(
    cue: &Cue,
    text: fn(&mut String, &str),
    ticker: &mut Ticker,
    mut emit: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    let Some(last) = cue.lines.iter().rposition(|line| has_text(line)) else {
        return Ok(());
    };
    let mut open: Vec<Style> = Vec::new();
    let mut rendered = String::new();
    for (i, line) in cue.lines.iter().enumerate() {
        ticker.tick()?;
        if !has_text(line) {
            continue;
        }
        rendered.clear();
        for span in line {
            match span {
                Span::Text(t) => text(&mut rendered, t),
                Span::Open(style) => {
                    if !open.contains(style) {
                        open.push(*style);
                        rendered.push_str(open_tag(*style));
                    }
                }
                Span::Close(style) => {
                    if let Some(at) = open.iter().position(|s| s == style) {
                        for inner in open[at..].iter().rev() {
                            rendered.push_str(close_tag(*inner));
                        }
                        open.remove(at);
                        for inner in &open[at..] {
                            rendered.push_str(open_tag(*inner));
                        }
                    }
                }
            }
        }
        if i == last {
            for style in open.iter().rev() {
                rendered.push_str(close_tag(*style));
            }
        }
        emit(&rendered)?;
    }
    Ok(())
}

fn open_tag(style: Style) -> &'static str {
    match style {
        Style::Italic => "<i>",
        Style::Bold => "<b>",
        Style::Underline => "<u>",
    }
}

fn close_tag(style: Style) -> &'static str {
    match style {
        Style::Italic => "</i>",
        Style::Bold => "</b>",
        Style::Underline => "</u>",
    }
}

/// Appends `text` to `out` escaped for WebVTT cue text.
fn escape_vtt(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// `line` with every `-->` written as `->`. A longer run of dashes before
/// `>` becomes `->` too, so no `-->` is left once it's done.
fn defuse_arrows(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains("-->") {
        return line.into();
    }
    let mut out = String::with_capacity(line.len());
    let mut dashes = 0;
    for c in line.chars() {
        match c {
            '-' => dashes += 1,
            '>' if dashes >= 2 => {
                out.push_str("->");
                dashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('-', dashes));
                out.push(c);
                dashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('-', dashes));
    out.into()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::decode::decode;
    use super::super::parse::{parse, Newline};
    use super::super::CHECK_EVERY;
    use super::*;

    fn text(s: &str) -> Span {
        Span::Text(s.to_owned())
    }

    fn doc(newline: Newline, cues: Vec<Cue>) -> Document {
        Document {
            cues,
            newline,
            webvtt: false,
        }
    }

    fn one_cue(lines: Vec<Vec<Span>>) -> Document {
        doc(
            Newline::Lf,
            vec![Cue {
                start_ms: 1000,
                end_ms: 2000,
                lines,
            }],
        )
    }

    fn vtt(doc: &Document) -> String {
        let mut out = Vec::new();
        write_vtt(doc, &mut out, &mut || false).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn srt(doc: &Document) -> String {
        let mut out = Vec::new();
        write_srt(doc, &mut out, false, &mut || false).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn txt(doc: &Document) -> String {
        let mut out = Vec::new();
        write_txt(doc, &mut out, &mut || false).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Decodes and parses `source`, as the convert step will.
    fn read(source: &[u8]) -> Document {
        let (text, _) = decode(source, &mut || false).unwrap();
        parse(&text, &mut || false).unwrap()
    }

    /// The text lines of a one-cue SRT, tags as written.
    fn srt_text(lines: Vec<Vec<Span>>) -> String {
        srt(&one_cue(lines))
            .strip_prefix("1\n00:00:01,000 --> 00:00:02,000\n")
            .unwrap()
            .to_owned()
    }

    // Rule 14: timestamps.

    #[test]
    fn timestamps_have_at_least_two_hour_digits_and_three_millisecond_digits() {
        assert_eq!(timestamp(0, '.'), "00:00:00.000");
        assert_eq!(timestamp(3_723_456, ','), "01:02:03,456");
        assert_eq!(timestamp(59_999, '.'), "00:00:59.999");
        assert_eq!(timestamp(360_000_000, ','), "100:00:00,000");
        assert_eq!(timestamp(u64::MAX, ','), "5124095576030:25:51,615");
    }

    #[test]
    fn hours_past_99_are_written_in_full() {
        let doc = read(b"1\n100:00:00,000 --> 123:04:05,006\nLong film");
        assert_eq!(
            vtt(&doc),
            "WEBVTT\n\n1\n100:00:00.000 --> 123:04:05.006\nLong film\n"
        );
        assert_eq!(srt(&doc), "1\n100:00:00,000 --> 123:04:05,006\nLong film\n");
    }

    // Rule 15: layout.

    #[test]
    fn every_line_uses_the_documents_newline_and_cues_have_one_blank_line_between() {
        let cues = vec![
            Cue {
                start_ms: 0,
                end_ms: 1,
                lines: vec![vec![text("a")], vec![text("b")]],
            },
            Cue {
                start_ms: 2,
                end_ms: 3,
                lines: vec![vec![text("c")]],
            },
        ];
        let crlf = doc(Newline::CrLf, cues);
        assert_eq!(
            vtt(&crlf),
            "WEBVTT\r\n\r\n1\r\n00:00:00.000 --> 00:00:00.001\r\na\r\nb\r\n\r\n2\r\n00:00:00.002 --> 00:00:00.003\r\nc\r\n"
        );
        assert_eq!(
            srt(&crlf),
            "1\r\n00:00:00,000 --> 00:00:00,001\r\na\r\nb\r\n\r\n2\r\n00:00:00,002 --> 00:00:00,003\r\nc\r\n"
        );
        assert_eq!(txt(&crlf), "a\r\nb\r\nc\r\n");
    }

    #[test]
    fn lines_and_cues_without_visible_text_are_never_written() {
        let doc = doc(
            Newline::Lf,
            vec![
                Cue {
                    start_ms: 0,
                    end_ms: 1000,
                    lines: vec![
                        vec![Span::Open(Style::Italic)],
                        vec![text("a")],
                        vec![text("  ")],
                        vec![],
                        vec![text("b")],
                    ],
                },
                Cue {
                    start_ms: 1000,
                    end_ms: 2000,
                    lines: vec![vec![text(" ")]],
                },
                Cue {
                    start_ms: 2000,
                    end_ms: 3000,
                    lines: vec![],
                },
                Cue {
                    start_ms: 3000,
                    end_ms: 4000,
                    lines: vec![vec![text("c")]],
                },
            ],
        );
        assert_eq!(
            vtt(&doc),
            "WEBVTT\n\n1\n00:00:00.000 --> 00:00:01.000\na\nb\n\n2\n00:00:03.000 --> 00:00:04.000\nc\n"
        );
        assert_eq!(
            srt(&doc),
            "1\n00:00:00,000 --> 00:00:01,000\na\nb\n\n2\n00:00:03,000 --> 00:00:04,000\nc\n"
        );
        assert_eq!(txt(&doc), "a\nb\nc\n");
    }

    // Rule 16: WebVTT.

    #[test]
    fn webvtt_starts_with_its_signature_and_numbers_its_cues() {
        let doc = read(b"5\n00:00:03,000 --> 00:00:04,000\nSecond\n\n9\n00:00:01,000 --> 00:00:02,000\nFirst\n");
        assert_eq!(
            vtt(&doc),
            "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.000\nFirst\n\n2\n00:00:03.000 --> 00:00:04.000\nSecond\n"
        );
    }

    #[test]
    fn webvtt_text_is_escaped_so_no_arrow_can_appear() {
        let doc = one_cue(vec![vec![
            Span::Open(Style::Bold),
            text("Tom & Jerry <3 --> a>b"),
            Span::Close(Style::Bold),
        ]]);
        let out = vtt(&doc);
        assert_eq!(
            out,
            "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.000\n<b>Tom &amp; Jerry &lt;3 --&gt; a&gt;b</b>\n"
        );
        assert_eq!(out.matches("-->").count(), 1);
    }

    // Rule 17: SRT.

    #[test]
    fn srt_starts_with_a_bom_only_when_asked() {
        let doc = one_cue(vec![vec![text("Hi")]]);
        let mut out = Vec::new();
        write_srt(&doc, &mut out, true, &mut || false).unwrap();
        assert_eq!(out, b"\xEF\xBB\xBF1\n00:00:01,000 --> 00:00:02,000\nHi\n");
        assert_eq!(srt(&doc), "1\n00:00:01,000 --> 00:00:02,000\nHi\n");
    }

    #[test]
    fn srt_text_is_raw_except_that_an_arrow_becomes_a_short_arrow() {
        assert_eq!(
            srt_text(vec![vec![text("Tom &amp; Jerry <3 a > b")]]),
            "Tom &amp; Jerry <3 a > b\n"
        );
        assert_eq!(
            srt_text(vec![vec![text("a --> b ---> c -> d - > e --")]]),
            "a -> b -> c -> d - > e --\n"
        );
        // Split across spans, the arrow is still caught.
        assert_eq!(srt_text(vec![vec![text("x--"), text(">y")]]), "x->y\n");
        assert_eq!(defuse_arrows("-->-->"), "->->");
        assert_eq!(defuse_arrows("no arrow"), "no arrow");
    }

    // Rule 18: balanced tags.

    #[test]
    fn styles_are_written_as_tags_and_closed_at_the_end_of_the_cue() {
        assert_eq!(
            srt_text(vec![
                vec![Span::Open(Style::Italic), text("one")],
                vec![Span::Open(Style::Bold), text("two")],
                vec![Span::Open(Style::Underline), text("three")],
            ]),
            "<i>one\n<b>two\n<u>three</u></b></i>\n"
        );
    }

    #[test]
    fn opening_an_open_style_or_closing_a_closed_one_writes_nothing() {
        assert_eq!(
            srt_text(vec![vec![
                Span::Close(Style::Bold),
                Span::Open(Style::Italic),
                text("a"),
                Span::Open(Style::Italic),
                text("b"),
                Span::Close(Style::Italic),
                Span::Close(Style::Italic),
                text("c"),
            ]]),
            "<i>ab</i>c\n"
        );
    }

    #[test]
    fn closing_an_outer_style_closes_and_reopens_the_inner_ones() {
        assert_eq!(
            srt_text(vec![vec![
                Span::Open(Style::Bold),
                Span::Open(Style::Italic),
                Span::Open(Style::Underline),
                text("a"),
                Span::Close(Style::Bold),
                text("b"),
            ]]),
            "<b><i><u>a</u></i></b><i><u>b</u></i>\n"
        );
    }

    #[test]
    fn tags_are_balanced_per_cue_not_per_document() {
        let doc = doc(
            Newline::Lf,
            vec![
                Cue {
                    start_ms: 0,
                    end_ms: 1,
                    lines: vec![vec![Span::Open(Style::Italic), text("a")]],
                },
                Cue {
                    start_ms: 2,
                    end_ms: 3,
                    lines: vec![vec![text("b"), Span::Close(Style::Italic)]],
                },
            ],
        );
        assert_eq!(
            vtt(&doc),
            "WEBVTT\n\n1\n00:00:00.000 --> 00:00:00.001\n<i>a</i>\n\n2\n00:00:00.002 --> 00:00:00.003\nb\n"
        );
    }

    // Rule 19: plain text.

    #[test]
    fn txt_has_only_the_text_and_skips_a_repeated_line() {
        let doc = read(
            b"WEBVTT\n\n00:01.000 --> 00:02.000\n<i>Hello</i> &amp; welcome\n<v Roger>Hi</v>\n\n\
              00:02.000 --> 00:03.000\n  Hi  \nNew line\n\n00:03.000 --> 00:04.000\nHello &amp; welcome\n",
        );
        assert_eq!(
            txt(&doc),
            "Hello & welcome\nHi\nNew line\nHello & welcome\n"
        );
        let crlf = read(b"1\r\n00:00:01,000 --> 00:00:02,000\r\n<b>a</b>\r\n");
        assert_eq!(txt(&crlf), "a\r\n");
    }

    // Cancellation.

    #[test]
    fn a_cancelled_job_makes_every_writer_fail() {
        let doc = one_cue(vec![vec![text("Hi")]]);
        let results = [
            write_vtt(&doc, &mut Vec::new(), &mut || true),
            write_srt(&doc, &mut Vec::new(), true, &mut || true),
            write_txt(&doc, &mut Vec::new(), &mut || true),
        ];
        for result in results {
            let err = result.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Other);
            assert_eq!(err.to_string(), "cancelled");
        }
        // Nothing at all is written, not even the BOM or the signature.
        let mut out = Vec::new();
        assert!(write_srt(&doc, &mut out, true, &mut || true).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn a_cancel_while_writing_stops_at_the_next_check() {
        let line = |i: usize| vec![text(&format!("line {i}"))];
        let n = CHECK_EVERY + 10;
        // Many lines in one cue, and as many one-line cues.
        let one_cue = doc(
            Newline::Lf,
            vec![Cue {
                start_ms: 0,
                end_ms: 1,
                lines: (0..n).map(line).collect(),
            }],
        );
        let many_cues = doc(
            Newline::Lf,
            (0..n)
                .map(|i| Cue {
                    start_ms: i as u64,
                    end_ms: i as u64 + 1,
                    lines: vec![line(i)],
                })
                .collect(),
        );
        type Writer = fn(&Document, &mut Vec<u8>, &mut dyn FnMut() -> bool) -> io::Result<()>;
        let writers: [Writer; 3] = [
            |doc, out, is_cancelled| write_vtt(doc, out, is_cancelled),
            |doc, out, is_cancelled| write_srt(doc, out, false, is_cancelled),
            |doc, out, is_cancelled| write_txt(doc, out, is_cancelled),
        ];
        for doc in [&one_cue, &many_cues] {
            for (w, writer) in writers.iter().enumerate() {
                // Not cancelled at the first check, before anything is
                // written; cancelled from the second on.
                let mut calls = 0;
                let mut is_cancelled = || {
                    calls += 1;
                    calls > 1
                };
                let mut out = Vec::new();
                let err = writer(doc, &mut out, &mut is_cancelled).unwrap_err();
                assert_eq!(err.to_string(), "cancelled");
                let written = String::from_utf8(out).unwrap();
                // The first check counted as one unit, each line as one.
                let lines = written.lines().filter(|l| l.starts_with("line ")).count();
                assert_eq!(lines, CHECK_EVERY - 1, "writer {w}");
            }
        }
    }

    // Whole conversions.

    #[test]
    fn an_srt_converts_to_exactly_this_webvtt() {
        let srt = "\u{FEFF}1\r\n\
                   00:00:01,000 --> 00:00:04,000\r\n\
                   <i>Hello</i>, world!\r\n\
                   Tom & Jerry <3\r\n\
                   \r\n\
                   2\r\n\
                   00:00:05,5 --> 00:00:07.25 X1:100 X2:200 Y1:10 Y2:20\r\n\
                   {\\an8}Second <b>cue</B>\r\n\
                   <font color=\"#ffff00\">yellow</font> &amp; raw\r\n\
                   \r\n\
                   3\r\n\
                   00:01:00,000 --> 00:01:02,000\r\n\
                   <b><i>unbalanced</b>\r\n";
        let doc = read(srt.as_bytes());
        assert_eq!(
            vtt(&doc),
            "WEBVTT\r\n\
             \r\n\
             1\r\n\
             00:00:01.000 --> 00:00:04.000\r\n\
             <i>Hello</i>, world!\r\n\
             Tom &amp; Jerry &lt;3\r\n\
             \r\n\
             2\r\n\
             00:00:05.500 --> 00:00:07.250\r\n\
             Second <b>cue</b>\r\n\
             yellow &amp;amp; raw\r\n\
             \r\n\
             3\r\n\
             00:01:00.000 --> 00:01:02.000\r\n\
             <b><i>unbalanced</i></b><i></i>\r\n"
        );
    }

    #[test]
    fn a_webvtt_converts_to_exactly_this_srt() {
        let vtt = "WEBVTT - Example\n\
                   Kind: captions\n\
                   Language: en\n\
                   \n\
                   NOTE This is a comment\n\
                   \n\
                   STYLE\n\
                   ::cue { color: yellow }\n\
                   \n\
                   intro\n\
                   00:01.000 --> 00:03.000 align:start position:10%\n\
                   <v Roger>Hi &amp; welcome</v>\n\
                   <c.yellow>Line two</c> --&gt; next\n\
                   \n\
                   00:00:04.000 --> 00:00:05.000\n\
                   <ruby>漢<rt>かん</rt></ruby> &lt;tag&gt;<00:00:04.500> <i.loud>loud</i>\n";
        let doc = read(vtt.as_bytes());
        assert!(doc.webvtt);
        assert_eq!(
            srt(&doc),
            "1\n\
             00:00:01,000 --> 00:00:03,000\n\
             Hi & welcome\n\
             Line two -> next\n\
             \n\
             2\n\
             00:00:04,000 --> 00:00:05,000\n\
             漢(かん) <tag> <i>loud</i>\n"
        );
    }

    #[test]
    fn srt_to_webvtt_to_srt_keeps_text_and_timings() {
        let original = "1\n00:00:01,000 --> 00:00:02,500\n<i>Tom & Jerry</i> &amp; <br> 1 < 2 > 0\nsecond line\n\n\
                        2\n00:00:03,000 --> 01:00:00,000\n<b>bold <u>under</u></b> {not ass}\n\n\
                        3\n100:00:00,001 --> 100:00:00,002\n2024\n";
        let doc = read(original.as_bytes());
        let as_vtt = vtt(&doc);
        let back = read(as_vtt.as_bytes());
        assert_eq!(back.cues, doc.cues);
        assert_eq!(srt(&back), original);
    }
}
