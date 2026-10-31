//! Output naming tests (`FORMAT_MATRIX.md` → "Output naming"): one test per
//! row of the tools naming table, plus conversions, collisions, `.tar.gz`
//! and awkward names.
#![allow(clippy::unwrap_used)]

use std::collections::HashSet;

use grape_core::{
    output_kind, output_name, page_file_name, pages_folder_name, produces_pages, split_name,
    tool_suffix, Channels, Format, Operation, OutputKind, Speed, ToolKey, ToolParams,
};
use Format::*;
use ToolKey::*;

fn none() -> HashSet<String> {
    HashSet::new()
}

fn taken(names: &[&str]) -> HashSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn tool(source: &str, t: ToolKey) -> String {
    output_name(source, &Operation::tool(t), &none())
}

fn tool_with(source: &str, t: ToolKey, params: ToolParams) -> String {
    output_name(source, &Operation::Tool { tool: t, params }, &none())
}

fn convert(source: &str, to: Format) -> String {
    output_name(source, &Operation::convert(to), &none())
}

// ---- Tools naming table: one test per row ----

#[test]
fn row_compress_same_ext_and_wav_to_flac() {
    assert_eq!(tool("photo.jpg", Compress), "photo Compressed.jpg");
    assert_eq!(tool("song.mp3", Compress), "song Compressed.mp3");
    assert_eq!(tool("clip.mp4", Compress), "clip Compressed.mp4");
    assert_eq!(tool("doc.pdf", Compress), "doc Compressed.pdf");
    assert_eq!(tool("take.wav", Compress), "take Compressed.flac");
    assert_eq!(tool("TAKE.WAV", Compress), "TAKE Compressed.flac");
}

#[test]
fn row_remove_metadata() {
    assert_eq!(tool("photo.heic", RemoveMetadata), "photo No Metadata.heic");
    assert_eq!(tool("anim.gif", RemoveMetadata), "anim No Metadata.gif");
    assert_eq!(tool("take.wav", RemoveMetadata), "take No Metadata.wav");
}

#[test]
fn row_edit_image() {
    assert_eq!(tool("photo.png", EditImage), "photo Edited.png");
}

#[test]
fn row_annotate_image_writes_png() {
    assert_eq!(tool("photo.jpg", AnnotateImage), "photo Annotated.png");
}

#[test]
fn row_frame_image() {
    assert_eq!(tool("photo.webp", FrameImage), "photo Background.webp");
}

#[test]
fn row_crop_image_and_video() {
    assert_eq!(tool("photo.tiff", CropImage), "photo Cropped.tiff");
    assert_eq!(tool("clip.mov", CropVideo), "clip Cropped.mov");
}

#[test]
fn row_redact_image_and_video() {
    assert_eq!(tool("photo.jpg", RedactImage), "photo Redacted.jpg");
    assert_eq!(tool("clip.mkv", RedactVideo), "clip Redacted.mkv");
}

#[test]
fn row_redact_audio_bleeped() {
    assert_eq!(tool("call.m4a", RedactAudio), "call Bleeped.m4a");
}

#[test]
fn row_normalize_audio() {
    assert_eq!(tool("song.flac", NormalizeAudio), "song Normalized.flac");
}

#[test]
fn row_mute_video() {
    assert_eq!(tool("clip.mp4", MuteVideo), "clip Muted.mp4");
}

#[test]
fn row_trim_video_and_audio() {
    assert_eq!(tool("clip.mov", TrimVideo), "clip Trimmed.mov");
    assert_eq!(tool("song.mp3", TrimAudio), "song Trimmed.mp3");
}

#[test]
fn row_change_video_speed() {
    let at = |h| ToolParams {
        speed: Speed::from_hundredths(h).unwrap(),
        ..ToolParams::default()
    };
    assert_eq!(
        tool_with("clip.mp4", ChangeVideoSpeed, at(150)),
        "clip Speed 1.5x.mp4"
    );
    assert_eq!(
        tool_with("clip.mp4", ChangeVideoSpeed, at(200)),
        "clip Speed 2x.mp4"
    );
    assert_eq!(
        tool_with("clip.mp4", ChangeVideoSpeed, at(25)),
        "clip Speed 0.25x.mp4"
    );
    assert_eq!(
        tool_with("clip.mov", ChangeVideoSpeed, at(1000)),
        "clip Speed 10x.mov"
    );
    assert_eq!(tool("clip.mkv", ChangeVideoSpeed), "clip Speed 2x.mkv");
}

#[test]
fn row_video_snapshots_writes_png() {
    assert_eq!(tool("clip.mp4", VideoSnapshots), "clip Snapshot.png");
}

#[test]
fn row_split_video_is_a_folder() {
    assert_eq!(tool("clip.mp4", SplitVideo), "clip Clips");
    assert_eq!(
        output_kind(&Operation::tool(SplitVideo)),
        OutputKind::Folder
    );
}

#[test]
fn row_split_pdf_is_a_folder() {
    assert_eq!(tool("doc.pdf", SplitPdf), "doc Pages");
    assert_eq!(output_kind(&Operation::tool(SplitPdf)), OutputKind::Folder);
}

#[test]
fn row_organize_pdf() {
    assert_eq!(tool("doc.pdf", OrganizePdf), "doc Organized.pdf");
    assert_eq!(tool("doc.PDF", OrganizePdf), "doc Organized.pdf");
}

#[test]
fn row_extract_archive_is_a_folder() {
    assert_eq!(tool("bundle.zip", ExtractArchive), "bundle Extracted");
    assert_eq!(tool("bundle.tar.gz", ExtractArchive), "bundle Extracted");
    assert_eq!(tool("bundle.tgz", ExtractArchive), "bundle Extracted");
    assert_eq!(tool("bundle.rar", ExtractArchive), "bundle Extracted");
    assert_eq!(
        output_kind(&Operation::tool(ExtractArchive)),
        OutputKind::Folder
    );
}

#[test]
fn row_audio_to_video_writes_mp4() {
    assert_eq!(tool("song.wav", AudioToVideo), "song Visualizer.mp4");
}

#[test]
fn row_audio_channels_mono_or_stereo() {
    assert_eq!(tool("song.mp3", AudioChannels), "song Mono.mp3");
    let stereo = ToolParams {
        channels: Channels::Stereo,
        ..ToolParams::default()
    };
    assert_eq!(
        tool_with("song.mp3", AudioChannels, stereo),
        "song Stereo.mp3"
    );
}

#[test]
fn row_combine_tools_use_first_stem() {
    assert_eq!(tool("first.jpg", CreatePdf), "first Combined.pdf");
    assert_eq!(tool("first.pdf", MergePdf), "first Merged.pdf");
    assert_eq!(tool("first.mov", JoinVideos), "first Joined.mov");
    assert_eq!(tool("first.mkv", JoinVideos), "first Joined.mkv");
    assert_eq!(tool("first.png", CreateCollage), "first Collage.png");
    assert_eq!(tool("first.jpg", CreateCollage), "first Collage.png");
}

#[test]
fn every_tool_has_a_naming_row() {
    for t in ToolKey::ALL {
        let name = tool("x.mp4", t);
        assert!(name.starts_with("x "), "{t:?}: {name}");
        assert!(!tool_suffix(t, &ToolParams::default()).is_empty());
    }
}

#[test]
fn only_three_tools_write_folders() {
    let folders: Vec<ToolKey> = ToolKey::ALL
        .into_iter()
        .filter(|&t| output_kind(&Operation::tool(t)) == OutputKind::Folder)
        .collect();
    assert_eq!(folders, vec![SplitVideo, SplitPdf, ExtractArchive]);
    assert_eq!(output_kind(&Operation::convert(Png)), OutputKind::File);
}

// ---- Conversions ----

#[test]
fn conversion_is_stem_dot_target() {
    assert_eq!(convert("photo.jpg", Png), "photo.png");
    assert_eq!(convert("song.flac", Mp3), "song.mp3");
    assert_eq!(convert("clip.mp4", Gif), "clip.gif");
    assert_eq!(convert("subs.srt", Vtt), "subs.vtt");
}

#[test]
fn conversion_uses_lowercase_target_ext() {
    assert_eq!(convert("PHOTO.JPG", Png), "PHOTO.png");
    assert_eq!(convert("Photo.JPEG", Webp), "Photo.webp");
}

#[test]
fn conversion_to_gz_writes_tar_gz() {
    assert_eq!(convert("x.zip", Gz), "x.tar.gz");
    assert_eq!(
        output_name("x.zip", &Operation::convert(Gz), &taken(&["x.tar.gz"])),
        "x 2.tar.gz"
    );
}

#[test]
fn tar_gz_and_tgz_stems_strip_as_one_unit() {
    assert_eq!(convert("x.tar.gz", Zip), "x.zip");
    assert_eq!(convert("x.TAR.GZ", Zip), "x.zip");
    assert_eq!(convert("x.tgz", Tar), "x.tar");
    assert_eq!(convert("x.gz", Zip), "x.zip");
    assert_eq!(split_name("x.tar.gz"), ("x", Some("tar.gz")));
    assert_eq!(split_name("x.Tar.Gz"), ("x", Some("Tar.Gz")));
    assert_eq!(split_name("x.tgz"), ("x", Some("tgz")));
}

#[test]
fn conversion_into_tar_gz_from_tar() {
    assert_eq!(convert("x.tar", Gz), "x.tar.gz");
    assert_eq!(convert("x.rar", Gz), "x.tar.gz");
}

// ---- Collisions ----

#[test]
fn collision_adds_2() {
    assert_eq!(
        output_name("a.jpg", &Operation::convert(Png), &taken(&["a.png"])),
        "a 2.png"
    );
}

#[test]
fn collision_adds_3() {
    assert_eq!(
        output_name(
            "a.jpg",
            &Operation::convert(Png),
            &taken(&["a.png", "a 2.png"])
        ),
        "a 3.png"
    );
}

#[test]
fn existing_name_2_gives_name_3() {
    // The `name 2.png` fixture from TESTING.md.
    assert_eq!(
        output_name(
            "name.jpg",
            &Operation::convert(Png),
            &taken(&["name.png", "name 2.png"])
        ),
        "name 3.png"
    );
}

#[test]
fn numbered_stems_are_not_renumbered() {
    assert_eq!(
        output_name(
            "name 2.jpg",
            &Operation::convert(Png),
            &taken(&["name 2.png"])
        ),
        "name 2 2.png"
    );
}

#[test]
fn collision_check_ignores_case() {
    assert_eq!(
        output_name("a.jpg", &Operation::convert(Png), &taken(&["A.PNG"])),
        "a 2.png"
    );
    assert_eq!(
        output_name(
            "Photo.JPG",
            &Operation::tool(Compress),
            &taken(&["photo compressed.jpg"])
        ),
        "Photo Compressed 2.JPG"
    );
    assert_eq!(
        output_name("ảnh.jpg", &Operation::convert(Png), &taken(&["ẢNH.PNG"])),
        "ảnh 2.png"
    );
    // Capital sharp s (U+1E9E) and ß are a case pair.
    assert_eq!(
        output_name(
            "straße.jpg",
            &Operation::convert(Png),
            &taken(&["STRAẞE.PNG"])
        ),
        "straße 2.png"
    );
    assert_eq!(
        output_name(
            "STRAẞE.jpg",
            &Operation::convert(Png),
            &taken(&["straße.png"])
        ),
        "STRAẞE 2.png"
    );
}

#[test]
fn gaps_in_numbering_take_the_first_free_number() {
    assert_eq!(
        output_name(
            "a.jpg",
            &Operation::convert(Png),
            &taken(&["a.png", "a 3.png"])
        ),
        "a 2.png"
    );
}

#[test]
fn tool_collision() {
    assert_eq!(
        output_name(
            "a.mp4",
            &Operation::tool(MuteVideo),
            &taken(&["a Muted.mp4"])
        ),
        "a Muted 2.mp4"
    );
}

#[test]
fn folder_collision() {
    assert_eq!(
        output_name(
            "x.zip",
            &Operation::tool(ExtractArchive),
            &taken(&["x Extracted"])
        ),
        "x Extracted 2"
    );
    // A file with the folder's name also blocks it.
    assert_eq!(
        output_name(
            "x.zip",
            &Operation::tool(ExtractArchive),
            &taken(&["X EXTRACTED"])
        ),
        "x Extracted 2"
    );
}

#[test]
fn unrelated_taken_names_do_not_matter() {
    assert_eq!(
        output_name(
            "a.jpg",
            &Operation::convert(Png),
            &taken(&["a.jpg", "a.webp", "b.png"])
        ),
        "a.png"
    );
}

// ---- "Same" extension keeps the source's literal extension ----

#[test]
fn same_ext_keeps_source_case() {
    assert_eq!(tool("Photo.JPG", Compress), "Photo Compressed.JPG");
    assert_eq!(tool("Clip.MoV", MuteVideo), "Clip Muted.MoV");
}

#[test]
fn same_ext_keeps_alias_spelling() {
    assert_eq!(tool("photo.jpeg", Compress), "photo Compressed.jpeg");
    assert_eq!(tool("scan.tif", CropImage), "scan Cropped.tif");
}

// ---- Awkward names ----

#[test]
fn unicode_names() {
    assert_eq!(convert("ảnh chụp.jpg", Png), "ảnh chụp.png");
    assert_eq!(tool("ảnh chụp.jpg", Compress), "ảnh chụp Compressed.jpg");
    assert_eq!(split_name("ảnh chụp.jpg"), ("ảnh chụp", Some("jpg")));
}

#[test]
fn names_with_spaces_and_many_dots() {
    assert_eq!(
        convert("my holiday.v2.final.jpg", Png),
        "my holiday.v2.final.png"
    );
    assert_eq!(split_name("a.b.c"), ("a.b", Some("c")));
}

#[test]
fn name_without_extension() {
    assert_eq!(split_name("Makefile"), ("Makefile", None));
    assert_eq!(convert("Makefile", Png), "Makefile.png");
    assert_eq!(tool("README", Compress), "README Compressed");
}

#[test]
fn dotfiles_have_no_extension() {
    assert_eq!(split_name(".env"), (".env", None));
    assert_eq!(split_name(".png"), (".png", None));
    assert_eq!(tool(".hidden", RemoveMetadata), ".hidden No Metadata");
    assert_eq!(split_name(".config.json"), (".config", Some("json")));
}

#[test]
fn trailing_dot_has_no_extension() {
    assert_eq!(split_name("notes."), ("notes.", None));
    assert_eq!(split_name("."), (".", None));
    assert_eq!(split_name(""), ("", None));
}

#[test]
fn bare_tar_gz_is_a_dotfile_with_gz() {
    assert_eq!(split_name(".tar.gz"), (".tar", Some("gz")));
    assert_eq!(split_name(".tgz"), (".tgz", None));
}

// ---- Pages ----

#[test]
fn documents_to_images_produce_pages() {
    assert!(produces_pages(Pdf, Png));
    assert!(produces_pages(Pdf, Jpg));
    assert!(produces_pages(Pptx, Png));
    assert!(produces_pages(Docx, Jpg));
    assert!(produces_pages(Txt, Png));
    assert!(!produces_pages(Pdf, Txt));
    assert!(!produces_pages(Jpg, Png));
    assert!(!produces_pages(Svg, Png));
    assert!(!produces_pages(Xlsx, Pdf));
}

#[test]
fn pages_folder_is_the_stem_and_collision_safe() {
    assert_eq!(pages_folder_name("report.pdf", &none()), "report");
    assert_eq!(
        pages_folder_name("report.pdf", &taken(&["Report"])),
        "report 2"
    );
    assert_eq!(
        pages_folder_name("report.pdf", &taken(&["report", "report 2"])),
        "report 3"
    );
}

#[test]
fn page_file_names_are_zero_padded_to_the_count() {
    assert_eq!(page_file_name("report", 3, 9, "png"), "report 3.png");
    assert_eq!(page_file_name("report", 3, 12, "png"), "report 03.png");
    assert_eq!(page_file_name("report", 12, 12, "jpg"), "report 12.jpg");
    assert_eq!(page_file_name("report", 7, 100, "png"), "report 007.png");
    assert_eq!(page_file_name("report", 1, 10, "png"), "report 01.png");
    assert_eq!(page_file_name("report", 1, 1, "png"), "report 1.png");
}

// ---- Params ----

#[test]
fn speed_labels() {
    let label = |h| Speed::from_hundredths(h).unwrap().label();
    assert_eq!(label(150), "1.5");
    assert_eq!(label(200), "2");
    assert_eq!(label(25), "0.25");
    assert_eq!(label(5), "0.05");
    assert_eq!(label(10), "0.1");
    assert_eq!(label(125), "1.25");
    assert_eq!(label(100), "1");
    assert_eq!(Speed::from_hundredths(0), None);
    assert_eq!(Speed::default().hundredths(), 200);
}

#[test]
fn tool_params_defaults() {
    let p = ToolParams::default();
    assert_eq!(p.channels, Channels::Mono);
    assert_eq!(p.speed.label(), "2");
    assert_eq!(p.target_lufs, -16);
    assert_eq!(p.compress, grape_core::CompressPreset::Balanced);
}

#[test]
fn operation_serde() {
    let op = Operation::convert(Png);
    assert_eq!(
        serde_json::to_string(&op).unwrap(),
        r#"{"kind":"convert","to":"png"}"#
    );
    let op: Operation = serde_json::from_str(r#"{"kind":"tool","tool":"mergePDF"}"#).unwrap();
    assert_eq!(op, Operation::tool(MergePdf));
    let op: Operation =
        serde_json::from_str(r#"{"kind":"tool","tool":"changeVideoSpeed","params":{"speed":150}}"#)
            .unwrap();
    assert_eq!(output_name("c.mp4", &op, &none()), "c Speed 1.5x.mp4");
    assert!(serde_json::from_str::<Operation>(
        r#"{"kind":"tool","tool":"changeVideoSpeed","params":{"speed":0}}"#
    )
    .is_err());
}
