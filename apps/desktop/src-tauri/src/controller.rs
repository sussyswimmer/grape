//! The wheel controller: `docs/INTERACTION_SPEC.md` §1 as a pure state
//! machine.
//!
//! Inputs are drag signals from `grape-platform`'s monitor, events from the
//! overlay's drop target, and a few app events. Outputs are [`Effect`]s the
//! app applies (show the overlay, highlight, close, log). Time comes in as
//! an argument, so tests drive it with scripted signals and no OS.
//!
//! M1 draws a placeholder ring of [`PLACEHOLDER_SEGMENTS`] segments and
//! logs where a drop landed. M3 swaps in the real wheel contents.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use grape_core::{clamp_anchor, hit_test, Point, WheelMode};
use grape_platform::{DisplayBounds, DragSignal, DropEvent, Modifiers, Rect};

/// Segments in the M1 placeholder ring.
pub const PLACEHOLDER_SEGMENTS: usize = 6;
/// Hide the overlay if nothing happens for this long while it's open
/// (`docs/INTERACTION_SPEC.md` §9).
pub const WATCHDOG: Duration = Duration::from_secs(30);
/// After the overlay opens, a drag that brings no file paths this long
/// after the next pointer move isn't a file drag: close the wheel
/// (`docs/PLATFORM_WINDOWS.md` → "Is it a file drag?").
pub const NO_FILES_GRACE: Duration = Duration::from_millis(300);
/// After the button goes up, how long to wait for the overlay's drop to
/// arrive (it comes from another thread) before closing anyway.
pub const DROP_GRACE: Duration = Duration::from_millis(500);

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

/// Looks up the display under a point. The app uses
/// `grape_platform::display_bounds_at`; tests use a fixed screen.
pub trait Screens {
    /// The display under the global physical point (`x`, `y`).
    fn bounds_at(&self, x: i32, y: i32) -> Option<DisplayBounds>;
}

impl<F: Fn(i32, i32) -> Option<DisplayBounds>> Screens for F {
    fn bounds_at(&self, x: i32, y: i32) -> Option<DisplayBounds> {
        self(x, y)
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

/// What the overlay should show, sent with `wheel://open`.
#[derive(Debug, Clone, PartialEq)]
pub struct WheelView {
    /// Increases with every open or swap, so late events can be matched.
    pub seq: u64,
    /// Which wheel.
    pub mode: WheelMode,
    /// The overlay's frame: the whole monitor, global physical pixels.
    pub monitor: Rect,
    /// Physical pixels per logical pixel on that monitor.
    pub scale: f64,
    /// The wheel's center in logical pixels from the overlay's top-left,
    /// already clamped to the work area.
    pub anchor: Point,
    /// How many segments to draw.
    pub count: usize,
}

/// Where a drop landed, for the log (and, from M3, the job runner).
#[derive(Debug, Clone, PartialEq)]
pub struct DropReport {
    /// The dropped files and folders.
    pub paths: Vec<PathBuf>,
    /// The wheel that was open.
    pub mode: WheelMode,
    /// The segment under the drop, or `None` for a cancel.
    pub segment: Option<usize>,
    /// The drop point in logical pixels from the anchor (y down).
    pub offset: Point,
}

impl fmt::Display for DropReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = match self.segment {
            Some(index) => format!("segment {index}"),
            None => "no segment (cancel)".to_owned(),
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

/// What the app must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Size the overlay to the monitor, draw the wheel and show it.
    Open(WheelView),
    /// Redraw the open wheel in place with another mode.
    Swap(WheelView),
    /// Highlight a segment, or none.
    Highlight(Option<usize>),
    /// Hide the overlay.
    Close,
    /// A drop landed on the open wheel.
    Dropped(DropReport),
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
    view: WheelView,
    held: Modifiers,
    highlight: Option<usize>,
    /// From the drop target's Enter.
    paths: Option<Vec<PathBuf>>,
    /// Set by the first pointer move after opening, cleared when files arrive.
    files_deadline: Option<Instant>,
    /// When the button went up, if it has.
    ended_at: Option<Instant>,
    /// The last signal or drop event, for the watchdog.
    last_event: Instant,
    /// When the key that opened (or swapped) the wheel arrived.
    key_at: Instant,
}

/// The state machine. See the module docs.
#[derive(Debug)]
pub struct WheelController<S> {
    screens: S,
    keys: KeyMap,
    state: State,
    paused: bool,
    seq: u64,
}

impl<S: Screens> WheelController<S> {
    /// A controller using `screens` for display bounds and `keys` for the
    /// key assignment.
    pub fn new(screens: S, keys: KeyMap) -> Self {
        Self {
            screens,
            keys,
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
            ("the drag ended without a drop on the overlay", State::Idle)
        } else if open.files_deadline.is_some_and(|at| now >= at) {
            ("no files arrived: not a file drag", State::Dismissed)
        } else if now >= open.last_event + WATCHDOG {
            ("watchdog: no drag events for 30 s", State::Idle)
        } else {
            return Vec::new();
        };
        self.state = next;
        vec![
            Effect::Close,
            Effect::Log(format!("wheel closed: {reason}")),
        ]
    }

    fn on_signal(&mut self, signal: DragSignal, now: Instant) -> Vec<Effect> {
        match signal {
            DragSignal::Started { modifiers } => {
                let mut out = Vec::new();
                if self.is_open() {
                    out.push(Effect::Close);
                }
                self.state = if self.paused {
                    State::Dismissed
                } else {
                    State::Dragging {
                        held: modifiers,
                        cursor: None,
                    }
                };
                out
            }
            DragSignal::Moved { x, y } => {
                match &mut self.state {
                    State::Dragging { cursor, .. } => *cursor = Some((x, y)),
                    State::Open(open) => {
                        open.last_event = now;
                        if open.paths.is_none() && open.files_deadline.is_none() {
                            open.files_deadline = Some(now + NO_FILES_GRACE);
                        }
                    }
                    State::Idle | State::Dismissed => {}
                }
                Vec::new()
            }
            DragSignal::Modifiers(held) => self.on_modifiers(held, now),
            DragSignal::Escape => match self.state {
                State::Open(_) => {
                    self.state = State::Dismissed;
                    vec![Effect::Close, Effect::Log("wheel closed: Esc".to_owned())]
                }
                _ => Vec::new(),
            },
            DragSignal::Ended => match &mut self.state {
                State::Open(open) => {
                    open.last_event = now;
                    open.ended_at.get_or_insert(now);
                    Vec::new()
                }
                _ => {
                    self.state = State::Idle;
                    Vec::new()
                }
            },
        }
    }

    fn on_modifiers(&mut self, held: Modifiers, now: Instant) -> Vec<Effect> {
        let mode = self.keys.mode_for(held);
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
                    Some(mode) if mode != open.view.mode => {
                        self.seq += 1;
                        open.view.seq = self.seq;
                        open.view.mode = mode;
                        open.highlight = None;
                        open.key_at = now;
                        vec![
                            Effect::Swap(open.view.clone()),
                            Effect::Log(format!("wheel swapped to {mode:?}")),
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
        let Some(bounds) = self.screens.bounds_at(x, y) else {
            return vec![Effect::Log(format!(
                "wheel not opened: no display found under ({x}, {y})"
            ))];
        };
        let scale = sane_scale(bounds.scale);
        let monitor = bounds.monitor;
        let to_logical = |px: i32, origin: i32| f64::from(px - origin) / scale;
        let cursor = Point {
            x: to_logical(x, monitor.x),
            y: to_logical(y, monitor.y),
        };
        let work_area = grape_core::Rect {
            x: to_logical(bounds.work_area.x, monitor.x),
            y: to_logical(bounds.work_area.y, monitor.y),
            width: f64::from(bounds.work_area.width) / scale,
            height: f64::from(bounds.work_area.height) / scale,
        };
        self.seq += 1;
        let view = WheelView {
            seq: self.seq,
            mode,
            monitor,
            scale,
            anchor: clamp_anchor(cursor, work_area),
            count: PLACEHOLDER_SEGMENTS,
        };
        let log = format!(
            "wheel open: {mode:?} at ({x}, {y}) on a {}×{} monitor at {:.0} % scale; anchor ({:.0}, {:.0})",
            monitor.width,
            monitor.height,
            scale * 100.0,
            view.anchor.x,
            view.anchor.y
        );
        self.state = State::Open(Box::new(OpenWheel {
            view: view.clone(),
            held,
            highlight: None,
            paths: None,
            files_deadline: None,
            ended_at: None,
            last_event: now,
            key_at: now,
        }));
        vec![Effect::Open(view), Effect::Log(log)]
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
                    self.state = State::Dismissed;
                    return vec![
                        Effect::Close,
                        Effect::Log("wheel closed: the drag carries no files".to_owned()),
                    ];
                }
                let log = format!("drag entered the overlay with {} item(s)", paths.len());
                open.paths = Some(paths);
                open.files_deadline = None;
                let mut out = vec![Effect::Log(log)];
                out.extend(set_highlight(open, x, y));
                out
            }
            DropEvent::Over { x, y } => set_highlight(open, x, y),
            DropEvent::Leave => {
                if open.highlight.take().is_some() {
                    vec![Effect::Highlight(None)]
                } else {
                    Vec::new()
                }
            }
            DropEvent::Drop { paths, x, y } => {
                let offset = offset_from_anchor(&open.view, x, y);
                let segment = hit_test(offset.x, offset.y, open.view.count);
                let report = DropReport {
                    paths,
                    mode: open.view.mode,
                    segment,
                    offset,
                };
                self.state = if open.ended_at.is_some() {
                    State::Idle
                } else {
                    State::Dismissed
                };
                vec![Effect::Close, Effect::Dropped(report)]
            }
        }
    }

    fn on_painted(&mut self, seq: u64, now: Instant) -> Vec<Effect> {
        match &self.state {
            State::Open(open) if open.view.seq == seq => {
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
        let was_open = self.is_open();
        self.state = match self.state {
            State::Idle => State::Idle,
            _ => State::Dismissed,
        };
        if was_open {
            vec![Effect::Close]
        } else {
            Vec::new()
        }
    }
}

/// The drop target's global physical point as a logical offset from the
/// wheel's anchor (y down), which is what `hit_test` takes.
fn offset_from_anchor(view: &WheelView, x: i32, y: i32) -> Point {
    Point {
        x: f64::from(x - view.monitor.x) / view.scale - view.anchor.x,
        y: f64::from(y - view.monitor.y) / view.scale - view.anchor.y,
    }
}

fn set_highlight(open: &mut OpenWheel, x: i32, y: i32) -> Vec<Effect> {
    let offset = offset_from_anchor(&open.view, x, y);
    let index = hit_test(offset.x, offset.y, open.view.count);
    if index == open.highlight {
        return Vec::new();
    }
    open.highlight = index;
    vec![Effect::Highlight(index)]
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
    use grape_platform::mock::scripts;

    const NONE: Modifiers = Modifiers::NONE;

    /// One 1920×1080 monitor at `scale`, offset to (`x`, `y`), with a 48 px
    /// taskbar at the bottom.
    fn screen(x: i32, y: i32, scale: f64) -> impl Fn(i32, i32) -> Option<DisplayBounds> {
        move |_, _| {
            Some(DisplayBounds {
                monitor: Rect {
                    x,
                    y,
                    width: 1920,
                    height: 1080,
                },
                work_area: Rect {
                    x,
                    y,
                    width: 1920,
                    height: 1032,
                },
                scale,
            })
        }
    }

    type Ctl = WheelController<Box<dyn Fn(i32, i32) -> Option<DisplayBounds>>>;

    fn controller() -> Ctl {
        WheelController::new(Box::new(screen(0, 0, 1.0)), KeyMap::WINDOWS)
    }

    struct Run {
        ctl: Ctl,
        now: Instant,
        effects: Vec<Effect>,
    }

    impl Run {
        fn new() -> Self {
            Self {
                ctl: controller(),
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

        fn opens(&self) -> Vec<&WheelView> {
            self.effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Open(view) => Some(view),
                    _ => None,
                })
                .collect()
        }

        fn swaps(&self) -> Vec<WheelMode> {
            self.effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Swap(view) => Some(view.mode),
                    _ => None,
                })
                .collect()
        }

        fn closes(&self) -> usize {
            self.effects
                .iter()
                .filter(|e| matches!(e, Effect::Close))
                .count()
        }

        fn highlights(&self) -> Vec<Option<usize>> {
            self.effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Highlight(index) => Some(*index),
                    _ => None,
                })
                .collect()
        }

        fn drops(&self) -> Vec<&DropReport> {
            self.effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Dropped(report) => Some(report),
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

    fn file() -> Vec<PathBuf> {
        vec![PathBuf::from(r"C:\Users\max\Pictures\shot.png")]
    }

    /// Drag, tap Shift at (400, 300): the wheel is open.
    fn open_at_400_300() -> Run {
        let mut run = Run::new();
        run.signals(vec![started(), moved(400, 300), keys(Modifiers::SHIFT)]);
        run
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
    fn a_plain_drag_never_opens_the_wheel() {
        let mut run = Run::new();
        run.signals(scripts::plain_drag());
        assert!(run.effects.is_empty(), "{:?}", run.effects);
        assert!(!run.ctl.is_open());
    }

    #[test]
    fn tapping_shift_opens_conversions_at_the_cursor() {
        let mut run = Run::new();
        run.signals(scripts::shift_tap());
        let opens = run.opens();
        let [view] = opens.as_slice() else {
            panic!("{:?}", run.effects);
        };
        assert_eq!(view.mode, WheelMode::Conversions);
        assert_eq!(view.anchor, Point { x: 400.0, y: 300.0 });
        assert_eq!(view.count, PLACEHOLDER_SEGMENTS);
        // Releasing the key keeps the wheel open; the button-up waits for
        // the drop.
        assert!(run.ctl.is_open());
        assert_eq!(run.closes(), 0);
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
        // Release and press again during the drag: now it opens.
        run.signals(vec![keys(NONE), keys(Modifiers::SHIFT)]);
        assert_eq!(run.opens().len(), 1);
    }

    #[test]
    fn the_scripted_held_shift_flow_opens_once() {
        let mut run = Run::new();
        run.signals(scripts::shift_held_from_start());
        assert_eq!(run.opens().len(), 1);
    }

    #[test]
    fn two_modifiers_mean_no_mode() {
        let mut run = Run::new();
        run.signals(scripts::two_modifiers());
        assert!(run.opens().is_empty());
    }

    #[test]
    fn another_key_swaps_the_wheel_in_place() {
        let mut run = Run::new();
        run.signals(scripts::key_swap());
        assert_eq!(run.opens().len(), 1);
        assert_eq!(run.swaps(), vec![WheelMode::Tools]);
        let Some(Effect::Swap(view)) = run.effects.iter().find(|e| matches!(e, Effect::Swap(_)))
        else {
            panic!("no swap");
        };
        assert_eq!(view.anchor, Point { x: 400.0, y: 300.0 });
        assert!(view.seq > 1);
    }

    #[test]
    fn the_same_key_again_does_not_swap() {
        let mut run = open_at_400_300();
        run.signals(vec![keys(NONE), keys(Modifiers::SHIFT)]);
        assert!(run.swaps().is_empty());
    }

    #[test]
    fn two_keys_while_open_change_nothing() {
        let mut run = open_at_400_300();
        run.signals(vec![keys(Modifiers {
            shift: true,
            alt: true,
            ..NONE
        })]);
        assert!(run.swaps().is_empty());
        assert!(run.ctl.is_open());
    }

    #[test]
    fn esc_closes_the_wheel_for_the_rest_of_the_drag() {
        let mut run = Run::new();
        run.signals(scripts::escape());
        assert_eq!(run.opens().len(), 1);
        assert_eq!(run.closes(), 1);
        assert!(!run.ctl.is_open());
        assert!(run.logs().contains(&"wheel closed: Esc"));
        // A key tap after Esc in the same drag doesn't reopen it.
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
        let mut run = open_at_400_300();
        run.drop_event(DropEvent::Enter {
            paths: file(),
            x: 400,
            y: 300,
        });
        // 84 px above the anchor: segment 0, the top one.
        run.drop_event(DropEvent::Over { x: 400, y: 216 });
        run.drop_event(DropEvent::Over { x: 401, y: 216 });
        // The dead zone.
        run.drop_event(DropEvent::Over { x: 410, y: 300 });
        assert_eq!(run.highlights(), vec![Some(0), None]);
    }

    #[test]
    fn a_drop_on_the_top_segment_reports_the_path() {
        let mut run = open_at_400_300();
        run.drop_event(DropEvent::Enter {
            paths: file(),
            x: 400,
            y: 300,
        });
        run.signals(vec![DragSignal::Ended]);
        run.drop_event(DropEvent::Drop {
            paths: file(),
            x: 400,
            y: 216,
        });
        let drops = run.drops();
        let [report] = drops.as_slice() else {
            panic!("{:?}", run.effects);
        };
        assert_eq!(report.segment, Some(0));
        assert_eq!(report.mode, WheelMode::Conversions);
        assert_eq!(report.paths, file());
        assert!(report.to_string().contains(r"shot.png"), "{report}");
        assert!(!run.ctl.is_open());
        assert_eq!(run.closes(), 1);
    }

    #[test]
    fn a_drop_off_the_ring_is_a_cancel() {
        let mut run = open_at_400_300();
        run.drop_event(DropEvent::Enter {
            paths: file(),
            x: 400,
            y: 300,
        });
        run.drop_event(DropEvent::Drop {
            paths: file(),
            x: 900,
            y: 700,
        });
        let drops = run.drops();
        assert_eq!(drops.first().map(|r| r.segment), Some(None));
        assert!(drops
            .first()
            .is_some_and(|r| r.to_string().contains("cancel")));
    }

    #[test]
    fn the_drop_can_arrive_before_or_after_the_button_up() {
        // Drop first, then Ended: back to idle, no second close.
        let mut run = open_at_400_300();
        run.drop_event(DropEvent::Enter {
            paths: file(),
            x: 400,
            y: 300,
        });
        run.drop_event(DropEvent::Drop {
            paths: file(),
            x: 400,
            y: 216,
        });
        run.signals(vec![DragSignal::Ended]);
        run.wait(DROP_GRACE * 2);
        assert_eq!(run.closes(), 1);
        assert_eq!(run.drops().len(), 1);
        // The next drag works normally.
        run.signals(vec![started(), moved(10, 10), keys(Modifiers::ALT)]);
        assert_eq!(run.opens().len(), 2);
    }

    #[test]
    fn a_drag_that_ends_elsewhere_closes_after_the_grace() {
        let mut run = open_at_400_300();
        run.drop_event(DropEvent::Enter {
            paths: file(),
            x: 400,
            y: 300,
        });
        run.signals(vec![DragSignal::Ended]);
        assert_eq!(run.closes(), 0);
        run.wait(DROP_GRACE / 2);
        assert_eq!(run.closes(), 0);
        run.wait(DROP_GRACE);
        assert_eq!(run.closes(), 1);
        assert!(run.drops().is_empty());
        assert!(!run.ctl.is_open());
    }

    #[test]
    fn a_drag_without_files_closes_the_wheel() {
        let mut run = open_at_400_300();
        run.drop_event(DropEvent::Enter {
            paths: Vec::new(),
            x: 400,
            y: 300,
        });
        assert_eq!(run.closes(), 1);
        assert!(!run.ctl.is_open());
        // It stays closed for the rest of this drag.
        run.signals(vec![keys(NONE), keys(Modifiers::SHIFT)]);
        assert_eq!(run.opens().len(), 1);
    }

    #[test]
    fn no_enter_after_a_move_means_no_file_drag() {
        let mut run = open_at_400_300();
        // No deadline until the pointer moves.
        run.wait(NO_FILES_GRACE * 3);
        assert!(run.ctl.is_open());
        run.signals(vec![moved(410, 300)]);
        run.wait(NO_FILES_GRACE / 2);
        assert!(run.ctl.is_open());
        run.wait(NO_FILES_GRACE);
        assert!(!run.ctl.is_open());
        assert!(run
            .logs()
            .iter()
            .any(|line| line.contains("not a file drag")));
    }

    #[test]
    fn files_arriving_cancel_the_no_files_deadline() {
        let mut run = open_at_400_300();
        run.signals(vec![moved(410, 300)]);
        run.drop_event(DropEvent::Enter {
            paths: file(),
            x: 410,
            y: 300,
        });
        run.wait(NO_FILES_GRACE * 3);
        assert!(run.ctl.is_open());
    }

    #[test]
    fn the_watchdog_hides_a_stuck_overlay() {
        let mut run = open_at_400_300();
        run.drop_event(DropEvent::Enter {
            paths: file(),
            x: 400,
            y: 300,
        });
        assert_eq!(
            run.ctl.next_deadline(),
            Some(run.now + WATCHDOG),
            "the watchdog is the only deadline"
        );
        run.wait(WATCHDOG - Duration::from_secs(1));
        assert!(run.ctl.is_open());
        run.wait(Duration::from_secs(2));
        assert!(!run.ctl.is_open());
        assert!(run.logs().iter().any(|line| line.contains("watchdog")));
    }

    #[test]
    fn leave_clears_the_highlight_but_keeps_the_wheel() {
        let mut run = open_at_400_300();
        run.drop_event(DropEvent::Enter {
            paths: file(),
            x: 400,
            y: 216,
        });
        run.drop_event(DropEvent::Leave);
        assert_eq!(run.highlights(), vec![Some(0), None]);
        assert!(run.ctl.is_open());
    }

    #[test]
    fn pause_closes_the_wheel_and_ignores_drags() {
        let mut run = open_at_400_300();
        run.input(Input::SetPaused(true));
        assert_eq!(run.closes(), 1);
        run.signals(vec![DragSignal::Ended]);
        run.signals(scripts::shift_tap());
        assert_eq!(run.opens().len(), 1);
        run.input(Input::SetPaused(false));
        run.signals(scripts::shift_tap());
        assert_eq!(run.opens().len(), 2);
    }

    #[test]
    fn drop_events_with_the_wheel_closed_are_ignored() {
        let mut run = Run::new();
        run.drop_event(DropEvent::Drop {
            paths: file(),
            x: 0,
            y: 0,
        });
        assert!(run.drops().is_empty());
        assert_eq!(run.logs().len(), 1);
    }

    #[test]
    fn a_missed_button_up_closes_the_wheel_at_the_next_drag() {
        let mut run = open_at_400_300();
        run.signals(vec![started()]);
        assert_eq!(run.closes(), 1);
        assert!(!run.ctl.is_open());
    }

    #[test]
    fn painted_logs_the_latency_for_the_current_wheel() {
        let mut run = open_at_400_300();
        let seq = run.opens().first().map(|v| v.seq).unwrap_or_default();
        run.input(Input::Painted { seq: seq + 7 });
        assert!(run.logs().iter().all(|line| !line.contains("painted")));
        run.input(Input::Painted { seq });
        assert!(run
            .logs()
            .iter()
            .any(|line| line.contains("painted 20 ms after the key")));
    }

    #[test]
    fn the_anchor_is_logical_and_clamped_on_a_scaled_offset_monitor() {
        // A 150 % monitor to the left of the primary: physical x -1920..0.
        let mut ctl: Ctl = WheelController::new(Box::new(screen(-1920, 0, 1.5)), KeyMap::WINDOWS);
        let now = Instant::now();
        for signal in [started(), moved(-1915, 1075), keys(Modifiers::SHIFT)] {
            let effects = ctl.handle(Input::Signal(signal), now);
            if let Some(Effect::Open(view)) = effects.first() {
                assert_eq!(view.monitor.x, -1920);
                assert!((view.scale - 1.5).abs() < 1e-9);
                // The cursor is 5 / 1.5 ≈ 3 logical px from the left edge
                // and inside the taskbar, so both axes clamp by 140.
                assert!((view.anchor.x - 140.0).abs() < 1e-9, "{:?}", view.anchor);
                assert!(
                    (view.anchor.y - (1032.0 / 1.5 - 140.0)).abs() < 1e-9,
                    "{:?}",
                    view.anchor
                );
                return;
            }
        }
        panic!("the wheel never opened");
    }

    #[test]
    fn hit_tests_use_logical_offsets_on_scaled_monitors() {
        let mut ctl: Ctl = WheelController::new(Box::new(screen(0, 0, 2.0)), KeyMap::WINDOWS);
        let now = Instant::now();
        for signal in [started(), moved(800, 600), keys(Modifiers::SHIFT)] {
            ctl.handle(Input::Signal(signal), now);
        }
        // 84 logical px above the anchor is 168 physical px.
        let effects = ctl.handle(
            Input::Drop(DropEvent::Enter {
                paths: file(),
                x: 800,
                y: 600 - 168,
            }),
            now,
        );
        assert!(effects.contains(&Effect::Highlight(Some(0))), "{effects:?}");
    }

    #[test]
    fn no_display_means_no_wheel() {
        let mut ctl = WheelController::new(|_: i32, _: i32| None, KeyMap::WINDOWS);
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
