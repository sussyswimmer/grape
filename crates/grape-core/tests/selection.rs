//! Wheel content tests (`INTERACTION_SPEC.md` §5, §7; `TESTING.md` multi-select
//! and folder items).
#![allow(clippy::unwrap_used)]

use grape_core::{
    wheel_content, ActionKey, EmptyState, Features, Format, Operation, Platform, SelectionItem,
    ToolKey, ToolParams, WheelContent, WheelEntry, WheelMode,
};
use Format::*;
use ToolKey::*;

const V1: Features = Features::V1_ONLY;
const FULL: Features = Features::FULL;

fn files(names: &[&str]) -> Vec<SelectionItem> {
    names
        .iter()
        .map(|n| SelectionItem::file(format!("/data/{n}")))
        .collect()
}

fn conversions(items: &[SelectionItem], f: Features) -> WheelContent {
    wheel_content(items, WheelMode::Conversions, f)
}

fn tool_wheel(items: &[SelectionItem], f: Features) -> WheelContent {
    wheel_content(items, WheelMode::Tools, f)
}

fn converts(formats: &[Format]) -> WheelContent {
    WheelContent::Entries(formats.iter().map(|&f| WheelEntry::Convert(f)).collect())
}

fn tool_entries(tools: &[ToolKey]) -> WheelContent {
    WheelContent::Entries(tools.iter().map(|&t| WheelEntry::Tool(t)).collect())
}

fn all_actions() -> WheelContent {
    WheelContent::Entries(
        ActionKey::ALL
            .iter()
            .map(|&a| WheelEntry::Action(a))
            .collect(),
    )
}

#[test]
fn three_jpgs_v1() {
    let items = files(&["a.jpg", "b.jpg", "c.jpg"]);
    assert_eq!(
        conversions(&items, V1),
        converts(&[Png, Webp, Heic, Tiff, Avif, Bmp, Pdf])
    );
    assert_eq!(
        tool_wheel(&items, V1),
        tool_entries(&[Compress, RemoveMetadata, CreatePdf])
    );
}

#[test]
fn three_jpgs_full_show_create_pdf_and_collage() {
    let items = files(&["a.jpg", "b.jpg", "c.jpg"]);
    assert_eq!(
        tool_wheel(&items, FULL),
        tool_entries(&[
            Compress,
            RemoveMetadata,
            CropImage,
            CreatePdf,
            CreateCollage
        ])
    );
}

#[test]
fn one_jpg_v1() {
    let items = files(&["a.jpg"]);
    assert_eq!(
        tool_wheel(&items, V1),
        tool_entries(&[Compress, RemoveMetadata])
    );
}

#[test]
fn jpg_and_png_conversions_intersect_in_first_order() {
    let items = files(&["a.jpg", "b.png"]);
    assert_eq!(
        conversions(&items, V1),
        converts(&[Webp, Heic, Tiff, Avif, Bmp, Pdf])
    );
    let items = files(&["b.png", "a.jpg"]);
    assert_eq!(
        conversions(&items, V1),
        converts(&[Webp, Heic, Tiff, Avif, Bmp, Pdf])
    );
}

#[test]
fn jpg_and_svg_tools() {
    let items = files(&["a.jpg", "b.svg"]);
    assert_eq!(
        tool_wheel(&items, FULL),
        tool_entries(&[CreatePdf, CreateCollage])
    );
    assert_eq!(tool_wheel(&items, V1), tool_entries(&[CreatePdf]));
}

#[test]
fn jpg_and_pdf_in_v1_keep_only_remove_metadata() {
    // Compress is v1 on jpg but v2 on pdf, so it must drop out.
    let items = files(&["a.jpg", "b.pdf"]);
    assert_eq!(tool_wheel(&items, V1), tool_entries(&[RemoveMetadata]));
    assert_eq!(
        tool_wheel(&items, FULL),
        tool_entries(&[Compress, RemoveMetadata])
    );
    assert_eq!(conversions(&items, V1), converts(&[Png]));
}

#[test]
fn pdf_alone_in_v1() {
    let items = files(&["a.pdf"]);
    assert_eq!(
        tool_wheel(&items, V1),
        tool_entries(&[RemoveMetadata, SplitPdf])
    );
    assert_eq!(conversions(&items, V1), converts(&[Jpg, Png, Txt, Md]));
}

#[test]
fn two_pdfs_in_v1() {
    let items = files(&["a.pdf", "b.pdf"]);
    assert_eq!(
        tool_wheel(&items, V1),
        tool_entries(&[RemoveMetadata, SplitPdf, MergePdf])
    );
}

#[test]
fn mixed_videos_can_join() {
    let items = files(&["a.mp4", "b.mov", "c.mkv"]);
    assert_eq!(
        tool_wheel(&items, FULL),
        tool_entries(&[Compress, RemoveMetadata, MuteVideo, JoinVideos])
    );
    assert_eq!(
        tool_wheel(&items, V1),
        tool_entries(&[Compress, RemoveMetadata, MuteVideo])
    );
}

#[test]
fn audio_and_video_share_only_media_tools() {
    let items = files(&["a.mp3", "b.mp4"]);
    assert_eq!(
        tool_wheel(&items, V1),
        tool_entries(&[Compress, RemoveMetadata])
    );
    // mp4 converts to mp3, but mp3 never targets itself: no common target.
    assert_eq!(
        conversions(&items, V1),
        WheelContent::Empty(EmptyState::NoConversions)
    );
}

#[test]
fn archives_extract_together() {
    let items = files(&["a.zip", "b.tar.gz", "c.rar"]);
    assert_eq!(tool_wheel(&items, V1), tool_entries(&[ExtractArchive]));
    assert_eq!(conversions(&items, V1), converts(&[Tar]));
}

#[test]
fn folder_only_gets_actions() {
    let items = vec![SelectionItem::folder("/data/photos")];
    assert_eq!(
        conversions(&items, FULL),
        WheelContent::Empty(EmptyState::NoConversions)
    );
    assert_eq!(
        tool_wheel(&items, FULL),
        WheelContent::Empty(EmptyState::NoTools)
    );
    assert_eq!(
        wheel_content(&items, WheelMode::Actions, FULL),
        all_actions()
    );
}

#[test]
fn folder_mixed_with_files_only_gets_actions() {
    let mut items = files(&["a.jpg"]);
    items.push(SelectionItem::folder("/data/photos"));
    assert_eq!(
        conversions(&items, FULL),
        WheelContent::Empty(EmptyState::NoConversions)
    );
    assert_eq!(
        tool_wheel(&items, FULL),
        WheelContent::Empty(EmptyState::NoTools)
    );
    assert_eq!(wheel_content(&items, WheelMode::Actions, V1), all_actions());
}

#[test]
fn unknown_format_only_gets_actions() {
    let items = files(&["setup.exe"]);
    assert_eq!(items[0].format(), None);
    assert_eq!(
        conversions(&items, FULL),
        WheelContent::Empty(EmptyState::NoConversions)
    );
    assert_eq!(
        tool_wheel(&items, FULL),
        WheelContent::Empty(EmptyState::NoTools)
    );
    assert_eq!(wheel_content(&items, WheelMode::Actions, V1), all_actions());
    let mixed = files(&["a.jpg", "setup.exe"]);
    assert_eq!(
        conversions(&mixed, FULL),
        WheelContent::Empty(EmptyState::NoConversions)
    );
}

#[test]
fn empty_selection() {
    assert_eq!(
        conversions(&[], FULL),
        WheelContent::Empty(EmptyState::NoConversions)
    );
    assert_eq!(
        tool_wheel(&[], FULL),
        WheelContent::Empty(EmptyState::NoTools)
    );
    assert_eq!(wheel_content(&[], WheelMode::Actions, V1), all_actions());
}

#[test]
fn actions_always_have_six() {
    for names in [&["a.jpg"][..], &["a.docx", "b.exe"][..], &["x.html"][..]] {
        let items = files(names);
        assert_eq!(
            wheel_content(&items, WheelMode::Actions, V1)
                .entries()
                .len(),
            6
        );
    }
}

#[test]
fn docx_is_empty_in_v1_and_pages_full_has_eight() {
    let docx = files(&["a.docx"]);
    assert_eq!(
        conversions(&docx, V1),
        WheelContent::Empty(EmptyState::NoConversions)
    );
    assert_eq!(conversions(&docx, FULL).entries().len(), 8);
    let pages = files(&["a.pages"]);
    assert_eq!(
        conversions(&pages, V1),
        WheelContent::Empty(EmptyState::NoConversions)
    );
    assert_eq!(conversions(&pages, FULL).entries().len(), 8);
}

#[test]
fn html_has_no_conversions_or_tools() {
    let items = files(&["index.htm"]);
    assert_eq!(items[0].format(), Some(Html));
    assert_eq!(
        conversions(&items, FULL),
        WheelContent::Empty(EmptyState::NoConversions)
    );
    assert_eq!(
        tool_wheel(&items, FULL),
        WheelContent::Empty(EmptyState::NoTools)
    );
}

#[test]
fn svg_alone_has_conversions_but_no_tools() {
    let items = files(&["logo.svg"]);
    assert_eq!(conversions(&items, V1).entries().len(), 8);
    assert_eq!(
        tool_wheel(&items, V1),
        WheelContent::Empty(EmptyState::NoTools)
    );
}

#[test]
fn v1_versus_full_for_documents() {
    let items = files(&["notes.md", "readme.txt"]);
    assert_eq!(
        conversions(&items, V1),
        WheelContent::Empty(EmptyState::NoConversions)
    );
    assert_eq!(
        conversions(&items, FULL),
        converts(&[Docx, Pages, Pdf, Html, Epub, Png, Jpg])
    );
}

#[test]
fn item_constructors() {
    let item = SelectionItem::file("/data/Backup.TAR.GZ");
    assert_eq!(item.format(), Some(Gz));
    let item = SelectionItem::file_with_format("/data/noext", Some(Png));
    assert_eq!(item.format(), Some(Png));
    assert_eq!(SelectionItem::folder("/data").format(), None);
}

#[test]
fn entry_titles() {
    assert_eq!(WheelEntry::Convert(Gz).title(Platform::Windows), "GZIP");
    assert_eq!(
        WheelEntry::Tool(MergePdf).title(Platform::MacOs),
        "MERGE PDF"
    );
    assert_eq!(
        WheelEntry::Action(ActionKey::Share).title(Platform::MacOs),
        "AIRDROP"
    );
    assert_eq!(
        WheelEntry::Action(ActionKey::Share).title(Platform::Windows),
        "SHARE"
    );
}

#[test]
fn entry_operations() {
    let p = ToolParams::default();
    assert_eq!(
        WheelEntry::Convert(Png).operation(p),
        Some(Operation::convert(Png))
    );
    assert_eq!(
        WheelEntry::Tool(Compress).operation(p),
        Some(Operation::tool(Compress))
    );
    assert_eq!(WheelEntry::Action(ActionKey::Copy).operation(p), None);
}

#[test]
fn wheel_content_serializes_for_the_ui() {
    let items = files(&["a.pdf"]);
    let json = serde_json::to_string(&tool_wheel(&items, V1)).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"entries","value":[{"kind":"tool","key":"removeMetadata"},{"kind":"tool","key":"splitPDF"}]}"#
    );
    let json = serde_json::to_string(&WheelContent::Empty(EmptyState::NoTools)).unwrap();
    assert_eq!(json, r#"{"kind":"empty","value":"noTools"}"#);
    let json = serde_json::to_string(&WheelEntry::Convert(TextClipping)).unwrap();
    assert_eq!(json, r#"{"kind":"convert","key":"textClipping"}"#);
    let json = serde_json::to_string(&WheelMode::Conversions).unwrap();
    assert_eq!(json, r#""conversions""#);
}
