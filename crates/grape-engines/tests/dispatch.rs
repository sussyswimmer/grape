//! Which conversions this build runs: [`supports`] and [`run_job`] decide
//! from the same table, so a pair is offered exactly when it can run.
//! [`supports`] assumes no sidecars; the PDF and RAR pairs need theirs
//! ([`supports_with`]).
#![allow(clippy::unwrap_used)]

use grape_core::{pairs, tools_enabled, Features, Format, Operation, ToolKey};
use grape_engines::{supports, supports_with, SidecarPaths};
use std::path::PathBuf;

const RASTER: [Format; 5] = [
    Format::Jpg,
    Format::Png,
    Format::Webp,
    Format::Tiff,
    Format::Bmp,
];

#[test]
fn supports_the_raster_subtitle_and_archive_pairs() {
    let supported: Vec<(Format, Format)> = pairs(Features::V1_ONLY)
        .into_iter()
        .filter(|&(from, to)| supports(&Operation::convert(to), Some(from)))
        .collect();
    let subtitles = [
        (Format::Srt, Format::Vtt),
        (Format::Srt, Format::Txt),
        (Format::Vtt, Format::Srt),
        (Format::Vtt, Format::Txt),
    ];
    let archives = [
        (Format::Zip, Format::Tar),
        (Format::Zip, Format::Gz),
        (Format::Tar, Format::Zip),
        (Format::Tar, Format::Gz),
        (Format::Gz, Format::Zip),
        (Format::Gz, Format::Tar),
    ];
    assert_eq!(supported.len(), 30);
    for pair @ (from, to) in &supported {
        assert!(
            (RASTER.contains(from) && RASTER.contains(to) && from != to)
                || subtitles.contains(pair)
                || archives.contains(pair),
            "{pair:?}"
        );
    }
    for pair in subtitles.into_iter().chain(archives) {
        assert!(supported.contains(&pair), "{pair:?}");
    }
    let full = pairs(Features::FULL)
        .into_iter()
        .filter(|&(from, to)| supports(&Operation::convert(to), Some(from)))
        .count();
    assert_eq!(full, 30);
    assert!(!supports(
        &Operation::convert(Format::Png),
        Some(Format::Png)
    ));
    assert!(!supports(&Operation::convert(Format::Png), None));
    assert!(!supports(
        &Operation::convert(Format::Png),
        Some(Format::Heic)
    ));
    assert!(!supports(
        &Operation::tool(ToolKey::Compress),
        Some(Format::Png)
    ));
}

/// Every sidecar a full install has; any path will do, nothing runs here.
fn all_sidecars() -> SidecarPaths {
    let some = || Some(PathBuf::from("x"));
    SidecarPaths {
        pdfium: some(),
        pdf_worker: some(),
        bsdtar: some(),
        ..SidecarPaths::default()
    }
}

/// The `(from, to)` pairs over every format that run with `sidecars`.
fn runnable_pairs(sidecars: &SidecarPaths) -> Vec<(Format, Format)> {
    Format::ALL
        .into_iter()
        .flat_map(|from| Format::ALL.into_iter().map(move |to| (from, to)))
        .filter(|&(from, to)| supports_with(&Operation::convert(to), &[Some(from)], sidecars))
        .collect()
}

/// The `(tool, format)` pairs over every tool and format that run with
/// `sidecars`.
fn runnable_tools(sidecars: &SidecarPaths) -> Vec<(ToolKey, Format)> {
    ToolKey::ALL
        .into_iter()
        .flat_map(|tool| Format::ALL.into_iter().map(move |format| (tool, format)))
        .filter(|&(tool, format)| supports_with(&Operation::tool(tool), &[Some(format)], sidecars))
        .collect()
}

#[test]
fn pdf_and_rar_pairs_need_their_sidecars() {
    let all = all_sidecars();
    let with: Vec<(Format, Format)> = pairs(Features::V1_ONLY)
        .into_iter()
        .filter(|&(from, to)| supports_with(&Operation::convert(to), &[Some(from)], &all))
        .collect();
    assert_eq!(with.len(), 37);
    for to in [Format::Jpg, Format::Png, Format::Txt, Format::Md] {
        assert!(with.contains(&(Format::Pdf, to)), "{to:?}");
        assert!(
            !supports(&Operation::convert(to), Some(Format::Pdf)),
            "{to:?}"
        );
    }
    for to in [Format::Zip, Format::Tar, Format::Gz] {
        assert!(with.contains(&(Format::Rar, to)), "{to:?}");
        assert!(
            !supports(&Operation::convert(to), Some(Format::Rar)),
            "{to:?}"
        );
    }
    assert!(!supports_with(&Operation::convert(Format::Png), &[], &all));
}

#[test]
fn nothing_runs_outside_the_v1_matrix_and_tools() {
    let all = all_sidecars();
    // Over every pair of formats, only the 37 v1 pairs run.
    let v1 = pairs(Features::V1_ONLY);
    let runnable = runnable_pairs(&all);
    assert_eq!(runnable.len(), 37);
    for pair in &runnable {
        assert!(v1.contains(pair), "{pair:?} isn't a v1 pair");
    }
    // Over every tool and format, exactly these seven run, each offered on
    // the wheel for one item or for several.
    let tools = runnable_tools(&all);
    assert_eq!(tools.len(), 7, "{tools:?}");
    for pair in [
        (ToolKey::ExtractArchive, Format::Zip),
        (ToolKey::ExtractArchive, Format::Tar),
        (ToolKey::ExtractArchive, Format::Gz),
        (ToolKey::ExtractArchive, Format::Rar),
        (ToolKey::SplitPdf, Format::Pdf),
        (ToolKey::MergePdf, Format::Pdf),
        (ToolKey::RemoveMetadata, Format::Pdf),
    ] {
        assert!(tools.contains(&pair), "{pair:?}");
    }
    for &(tool, format) in &tools {
        assert!(
            tools_enabled(format, 1, Features::V1_ONLY).contains(&tool)
                || tools_enabled(format, 2, Features::V1_ONLY).contains(&tool),
            "{tool:?} on {format:?} isn't offered in v1"
        );
    }
    // PDF needs both PDFium and the worker.
    for without in [
        SidecarPaths {
            pdfium: None,
            ..all.clone()
        },
        SidecarPaths {
            pdf_worker: None,
            ..all.clone()
        },
    ] {
        assert!(
            runnable_pairs(&without)
                .iter()
                .all(|&(from, _)| from != Format::Pdf),
            "{without:?}"
        );
        assert!(
            runnable_tools(&without)
                .iter()
                .all(|&(_, format)| format != Format::Pdf),
            "{without:?}"
        );
        assert_eq!(runnable_pairs(&without).len(), 33, "{without:?}");
    }
    // RAR needs bsdtar.
    let no_bsdtar = SidecarPaths {
        bsdtar: None,
        ..all.clone()
    };
    assert!(runnable_pairs(&no_bsdtar)
        .iter()
        .all(|&(from, _)| from != Format::Rar));
    assert!(!runnable_tools(&no_bsdtar).contains(&(ToolKey::ExtractArchive, Format::Rar)));
}
