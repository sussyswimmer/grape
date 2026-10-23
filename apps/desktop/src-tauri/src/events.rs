//! What Rust sends the overlay and toast windows: the event contract in
//! `docs/design/m1-drag-wheel.md` §C. The UI mirrors these types in
//! `ui/src/shared/events.ts`, and both sides test against the same golden
//! JSON in `ui/src/shared/fixtures/`.

use grape_core::{wheel_layout, EmptyState, Geometry, Platform, Point, Segment, WheelContent};
use grape_core::{WheelEntry, WheelMode, GEOMETRY};
use serde::Serialize;

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
    /// Whether clicking reveals the output in Explorer.
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
}
