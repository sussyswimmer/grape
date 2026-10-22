//! Job planning tests (`FORMAT_MATRIX.md` → "Output naming", "Location").
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use grape_core::{
    plan, DirSnapshot, Features, Format, JobOutput, Operation, OutputLocation, PlanError,
    Selection, SelectionItem, Settings, ToolKey,
};
use Format::*;
use ToolKey::*;

const PHOTOS: &str = "/home/u/photos";
const DOCS: &str = "/home/u/docs";
const OUT: &str = "/home/u/out";
const FALLBACK: &str = "/home/u/Downloads/Grape";

fn p(dir: &str, name: &str) -> PathBuf {
    Path::new(dir).join(name)
}

fn writable(names: &[&str]) -> DirSnapshot {
    DirSnapshot::new(names.iter().copied(), true)
}

fn read_only(names: &[&str]) -> DirSnapshot {
    DirSnapshot::new(names.iter().copied(), false)
}

/// Files in `dir`, whose snapshot lists exactly those names plus `extra`.
fn selection_in(dir: &str, names: &[&str], extra: &[&str], dir_writable: bool) -> Selection {
    let items = names
        .iter()
        .map(|n| SelectionItem::file(p(dir, n)))
        .collect();
    let all: Vec<&str> = names.iter().chain(extra).copied().collect();
    let snapshot = DirSnapshot::new(all, dir_writable);
    Selection::new(items)
        .with_dir(dir, snapshot)
        .with_dir(FALLBACK, writable(&[]))
}

fn settings() -> Settings {
    Settings::new(FALLBACK)
}

fn full() -> Settings {
    Settings {
        features: Features::FULL,
        ..settings()
    }
}

fn file_outputs(sel: &Selection, op: Operation, s: &Settings) -> Vec<PathBuf> {
    plan(sel, &op, s)
        .unwrap()
        .jobs
        .into_iter()
        .map(|job| match job.output {
            JobOutput::File(path) => path,
            other => panic!("expected a file output, got {other:?}"),
        })
        .collect()
}

#[test]
fn conversion_writes_next_to_source() {
    let sel = selection_in(PHOTOS, &["a.jpg"], &[], true);
    let jobs = plan(&sel, &Operation::convert(Png), &settings())
        .unwrap()
        .jobs;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].inputs, vec![p(PHOTOS, "a.jpg")]);
    assert_eq!(jobs[0].output, JobOutput::File(p(PHOTOS, "a.png")));
    assert_eq!(jobs[0].op, Operation::convert(Png));
    assert!(!jobs[0].used_fallback);
}

#[test]
fn existing_names_on_disk_collide() {
    let sel = selection_in(PHOTOS, &["a.jpg"], &["a.png", "A 2.PNG"], true);
    assert_eq!(
        file_outputs(&sel, Operation::convert(Png), &settings()),
        vec![p(PHOTOS, "a 3.png")]
    );
}

#[test]
fn batch_collision_between_planned_names() {
    let sel = selection_in(PHOTOS, &["a.jpg", "a.webp"], &[], true);
    assert_eq!(
        file_outputs(&sel, Operation::convert(Png), &settings()),
        vec![p(PHOTOS, "a.png"), p(PHOTOS, "a 2.png")]
    );
}

#[test]
fn per_file_tool_on_multi_selection_gives_one_job_each() {
    let sel = selection_in(PHOTOS, &["a.jpg", "b.jpg", "c.jpg"], &[], true);
    let jobs = plan(&sel, &Operation::tool(Compress), &settings())
        .unwrap()
        .jobs;
    assert_eq!(jobs.len(), 3);
    for (job, name) in jobs.iter().zip(["a", "b", "c"]) {
        assert_eq!(job.inputs, vec![p(PHOTOS, &format!("{name}.jpg"))]);
        assert_eq!(
            job.output,
            JobOutput::File(p(PHOTOS, &format!("{name} Compressed.jpg")))
        );
    }
}

#[test]
fn sources_are_never_overwritten_even_if_the_snapshot_misses_them() {
    let items = vec![
        SelectionItem::file(p(PHOTOS, "a.jpg")),
        SelectionItem::file(p(PHOTOS, "a Compressed.jpg")),
    ];
    let sel = Selection::new(items).with_dir(PHOTOS, writable(&[]));
    assert_eq!(
        file_outputs(&sel, Operation::tool(Compress), &settings()),
        vec![
            p(PHOTOS, "a Compressed 2.jpg"),
            p(PHOTOS, "a Compressed Compressed.jpg"),
        ]
    );
}

#[test]
fn read_only_source_folder_falls_back() {
    let sel = selection_in(PHOTOS, &["a.jpg"], &[], false);
    let jobs = plan(&sel, &Operation::convert(Png), &settings())
        .unwrap()
        .jobs;
    assert_eq!(jobs[0].output, JobOutput::File(p(FALLBACK, "a.png")));
    assert!(jobs[0].used_fallback);
}

#[test]
fn fallback_names_collide_too() {
    let sel = selection_in(PHOTOS, &["a.jpg"], &[], false).with_dir(FALLBACK, writable(&["a.png"]));
    let jobs = plan(&sel, &Operation::convert(Png), &settings())
        .unwrap()
        .jobs;
    assert_eq!(jobs[0].output, JobOutput::File(p(FALLBACK, "a 2.png")));
}

#[test]
fn fixed_output_folder_is_used() {
    let sel = selection_in(PHOTOS, &["a.jpg"], &[], true).with_dir(OUT, writable(&[]));
    let s = Settings {
        output: OutputLocation::Folder(OUT.into()),
        ..settings()
    };
    let jobs = plan(&sel, &Operation::convert(Png), &s).unwrap().jobs;
    assert_eq!(jobs[0].output, JobOutput::File(p(OUT, "a.png")));
    assert!(!jobs[0].used_fallback);
}

#[test]
fn unwritable_output_folder_falls_back() {
    let sel = selection_in(PHOTOS, &["a.jpg"], &[], true).with_dir(OUT, read_only(&[]));
    let s = Settings {
        output: OutputLocation::Folder(OUT.into()),
        ..settings()
    };
    let jobs = plan(&sel, &Operation::convert(Png), &s).unwrap().jobs;
    assert_eq!(jobs[0].output, JobOutput::File(p(FALLBACK, "a.png")));
    assert!(jobs[0].used_fallback);
}

#[test]
fn same_names_from_two_folders_into_one_output_folder() {
    let items = vec![
        SelectionItem::file(p(PHOTOS, "a.jpg")),
        SelectionItem::file(p(DOCS, "a.jpg")),
    ];
    let sel = Selection::new(items)
        .with_dir(PHOTOS, writable(&["a.jpg"]))
        .with_dir(DOCS, writable(&["a.jpg"]))
        .with_dir(OUT, writable(&[]))
        .with_dir(FALLBACK, writable(&[]));
    let s = Settings {
        output: OutputLocation::Folder(OUT.into()),
        ..settings()
    };
    assert_eq!(
        file_outputs(&sel, Operation::convert(Png), &s),
        vec![p(OUT, "a.png"), p(OUT, "a 2.png")]
    );
}

#[test]
fn items_in_different_folders_stay_next_to_their_source() {
    let items = vec![
        SelectionItem::file(p(PHOTOS, "a.jpg")),
        SelectionItem::file(p(DOCS, "a.jpg")),
    ];
    let sel = Selection::new(items)
        .with_dir(PHOTOS, writable(&["a.jpg"]))
        .with_dir(DOCS, writable(&["a.jpg"]));
    assert_eq!(
        file_outputs(&sel, Operation::convert(Png), &settings()),
        vec![p(PHOTOS, "a.png"), p(DOCS, "a.png")]
    );
}

#[test]
fn no_writable_destination() {
    let sel = selection_in(PHOTOS, &["a.jpg"], &[], false).with_dir(FALLBACK, read_only(&[]));
    assert_eq!(
        plan(&sel, &Operation::convert(Png), &settings()),
        Err(PlanError::NoWritableDestination)
    );
}

#[test]
fn missing_source_dir_snapshot() {
    let sel = Selection::new(vec![SelectionItem::file(p(PHOTOS, "a.jpg"))]);
    assert_eq!(
        plan(&sel, &Operation::convert(Png), &settings()),
        Err(PlanError::MissingDirSnapshot(PHOTOS.into()))
    );
}

#[test]
fn missing_fallback_snapshot_when_needed() {
    let sel = Selection::new(vec![SelectionItem::file(p(PHOTOS, "a.jpg"))])
        .with_dir(PHOTOS, read_only(&["a.jpg"]));
    assert_eq!(
        plan(&sel, &Operation::convert(Png), &settings()),
        Err(PlanError::MissingDirSnapshot(FALLBACK.into()))
    );
}

#[test]
fn fallback_snapshot_not_needed_when_writable() {
    let sel = Selection::new(vec![SelectionItem::file(p(PHOTOS, "a.jpg"))])
        .with_dir(PHOTOS, writable(&["a.jpg"]));
    assert!(plan(&sel, &Operation::convert(Png), &settings()).is_ok());
}

#[test]
fn empty_selection() {
    assert_eq!(
        plan(&Selection::default(), &Operation::convert(Png), &settings()),
        Err(PlanError::EmptySelection)
    );
}

#[test]
fn not_offered_self_conversion() {
    let sel = selection_in(PHOTOS, &["a.png"], &[], true);
    assert_eq!(
        plan(&sel, &Operation::convert(Png), &settings()),
        Err(PlanError::NotOffered)
    );
}

#[test]
fn not_offered_v2_pair_in_v1() {
    let sel = selection_in(DOCS, &["a.docx"], &[], true);
    assert_eq!(
        plan(&sel, &Operation::convert(Pdf), &settings()),
        Err(PlanError::NotOffered)
    );
    assert!(plan(&sel, &Operation::convert(Pdf), &full()).is_ok());
}

#[test]
fn not_offered_combine_with_one_file() {
    let sel = selection_in(DOCS, &["a.pdf"], &[], true);
    assert_eq!(
        plan(&sel, &Operation::tool(MergePdf), &settings()),
        Err(PlanError::NotOffered)
    );
}

#[test]
fn not_offered_for_folders() {
    let sel = Selection::new(vec![SelectionItem::folder(p(PHOTOS, "album"))])
        .with_dir(PHOTOS, writable(&["album"]));
    assert_eq!(
        plan(&sel, &Operation::convert(Zip), &full()),
        Err(PlanError::NotOffered)
    );
}

#[test]
fn not_offered_mixed_tool_tier() {
    let items = vec![
        SelectionItem::file(p(PHOTOS, "a.jpg")),
        SelectionItem::file(p(DOCS, "b.pdf")),
    ];
    let sel = Selection::new(items)
        .with_dir(PHOTOS, writable(&[]))
        .with_dir(DOCS, writable(&[]));
    assert_eq!(
        plan(&sel, &Operation::tool(Compress), &settings()),
        Err(PlanError::NotOffered)
    );
    assert!(plan(&sel, &Operation::tool(RemoveMetadata), &settings()).is_ok());
}

#[test]
fn combine_makes_one_job_in_selection_order() {
    let sel = selection_in(PHOTOS, &["b.jpg", "a.png", "c.jpg"], &[], true);
    let jobs = plan(&sel, &Operation::tool(CreatePdf), &settings())
        .unwrap()
        .jobs;
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].inputs,
        vec![p(PHOTOS, "b.jpg"), p(PHOTOS, "a.png"), p(PHOTOS, "c.jpg")]
    );
    assert_eq!(jobs[0].output, JobOutput::File(p(PHOTOS, "b Combined.pdf")));
}

#[test]
fn combine_goes_next_to_the_first_item() {
    let items = vec![
        SelectionItem::file(p(DOCS, "z.pdf")),
        SelectionItem::file(p(PHOTOS, "a.pdf")),
    ];
    let sel = Selection::new(items)
        .with_dir(DOCS, writable(&["z.pdf", "z Merged.pdf"]))
        .with_dir(PHOTOS, writable(&["a.pdf"]));
    let jobs = plan(&sel, &Operation::tool(MergePdf), &settings())
        .unwrap()
        .jobs;
    assert_eq!(jobs[0].output, JobOutput::File(p(DOCS, "z Merged 2.pdf")));
}

#[test]
fn combine_falls_back_with_flag() {
    let sel = selection_in(PHOTOS, &["a.mp4", "b.mov"], &[], false);
    let jobs = plan(&sel, &Operation::tool(JoinVideos), &full())
        .unwrap()
        .jobs;
    assert_eq!(jobs[0].output, JobOutput::File(p(FALLBACK, "a Joined.mp4")));
    assert!(jobs[0].used_fallback);
}

#[test]
fn folder_tools_plan_folders() {
    let sel = selection_in(DOCS, &["x.zip"], &["x Extracted"], true);
    let jobs = plan(&sel, &Operation::tool(ExtractArchive), &settings())
        .unwrap()
        .jobs;
    assert_eq!(jobs[0].output, JobOutput::Folder(p(DOCS, "x Extracted 2")));
    let sel = selection_in(DOCS, &["doc.pdf"], &[], true);
    let jobs = plan(&sel, &Operation::tool(SplitPdf), &settings())
        .unwrap()
        .jobs;
    assert_eq!(jobs[0].output, JobOutput::Folder(p(DOCS, "doc Pages")));
}

#[test]
fn pdf_to_png_reserves_file_and_folder() {
    let sel = selection_in(DOCS, &["doc.pdf"], &[], true);
    let jobs = plan(&sel, &Operation::convert(Png), &settings())
        .unwrap()
        .jobs;
    assert_eq!(
        jobs[0].output,
        JobOutput::PagesFileOrFolder {
            file: p(DOCS, "doc.png"),
            folder: p(DOCS, "doc"),
            page_stem: "doc".into(),
        }
    );
    assert_eq!(jobs[0].output.display_path(), p(DOCS, "doc.png"));
}

#[test]
fn pages_folder_collides_with_existing_names() {
    let sel = selection_in(DOCS, &["doc.pdf"], &["Doc"], true);
    let jobs = plan(&sel, &Operation::convert(Jpg), &settings())
        .unwrap()
        .jobs;
    assert_eq!(
        jobs[0].output,
        JobOutput::PagesFileOrFolder {
            file: p(DOCS, "doc.jpg"),
            folder: p(DOCS, "doc 2"),
            page_stem: "doc".into(),
        }
    );
}

#[test]
fn page_outputs_are_reserved_for_later_jobs() {
    let sel = selection_in(DOCS, &["doc.pdf", "doc.txt"], &[], true);
    let jobs = plan(&sel, &Operation::convert(Png), &full()).unwrap().jobs;
    assert_eq!(
        jobs[1].output,
        JobOutput::PagesFileOrFolder {
            file: p(DOCS, "doc 2.png"),
            folder: p(DOCS, "doc 2"),
            page_stem: "doc".into(),
        }
    );
}

#[test]
fn pdf_to_txt_is_a_plain_file() {
    let sel = selection_in(DOCS, &["doc.pdf"], &[], true);
    assert_eq!(
        file_outputs(&sel, Operation::convert(Txt), &settings()),
        vec![p(DOCS, "doc.txt")]
    );
}

#[test]
fn gz_conversion_writes_tar_gz() {
    let sel = selection_in(DOCS, &["x.zip"], &["x.tar.gz"], true);
    assert_eq!(
        file_outputs(&sel, Operation::convert(Gz), &settings()),
        vec![p(DOCS, "x 2.tar.gz")]
    );
}

#[test]
fn compress_wav_to_flac_follows_the_detected_format() {
    const MUSIC: &str = "/home/u/music";
    let items = vec![
        // WAV content behind an mp3 extension: compress writes flac.
        SelectionItem::file_with_format(p(MUSIC, "take.mp3"), Some(Wav)),
        // MP3 content behind a wav extension: "same" keeps the extension.
        SelectionItem::file_with_format(p(MUSIC, "song.wav"), Some(Mp3)),
        // WAV content with no extension.
        SelectionItem::file_with_format(p(MUSIC, "memo"), Some(Wav)),
        // Detected from the extension.
        SelectionItem::file(p(MUSIC, "loop.WAV")),
    ];
    let sel = Selection::new(items).with_dir(MUSIC, writable(&[]));
    assert_eq!(
        file_outputs(&sel, Operation::tool(Compress), &settings()),
        vec![
            p(MUSIC, "take Compressed.flac"),
            p(MUSIC, "song Compressed.wav"),
            p(MUSIC, "memo Compressed.flac"),
            p(MUSIC, "loop Compressed.flac"),
        ]
    );
}

#[test]
fn source_without_parent_is_an_error() {
    let sel = Selection::new(vec![SelectionItem::file("a.jpg")]);
    assert_eq!(
        plan(&sel, &Operation::convert(Png), &settings()),
        Err(PlanError::NoParentDir("a.jpg".into()))
    );
}

#[test]
fn source_without_file_name_is_an_error() {
    let item = SelectionItem::file_with_format("/home/u/..", Some(Jpg));
    let sel = Selection::new(vec![item]).with_dir("/home/u", writable(&[]));
    assert_eq!(
        plan(&sel, &Operation::convert(Png), &settings()),
        Err(PlanError::NoFileName("/home/u/..".into()))
    );
}

#[test]
fn plans_are_deterministic() {
    let sel = selection_in(
        PHOTOS,
        &["a.jpg", "a.webp", "b.png"],
        &["a.png", "b 2.png"],
        true,
    );
    let first = plan(&sel, &Operation::convert(Png), &settings());
    for _ in 0..10 {
        assert_eq!(plan(&sel, &Operation::convert(Png), &settings()), first);
    }
}

#[test]
fn plan_errors_have_messages() {
    assert_eq!(
        PlanError::NotOffered.to_string(),
        "this operation isn't offered for the selection"
    );
    assert!(PlanError::MissingDirSnapshot(PHOTOS.into())
        .to_string()
        .contains("photos"));
}

#[test]
fn job_plan_serializes() {
    let sel = selection_in(DOCS, &["doc.pdf"], &[], true);
    let jobs = plan(&sel, &Operation::convert(Png), &settings()).unwrap();
    let json = serde_json::to_value(&jobs).unwrap();
    let job = &json["jobs"][0];
    assert_eq!(job["op"]["kind"], "convert");
    assert_eq!(job["usedFallback"], false);
    assert_eq!(job["output"]["pagesFileOrFolder"]["pageStem"], "doc");
}
