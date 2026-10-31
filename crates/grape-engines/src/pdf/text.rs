//! PDF → TXT and MD: PDFium's characters, turned into text.
//!
//! [`page_glyphs`] reads one page's characters from PDFium. The rest is
//! pure and works on [`Glyph`]s:
//!
//! - **TXT** ([`Text::txt`]): each page's characters in PDFium's order,
//!   PDFium's generated line breaks as `\n`, other control characters
//!   dropped; pages joined with a blank line (no form feed); UTF-8 without
//!   a byte-order mark, LF line ends.
//! - **MD** ([`Text::md`]): characters become lines (at a line break, or
//!   where the baseline jumps), lines become paragraphs (a gap of more than
//!   1.5 times the page's usual line spacing, or a change of size, starts a
//!   new one). The body size is the most common size by characters; the
//!   three largest distinct sizes at least 1.15 times the body size become
//!   `#`, `##` and `###` (smaller heading sizes also `###`). A page break
//!   ends a paragraph; there is no `---`. Bullets become `- `, and
//!   Markdown's special characters are escaped.
//! - **Rotated text** (a margin stamp, a table header turned on its side):
//!   a character's size is its font size times the length of its matrix's
//!   vertical axis, so turning text keeps its size. Text that runs more up
//!   or down than across ("vertical") makes lines of its own, split where
//!   its x jumps; those lines are always paragraphs of their own, never
//!   headings, and don't count towards the body size.
//!
//! A [`Text`] keeps only what its format needs, so the text bound counts
//! the output's own text. Text comes in the order it is written in the
//! file, so multi-column pages and tables can interleave. A document with
//! no text at all is refused ("it may be a scan"): OCR is v2.

use std::collections::BTreeMap;

use pdfium_render::prelude::{PdfPage, PdfiumError};

use super::protocol::TextFormat;

/// One character of a page, as PDFium reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Glyph {
    /// The character.
    pub(crate) ch: char,
    /// Its size in points, any scaling or rotation included
    /// ([`glyph_shape`]).
    pub(crate) size: f32,
    /// Its origin on the page, in points (y grows upwards).
    pub(crate) x: f32,
    /// See `x`.
    pub(crate) y: f32,
    /// PDFium inserted it (a space or a line break between text runs).
    pub(crate) generated: bool,
    /// Its baseline runs more across than up or down (upside-down text
    /// included).
    pub(crate) horizontal: bool,
}

/// Reads every character of `page`, in PDFium's order.
pub(crate) fn page_glyphs(page: &PdfPage) -> Result<Vec<Glyph>, PdfiumError> {
    let text = page.text()?;
    let chars = text.chars();
    let mut glyphs = Vec::with_capacity(chars.len());
    for ch in chars.iter() {
        let Some(c) = ch.unicode_char() else {
            continue;
        };
        let (x, y) = ch
            .origin()
            .map(|(x, y)| (x.value, y.value))
            .unwrap_or((0.0, 0.0));
        let matrix = ch.matrix().ok().map(|m| [m.a(), m.b(), m.c(), m.d()]);
        let (size, horizontal) = glyph_shape(ch.unscaled_font_size().value, matrix);
        glyphs.push(Glyph {
            ch: c,
            size,
            x,
            y,
            generated: ch.is_generated().unwrap_or(false),
            horizontal,
        });
    }
    Ok(glyphs)
}

/// A character's size and direction from its font size and its matrix
/// `[a, b, c, d]`. The size is the font size times the length of the
/// matrix's vertical axis `(c, d)`: pdfium-render's `scaled_font_size`
/// multiplies by `d` alone, which is 0 for text turned 90° and negative for
/// text upside down. The text is horizontal when its baseline `(a, b)`
/// runs more across than up or down. Without a matrix: the font size,
/// horizontal.
pub(crate) fn glyph_shape(font_size: f32, matrix: Option<[f32; 4]>) -> (f32, bool) {
    match matrix {
        Some([a, b, c, d]) => (font_size * c.hypot(d), a.abs() >= b.abs()),
        None => (font_size, true),
    }
}

/// A line of text, for Markdown.
#[derive(Debug, Clone, PartialEq)]
struct Line {
    /// The text, without control characters or trailing spaces.
    text: String,
    /// The most common size of its visible characters, in half points.
    size: u32,
    /// The baseline of its first visible character.
    y: f32,
    /// Its first visible character is horizontal. A vertical line is never
    /// a heading and never joins another line.
    horizontal: bool,
}

/// Where a line's first visible character sits, and how it runs.
#[derive(Debug, Clone, Copy)]
struct Start {
    x: f32,
    y: f32,
    size: f32,
    horizontal: bool,
}

impl Start {
    fn of(glyph: &Glyph) -> Start {
        Start {
            x: glyph.x,
            y: glyph.y,
            size: glyph.size,
            horizontal: glyph.horizontal,
        }
    }

    /// True when `glyph` can't be on this line: it runs the other way, or
    /// it is off the line by more than half the larger size, measured
    /// across the line (y for horizontal text, x for vertical).
    fn leaves(&self, glyph: &Glyph) -> bool {
        if glyph.horizontal != self.horizontal {
            return true;
        }
        let jump = if self.horizontal {
            glyph.y - self.y
        } else {
            glyph.x - self.x
        };
        jump.abs() > 0.5 * self.size.max(glyph.size)
    }
}

/// A size rounded to half points, the unit sizes are compared in.
fn half_points(size: f32) -> u32 {
    if size.is_finite() && size > 0.0 {
        // In range: sizes are clamped to 10,000 pt first.
        (size.min(10_000.0) * 2.0).round() as u32
    } else {
        0
    }
}

/// True for a character that ends a line.
fn is_break(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Splits one page's glyphs into lines: at a line break, where the text
/// turns, or where it moves off the line by more than half the larger of
/// the two sizes ([`Start::leaves`]).
fn lines(glyphs: &[Glyph]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut text = String::new();
    let mut sizes: BTreeMap<u32, usize> = BTreeMap::new();
    let mut baseline: Option<Start> = None;
    let mut finish =
        |text: &mut String, sizes: &mut BTreeMap<u32, usize>, baseline: Option<Start>| {
            let trimmed = text.trim_end();
            if !trimmed.trim_start().is_empty() {
                lines.push(Line {
                    text: trimmed.to_string(),
                    size: mode(sizes).unwrap_or(0),
                    y: baseline.map(|start| start.y).unwrap_or(0.0),
                    horizontal: baseline.is_none_or(|start| start.horizontal),
                });
            }
            text.clear();
            sizes.clear();
        };
    for glyph in glyphs {
        if is_break(glyph.ch) {
            finish(&mut text, &mut sizes, baseline.take());
            continue;
        }
        if glyph.ch.is_control() && glyph.ch != '\t' {
            continue;
        }
        let visible = !glyph.ch.is_whitespace();
        if visible && !glyph.generated {
            if baseline.is_some_and(|start| start.leaves(glyph)) {
                finish(&mut text, &mut sizes, baseline.take());
            }
            if baseline.is_none() {
                baseline = Some(Start::of(glyph));
            }
            *sizes.entry(half_points(glyph.size)).or_default() += 1;
        }
        if text.is_empty() && !visible {
            continue;
        }
        text.push(if glyph.ch == '\t' { ' ' } else { glyph.ch });
    }
    finish(&mut text, &mut sizes, baseline);
    lines
}

/// The most common key by count; the smaller key wins a tie.
fn mode(counts: &BTreeMap<u32, usize>) -> Option<u32> {
    let mut best: Option<(u32, usize)> = None;
    for (&key, &count) in counts {
        if best.is_none_or(|(_, n)| count > n) {
            best = Some((key, count));
        }
    }
    best.map(|(key, _)| key)
}

/// A whole document's text, page by page, built as pages are read. It
/// keeps only what its format needs.
#[derive(Debug)]
pub(crate) struct Text {
    /// What it is for.
    format: TextFormat,
    /// TXT: each page's text.
    pages: Vec<String>,
    /// MD: each page's lines.
    page_lines: Vec<Vec<Line>>,
    /// MD: visible horizontal characters per size (half points),
    /// document-wide.
    sizes: BTreeMap<u32, usize>,
    /// Visible characters in all.
    visible: usize,
    /// Bytes of text held so far.
    bytes: u64,
}

impl Text {
    /// An empty document's text, for `format`.
    pub(crate) fn new(format: TextFormat) -> Text {
        Text {
            format,
            pages: Vec::new(),
            page_lines: Vec::new(),
            sizes: BTreeMap::new(),
            visible: 0,
            bytes: 0,
        }
    }

    /// Adds the next page.
    pub(crate) fn add_page(&mut self, glyphs: &[Glyph]) {
        let mut page = String::new();
        let mut last_was_cr = false;
        for glyph in glyphs {
            match glyph.ch {
                '\r' => page.push('\n'),
                '\n' if last_was_cr => {}
                '\n' | '\u{2028}' | '\u{2029}' => page.push('\n'),
                '\t' => page.push('\t'),
                c if c.is_control() => {}
                c => {
                    if !c.is_whitespace() {
                        self.visible += 1;
                        if !glyph.generated && glyph.horizontal {
                            *self.sizes.entry(half_points(glyph.size)).or_default() += 1;
                        }
                    }
                    page.push(c);
                }
            }
            last_was_cr = glyph.ch == '\r';
        }
        match self.format {
            TextFormat::Txt => {
                self.bytes += page.len() as u64;
                self.pages.push(page);
            }
            TextFormat::Md => {
                let lines = lines(glyphs);
                self.bytes += lines.iter().map(|l| l.text.len() as u64).sum::<u64>();
                self.page_lines.push(lines);
            }
        }
    }

    /// The finished text in its format.
    pub(crate) fn output(&self) -> String {
        match self.format {
            TextFormat::Txt => self.txt(),
            TextFormat::Md => self.md(),
        }
    }

    /// True when no page had a visible character.
    pub(crate) fn is_empty(&self) -> bool {
        self.visible == 0
    }

    /// The bytes of text held so far (to bound memory).
    pub(crate) fn held_bytes(&self) -> u64 {
        self.bytes
    }

    /// Plain text: pages with text, trimmed, joined with a blank line,
    /// ending with a line break. Trailing spaces are removed from lines.
    fn txt(&self) -> String {
        let mut out = String::new();
        for page in &self.pages {
            let lines: Vec<&str> = page.lines().map(str::trim_end).collect();
            let text = lines.join("\n");
            let text = text.trim_matches('\n');
            if text.trim().is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(text);
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// Markdown (see the module notes).
    fn md(&self) -> String {
        let body = mode(&self.sizes).unwrap_or(0);
        let levels = heading_levels(&self.sizes, body);
        let mut blocks: Vec<String> = Vec::new();
        for lines in &self.page_lines {
            let spacing = line_spacing(lines);
            let mut current: Option<(Option<usize>, String)> = None;
            let mut previous: Option<&Line> = None;
            for line in lines {
                let level = levels
                    .get(&line.size)
                    .copied()
                    .filter(|_| line.horizontal && line.text.chars().count() <= 160);
                let joins = match (&current, previous) {
                    (Some((current_level, _)), Some(previous)) => {
                        let gap = previous.y - line.y;
                        line.horizontal
                            && previous.horizontal
                            && *current_level == level
                            && bullet(&line.text).is_none()
                            && previous.size == line.size
                            && gap > 0.0
                            && gap <= 1.5 * spacing.unwrap_or(1.2 * line.size as f32 / 2.0)
                    }
                    _ => false,
                };
                if joins {
                    if let Some((_, text)) = current.as_mut() {
                        join(text, &line.text);
                    }
                } else {
                    if let Some(block) = current.take() {
                        blocks.push(block_text(block));
                    }
                    current = Some((level, line.text.clone()));
                }
                previous = Some(line);
            }
            if let Some(block) = current.take() {
                blocks.push(block_text(block));
            }
        }
        let mut out = blocks.join("\n\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }
}

/// The heading level (1–3) of each size at least 1.15 times `body`: the
/// largest is 1, the next 2, the rest 3.
fn heading_levels(sizes: &BTreeMap<u32, usize>, body: u32) -> BTreeMap<u32, usize> {
    let threshold = body as f32 * 1.15;
    sizes
        .keys()
        .rev()
        .filter(|&&size| body > 0 && size as f32 >= threshold)
        .enumerate()
        .map(|(rank, &size)| (size, (rank + 1).min(3)))
        .collect()
}

/// The page's usual distance between baselines: the (lower) median of the
/// positive gaps between consecutive horizontal lines of the same size.
fn line_spacing(lines: &[Line]) -> Option<f32> {
    let mut gaps: Vec<f32> = lines
        .windows(2)
        .filter(|pair| pair[0].size == pair[1].size && pair[0].horizontal && pair[1].horizontal)
        .map(|pair| pair[0].y - pair[1].y)
        .filter(|gap| gap.is_finite() && *gap > 0.0)
        .collect();
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_by(f32::total_cmp);
    // The lower median: with two gaps, the smaller is the line spacing.
    Some(gaps[(gaps.len() - 1) / 2])
}

/// Appends the next line of a paragraph: directly after a hyphen, else
/// after a space. A soft hyphen at the end is dropped.
fn join(text: &mut String, next: &str) {
    if text.ends_with('\u{ad}') {
        text.pop();
    } else if !text.ends_with('-') {
        text.push(' ');
    }
    text.push_str(next.trim_start());
}

/// A block's Markdown: a heading, a list item or a paragraph.
fn block_text((level, text): (Option<usize>, String)) -> String {
    let text = text.trim();
    match level {
        Some(level) => format!("{} {}", "#".repeat(level), escape(text)),
        None => match bullet(text) {
            Some(rest) => format!("- {}", escape_start(&escape(rest))),
            None => escape_start(&escape(text)),
        },
    }
}

/// The rest of a line that starts with a bullet and a space.
fn bullet(text: &str) -> Option<&str> {
    ['•', '◦', '▪', '‣', '●', '○', '■', '–']
        .iter()
        .find_map(|&mark| {
            text.strip_prefix(mark)
                .filter(|rest| rest.starts_with(char::is_whitespace))
                .map(str::trim_start)
        })
}

/// Escapes Markdown's inline special characters.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escapes what would make a paragraph (or a list item's text) start a
/// heading, a list, a rule or a `~~~` code fence. (`>`, `*`, `_` and
/// backticks are escaped everywhere by [`escape`].)
fn escape_start(text: &str) -> String {
    let first = text.chars().next();
    if matches!(first, Some('#' | '+' | '-' | '=' | '~')) {
        return format!("\\{text}");
    }
    // "1. " or "1) " would start a numbered list.
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 9 {
        let rest = &text[digits..];
        if let Some(mark) = rest.chars().next().filter(|c| matches!(c, '.' | ')')) {
            if rest[mark.len_utf8()..].starts_with(' ') || rest.len() == mark.len_utf8() {
                return format!("{}\\{}", &text[..digits], rest);
            }
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A run of glyphs for `text` at (x, y) in `size` points, each 0.5 em
    /// wide, with PDFium-style generated breaks for `\n`.
    fn run(text: &str, x: f32, y: f32, size: f32) -> Vec<Glyph> {
        let mut glyphs = Vec::new();
        let mut cx = x;
        for ch in text.chars() {
            glyphs.push(Glyph {
                ch,
                size,
                x: cx,
                y,
                generated: ch == '\n' || ch == '\r',
                horizontal: true,
            });
            cx += size * 0.5;
        }
        glyphs
    }

    /// Lines at 12 pt every 14 pt from y = 700, with a generated CRLF
    /// between them.
    fn paragraph(lines: &[&str], top: f32, size: f32) -> Vec<Glyph> {
        let mut glyphs = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            glyphs.extend(run(line, 72.0, top - i as f32 * size * 1.2, size));
            glyphs.extend(run("\r\n", 72.0, top - i as f32 * size * 1.2, size));
        }
        glyphs
    }

    fn doc(format: TextFormat, pages: &[Vec<Glyph>]) -> Text {
        let mut text = Text::new(format);
        for page in pages {
            text.add_page(page);
        }
        text
    }

    fn txt(pages: &[Vec<Glyph>]) -> String {
        doc(TextFormat::Txt, pages).output()
    }

    fn md(pages: &[Vec<Glyph>]) -> String {
        doc(TextFormat::Md, pages).output()
    }

    /// `text` written upwards from (x, y), each character 0.5 em further
    /// up (a page's margin stamp turned 90°).
    fn upwards(text: &str, x: f32, y: f32, size: f32) -> Vec<Glyph> {
        run(text, x, y, size)
            .into_iter()
            .enumerate()
            .map(|(i, glyph)| Glyph {
                x,
                y: y + i as f32 * size * 0.5,
                horizontal: false,
                ..glyph
            })
            .collect()
    }

    #[test]
    fn txt_joins_pages_with_a_blank_line() {
        let pages = [
            paragraph(&["Hello  ", "world"], 700.0, 12.0),
            Vec::new(),
            paragraph(&["Page three\u{7}"], 700.0, 12.0),
        ];
        assert!(!doc(TextFormat::Txt, &pages).is_empty());
        assert_eq!(txt(&pages), "Hello\nworld\n\nPage three\n");
    }

    #[test]
    fn a_document_without_visible_text_is_empty() {
        let pages = [run(" \r\n \t", 0.0, 0.0, 12.0), Vec::new()];
        assert!(doc(TextFormat::Txt, &pages).is_empty());
        assert!(doc(TextFormat::Md, &pages).is_empty());
        assert_eq!(txt(&pages), "");
        assert_eq!(md(&pages), "");
    }

    #[test]
    fn md_finds_headings_by_size_and_paragraphs_by_gaps() {
        let mut page = Vec::new();
        page.extend(paragraph(&["Report"], 760.0, 24.0));
        page.extend(paragraph(&["Summary"], 720.0, 16.0));
        page.extend(paragraph(
            &["The first line of", "a paragraph that", "goes on."],
            690.0,
            12.0,
        ));
        // A bigger gap: a new paragraph.
        page.extend(paragraph(&["Second para_graph *here*."], 600.0, 12.0));
        page.extend(paragraph(&["Details"], 560.0, 14.0));
        page.extend(paragraph(&["Fine print"], 540.0, 12.5));
        page.extend(paragraph(
            &["Body again with more words in it."],
            520.0,
            12.0,
        ));
        assert_eq!(
            md(&[page]),
            "# Report\n\n## Summary\n\nThe first line of a paragraph that goes on.\n\n\
             Second para\\_graph \\*here\\*.\n\n### Details\n\nFine print\n\n\
             Body again with more words in it.\n"
        );
    }

    #[test]
    fn md_sizes_beyond_three_are_level_three() {
        let mut page = Vec::new();
        for (i, size) in [40.0, 32.0, 24.0, 18.0].iter().enumerate() {
            page.extend(paragraph(&["H"], 700.0 - i as f32 * 60.0, *size));
        }
        page.extend(paragraph(
            &["Body text that is the most common size by far."],
            400.0,
            10.0,
        ));
        assert_eq!(
            md(&[page]),
            "# H\n\n## H\n\n### H\n\n### H\n\nBody text that is the most common size by far.\n"
        );
    }

    #[test]
    fn md_breaks_paragraphs_at_page_breaks_and_handles_bullets_and_hyphens() {
        let first = paragraph(
            &[
                "A sentence that con-",
                "tinues, and a soft\u{ad}",
                "hyphen.",
            ],
            100.0,
            12.0,
        );
        let second = paragraph(
            &[
                "• First item",
                "• Second # item",
                "continued",
                "• # of users grew",
            ],
            700.0,
            12.0,
        );
        assert_eq!(
            md(&[first, second]),
            "A sentence that con-tinues, and a softhyphen.\n\n\
             - First item\n\n- Second # item continued\n\n- \\# of users grew\n"
        );
        // One paragraph per page: the starts are escaped, a `~~~` fence
        // too.
        let pages: Vec<Vec<Glyph>> = [
            "1. Not a list",
            "# not a heading",
            "- dash",
            "2) two",
            "~~~ Part two ~~~",
        ]
        .iter()
        .map(|line| paragraph(&[line], 700.0, 12.0))
        .collect();
        assert_eq!(
            md(&pages),
            "1\\. Not a list\n\n\\# not a heading\n\n\\- dash\n\n2\\) two\n\n\
             \\~~~ Part two ~~~\n"
        );
    }

    #[test]
    fn lines_split_where_the_baseline_jumps() {
        // Two runs with no break character between them, 20 pt apart.
        let mut glyphs = run("upper", 72.0, 700.0, 12.0);
        glyphs.extend(run("lower", 72.0, 680.0, 12.0));
        let found = lines(&glyphs);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].text, "upper");
        assert_eq!(found[1].size, 24);
        // A superscript a little higher stays on the line.
        let mut glyphs = run("x", 72.0, 700.0, 12.0);
        glyphs.extend(run("2", 78.0, 704.0, 8.0));
        assert_eq!(lines(&glyphs).len(), 1);
    }

    #[test]
    fn sizes_and_modes() {
        assert_eq!(half_points(12.0), 24);
        assert_eq!(half_points(12.2), 24);
        assert_eq!(half_points(f32::NAN), 0);
        assert_eq!(half_points(-3.0), 0);
        assert_eq!(half_points(1e30), 20_000);
        let counts = BTreeMap::from([(24, 5), (20, 5), (30, 1)]);
        assert_eq!(mode(&counts), Some(20));
        assert_eq!(mode(&BTreeMap::new()), None);
        let levels = heading_levels(&BTreeMap::from([(24, 100), (27, 1), (28, 1), (48, 1)]), 24);
        assert_eq!(levels, BTreeMap::from([(48, 1), (28, 2)]));
        assert!(heading_levels(&BTreeMap::from([(30, 1)]), 0).is_empty());
    }

    #[test]
    fn escaping() {
        assert_eq!(
            escape("a*b_c`d[e]<f>|g\\"),
            "a\\*b\\_c\\`d\\[e\\]\\<f\\>\\|g\\\\"
        );
        assert_eq!(escape_start("+ plus"), "\\+ plus");
        assert_eq!(escape_start("12. twelve"), "12\\. twelve");
        assert_eq!(escape_start("3.5 percent"), "3.5 percent");
        assert_eq!(escape_start("4."), "4\\.");
        assert_eq!(escape_start("plain"), "plain");
        assert_eq!(escape_start("~~~ fence"), "\\~~~ fence");
        // A list item's text gets the same start escapes.
        assert_eq!(block_text((None, "• 3. place".to_string())), "- 3\\. place");
        assert_eq!(block_text((None, "• - x".to_string())), "- \\- x");
    }

    #[test]
    fn rotated_text_keeps_its_size_and_direction() {
        // Upright, scaled through the matrix, turned 90°, upside down.
        assert_eq!(glyph_shape(12.0, Some([1.0, 0.0, 0.0, 1.0])), (12.0, true));
        assert_eq!(glyph_shape(1.0, Some([20.0, 0.0, 0.0, 20.0])), (20.0, true));
        assert_eq!(
            glyph_shape(20.0, Some([0.0, 1.0, -1.0, 0.0])),
            (20.0, false)
        );
        assert_eq!(
            glyph_shape(10.0, Some([-1.0, 0.0, 0.0, -1.0])),
            (10.0, true)
        );
        assert_eq!(glyph_shape(9.0, None), (9.0, true));
        // Up to 45° is horizontal; steeper is vertical.
        let half = std::f32::consts::FRAC_1_SQRT_2;
        assert!(glyph_shape(10.0, Some([half, half, -half, half])).1);
        let (size, horizontal) = glyph_shape(10.0, Some([0.6, 0.8, -0.8, 0.6]));
        assert!((size - 10.0).abs() < 1e-4 && !horizontal);
    }

    #[test]
    fn a_margin_stamp_turned_on_its_side_is_one_paragraph() {
        let mut page = paragraph(&["Title Here"], 720.0, 20.0);
        page.extend(paragraph(&["Body text line one."], 690.0, 10.0));
        // No break between the body and the stamp: the turn ends the line.
        page.extend(run("Body text line two.", 72.0, 678.0, 10.0));
        page.extend(upwards(
            "arXiv:2401.00001v1 [cs.LG] 1 Jan 2024",
            30.0,
            200.0,
            20.0,
        ));
        // A 20 pt stamp longer than the title doesn't move the body size,
        // and doesn't become a heading.
        assert_eq!(
            md(&[page]),
            "# Title Here\n\nBody text line one. Body text line two.\n\n\
             arXiv:2401.00001v1 \\[cs.LG\\] 1 Jan 2024\n"
        );
    }

    #[test]
    fn upside_down_text_stays_on_its_line() {
        let mut page = paragraph(&["Body text line one.", "Body text line two."], 690.0, 10.0);
        // Turned 180°: running right to left on one baseline.
        page.extend(
            run("Upside down", 400.0, 400.0, 10.0)
                .into_iter()
                .enumerate()
                .map(|(i, glyph)| Glyph {
                    x: 400.0 - i as f32 * 5.0,
                    ..glyph
                }),
        );
        assert_eq!(
            md(&[page]),
            "Body text line one. Body text line two.\n\nUpside down\n"
        );
    }

    #[test]
    fn text_counts_only_what_its_format_keeps() {
        let page = paragraph(&["Hello world"], 700.0, 12.0);
        let txt = doc(TextFormat::Txt, std::slice::from_ref(&page));
        assert_eq!(txt.held_bytes(), "Hello world\n".len() as u64);
        let md = doc(TextFormat::Md, std::slice::from_ref(&page));
        assert_eq!(md.held_bytes(), "Hello world".len() as u64);
        assert_eq!(md.output(), "Hello world\n");
    }
}
