//! The tool panel's model (`docs/INTERACTION_SPEC.md` §11, DECISIONS D31):
//! which drops open a panel, the request the page sees (ids and display
//! names, never paths), what a run may choose, and where the window goes
//! and how big it is. Pure: no Tauri, no OS calls, no disk.
//!
//! - One panel at a time. A new request replaces the open one, which
//!   counts as cancelled.
//! - The page answers by the panel's own `seq` and by item ids. A stale
//!   `seq` or a second run is ignored; an order that isn't every id once, a
//!   choice for another tool or a target off the grid is refused, and the
//!   panel stays open.
//! - An accepted run takes the panel and hands back the [`Operation`] with
//!   the chosen [`ToolParams`] and the items in the chosen order. For Merge
//!   that order is the page order, so the new first file names the PDF and
//!   picks its folder.
//!
//! [`PanelRequest`], [`Pending`] and [`Accepted`] hold paths: never format
//! one in a log line. [`describe`] is the only log text built from a
//! request.

use std::collections::HashSet;

use grape_core::{
    output_name, wheel_content, Channels, CompressPreset, Features, Operation, SelectionItem,
    ToolKey, ToolParams, WheelEntry, WheelMode, GEOMETRY,
};
use grape_engines::SidecarPaths;
use grape_platform::{DisplayBounds, Rect};

use crate::events::{PanelChoice, PanelItem, PanelOpen, PanelRun, PanelSpec};
use crate::float::display_name;
use crate::jobs::{self, NotRunnable};

// The window's layout, in logical pixels (points on macOS). The page lays
// out to fill the window, with the same numbers.
/// Outer width: the panel (340) plus [`INSET`] on both sides.
const WIDTH: f64 = 352.0;
/// Room for the shadow on every side.
const INSET: f64 = 6.0;
/// Title, count and ×.
const HEADER: f64 = 52.0;
/// Hint, Cancel and Run.
const FOOTER: f64 = 60.0;
/// Audio Channels' segmented control and note.
const CHOICE_BODY: f64 = 100.0;
/// Compress's segmented control and its note, which takes up to two lines
/// (Small's is wider than one in some fonts).
const COMPRESS_BODY: f64 = 118.0;
/// Normalize's slider, its scale and note.
const LOUDNESS_BODY: f64 = 110.0;
/// Merge's label, "Saves as" and the move buttons, without the rows.
const MERGE_BODY: f64 = 90.0;
/// One row of Merge's order list.
const ROW: f64 = 36.0;
/// Rows shown before the list scrolls.
const ROWS_SHOWN: usize = 6;
/// Space between the wheel's glass and the panel, as the toast's.
const SPACING: f64 = 16.0;

/// What the panels start from: the last Compress preset and Normalize
/// target that ran (`tools.compress`, `tools.normalize_lufs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolMemory {
    /// Compress's preset.
    pub compress: CompressPreset,
    /// Normalize's target, in LUFS.
    pub normalize_lufs: i8,
}

impl ToolMemory {
    /// Balanced and −16 LUFS: grape-core's `ToolParams::DEFAULT`.
    pub const DEFAULT: Self = Self {
        compress: ToolParams::DEFAULT.compress,
        normalize_lufs: ToolParams::DEFAULT.target_lufs,
    };
}

/// A choice a panel remembers when its Run is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    /// Compress ran with this preset.
    Compress(CompressPreset),
    /// Normalize ran with this target.
    NormalizeLufs(i8),
}

/// Where a drop on a tool or conversion goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Open this tool's panel first.
    Panel(ToolKey),
    /// Straight to `jobs::submit`, which runs it or says why it can't.
    Direct,
}

/// A v1 panel tool ([`ToolKey::has_v1_panel`]) opens its panel when it's
/// offered for these items under `features` (the wheel's rule, which
/// `grape_core::plan` checks again: Audio Channels on one file only, Merge
/// PDFs on two or more, nothing a format's tools leave out) and its
/// default-params operation passes `check`. Everything else goes the
/// direct way, where `jobs::submit` runs it or shows why it can't. So a
/// panel never opens for a run that would fail, whether this build can't
/// run the tool or the plan wouldn't offer it.
pub fn route(
    entry: WheelEntry,
    items: &[SelectionItem],
    features: Features,
    check: impl Fn(&Operation, &[SelectionItem]) -> Result<(), NotRunnable>,
) -> Route {
    match entry {
        WheelEntry::Tool(tool)
            if tool.has_v1_panel()
                && offered(tool, items, features)
                && check(&Operation::tool(tool), items).is_ok() =>
        {
            Route::Panel(tool)
        }
        _ => Route::Direct,
    }
}

/// Whether the Tools wheel for `items` under `features` has `tool`: the
/// formats' tools for this many items (`grape_core::tools`, so only the
/// multi-OK ones for several), shared by every format in the drop.
pub fn offered(tool: ToolKey, items: &[SelectionItem], features: Features) -> bool {
    wheel_content(items, WheelMode::Tools, features).contains(WheelEntry::Tool(tool))
}

/// The check `GRAPE_PREVIEW_PANELS=1` routes with: the four panel tools
/// pass, so their panels open where they're offered even when this build
/// can't run them, and everything else gets [`jobs::check_runnable`]. A
/// preview panel's Run still meets the real check in `jobs::submit_op`, so
/// nothing is written.
pub fn preview_check(
    op: &Operation,
    items: &[SelectionItem],
    sidecars: &SidecarPaths,
) -> Result<(), NotRunnable> {
    match op {
        Operation::Tool { tool, .. } if tool.has_v1_panel() => Ok(()),
        _ => jobs::check_runnable(op, items, sidecars),
    }
}

/// A drop waiting in a panel. Holds paths: never format it in a log line.
pub struct PanelRequest {
    /// The panel tool.
    pub tool: ToolKey,
    /// The dropped items, in drop order.
    pub items: Vec<SelectionItem>,
    /// The wheel's center, in global coordinates.
    pub anchor: (i32, i32),
    /// The display the wheel opened on.
    pub display: DisplayBounds,
    /// The wheel the drop came from, for the log.
    pub wheel_seq: u64,
}

/// The open panel. Holds paths: never format it in a log line.
pub struct Pending {
    /// The panel's own seq.
    pub seq: u64,
    /// What was dropped.
    pub request: PanelRequest,
    /// What the page shows.
    pub view: PanelOpen,
    /// Where the window goes, in global coordinates.
    pub frame: Rect,
    /// Whether the panel window has had the focus while this panel was
    /// open. From then on Esc is the page's, and the Esc poll leaves this
    /// panel alone ([`PanelModel::esc_target`]).
    pub focused_once: bool,
}

/// The panel requests: at most one open, and the last seq handed out.
#[derive(Default)]
pub struct PanelModel {
    last_seq: u64,
    pending: Option<Pending>,
    /// Whether the panel window has the focus now (its `Focused` events).
    focused: bool,
}

/// An accepted run: what to submit. Holds paths: never format it in a log
/// line.
pub struct Accepted {
    /// The panel that ran.
    pub seq: u64,
    /// The tool with the chosen params.
    pub op: Operation,
    /// The items, in the chosen order.
    pub items: Vec<SelectionItem>,
    /// The wheel's center, for the job's toast.
    pub anchor: (i32, i32),
    /// The display the wheel opened on.
    pub display: DisplayBounds,
    /// What the next panel starts from, if this tool remembers.
    pub remember: Option<ToolChoice>,
}

/// Why a `panel_run` wasn't accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// No panel is open (a second run, or one after a cancel or Pause).
    NoPanel,
    /// Another panel is open now.
    Stale {
        /// The open panel's seq.
        open: u64,
    },
    /// The order isn't each item id exactly once.
    BadOrder,
    /// The choice is for another tool.
    WrongTool,
    /// The Normalize target is off the range or the step.
    OutOfRange,
}

impl Rejected {
    /// `NoPanel` and `Stale` are ignored (`Ok` to the page); the rest are
    /// malformed.
    pub fn ignored(self) -> bool {
        matches!(self, Self::NoPanel | Self::Stale { .. })
    }

    /// For the log.
    pub fn why(self) -> &'static str {
        match self {
            Self::NoPanel => "no panel is open",
            Self::Stale { .. } => "another panel is open",
            Self::BadOrder => "the order isn't every item once",
            Self::WrongTool => "the choice is for another tool",
            Self::OutOfRange => "the target is out of range",
        }
    }
}

impl PanelModel {
    /// Opens a panel for `request`, replacing any open one. Returns the
    /// payload and the seq it replaced, or `None`, leaving the model as it
    /// was, when the request can't have a panel: a tool without one
    /// ([`ToolKey::has_v1_panel`]), no items, or Merge with fewer than two.
    pub fn open(
        &mut self,
        request: PanelRequest,
        memory: ToolMemory,
    ) -> Option<(PanelOpen, Option<u64>)> {
        if request.items.is_empty() {
            return None;
        }
        let names: Vec<String> = request
            .items
            .iter()
            .map(|item| display_name(&item.path))
            .collect();
        let panel = spec_for(request.tool, memory, &names)?;
        let seq = self.last_seq.checked_add(1)?;
        self.last_seq = seq;
        let view = PanelOpen {
            seq,
            title: request.tool.name().to_owned(),
            items: (1..)
                .zip(names)
                .map(|(id, name)| PanelItem { id, name })
                .collect(),
            panel,
        };
        let frame = place(
            request.anchor,
            &request.display,
            size(&view.panel, view.items.len()),
        );
        let replaced = self
            .pending
            .replace(Pending {
                seq,
                request,
                view: view.clone(),
                frame,
                // A replacement while the panel has the focus is in use
                // already.
                focused_once: self.focused,
            })
            .map(|old| old.seq);
        Some((view, replaced))
    }

    /// The open request, as the page sees it.
    pub fn current(&self) -> Option<&PanelOpen> {
        self.pending.as_ref().map(|pending| &pending.view)
    }

    /// The open panel.
    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    /// Validates `run` and, when it's accepted, takes the panel (a second
    /// run then gets `NoPanel`). A rejected run leaves the panel open.
    pub fn run(&mut self, run: &PanelRun) -> Result<Accepted, Rejected> {
        let pending = self.pending.as_ref().ok_or(Rejected::NoPanel)?;
        if run.seq != pending.seq {
            return Err(Rejected::Stale { open: pending.seq });
        }
        if !every_id_once(&run.order, pending.request.items.len()) {
            return Err(Rejected::BadOrder);
        }
        let (params, remember) = chosen(pending.request.tool, run.choice)?;
        let Some(pending) = self.pending.take() else {
            return Err(Rejected::NoPanel);
        };
        let request = pending.request;
        let mut slots: Vec<Option<SelectionItem>> = request.items.into_iter().map(Some).collect();
        let items = run
            .order
            .iter()
            .filter_map(|&id| {
                let index = usize::try_from(id.checked_sub(1)?).ok()?;
                slots.get_mut(index)?.take()
            })
            .collect();
        Ok(Accepted {
            seq: pending.seq,
            op: Operation::Tool {
                tool: request.tool,
                params,
            },
            items,
            anchor: request.anchor,
            display: request.display,
            remember,
        })
    }

    /// Closes panel `seq` if it's the open one.
    pub fn cancel(&mut self, seq: u64) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.seq == seq)
        {
            self.pending = None;
            true
        } else {
            false
        }
    }

    /// Closes whatever is open (Pause, Quit). Returns its seq.
    pub fn close_any(&mut self) -> Option<u64> {
        self.pending.take().map(|pending| pending.seq)
    }

    /// The panel window gained or lost the focus. Returns the open panel's
    /// seq when this is the first time it has the focus, for the log.
    pub fn focus_changed(&mut self, focused: bool) -> Option<u64> {
        self.focused = focused;
        let pending = self.pending.as_mut().filter(|_| focused)?;
        let first = !pending.focused_once;
        pending.focused_once = true;
        first.then_some(pending.seq)
    }

    /// The open panel that an Esc pressed anywhere may close (Windows' Esc
    /// poll): one whose window hasn't had the focus since it opened. Right
    /// after the drop Explorer keeps the keyboard, so the poll is the only
    /// way Esc reaches the panel. Once the panel has been clicked, its page
    /// takes Esc while it has the focus, and an Esc pressed in another app
    /// (cancelling a rename, closing a menu) is that app's: a glance at
    /// Explorer must not lose a merge order.
    #[cfg(any(target_os = "windows", test))]
    pub fn esc_target(&self) -> Option<u64> {
        self.pending
            .as_ref()
            .filter(|pending| !pending.focused_once)
            .map(|pending| pending.seq)
    }
}

/// How the Esc poll reads its samples (Windows' "grape-panel-esc"): which
/// one is an Esc press it may act on. A press is Esc up in the sample before
/// and down in this one. It doesn't count while a mouse button is down, or
/// was in the sample before: that Esc belongs to a drag (the next wheel's,
/// or the page's reorder), and OLE ends a drag on the Esc key-down, so the
/// user may let go of the button before the poll looks again.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscPoll {
    /// Esc was down in the last sample.
    esc: bool,
    /// A mouse button was down in the last sample.
    button: bool,
}

#[cfg(any(target_os = "windows", test))]
impl EscPoll {
    /// Starts from the poll's first sample, which is never a press: an Esc
    /// already down when the poll starts doesn't count until it's pressed
    /// again.
    pub fn new(esc: bool, button: bool) -> Self {
        Self { esc, button }
    }

    /// Takes the next sample and says whether it's a press to act on.
    pub fn sample(&mut self, esc: bool, button: bool) -> bool {
        let press = esc && !self.esc && !button && !self.button;
        *self = Self { esc, button };
        press
    }
}

/// The panel for `tool`, starting from `memory`. `None` for a tool without
/// one, and for Merge with fewer than two files.
fn spec_for(tool: ToolKey, memory: ToolMemory, names: &[String]) -> Option<PanelSpec> {
    match tool {
        ToolKey::Compress => Some(PanelSpec::Compress {
            preset: memory.compress,
        }),
        // Mono every time: the right choice depends on the file.
        ToolKey::AudioChannels => Some(PanelSpec::AudioChannels {
            channels: Channels::Mono,
        }),
        ToolKey::NormalizeAudio => {
            let target = if ToolParams::lufs_ok(memory.normalize_lufs) {
                memory.normalize_lufs
            } else {
                ToolMemory::DEFAULT.normalize_lufs
            };
            Some(PanelSpec::NormalizeAudio {
                target_lufs: target,
                min_lufs: ToolParams::LUFS_MIN,
                max_lufs: ToolParams::LUFS_MAX,
                step_lufs: ToolParams::LUFS_STEP,
            })
        }
        ToolKey::MergePdf if names.len() >= 2 => {
            let op = Operation::tool(ToolKey::MergePdf);
            let none = HashSet::new();
            Some(PanelSpec::MergePdf {
                merged_names: names
                    .iter()
                    .map(|name| output_name(name, &op, &none))
                    .collect(),
            })
        }
        _ => None,
    }
}

/// True when `order` is each of `1..=count` exactly once.
fn every_id_once(order: &[u64], count: usize) -> bool {
    if order.len() != count {
        return false;
    }
    let mut seen = vec![false; count];
    order.iter().all(|&id| {
        let slot = id
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| seen.get_mut(index));
        match slot {
            Some(seen) if !*seen => {
                *seen = true;
                true
            }
            _ => false,
        }
    })
}

/// The params for `choice` on `tool`: the defaults with only the chosen
/// field set, and what to remember.
fn chosen(
    tool: ToolKey,
    choice: PanelChoice,
) -> Result<(ToolParams, Option<ToolChoice>), Rejected> {
    let mut params = ToolParams::DEFAULT;
    let remember = match (tool, choice) {
        (ToolKey::Compress, PanelChoice::Compress { preset }) => {
            params.compress = preset;
            Some(ToolChoice::Compress(preset))
        }
        (ToolKey::AudioChannels, PanelChoice::AudioChannels { channels }) => {
            params.channels = channels;
            None
        }
        (ToolKey::NormalizeAudio, PanelChoice::NormalizeAudio { target_lufs }) => {
            if !ToolParams::lufs_ok(target_lufs) {
                return Err(Rejected::OutOfRange);
            }
            params.target_lufs = target_lufs;
            Some(ToolChoice::NormalizeLufs(target_lufs))
        }
        (ToolKey::MergePdf, PanelChoice::MergePdf) => None,
        _ => return Err(Rejected::WrongTool),
    };
    Ok((params, remember))
}

/// The window's size in logical pixels for `spec` with `count` items:
/// 352 wide, and the inset, header, body and footer tall.
pub fn size(spec: &PanelSpec, count: usize) -> (f64, f64) {
    let body = match spec {
        PanelSpec::Compress { .. } => COMPRESS_BODY,
        PanelSpec::AudioChannels { .. } => CHOICE_BODY,
        PanelSpec::NormalizeAudio { .. } => LOUDNESS_BODY,
        PanelSpec::MergePdf { .. } => MERGE_BODY + count.min(ROWS_SHOWN) as f64 * ROW,
    };
    (WIDTH, 2.0 * INSET + HEADER + body + FOOTER)
}

/// Where the panel goes: beside the wheel's spot, so the drop point and
/// the files around it stay uncovered, vertically centered on it. Right of
/// the wheel's glass, or left of it when there's no room on the right, or
/// over it when the screen is too narrow for either; moved, never resized,
/// to lie inside the work area. Global coordinates: physical pixels on
/// Windows, points on macOS (scale 1.0).
pub fn place(anchor: (i32, i32), display: &DisplayBounds, size: (f64, f64)) -> Rect {
    let scale = display.scale;
    let (width, height) = (px(size.0, scale), px(size.1, scale));
    let reach = px(GEOMETRY.backdrop_r + SPACING, scale);
    let work = display.work_area;
    let y = anchor.1 - height / 2;
    let right = anchor.0 + reach;
    let left = anchor.0 - reach - width;
    let x = if right + width <= work.x + work.width {
        right
    } else if left >= work.x {
        left
    } else {
        anchor.0 - width / 2
    };
    inside(
        Rect {
            x,
            y,
            width,
            height,
        },
        work,
    )
}

/// "panel 4 (wheel 12): Merge PDFs, 3 item(s)". The only log text built
/// from a request: counts, seqs and the tool, never a name or a path.
pub fn describe(view: &PanelOpen, wheel_seq: u64) -> String {
    format!(
        "panel {} (wheel {wheel_seq}): {}, {} item(s)",
        view.seq,
        view.title,
        view.items.len()
    )
}

/// `logical` in physical pixels at `scale` (a bad scale counts as 1).
fn px(logical: f64, scale: f64) -> i32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (logical * scale).round() as i32
}

/// `rect` moved, not resized, to lie inside `work`. When it's bigger than
/// `work`, its top-left corner stays inside.
fn inside(rect: Rect, work: Rect) -> Rect {
    Rect {
        x: rect
            .x
            .clamp(work.x, (work.x + work.width - rect.width).max(work.x)),
        y: rect
            .y
            .clamp(work.y, (work.y + work.height - rect.height).max(work.y)),
        ..rect
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use grape_core::{plan, DirSnapshot, Format, Selection, Settings};
    use serde_json::Value;

    use super::*;

    /// In `ToolKey::ALL`'s order.
    const ALL_FOUR: [ToolKey; 4] = [
        ToolKey::Compress,
        ToolKey::NormalizeAudio,
        ToolKey::AudioChannels,
        ToolKey::MergePdf,
    ];

    fn golden(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn files(paths: &[&str]) -> Vec<SelectionItem> {
        paths
            .iter()
            .map(|path| SelectionItem::file(*path))
            .collect()
    }

    fn display(scale: f64) -> DisplayBounds {
        DisplayBounds {
            monitor: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            work_area: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1040,
            },
            scale,
        }
    }

    fn request(tool: ToolKey, paths: &[&str]) -> PanelRequest {
        PanelRequest {
            tool,
            items: files(paths),
            anchor: (960, 520),
            display: display(1.0),
            wheel_seq: 12,
        }
    }

    /// The fixtures' four requests, in fixture order (seq 1 to 4).
    fn fixture_requests() -> [PanelRequest; 4] {
        [
            request(ToolKey::Compress, &["/pics/photo.jpg", "/pics/beach.png"]),
            request(ToolKey::AudioChannels, &["/rec/interview.wav"]),
            request(
                ToolKey::NormalizeAudio,
                &["/pod/episode 12.mp3", "/pod/intro.m4a"],
            ),
            request(
                ToolKey::MergePdf,
                &["/docs/report.pdf", "/docs/cover.pdf", "/docs/appendix.pdf"],
            ),
        ]
    }

    /// A model whose open panel is fixture request `seq` (1 to 4), opened
    /// after the ones before it.
    fn opened(seq: usize) -> PanelModel {
        let mut model = PanelModel::default();
        for request in fixture_requests().into_iter().take(seq) {
            model.open(request, ToolMemory::DEFAULT).unwrap();
        }
        model
    }

    fn run_fixture(text: &str) -> PanelRun {
        serde_json::from_str(text).unwrap()
    }

    fn accepted(model: &mut PanelModel, run: &PanelRun) -> Accepted {
        match model.run(run) {
            Ok(accepted) => accepted,
            Err(rejected) => panic!("rejected: {rejected:?}"),
        }
    }

    fn rejected(model: &mut PanelModel, run: &PanelRun) -> Rejected {
        match model.run(run) {
            Ok(accepted) => panic!("accepted panel {}", accepted.seq),
            Err(rejected) => rejected,
        }
    }

    fn params(op: Operation) -> ToolParams {
        match op {
            Operation::Tool { params, .. } => params,
            Operation::Convert { .. } => panic!("not a tool"),
        }
    }

    fn run(seq: u64, order: &[u64], choice: PanelChoice) -> PanelRun {
        PanelRun {
            seq,
            order: order.to_vec(),
            choice,
        }
    }

    /// The features Grape plans with (`settings::planning_features`).
    const V1: Features = Features::V1_ONLY;

    /// For each of the four, a drop the wheel offers it for: the fixtures'.
    fn offered_drop(tool: ToolKey) -> Vec<SelectionItem> {
        match tool {
            ToolKey::Compress => files(&["/pics/photo.jpg", "/pics/beach.png"]),
            ToolKey::AudioChannels => files(&["/rec/interview.wav"]),
            ToolKey::NormalizeAudio => files(&["/pod/episode 12.mp3", "/pod/intro.m4a"]),
            _ => files(&["/docs/report.pdf", "/docs/cover.pdf"]),
        }
    }

    fn not_runnable(_: &Operation, _: &[SelectionItem]) -> Result<(), NotRunnable> {
        Err(NotRunnable {
            title: "Not in this build yet".to_owned(),
            detail: None,
        })
    }

    #[test]
    fn exactly_the_four_v1_tools_have_a_panel() {
        let with: Vec<ToolKey> = ToolKey::ALL
            .into_iter()
            .filter(|tool| tool.has_v1_panel())
            .collect();
        assert_eq!(with, ALL_FOUR);
        // grape-core says it opens one; v1 runs it at once.
        assert!(ToolKey::RemoveMetadata.opens_panel());
        assert!(!ToolKey::RemoveMetadata.has_v1_panel());
        // No other tool gets a panel, even when asked for one.
        let mut model = PanelModel::default();
        for tool in ToolKey::ALL {
            let opened = model
                .open(
                    request(tool, &["/a/one.pdf", "/a/two.pdf"]),
                    ToolMemory::DEFAULT,
                )
                .is_some();
            assert_eq!(opened, tool.has_v1_panel(), "{tool:?}");
        }
    }

    #[test]
    fn a_runnable_panel_tool_routes_to_its_panel_and_the_rest_go_direct() {
        let anything = |_: &Operation, _: &[SelectionItem]| Ok(());
        for tool in ALL_FOUR {
            let items = offered_drop(tool);
            assert_eq!(
                route(WheelEntry::Tool(tool), &items, V1, anything),
                Route::Panel(tool)
            );
            // One this build can't run goes the direct way, to its toast.
            assert_eq!(
                route(WheelEntry::Tool(tool), &items, V1, not_runnable),
                Route::Direct
            );
        }
        let items = files(&["/a/one.mp4", "/a/two.mp4"]);
        for entry in [
            WheelEntry::Tool(ToolKey::RemoveMetadata),
            WheelEntry::Tool(ToolKey::MuteVideo),
            WheelEntry::Convert(Format::Mov),
        ] {
            assert_eq!(
                route(entry, &items, V1, anything),
                Route::Direct,
                "{entry:?}"
            );
        }
    }

    #[test]
    fn a_panel_opens_only_where_the_wheel_offers_its_tool() {
        let anything = |_: &Operation, _: &[SelectionItem]| Ok(());
        let preview = |op: &Operation, items: &[SelectionItem]| {
            preview_check(op, items, &SidecarPaths::default())
        };
        let not_offered = [
            // Audio Channels isn't multi-OK: one file at a time.
            (ToolKey::AudioChannels, files(&["/a/one.wav", "/a/two.wav"])),
            // Merge PDFs takes two or more.
            (ToolKey::MergePdf, files(&["/a/only.pdf"])),
            // Compress on a PDF is v2, so not beside a JPG either.
            (ToolKey::Compress, files(&["/a/doc.pdf"])),
            (ToolKey::Compress, files(&["/a/photo.jpg", "/a/doc.pdf"])),
            // Normalize is for audio, and every file must take it.
            (ToolKey::NormalizeAudio, files(&["/a/clip.mp4"])),
            (
                ToolKey::NormalizeAudio,
                files(&["/a/song.mp3", "/a/clip.mp4"]),
            ),
            // A folder, a file of no known format, or nothing.
            (ToolKey::Compress, vec![SelectionItem::folder("/a/pics")]),
            (ToolKey::Compress, files(&["/a/notes.xyz"])),
            (ToolKey::Compress, Vec::new()),
        ];
        for (tool, items) in &not_offered {
            let entry = WheelEntry::Tool(*tool);
            assert!(!offered(*tool, items, V1), "{tool:?}");
            assert_eq!(route(entry, items, V1, anything), Route::Direct, "{tool:?}");
            assert_eq!(route(entry, items, V1, preview), Route::Direct, "{tool:?}");
        }
        // The same drops where the wheel does offer the tool.
        for (tool, items) in [
            (ToolKey::AudioChannels, files(&["/a/one.wav"])),
            (ToolKey::MergePdf, files(&["/a/one.pdf", "/a/two.pdf"])),
            (ToolKey::Compress, files(&["/a/photo.jpg", "/a/beach.png"])),
            (
                ToolKey::NormalizeAudio,
                files(&["/a/song.mp3", "/a/b.flac"]),
            ),
        ] {
            assert!(offered(tool, &items, V1), "{tool:?}");
        }
        // Compress on a PDF is offered once v2 is.
        assert!(offered(
            ToolKey::Compress,
            &files(&["/a/doc.pdf"]),
            Features::FULL
        ));
    }

    /// What Grape finds in a full install: FFmpeg and ffprobe, PDFium, the
    /// worker and bsdtar.
    fn installed() -> SidecarPaths {
        SidecarPaths {
            ffmpeg: Some(PathBuf::from("ffmpeg.exe")),
            ffprobe: Some(PathBuf::from("ffprobe.exe")),
            pdfium: Some(PathBuf::from("pdfium.dll")),
            pdf_worker: Some(PathBuf::from("Grape.exe")),
            bsdtar: Some(PathBuf::from("bsdtar.exe")),
            ..SidecarPaths::default()
        }
    }

    /// The four open their panels in a normal build wherever lane B's
    /// tools (D26) and the PDF engine run them, and nowhere else.
    #[test]
    fn this_build_opens_the_four_panels_where_they_run() {
        let real =
            |op: &Operation, items: &[SelectionItem]| jobs::check_runnable(op, items, &installed());
        let bare = |op: &Operation, items: &[SelectionItem]| {
            jobs::check_runnable(op, items, &SidecarPaths::default())
        };
        // Compress on JPG and PNG needs no helper.
        let images = files(&["/a/photo.jpg", "/a/beach.png"]);
        assert_eq!(
            route(WheelEntry::Tool(ToolKey::Compress), &images, V1, bare),
            Route::Panel(ToolKey::Compress)
        );
        // The media ones need FFmpeg; Merge PDFs needs PDFium.
        for (tool, items) in [
            (ToolKey::Compress, files(&["/a/one.mp3", "/a/two.flac"])),
            (ToolKey::Compress, files(&["/a/song.m4a", "/a/take.wav"])),
            (
                ToolKey::Compress,
                files(&["/a/clip.mp4", "/a/clip.mov", "/a/clip.mkv"]),
            ),
            (
                ToolKey::NormalizeAudio,
                files(&["/a/1.mp3", "/a/2.m4a", "/a/3.wav", "/a/4.flac"]),
            ),
            (ToolKey::AudioChannels, files(&["/a/interview.wav"])),
            (ToolKey::AudioChannels, files(&["/a/song.m4a"])),
            (ToolKey::MergePdf, files(&["/a/one.pdf", "/a/two.pdf"])),
        ] {
            assert_eq!(
                route(WheelEntry::Tool(tool), &items, V1, real),
                Route::Panel(tool),
                "{tool:?}"
            );
            // Without them the drop goes the direct way, to its toast.
            assert_eq!(
                route(WheelEntry::Tool(tool), &items, V1, bare),
                Route::Direct,
                "{tool:?}"
            );
        }
        // Formats still coming (D26's later slices): no panel.
        for items in [
            files(&["/a/scan.tiff"]),
            files(&["/a/photo.webp", "/a/other.webp"]),
            files(&["/a/IMG_0001.heic"]),
            files(&["/a/photo.jpg", "/a/scan.tiff"]),
        ] {
            assert_eq!(
                route(WheelEntry::Tool(ToolKey::Compress), &items, V1, real),
                Route::Direct
            );
        }
    }

    #[test]
    fn the_preview_opens_the_four_panels_where_offered_and_nothing_else() {
        let preview = |op: &Operation, items: &[SelectionItem]| {
            preview_check(op, items, &SidecarPaths::default())
        };
        // Wherever they're offered, whether this build runs them or not:
        // Compress on TIFF (which runs from lane B's D28 on).
        let tiffs = files(&["/a/one.tiff", "/a/two.tiff"]);
        assert_eq!(
            route(WheelEntry::Tool(ToolKey::Compress), &tiffs, V1, preview),
            Route::Panel(ToolKey::Compress)
        );
        // Or where a helper is missing.
        for tool in ALL_FOUR {
            assert_eq!(
                route(WheelEntry::Tool(tool), &offered_drop(tool), V1, preview),
                Route::Panel(tool)
            );
        }
        // Never where the wheel wouldn't offer them, nor for the others.
        for entry in [
            WheelEntry::Tool(ToolKey::NormalizeAudio),
            WheelEntry::Tool(ToolKey::AudioChannels),
            WheelEntry::Tool(ToolKey::MergePdf),
            WheelEntry::Tool(ToolKey::RemoveMetadata),
            WheelEntry::Tool(ToolKey::MuteVideo),
            WheelEntry::Tool(ToolKey::SplitPdf),
            WheelEntry::Convert(Format::Jpg),
        ] {
            assert_eq!(
                route(entry, &tiffs, V1, preview),
                Route::Direct,
                "{entry:?}"
            );
        }
        // The real check still answers for everything but the four.
        let items = files(&["/a/one.png", "/a/two.png"]);
        let to_jpg = Operation::convert(Format::Jpg);
        assert_eq!(preview(&to_jpg, &items), Ok(()));
        assert!(preview(&Operation::tool(ToolKey::MuteVideo), &items).is_err());
    }

    #[test]
    fn the_normalize_range_is_grape_cores() {
        assert_eq!(
            (
                ToolParams::LUFS_MIN,
                ToolParams::LUFS_MAX,
                ToolParams::LUFS_STEP
            ),
            (-24, -10, 1)
        );
        let mut model = PanelModel::default();
        let (view, _) = model
            .open(
                request(ToolKey::NormalizeAudio, &["/a/b.mp3"]),
                ToolMemory::DEFAULT,
            )
            .unwrap();
        assert_eq!(
            view.panel,
            PanelSpec::NormalizeAudio {
                target_lufs: -16,
                min_lufs: ToolParams::LUFS_MIN,
                max_lufs: ToolParams::LUFS_MAX,
                step_lufs: ToolParams::LUFS_STEP,
            }
        );
        assert_eq!(
            ToolMemory::DEFAULT,
            ToolMemory {
                compress: CompressPreset::Balanced,
                normalize_lufs: -16
            }
        );
        assert!(ToolParams::lufs_ok(ToolMemory::DEFAULT.normalize_lufs));
    }

    #[test]
    fn opens_build_the_fixtures_in_order_and_replace_each_other() {
        let fixtures = [
            include_str!("../../../ui/src/shared/fixtures/panel-open-compress.json"),
            include_str!("../../../ui/src/shared/fixtures/panel-open-channels.json"),
            include_str!("../../../ui/src/shared/fixtures/panel-open-normalize.json"),
            include_str!("../../../ui/src/shared/fixtures/panel-open-merge.json"),
        ];
        let mut model = PanelModel::default();
        let mut previous = None;
        for (request, fixture) in fixture_requests().into_iter().zip(fixtures) {
            let (view, replaced) = model.open(request, ToolMemory::DEFAULT).unwrap();
            assert_eq!(serde_json::to_value(&view).unwrap(), golden(fixture));
            assert_eq!(replaced, previous);
            assert_eq!(model.current(), Some(&view));
            previous = Some(view.seq);
        }
        assert_eq!(previous, Some(4));
    }

    #[test]
    fn panels_start_from_the_memory_except_channels() {
        let memory = ToolMemory {
            compress: CompressPreset::High,
            normalize_lufs: -23,
        };
        let mut model = PanelModel::default();
        let (view, _) = model
            .open(request(ToolKey::Compress, &["/a/b.jpg"]), memory)
            .unwrap();
        assert_eq!(
            view.panel,
            PanelSpec::Compress {
                preset: CompressPreset::High
            }
        );
        let (view, _) = model
            .open(request(ToolKey::NormalizeAudio, &["/a/b.mp3"]), memory)
            .unwrap();
        assert_eq!(
            view.panel,
            PanelSpec::NormalizeAudio {
                target_lufs: -23,
                min_lufs: -24,
                max_lufs: -10,
                step_lufs: 1
            }
        );
        let (view, _) = model
            .open(request(ToolKey::AudioChannels, &["/a/b.wav"]), memory)
            .unwrap();
        assert_eq!(
            view.panel,
            PanelSpec::AudioChannels {
                channels: Channels::Mono
            }
        );
        // A target off the grid never reaches the page.
        let odd = ToolMemory {
            normalize_lufs: -40,
            ..memory
        };
        let (view, _) = model
            .open(request(ToolKey::NormalizeAudio, &["/a/b.mp3"]), odd)
            .unwrap();
        assert!(matches!(
            view.panel,
            PanelSpec::NormalizeAudio {
                target_lufs: -16,
                ..
            }
        ));
    }

    #[test]
    fn a_request_without_a_panel_changes_nothing() {
        let mut model = opened(1);
        assert!(model
            .open(
                request(ToolKey::RemoveMetadata, &["/a/b.jpg"]),
                ToolMemory::DEFAULT
            )
            .is_none());
        assert!(model
            .open(
                request(ToolKey::MergePdf, &["/a/only.pdf"]),
                ToolMemory::DEFAULT
            )
            .is_none());
        assert!(model
            .open(request(ToolKey::Compress, &[]), ToolMemory::DEFAULT)
            .is_none());
        assert_eq!(model.current().map(|view| view.seq), Some(1));
        // The next panel still gets the next seq.
        let (view, replaced) = model
            .open(
                request(ToolKey::Compress, &["/a/b.jpg"]),
                ToolMemory::DEFAULT,
            )
            .unwrap();
        assert_eq!((view.seq, replaced), (2, Some(1)));
    }

    #[test]
    fn display_names_never_carry_the_folder() {
        let mut model = PanelModel::default();
        let (view, _) = model
            .open(
                request(
                    ToolKey::Compress,
                    &["/a/b/photo.jpg", "/a/Invoices/", "/deep/er/still/x.png"],
                ),
                ToolMemory::DEFAULT,
            )
            .unwrap();
        let names: Vec<&str> = view.items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["photo.jpg", "Invoices", "x.png"]);
        for name in names {
            assert!(!name.contains('/') && !name.contains('\\'), "{name}");
        }
        let ids: Vec<u64> = view.items.iter().map(|item| item.id).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[cfg(windows)]
    #[test]
    fn a_root_is_named_by_its_last_part() {
        assert_eq!(display_name(Path::new(r"D:\")), "D:");
    }

    #[cfg(not(windows))]
    #[test]
    fn a_root_is_named_by_its_last_part() {
        // "/" has no part but the root itself.
        assert_eq!(display_name(Path::new("/")), "/");
    }

    #[test]
    fn compress_runs_with_the_chosen_preset_and_remembers_it() {
        let mut model = opened(1);
        let fixture = run_fixture(include_str!(
            "../../../ui/src/shared/fixtures/panel-run-compress.json"
        ));
        let accepted = accepted(&mut model, &fixture);
        assert_eq!(accepted.seq, 1);
        assert_eq!(
            params(accepted.op),
            ToolParams {
                compress: CompressPreset::Small,
                ..ToolParams::DEFAULT
            }
        );
        assert_eq!(
            accepted.remember,
            Some(ToolChoice::Compress(CompressPreset::Small))
        );
        assert_eq!(
            accepted.items,
            files(&["/pics/photo.jpg", "/pics/beach.png"])
        );
        assert_eq!(accepted.anchor, (960, 520));
        assert_eq!(accepted.display, display(1.0));
        assert!(model.current().is_none());
    }

    #[test]
    fn channels_run_with_the_chosen_layout_and_name_the_output_after_it() {
        let mut model = opened(2);
        let fixture = run_fixture(include_str!(
            "../../../ui/src/shared/fixtures/panel-run-channels.json"
        ));
        let accepted = accepted(&mut model, &fixture);
        assert_eq!(
            params(accepted.op),
            ToolParams {
                channels: Channels::Stereo,
                ..ToolParams::DEFAULT
            }
        );
        assert_eq!(accepted.remember, None);
        assert_eq!(
            output_name("interview.wav", &accepted.op, &HashSet::new()),
            "interview Stereo.wav"
        );
    }

    #[test]
    fn normalize_runs_with_the_chosen_target_and_remembers_it() {
        let mut model = opened(3);
        let fixture = run_fixture(include_str!(
            "../../../ui/src/shared/fixtures/panel-run-normalize.json"
        ));
        let accepted = accepted(&mut model, &fixture);
        assert_eq!(
            params(accepted.op),
            ToolParams {
                target_lufs: -14,
                ..ToolParams::DEFAULT
            }
        );
        assert_eq!(accepted.remember, Some(ToolChoice::NormalizeLufs(-14)));
    }

    #[test]
    fn merge_runs_in_the_chosen_order_and_the_new_first_file_names_it() {
        let mut model = opened(4);
        let fixture = run_fixture(include_str!(
            "../../../ui/src/shared/fixtures/panel-run-merge.json"
        ));
        let accepted = accepted(&mut model, &fixture);
        assert_eq!(params(accepted.op), ToolParams::DEFAULT);
        assert_eq!(accepted.remember, None);
        assert_eq!(
            accepted.items,
            files(&["/docs/cover.pdf", "/docs/report.pdf", "/docs/appendix.pdf"])
        );
        let docs = DirSnapshot::new(["report.pdf", "cover.pdf", "appendix.pdf"], true);
        let selection = Selection::new(accepted.items).with_dir("/docs", docs);
        let planned = plan(&selection, &accepted.op, &Settings::new("/fallback")).unwrap();
        let [job] = planned.jobs.as_slice() else {
            panic!("{} jobs", planned.jobs.len());
        };
        assert_eq!(
            job.output.display_path().file_name(),
            Some(std::ffi::OsStr::new("cover Merged.pdf"))
        );
        assert_eq!(
            job.inputs,
            ["/docs/cover.pdf", "/docs/report.pdf", "/docs/appendix.pdf"].map(PathBuf::from)
        );
    }

    #[test]
    fn a_stale_seq_is_ignored_and_the_panel_stays_open() {
        let mut model = opened(2);
        let compress = run(
            1,
            &[1, 2],
            PanelChoice::Compress {
                preset: CompressPreset::Small,
            },
        );
        let why = rejected(&mut model, &compress);
        assert_eq!(why, Rejected::Stale { open: 2 });
        assert!(why.ignored());
        assert_eq!(model.current().map(|view| view.seq), Some(2));
    }

    #[test]
    fn a_second_run_and_a_run_after_cancel_find_no_panel() {
        let mut model = opened(1);
        let fixture = run_fixture(include_str!(
            "../../../ui/src/shared/fixtures/panel-run-compress.json"
        ));
        accepted(&mut model, &fixture);
        assert_eq!(rejected(&mut model, &fixture), Rejected::NoPanel);

        let mut model = opened(1);
        assert!(model.cancel(1));
        assert_eq!(rejected(&mut model, &fixture), Rejected::NoPanel);
    }

    #[test]
    fn an_order_that_isnt_every_id_once_is_refused_and_the_panel_stays() {
        let mut model = opened(1);
        let small = PanelChoice::Compress {
            preset: CompressPreset::Small,
        };
        let orders: [&[u64]; 7] = [
            &[],
            &[1],
            &[1, 1],
            &[0, 1],
            &[1, 3],
            &[1, 2, 3],
            &[2, u64::MAX],
        ];
        for order in orders {
            let why = rejected(&mut model, &run(1, order, small));
            assert_eq!(why, Rejected::BadOrder, "{order:?}");
            assert!(!why.ignored());
            assert_eq!(model.current().map(|view| view.seq), Some(1));
        }
        let accepted = accepted(&mut model, &run(1, &[2, 1], small));
        assert_eq!(
            accepted.items,
            files(&["/pics/beach.png", "/pics/photo.jpg"])
        );
    }

    #[test]
    fn a_choice_for_another_tool_is_refused() {
        let mut model = opened(1);
        for choice in [
            PanelChoice::MergePdf,
            PanelChoice::AudioChannels {
                channels: Channels::Mono,
            },
            PanelChoice::NormalizeAudio { target_lufs: -16 },
        ] {
            let why = rejected(&mut model, &run(1, &[1, 2], choice));
            assert_eq!(why, Rejected::WrongTool);
            assert!(!why.ignored());
        }
        assert!(model.current().is_some());
    }

    #[test]
    fn a_target_off_the_range_is_refused_and_the_ends_are_taken() {
        for target in [-25, -9, i8::MIN, i8::MAX] {
            let mut model = opened(3);
            let choice = PanelChoice::NormalizeAudio {
                target_lufs: target,
            };
            let why = rejected(&mut model, &run(3, &[1, 2], choice));
            assert_eq!(why, Rejected::OutOfRange, "{target}");
            assert!(!why.ignored());
            assert!(model.current().is_some());
        }
        for target in [-24, -10] {
            let mut model = opened(3);
            let choice = PanelChoice::NormalizeAudio {
                target_lufs: target,
            };
            let accepted = accepted(&mut model, &run(3, &[1, 2], choice));
            assert_eq!(params(accepted.op).target_lufs, target);
        }
    }

    #[test]
    fn only_no_panel_and_stale_are_ignored() {
        let ignored: BTreeSet<&str> = [
            Rejected::NoPanel,
            Rejected::Stale { open: 3 },
            Rejected::BadOrder,
            Rejected::WrongTool,
            Rejected::OutOfRange,
        ]
        .into_iter()
        .filter(|why| why.ignored())
        .map(Rejected::why)
        .collect();
        assert_eq!(
            ignored,
            BTreeSet::from(["no panel is open", "another panel is open"])
        );
    }

    #[test]
    fn cancel_closes_only_the_open_seq_and_close_any_closes_it_once() {
        let mut model = opened(2);
        assert!(!model.cancel(1));
        assert!(!model.cancel(3));
        assert!(model.current().is_some());
        assert!(model.cancel(2));
        assert!(!model.cancel(2));
        assert!(model.pending().is_none());

        let mut model = opened(3);
        assert_eq!(model.close_any(), Some(3));
        assert_eq!(model.close_any(), None);
    }

    #[test]
    fn the_esc_poll_closes_a_panel_only_until_its_window_has_had_the_focus() {
        let mut model = opened(1);
        // Right after the drop Explorer has the keyboard: the poll's Esc.
        assert_eq!(model.esc_target(), Some(1));
        // The first click: from now on Esc is the page's.
        assert_eq!(model.focus_changed(true), Some(1));
        assert_eq!(model.esc_target(), None);
        // A click on Explorer, then Esc there (a rename, a menu): the panel
        // and its order stay.
        assert_eq!(model.focus_changed(false), None);
        assert_eq!(model.esc_target(), None);
        assert_eq!(model.focus_changed(true), None);
        assert_eq!(model.esc_target(), None);
        assert!(model.current().is_some());
    }

    #[test]
    fn a_new_panel_starts_with_the_poll_unless_the_window_has_the_focus() {
        let mut model = opened(1);
        model.focus_changed(true);
        // Replaced while the panel itself has the keyboard: in use already.
        let [_, channels, normalize, _] = fixture_requests();
        model.open(channels, ToolMemory::DEFAULT).unwrap();
        assert_eq!(model.esc_target(), None);
        assert_eq!(model.focus_changed(true), None);
        // Replaced after the user went back to Explorer for the next drop.
        model.focus_changed(false);
        model.open(normalize, ToolMemory::DEFAULT).unwrap();
        assert_eq!(model.esc_target(), Some(3));
        // Focus with no panel open marks nothing.
        assert!(model.cancel(3));
        assert_eq!(model.focus_changed(true), None);
        assert_eq!(model.esc_target(), None);
    }

    #[test]
    fn an_esc_press_counts_once_and_never_near_a_mouse_button() {
        // A plain press, held for a few samples, then pressed again.
        let mut poll = EscPoll::new(false, false);
        assert!(poll.sample(true, false));
        assert!(!poll.sample(true, false));
        assert!(!poll.sample(false, false));
        assert!(poll.sample(true, false));

        // Already down when the poll starts: not a press until pressed again.
        let mut poll = EscPoll::new(true, false);
        assert!(!poll.sample(true, false));
        assert!(!poll.sample(false, false));
        assert!(poll.sample(true, false));

        // During a drag: the button is down.
        let mut poll = EscPoll::new(false, true);
        assert!(!poll.sample(true, true));
        assert!(!poll.sample(true, false));

        // OLE cancelled the drag on the key-down and the user let go before
        // the next sample: the button was down in the sample before.
        let mut poll = EscPoll::new(false, true);
        assert!(!poll.sample(true, false));
        assert!(!poll.sample(false, false));
        // Once the button has been up for a whole sample, Esc counts again.
        assert!(poll.sample(true, false));
    }

    #[test]
    fn the_size_follows_the_tool_and_the_rows() {
        let compress = PanelSpec::Compress {
            preset: CompressPreset::Balanced,
        };
        let channels = PanelSpec::AudioChannels {
            channels: Channels::Mono,
        };
        let normalize = PanelSpec::NormalizeAudio {
            target_lufs: -16,
            min_lufs: -24,
            max_lufs: -10,
            step_lufs: 1,
        };
        let merge = PanelSpec::MergePdf {
            merged_names: vec![],
        };
        assert_eq!(size(&compress, 2), (352.0, 242.0));
        assert_eq!(size(&channels, 1), (352.0, 224.0));
        assert_eq!(size(&normalize, 2), (352.0, 234.0));
        assert_eq!(size(&merge, 2), (352.0, 286.0));
        assert_eq!(size(&merge, 3), (352.0, 322.0));
        assert_eq!(size(&merge, 6), (352.0, 430.0));
        assert_eq!(size(&merge, 50), (352.0, 430.0));
    }

    fn work(x: i32, y: i32, width: i32, height: i32) -> DisplayBounds {
        let area = Rect {
            x,
            y,
            width,
            height,
        };
        DisplayBounds {
            monitor: area,
            work_area: area,
            scale: 1.0,
        }
    }

    const SMALL: (f64, f64) = (352.0, 224.0);

    #[test]
    fn the_panel_sits_right_of_the_wheel_centered_on_it() {
        assert_eq!(
            place((960, 520), &display(1.0), SMALL),
            Rect {
                x: 1102,
                y: 408,
                width: 352,
                height: 224
            }
        );
    }

    #[test]
    fn near_the_right_edge_it_sits_left_of_the_wheel() {
        assert_eq!(place((1800, 520), &display(1.0), SMALL).x, 1306);
    }

    #[test]
    fn the_scale_sizes_and_spaces_it() {
        assert_eq!(
            place((960, 520), &display(1.5), SMALL),
            Rect {
                x: 1173,
                y: 352,
                width: 528,
                height: 336
            }
        );
    }

    #[test]
    fn it_stays_inside_the_work_area() {
        assert_eq!(place((960, 100), &display(1.0), SMALL).y, 0);
        assert_eq!(place((960, 1030), &display(1.0), SMALL).y, 1040 - 224);
    }

    #[test]
    fn a_work_area_that_doesnt_start_at_zero_bounds_the_left_side() {
        // A taskbar on the left: the work area starts at x 48. On the left
        // of the wheel there'd only be room from x 36, so it goes over it.
        let narrow = work(48, 0, 700, 1040);
        assert_eq!(place((530, 300), &narrow, SMALL).x, 530 - 176);
        assert_eq!(place((530, 300), &work(0, 0, 748, 1040), SMALL).x, 36);
        assert_eq!(place((560, 300), &narrow, SMALL).x, 66);
    }

    #[test]
    fn a_monitor_left_of_the_primary_works_in_negative_x() {
        let left = work(-1920, 0, 1920, 1040);
        assert_eq!(
            place((-960, 520), &left, SMALL),
            Rect {
                x: -818,
                y: 408,
                width: 352,
                height: 224
            }
        );
        assert_eq!(place((-100, 520), &left, SMALL).x, -100 - 142 - 352);
    }

    #[test]
    fn a_narrow_screen_puts_it_over_the_wheel_then_clamps_it() {
        let narrow = work(0, 0, 500, 1040);
        assert_eq!(place((250, 520), &narrow, SMALL).x, 250 - 176);
        assert_eq!(place((450, 520), &narrow, SMALL).x, 500 - 352);
    }

    #[test]
    fn a_bad_scale_counts_as_one() {
        let at_one = place((960, 520), &display(1.0), SMALL);
        for scale in [0.0, f64::NAN, -2.0, f64::INFINITY] {
            assert_eq!(place((960, 520), &display(scale), SMALL), at_one, "{scale}");
        }
    }

    #[test]
    fn describe_carries_counts_and_seqs_never_names() {
        let model = opened(4);
        let view = model.current().unwrap();
        let line = describe(view, 12);
        assert_eq!(line, "panel 4 (wheel 12): Merge PDFs, 3 item(s)");
        for item in &view.items {
            let stem = Path::new(&item.name).file_stem().unwrap().to_str().unwrap();
            assert!(!line.contains(stem), "{line} names {}", item.name);
        }
        assert!(!line.contains("Merged.pdf"));
    }
}
