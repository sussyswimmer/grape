//! Which conversions this build runs: [`supports`] and [`run_job`] decide
//! from the same table, so a pair is offered exactly when it can run.
#![allow(clippy::unwrap_used)]

use grape_core::{pairs, Features, Format, Operation, ToolKey};
use grape_engines::supports;

const RASTER: [Format; 5] = [
    Format::Jpg,
    Format::Png,
    Format::Webp,
    Format::Tiff,
    Format::Bmp,
];

#[test]
fn supports_the_raster_and_subtitle_pairs() {
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
    assert_eq!(supported.len(), 24);
    for pair @ (from, to) in &supported {
        assert!(
            (RASTER.contains(from) && RASTER.contains(to) && from != to)
                || subtitles.contains(pair),
            "{pair:?}"
        );
    }
    for pair in subtitles {
        assert!(supported.contains(&pair), "{pair:?}");
    }
    let full = pairs(Features::FULL)
        .into_iter()
        .filter(|&(from, to)| supports(&Operation::convert(to), Some(from)))
        .count();
    assert_eq!(full, 24);
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
