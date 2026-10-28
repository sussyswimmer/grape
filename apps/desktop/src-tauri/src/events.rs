//! What Rust sends the overlay, toast, shelf and panel windows: the event
//! contract in `docs/design/m1-drag-wheel.md` §C, the float shelf's
//! `float://view` and `float://drag-ended`, and the tool panel's
//! `panel://open` and `panel://close`, plus what the panel sends back in
//! `panel_run` ([`PanelRun`]). The UI mirrors these types in
//! `ui/src/shared/events.ts`, and both sides test against the same golden
//! JSON in `ui/src/shared/fixtures/`.

use grape_core::{wheel_layout, EmptyState, Geometry, Platform, Point, Segment, WheelContent};
use grape_core::{Channels, CompressPreset, WheelEntry, WheelMode, GEOMETRY};
use serde::{Deserialize, Serialize};

use crate::controller::KeyMap;

/// `wheel://open`: draw or redraw the wheel (open, key swap, files arrived).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WheelOpen {
    /// The wheel instance. Stays the same across swaps and content updates.
    pub seq: u64,
    /// Which wheel.
    pub mode: WheelMode,
    /// The wheel's center, in logical pixels from the overlay's top-left.
    pub anchor: Point,
    /// Physical pixels per logical pixel on the overlay's monitor.
    pub scale: f64,
    /// grape-core's geometry constants.
    pub geometry: Geometry,
    /// The user's key names, for the empty-state messages.
    pub keys: KeyNames,
    /// Whether to play the highlight tick.
    pub sounds: bool,
    /// What to draw.
    pub content: UiContent,
}

/// What the wheel shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UiContent {
    /// The files aren't known yet (no drop-target Enter): a neutral ring.
    Pending,
    /// Segments in wheel order.
    Entries {
        /// At `DENSE_AT` segments or more, use the compact label style.
        dense: bool,
        /// Geometry, title and item for each segment.
        segments: Vec<UiSegment>,
    },
    /// No segments: show this message.
    Empty {
        /// Which message.
        state: EmptyState,
    },
}

impl UiContent {
    /// The UI form of grape-core's wheel content.
    pub fn from_core(content: &WheelContent, platform: Platform) -> Self {
        match content {
            WheelContent::Empty(state) => UiContent::Empty { state: *state },
            WheelContent::Entries(entries) => {
                let segments = wheel_layout(entries.len())
                    .into_iter()
                    .zip(entries)
                    .map(|(geo, &entry)| UiSegment {
                        geo,
                        title: entry.title(platform).to_owned(),
                        entry,
                    })
                    .collect::<Vec<_>>();
                UiContent::Entries {
                    dense: segments.len() >= GEOMETRY.dense_at,
                    segments,
                }
            }
        }
    }
}

/// One segment: grape-core's geometry plus what it does.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UiSegment {
    /// Index, angles and label point.
    #[serde(flatten)]
    pub geo: Segment,
    /// The label, e.g. "PNG".
    pub title: String,
    /// What a drop here does.
    pub entry: WheelEntry,
}

/// The keys that open each wheel, as the user would name them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyNames {
    /// The conversions key, e.g. "Shift".
    pub conversions: String,
    /// The tools key, e.g. "Ctrl".
    pub tools: String,
    /// The file actions key, e.g. "Alt".
    pub actions: String,
}

impl KeyNames {
    /// The names for `keys` on this OS.
    pub fn of(keys: KeyMap) -> Self {
        Self {
            conversions: keys.conversions.name().to_owned(),
            tools: keys.tools.name().to_owned(),
            actions: keys.actions.name().to_owned(),
        }
    }
}

/// `wheel://highlight`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WheelHighlight {
    /// The wheel instance.
    pub seq: u64,
    /// The highlighted segment, or none.
    pub index: Option<usize>,
}

/// `wheel://close`: clear the wheel now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WheelClose {
    /// The wheel instance.
    pub seq: u64,
    /// Why, for the UI's own bookkeeping and the log.
    pub reason: CloseReason,
}

/// Why the wheel closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CloseReason {
    /// Dropped on a segment.
    Drop,
    /// Dropped off the ring, in the dead zone or in a gap.
    Cancel,
    /// Esc.
    Escape,
    /// The drag carries no files.
    NoFiles,
    /// The drag ended without a drop on the overlay.
    DragEnded,
    /// Nothing happened for 30 s.
    Watchdog,
    /// Tray → Pause Grape.
    Paused,
}

/// `toast://show`: replace whatever the toast shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToastShow {
    /// Identifies the batch, for `toast_clicked`.
    pub id: u64,
    /// How it looks.
    pub tone: ToastTone,
    /// The main line.
    pub title: String,
    /// A second line, if any.
    pub detail: Option<String>,
    /// 0–1 when known.
    pub progress: Option<f32>,
    /// Whether clicking reveals the output in Explorer or Finder.
    pub revealable: bool,
}

/// The toast's look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ToastTone {
    /// A job is running.
    Working,
    /// Finished.
    Done,
    /// Failed.
    Error,
    /// Something to know, not a result.
    Info,
}

/// `toast://hide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ToastHide {
    /// The batch to hide.
    pub id: u64,
}

/// `float://view`: what the float shelf lists, in shelf order. Sent after
/// every change and before the shelf is shown, and returned by
/// `float_view`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FloatView {
    /// The items, oldest first.
    pub items: Vec<FloatItem>,
}

/// One item on the shelf. The page only ever gets its id and name; Rust
/// keeps the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FloatItem {
    /// Names the item in `float_remove`, `float_reveal` and
    /// `float_drag_out`. Never reused while Grape runs.
    pub id: u64,
    /// The display name ("photo.jpg"), never a path.
    pub name: String,
    /// File or folder, for the glyph.
    pub kind: FloatKind,
    /// The item is gone from where it was: shown dimmed, not draggable.
    pub missing: bool,
}

/// What a shelf item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FloatKind {
    /// A file.
    File,
    /// A folder.
    Folder,
}

/// `float://drag-ended`: a drag out of the shelf is over, so the page can
/// take gestures again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FloatDragEnded {
    /// How it went.
    pub outcome: FloatOutcome,
}

/// How a drag out of the shelf ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FloatOutcome {
    /// Dropped somewhere that copied the files.
    Copied,
    /// Dropped somewhere that made shortcuts (aliases on the Mac).
    Linked,
    /// Dropped nowhere, or Esc. On the Mac also a drop on one of Grape's
    /// own windows.
    Cancelled,
    /// Nothing was there to drag, or the button was already up.
    Nothing,
    /// The drag couldn't start.
    Failed,
}

/// `panel://open`: show this panel request, replacing whatever the panel
/// showed. Also `panel_current`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PanelOpen {
    /// The panel's own request counter: from 1, never reused while Grape
    /// runs. Not the wheel's seq.
    pub seq: u64,
    /// The tool's name ("Merge PDFs").
    pub title: String,
    /// The dropped items, ids 1..=n in drop order.
    pub items: Vec<PanelItem>,
    /// What the panel offers, and where it starts.
    pub panel: PanelSpec,
}

/// One item, by id and display name. Rust keeps the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PanelItem {
    /// 1..=n, in drop order, per request.
    pub id: u64,
    /// The display name ("photo.jpg"), never a path.
    pub name: String,
}

/// A panel's controls and their starting values. `tool` is grape-core's
/// `ToolKey` serde key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "tool", rename_all_fields = "camelCase")]
pub enum PanelSpec {
    /// Compress: the preset to start from (the last one that ran).
    #[serde(rename = "compress")]
    Compress {
        /// Small, Balanced or High.
        preset: CompressPreset,
    },
    /// Audio Channels: always starts at Mono.
    #[serde(rename = "audioChannels")]
    AudioChannels {
        /// Mono or Stereo.
        channels: Channels,
    },
    /// Normalize Volume: a loudness target on a whole-LUFS grid.
    #[serde(rename = "normalizeAudio")]
    NormalizeAudio {
        /// Where the slider starts (the last target that ran).
        target_lufs: i8,
        /// The quietest target.
        min_lufs: i8,
        /// The loudest target.
        max_lufs: i8,
        /// The slider's step.
        step_lufs: u8,
    },
    /// Merge PDFs: the order list.
    #[serde(rename = "mergePDF")]
    MergePdf {
        /// `merged_names[i]` is the merged file's name if `items[i]` goes
        /// first. As long as `items`.
        merged_names: Vec<String>,
    },
}

/// `panel://close`: panel `seq` closed (run, cancelled, paused).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PanelClose {
    /// The panel request that closed.
    pub seq: u64,
}

/// What the page chose, in `panel_run`. Rust checks it against the open
/// request before anything runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "tool", rename_all_fields = "camelCase")]
pub enum PanelChoice {
    /// Compress with this preset.
    #[serde(rename = "compress")]
    Compress {
        /// Small, Balanced or High.
        preset: CompressPreset,
    },
    /// Audio Channels with this layout.
    #[serde(rename = "audioChannels")]
    AudioChannels {
        /// Mono or Stereo.
        channels: Channels,
    },
    /// Normalize Volume to this target.
    #[serde(rename = "normalizeAudio")]
    NormalizeAudio {
        /// In LUFS; Rust checks the range and the step.
        target_lufs: i8,
    },
    /// Merge PDFs, in the run's order.
    #[serde(rename = "mergePDF")]
    MergePdf,
}

/// `panel_run`'s args as one value, for the model and the fixture tests.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PanelRun {
    /// The panel request this answers.
    pub seq: u64,
    /// Every item id once, in the order to run them.
    pub order: Vec<u64>,
    /// The options.
    pub choice: PanelChoice,
}

#[cfg(test)]
mod tests {
    use super::*;
    use grape_core::{Format, ToolKey};
    use serde_json::Value;

    fn golden(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    fn json<T: Serialize>(value: &T) -> Value {
        serde_json::to_value(value).unwrap()
    }

    fn open(seq: u64, mode: WheelMode, scale: f64, sounds: bool, content: UiContent) -> WheelOpen {
        WheelOpen {
            seq,
            mode,
            anchor: Point { x: 400.0, y: 300.0 },
            scale,
            geometry: GEOMETRY,
            // The fixtures spell the Windows keys. `KeyNames::of` names
            // keys the way this OS does ("Option" on a Mac), so build the
            // fixture's names by hand.
            keys: KeyNames {
                conversions: "Shift".to_owned(),
                tools: "Ctrl".to_owned(),
                actions: "Alt".to_owned(),
            },
            sounds,
            content,
        }
    }

    #[test]
    fn open_pending_matches_the_fixture() {
        let payload = open(1, WheelMode::Conversions, 1.5, true, UiContent::Pending);
        assert_eq!(
            json(&payload),
            golden(include_str!(
                "../../ui/src/shared/fixtures/wheel-open-pending.json"
            ))
        );
    }

    #[test]
    fn open_entries_matches_the_fixture() {
        // Round numbers by hand, as the fixture has, not from wheel_layout.
        let segment = UiSegment {
            geo: Segment {
                index: 0,
                center: 1.5,
                start: 2.0,
                end: 1.0,
                label_x: 0.0,
                label_y: -84.0,
            },
            title: "PNG".to_owned(),
            entry: WheelEntry::Convert(Format::Png),
        };
        let content = UiContent::Entries {
            dense: false,
            segments: vec![segment],
        };
        let payload = open(2, WheelMode::Conversions, 1.0, false, content);
        assert_eq!(
            json(&payload),
            golden(include_str!(
                "../../ui/src/shared/fixtures/wheel-open-entries.json"
            ))
        );
    }

    #[test]
    fn open_empty_matches_the_fixture() {
        let content = UiContent::Empty {
            state: EmptyState::NoTools,
        };
        let payload = open(3, WheelMode::Tools, 1.0, true, content);
        assert_eq!(
            json(&payload),
            golden(include_str!(
                "../../ui/src/shared/fixtures/wheel-open-empty.json"
            ))
        );
    }

    #[test]
    fn highlight_and_close_match_the_fixtures() {
        assert_eq!(
            json(&WheelHighlight {
                seq: 1,
                index: Some(0)
            }),
            golden(include_str!(
                "../../ui/src/shared/fixtures/wheel-highlight.json"
            ))
        );
        assert_eq!(
            json(&WheelHighlight {
                seq: 1,
                index: None
            }),
            golden(include_str!(
                "../../ui/src/shared/fixtures/wheel-highlight-null.json"
            ))
        );
        assert_eq!(
            json(&WheelClose {
                seq: 1,
                reason: CloseReason::NoFiles
            }),
            golden(include_str!(
                "../../ui/src/shared/fixtures/wheel-close.json"
            ))
        );
    }

    #[test]
    fn toasts_match_the_fixtures() {
        let working = ToastShow {
            id: 4,
            tone: ToastTone::Working,
            title: "Making photo.png…".to_owned(),
            detail: None,
            progress: None,
            revealable: false,
        };
        assert_eq!(
            json(&working),
            golden(include_str!(
                "../../ui/src/shared/fixtures/toast-show-working.json"
            ))
        );
        let done = ToastShow {
            id: 4,
            tone: ToastTone::Done,
            title: "Done".to_owned(),
            detail: Some("photo.png".to_owned()),
            progress: None,
            revealable: true,
        };
        assert_eq!(
            json(&done),
            golden(include_str!(
                "../../ui/src/shared/fixtures/toast-show-done.json"
            ))
        );
        assert_eq!(
            json(&ToastHide { id: 4 }),
            golden(include_str!("../../ui/src/shared/fixtures/toast-hide.json"))
        );
    }

    #[test]
    fn float_views_match_the_fixtures() {
        let view = FloatView {
            items: vec![
                FloatItem {
                    id: 1,
                    name: "photo.jpg".to_owned(),
                    kind: FloatKind::File,
                    missing: false,
                },
                FloatItem {
                    id: 2,
                    name: "Invoices".to_owned(),
                    kind: FloatKind::Folder,
                    missing: false,
                },
                FloatItem {
                    id: 3,
                    name: "notes.txt".to_owned(),
                    kind: FloatKind::File,
                    missing: true,
                },
            ],
        };
        assert_eq!(
            json(&view),
            golden(include_str!("../../ui/src/shared/fixtures/float-view.json"))
        );
        assert_eq!(
            json(&FloatView { items: vec![] }),
            golden(include_str!(
                "../../ui/src/shared/fixtures/float-view-empty.json"
            ))
        );
        assert_eq!(
            json(&FloatDragEnded {
                outcome: FloatOutcome::Copied
            }),
            golden(include_str!(
                "../../ui/src/shared/fixtures/float-drag-ended.json"
            ))
        );
    }

    #[test]
    fn every_float_outcome_is_lower_case() {
        let names: Vec<Value> = [
            FloatOutcome::Copied,
            FloatOutcome::Linked,
            FloatOutcome::Cancelled,
            FloatOutcome::Nothing,
            FloatOutcome::Failed,
        ]
        .iter()
        .map(json)
        .collect();
        assert_eq!(
            names,
            ["copied", "linked", "cancelled", "nothing", "failed"].map(Value::from)
        );
    }

    #[test]
    fn every_close_reason_is_camel_case() {
        let names: Vec<Value> = [
            CloseReason::Drop,
            CloseReason::Cancel,
            CloseReason::Escape,
            CloseReason::NoFiles,
            CloseReason::DragEnded,
            CloseReason::Watchdog,
            CloseReason::Paused,
        ]
        .iter()
        .map(json)
        .collect();
        assert_eq!(
            names,
            [
                "drop",
                "cancel",
                "escape",
                "noFiles",
                "dragEnded",
                "watchdog",
                "paused"
            ]
            .map(Value::from)
        );
    }

    #[test]
    fn content_from_core_lays_out_titled_segments() {
        let core = WheelContent::Entries(vec![
            WheelEntry::Convert(Format::Jpg),
            WheelEntry::Convert(Format::Webp),
            WheelEntry::Tool(ToolKey::Compress),
        ]);
        let UiContent::Entries { dense, segments } = UiContent::from_core(&core, Platform::Windows)
        else {
            panic!("expected entries");
        };
        assert!(!dense);
        let titles: Vec<&str> = segments.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["JPG", "WEBP", "COMPRESS"]);
        assert_eq!(
            segments.iter().map(|s| s.geo).collect::<Vec<_>>(),
            wheel_layout(3)
        );
        let empty = WheelContent::Empty(EmptyState::NoConversions);
        assert_eq!(
            UiContent::from_core(&empty, Platform::Windows),
            UiContent::Empty {
                state: EmptyState::NoConversions
            }
        );
    }

    #[test]
    fn seven_segments_are_dense() {
        let core = WheelContent::Entries(vec![WheelEntry::Convert(Format::Png); 7]);
        let UiContent::Entries { dense, .. } = UiContent::from_core(&core, Platform::Windows)
        else {
            panic!("expected entries");
        };
        assert!(dense);
    }

    fn panel_open(seq: u64, title: &str, names: &[&str], panel: PanelSpec) -> PanelOpen {
        PanelOpen {
            seq,
            title: title.to_owned(),
            items: names
                .iter()
                .zip(1..)
                .map(|(name, id)| PanelItem {
                    id,
                    name: (*name).to_owned(),
                })
                .collect(),
            panel,
        }
    }

    #[test]
    fn panel_opens_and_the_close_match_the_fixtures() {
        let compress = panel_open(
            1,
            "Compress",
            &["photo.jpg", "beach.png"],
            PanelSpec::Compress {
                preset: CompressPreset::Balanced,
            },
        );
        assert_eq!(
            json(&compress),
            golden(include_str!(
                "../../ui/src/shared/fixtures/panel-open-compress.json"
            ))
        );
        let channels = panel_open(
            2,
            "Audio Channels",
            &["interview.wav"],
            PanelSpec::AudioChannels {
                channels: Channels::Mono,
            },
        );
        assert_eq!(
            json(&channels),
            golden(include_str!(
                "../../ui/src/shared/fixtures/panel-open-channels.json"
            ))
        );
        let normalize = panel_open(
            3,
            "Normalize Volume",
            &["episode 12.mp3", "intro.m4a"],
            PanelSpec::NormalizeAudio {
                target_lufs: -16,
                min_lufs: -24,
                max_lufs: -10,
                step_lufs: 1,
            },
        );
        assert_eq!(
            json(&normalize),
            golden(include_str!(
                "../../ui/src/shared/fixtures/panel-open-normalize.json"
            ))
        );
        let merge = panel_open(
            4,
            "Merge PDFs",
            &["report.pdf", "cover.pdf", "appendix.pdf"],
            PanelSpec::MergePdf {
                merged_names: [
                    "report Merged.pdf",
                    "cover Merged.pdf",
                    "appendix Merged.pdf",
                ]
                .map(str::to_owned)
                .to_vec(),
            },
        );
        assert_eq!(
            json(&merge),
            golden(include_str!(
                "../../ui/src/shared/fixtures/panel-open-merge.json"
            ))
        );
        assert_eq!(
            json(&PanelClose { seq: 4 }),
            golden(include_str!(
                "../../ui/src/shared/fixtures/panel-close.json"
            ))
        );
    }

    fn run_fixture(text: &str) -> PanelRun {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn panel_runs_read_from_the_fixtures() {
        assert_eq!(
            run_fixture(include_str!(
                "../../ui/src/shared/fixtures/panel-run-compress.json"
            )),
            PanelRun {
                seq: 1,
                order: vec![1, 2],
                choice: PanelChoice::Compress {
                    preset: CompressPreset::Small
                },
            }
        );
        assert_eq!(
            run_fixture(include_str!(
                "../../ui/src/shared/fixtures/panel-run-channels.json"
            )),
            PanelRun {
                seq: 2,
                order: vec![1],
                choice: PanelChoice::AudioChannels {
                    channels: Channels::Stereo
                },
            }
        );
        assert_eq!(
            run_fixture(include_str!(
                "../../ui/src/shared/fixtures/panel-run-normalize.json"
            )),
            PanelRun {
                seq: 3,
                order: vec![1, 2],
                choice: PanelChoice::NormalizeAudio { target_lufs: -14 },
            }
        );
        assert_eq!(
            run_fixture(include_str!(
                "../../ui/src/shared/fixtures/panel-run-merge.json"
            )),
            PanelRun {
                seq: 4,
                order: vec![2, 1, 3],
                choice: PanelChoice::MergePdf,
            }
        );
    }

    #[test]
    fn panel_cancel_carries_only_the_seq() {
        /// `panel_cancel`'s args.
        #[derive(Debug, PartialEq, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Cancel {
            seq: u64,
        }
        let cancel: Cancel = serde_json::from_str(include_str!(
            "../../ui/src/shared/fixtures/panel-cancel.json"
        ))
        .unwrap();
        assert_eq!(cancel, Cancel { seq: 4 });
    }

    #[test]
    fn every_panel_tag_is_the_tools_serde_key() {
        let specs = [
            (
                PanelSpec::Compress {
                    preset: CompressPreset::High,
                },
                ToolKey::Compress,
            ),
            (
                PanelSpec::AudioChannels {
                    channels: Channels::Stereo,
                },
                ToolKey::AudioChannels,
            ),
            (
                PanelSpec::NormalizeAudio {
                    target_lufs: -16,
                    min_lufs: -24,
                    max_lufs: -10,
                    step_lufs: 1,
                },
                ToolKey::NormalizeAudio,
            ),
            (
                PanelSpec::MergePdf {
                    merged_names: vec![],
                },
                ToolKey::MergePdf,
            ),
        ];
        for (spec, tool) in specs {
            assert_eq!(json(&spec)["tool"], json(&tool), "{tool:?}");
            let choice: PanelChoice = serde_json::from_value(serde_json::json!({
                "tool": json(&tool),
                "preset": "small",
                "channels": "mono",
                "targetLufs": -16,
            }))
            .unwrap();
            let chosen = match choice {
                PanelChoice::Compress { .. } => ToolKey::Compress,
                PanelChoice::AudioChannels { .. } => ToolKey::AudioChannels,
                PanelChoice::NormalizeAudio { .. } => ToolKey::NormalizeAudio,
                PanelChoice::MergePdf => ToolKey::MergePdf,
            };
            assert_eq!(chosen, tool);
        }
    }

    #[test]
    fn an_unknown_tool_preset_or_target_is_refused() {
        for bad in [
            r#"{"tool":"removeMetadata"}"#,
            r#"{"tool":"compress","preset":"medium"}"#,
            r#"{"tool":"compress"}"#,
            r#"{"tool":"audioChannels","channels":"surround"}"#,
            r#"{"tool":"normalizeAudio","targetLufs":-16.5}"#,
            r#"{"tool":"normalizeAudio","targetLufs":-200}"#,
            r#"{"tool":"normalizeAudio","targetLufs":"-16"}"#,
            r#"{"preset":"small"}"#,
        ] {
            assert!(
                serde_json::from_str::<PanelChoice>(bad).is_err(),
                "{bad} was accepted"
            );
        }
        let bad_run = r#"{"seq":1,"order":[1],"choice":{"tool":"compress","preset":"medium"}}"#;
        assert!(serde_json::from_str::<PanelRun>(bad_run).is_err());
    }
}
