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
//!
//! Esc closes an open wheel, and Esc before any wheel keeps one from
//! opening: Explorer cancels its drag on Esc, so a key tapped while the
//! button is still down would open a wheel over a drag that no longer
//! exists. Either way the controller stays out of the way until the button
//! goes up.
//!
//! Two [`ControllerConfig`] flags cover what AppKit hides on macOS. Both
//! are off on Windows, where the controller behaves as before.
//! `release_fallback` runs the highlighted segment when the button goes up
//! with Command or Control held and no drop arrives, because AppKit may
//! refuse the drop then. `leave_ends_drag` reads a Leave that arrives while
//! the button is still down, with the cursor still on the wheel's display,
//! as Esc: Esc ends the AppKit drag session but not the press, and Grape
//! can't read the key itself (`docs/DECISIONS.md` D7). A refused drop
//! brings its Leave within [`REFUSED_LEAVE_WINDOW`] of the button-up; a
//! button-up later than that after a Leave follows Esc, so nothing runs.

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
/// Shift move. Under [`ControllerConfig::release_fallback`] it's also how
/// long to wait before running the highlighted segment without a drop.
pub const DROP_GRACE: Duration = Duration::from_millis(750);
/// Under [`ControllerConfig::leave_ends_drag`], how long a Leave with the
/// button still down waits for the drag to come back before it counts as
/// Esc. An Enter or Over within this time was a flicker. A button-up
/// within [`REFUSED_LEAVE_WINDOW`] of the Leave was a refused drop, and a
/// later one ends the drag with nothing run.
pub const LEAVE_GRACE: Duration = Duration::from_millis(200);
/// Under [`ControllerConfig::release_fallback`], how soon after a Leave the
/// button must go up for the Leave to count as a refused drop, whose
/// highlighted segment may then run. A refused drop's Leave and the
/// poller's button-up arrive within a frame or two of each other. A later
/// button-up follows Esc, which on macOS is a Leave with the button still
/// down: the wheel closes and nothing runs.
pub const REFUSED_LEAVE_WINDOW: Duration = Duration::from_millis(50);

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
    /// The display under the global point (`x`, `y`): physical pixels on
    /// Windows, points on macOS.
    fn display_at(&self, x: i32, y: i32) -> Option<DisplayBounds>;
    /// The dragged paths as selection items: folders as folders, everything
    /// else as files.
    fn classify(&self, paths: &[PathBuf]) -> Vec<SelectionItem>;
}

/// The settings the controller follows. Settings become editable in M6.
///
/// `release_fallback` and `leave_ends_drag` are macOS behaviors. They are
/// on in [`for_this_os`](Self::for_this_os) on macOS and off elsewhere.
/// With both off the controller behaves exactly as it does on Windows.
/// They work as a pair: a refused drop can bring its Leave just before the
/// button-up (within [`REFUSED_LEAVE_WINDOW`]), and `leave_ends_drag` keeps
/// the highlight through it.
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
    /// Run the highlighted segment when the button goes up over the
    /// overlay with Command or Control held and no drop arrives within
    /// [`DROP_GRACE`]. macOS narrows the drag's allowed operations while
    /// those keys are held, and wry always answers Copy, so AppKit may
    /// refuse the drop: no Drop event comes although the user released
    /// over a segment. With neither key held a normal drop comes, so
    /// nothing runs on release. A button-up just after a Leave counts as
    /// over the overlay only within [`REFUSED_LEAVE_WINDOW`] of the Leave.
    pub release_fallback: bool,
    /// Read a Leave from the drop target as Esc when it arrives while the
    /// button is still down and the cursor is still on the wheel's
    /// monitor. The wheel keeps its highlight for [`LEAVE_GRACE`]; an
    /// Enter or Over in that time was a flicker, a button-up within
    /// [`REFUSED_LEAVE_WINDOW`] is a refused drop's release, a later one
    /// ends the drag with nothing run, and otherwise the wheel closes as
    /// for Esc. On macOS Esc cancels the drag session (a Leave) while the
    /// button stays down, and Grape can't read the key itself
    /// (`docs/DECISIONS.md` D7).
    pub leave_ends_drag: bool,
}

impl ControllerConfig {
    /// The defaults for the OS Grape is running on: its keys, v1 only,
    /// sounds on, and the macOS drop workarounds on macOS only.
    pub const fn for_this_os() -> Self {
        let mac = cfg!(target_os = "macos");
        Self {
            keys: KeyMap::for_this_os(),
            features: Features::V1_ONLY,
            platform: if mac {
                Platform::MacOs
            } else {
                Platform::Windows
            },
            sounds: true,
            release_fallback: mac,
            leave_ends_drag: mac,
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
    /// The wheel's center, in global coordinates (to place the toast).
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
    /// Frame the overlay on this monitor (global coordinates) and show it.
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
    /// The wheel was closed or refused for this drag (Esc with or without a
    /// wheel, not a file drag, a drop, Pause): stay out of the way until the
    /// drag ends.
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
    /// The keys held when `ended_at` was set, for `release_fallback`.
    held_at_release: Modifiers,
    /// The last cursor position from the drag monitor, global.
    cursor: (i32, i32),
    /// Under `leave_ends_drag`: when a Leave arrived with the button still
    /// down and the cursor on this wheel's monitor. Enter, Over and the
    /// button-up clear it.
    left_at: Option<Instant>,
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

    /// `true` while the last cursor position is on this wheel's monitor.
    fn cursor_on_monitor(&self) -> bool {
        contains(self.display.monitor, self.cursor)
    }

    /// `true` if a pending Leave came at most [`REFUSED_LEAVE_WINDOW`]
    /// before `now`, so a button-up now may be its refused drop.
    fn left_just_before(&self, now: Instant) -> bool {
        self.left_at
            .is_some_and(|at| now.saturating_duration_since(at) <= REFUSED_LEAVE_WINDOW)
    }

    /// Clears the highlight, with the event the overlay needs if there was
    /// one.
    fn clear_highlight(&mut self) -> Vec<Effect> {
        if self.highlight.take().is_some() {
            vec![Effect::Highlight(WheelHighlight {
                seq: self.seq,
                index: None,
            })]
        } else {
            Vec::new()
        }
    }

    /// A global point as a logical offset from the anchor (y
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

    /// The anchor in global coordinates.
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
            open.left_at.map(|at| at + LEAVE_GRACE),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Closes the wheel if a deadline has passed: the button went up and no
    /// drop came (under `release_fallback` that may run the highlighted
    /// segment instead), the drag left with the button down and didn't
    /// come back (under `leave_ends_drag`), no files arrived, or the
    /// watchdog fired.
    pub fn tick(&mut self, now: Instant) -> Vec<Effect> {
        let State::Open(open) = &self.state else {
            return Vec::new();
        };
        let (reason, next) = if open.ended_at.is_some_and(|at| now >= at + DROP_GRACE) {
            if let Some((run, log)) = release_run(&self.config, open) {
                let mut out = self.close(CloseReason::Drop, State::Idle);
                out.push(Effect::Log(log));
                out.push(Effect::Run(run));
                return out;
            }
            (CloseReason::DragEnded, State::Idle)
        } else if open.left_at.is_some_and(|at| now >= at + LEAVE_GRACE) {
            let log = format!(
                "wheel {}: the drag left the overlay with the button still down and didn't come back within {} ms; taking it as Esc",
                open.seq,
                LEAVE_GRACE.as_millis()
            );
            let mut out = self.close(CloseReason::Escape, State::Dismissed);
            out.push(Effect::Log(log));
            return out;
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
            DragSignal::Moved { x, y } => match &mut self.state {
                State::Dragging { cursor, .. } => {
                    *cursor = Some((x, y));
                    Vec::new()
                }
                State::Open(open) => {
                    open.last_event = now;
                    open.cursor = (x, y);
                    if open.painted && open.items.is_none() && open.files_deadline.is_none() {
                        open.files_deadline = Some(now + NO_FILES_GRACE);
                    }
                    // The Leave came just before the cursor reached another
                    // display: the drag moved there, it wasn't Esc.
                    if open.left_at.is_some() && !open.cursor_on_monitor() {
                        open.left_at = None;
                        return open.clear_highlight();
                    }
                    Vec::new()
                }
                State::Idle | State::Dismissed => Vec::new(),
            },
            DragSignal::Modifiers(held) => self.on_modifiers(held, now),
            DragSignal::Escape => match self.state {
                State::Open(_) => self.close(CloseReason::Escape, State::Dismissed),
                // Esc before any wheel: Explorer cancels its drag on Esc, so
                // a key tapped before the button goes up mustn't open a
                // wheel over a drag that's gone.
                State::Dragging { .. } => {
                    self.state = State::Dismissed;
                    vec![Effect::Log(
                        "Esc during the drag: no wheel until the button goes up".to_owned(),
                    )]
                }
                State::Idle | State::Dismissed => Vec::new(),
            },
            DragSignal::Ended => {
                let release_fallback = self.config.release_fallback;
                match &mut self.state {
                    // Over the overlay, or right after a Leave that may be a
                    // refused drop: wait for the drop.
                    State::Open(open)
                        if open.inside || (release_fallback && open.left_just_before(now)) =>
                    {
                        open.last_event = now;
                        open.left_at = None;
                        if open.ended_at.is_none() {
                            open.ended_at = Some(now);
                            open.held_at_release = open.held;
                        }
                        Vec::new()
                    }
                    State::Open(open) => {
                        // A Leave too long before the button-up for a
                        // refused drop was Esc: nothing runs.
                        let log = open.left_at.filter(|_| release_fallback).map(|at| {
                            format!(
                                "wheel {}: the button went up {} ms after a Leave, later than a refused drop's {} ms; taking the Leave as Esc, so nothing runs",
                                open.seq,
                                now.saturating_duration_since(at).as_millis(),
                                REFUSED_LEAVE_WINDOW.as_millis()
                            )
                        });
                        let mut out = self.close(CloseReason::DragEnded, State::Idle);
                        out.extend(log.map(Effect::Log));
                        out
                    }
                    _ => {
                        self.state = State::Idle;
                        Vec::new()
                    }
                }
            }
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
            held_at_release: Modifiers::NONE,
            cursor: (x, y),
            left_at: None,
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
                open.left_at = None;
                open.files_deadline = None;
                open.highlight = None;
                let event = open_event(&self.config, open);
                let mut out = vec![Effect::Open(event), Effect::Log(log)];
                out.extend(self.update_highlight(x, y));
                out
            }
            DropEvent::Over { x, y } => {
                open.inside = true;
                open.left_at = None;
                self.update_highlight(x, y)
            }
            DropEvent::Leave => {
                open.inside = false;
                if open.ended_at.is_some() {
                    // A refused drop can bring its Leave after the
                    // button-up: the release fallback decides at the grace.
                    if release_entry(&self.config, open).is_some() {
                        return Vec::new();
                    }
                    return self.close(CloseReason::DragEnded, State::Idle);
                }
                if self.config.leave_ends_drag && open.cursor_on_monitor() {
                    // Maybe Esc, which ends the drag session but not the
                    // press. Keep the highlight in case it's a flicker or a
                    // refused drop.
                    open.left_at.get_or_insert(now);
                    return Vec::new();
                }
                open.clear_highlight()
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

/// Command or Control: macOS narrows the drag's allowed operations while
/// either is held, so AppKit may refuse the drop.
fn drop_may_be_refused(held: Modifiers) -> bool {
    held.meta || held.ctrl
}

/// Under `release_fallback`, the segment a button-up with no drop runs:
/// the highlighted one, when Command or Control was held at the release
/// and the wheel has its items. `None` means close as usual.
fn release_entry(config: &ControllerConfig, open: &OpenWheel) -> Option<(usize, WheelEntry)> {
    if !config.release_fallback || !drop_may_be_refused(open.held_at_release) {
        return None;
    }
    open.items.as_ref()?;
    let index = open.highlight?;
    let entry = open.content.as_ref()?.entries().get(index).copied()?;
    Some((index, entry))
}

/// The run for [`release_entry`]'s segment, on the wheel's items, and a log
/// line that says it ran on release.
fn release_run(config: &ControllerConfig, open: &OpenWheel) -> Option<(RunRequest, String)> {
    let (index, entry) = release_entry(config, open)?;
    let items = open.items.clone()?;
    let paths: Vec<String> = items
        .iter()
        .map(|item| item.path.display().to_string())
        .collect();
    let log = format!(
        "wheel {}: no drop arrived within {} ms of the button-up with {} held, so segment {index} ({entry:?}) ran on release; {} item(s): {}",
        open.seq,
        DROP_GRACE.as_millis(),
        held_names(open.held_at_release),
        paths.len(),
        paths.join(" | ")
    );
    let run = RunRequest {
        seq: open.seq,
        items,
        entry,
        anchor: open.anchor_global(),
        display: open.display,
    };
    Some((run, log))
}

/// The held keys' names joined with "+", for the log.
fn held_names(held: Modifiers) -> String {
    let names: Vec<&str> = [Key::Shift, Key::Ctrl, Key::Alt, Key::Meta]
        .into_iter()
        .filter(|key| key.is_held(held))
        .map(Key::name)
        .collect();
    names.join("+")
}

/// `true` if `rect` holds the point. The left and top edges are inside, the
/// right and bottom edges are not.
fn contains(rect: Rect, (x, y): (i32, i32)) -> bool {
    let (x, y) = (i64::from(x), i64::from(y));
    let (left, top) = (i64::from(rect.x), i64::from(rect.y));
    x >= left && x < left + i64::from(rect.width) && y >= top && y < top + i64::from(rect.height)
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
    /// How far the clock moves before each input.
    const STEP: Duration = Duration::from_millis(10);

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
            release_fallback: false,
            leave_ends_drag: false,
        }
    }

    /// The macOS keys and titles, with both macOS flags on.
    fn mac_config() -> ControllerConfig {
        ControllerConfig {
            keys: KeyMap::MACOS,
            platform: Platform::MacOs,
            release_fallback: true,
            leave_ends_drag: true,
            ..config()
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

        /// The default fake OS with `config`.
        fn with(config: ControllerConfig) -> Self {
            Self {
                ctl: WheelController::new(FakeEnv::at(0, 0, 1.0), config),
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
            self.now += STEP;
            let effects = self.ctl.handle(input, self.now);
            self.effects.extend(effects);
            self
        }

        /// `signal`, exactly `after` the last input (at least one [`STEP`]).
        fn signal_after(&mut self, after: Duration, signal: DragSignal) -> &mut Self {
            self.wait(after.saturating_sub(STEP));
            self.input(Input::Signal(signal))
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
    fn esc_before_the_wheel_keeps_it_shut_until_the_button_goes_up() {
        let mut run = Run::new();
        run.signals(vec![
            started(),
            moved(400, 300),
            DragSignal::Escape,
            keys(Modifiers::SHIFT),
        ]);
        assert!(run.opens().is_empty());
        assert!(matches!(run.ctl.state, State::Dismissed));
        // Nor does any other key or move later in the same press.
        run.signals(vec![
            moved(420, 310),
            keys(NONE),
            keys(Modifiers::ALT),
            DragSignal::Escape,
        ]);
        assert!(run.opens().is_empty());
        assert_eq!(run.shows(), 0);
        run.signals(vec![DragSignal::Ended]);
        assert!(matches!(run.ctl.state, State::Idle));
        assert!(run.closes().is_empty());
        // The next drag opens normally.
        run.signals(vec![started(), moved(10, 10), keys(Modifiers::SHIFT)]);
        assert_eq!(run.opens().len(), 1);
        assert_eq!(run.shows(), 1);
        assert!(run.ctl.is_open());
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

    // The macOS flags (`release_fallback`, `leave_ends_drag`).

    /// Both macOS flags on: drag, press `key` at (400, 300) and keep
    /// holding it, and the PNG enters on the top segment, which highlights.
    /// Command opens Tools (COMPRESS on top), Control opens Actions (Share
    /// on top), Shift opens Conversions (JPG on top).
    fn mac_wheel(key: Modifiers) -> Run {
        let mut run = Run::with(mac_config());
        run.signals(vec![started(), moved(400, 300), keys(key)]);
        run.drop_event(enter(png(), 400, 216));
        assert_eq!(run.highlights(), vec![Some(0)], "{:?}", run.effects);
        run
    }

    fn png_item() -> Vec<SelectionItem> {
        vec![SelectionItem::file(png()[0].clone())]
    }

    #[test]
    fn the_mac_flags_follow_the_os() {
        let config = ControllerConfig::for_this_os();
        let mac = cfg!(target_os = "macos");
        assert_eq!(
            (config.release_fallback, config.leave_ends_drag),
            (mac, mac)
        );
    }

    #[test]
    fn command_held_at_release_runs_the_highlight_when_no_drop_comes() {
        let mut run = mac_wheel(Modifiers::META);
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE / 2);
        assert!(run.closes().is_empty());
        assert!(run.runs().is_empty());
        run.wait(DROP_GRACE);
        // Exactly what a drop on the segment gives.
        let tail = &run.effects[run.effects.len().saturating_sub(5)..];
        assert!(
            matches!(
                tail,
                [
                    Effect::Close(WheelClose {
                        reason: CloseReason::Drop,
                        ..
                    }),
                    Effect::HideOverlay,
                    Effect::Log(_),
                    Effect::Log(_),
                    Effect::Run(_)
                ]
            ),
            "{tail:?}"
        );
        let runs = run.runs();
        let [request] = runs.as_slice() else {
            panic!("{:?}", run.effects);
        };
        assert_eq!(request.seq, 1);
        assert_eq!(
            request.entry,
            WheelEntry::Tool(grape_core::ToolKey::Compress)
        );
        assert_eq!(request.items, png_item());
        assert_eq!(request.anchor, (400, 300));
        assert_eq!(request.display.monitor.width, 1920);
        assert!(run
            .logs()
            .iter()
            .any(|line| line.contains("no drop arrived")
                && line.contains("ran on release")
                && line.contains("shot.png")));
        assert!(!run.ctl.is_open());
        // Idle: the next drag opens a wheel.
        run.signals(vec![started(), moved(10, 10), keys(Modifiers::SHIFT)]);
        assert_eq!(run.shows(), 2);
    }

    #[test]
    fn shift_held_at_release_with_no_drop_closes_drag_ended() {
        let mut run = mac_wheel(Modifiers::SHIFT);
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        assert!(run.runs().is_empty());
    }

    #[test]
    fn a_key_tapped_and_let_go_before_the_release_runs_nothing() {
        let mut run = mac_wheel(Modifiers::META);
        run.signals(vec![keys(NONE), DragSignal::Ended]);
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        assert!(run.runs().is_empty());
    }

    #[test]
    fn a_drop_within_the_grace_runs_once() {
        let mut run = mac_wheel(Modifiers::META);
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE / 2);
        run.drop_event(drop_at(png(), 400, 216));
        assert_eq!(run.closes(), vec![CloseReason::Drop]);
        assert_eq!(run.runs().len(), 1);
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.closes(), vec![CloseReason::Drop]);
        assert_eq!(run.runs().len(), 1);
    }

    #[test]
    fn no_highlight_means_no_run_on_release() {
        let mut run = Run::with(mac_config());
        run.signals(vec![started(), moved(400, 300), keys(Modifiers::META)]);
        // The dead zone.
        run.drop_event(enter(png(), 400, 300));
        assert!(run.highlights().is_empty());
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        assert!(run.runs().is_empty());
    }

    #[test]
    fn the_fallback_off_never_runs_on_release() {
        // Windows: Ctrl opens Tools and is still held at the release.
        let mut run = Run::new();
        run.signals(vec![started(), moved(400, 300), keys(Modifiers::CTRL)]);
        run.drop_event(enter(png(), 400, 216));
        assert_eq!(run.highlights(), vec![Some(0)]);
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        assert!(run.runs().is_empty());
    }

    #[test]
    fn a_leave_after_the_release_waits_for_the_fallback() {
        let mut run = mac_wheel(Modifiers::META);
        run.signals(vec![DragSignal::Ended]);
        run.drop_event(DropEvent::Leave);
        assert!(run.closes().is_empty());
        assert!(run.ctl.is_open());
        run.wait(DROP_GRACE);
        assert_eq!(run.closes(), vec![CloseReason::Drop]);
        assert_eq!(
            run.runs().first().map(|r| r.entry),
            Some(WheelEntry::Tool(grape_core::ToolKey::Compress))
        );
    }

    #[test]
    fn a_leave_after_a_shift_release_still_closes_at_once() {
        let mut run = mac_wheel(Modifiers::SHIFT);
        run.signals(vec![DragSignal::Ended]);
        run.drop_event(DropEvent::Leave);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        assert!(run.runs().is_empty());
    }

    #[test]
    fn a_leave_with_the_button_down_closes_as_esc_after_the_grace() {
        let mut run = mac_wheel(Modifiers::SHIFT);
        run.drop_event(DropEvent::Leave);
        // The highlight stays while it might be a flicker.
        assert_eq!(run.highlights(), vec![Some(0)]);
        assert_eq!(run.ctl.next_deadline(), Some(run.now + LEAVE_GRACE));
        // Small moves on the same display don't change that.
        run.signals(vec![moved(402, 217)]);
        run.wait(LEAVE_GRACE / 2);
        assert!(run.ctl.is_open());
        assert!(run.closes().is_empty());
        run.wait(LEAVE_GRACE);
        assert_eq!(run.closes(), vec![CloseReason::Escape]);
        assert_eq!(run.hides(), 1);
        assert!(!run.ctl.is_open());
        assert!(run.runs().is_empty());
        // Out of the way until the button goes up.
        let opens = run.opens().len();
        run.signals(vec![
            moved(500, 500),
            keys(NONE),
            keys(Modifiers::META),
            keys(NONE),
            keys(Modifiers::SHIFT),
        ]);
        assert_eq!(run.opens().len(), opens);
        assert_eq!(run.shows(), 1);
        run.signals(vec![DragSignal::Ended]);
        run.signals(vec![started(), moved(10, 10), keys(Modifiers::SHIFT)]);
        assert_eq!(run.shows(), 2);
    }

    #[test]
    fn leave_then_over_cancels_the_pending_close() {
        let mut run = mac_wheel(Modifiers::SHIFT);
        run.drop_event(DropEvent::Leave);
        run.drop_event(DropEvent::Over { x: 400, y: 216 });
        assert_eq!(run.ctl.next_deadline(), Some(run.now + WATCHDOG));
        run.wait(LEAVE_GRACE * 3);
        assert!(run.ctl.is_open());
        assert!(run.closes().is_empty());
        assert_eq!(run.highlights(), vec![Some(0)]);
    }

    #[test]
    fn leave_then_enter_cancels_the_pending_close() {
        let mut run = mac_wheel(Modifiers::SHIFT);
        run.drop_event(DropEvent::Leave);
        run.drop_event(enter(png(), 400, 216));
        run.wait(LEAVE_GRACE * 3);
        assert!(run.ctl.is_open());
        assert!(run.closes().is_empty());
    }

    #[test]
    fn leave_then_ended_with_control_held_runs_the_highlight() {
        let mut run = mac_wheel(Modifiers::CTRL);
        run.drop_event(DropEvent::Leave);
        run.signals(vec![DragSignal::Ended]);
        // The release replaced the Esc deadline with the drop grace.
        run.wait(LEAVE_GRACE * 2);
        assert!(run.closes().is_empty());
        run.wait(DROP_GRACE);
        assert_eq!(run.closes(), vec![CloseReason::Drop]);
        let runs = run.runs();
        let [request] = runs.as_slice() else {
            panic!("{:?}", run.effects);
        };
        assert_eq!(request.entry, WheelEntry::Action(ActionKey::Share));
        assert_eq!(request.items, png_item());
        assert!(!run.ctl.is_open());
        run.signals(vec![started(), moved(10, 10), keys(Modifiers::SHIFT)]);
        assert_eq!(run.shows(), 2);
    }

    #[test]
    fn esc_then_a_release_within_the_leave_grace_runs_nothing() {
        // On macOS Esc is a Leave with the button still down. The button
        // goes up 100 ms later, before LEAVE_GRACE takes it as Esc.
        for key in [Modifiers::CTRL, Modifiers::META] {
            let mut run = mac_wheel(key);
            run.drop_event(DropEvent::Leave);
            run.signal_after(Duration::from_millis(100), DragSignal::Ended);
            assert_eq!(run.closes(), vec![CloseReason::DragEnded], "{key:?}");
            assert_eq!(run.hides(), 1);
            assert!(!run.ctl.is_open());
            assert_eq!(run.ctl.next_deadline(), None);
            assert!(
                run.logs()
                    .iter()
                    .any(|line| line.contains("100 ms after a Leave")
                        && line.contains("nothing runs"))
            );
            run.wait(DROP_GRACE * 2);
            assert!(run.runs().is_empty(), "{key:?}: {:?}", run.effects);
            assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
            // Idle: the next drag opens a wheel.
            run.signals(vec![started(), moved(10, 10), keys(Modifiers::SHIFT)]);
            assert_eq!(run.shows(), 2);
        }
    }

    #[test]
    fn a_release_20_ms_after_a_leave_with_control_held_runs_once() {
        let mut run = mac_wheel(Modifiers::CTRL);
        run.drop_event(DropEvent::Leave);
        run.signal_after(Duration::from_millis(20), DragSignal::Ended);
        run.wait(DROP_GRACE / 2);
        assert!(run.closes().is_empty());
        assert!(run.runs().is_empty());
        run.wait(DROP_GRACE);
        assert_eq!(run.closes(), vec![CloseReason::Drop]);
        let runs = run.runs();
        let [request] = runs.as_slice() else {
            panic!("{:?}", run.effects);
        };
        assert_eq!(request.entry, WheelEntry::Action(ActionKey::Share));
        assert_eq!(request.items, png_item());
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.runs().len(), 1);
        assert_eq!(run.closes(), vec![CloseReason::Drop]);
    }

    #[test]
    fn the_refused_leave_window_includes_its_end() {
        let mut run = mac_wheel(Modifiers::META);
        run.drop_event(DropEvent::Leave);
        run.signal_after(REFUSED_LEAVE_WINDOW, DragSignal::Ended);
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.runs().len(), 1);
        let mut run = mac_wheel(Modifiers::META);
        run.drop_event(DropEvent::Leave);
        run.signal_after(
            REFUSED_LEAVE_WINDOW + Duration::from_millis(1),
            DragSignal::Ended,
        );
        run.wait(DROP_GRACE * 2);
        assert!(run.runs().is_empty());
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
    }

    #[test]
    fn on_windows_a_release_after_a_leave_closes_at_once() {
        // Both flags off: a Leave only clears the highlight, and the button
        // going up outside the overlay closes at once, however soon.
        for after in [Duration::from_millis(20), Duration::from_millis(100)] {
            let mut run = Run::new();
            run.signals(vec![started(), moved(400, 300), keys(Modifiers::CTRL)]);
            run.drop_event(enter(png(), 400, 216));
            run.drop_event(DropEvent::Leave);
            assert_eq!(run.highlights(), vec![Some(0), None]);
            run.signal_after(after, DragSignal::Ended);
            assert_eq!(run.closes(), vec![CloseReason::DragEnded], "{after:?}");
            assert!(run
                .logs()
                .iter()
                .all(|line| !line.contains("after a Leave")));
            run.wait(DROP_GRACE * 2);
            assert!(run.runs().is_empty());
            assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        }
    }

    #[test]
    fn leave_then_ended_with_shift_held_closes_drag_ended() {
        let mut run = mac_wheel(Modifiers::SHIFT);
        run.drop_event(DropEvent::Leave);
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        assert!(run.runs().is_empty());
    }

    #[test]
    fn leave_then_ended_without_the_fallback_closes_at_once() {
        let mut run = Run::with(ControllerConfig {
            release_fallback: false,
            ..mac_config()
        });
        run.signals(vec![started(), moved(400, 300), keys(Modifiers::CTRL)]);
        run.drop_event(enter(png(), 400, 216));
        run.drop_event(DropEvent::Leave);
        assert!(run.closes().is_empty());
        run.signals(vec![DragSignal::Ended]);
        assert_eq!(run.closes(), vec![CloseReason::DragEnded]);
        assert!(run.runs().is_empty());
    }

    #[test]
    fn a_leave_with_the_cursor_on_another_monitor_keeps_the_wheel() {
        let mut run = mac_wheel(Modifiers::SHIFT);
        // The fake monitor is 1920 wide at (0, 0).
        run.signals(vec![moved(2500, 300)]);
        run.drop_event(DropEvent::Leave);
        assert_eq!(run.highlights(), vec![Some(0), None]);
        run.wait(LEAVE_GRACE * 3);
        assert!(run.ctl.is_open());
        assert!(run.closes().is_empty());
    }

    #[test]
    fn moving_to_another_monitor_after_a_leave_keeps_the_wheel() {
        let mut run = mac_wheel(Modifiers::SHIFT);
        run.drop_event(DropEvent::Leave);
        run.signals(vec![moved(2500, 300)]);
        assert_eq!(run.highlights(), vec![Some(0), None]);
        run.wait(LEAVE_GRACE * 3);
        assert!(run.ctl.is_open());
        assert!(run.closes().is_empty());
    }

    #[test]
    fn leave_ends_drag_off_keeps_todays_leave() {
        let mut run = Run::with(ControllerConfig {
            leave_ends_drag: false,
            ..mac_config()
        });
        run.signals(vec![started(), moved(400, 300), keys(Modifiers::SHIFT)]);
        run.drop_event(enter(png(), 400, 216));
        run.drop_event(DropEvent::Leave);
        assert_eq!(run.highlights(), vec![Some(0), None]);
        run.wait(LEAVE_GRACE * 3);
        assert!(run.ctl.is_open());
    }

    #[test]
    fn rect_contains_its_top_left_edges_only() {
        let rect = Rect {
            x: -1440,
            y: -900,
            width: 1440,
            height: 900,
        };
        assert!(contains(rect, (-1440, -900)));
        assert!(contains(rect, (-1, -1)));
        assert!(!contains(rect, (0, -1)));
        assert!(!contains(rect, (-1, 0)));
        assert!(!contains(rect, (-1441, -500)));
        let far = Rect {
            x: i32::MAX - 1,
            y: 0,
            width: 10,
            height: 10,
        };
        assert!(contains(far, (i32::MAX, 5)));
    }

    #[test]
    fn held_names_list_every_held_key() {
        let held = Modifiers {
            shift: true,
            meta: true,
            ..NONE
        };
        let expected = format!("Shift+{}", Key::Meta.name());
        assert_eq!(held_names(held), expected);
        assert_eq!(held_names(NONE), "");
    }
}
