//! The wheel controller: `docs/INTERACTION_SPEC.md` §1 as a pure state
//! machine.
//!
//! Inputs are drag signals from `grape-platform`'s monitor, events from the
//! overlay's drop target, and a few app events. Outputs are [`Effect`]s the
//! app applies: show or hide the overlay, send the `wheel://` events from
//! [`crate::events`], run a job, log. Time and the OS (display bounds, what
//! a path is) come in as arguments, so tests drive it with scripted signals
//! and no OS.
//!
//! The wheel opens as a neutral ring ([`UiContent::Pending`]) and fills in
//! with grape-core's [`wheel_content`] once the drop target's Enter says
//! which files are being dragged.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use grape_core::{
    clamp_anchor, hit_test, wheel_content, Features, Platform, Point, SelectionItem, WheelContent,
    WheelEntry, WheelMode, GEOMETRY,
};
use grape_platform::{DisplayBounds, DragSignal, DropEvent, Modifiers, Rect};

use crate::events::{CloseReason, KeyNames, UiContent, WheelClose, WheelHighlight, WheelOpen};

/// Hide the overlay if nothing happens for this long while it's open
/// (`docs/INTERACTION_SPEC.md` §9).
pub const WATCHDOG: Duration = Duration::from_secs(30);
/// Once the overlay is on screen, a drag that brings no file paths this
/// long after the next pointer move isn't a file drag: close the wheel
/// (`docs/PLATFORM_WINDOWS.md` → "Is it a file drag?"). The clock starts
/// only after the overlay reports it painted, so a slow first show can't
/// close the wheel before the drag ever reached it.
pub const NO_FILES_GRACE: Duration = Duration::from_millis(300);
/// After the button goes up over the overlay, how long to keep it up
/// waiting for its drop. The mouse hook sees the release before Explorer's
/// OLE loop delivers the drop to Grape's target, so hiding on the release
/// could let the drop fall through to the window underneath, maybe as a
/// Shift move.
pub const DROP_GRACE: Duration = Duration::from_millis(750);

/// A modifier key that can be assigned to a wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Shift.
    Shift,
    /// Control (Ctrl).
    Ctrl,
    /// Alt (Option on macOS).
    Alt,
    /// Command on macOS, the Windows key on Windows.
    Meta,
}

impl Key {
    fn is_held(self, held: Modifiers) -> bool {
        match self {
            Key::Shift => held.shift,
            Key::Ctrl => held.ctrl,
            Key::Alt => held.alt,
            Key::Meta => held.meta,
        }
    }

    /// The key's name as this OS's keyboards label it.
    pub const fn name(self) -> &'static str {
        let mac = cfg!(target_os = "macos");
        match self {
            Key::Shift => "Shift",
            Key::Ctrl if mac => "Control",
            Key::Ctrl => "Ctrl",
            Key::Alt if mac => "Option",
            Key::Alt => "Alt",
            Key::Meta if mac => "Command",
            Key::Meta => "Windows",
        }
    }
}

/// Which key opens which wheel (`docs/INTERACTION_SPEC.md` §2). Settings
/// make this remappable in M6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyMap {
    /// Opens the conversions wheel.
    pub conversions: Key,
    /// Opens the tools wheel.
    pub tools: Key,
    /// Opens the file actions wheel.
    pub actions: Key,
}

impl KeyMap {
    /// Windows defaults: Shift / Ctrl / Alt (DECISIONS D6).
    pub const WINDOWS: Self = Self {
        conversions: Key::Shift,
        tools: Key::Ctrl,
        actions: Key::Alt,
    };
    /// macOS defaults: Shift / Command / Control.
    pub const MACOS: Self = Self {
        conversions: Key::Shift,
        tools: Key::Meta,
        actions: Key::Ctrl,
    };

    /// The defaults for the OS Grape is running on.
    pub const fn for_this_os() -> Self {
        if cfg!(target_os = "macos") {
            Self::MACOS
        } else {
            Self::WINDOWS
        }
    }

    /// The wheel for this set of held keys: only when exactly one modifier
    /// is held and it's assigned to a wheel. Two or more held keys mean "no
    /// mode".
    pub fn mode_for(self, held: Modifiers) -> Option<WheelMode> {
        if held.count() != 1 {
            return None;
        }
        [
            (self.conversions, WheelMode::Conversions),
            (self.tools, WheelMode::Tools),
            (self.actions, WheelMode::Actions),
        ]
        .into_iter()
        .find_map(|(key, mode)| key.is_held(held).then_some(mode))
    }
}

/// What the controller needs from the OS.
pub trait Env {
    /// The display under the global physical point (`x`, `y`).
    fn display_at(&self, x: i32, y: i32) -> Option<DisplayBounds>;
    /// The dragged paths as selection items: folders as folders, everything
    /// else as files.
    fn classify(&self, paths: &[PathBuf]) -> Vec<SelectionItem>;
}

/// The settings the controller follows. Settings become editable in M6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerConfig {
    /// Which key opens which wheel.
    pub keys: KeyMap,
    /// Which tiers the wheels offer.
    pub features: Features,
    /// Titles differ per OS (Share vs AirDrop).
    pub platform: Platform,
    /// Whether the overlay plays its highlight tick.
    pub sounds: bool,
}

impl ControllerConfig {
    /// The defaults for the OS Grape is running on: its keys, v1 only,
    /// sounds on.
    pub const fn for_this_os() -> Self {
        Self {
            keys: KeyMap::for_this_os(),
            features: Features::V1_ONLY,
            platform: if cfg!(target_os = "macos") {
                Platform::MacOs
            } else {
                Platform::Windows
            },
            sounds: true,
        }
    }
}

/// Something that happened, for the controller.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// From the drag monitor.
    Signal(DragSignal),
    /// From the overlay's drop target.
    Drop(DropEvent),
    /// The overlay finished painting wheel `seq` (for the latency log).
    Painted {
        /// The `seq` of the `wheel://open` that was painted.
        seq: u64,
    },
    /// Tray → Pause Grape was toggled.
    SetPaused(bool),
}

/// A drop on a segment: what to run, on what.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRequest {
    /// The wheel it came from.
    pub seq: u64,
    /// The dropped files and folders.
    pub items: Vec<SelectionItem>,
    /// The segment's item.
    pub entry: WheelEntry,
    /// The wheel's center, global physical pixels (to place the toast).
    pub anchor: (i32, i32),
    /// The display the wheel was on.
    pub display: DisplayBounds,
}

/// Where a drop landed, for the log.
#[derive(Debug, Clone, PartialEq)]
pub struct DropReport {
    /// The dropped files and folders.
    pub paths: Vec<PathBuf>,
    /// The wheel that was open.
    pub mode: WheelMode,
    /// The segment under the drop, or `None` for a cancel.
    pub segment: Option<usize>,
    /// That segment's item.
    pub entry: Option<WheelEntry>,
    /// The drop point in logical pixels from the anchor (y down).
    pub offset: Point,
}

impl fmt::Display for DropReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = match (self.segment, self.entry) {
            (Some(index), Some(entry)) => format!("segment {index} ({entry:?})"),
            (Some(index), None) => format!("segment {index}"),
            (None, _) => "no segment (cancel)".to_owned(),
        };
        write!(
            f,
            "drop on the {:?} wheel: {target}, at ({:.0}, {:.0}) from the anchor; ",
            self.mode, self.offset.x, self.offset.y
        )?;
        let paths: Vec<String> = self
            .paths
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        write!(f, "{} item(s): {}", paths.len(), paths.join(" | "))
    }
}

/// What the app must do, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Send `wheel://open`: draw or redraw the wheel.
    Open(WheelOpen),
    /// Frame the overlay on this monitor (global physical) and show it.
    ShowOverlay {
        /// The whole monitor.
        monitor: Rect,
    },
    /// Send `wheel://highlight`.
    Highlight(WheelHighlight),
    /// Send `wheel://close`.
    Close(WheelClose),
    /// Hide the overlay.
    HideOverlay,
    /// Plan and run a job.
    Run(RunRequest),
    /// A line for the log.
    Log(String),
}

#[derive(Debug, Clone)]
enum State {
    /// No drag.
    Idle,
    /// A drag with no wheel yet.
    Dragging {
        held: Modifiers,
        cursor: Option<(i32, i32)>,
    },
    /// The wheel is open.
    Open(Box<OpenWheel>),
    /// The wheel was closed or refused for this drag (Esc, not a file drag,
    /// a drop, Pause): stay out of the way until the drag ends.
    Dismissed,
}

#[derive(Debug, Clone)]
struct OpenWheel {
    seq: u64,
    mode: WheelMode,
    display: DisplayBounds,
    /// `display.scale`, made safe to divide by.
    scale: f64,
    /// Logical pixels from the monitor's top-left.
    anchor: Point,
    held: Modifiers,
    highlight: Option<usize>,
    /// From the drop target's Enter; `None` while pending.
    items: Option<Vec<SelectionItem>>,
    content: Option<WheelContent>,
    /// Between the drop target's Enter and Leave.
    inside: bool,
    /// The overlay reported it painted this wheel, so it's on screen.
    painted: bool,
    /// Set by the first pointer move once painted, cleared when files
    /// arrive.
    files_deadline: Option<Instant>,
    /// When the button went up over the overlay, if it has.
    ended_at: Option<Instant>,
    /// The last signal or drop event, for the watchdog.
    last_event: Instant,
    /// When the key that opened (or swapped) the wheel arrived.
    key_at: Instant,
}

impl OpenWheel {
    /// How many segments are drawn: 0 while pending or empty.
    fn segment_count(&self) -> usize {
        self.content.as_ref().map_or(0, |c| c.entries().len())
    }

    /// A global physical point as a logical offset from the anchor (y
    /// down), which is what `hit_test` takes.
    fn offset(&self, x: i32, y: i32) -> Point {
        let monitor = self.display.monitor;
        Point {
            x: f64::from(x - monitor.x) / self.scale - self.anchor.x,
            y: f64::from(y - monitor.y) / self.scale - self.anchor.y,
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let offset = self.offset(x, y);
        hit_test(offset.x, offset.y, self.segment_count())
    }

    /// The anchor in global physical pixels.
    fn anchor_global(&self) -> (i32, i32) {
        let monitor = self.display.monitor;
        (
            monitor.x + (self.anchor.x * self.scale).round() as i32,
            monitor.y + (self.anchor.y * self.scale).round() as i32,
        )
    }
}

/// The state machine. See the module docs.
#[derive(Debug)]
pub struct WheelController<E> {
    env: E,
    config: ControllerConfig,
    state: State,
    paused: bool,
    seq: u64,
}

impl<E: Env> WheelController<E> {
    /// A controller using `env` for the OS and `config` for its settings.
    pub fn new(env: E, config: ControllerConfig) -> Self {
        Self {
            env,
            config,
            state: State::Idle,
            paused: false,
            seq: 0,
        }
    }

    /// `true` while the wheel is open.
    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Open(_))
    }

    /// Handles one input at time `now`, then any deadline that has passed.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        let mut out = match input {
            Input::Signal(signal) => self.on_signal(signal, now),
            Input::Drop(event) => self.on_drop_event(event, now),
            Input::Painted { seq } => self.on_painted(seq, now),
            Input::SetPaused(paused) => self.on_paused(paused),
        };
        out.extend(self.tick(now));
        out
    }

    /// The next time [`tick`](Self::tick) has work, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        let State::Open(open) = &self.state else {
            return None;
        };
        [
            Some(open.last_event + WATCHDOG),
            open.files_deadline,
            open.ended_at.map(|at| at + DROP_GRACE),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Closes the wheel if a deadline has passed: the button went up and no
    /// drop came, no files arrived, or the watchdog fired.
    pub fn tick(&mut self, now: Instant) -> Vec<Effect> {
        let State::Open(open) = &self.state else {
            return Vec::new();
        };
        let (reason, next) = if open.ended_at.is_some_and(|at| now >= at + DROP_GRACE) {
            (CloseReason::DragEnded, State::Idle)
        } else if open.files_deadline.is_some_and(|at| now >= at) {
            (CloseReason::NoFiles, State::Dismissed)
        } else if now >= open.last_event + WATCHDOG {
            (CloseReason::Watchdog, State::Idle)
        } else {
            return Vec::new();
        };
        self.close(reason, next)
    }

    /// Closes the open wheel (if any) and moves to `next`.
    fn close(&mut self, reason: CloseReason, next: State) -> Vec<Effect> {
        let previous = std::mem::replace(&mut self.state, next);
        let State::Open(open) = previous else {
            return Vec::new();
        };
        vec![
            Effect::Close(WheelClose {
                seq: open.seq,
                reason,
            }),
            Effect::HideOverlay,
            Effect::Log(format!("wheel {} closed: {reason:?}", open.seq)),
        ]
    }

    fn on_signal(&mut self, signal: DragSignal, now: Instant) -> Vec<Effect> {
        match signal {
            DragSignal::Started { modifiers } => {
                let next = if self.paused {
                    State::Dismissed
                } else {
                    State::Dragging {
                        held: modifiers,
                        cursor: None,
                    }
                };
                // A wheel still open here missed its button-up.
                self.close(CloseReason::DragEnded, next)
            }
            DragSignal::Moved { x, y } => {
                match &mut self.state {
                    State::Dragging { cursor, .. } => *cursor = Some((x, y)),
                    State::Open(open) => {
                        open.last_event = now;
                        if open.painted && open.items.is_none() && open.files_deadline.is_none() {
                            open.files_deadline = Some(now + NO_FILES_GRACE);
                        }
                    }
                    State::Idle | State::Dismissed => {}
                }
                Vec::new()
            }
            DragSignal::Modifiers(held) => self.on_modifiers(held, now),
            DragSignal::Escape => match self.state {
                State::Open(_) => self.close(CloseReason::Escape, State::Dismissed),
                _ => Vec::new(),
            },
            DragSignal::Ended => match &mut self.state {
                State::Open(open) if open.inside => {
                    open.last_event = now;
                    open.ended_at.get_or_insert(now);
                    Vec::new()
                }
                State::Open(_) => self.close(CloseReason::DragEnded, State::Idle),
                _ => {
                    self.state = State::Idle;
                    Vec::new()
                }
            },
        }
    }

    fn on_modifiers(&mut self, held: Modifiers, now: Instant) -> Vec<Effect> {
        let mode = self.config.keys.mode_for(held);
        match &mut self.state {
            State::Dragging {
                held: was_held,
                cursor,
            } => {
                if held == *was_held {
                    return Vec::new();
                }
                *was_held = held;
                match (mode, *cursor) {
                    (Some(mode), Some(cursor)) => self.open(mode, held, cursor, now),
                    _ => Vec::new(),
                }
            }
            State::Open(open) => {
                open.held = held;
                open.last_event = now;
                match mode {
                    Some(mode) if mode != open.mode => {
                        open.mode = mode;
                        open.highlight = None;
                        open.key_at = now;
                        open.content = open
                            .items
                            .as_ref()
                            .map(|items| wheel_content(items, mode, self.config.features));
                        let event = open_event(&self.config, open);
                        vec![
                            Effect::Open(event),
                            Effect::Log(format!("wheel {} swapped to {mode:?}", open.seq)),
                        ]
                    }
                    _ => Vec::new(),
                }
            }
            State::Idle | State::Dismissed => Vec::new(),
        }
    }

    fn open(
        &mut self,
        mode: WheelMode,
        held: Modifiers,
        (x, y): (i32, i32),
        now: Instant,
    ) -> Vec<Effect> {
        let Some(display) = self.env.display_at(x, y) else {
            return vec![Effect::Log(format!(
                "wheel not opened: no display found under ({x}, {y})"
            ))];
        };
        let scale = sane_scale(display.scale);
        let monitor = display.monitor;
        let to_logical = |px: i32, origin: i32| f64::from(px - origin) / scale;
        let cursor = Point {
            x: to_logical(x, monitor.x),
            y: to_logical(y, monitor.y),
        };
        let work_area = grape_core::Rect {
            x: to_logical(display.work_area.x, monitor.x),
            y: to_logical(display.work_area.y, monitor.y),
            width: f64::from(display.work_area.width) / scale,
            height: f64::from(display.work_area.height) / scale,
        };
        self.seq += 1;
        let open = OpenWheel {
            seq: self.seq,
            mode,
            display,
            scale,
            anchor: clamp_anchor(cursor, work_area),
            held,
            highlight: None,
            items: None,
            content: None,
            inside: false,
            painted: false,
            files_deadline: None,
            ended_at: None,
            last_event: now,
            key_at: now,
        };
        let log = format!(
            "wheel {} open: {mode:?} at ({x}, {y}) on a {}×{} monitor at {:.0} % scale; anchor ({:.0}, {:.0})",
            open.seq,
            monitor.width,
            monitor.height,
            scale * 100.0,
            open.anchor.x,
            open.anchor.y
        );
        let event = open_event(&self.config, &open);
        self.state = State::Open(Box::new(open));
        vec![
            Effect::Open(event),
            Effect::ShowOverlay { monitor },
            Effect::Log(log),
        ]
    }

    fn on_drop_event(&mut self, event: DropEvent, now: Instant) -> Vec<Effect> {
        let State::Open(open) = &mut self.state else {
            return vec![Effect::Log(format!(
                "drop target event with the wheel closed, ignored: {event:?}"
            ))];
        };
        open.last_event = now;
        match event {
            DropEvent::Enter { paths, x, y } => {
                if paths.is_empty() {
                    return self.close(CloseReason::NoFiles, State::Dismissed);
                }
                let items = self.env.classify(&paths);
                let content = wheel_content(&items, open.mode, self.config.features);
                let log = format!(
                    "wheel {}: {} item(s) entered; {} segment(s)",
                    open.seq,
                    paths.len(),
                    content.entries().len()
                );
                open.items = Some(items);
                open.content = Some(content);
                open.inside = true;
                open.files_deadline = None;
                open.highlight = None;
                let event = open_event(&self.config, open);
                let mut out = vec![Effect::Open(event), Effect::Log(log)];
                out.extend(self.update_highlight(x, y));
                out
            }
            DropEvent::Over { x, y } => {
                open.inside = true;
                self.update_highlight(x, y)
            }
            DropEvent::Leave => {
                open.inside = false;
                if open.ended_at.is_some() {
                    return self.close(CloseReason::DragEnded, State::Idle);
                }
                if open.highlight.take().is_some() {
                    vec![Effect::Highlight(WheelHighlight {
                        seq: open.seq,
                        index: None,
                    })]
                } else {
                    Vec::new()
                }
            }
            DropEvent::Drop { paths, x, y } => self.on_drop(paths, x, y),
        }
    }

    fn on_drop(&mut self, paths: Vec<PathBuf>, x: i32, y: i32) -> Vec<Effect> {
        let State::Open(open) = &self.state else {
            return Vec::new();
        };
        let segment = open.hit(x, y);
        let entry = segment.and_then(|index| {
            open.content
                .as_ref()
                .and_then(|content| content.entries().get(index).copied())
        });
        let report = DropReport {
            paths,
            mode: open.mode,
            segment,
            entry,
            offset: open.offset(x, y),
        };
        let run = entry.map(|entry| RunRequest {
            seq: open.seq,
            items: self.env.classify(&report.paths),
            entry,
            anchor: open.anchor_global(),
            display: open.display,
        });
        let next = if open.ended_at.is_some() {
            State::Idle
        } else {
            State::Dismissed
        };
        let reason = if run.is_some() {
            CloseReason::Drop
        } else {
            CloseReason::Cancel
        };
        let mut out = self.close(reason, next);
        out.push(Effect::Log(report.to_string()));
        out.extend(run.map(Effect::Run));
        out
    }

    fn update_highlight(&mut self, x: i32, y: i32) -> Vec<Effect> {
        let State::Open(open) = &mut self.state else {
            return Vec::new();
        };
        let index = open.hit(x, y);
        if index == open.highlight {
            return Vec::new();
        }
        open.highlight = index;
        vec![Effect::Highlight(WheelHighlight {
            seq: open.seq,
            index,
        })]
    }

    fn on_painted(&mut self, seq: u64, now: Instant) -> Vec<Effect> {
        match &mut self.state {
            State::Open(open) if open.seq == seq => {
                let first = !open.painted;
                open.painted = true;
                if !first {
                    return Vec::new();
                }
                let ms = now.saturating_duration_since(open.key_at).as_secs_f64() * 1000.0;
                vec![Effect::Log(format!(
                    "wheel {seq} painted {ms:.0} ms after the key (target: under 100 ms)"
                ))]
            }
            _ => Vec::new(),
        }
    }

    fn on_paused(&mut self, paused: bool) -> Vec<Effect> {
        self.paused = paused;
        if !paused {
            return Vec::new();
        }
        let next = match self.state {
            State::Idle => State::Idle,
            _ => State::Dismissed,
        };
        self.close(CloseReason::Paused, next)
    }
}

/// The `wheel://open` payload for `open` as it is now.
fn open_event(config: &ControllerConfig, open: &OpenWheel) -> WheelOpen {
    WheelOpen {
        seq: open.seq,
        mode: open.mode,
        anchor: open.anchor,
        scale: open.scale,
        geometry: GEOMETRY,
        keys: KeyNames::of(config.keys),
        sounds: config.sounds,
        content: open.content.as_ref().map_or(UiContent::Pending, |content| {
            UiContent::from_core(content, config.platform)
        }),
    }
}

/// A usable scale factor: anything not finite or not positive counts as 1.
fn sane_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grape_core::{ActionKey, Format};
    use grape_platform::mock::scripts;

    const NONE: Modifiers = Modifiers::NONE;

    /// A fake OS: one 1920×1080 monitor at `scale`, offset to (`x`, `y`),
    /// with a 48 px taskbar at the bottom. Paths ending in `/` are folders.
    #[derive(Debug, Clone, Copy)]
    struct FakeEnv {
        x: i32,
        y: i32,
        scale: f64,
        has_display: bool,
    }

    impl FakeEnv {
        fn at(x: i32, y: i32, scale: f64) -> Self {
            Self {
                x,
                y,
                scale,
                has_display: true,
            }
        }
    }

    impl Env for FakeEnv {
        fn display_at(&self, _x: i32, _y: i32) -> Option<DisplayBounds> {
            self.has_display.then_some(DisplayBounds {
                monitor: Rect {
                    x: self.x,
                    y: self.y,
                    width: 1920,
                    height: 1080,
                },
                work_area: Rect {
                    x: self.x,
                    y: self.y,
                    width: 1920,
                    height: 1032,
                },
                scale: self.scale,
            })
        }

        fn classify(&self, paths: &[PathBuf]) -> Vec<SelectionItem> {
            paths
                .iter()
                .map(|path| {
                    if path.to_string_lossy().ends_with('/') {
                        SelectionItem::folder(path.clone())
                    } else {
                        SelectionItem::file(path.clone())
                    }
                })
                .collect()
        }
    }

    type Ctl = WheelController<FakeEnv>;

    fn config() -> ControllerConfig {
        ControllerConfig {
            keys: KeyMap::WINDOWS,
            features: Features::V1_ONLY,
            platform: Platform::Windows,
            sounds: true,
        }
    }

    fn controller_on(env: FakeEnv) -> Ctl {
        WheelController::new(env, config())
    }

    struct Run {
        ctl: Ctl,
        now: Instant,
        effects: Vec<Effect>,
    }

    impl Run {
        fn new() -> Self {
            Self::on(FakeEnv::at(0, 0, 1.0))
        }

        fn on(env: FakeEnv) -> Self {
            Self {
                ctl: controller_on(env),
                now: Instant::now(),
                effects: Vec::new(),
            }
        }

        fn signals(&mut self, signals: Vec<DragSignal>) -> &mut Self {
            for signal in signals {
                self.input(Input::Signal(signal));
            }
            self
        }

        fn input(&mut self, input: Input) -> &mut Self {
            self.now += Duration::from_millis(10);
            let effects = self.ctl.handle(input, self.now);
            self.effects.extend(effects);
            self
        }

        fn drop_event(&mut self, event: DropEvent) -> &mut Self {
            self.input(Input::Drop(event))
        }

        fn wait(&mut self, by: Duration) -> &mut Self {
            self.now += by;
            let effects = self.ctl.tick(self.now);
            self.effects.extend(effects);
            self
        }

        fn painted(&mut self) -> &mut Self {
            let seq = self.opens().last().map(|o| o.seq).unwrap_or_default();
            self.input(Input::Painted { seq })
        }

        fn opens(&self) -> Vec<&WheelOpen> {
            self.effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Open(open) => Some(open),
                    _ => None,
                })
                .collect()
        }

        fn shows(&self) -> usize {
            self.effects
                .iter()
                .filter(|e| matches!(e, Effect::ShowOverlay { .. }))
                .count()
        }

        fn closes(&self) -> Vec<CloseReason> {
            self.effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Close(close) => Some(close.reason),
                    _ => None,
                })
                .collect()
        }

        fn hides(&self) -> usize {
            self.effects
                .iter()
                .filter(|e| matches!(e, Effect::HideOverlay))
                .count()
        }

        fn highlights(&self) -> Vec<Option<usize>> {
            self.effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Highlight(h) => Some(h.index),
                    _ => None,
                })
                .collect()
        }

        fn runs(&self) -> Vec<&RunRequest> {
            self.effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Run(run) => Some(run),
                    _ => None,
                })
                .collect()
        }

        fn logs(&self) -> Vec<&str> {
            self.effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Log(line) => Some(line.as_str()),
                    _ => None,
                })
                .collect()
        }

        fn last_content(&self) -> Option<&UiContent> {
            self.opens().last().map(|o| &o.content)
        }
    }

    fn started() -> DragSignal {
        DragSignal::Started { modifiers: NONE }
    }

    fn moved(x: i32, y: i32) -> DragSignal {
        DragSignal::Moved { x, y }
    }

    fn keys(held: Modifiers) -> DragSignal {
        DragSignal::Modifiers(held)
    }

    fn png() -> Vec<PathBuf> {
        vec![PathBuf::from(r"C:\Users\max\Pictures\shot.png")]
    }

    fn enter(paths: Vec<PathBuf>, x: i32, y: i32) -> DropEvent {
        DropEvent::Enter { paths, x, y }
    }

    fn drop_at(paths: Vec<PathBuf>, x: i32, y: i32) -> DropEvent {
        DropEvent::Drop { paths, x, y }
    }

    /// Drag, tap Shift at (400, 300): the wheel is open, still pending.
    fn open_at_400_300() -> Run {
        let mut run = Run::new();
        run.signals(vec![started(), moved(400, 300), keys(Modifiers::SHIFT)]);
        run
    }

    /// As above, then the PNG enters the overlay at the anchor: the
    /// conversions wheel shows PNG's 7 targets.
    fn png_wheel() -> Run {
        let mut run = open_at_400_300();
        run.drop_event(enter(png(), 400, 300));
        run
    }

    /// The point `r` logical px from the anchor (400, 300) at math angle
    /// `angle`, at 100 % scale.
    fn at_angle(r: f64, angle: f64) -> (i32, i32) {
        (
            400 + (r * angle.cos()).round() as i32,
            300 - (r * angle.sin()).round() as i32,
        )
    }

    #[test]
    fn key_maps_pick_one_wheel_per_key() {
        let win = KeyMap::WINDOWS;
        assert_eq!(win.mode_for(Modifiers::SHIFT), Some(WheelMode::Conversions));
        assert_eq!(win.mode_for(Modifiers::CTRL), Some(WheelMode::Tools));
        assert_eq!(win.mode_for(Modifiers::ALT), Some(WheelMode::Actions));
        assert_eq!(win.mode_for(Modifiers::META), None);
        assert_eq!(win.mode_for(NONE), None);
        let two = Modifiers {
            shift: true,
            ctrl: true,
            ..NONE
        };
        assert_eq!(win.mode_for(two), None);
        let mac = KeyMap::MACOS;
        assert_eq!(mac.mode_for(Modifiers::META), Some(WheelMode::Tools));
        assert_eq!(mac.mode_for(Modifiers::CTRL), Some(WheelMode::Actions));
        assert_eq!(mac.mode_for(Modifiers::ALT), None);
    }

    #[test]
    fn key_names_follow_the_os() {
        let names = KeyNames::of(KeyMap::WINDOWS);
        if cfg!(target_os = "macos") {
            assert_eq!(names.tools, "Control");
        } else {
            assert_eq!(
                (
                    names.conversions.as_str(),
                    names.tools.as_str(),
                    names.actions.as_str()
                ),
                ("Shift", "Ctrl", "Alt")
            );
        }
    }

    #[test]
    fn a_plain_drag_never_opens_the_wheel() {
        let mut run = Run::new();
        run.signals(scripts::plain_drag());
        assert!(run.effects.is_empty(), "{:?}", run.effects);
        assert!(!run.ctl.is_open());
    }

    #[test]
    fn tapping_shift_opens_a_pending_conversions_wheel_at_the_cursor() {
        let mut run = Run::new();
        run.signals(scripts::shift_tap());
        let opens = run.opens();
        let [open] = opens.as_slice() else {
            panic!("{:?}", run.effects);
        };
        assert_eq!(open.mode, WheelMode::Conversions);
        assert_eq!(open.anchor, Point { x: 400.0, y: 300.0 });
        assert_eq!(open.content, UiContent::Pending);
        assert_eq!(open.keys, KeyNames::of(KeyMap::WINDOWS));
        assert!(open.sounds);
        assert_eq!(run.shows(), 1);
        // The event goes out before the overlay shows, so it's drawn by then.
        let first_open = run
            .effects
            .iter()
            .position(|e| matches!(e, Effect::Open(_)));
        let first_show = run
            .effects
            .iter()
            .position(|e| matches!(e, Effect::ShowOverlay { .. }));
        assert!(first_open < first_show);
        // The button went up while not inside the overlay: it closes.
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
    }

    #[test]
    fn files_entering_fill_the_wheel_with_their_targets() {
        let run = png_wheel();
        let Some(UiContent::Entries { segments, .. }) = run.last_content() else {
            panic!("{:?}", run.effects);
        };
        let titles: Vec<&str> = segments.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            ["JPG", "WEBP", "HEIC", "TIFF", "AVIF", "BMP", "PDF"]
        );
        // Same wheel instance as the pending open.
        let seqs: Vec<u64> = run.opens().iter().map(|o| o.seq).collect();
        assert_eq!(seqs, vec![1, 1]);
        assert_eq!(run.shows(), 1);
    }

    #[test]
    fn a_folder_gets_actions_and_empty_conversions() {
        let mut run = open_at_400_300();
        run.drop_event(enter(vec![PathBuf::from("C:/Users/max/Photos/")], 400, 300));
        assert_eq!(
            run.last_content(),
            Some(&UiContent::Empty {
                state: grape_core::EmptyState::NoConversions
            })
        );
        run.signals(vec![keys(NONE), keys(Modifiers::ALT)]);
        let Some(UiContent::Entries { segments, .. }) = run.last_content() else {
            panic!("{:?}", run.effects);
        };
        assert_eq!(segments.len(), 6);
        assert_eq!(segments.first().map(|s| s.title.as_str()), Some("SHARE"));
    }

    #[test]
    fn a_key_held_before_the_drag_does_not_open_the_wheel() {
        let mut run = Run::new();
        run.signals(vec![
            DragSignal::Started {
                modifiers: Modifiers::SHIFT,
            },
            moved(400, 300),
            moved(430, 310),
        ]);
        assert!(run.opens().is_empty());
        run.signals(vec![keys(NONE), keys(Modifiers::SHIFT)]);
        assert_eq!(run.opens().len(), 1);
    }

    #[test]
    fn the_scripted_held_shift_flow_opens_once() {
        let mut run = Run::new();
        run.signals(scripts::shift_held_from_start());
        assert_eq!(run.shows(), 1);
    }

    #[test]
    fn two_modifiers_mean_no_mode() {
        let mut run = Run::new();
        run.signals(scripts::two_modifiers());
        assert!(run.opens().is_empty());
    }

    #[test]
    fn another_key_swaps_the_wheel_in_place() {
        let mut run = png_wheel();
        run.signals(vec![keys(NONE), keys(Modifiers::CTRL)]);
        let opens = run.opens();
        let Some(last) = opens.last() else {
            panic!("no open");
        };
        assert_eq!(last.mode, WheelMode::Tools);
        assert_eq!(last.anchor, Point { x: 400.0, y: 300.0 });
        assert_eq!(last.seq, 1);
        let UiContent::Entries { segments, .. } = &last.content else {
            panic!("{:?}", last.content);
        };
        let titles: Vec<&str> = segments.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["COMPRESS", "METADATA"]);
        assert_eq!(run.shows(), 1);
    }

    #[test]
    fn a_swap_before_files_arrive_stays_pending() {
        let mut run = open_at_400_300();
        run.signals(vec![keys(NONE), keys(Modifiers::CTRL)]);
        assert_eq!(run.last_content(), Some(&UiContent::Pending));
        assert_eq!(run.opens().last().map(|o| o.mode), Some(WheelMode::Tools));
    }

    #[test]
    fn the_same_key_again_does_not_swap() {
        let mut run = open_at_400_300();
        run.signals(vec![keys(NONE), keys(Modifiers::SHIFT)]);
        assert_eq!(run.opens().len(), 1);
    }

    #[test]
    fn two_keys_while_open_change_nothing() {
        let mut run = open_at_400_300();
        run.signals(vec![keys(Modifiers {
            shift: true,
            alt: true,
            ..NONE
        })]);
        assert_eq!(run.opens().len(), 1);
        assert!(run.ctl.is_open());
    }

    #[test]
    fn esc_closes_the_wheel_for_the_rest_of_the_drag() {
        let mut run = Run::new();
        run.signals(scripts::escape());
        assert_eq!(run.opens().len(), 1);
        assert_eq!(run.closes(), vec![CloseReason::Escape]);
        assert_eq!(run.hides(), 1);
        let mut run = Run::new();
        run.signals(vec![
            started(),
            moved(400, 300),
            keys(Modifiers::SHIFT),
            DragSignal::Escape,
            keys(NONE),
            keys(Modifiers::SHIFT),
        ]);
        assert_eq!(run.opens().len(), 1);
    }

    #[test]
    fn hovering_highlights_the_segment_under_the_pointer() {
        let mut run = png_wheel();
        // 84 px above the anchor: segment 0, the top one.
        run.drop_event(DropEvent::Over { x: 400, y: 216 });
        run.drop_event(DropEvent::Over { x: 401, y: 216 });
        // The dead zone.
        run.drop_event(DropEvent::Over { x: 410, y: 300 });
        assert_eq!(run.highlights(), vec![Some(0), None]);
    }

    #[test]
    fn a_pending_wheel_highlights_nothing() {
        let mut run = open_at_400_300();
        run.signals(vec![moved(400, 216)]);
        assert!(run.highlights().is_empty());
    }

    #[test]
    fn a_drop_on_a_segment_runs_its_entry() {
        let mut run = png_wheel();
        // Segment 1 of 7 sits at 90° − 360°/7 clockwise from the top.
        let angle = std::f64::consts::FRAC_PI_2 - std::f64::consts::TAU / 7.0;
        let (x, y) = at_angle(84.0, angle);
        run.signals(vec![DragSignal::Ended]);
        run.drop_event(drop_at(png(), x, y));
        let runs = run.runs();
        let [request] = runs.as_slice() else {
            panic!("{:?}", run.effects);
        };
        assert_eq!(request.entry, WheelEntry::Convert(Format::Webp));
        assert_eq!(request.items, vec![SelectionItem::file(png()[0].clone())]);
        assert_eq!(request.anchor, (400, 300));
        assert_eq!(run.closes(), vec![CloseReason::Drop]);
        assert!(run
            .logs()
            .iter()
            .any(|line| line.contains("segment 1 (Convert(Webp))") && line.contains("shot.png")));
        assert!(!run.ctl.is_open());
    }

    #[test]
    fn a_drop_off_the_ring_is_a_cancel() {
        let mut run = png_wheel();
        run.drop_event(drop_at(png(), 900, 700));
        assert!(run.runs().is_empty());
        assert_eq!(run.closes(), vec![CloseReason::Cancel]);
        assert!(run.logs().iter().any(|line| line.contains("cancel")));
    }

    #[test]
    fn a_drop_on_a_pending_wheel_is_a_cancel() {
        let mut run = open_at_400_300();
        run.drop_event(drop_at(png(), 400, 216));
        assert!(run.runs().is_empty());
        assert_eq!(run.closes(), vec![CloseReason::Cancel]);
    }

    #[test]
    fn an_actions_drop_still_asks_to_run() {
        let mut run = Run::new();
        run.signals(vec![started(), moved(400, 300), keys(Modifiers::ALT)]);
        run.drop_event(enter(png(), 400, 300));
        run.drop_event(drop_at(png(), 400, 216));
        assert_eq!(
            run.runs().first().map(|r| r.entry),
            Some(WheelEntry::Action(ActionKey::Share))
        );
    }

    #[test]
    fn the_drop_can_arrive_before_or_after_the_button_up() {
        let mut run = png_wheel();
        run.drop_event(drop_at(png(), 400, 216));
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.closes(), vec![CloseReason::Drop]);
        assert_eq!(run.runs().len(), 1);
        run.signals(vec![started(), moved(10, 10), keys(Modifiers::ALT)]);
        assert_eq!(run.shows(), 2);
    }

    #[test]
    fn a_button_up_inside_waits_for_the_drop() {
        let mut run = png_wheel();
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE / 2);
        assert!(run.closes().is_empty());
        run.drop_event(drop_at(png(), 400, 216));
        assert_eq!(run.closes(), vec![CloseReason::Drop]);
    }

    #[test]
    fn a_button_up_inside_with_no_drop_closes_after_the_grace() {
        let mut run = png_wheel();
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE / 2);
        assert!(run.closes().is_empty());
        run.wait(DROP_GRACE);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        assert!(run.runs().is_empty());
    }

    #[test]
    fn a_button_up_outside_closes_at_once() {
        let mut run = png_wheel();
        run.drop_event(DropEvent::Leave);
        run.signals(vec![DragSignal::Ended]);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
    }

    #[test]
    fn leaving_after_the_button_up_closes() {
        let mut run = png_wheel();
        run.signals(vec![DragSignal::Ended]);
        run.drop_event(DropEvent::Leave);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
    }

    #[test]
    fn a_drag_without_files_closes_the_wheel() {
        let mut run = open_at_400_300();
        run.drop_event(enter(Vec::new(), 400, 300));
        assert_eq!(run.closes(), vec![CloseReason::NoFiles]);
        run.signals(vec![keys(NONE), keys(Modifiers::SHIFT)]);
        assert_eq!(run.shows(), 1);
    }

    #[test]
    fn the_no_files_clock_waits_for_the_overlay_to_paint() {
        let mut run = open_at_400_300();
        run.signals(vec![moved(410, 300)]);
        run.wait(NO_FILES_GRACE * 3);
        run.signals(vec![moved(420, 300)]);
        run.wait(NO_FILES_GRACE * 3);
        assert!(run.ctl.is_open());
        run.painted();
        run.signals(vec![moved(430, 300)]);
        run.wait(NO_FILES_GRACE * 2);
        assert_eq!(run.closes(), vec![CloseReason::NoFiles]);
    }

    #[test]
    fn files_arriving_cancel_the_no_files_deadline() {
        let mut run = open_at_400_300();
        run.painted();
        run.signals(vec![moved(410, 300)]);
        run.drop_event(enter(png(), 410, 300));
        run.wait(NO_FILES_GRACE * 3);
        assert!(run.ctl.is_open());
    }

    #[test]
    fn the_watchdog_hides_a_stuck_overlay() {
        let mut run = png_wheel();
        assert_eq!(run.ctl.next_deadline(), Some(run.now + WATCHDOG));
        run.wait(WATCHDOG - Duration::from_secs(1));
        assert!(run.ctl.is_open());
        run.wait(Duration::from_secs(2));
        assert_eq!(run.closes(), vec![CloseReason::Watchdog]);
    }

    #[test]
    fn leave_clears_the_highlight_but_keeps_the_wheel() {
        let mut run = open_at_400_300();
        run.drop_event(enter(png(), 400, 216));
        run.drop_event(DropEvent::Leave);
        assert_eq!(run.highlights(), vec![Some(0), None]);
        assert!(run.ctl.is_open());
    }

    #[test]
    fn pause_closes_the_wheel_and_ignores_drags() {
        let mut run = open_at_400_300();
        run.input(Input::SetPaused(true));
        assert_eq!(run.closes(), vec![CloseReason::Paused]);
        run.signals(vec![DragSignal::Ended]);
        run.signals(scripts::shift_tap());
        assert_eq!(run.shows(), 1);
        run.input(Input::SetPaused(false));
        run.signals(scripts::shift_tap());
        assert_eq!(run.shows(), 2);
    }

    #[test]
    fn drop_events_with_the_wheel_closed_are_ignored() {
        let mut run = Run::new();
        run.drop_event(drop_at(png(), 0, 0));
        assert!(run.runs().is_empty());
        assert_eq!(run.logs().len(), 1);
    }

    #[test]
    fn a_missed_button_up_closes_the_wheel_at_the_next_drag() {
        let mut run = open_at_400_300();
        run.signals(vec![started()]);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        assert!(!run.ctl.is_open());
    }

    #[test]
    fn painted_logs_the_latency_once_for_the_current_wheel() {
        let mut run = open_at_400_300();
        run.input(Input::Painted { seq: 8 });
        assert!(run.logs().iter().all(|line| !line.contains("painted")));
        run.input(Input::Painted { seq: 1 });
        run.input(Input::Painted { seq: 1 });
        let logs = run.logs();
        let painted: Vec<&str> = logs
            .iter()
            .copied()
            .filter(|line| line.contains("painted"))
            .collect();
        assert_eq!(painted.len(), 1);
        assert!(painted
            .first()
            .is_some_and(|line| line.contains("painted 20 ms after the key")));
    }

    #[test]
    fn the_anchor_is_logical_and_clamped_on_a_scaled_offset_monitor() {
        let mut ctl = controller_on(FakeEnv::at(-1920, 0, 1.5));
        let now = Instant::now();
        for signal in [started(), moved(-1915, 1075), keys(Modifiers::SHIFT)] {
            let effects = ctl.handle(Input::Signal(signal), now);
            if let Some(Effect::Open(open)) = effects.first() {
                assert!((open.scale - 1.5).abs() < 1e-9);
                assert!((open.anchor.x - 140.0).abs() < 1e-9, "{:?}", open.anchor);
                assert!(
                    (open.anchor.y - (1032.0 / 1.5 - 140.0)).abs() < 1e-9,
                    "{:?}",
                    open.anchor
                );
                assert!(effects.contains(&Effect::ShowOverlay {
                    monitor: Rect {
                        x: -1920,
                        y: 0,
                        width: 1920,
                        height: 1080
                    }
                }));
                return;
            }
        }
        panic!("the wheel never opened");
    }

    #[test]
    fn hit_tests_use_logical_offsets_on_scaled_monitors() {
        let mut ctl = controller_on(FakeEnv::at(0, 0, 2.0));
        let now = Instant::now();
        for signal in [started(), moved(800, 600), keys(Modifiers::SHIFT)] {
            ctl.handle(Input::Signal(signal), now);
        }
        // 84 logical px above the anchor is 168 physical px.
        let effects = ctl.handle(Input::Drop(enter(png(), 800, 600 - 168)), now);
        assert!(
            effects.contains(&Effect::Highlight(WheelHighlight {
                seq: 1,
                index: Some(0)
            })),
            "{effects:?}"
        );
    }

    #[test]
    fn the_run_anchor_is_global_physical() {
        let mut ctl = controller_on(FakeEnv::at(1920, 0, 1.5));
        let now = Instant::now();
        for signal in [started(), moved(2400, 600), keys(Modifiers::SHIFT)] {
            ctl.handle(Input::Signal(signal), now);
        }
        ctl.handle(Input::Drop(enter(png(), 2400, 600)), now);
        let effects = ctl.handle(Input::Drop(drop_at(png(), 2400, 600 - 126)), now);
        let run = effects.iter().find_map(|e| match e {
            Effect::Run(run) => Some(run),
            _ => None,
        });
        assert_eq!(run.map(|r| r.anchor), Some((2400, 600)));
    }

    #[test]
    fn no_display_means_no_wheel() {
        let mut ctl = controller_on(FakeEnv {
            has_display: false,
            ..FakeEnv::at(0, 0, 1.0)
        });
        let now = Instant::now();
        let mut effects = Vec::new();
        for signal in [started(), moved(5, 5), keys(Modifiers::SHIFT)] {
            effects.extend(ctl.handle(Input::Signal(signal), now));
        }
        assert!(!ctl.is_open());
        assert!(matches!(effects.as_slice(), [Effect::Log(_)]));
    }

    #[test]
    fn bad_scales_count_as_one() {
        assert!((sane_scale(f64::NAN) - 1.0).abs() < 1e-9);
        assert!((sane_scale(0.0) - 1.0).abs() < 1e-9);
        assert!((sane_scale(-2.0) - 1.0).abs() < 1e-9);
        assert!((sane_scale(1.25) - 1.25).abs() < 1e-9);
    }
}
