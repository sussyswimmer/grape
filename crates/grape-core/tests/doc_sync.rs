//! Doc-sync tests: parse the spec tables straight from `docs/` and compare
//! them with the code, so any drift between the two fails the build.
//!
//! The parser only reads `|` table rows and a few rule lines it looks for
//! by their exact wording; other prose and `[Grape]` bullets around them
//! are ignored. Rules that are formulas or prose (the §4 angle, hit-test and
//! hit-shape rules, the naming bullets) are pinned by phrase, and their
//! examples are run through the code.
#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::f64::consts::{FRAC_PI_2, PI, TAU};
use std::path::Path;

use grape_core::geometry::{
    fillet_radius, start_angle, CORNER, GAP, GEOMETRY, INNER_R, OUTER_R, SCREEN_MARGIN,
};
use grape_core::{
    actions, hit_test, output_name, page_file_name, pair_tier, pairs, plan, split_name, targets,
    tool_naming, tool_suffix, tool_tier, tools, wheel_content, wheel_layout, Category, Channels,
    DirSnapshot, EmptyState, Features, Format, JobOutput, Operation, OutputExt, Platform,
    Selection, SelectionItem, Settings, Speed, Suffix, Tier, ToolKey, ToolParams, WheelContent,
    WheelMode, INPUT_ONLY, TOOL_ROWS,
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

/// Asserts that `line` still has each pinned phrase. Used for rules that
/// are prose or formulas rather than tables: the test next to the pin
/// checks the code against the same rule.
fn assert_pinned(line: &str, pinned: &[&str]) {
    for phrase in pinned {
        assert!(line.contains(phrase), "{phrase:?} not in {line:?}");
    }
}

fn names(list: &[&str]) -> HashSet<String> {
    list.iter().map(|s| s.to_string()).collect()
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

#[test]
fn collision_rule_matches() {
    let line = line_with(FORMAT_MATRIX, "If that name exists (on disk");
    assert_pinned(line, &["or already planned in this batch"]);
    let templates = backticked(line);
    assert_eq!(templates.len(), 2, "{line}");
    let png = Operation::convert(Format::Png);
    let mut taken = names(&["a.png"]);
    for template in templates {
        let expected = template.replace("{stem}", "a").replace("{ext}", "png");
        assert_eq!(output_name("a.jpg", &png, &taken), expected);
        taken.insert(expected);
    }
    // Tools use "the same ` 2`, ` 3` collision rule".
    let line = line_with(FORMAT_MATRIX, "Output = `{stem} {Suffix}.{ext}`");
    assert_eq!(backticked(line), ["{stem} {Suffix}.{ext}", " 2", " 3"]);
    let compress = Operation::tool(ToolKey::Compress);
    let mut taken = names(&["a Compressed.jpg"]);
    for counter in backticked(line).into_iter().skip(1) {
        let expected = format!("a Compressed{counter}.jpg");
        assert_eq!(output_name("a.jpg", &compress, &taken), expected);
        taken.insert(expected);
    }
}

#[test]
fn case_rule_matches() {
    let line = line_with(FORMAT_MATRIX, "The collision check ignores case");
    let example = backticked(line);
    assert_eq!(example.len(), 2, "{line}");
    let (existing, wanted) = (example[0], example[1]);
    let (stem, ext) = split_name(wanted);
    let to = Format::from_extension(ext.unwrap()).unwrap();
    let source = format!("{stem}.source");
    let name = output_name(&source, &Operation::convert(to), &names(&[existing]));
    assert_ne!(name, wanted, "{existing:?} must block {wanted:?}");
    assert_eq!(name, format!("{stem} 2.{}", ext.unwrap()));
}

#[test]
fn no_extension_rule_matches() {
    let line = line_with(FORMAT_MATRIX, "A name with no dot");
    let example = backticked(line);
    assert_eq!(example.len(), 5, "{line}");
    for name in &example[..3] {
        assert_eq!(split_name(name), (*name, None), "{name}");
    }
    let (source, output) = (example[3], example[4]);
    let to = Format::from_extension(split_name(output).1.unwrap()).unwrap();
    assert_eq!(
        output_name(source, &Operation::convert(to), &HashSet::new()),
        output
    );
}

#[test]
fn pages_rule_matches() {
    let line = line_with(FORMAT_MATRIX, "zero-padded to the width of the page count");
    assert_pinned(line, &["`{stem} {n}.{ext}`", "counting from 1"]);
    let example = backticked(line);
    let (first, last) = (example[example.len() - 3], example[example.len() - 2]);
    assert_eq!(page_file_name("report", 1, 12, "png"), first);
    assert_eq!(page_file_name("report", 12, 12, "png"), last);
}

#[test]
fn same_extension_rule_matches() {
    let line = line_with(
        FORMAT_MATRIX,
        "\"same\" means the source's literal extension",
    );
    let example = backticked(line);
    assert_eq!(example.len(), 4, "{line}");
    let compress = Operation::tool(ToolKey::Compress);
    assert_eq!(
        output_name(example[0], &compress, &HashSet::new()),
        example[1]
    );
    // wav → flac goes by the detected format, which only the planner has.
    let dir = Path::new("/music");
    let item = SelectionItem::file_with_format(dir.join(example[2]), Some(Format::Wav));
    let sel = Selection::new(vec![item]).with_dir(dir, DirSnapshot::new([example[2]], true));
    let job = plan(&sel, &compress, &Settings::new(dir)).unwrap();
    assert_eq!(job.jobs[0].output, JobOutput::File(dir.join(example[3])));
}

#[test]
fn speed_rule_matches() {
    let line = line_with(FORMAT_MATRIX, "in `Speed {N}x` is the factor");
    assert_pinned(line, &["at most two decimals and no trailing zeros"]);
    let examples: Vec<&str> = backticked(line)
        .into_iter()
        .filter(|s| s.starts_with("Speed ") && !s.contains('{'))
        .collect();
    assert_eq!(examples.len(), 3, "{line}");
    for example in examples {
        let factor = example
            .strip_prefix("Speed ")
            .and_then(|s| s.strip_suffix('x'))
            .unwrap();
        let hundredths = (factor.parse::<f64>().unwrap() * 100.0).round() as u16;
        let params = ToolParams {
            speed: Speed::from_hundredths(hundredths).unwrap(),
            ..ToolParams::default()
        };
        assert_eq!(
            tool_suffix(ToolKey::ChangeVideoSpeed, &params),
            example,
            "{factor}"
        );
    }
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

#[test]
fn folders_stay_actions_only() {
    let line = line_with(INTERACTION_SPEC, "**Folders** only get the Actions wheel");
    // If the spec ever defines folder archives, this pin fails and the
    // folder rows have to be added to the code.
    assert!(
        line.contains("folders stay Actions-only even with v2 enabled"),
        "{line}"
    );
    let folder = [SelectionItem::folder("/data/album")];
    for features in [Features::V1_ONLY, Features::FULL] {
        assert_eq!(
            wheel_content(&folder, WheelMode::Conversions, features),
            WheelContent::Empty(EmptyState::NoConversions)
        );
        assert_eq!(
            wheel_content(&folder, WheelMode::Tools, features),
            WheelContent::Empty(EmptyState::NoTools)
        );
        assert_eq!(
            wheel_content(&folder, WheelMode::Actions, features)
                .entries()
                .len(),
            6
        );
    }
}

#[test]
fn wheel_angle_rule_matches() {
    let line = line_with(INTERACTION_SPEC, "The center angle of segment `i` is");
    assert_pinned(
        line,
        &[
            "`start(n) − 2π·i/n`",
            "counter-clockwise from +x, so segments advance clockwise",
            "`start(n) = π/2` (item 0 at the **top**)",
            "except `start(3) = π` (item 0 on the **left**)",
        ],
    );
    for n in 1..=12 {
        let start = if n == 3 { PI } else { FRAC_PI_2 };
        assert_eq!(start_angle(n), start, "n={n}");
        for segment in wheel_layout(n) {
            let center = start - TAU * segment.index as f64 / n as f64;
            assert!((segment.center - center).abs() < 1e-12, "n={n}");
        }
    }
}

#[test]
fn hit_test_rule_matches() {
    let line = line_with(INTERACTION_SPEC, "**Hit test**");
    assert_pinned(
        line,
        &[
            "If `d < INNER_R` or `d > OUTER_R`, nothing is selected",
            "pick the nearest segment by angle",
            "exceeds `π/n − asin(GAP/2 / d)`, it's in a gap",
        ],
    );
    // `d == INNER_R` and `d == OUTER_R` are on the ring (exact straight up,
    // where the distance has no rounding error).
    assert_eq!(hit_test(0.0, -INNER_R, 4), Some(0));
    assert_eq!(hit_test(0.0, -OUTER_R, 4), Some(0));
    // Screen offset (y down) for a math angle and a distance.
    let at = |angle: f64, d: f64| (d * angle.cos(), -d * angle.sin());
    let eps = 1e-6;
    for n in 1..=12 {
        for segment in wheel_layout(n) {
            let i = Some(segment.index);
            let hit = |angle: f64, d: f64| {
                let (dx, dy) = at(angle, d);
                hit_test(dx, dy, n)
            };
            // The ring: `d < INNER_R` and `d > OUTER_R` select nothing.
            assert_eq!(hit(segment.center, INNER_R - eps), None, "n={n}");
            assert_eq!(hit(segment.center, INNER_R + eps), i, "n={n}");
            assert_eq!(hit(segment.center, OUTER_R - eps), i, "n={n}");
            assert_eq!(hit(segment.center, OUTER_R + eps), None, "n={n}");
            // The gap: past `π/n − asin(GAP/2 / d)` from the center. One
            // item has no gap (the [Grape] shape rule, checked below).
            if n == 1 {
                continue;
            }
            // Just inside the gap edge at `OUTER_R − 1` is a rounded-off
            // corner, so it selects nothing either (checked below).
            for (d, in_corner) in [
                (INNER_R + 1.0, false),
                (GEOMETRY.label_r, false),
                (OUTER_R - 1.0, true),
            ] {
                let half = PI / n as f64 - (GAP / 2.0 / d).asin();
                for side in [1.0, -1.0] {
                    let inside = segment.center + side * (half - eps);
                    let outside = segment.center + side * (half + eps);
                    assert_eq!(hit(outside, d), None, "n={n} d={d}");
                    if !in_corner {
                        assert_eq!(hit(inside, d), i, "n={n} d={d}");
                    }
                }
            }
        }
    }
}

#[test]
fn hit_test_shape_rule_matches() {
    let line = line_with(INTERACTION_SPEC, "honors the rounded outer corners itself");
    assert_pinned(
        line,
        &[
            "**[Grape]** Hit testing runs in `grape-core`",
            "the overlay receives no pointer events during an OS drag",
            "rounded by a circle tangent to the gap edge and to the outer circle",
            "with radius `min(CORNER, the largest that fits the segment)`",
            "in the cut-off corner outside that circle selects nothing",
            "The inner corners stay square",
            "With one item the wheel is a full ring with no gap",
            "any pointer with `INNER_R ≤ d ≤ OUTER_R` selects it",
            "Segments too narrow to draw (their inner corners would cross, from 76 items with these constants) select nothing",
        ],
    );
    let at = |angle: f64, d: f64| (d * angle.cos(), -d * angle.sin());
    let eps = 1e-6;

    // "from 76 items": segment 0's center ray still hits at 75 and not at 76.
    let (dx, dy) = at(start_angle(75), GEOMETRY.label_r);
    assert_eq!(hit_test(dx, dy, 75), Some(0));
    let (dx, dy) = at(start_angle(76), GEOMETRY.label_r);
    assert_eq!(hit_test(dx, dy, 76), None);

    // One item: a full ring, also straight below the anchor where a gap
    // would be.
    assert_eq!(hit_test(0.0, INNER_R, 1), Some(0));
    assert_eq!(hit_test(0.0, GEOMETRY.label_r, 1), Some(0));
    assert_eq!(hit_test(0.0, OUTER_R, 1), Some(0));
    for k in 0..720 {
        let angle = TAU * f64::from(k) / 720.0;
        for d in [INNER_R + eps, GEOMETRY.label_r, OUTER_R - eps] {
            let (dx, dy) = at(angle, d);
            assert_eq!(hit_test(dx, dy, 1), Some(0), "angle={angle} d={d}");
        }
    }

    for n in 2..=12 {
        let h = PI / n as f64;
        // A circle tangent to the gap edge (GAP/2 + ρ from the boundary ray)
        // and, from inside, to the outer circle (OUTER_R − ρ from the anchor).
        let center_angle = |rho: f64| ((GAP / 2.0 + rho) / (OUTER_R - rho)).asin();
        // `CORNER` fits a twelfth of the ring (its tangent point stays short
        // of the segment's center line), so `min(CORNER, …)` is `CORNER`.
        assert!(center_angle(CORNER) < h, "n={n}");
        let rho = fillet_radius(n);
        assert_eq!(rho, CORNER, "n={n}");
        let phi = center_angle(rho);
        for segment in wheel_layout(n) {
            let i = Some(segment.index);
            for side in [1.0, -1.0] {
                // The boundary ray, and the fillet center just inside it.
                let beta = segment.center + side * h;
                let (cx, cy) = at(beta - side * phi, OUTER_R - rho);
                assert_eq!(hit_test(cx, cy, n), i, "n={n}: fillet center");
                // Just inside the gap edge at OUTER_R − 1: the [observed] gap
                // rule alone would select it, but it's outside the fillet.
                let d = OUTER_R - 1.0;
                let gap_edge = segment.center + side * (h - (GAP / 2.0 / d).asin() - eps);
                let (x, y) = at(gap_edge, d);
                assert!((x - cx).hypot(y - cy) > rho, "n={n}: in the cut-off corner");
                assert_eq!(hit_test(x, y, n), None, "n={n}: cut-off corner");
                // Halfway between the fillet center and that point: inside
                // the fillet, so selected.
                let (mx, my) = ((x + cx) / 2.0, (y + cy) / 2.0);
                assert!((mx - cx).hypot(my - cy) < rho, "n={n}");
                assert_eq!(hit_test(mx, my, n), i, "n={n}: inside the fillet");
            }
        }
    }
}
