//! Doc-sync tests: parse the spec tables straight from `docs/` and compare
//! them with the code, so any drift between the two fails the build.
//!
//! The parser only reads `|` table rows and a few rule lines it looks for
//! by their exact wording; prose and `[Grape]` bullets around them are
//! ignored.
#![allow(clippy::unwrap_used)]

use std::collections::HashSet;

use grape_core::geometry::{GEOMETRY, SCREEN_MARGIN};
use grape_core::{
    actions, pair_tier, pairs, targets, tool_naming, tool_suffix, tool_tier, tools, Category,
    Channels, Features, Format, OutputExt, Platform, Suffix, Tier, ToolKey, ToolParams, INPUT_ONLY,
    TOOL_ROWS,
};

const FORMAT_MATRIX: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/FORMAT_MATRIX.md"
));
const TOOLS_AND_ACTIONS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/TOOLS_AND_ACTIONS.md"
));
const INTERACTION_SPEC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/INTERACTION_SPEC.md"
));

// ---- A tiny markdown reader ----

/// The body of the section whose heading line starts with `heading`, up to
/// the next heading of the same or a higher level.
fn section<'a>(doc: &'a str, heading: &str) -> &'a str {
    let level = heading.chars().take_while(|&c| c == '#').count();
    let mut offset = 0;
    let mut start = None;
    for line in doc.split_inclusive('\n') {
        match start {
            None if line.starts_with(heading) => start = Some(offset + line.len()),
            None => {}
            Some(body) => {
                let hashes = line.chars().take_while(|&c| c == '#').count();
                if hashes > 0 && hashes <= level && line[hashes..].starts_with(' ') {
                    return &doc[body..offset];
                }
            }
        }
        offset += line.len();
    }
    let start = start.unwrap_or_else(|| panic!("heading {heading:?} not found"));
    &doc[start..]
}

/// The table rows in `text` as trimmed cells, without separator rows and
/// without header rows (whose first cell is `header`).
fn rows(text: &str, header: &str) -> Vec<Vec<String>> {
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with('|'))
        .map(|line| {
            let inner = line.strip_prefix('|').unwrap();
            let inner = inner.strip_suffix('|').unwrap_or(inner);
            inner.split('|').map(|c| c.trim().to_string()).collect()
        })
        .filter(|cells: &Vec<String>| {
            let separator = cells
                .iter()
                .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'));
            !separator && cells.first().map(String::as_str) != Some(header)
        })
        .collect()
}

/// The `code` spans in `s`, in order.
fn backticked(s: &str) -> Vec<&str> {
    s.split('`').skip(1).step_by(2).collect()
}

/// The single line of `doc` that contains `needle`.
fn line_with<'a>(doc: &'a str, needle: &str) -> &'a str {
    let found: Vec<&str> = doc.lines().filter(|l| l.contains(needle)).collect();
    assert_eq!(found.len(), 1, "expected one line containing {needle:?}");
    found[0]
}

/// The integer right after `label` in `line`.
fn number_after(line: &str, label: &str) -> usize {
    let at = line
        .find(label)
        .unwrap_or_else(|| panic!("{label:?} not in {line:?}"))
        + label.len();
    let digits: String = line[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap()
}

fn format(key: &str) -> Format {
    Format::ALL
        .iter()
        .copied()
        .find(|f| f.key() == key)
        .unwrap_or_else(|| panic!("unknown format key {key:?}"))
}

fn tool(key: &str) -> ToolKey {
    ToolKey::from_key(key).unwrap_or_else(|| panic!("unknown tool key {key:?}"))
}

/// `v1`, `v2` or `later`, via the serde keys.
fn tier(s: &str) -> Tier {
    serde_json::from_str(&format!("\"{s}\"")).unwrap_or_else(|_| panic!("bad tier {s:?}"))
}

/// `docx<sup>v2</sup>` → (docx, V2); `jpg` → (jpg, V1).
fn target_entry(token: &str) -> (Format, Tier) {
    match token.split_once("<sup>") {
        Some((key, rest)) => (format(key), tier(rest.trim_end_matches("</sup>"))),
        None => (format(token), Tier::V1),
    }
}

/// `— (empty)` → []; `a, b` → [a, b].
fn tool_list(cell: &str) -> Vec<ToolKey> {
    if cell.starts_with('—') {
        Vec::new()
    } else {
        cell.split(',').map(|k| tool(k.trim())).collect()
    }
}

fn offered_anywhere(t: ToolKey, f: Format) -> bool {
    tools(f, 1).contains(&t) || tools(f, 2).contains(&t)
}

// ---- FORMAT_MATRIX.md ----

#[test]
fn conversion_tables_match_the_matrix() {
    let table = rows(section(FORMAT_MATRIX, "## Conversion targets"), "Input");
    assert_eq!(table.len(), 45, "one row per input");
    for (i, row) in table.iter().enumerate() {
        let from = format(backticked(&row[0])[0]);
        assert_eq!(from, Format::ALL[i], "rows are in Format::ALL order");
        let count: usize = row[1].parse().unwrap();
        let entries: Vec<(Format, Tier)> = row[2].split_whitespace().map(target_entry).collect();
        assert_eq!(entries.len(), count, "{from:?}: # column vs targets");
        let doc_targets: Vec<Format> = entries.iter().map(|e| e.0).collect();
        assert_eq!(targets(from), doc_targets.as_slice(), "{from:?} targets");
        for &(to, t) in &entries {
            assert_eq!(pair_tier(from, to), Some(t), "{from:?} -> {to:?} tier");
        }
        let doc_tiers: HashSet<Tier> = row[3].split('/').map(tier).collect();
        let row_tiers: HashSet<Tier> = entries.iter().map(|e| e.1).collect();
        assert_eq!(doc_tiers, row_tiers, "{from:?} Tier column");
    }
}

#[test]
fn conversion_subtables_are_the_nine_groups() {
    let body = section(FORMAT_MATRIX, "## Conversion targets");
    let groups: Vec<&str> = body
        .lines()
        .filter_map(|l| l.strip_prefix("### "))
        .collect();
    assert_eq!(
        groups,
        [
            "Images",
            "Audio",
            "Video",
            "PDF",
            "Slides",
            "Spreadsheets",
            "Documents",
            "Subtitles",
            "Archives"
        ]
    );
}

#[test]
fn total_line_matches_pair_counts() {
    let line = line_with(FORMAT_MATRIX, "**Total:");
    let total = number_after(line, "**Total: ");
    let v1 = number_after(line, "v1: ");
    let v2 = number_after(line, "v2: ");
    let later = number_after(line, "later: ");
    assert_eq!(pairs(Features::FULL).len(), total);
    assert_eq!(pairs(Features::V1_ONLY).len(), v1);
    let only_v2 = Features {
        v2: true,
        later: false,
    };
    assert_eq!(pairs(only_v2).len() - v1, v2);
    assert_eq!(total, v1 + v2 + later);
}

#[test]
fn input_only_rule_matches() {
    let line = line_with(FORMAT_MATRIX, "**Input-only formats**");
    let doc: Vec<Format> = backticked(line).into_iter().map(format).collect();
    assert_eq!(doc, INPUT_ONLY.to_vec());
}

#[test]
fn alias_rule_matches() {
    let line = line_with(FORMAT_MATRIX, "Sniff magic bytes");
    let (observed, grape) = line.split_once("**[Grape]**").unwrap();
    for name in backticked(observed) {
        if name != "gz" {
            assert_eq!(
                Format::from_file_name(&format!("x{name}")),
                Some(Format::Gz),
                "{name}"
            );
        }
    }
    let aliases = grape.split_once("and use ").unwrap().1;
    let mut seen = 0;
    for pair in aliases.split(", ") {
        let names = backticked(pair);
        assert_eq!(names.len(), 2, "{pair:?}");
        assert_eq!(
            Format::from_extension(names[0]),
            Some(format(names[1])),
            "{pair:?}"
        );
        seen += 1;
    }
    assert_eq!(seen, 5);
}

#[test]
fn display_name_and_gz_rules_match() {
    let line = line_with(FORMAT_MATRIX, "Display names are uppercase");
    assert_eq!(backticked(line), ["gz"]);
    assert!(line.contains(&format!("**{}**", Format::Gz.display_name())));
    let line = line_with(FORMAT_MATRIX, "Converting to `gz` writes");
    let written = backticked(line)[1];
    assert_eq!(written, format!("name.{}", Format::Gz.output_ext()));
}

#[test]
fn category_line_matches() {
    let line = FORMAT_MATRIX
        .lines()
        .find(|l| l.starts_with("audio → "))
        .unwrap();
    let mut seen = 0;
    for part in line.split(" · ") {
        let (name, list) = part.split_once(" → ").unwrap();
        let category: Category = serde_json::from_str(&format!("\"{name}\"")).unwrap();
        for key in backticked(list)[0].split_whitespace() {
            assert_eq!(format(key).category(), category, "{key}");
            seen += 1;
        }
    }
    assert_eq!(seen, Format::ALL.len(), "every format is categorized once");
}

#[test]
fn tools_per_input_matches_tool_rows() {
    let table = rows(section(FORMAT_MATRIX, "## Tools per input"), "Input(s)");
    let (listed, rest): (Vec<_>, Vec<_>) = table.iter().partition(|r| r[0].starts_with('`'));
    assert_eq!(listed.len(), TOOL_ROWS.len());
    for (row, code) in listed.iter().zip(TOOL_ROWS) {
        let formats: Vec<Format> = backticked(&row[0])[0]
            .split_whitespace()
            .map(format)
            .collect();
        assert_eq!(formats, code.formats);
        assert_eq!(tool_list(&row[1]), code.single, "{formats:?} single");
        assert_eq!(tool_list(&row[2]), code.multi, "{formats:?} multi");
        for f in formats {
            assert_eq!(tools(f, 1), tool_list(&row[1]));
            assert_eq!(tools(f, 2), tool_list(&row[2]));
        }
    }
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0][0], "everything else");
    assert!(tool_list(&rest[0][1]).is_empty() && tool_list(&rest[0][2]).is_empty());
    let in_rows: HashSet<Format> = TOOL_ROWS
        .iter()
        .flat_map(|r| r.formats.iter().copied())
        .collect();
    for f in Format::ALL.iter().filter(|f| !in_rows.contains(f)) {
        assert!(tools(*f, 1).is_empty() && tools(*f, 2).is_empty(), "{f:?}");
    }
}

#[test]
fn multi_allowed_set_matches() {
    let line = line_with(FORMAT_MATRIX, "**multi-allowed set** = {");
    let inner = line.split_once('{').unwrap().1.split_once('}').unwrap().0;
    let doc: HashSet<ToolKey> = inner.split(',').map(|k| tool(k.trim())).collect();
    let code: HashSet<ToolKey> = ToolKey::ALL.into_iter().filter(|t| t.multi_ok()).collect();
    assert_eq!(doc, code);
}

#[test]
fn formats_observed_without_tools_have_none() {
    let line = line_with(FORMAT_MATRIX, "Formats observed with no tools");
    for key in backticked(line)[0].split_whitespace() {
        let f = format(key);
        assert!(tools(f, 1).is_empty() && tools(f, 2).is_empty(), "{key}");
    }
}

fn parse_ext(cell: &str) -> OutputExt {
    match cell {
        "same" | "first file's container" => OutputExt::Same,
        "same; **wav → flac**" => OutputExt::SameWavToFlac,
        "folder" => OutputExt::Folder,
        key => OutputExt::Fixed(format(key)),
    }
}

#[test]
fn naming_table_matches_tool_naming() {
    let output_naming = section(FORMAT_MATRIX, "## Output naming");
    let table = rows(section(output_naming, "### Tools"), "Tool");
    let mut covered = Vec::new();
    for row in &table {
        let keys: Vec<ToolKey> = row[0].split('/').map(|k| tool(k.trim())).collect();
        let suffixes: Vec<Suffix> = if row[1].contains('`') {
            backticked(&row[1])
                .into_iter()
                .map(|s| match s.trim() {
                    "Combined" => Suffix::Text("Combined"),
                    "Merged" => Suffix::Text("Merged"),
                    "Joined" => Suffix::Text("Joined"),
                    "Collage" => Suffix::Text("Collage"),
                    other => panic!("unexpected combine suffix {other:?}"),
                })
                .collect()
        } else {
            vec![tool_naming(keys[0]).suffix; keys.len()]
        };
        assert_eq!(suffixes.len(), keys.len(), "{:?}", row[0]);
        let exts: Vec<&str> = row[2].split(" / ").collect();
        for (i, &t) in keys.iter().enumerate() {
            let naming = tool_naming(t);
            assert_eq!(naming.suffix, suffixes[i], "{t:?} suffix");
            if !row[1].contains('`') {
                let rendered = match naming.suffix {
                    Suffix::Text(text) => text.to_string(),
                    Suffix::Speed => "Speed {N}x".to_string(),
                    Suffix::Channels => format!(
                        "{} (or {})",
                        Channels::Mono.label(),
                        Channels::Stereo.label()
                    ),
                };
                assert_eq!(rendered, row[1], "{t:?} suffix text");
            }
            let ext = if exts.len() == keys.len() {
                exts[i]
            } else {
                assert_eq!(exts.len(), 1, "{:?}", row[2]);
                exts[0]
            };
            assert_eq!(naming.ext, parse_ext(ext), "{t:?} ext");
        }
        covered.extend(keys);
    }
    let unique: HashSet<ToolKey> = covered.iter().copied().collect();
    assert_eq!(covered.len(), 26, "each tool has exactly one naming row");
    assert_eq!(unique.len(), 26);
    assert_eq!(table.len(), 20, "20 naming rows");
    // "Speed {N}x" and "Mono (or Stereo)" render with the params.
    let p = ToolParams {
        channels: Channels::Stereo,
        ..ToolParams::default()
    };
    assert_eq!(tool_suffix(ToolKey::AudioChannels, &p), "Stereo");
    assert_eq!(tool_suffix(ToolKey::ChangeVideoSpeed, &p), "Speed 2x");
}

// ---- TOOLS_AND_ACTIONS.md ----

fn check(cell: &str) -> bool {
    match cell {
        "✓" => true,
        "" => false,
        other => panic!("unexpected flag cell {other:?}"),
    }
}

/// `img`, `audio`, `video` or `pdf`, as used in the Tier column.
fn tier_group(name: &str) -> Vec<Format> {
    let category = match name {
        "img" => Category::Image,
        "audio" => Category::Audio,
        "video" => Category::Video,
        "pdf" => return vec![Format::Pdf],
        other => panic!("unknown tier group {other:?}"),
    };
    Format::ALL
        .into_iter()
        .filter(|f| f.category() == category)
        .collect()
}

#[test]
fn tools_table_matches_info() {
    let table = rows(section(TOOLS_AND_ACTIONS, "## Tools"), "Key");
    assert_eq!(table.len(), ToolKey::ALL.len());
    for (row, t) in table.iter().zip(ToolKey::ALL) {
        let info = t.info();
        assert_eq!(row[0], info.key);
        assert_eq!(row[1], info.title, "{t:?} title");
        assert_eq!(row[2], info.name, "{t:?} name");
        assert_eq!(check(&row[3]), info.panel, "{t:?} panel");
        assert_eq!(check(&row[4]), info.combine, "{t:?} combine");
        assert_eq!(check(&row[5]), info.multi_ok, "{t:?} multi-OK");
        let cell = &row[6];
        if cell.contains('(') {
            let mut first = true;
            for part in cell.split(", ") {
                let (tier_name, groups) = part.split_once(" (").unwrap();
                let t_tier = tier(tier_name);
                if first {
                    assert_eq!(info.tier, t_tier, "{t:?} base tier");
                    first = false;
                }
                for group in groups.trim_end_matches(')').split('/') {
                    for f in tier_group(group) {
                        if offered_anywhere(t, f) {
                            assert_eq!(tool_tier(t, f), t_tier, "{t:?} on {f:?}");
                        }
                    }
                }
            }
        } else {
            let t_tier = tier(cell);
            assert_eq!(info.tier, t_tier, "{t:?} tier");
            for f in Format::ALL {
                if offered_anywhere(t, f) {
                    assert_eq!(tool_tier(t, f), t_tier, "{t:?} on {f:?}");
                }
            }
        }
    }
}

#[test]
fn tool_and_action_counts_match() {
    assert_eq!(
        number_after(TOOLS_AND_ACTIONS, "# Tools ("),
        ToolKey::ALL.len()
    );
    assert_eq!(
        number_after(TOOLS_AND_ACTIONS, "and file actions ("),
        actions().len()
    );
    let line = line_with(TOOLS_AND_ACTIONS, " tools ✓.");
    assert_eq!(number_after(line, ""), ToolKey::ALL.len());
}

#[test]
fn actions_table_matches() {
    let table = rows(section(TOOLS_AND_ACTIONS, "## File actions"), "Key");
    assert_eq!(table.len(), 6);
    for (row, a) in table.iter().zip(actions()) {
        assert_eq!(row[0], a.key());
        let (mac, windows) = match row[1].split_once(" → ") {
            Some((mac, rest)) => (mac, rest.split("**").nth(1).unwrap()),
            None => (row[1].as_str(), row[1].as_str()),
        };
        assert_eq!(a.title(Platform::MacOs), mac, "{a:?} macOS title");
        assert_eq!(a.title(Platform::Windows), windows, "{a:?} Windows title");
        assert_eq!(row[4], "v1");
    }
}

// ---- INTERACTION_SPEC.md ----

#[test]
fn geometry_table_matches() {
    let table = rows(section(INTERACTION_SPEC, "## 4. Geometry"), "Constant");
    let json = serde_json::to_value(GEOMETRY).unwrap();
    let field = |name: &str| match name {
        "INNER_R" => "innerR",
        "OUTER_R" => "outerR",
        "BACKDROP_R" => "backdropR",
        "GAP" => "gap",
        "CORNER" => "corner",
        "LABEL_R" => "labelR",
        "DENSE_AT" => "denseAt",
        other => panic!("unknown constant {other:?}"),
    };
    assert_eq!(table.len(), 7);
    for row in table {
        let name = backticked(&row[0])[0];
        let value: f64 = row[1].trim_start_matches('≈').trim().parse().unwrap();
        assert_eq!(json[field(name)].as_f64(), Some(value), "{name}");
    }
    let line = line_with(INTERACTION_SPEC, "px margin stays on screen");
    let margin = line
        .split(" px margin")
        .next()
        .unwrap()
        .rsplit(' ')
        .next()
        .unwrap();
    assert_eq!(margin.parse::<f64>().unwrap(), SCREEN_MARGIN);
}
