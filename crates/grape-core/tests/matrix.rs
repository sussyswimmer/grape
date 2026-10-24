//! Conversion matrix tests (`FORMAT_MATRIX.md` → "Conversion targets",
//! "Rules", "Category of an input", "Test assertions").
#![allow(clippy::unwrap_used)]

use std::collections::HashSet;

use grape_core::{
    pair_tier, pairs, targets, targets_enabled, Category, Features, Format, Tier, INPUT_ONLY,
};
use Format::*;

fn count_tier(tier: Tier) -> usize {
    pairs(Features::FULL)
        .into_iter()
        .filter(|&(from, to)| pair_tier(from, to) == Some(tier))
        .count()
}

#[test]
fn full_matrix_has_275_pairs() {
    assert_eq!(pairs(Features::FULL).len(), 275);
}

#[test]
fn v1_matrix_has_178_pairs() {
    assert_eq!(pairs(Features::V1_ONLY).len(), 178);
    assert_eq!(pairs(Features::default()).len(), 178);
}

#[test]
fn v2_has_81_pairs() {
    assert_eq!(count_tier(Tier::V2), 81);
}

#[test]
fn later_has_16_pairs() {
    assert_eq!(count_tier(Tier::Later), 16);
}

#[test]
fn v1_tier_count_matches_v1_pairs() {
    assert_eq!(count_tier(Tier::V1), 178);
}

#[test]
fn v2_without_later_has_259_pairs() {
    let f = Features {
        v2: true,
        later: false,
    };
    assert_eq!(pairs(f).len(), 178 + 81);
}

#[test]
fn later_without_v2_has_194_pairs() {
    let f = Features {
        v2: false,
        later: true,
    };
    assert_eq!(pairs(f).len(), 178 + 16);
}

#[test]
fn format_count_is_46() {
    assert_eq!(Format::ALL.len(), 46);
    let unique: HashSet<Format> = Format::ALL.iter().copied().collect();
    assert_eq!(unique.len(), 46);
}

#[test]
fn forty_five_inputs() {
    let inputs = Format::ALL.iter().filter(|f| f.is_input()).count();
    assert_eq!(inputs, 45);
}

#[test]
fn html_is_the_only_target_only_format() {
    let target_only: Vec<Format> = Format::ALL
        .iter()
        .copied()
        .filter(|f| !f.is_input())
        .collect();
    assert_eq!(target_only, vec![Html]);
    assert!(targets(Html).is_empty());
    let all_targets: HashSet<Format> = pairs(Features::FULL).iter().map(|p| p.1).collect();
    assert!(all_targets.contains(&Html));
}

#[test]
fn html_is_last_in_all() {
    assert_eq!(Format::ALL.last(), Some(&Html));
}

#[test]
fn no_self_pairs() {
    for (from, to) in pairs(Features::FULL) {
        assert_ne!(from, to, "{from:?} converts to itself");
    }
}

#[test]
fn no_duplicate_targets_in_a_row() {
    for f in Format::ALL {
        let row = targets(f);
        let unique: HashSet<&Format> = row.iter().collect();
        assert_eq!(unique.len(), row.len(), "duplicate target in {f:?}");
    }
}

#[test]
fn input_only_never_target() {
    assert_eq!(INPUT_ONLY, [Rar, Ppt, Xls, Rtf, Rtfd, TextClipping]);
    for (from, to) in pairs(Features::FULL) {
        assert!(
            !to.is_input_only(),
            "{from:?} -> {to:?} targets an input-only format"
        );
    }
    for f in INPUT_ONLY {
        assert!(f.is_input(), "{f:?} must still be an input");
    }
}

#[test]
fn svg_is_never_a_target() {
    assert!(pairs(Features::FULL).iter().all(|&(_, to)| to != Svg));
    // svg is "input-only within images" but not in rule 2's list.
    assert!(!Svg.is_input_only());
}

#[test]
fn never_targets_are_the_input_only_list_plus_svg() {
    let all_targets: HashSet<Format> = pairs(Features::FULL).iter().map(|p| p.1).collect();
    let never: HashSet<Format> = Format::ALL
        .iter()
        .copied()
        .filter(|f| !all_targets.contains(f))
        .collect();
    let mut expected: HashSet<Format> = INPUT_ONLY.iter().copied().collect();
    expected.insert(Svg);
    assert_eq!(never, expected);
}

/// The `#` column of every row in the doc.
const ROW_COUNTS: [(Format, usize); 45] = [
    (Jpg, 7),
    (Png, 7),
    (Webp, 7),
    (Heic, 7),
    (Tiff, 7),
    (Svg, 8),
    (Avif, 7),
    (Bmp, 7),
    (Mp3, 7),
    (M4a, 7),
    (Wav, 7),
    (Flac, 7),
    (Ogg, 7),
    (Opus, 7),
    (Aiff, 7),
    (Wma, 7),
    (Mp4, 7),
    (Mov, 7),
    (Mkv, 7),
    (Webm, 7),
    (Avi, 7),
    (Wmv, 7),
    (Gif, 6),
    (Pdf, 9),
    (Ppt, 3),
    (Pptx, 3),
    (Xlsx, 4),
    (Xls, 5),
    (Ods, 4),
    (Csv, 4),
    (Tsv, 4),
    (Docx, 8),
    (Pages, 8),
    (Md, 8),
    (Rtf, 9),
    (Rtfd, 9),
    (TextClipping, 9),
    (Epub, 6),
    (Txt, 8),
    (Srt, 2),
    (Vtt, 2),
    (Zip, 2),
    (Tar, 2),
    (Gz, 2),
    (Rar, 3),
];

#[test]
fn per_row_counts_match_doc() {
    for (f, n) in ROW_COUNTS {
        assert_eq!(targets(f).len(), n, "row {f:?}");
    }
    let rows: Vec<Format> = ROW_COUNTS.iter().map(|r| r.0).collect();
    assert_eq!(rows, Format::ALL[..45].to_vec(), "rows are in doc order");
}

/// The doc's nine tables, the formats in each, and the subtotal of `#`.
const TABLES: [(&str, &[Format], usize); 9] = [
    ("Images", &[Jpg, Png, Webp, Heic, Tiff, Svg, Avif, Bmp], 57),
    ("Audio", &[Mp3, M4a, Wav, Flac, Ogg, Opus, Aiff, Wma], 56),
    ("Video", &[Mp4, Mov, Mkv, Webm, Avi, Wmv, Gif], 48),
    ("PDF", &[Pdf], 9),
    ("Slides", &[Ppt, Pptx], 6),
    ("Spreadsheets", &[Xlsx, Xls, Ods, Csv, Tsv], 21),
    (
        "Documents",
        &[Docx, Pages, Md, Rtf, Rtfd, TextClipping, Epub, Txt],
        65,
    ),
    ("Subtitles", &[Srt, Vtt], 4),
    ("Archives", &[Zip, Tar, Gz, Rar], 9),
];

#[test]
fn per_table_subtotals() {
    let mut total = 0;
    for (name, formats, subtotal) in TABLES {
        let sum: usize = formats.iter().map(|&f| targets(f).len()).sum();
        assert_eq!(sum, subtotal, "table {name}");
        total += sum;
    }
    assert_eq!(total, 275);
}

#[test]
fn pdf_row_and_tiers() {
    use Tier::*;
    let expected = [
        (Docx, V2),
        (Pages, Later),
        (Pptx, V2),
        (Xlsx, V2),
        (Jpg, V1),
        (Png, V1),
        (Epub, V2),
        (Txt, V1),
        (Md, V1),
    ];
    let row: Vec<Format> = expected.iter().map(|e| e.0).collect();
    assert_eq!(targets(Pdf), row.as_slice());
    for (to, tier) in expected {
        assert_eq!(pair_tier(Pdf, to), Some(tier), "pdf -> {to:?}");
    }
}

#[test]
fn pdf_v1_targets() {
    assert_eq!(
        targets_enabled(Pdf, Features::V1_ONLY),
        vec![Jpg, Png, Txt, Md]
    );
}

#[test]
fn documents_have_no_v1_targets() {
    for f in [
        Docx,
        Pages,
        Md,
        Rtf,
        Rtfd,
        TextClipping,
        Epub,
        Txt,
        Ppt,
        Pptx,
    ] {
        assert!(targets_enabled(f, Features::V1_ONLY).is_empty(), "{f:?}");
    }
}

#[test]
fn pages_row_is_all_later() {
    for &to in targets(Pages) {
        assert_eq!(pair_tier(Pages, to), Some(Tier::Later));
    }
    let only_v2 = Features {
        v2: true,
        later: false,
    };
    assert!(targets_enabled(Pages, only_v2).is_empty());
    assert_eq!(targets_enabled(Pages, Features::FULL).len(), 8);
}

#[test]
fn every_pages_target_is_later() {
    for (from, to) in pairs(Features::FULL) {
        if to == Pages || from == Pages {
            assert_eq!(pair_tier(from, to), Some(Tier::Later), "{from:?} -> {to:?}");
        }
    }
}

#[test]
fn pair_tier_is_none_for_missing_pairs() {
    assert_eq!(pair_tier(Jpg, Jpg), None);
    assert_eq!(pair_tier(Gif, Png), None);
    assert_eq!(pair_tier(Html, Pdf), None);
    assert_eq!(pair_tier(Jpg, Png), Some(Tier::V1));
}

#[test]
fn gif_gets_no_image_targets() {
    for &to in targets(Gif) {
        assert_eq!(to.category(), Category::Video);
    }
}

#[test]
fn video_to_mp3_is_offered() {
    for f in [Mp4, Mov, Mkv, Webm, Avi, Wmv] {
        assert_eq!(targets(f).last(), Some(&Mp3), "{f:?}");
    }
}

#[test]
fn aliases_resolve() {
    assert_eq!(Format::from_extension("jpeg"), Some(Jpg));
    assert_eq!(Format::from_extension("tif"), Some(Tiff));
    assert_eq!(Format::from_extension("aif"), Some(Aiff));
    assert_eq!(Format::from_extension("htm"), Some(Html));
    assert_eq!(Format::from_extension("markdown"), Some(Md));
    assert_eq!(Format::from_extension("tgz"), Some(Gz));
    assert_eq!(Format::from_extension("tar.gz"), Some(Gz));
}

#[test]
fn extension_lookup_ignores_case_and_leading_dot() {
    assert_eq!(Format::from_extension("JPG"), Some(Jpg));
    assert_eq!(Format::from_extension(".Png"), Some(Png));
    assert_eq!(Format::from_extension("JPEG"), Some(Jpg));
    assert_eq!(Format::from_extension("textClipping"), Some(TextClipping));
    assert_eq!(Format::from_extension("TEXTCLIPPING"), Some(TextClipping));
    assert_eq!(Format::from_extension("TAR.GZ"), Some(Gz));
}

#[test]
fn unknown_extension_is_none() {
    assert_eq!(Format::from_extension("exe"), None);
    assert_eq!(Format::from_extension(""), None);
    assert_eq!(Format::from_extension("."), None);
    assert_eq!(Format::from_extension("jpg "), None);
}

#[test]
fn every_key_round_trips_through_from_extension() {
    for f in Format::ALL {
        assert_eq!(Format::from_extension(f.key()), Some(f), "{f:?}");
    }
}

#[test]
fn file_names_resolve() {
    assert_eq!(Format::from_file_name("photo.JPG"), Some(Jpg));
    assert_eq!(Format::from_file_name("backup.tar.gz"), Some(Gz));
    assert_eq!(Format::from_file_name("backup.TAR.GZ"), Some(Gz));
    assert_eq!(Format::from_file_name("backup.tgz"), Some(Gz));
    assert_eq!(Format::from_file_name("backup.gz"), Some(Gz));
    assert_eq!(Format::from_file_name("backup.tar"), Some(Tar));
    assert_eq!(Format::from_file_name("ảnh chụp.jpg"), Some(Jpg));
    assert_eq!(Format::from_file_name("my.report.v2.pdf"), Some(Pdf));
    assert_eq!(Format::from_file_name("Makefile"), None);
    assert_eq!(Format::from_file_name(".png"), None);
    assert_eq!(Format::from_file_name("notes."), None);
    assert_eq!(Format::from_file_name("index.htm"), Some(Html));
}

#[test]
fn display_names_are_uppercase_keys_except_gzip() {
    for f in Format::ALL {
        if f == Gz {
            assert_eq!(f.display_name(), "GZIP");
        } else {
            assert_eq!(f.display_name(), f.key().to_uppercase(), "{f:?}");
        }
    }
}

#[test]
fn output_ext_is_key_except_tar_gz() {
    for f in Format::ALL {
        if f == Gz {
            assert_eq!(f.output_ext(), "tar.gz");
        } else {
            assert_eq!(f.output_ext(), f.key(), "{f:?}");
        }
    }
}

#[test]
fn category_lists_match_doc() {
    use Category::*;
    let expected: [(Category, &[Format]); 7] = [
        (Audio, &[Mp3, M4a, Wav, Flac, Ogg, Opus, Aiff, Wma]),
        (Video, &[Mp4, Mov, Mkv, Webm, Avi, Wmv, Gif]),
        (
            Document,
            &[
                Pdf,
                Ppt,
                Pptx,
                Docx,
                Pages,
                Md,
                Rtf,
                Rtfd,
                TextClipping,
                Txt,
                Html,
                Epub,
            ],
        ),
        (Spreadsheet, &[Xlsx, Xls, Ods, Csv, Tsv]),
        (Archive, &[Zip, Tar, Gz, Rar]),
        (Subtitle, &[Srt, Vtt]),
        (Image, &[Jpg, Png, Webp, Heic, Tiff, Svg, Avif, Bmp]),
    ];
    let mut seen = 0;
    for (cat, formats) in expected {
        let actual: HashSet<Format> = Format::ALL
            .iter()
            .copied()
            .filter(|f| f.category() == cat)
            .collect();
        let want: HashSet<Format> = formats.iter().copied().collect();
        assert_eq!(actual, want, "{cat:?}");
        seen += formats.len();
    }
    assert_eq!(seen, 46);
}

#[test]
fn serde_uses_doc_keys() {
    for f in Format::ALL {
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, format!("\"{}\"", f.key()));
        let back: Format = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }
    assert_eq!(serde_json::to_string(&Tier::Later).unwrap(), "\"later\"");
    assert_eq!(serde_json::to_string(&Tier::V2).unwrap(), "\"v2\"");
    assert_eq!(
        serde_json::to_string(&Category::Spreadsheet).unwrap(),
        "\"spreadsheet\""
    );
    assert_eq!(
        serde_json::to_string(&Features::FULL).unwrap(),
        r#"{"v2":true,"later":true}"#
    );
}

#[test]
fn tiers_gate_independently() {
    assert!(Tier::V1.is_enabled(Features::V1_ONLY));
    assert!(!Tier::V2.is_enabled(Features::V1_ONLY));
    assert!(!Tier::Later.is_enabled(Features::V1_ONLY));
    assert!(Tier::V2.is_enabled(Features::FULL));
    assert!(Tier::Later.is_enabled(Features::FULL));
    assert_eq!(Features::default(), Features::V1_ONLY);
}

#[test]
fn pairs_are_in_doc_order() {
    let p = pairs(Features::FULL);
    assert_eq!(p.first(), Some(&(Jpg, Png)));
    assert_eq!(p.last(), Some(&(Rar, Gz)));
    let v1 = pairs(Features::V1_ONLY);
    let pdf_pairs: Vec<Format> = v1
        .iter()
        .filter(|(f, _)| *f == Pdf)
        .map(|(_, t)| *t)
        .collect();
    assert_eq!(pdf_pairs, vec![Jpg, Png, Txt, Md]);
}
