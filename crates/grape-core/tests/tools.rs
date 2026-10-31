//! Tools and actions tests (`FORMAT_MATRIX.md` → "Tools per input",
//! `TOOLS_AND_ACTIONS.md` → "Tools", "File actions").
#![allow(clippy::unwrap_used)]

use std::collections::HashSet;

use grape_core::{
    actions, tool_row, tool_tier, tools, tools_enabled, ActionKey, Category, Features, Format,
    Platform, Tier, ToolKey, ToolParams, TOOL_ROWS,
};
use Format::*;
use ToolKey::*;

/// Asserts that every format in `formats` gets exactly `expected` for a
/// selection of `n` files.
fn assert_row(formats: &[Format], n: usize, expected: &[ToolKey]) {
    for &f in formats {
        assert_eq!(tools(f, n), expected, "{f:?} with {n} selected");
    }
}

const IMAGES: [Format; 5] = [Jpg, Png, Webp, Heic, Tiff];
const AUDIO: [Format; 4] = [Mp3, M4a, Wav, Flac];
const VIDEO: [Format; 3] = [Mp4, Mov, Mkv];
const ARCHIVES: [Format; 4] = [Zip, Tar, Gz, Rar];
const EVERYTHING_ELSE: [Format; 27] = [
    Ogg,
    Opus,
    Aiff,
    Wma,
    Webm,
    Avi,
    Wmv,
    Avif,
    Bmp,
    Ppt,
    Pptx,
    Xlsx,
    Xls,
    Ods,
    Csv,
    Tsv,
    Docx,
    Pages,
    Md,
    Rtf,
    Rtfd,
    TextClipping,
    Epub,
    Txt,
    Srt,
    Vtt,
    Html,
];

#[test]
fn tools_row_images_single() {
    assert_row(
        &IMAGES,
        1,
        &[
            Compress,
            RemoveMetadata,
            EditImage,
            AnnotateImage,
            FrameImage,
            CropImage,
            RedactImage,
        ],
    );
}

#[test]
fn tools_row_images_multi() {
    let expected = [
        Compress,
        RemoveMetadata,
        CropImage,
        CreatePdf,
        CreateCollage,
    ];
    assert_row(&IMAGES, 2, &expected);
    assert_row(&IMAGES, 300, &expected);
}

#[test]
fn tools_row_svg_single() {
    assert_row(&[Svg], 1, &[]);
}

#[test]
fn tools_row_svg_multi() {
    assert_row(&[Svg], 2, &[CreatePdf, CreateCollage]);
}

#[test]
fn tools_row_audio_single() {
    assert_row(
        &AUDIO,
        1,
        &[
            Compress,
            RemoveMetadata,
            NormalizeAudio,
            AudioToVideo,
            TrimAudio,
            AudioChannels,
            RedactAudio,
        ],
    );
}

#[test]
fn tools_row_audio_multi() {
    assert_row(&AUDIO, 2, &[Compress, RemoveMetadata, NormalizeAudio]);
}

#[test]
fn tools_row_video_single() {
    assert_row(
        &VIDEO,
        1,
        &[
            Compress,
            RemoveMetadata,
            MuteVideo,
            TrimVideo,
            CropVideo,
            ChangeVideoSpeed,
            VideoSnapshots,
            SplitVideo,
            RedactVideo,
        ],
    );
}

#[test]
fn tools_row_video_multi() {
    assert_row(
        &VIDEO,
        2,
        &[Compress, RemoveMetadata, MuteVideo, JoinVideos],
    );
}

#[test]
fn tools_row_gif_single() {
    assert_row(&[Gif], 1, &[RemoveMetadata]);
}

#[test]
fn tools_row_gif_multi() {
    assert_row(&[Gif], 2, &[RemoveMetadata]);
}

#[test]
fn tools_row_pdf_single() {
    assert_row(
        &[Pdf],
        1,
        &[Compress, RemoveMetadata, SplitPdf, OrganizePdf],
    );
}

#[test]
fn tools_row_pdf_multi() {
    assert_row(&[Pdf], 2, &[Compress, RemoveMetadata, SplitPdf, MergePdf]);
}

#[test]
fn tools_row_archives_single() {
    assert_row(&ARCHIVES, 1, &[ExtractArchive]);
}

#[test]
fn tools_row_archives_multi() {
    assert_row(&ARCHIVES, 2, &[ExtractArchive]);
}

#[test]
fn tools_row_everything_else_single() {
    assert_row(&EVERYTHING_ELSE, 1, &[]);
}

#[test]
fn tools_row_everything_else_multi() {
    assert_row(&EVERYTHING_ELSE, 2, &[]);
}

#[test]
fn rows_partition_all_formats() {
    let mut covered: Vec<Format> = TOOL_ROWS
        .iter()
        .flat_map(|r| r.formats.iter().copied())
        .collect();
    covered.extend(EVERYTHING_ELSE);
    let unique: HashSet<Format> = covered.iter().copied().collect();
    assert_eq!(covered.len(), 46, "no format is in two rows");
    assert_eq!(unique, Format::ALL.iter().copied().collect());
    for f in EVERYTHING_ELSE {
        assert!(tool_row(f).is_none(), "{f:?}");
    }
}

#[test]
fn zero_selected_is_empty() {
    for f in Format::ALL {
        assert!(tools(f, 0).is_empty(), "{f:?}");
    }
}

#[test]
fn multi_filter_removes_nothing() {
    for row in TOOL_ROWS {
        for &f in row.formats {
            assert_eq!(tools(f, 2), row.multi, "{f:?}");
        }
        for t in row.multi {
            assert!(t.multi_ok(), "{t:?} is in a multi list but not multi-OK");
        }
    }
}

#[test]
fn single_lists_have_no_combine_tools() {
    for row in TOOL_ROWS {
        assert!(row.single.iter().all(|t| !t.is_combine()));
    }
}

fn set(keys: &[ToolKey]) -> HashSet<ToolKey> {
    keys.iter().copied().collect()
}

fn where_(pred: fn(ToolKey) -> bool) -> HashSet<ToolKey> {
    ToolKey::ALL.iter().copied().filter(|&t| pred(t)).collect()
}

#[test]
fn twenty_six_tools() {
    assert_eq!(ToolKey::ALL.len(), 26);
    assert_eq!(set(&ToolKey::ALL).len(), 26);
}

#[test]
fn multi_allowed_set_matches_doc() {
    let expected = set(&[
        Compress,
        RemoveMetadata,
        CropImage,
        NormalizeAudio,
        MuteVideo,
        SplitPdf,
        ExtractArchive,
        CreatePdf,
        MergePdf,
        JoinVideos,
        CreateCollage,
    ]);
    assert_eq!(expected.len(), 11);
    assert_eq!(where_(ToolKey::multi_ok), expected);
}

#[test]
fn combine_set_has_four_tools() {
    assert_eq!(
        where_(ToolKey::is_combine),
        set(&[CreatePdf, MergePdf, JoinVideos, CreateCollage])
    );
}

#[test]
fn panel_set_has_22_tools() {
    let no_panel = set(&[MuteVideo, SplitPdf, CreatePdf, ExtractArchive]);
    let panel = where_(ToolKey::opens_panel);
    assert_eq!(panel.len(), 22);
    assert!(panel.is_disjoint(&no_panel));
}

#[test]
fn v1_tool_set_has_nine_tools() {
    let v1 = where_(|t| t.base_tier() == Tier::V1);
    assert_eq!(
        v1,
        set(&[
            Compress,
            RemoveMetadata,
            NormalizeAudio,
            AudioChannels,
            MuteVideo,
            SplitPdf,
            MergePdf,
            CreatePdf,
            ExtractArchive,
        ])
    );
}

#[test]
fn compress_is_v1_for_media_and_v2_for_pdf() {
    for f in IMAGES.iter().chain(&AUDIO).chain(&VIDEO) {
        assert_eq!(tool_tier(Compress, *f), Tier::V1, "{f:?}");
    }
    assert_eq!(tool_tier(Compress, Pdf), Tier::V2);
    assert_eq!(tool_tier(RemoveMetadata, Pdf), Tier::V1);
}

#[test]
fn every_tool_tier_matches_base_except_compress_pdf() {
    for t in ToolKey::ALL {
        for f in Format::ALL {
            let expected = if t == Compress && f == Pdf {
                Tier::V2
            } else {
                t.base_tier()
            };
            assert_eq!(tool_tier(t, f), expected, "{t:?} on {f:?}");
        }
    }
}

#[test]
fn v1_tools_per_row() {
    let v1 = Features::V1_ONLY;
    assert_eq!(tools_enabled(Jpg, 1, v1), vec![Compress, RemoveMetadata]);
    assert_eq!(
        tools_enabled(Jpg, 3, v1),
        vec![Compress, RemoveMetadata, CreatePdf]
    );
    assert!(tools_enabled(Svg, 1, v1).is_empty());
    assert_eq!(tools_enabled(Svg, 2, v1), vec![CreatePdf]);
    assert_eq!(
        tools_enabled(Mp3, 1, v1),
        vec![Compress, RemoveMetadata, NormalizeAudio, AudioChannels]
    );
    assert_eq!(
        tools_enabled(Wav, 2, v1),
        vec![Compress, RemoveMetadata, NormalizeAudio]
    );
    assert_eq!(
        tools_enabled(Mp4, 1, v1),
        vec![Compress, RemoveMetadata, MuteVideo]
    );
    assert_eq!(
        tools_enabled(Mov, 2, v1),
        vec![Compress, RemoveMetadata, MuteVideo]
    );
    assert_eq!(tools_enabled(Gif, 1, v1), vec![RemoveMetadata]);
    assert_eq!(tools_enabled(Pdf, 1, v1), vec![RemoveMetadata, SplitPdf]);
    assert_eq!(
        tools_enabled(Pdf, 2, v1),
        vec![RemoveMetadata, SplitPdf, MergePdf]
    );
    assert_eq!(tools_enabled(Rar, 1, v1), vec![ExtractArchive]);
}

#[test]
fn v1_panels_are_four_tools_that_open_one() {
    let panels: Vec<ToolKey> = ToolKey::ALL
        .into_iter()
        .filter(|t| t.has_v1_panel())
        .collect();
    assert_eq!(panels, [Compress, NormalizeAudio, AudioChannels, MergePdf]);
    for t in panels {
        assert!(t.opens_panel(), "{t:?}");
        assert_eq!(t.base_tier(), Tier::V1, "{t:?}");
    }
    // Marked Panel in the table, but it runs at once in v1.
    assert!(RemoveMetadata.opens_panel());
    assert!(!RemoveMetadata.has_v1_panel());
}

#[test]
fn normalize_targets_run_from_minus_24_to_minus_10_lufs() {
    let cases = [
        (i8::MIN, false),
        (-25, false),
        (-24, true),
        (-23, true),
        (-16, true),
        (-10, true),
        (-9, false),
        (0, false),
        (i8::MAX, false),
    ];
    for (lufs, ok) in cases {
        assert_eq!(ToolParams::lufs_ok(lufs), ok, "{lufs}");
    }
    assert!(ToolParams::lufs_ok(ToolParams::DEFAULT.target_lufs));
    let offered = (i8::MIN..=i8::MAX)
        .filter(|&lufs| ToolParams::lufs_ok(lufs))
        .count();
    assert_eq!(offered, 15);
}

#[test]
fn full_features_offer_every_listed_tool() {
    for row in TOOL_ROWS {
        for &f in row.formats {
            assert_eq!(tools_enabled(f, 1, Features::FULL), row.single);
            assert_eq!(tools_enabled(f, 2, Features::FULL), row.multi);
        }
    }
}

#[test]
fn titles_and_names_match_doc() {
    let expected: [(ToolKey, &str, &str, &str); 26] = [
        (Compress, "compress", "COMPRESS", "Compress"),
        (
            RemoveMetadata,
            "removeMetadata",
            "METADATA",
            "Remove Metadata",
        ),
        (EditImage, "editImage", "EDIT", "Edit Photo"),
        (AnnotateImage, "annotateImage", "ANNOTATE", "Annotate Photo"),
        (FrameImage, "frameImage", "ADD BG", "Add Background"),
        (CropImage, "cropImage", "CROP", "Crop Image"),
        (RedactImage, "redactImage", "REDACT", "Redact Photo"),
        (
            NormalizeAudio,
            "normalizeAudio",
            "NORMALIZE",
            "Normalize Volume",
        ),
        (TrimAudio, "trimAudio", "TRIM", "Trim Audio"),
        (AudioChannels, "audioChannels", "CHANNELS", "Audio Channels"),
        (
            AudioToVideo,
            "audioToVideo",
            "VISUALIZE",
            "Audio Visualizer",
        ),
        (RedactAudio, "redactAudio", "BLEEP", "Bleep Audio"),
        (MuteVideo, "muteVideo", "MUTE", "Remove Audio"),
        (TrimVideo, "trimVideo", "TRIM", "Trim Video"),
        (CropVideo, "cropVideo", "CROP", "Crop Video"),
        (
            ChangeVideoSpeed,
            "changeVideoSpeed",
            "SPEED",
            "Change Speed",
        ),
        (
            VideoSnapshots,
            "videoSnapshots",
            "SNAPSHOT",
            "Video Snapshots",
        ),
        (SplitVideo, "splitVideo", "SPLIT", "Split Video"),
        (JoinVideos, "joinVideos", "JOIN", "Join Videos"),
        (RedactVideo, "redactVideo", "REDACT", "Redact Video"),
        (SplitPdf, "splitPDF", "SPLIT", "Split PDF"),
        (OrganizePdf, "organizePDF", "ORGANIZE", "Organize PDF"),
        (MergePdf, "mergePDF", "MERGE PDF", "Merge PDFs"),
        (CreatePdf, "createPDF", "CREATE PDF", "Create PDF"),
        (CreateCollage, "createCollage", "COLLAGE", "Create Collage"),
        (
            ExtractArchive,
            "extractArchive",
            "EXTRACT",
            "Extract Archive",
        ),
    ];
    let order: Vec<ToolKey> = expected.iter().map(|e| e.0).collect();
    assert_eq!(order, ToolKey::ALL.to_vec(), "doc order");
    for (t, key, title, name) in expected {
        assert_eq!(t.key(), key);
        assert_eq!(t.title(), title);
        assert_eq!(t.name(), name);
        assert_eq!(ToolKey::from_key(key), Some(t));
    }
    assert_eq!(ToolKey::from_key("splitPdf"), None);
}

#[test]
fn tool_serde_uses_doc_keys() {
    for t in ToolKey::ALL {
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, format!("\"{}\"", t.key()));
        let back: ToolKey = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }
}

#[test]
fn tool_info_serializes_for_the_ui() {
    let json = serde_json::to_value(MergePdf.info()).unwrap();
    assert_eq!(json["key"], "mergePDF");
    assert_eq!(json["title"], "MERGE PDF");
    assert_eq!(json["multiOk"], true);
    assert_eq!(json["tier"], "v1");
}

#[test]
fn tools_only_apply_to_their_categories() {
    use Category::*;
    for row in TOOL_ROWS {
        for &f in row.formats {
            for t in row.single.iter().chain(row.multi) {
                let ok = match t {
                    Compress | RemoveMetadata => true,
                    EditImage | AnnotateImage | FrameImage | CropImage | RedactImage
                    | CreateCollage => f.category() == Image,
                    CreatePdf => f.category() == Image,
                    NormalizeAudio | TrimAudio | AudioChannels | AudioToVideo | RedactAudio => {
                        f.category() == Audio
                    }
                    MuteVideo | TrimVideo | CropVideo | ChangeVideoSpeed | VideoSnapshots
                    | SplitVideo | JoinVideos | RedactVideo => f.category() == Video,
                    SplitPdf | OrganizePdf | MergePdf => f == Pdf,
                    ExtractArchive => f.category() == Archive,
                };
                assert!(ok, "{t:?} on {f:?}");
            }
        }
    }
}

#[test]
fn actions_in_doc_order() {
    use ActionKey::*;
    assert_eq!(actions(), &[Share, Float, Copy, Move, CopyPath, Terminal]);
    assert_eq!(actions().len(), 6);
}

#[test]
fn action_titles_on_windows() {
    let titles: Vec<&str> = actions()
        .iter()
        .map(|a| a.title(Platform::Windows))
        .collect();
    assert_eq!(
        titles,
        ["SHARE", "FLOAT", "COPY", "MOVE", "COPY PATH", "TERMINAL"]
    );
}

#[test]
fn action_titles_on_macos() {
    let titles: Vec<&str> = actions().iter().map(|a| a.title(Platform::MacOs)).collect();
    assert_eq!(
        titles,
        ["AIRDROP", "FLOAT", "COPY", "MOVE", "COPY PATH", "TERMINAL"]
    );
}

#[test]
fn action_keys_and_serde() {
    let keys: Vec<&str> = actions().iter().map(|a| a.key()).collect();
    assert_eq!(
        keys,
        ["share", "float", "copy", "move", "copyPath", "terminal"]
    );
    for a in actions() {
        assert_eq!(
            serde_json::to_string(a).unwrap(),
            format!("\"{}\"", a.key())
        );
    }
    assert_eq!(
        serde_json::to_string(&Platform::MacOs).unwrap(),
        "\"macos\""
    );
    assert_eq!(
        serde_json::to_string(&Platform::Windows).unwrap(),
        "\"windows\""
    );
}
