//! The open Shortcuts wheel's state (`docs/design/m10-shortcuts-wheel.md` §5).
//!
//! Pure: no clock, no OS and no window. [`crate::controller`] owns the
//! state machine and its effects; this module holds one open wheel and
//! answers each input with a [`Decision`] the controller carries out.
//!
//! - [`WheelPlace`] is the anchor math every wheel uses: where the wheel
//!   sits on its display, logical offsets from the anchor, the hit test and
//!   the anchor in global coordinates.
//! - [`Session`] is one open Shortcuts wheel: the ring picked for the app
//!   in front, the open groups, the highlight, and the tap, click and
//!   paint rules.
//!
//! Nothing here numbers a wheel or names a segment in a log: the controller
//! hands the `seq` in, and a pick is reported by its index.

use std::fmt;
use std::time::{Duration, Instant};

use grape_core::{clamp_anchor, hit_test, Point, GEOMETRY};
use grape_platform::shortcuts::{FrontApp, HotkeyId, HotkeySignal, PointerSignal};
use grape_platform::DisplayBounds;

use crate::events::{CloseReason, UiContent};
use crate::shortcuts::model::{
    menu_for, runnable, OpensAt, RingItem, RingKind, ShortcutRun, Snapshot,
};

/// A hot key let go sooner than this after it went down was a tap: the
/// wheel stays open and waits for a click.
pub const TAP: Duration = Duration::from_millis(300);
/// How long an open wheel may go without the overlay reporting it painted.
/// An undrawn wheel must not hold Esc, Return and the screen.
pub const PAINT_DEADLINE: Duration = Duration::from_secs(1);

/// A usable scale factor: anything not finite or not positive counts as 1.
pub fn sane_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// Where a wheel sits on its display. Global positions are physical pixels
/// on Windows and points on macOS; everything the hit test sees is logical.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WheelPlace {
    /// The display the wheel is on.
    pub display: DisplayBounds,
    /// `display.scale`, made safe to divide by.
    pub scale: f64,
    /// The wheel's center, in logical pixels from the monitor's top-left.
    pub anchor: Point,
}

impl WheelPlace {
    /// A wheel on `display` at the global point, moved in from the work
    /// area's edges so the whole wheel shows.
    pub fn at(display: DisplayBounds, (x, y): (i32, i32)) -> Self {
        let scale = sane_scale(display.scale);
        let monitor = display.monitor;
        let to_logical = |px: i32, origin: i32| f64::from(px - origin) / scale;
        let point = Point {
            x: to_logical(x, monitor.x),
            y: to_logical(y, monitor.y),
        };
        Self {
            display,
            scale,
            anchor: clamp_anchor(point, work_area(display, scale)),
        }
    }

    /// A wheel in the middle of `display`'s work area.
    pub fn centered(display: DisplayBounds) -> Self {
        let scale = sane_scale(display.scale);
        let area = work_area(display, scale);
        let middle = Point {
            x: area.x + area.width / 2.0,
            y: area.y + area.height / 2.0,
        };
        Self {
            display,
            scale,
            anchor: clamp_anchor(middle, area),
        }
    }

    /// A global point as a logical offset from the anchor (y
    /// down), which is what `hit_test` takes.
    pub fn offset(&self, x: i32, y: i32) -> Point {
        let monitor = self.display.monitor;
        Point {
            x: f64::from(x - monitor.x) / self.scale - self.anchor.x,
            y: f64::from(y - monitor.y) / self.scale - self.anchor.y,
        }
    }

    /// The segment of a wheel of `segments` under a global point.
    pub fn hit(&self, x: i32, y: i32, segments: usize) -> Option<usize> {
        let offset = self.offset(x, y);
        hit_test(offset.x, offset.y, segments)
    }

    /// The anchor in global coordinates.
    pub fn anchor_global(&self) -> (i32, i32) {
        let monitor = self.display.monitor;
        (
            monitor.x + (self.anchor.x * self.scale).round() as i32,
            monitor.y + (self.anchor.y * self.scale).round() as i32,
        )
    }
}

/// `display`'s work area in logical pixels from the monitor's top-left.
fn work_area(display: DisplayBounds, scale: f64) -> grape_core::Rect {
    let monitor = display.monitor;
    let to_logical = |px: i32, origin: i32| f64::from(px - origin) / scale;
    grape_core::Rect {
        x: to_logical(display.work_area.x, monitor.x),
        y: to_logical(display.work_area.y, monitor.y),
        width: f64::from(display.work_area.width) / scale,
        height: f64::from(display.work_area.height) / scale,
    }
}

/// What the controller must do after an input.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Nothing the overlay or the app needs to hear of.
    Nothing,
    /// The highlight changed to this.
    Highlight(Option<usize>),
    /// A group opened or closed: send the wheel again with the same `seq`,
    /// then this highlight if there is one.
    Redraw {
        /// The segment of the new ring under the pointer.
        highlight: Option<usize>,
    },
    /// Close the wheel.
    Close(CloseReason),
    /// A shortcut was picked: close the wheel, then hand its run on.
    Pick(Pick),
}

/// A picked shortcut.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    /// Its segment in the ring that was showing. Logs name it by this.
    pub index: usize,
    /// What the runner gets.
    pub run: ShortcutRun,
    /// `false` when no step has anything to do yet. The runner then
    /// answers "nothing to run" and calls nothing.
    pub runnable: bool,
}

/// Why an open wheel has been up too long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expired {
    /// The overlay never reported it painted, [`PAINT_DEADLINE`] after the
    /// press.
    NotPainted,
    /// No hot key or pointer input for the watchdog's time.
    Quiet,
}

/// One open Shortcuts wheel. `Debug` prints counts and flags only: the
/// ring holds names and steps the user typed.
#[derive(Clone)]
pub struct Session {
    seq: u64,
    place: WheelPlace,
    /// The app in front when the hot key went down.
    front: Option<FrontApp>,
    /// The menu's own ring.
    root: Vec<RingItem>,
    /// The open groups, outermost first, each by its index in its ring.
    trail: Vec<usize>,
    highlight: Option<usize>,
    /// The last pointer position, global. The press's cursor until the
    /// pointer watch reports.
    pointer: (i32, i32),
    /// The hot key was tapped, or a group was opened or closed: letting go
    /// of the hot key no longer picks.
    sticky: bool,
    /// A button-down was seen while open, so the next button-up counts.
    button_seen: bool,
    painted: bool,
    /// When the hot key went down.
    opened_at: Instant,
    /// The last hot key or pointer input, for the watchdog.
    last_event: Instant,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("seq", &self.seq)
            .field("place", &self.place)
            .field("front", &self.front)
            .field("segments", &self.ring().len())
            .field("depth", &self.trail.len())
            .field("highlight", &self.highlight)
            .field("sticky", &self.sticky)
            .field("painted", &self.painted)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Opens wheel `seq` for a hot key that went down at `now` with the
    /// cursor at `cursor` on `display` and `front` in front. The ring is
    /// that app's menu, or Everywhere; the anchor is the cursor or the
    /// work area's middle, as the snapshot says.
    pub fn open(
        seq: u64,
        snapshot: &Snapshot,
        front: Option<FrontApp>,
        display: DisplayBounds,
        cursor: (i32, i32),
        now: Instant,
    ) -> Self {
        let front_id = front.as_ref().and_then(|app| app.id.as_deref());
        Self {
            seq,
            place: place_for(snapshot.opens_at, display, cursor),
            root: menu_for(snapshot, front_id),
            front,
            trail: Vec::new(),
            highlight: None,
            pointer: cursor,
            sticky: false,
            button_seen: false,
            painted: false,
            opened_at: now,
            last_event: now,
        }
    }

    /// The wheel's number, from the controller's counter.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Where the wheel sits.
    pub fn place(&self) -> WheelPlace {
        self.place
    }

    /// How many segments the ring that is showing has.
    pub fn segment_count(&self) -> usize {
        self.ring().len()
    }

    /// How many groups are open.
    pub fn depth(&self) -> usize {
        self.trail.len()
    }

    /// The highlighted segment.
    #[cfg(test)]
    pub fn highlight(&self) -> Option<usize> {
        self.highlight
    }

    /// `true` once letting go of the hot key no longer picks.
    #[cfg(test)]
    pub fn is_sticky(&self) -> bool {
        self.sticky
    }

    /// What the overlay draws: the ring that is showing, under the open
    /// groups' names.
    pub fn content(&self) -> UiContent {
        let mut names = Vec::with_capacity(self.trail.len());
        let mut ring = self.root.as_slice();
        for &index in &self.trail {
            let Some(item) = ring.get(index) else { break };
            let RingKind::Group(children) = &item.kind else {
                break;
            };
            names.push(item.name.clone());
            ring = children;
        }
        UiContent::shortcuts(
            names,
            ring.iter()
                .map(|item| (item.name.clone(), item.icon.to_ui(), item.is_group())),
        )
    }

    /// A hot key signal at `now`.
    ///
    /// - Open let go within [`TAP`] of the press makes the wheel sticky,
    ///   whatever is highlighted. Later it picks the highlight, or cancels.
    /// - Open pressed again closes a sticky wheel; before that it's a
    ///   repeat.
    /// - Confirm picks the highlight, if there is one. Escape closes.
    pub fn hotkey(&mut self, signal: &HotkeySignal, now: Instant) -> Decision {
        match signal {
            HotkeySignal::Pressed(HotkeyId::Open, _) if self.sticky => {
                self.last_event = now;
                Decision::Close(CloseReason::Cancel)
            }
            HotkeySignal::Released(HotkeyId::Open) if !self.sticky => {
                self.last_event = now;
                if now.saturating_duration_since(self.opened_at) < TAP {
                    self.sticky = true;
                    return Decision::Nothing;
                }
                match self.highlight {
                    Some(index) => self.pick(index),
                    None => Decision::Close(CloseReason::Cancel),
                }
            }
            HotkeySignal::Pressed(HotkeyId::Confirm, _) => {
                self.last_event = now;
                match self.highlight {
                    Some(index) => self.pick(index),
                    None => Decision::Nothing,
                }
            }
            HotkeySignal::Pressed(HotkeyId::Escape, _) => {
                self.last_event = now;
                Decision::Close(CloseReason::Escape)
            }
            HotkeySignal::Pressed(HotkeyId::Open, _)
            | HotkeySignal::Released(HotkeyId::Open | HotkeyId::Escape | HotkeyId::Confirm) => {
                Decision::Nothing
            }
        }
    }

    /// A pointer signal at `now`.
    ///
    /// - A move highlights the segment under it.
    /// - A button-up counts only after a button-down seen while open. On a
    ///   segment it picks; nearer the anchor than the ring it goes back one
    ///   group, or cancels at the root; past the ring it cancels; in a gap
    ///   or a rounded-off corner it does nothing.
    pub fn pointer(&mut self, signal: PointerSignal, now: Instant) -> Decision {
        self.last_event = now;
        match signal {
            PointerSignal::Moved { x, y } => {
                self.pointer = (x, y);
                let index = self.place.hit(x, y, self.ring().len());
                if index == self.highlight {
                    return Decision::Nothing;
                }
                self.highlight = index;
                Decision::Highlight(index)
            }
            PointerSignal::Down { x, y } => {
                self.pointer = (x, y);
                self.button_seen = true;
                Decision::Nothing
            }
            PointerSignal::Up { x, y } => {
                self.pointer = (x, y);
                if !std::mem::take(&mut self.button_seen) {
                    return Decision::Nothing;
                }
                if let Some(index) = self.place.hit(x, y, self.ring().len()) {
                    return self.pick(index);
                }
                let offset = self.place.offset(x, y);
                let distance = offset.x.hypot(offset.y);
                if distance < GEOMETRY.inner_r {
                    self.back()
                } else if distance > GEOMETRY.outer_r {
                    Decision::Close(CloseReason::Cancel)
                } else {
                    Decision::Nothing
                }
            }
        }
    }

    /// The overlay painted this wheel at `now`. The first time, how long
    /// that took since the press.
    pub fn painted(&mut self, now: Instant) -> Option<Duration> {
        if std::mem::replace(&mut self.painted, true) {
            return None;
        }
        Some(now.saturating_duration_since(self.opened_at))
    }

    /// When [`expired`](Self::expired) next has an answer: the paint
    /// deadline until the wheel is painted, then `watchdog` after the last
    /// input.
    pub fn deadline(&self, watchdog: Duration) -> Instant {
        let quiet = self.last_event + watchdog;
        if self.painted {
            quiet
        } else {
            quiet.min(self.opened_at + PAINT_DEADLINE)
        }
    }

    /// Whether the wheel has been up too long at `now`, and why.
    pub fn expired(&self, now: Instant, watchdog: Duration) -> Option<Expired> {
        if !self.painted && now >= self.opened_at + PAINT_DEADLINE {
            Some(Expired::NotPainted)
        } else if now >= self.last_event + watchdog {
            Some(Expired::Quiet)
        } else {
            None
        }
    }

    /// The ring that is showing: the root's, or the innermost open group's.
    fn ring(&self) -> &[RingItem] {
        let mut ring = self.root.as_slice();
        for &index in &self.trail {
            match ring.get(index).map(|item| &item.kind) {
                Some(RingKind::Group(children)) => ring = children,
                _ => break,
            }
        }
        ring
    }

    /// Picks segment `index`: a group opens, a shortcut becomes a run.
    fn pick(&mut self, index: usize) -> Decision {
        let Some(item) = self.ring().get(index) else {
            return Decision::Nothing;
        };
        let steps = match &item.kind {
            RingKind::Group(_) => {
                self.trail.push(index);
                return self.redraw();
            }
            RingKind::Steps(steps) => steps,
        };
        Decision::Pick(Pick {
            index,
            runnable: runnable(steps),
            run: ShortcutRun {
                seq: self.seq,
                name: item.name.clone(),
                steps: steps.clone(),
                front: self.front.clone(),
                anchor: self.place.anchor_global(),
                display: self.place.display,
            },
        })
    }

    /// Leaves the innermost group, or cancels at the root.
    fn back(&mut self) -> Decision {
        if self.trail.pop().is_none() {
            return Decision::Close(CloseReason::Cancel);
        }
        self.redraw()
    }

    /// After the ring changed: sticky from now on, and the highlight is
    /// whatever segment of the new ring the pointer is on.
    fn redraw(&mut self) -> Decision {
        self.sticky = true;
        let (x, y) = self.pointer;
        self.highlight = self.place.hit(x, y, self.ring().len());
        Decision::Redraw {
            highlight: self.highlight,
        }
    }
}

/// Where a wheel opens for a press with the cursor at `cursor`.
pub fn place_for(opens_at: OpensAt, display: DisplayBounds, cursor: (i32, i32)) -> WheelPlace {
    match opens_at {
        OpensAt::Cursor => WheelPlace::at(display, cursor),
        OpensAt::Center => WheelPlace::centered(display),
    }
}

/// Menus for tests here and in the controller, made with the model's own
/// edits.
#[cfg(test)]
pub(crate) mod fixture {
    use grape_core::Platform;
    use grape_platform::mock::scripts;
    use grape_platform::{DisplayBounds, Rect};

    use crate::shortcuts::model::{
        add_app_menu, add_item, add_step, set_enabled, set_item, set_menu_everywhere, set_step,
        Config, Icon, Id, NewItem, Snapshot, StepEdit, StepKindId,
    };

    /// Stands in for anything the user typed.
    pub(crate) const MARKER: &str = "zq-marker";

    /// A 1920×1080 monitor at (`x`, `y`) with a 48 px bar at the bottom.
    pub(crate) fn display(x: i32, y: i32, scale: f64) -> DisplayBounds {
        DisplayBounds {
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
        }
    }

    /// A config that is switched on, with an empty Everywhere menu, and
    /// that menu's id.
    pub(crate) fn empty() -> (Config, Id) {
        let mut config = Config::defaults(Platform::Windows);
        set_enabled(&mut config, true).unwrap();
        let everywhere = config.menus[0].id;
        (config, everywhere)
    }

    /// A shortcut named `name` at the end of `menu` (or of `group` in it)
    /// with a fresh step of each of `kinds`.
    pub(crate) fn shortcut(
        config: &mut Config,
        menu: Id,
        group: Option<Id>,
        name: &str,
        kinds: &[StepKindId],
    ) -> Id {
        let item = add_item(config, menu, group, NewItem::Steps).unwrap();
        set_item(config, item, name, &Icon::None).unwrap();
        for kind in kinds {
            add_step(config, item, *kind).unwrap();
        }
        item
    }

    /// Everywhere with four segments, clockwise from the top: "Copy" (one
    /// step), the group "Tidy" ("Left" and "Right"), "Later" (one step
    /// with nothing typed yet) and "Paste" (one step).
    pub(crate) fn config() -> Config {
        let (mut config, everywhere) = empty();
        shortcut(
            &mut config,
            everywhere,
            None,
            "Copy",
            &[StepKindId::Clipboard],
        );
        let group = add_item(&mut config, everywhere, None, NewItem::Group).unwrap();
        set_item(&mut config, group, "Tidy", &Icon::glyph("window")).unwrap();
        for name in ["Left", "Right"] {
            shortcut(
                &mut config,
                everywhere,
                Some(group),
                name,
                &[StepKindId::Window],
            );
        }
        shortcut(
            &mut config,
            everywhere,
            None,
            "Later",
            &[StepKindId::TypeText],
        );
        shortcut(
            &mut config,
            everywhere,
            None,
            "Paste",
            &[StepKindId::Clipboard],
        );
        config
    }

    /// [`config`] as the controller holds it.
    pub(crate) fn snapshot() -> Snapshot {
        Snapshot::of(&config())
    }

    /// [`config`] plus a menu for [`scripts::front_app`] holding "Build",
    /// with the Everywhere switch on.
    pub(crate) fn with_app_menu() -> Snapshot {
        let mut config = config();
        let id = scripts::front_app().id.unwrap();
        let menu = add_app_menu(&mut config, &id, "Editor", "app.grape.test").unwrap();
        shortcut(&mut config, menu, None, "Build", &[StepKindId::Delay]);
        set_menu_everywhere(&mut config, menu, true).unwrap();
        Snapshot::of(&config)
    }

    /// One Everywhere shortcut whose name and typed text are [`MARKER`].
    pub(crate) fn marked() -> Snapshot {
        let (mut config, everywhere) = empty();
        let item = add_item(&mut config, everywhere, None, NewItem::Steps).unwrap();
        let icon = Icon::Emoji {
            value: MARKER.to_owned(),
        };
        set_item(&mut config, item, MARKER, &icon).unwrap();
        let step = add_step(&mut config, item, StepKindId::TypeText).unwrap();
        let edit = StepEdit::TypeText {
            text: format!("typed {MARKER}"),
        };
        set_step(&mut config, step, &edit).unwrap();
        Snapshot::of(&config)
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{self, MARKER};
    use super::*;
    use crate::events::{UiIcon, UiShortcutSegment};
    use crate::shortcuts::model::{set_opens_at, StepKindId};
    use grape_platform::mock::scripts;
    use grape_platform::shortcuts::PressContext;

    const WATCHDOG: Duration = Duration::from_secs(30);
    /// Past [`TAP`].
    const HELD: Duration = Duration::from_millis(400);

    /// The fixture's ring, clockwise from the top at 100 % scale with the
    /// anchor at (400, 300).
    const TOP: (i32, i32) = (400, 216);
    const RIGHT: (i32, i32) = (484, 300);
    const BOTTOM: (i32, i32) = (400, 384);
    const LEFT: (i32, i32) = (316, 300);

    fn open_with(snapshot: &Snapshot, now: Instant) -> Session {
        Session::open(
            7,
            snapshot,
            Some(scripts::front_app()),
            fixture::display(0, 0, 1.0),
            scripts::WHEEL_AT,
            now,
        )
    }

    fn open(now: Instant) -> Session {
        open_with(&fixture::snapshot(), now)
    }

    fn moved((x, y): (i32, i32)) -> PointerSignal {
        PointerSignal::Moved { x, y }
    }

    fn down((x, y): (i32, i32)) -> PointerSignal {
        PointerSignal::Down { x, y }
    }

    fn up((x, y): (i32, i32)) -> PointerSignal {
        PointerSignal::Up { x, y }
    }

    /// A click at `at`: the button going down, then up.
    fn click(session: &mut Session, at: (i32, i32), now: Instant) -> Decision {
        assert_eq!(session.pointer(down(at), now), Decision::Nothing);
        session.pointer(up(at), now)
    }

    fn pressed(id: HotkeyId) -> HotkeySignal {
        HotkeySignal::Pressed(
            id,
            PressContext {
                cursor: Some(scripts::WHEEL_AT),
                front: None,
                button_down: false,
            },
        )
    }

    fn titles(content: &UiContent) -> (Vec<String>, Vec<String>) {
        let UiContent::Shortcuts {
            trail, segments, ..
        } = content
        else {
            panic!("{content:?}");
        };
        (
            trail.clone(),
            segments.iter().map(|s| s.title.clone()).collect(),
        )
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_place_at_the_cursor_is_logical_and_clamped() {
        let place = WheelPlace::at(fixture::display(-1920, 0, 1.5), (-1915, 1075));
        assert!((place.scale - 1.5).abs() < 1e-9);
        assert!((place.anchor.x - 140.0).abs() < 1e-9, "{:?}", place.anchor);
        assert!(
            (place.anchor.y - (1032.0 / 1.5 - 140.0)).abs() < 1e-9,
            "{:?}",
            place.anchor
        );
        assert_eq!(place.anchor_global(), (-1920 + 210, 1032 - 210));
        // A point well inside is kept as it is.
        let place = WheelPlace::at(fixture::display(1920, 0, 1.5), (2400, 600));
        assert_eq!(place.anchor, Point { x: 320.0, y: 400.0 });
        assert_eq!(place.anchor_global(), (2400, 600));
    }

    #[test]
    fn a_centered_place_is_the_middle_of_the_work_area() {
        let place = WheelPlace::centered(fixture::display(0, 0, 1.0));
        assert_eq!(place.anchor, Point { x: 960.0, y: 516.0 });
        assert_eq!(place.anchor_global(), (960, 516));
        let place = WheelPlace::centered(fixture::display(-1920, 100, 2.0));
        assert_eq!(place.anchor, Point { x: 480.0, y: 258.0 });
        assert_eq!(place.anchor_global(), (-960, 616));
        assert_eq!(
            place_for(OpensAt::Center, fixture::display(0, 0, 1.0), (5, 5)).anchor,
            Point { x: 960.0, y: 516.0 }
        );
        assert_eq!(
            place_for(OpensAt::Cursor, fixture::display(0, 0, 1.0), (400, 300)).anchor,
            Point { x: 400.0, y: 300.0 }
        );
    }

    #[test]
    fn offsets_and_hits_are_logical_on_a_scaled_offset_monitor() {
        let place = WheelPlace::at(fixture::display(1920, 0, 2.0), (2720, 600));
        assert_eq!(place.offset(2720, 600), Point { x: 0.0, y: 0.0 });
        // 84 logical px above the anchor is 168 physical px.
        assert_eq!(place.offset(2720, 432), Point { x: 0.0, y: -84.0 });
        assert_eq!(place.hit(2720, 432, 4), Some(0));
        assert_eq!(place.hit(2720 + 168, 600, 4), Some(1));
        // 84 physical px is inside the dead zone.
        assert_eq!(place.hit(2720, 600 - 84, 4), None);
        assert_eq!(place.hit(2720, 432, 0), None);
    }

    #[test]
    fn a_bad_scale_counts_as_one() {
        for scale in [f64::NAN, 0.0, -2.0, f64::INFINITY] {
            assert!((sane_scale(scale) - 1.0).abs() < 1e-9);
            let place = WheelPlace::at(fixture::display(0, 0, scale), (400, 300));
            assert_eq!(place.anchor, Point { x: 400.0, y: 300.0 });
            assert_eq!(place.anchor_global(), (400, 300));
        }
        assert!((sane_scale(1.25) - 1.25).abs() < 1e-9);
    }

    #[test]
    fn the_ring_is_the_everywhere_menu_for_an_app_without_one() {
        let session = open(Instant::now());
        assert_eq!(session.seq(), 7);
        assert_eq!(session.segment_count(), 4);
        assert_eq!(session.depth(), 0);
        assert_eq!(session.place().anchor, Point { x: 400.0, y: 300.0 });
        let content = session.content();
        assert_eq!(
            titles(&content),
            (Vec::new(), names(&["Copy", "Tidy", "Later", "Paste"]))
        );
        let UiContent::Shortcuts {
            dense, segments, ..
        } = &content
        else {
            panic!("{content:?}");
        };
        assert!(!dense);
        let groups: Vec<bool> = segments.iter().map(|s| s.group).collect();
        assert_eq!(groups, [false, true, false, false]);
        assert!(matches!(
            segments.get(1),
            Some(UiShortcutSegment {
                icon: UiIcon::Glyph { value },
                ..
            }) if value == "window"
        ));
    }

    #[test]
    fn the_front_apps_own_menu_wins_and_ends_with_everywhere() {
        let now = Instant::now();
        let snapshot = fixture::with_app_menu();
        let mut session = open_with(&snapshot, now);
        assert_eq!(
            titles(&session.content()),
            (Vec::new(), names(&["Build", "Everywhere"]))
        );
        // Two segments: the top half and the bottom half. Straight down is
        // the last of the group's three.
        assert_eq!(
            click(&mut session, BOTTOM, now),
            Decision::Redraw { highlight: Some(2) }
        );
        // The Everywhere group leaves Everywhere's own group out.
        assert_eq!(
            titles(&session.content()),
            (names(&["Everywhere"]), names(&["Copy", "Later", "Paste"]))
        );
        // Another app, or one with no id, gets Everywhere itself.
        for front in [
            None,
            Some(FrontApp { pid: 9, id: None }),
            Some(FrontApp {
                pid: 9,
                id: Some("com.example.other".to_owned()),
            }),
        ] {
            let display = fixture::display(0, 0, 1.0);
            let session = Session::open(1, &snapshot, front, display, scripts::WHEEL_AT, now);
            assert_eq!(session.segment_count(), 4);
        }
    }

    #[test]
    fn an_empty_menu_has_no_segments() {
        let now = Instant::now();
        let (config, _) = fixture::empty();
        let mut session = open_with(&Snapshot::of(&config), now);
        assert_eq!(session.segment_count(), 0);
        assert_eq!(
            session.content(),
            UiContent::Shortcuts {
                dense: false,
                trail: Vec::new(),
                segments: Vec::new(),
            }
        );
        assert_eq!(session.pointer(moved(TOP), now), Decision::Nothing);
        // On the ring's band there is nothing to pick and nothing to leave.
        assert_eq!(click(&mut session, TOP, now), Decision::Nothing);
        assert_eq!(
            click(&mut session, scripts::WHEEL_AT, now),
            Decision::Close(CloseReason::Cancel)
        );
    }

    #[test]
    fn it_opens_at_the_center_when_the_snapshot_says_so() {
        let mut config = fixture::config();
        set_opens_at(&mut config, OpensAt::Center).unwrap();
        let session = open_with(&Snapshot::of(&config), Instant::now());
        assert_eq!(session.place().anchor, Point { x: 960.0, y: 516.0 });
    }

    #[test]
    fn moves_highlight_on_change_only() {
        let now = Instant::now();
        let mut session = open(now);
        assert_eq!(
            session.pointer(moved(scripts::WHEEL_AT), now),
            Decision::Nothing
        );
        assert_eq!(
            session.pointer(moved(TOP), now),
            Decision::Highlight(Some(0))
        );
        assert_eq!(session.pointer(moved((401, 216)), now), Decision::Nothing);
        assert_eq!(
            session.pointer(moved(LEFT), now),
            Decision::Highlight(Some(3))
        );
        assert_eq!(
            session.pointer(moved((410, 300)), now),
            Decision::Highlight(None)
        );
        assert_eq!(session.highlight(), None);
    }

    #[test]
    fn a_button_up_counts_only_after_a_button_down() {
        let now = Instant::now();
        let mut session = open(now);
        assert_eq!(session.pointer(up(TOP), now), Decision::Nothing);
        assert_eq!(session.pointer(down(TOP), now), Decision::Nothing);
        assert!(matches!(
            session.pointer(up(TOP), now),
            Decision::Pick(Pick { index: 0, .. })
        ));
        // The press is used up.
        assert_eq!(session.pointer(up(TOP), now), Decision::Nothing);
    }

    #[test]
    fn a_click_picks_cancels_or_does_nothing_by_where_it_lands() {
        let now = Instant::now();
        let mut session = open(now);
        // A gap between two of the four segments, on the ring's band.
        let gap = (400 + 59, 300 - 59);
        assert_eq!(session.place().hit(gap.0, gap.1, 4), None);
        assert_eq!(click(&mut session, gap, now), Decision::Nothing);
        // A rounded-off outer corner of the top segment.
        let corner = (479, 213);
        let offset = session.place().offset(corner.0, corner.1);
        let distance = offset.x.hypot(offset.y);
        assert!(distance > 110.0 && distance < GEOMETRY.outer_r);
        assert_eq!(session.place().hit(corner.0, corner.1, 4), None);
        assert_eq!(session.place().hit(corner.0 - 12, corner.1, 4), Some(0));
        assert_eq!(click(&mut session, corner, now), Decision::Nothing);
        // Past the ring.
        assert_eq!(
            click(&mut session, (400, 300 - 121), now),
            Decision::Close(CloseReason::Cancel)
        );
        // The ring's own edges still belong to it.
        assert!(matches!(
            click(&mut session, (400, 300 - 120), now),
            Decision::Pick(_)
        ));
        assert!(matches!(
            click(&mut session, (400, 300 - 48), now),
            Decision::Pick(_)
        ));
        // Inside the ring, at the root.
        assert_eq!(
            click(&mut session, (400, 300 - 47), now),
            Decision::Close(CloseReason::Cancel)
        );
    }

    #[test]
    fn a_pick_carries_the_shortcut_for_the_runner() {
        let now = Instant::now();
        let mut session = open(now);
        let Decision::Pick(pick) = click(&mut session, LEFT, now) else {
            panic!("no pick");
        };
        assert_eq!(pick.index, 3);
        assert!(pick.runnable);
        assert_eq!(pick.run.seq, 7);
        assert_eq!(pick.run.name, "Paste");
        let kinds: Vec<StepKindId> = pick.run.steps.iter().map(|s| s.kind.id()).collect();
        assert_eq!(kinds, [StepKindId::Clipboard]);
        assert_eq!(pick.run.front, Some(scripts::front_app()));
        assert_eq!(pick.run.anchor, (400, 300));
        assert_eq!(pick.run.display, fixture::display(0, 0, 1.0));
    }

    #[test]
    fn a_shortcut_with_nothing_typed_yet_is_picked_as_not_runnable() {
        let now = Instant::now();
        let mut session = open(now);
        let Decision::Pick(pick) = click(&mut session, BOTTOM, now) else {
            panic!("no pick");
        };
        assert_eq!((pick.index, pick.runnable), (2, false));
        assert_eq!(pick.run.name, "Later");
        assert_eq!(pick.run.steps.len(), 1);
    }

    #[test]
    fn the_run_anchor_is_global_on_a_scaled_offset_monitor() {
        let now = Instant::now();
        let display = fixture::display(1920, 0, 1.5);
        let mut session = Session::open(1, &fixture::snapshot(), None, display, (2400, 600), now);
        // 84 logical px above the anchor is 126 physical px.
        assert_eq!(
            session.pointer(moved((2400, 600 - 126)), now),
            Decision::Highlight(Some(0))
        );
        let Decision::Pick(pick) = session.hotkey(&pressed(HotkeyId::Confirm), now) else {
            panic!("no pick");
        };
        assert_eq!(pick.run.anchor, (2400, 600));
        assert_eq!(pick.run.display, display);
        assert_eq!(pick.run.front, None);
    }

    #[test]
    fn a_group_opens_a_second_ring_and_the_middle_leaves_it() {
        let now = Instant::now();
        let mut session = open(now);
        assert_eq!(
            session.pointer(moved(RIGHT), now),
            Decision::Highlight(Some(1))
        );
        // The pointer stays where the group was: a gap of the two-segment
        // ring, so nothing is highlighted in it.
        assert_eq!(
            click(&mut session, RIGHT, now),
            Decision::Redraw { highlight: None }
        );
        assert!(session.is_sticky());
        assert_eq!(session.depth(), 1);
        assert_eq!(session.highlight(), None);
        assert_eq!(
            titles(&session.content()),
            (names(&["Tidy"]), names(&["Left", "Right"]))
        );
        assert_eq!(
            session.pointer(moved(BOTTOM), now),
            Decision::Highlight(Some(1))
        );
        // The middle goes back one ring. The pointer is in the dead zone.
        assert_eq!(
            click(&mut session, (410, 300), now),
            Decision::Redraw { highlight: None }
        );
        assert_eq!(session.depth(), 0);
        assert_eq!(session.segment_count(), 4);
        // And at the root the middle cancels.
        assert_eq!(
            click(&mut session, (410, 300), now),
            Decision::Close(CloseReason::Cancel)
        );
    }

    #[test]
    fn a_new_ring_highlights_the_segment_under_the_pointer() {
        let now = Instant::now();
        let mut session = open(now);
        // Just inside the right segment, above its middle: the top half of
        // the group's ring.
        let at = (400 + 70, 300 - 40);
        assert_eq!(
            session.pointer(moved(at), now),
            Decision::Highlight(Some(1))
        );
        assert_eq!(
            session.hotkey(&pressed(HotkeyId::Confirm), now),
            Decision::Redraw { highlight: Some(0) }
        );
        assert_eq!(session.highlight(), Some(0));
        let Decision::Pick(pick) = session.hotkey(&pressed(HotkeyId::Confirm), now) else {
            panic!("no pick");
        };
        assert_eq!(pick.run.name, "Left");
        assert_eq!(pick.index, 0);
    }

    #[test]
    fn a_tap_makes_the_wheel_sticky_and_never_picks() {
        let now = Instant::now();
        let mut session = open(now);
        session.pointer(moved(TOP), now);
        let just_under = now + TAP - Duration::from_millis(1);
        assert_eq!(
            session.hotkey(&HotkeySignal::Released(HotkeyId::Open), just_under),
            Decision::Nothing
        );
        assert!(session.is_sticky());
        assert_eq!(session.highlight(), Some(0));
        // Later releases mean nothing, and a second press cancels.
        assert_eq!(
            session.hotkey(&HotkeySignal::Released(HotkeyId::Open), now + HELD),
            Decision::Nothing
        );
        assert_eq!(
            session.hotkey(&pressed(HotkeyId::Open), now + HELD),
            Decision::Close(CloseReason::Cancel)
        );
    }

    #[test]
    fn letting_go_after_a_hold_picks_the_highlight_or_cancels() {
        let now = Instant::now();
        let mut session = open(now);
        session.pointer(moved(TOP), now);
        // A repeat of the press while held.
        assert_eq!(
            session.hotkey(&pressed(HotkeyId::Open), now + HELD),
            Decision::Nothing
        );
        assert!(matches!(
            session.hotkey(&HotkeySignal::Released(HotkeyId::Open), now + TAP),
            Decision::Pick(Pick { index: 0, .. })
        ));

        let mut session = open(now);
        assert_eq!(
            session.hotkey(&HotkeySignal::Released(HotkeyId::Open), now + HELD),
            Decision::Close(CloseReason::Cancel)
        );
    }

    #[test]
    fn letting_go_over_a_group_opens_it_and_stays() {
        let now = Instant::now();
        let mut session = open(now);
        session.pointer(moved(RIGHT), now);
        assert_eq!(
            session.hotkey(&HotkeySignal::Released(HotkeyId::Open), now + HELD),
            Decision::Redraw { highlight: None }
        );
        assert!(session.is_sticky());
        assert_eq!(session.depth(), 1);
    }

    #[test]
    fn confirm_and_escape() {
        let now = Instant::now();
        let mut session = open(now);
        assert_eq!(
            session.hotkey(&pressed(HotkeyId::Confirm), now),
            Decision::Nothing
        );
        for id in [HotkeyId::Confirm, HotkeyId::Escape] {
            assert_eq!(
                session.hotkey(&HotkeySignal::Released(id), now),
                Decision::Nothing
            );
        }
        session.pointer(moved(TOP), now);
        assert!(matches!(
            session.hotkey(&pressed(HotkeyId::Confirm), now),
            Decision::Pick(Pick { index: 0, .. })
        ));
        assert_eq!(
            session.hotkey(&pressed(HotkeyId::Escape), now),
            Decision::Close(CloseReason::Escape)
        );
    }

    #[test]
    fn the_paint_deadline_comes_first_then_the_watchdog() {
        let now = Instant::now();
        let mut session = open(now);
        assert_eq!(session.deadline(WATCHDOG), now + PAINT_DEADLINE);
        assert_eq!(session.expired(now + PAINT_DEADLINE / 2, WATCHDOG), None);
        assert_eq!(
            session.expired(now + PAINT_DEADLINE, WATCHDOG),
            Some(Expired::NotPainted)
        );
        let after = Duration::from_millis(80);
        assert_eq!(session.painted(now + after), Some(after));
        assert_eq!(session.painted(now + after * 2), None);
        assert_eq!(session.deadline(WATCHDOG), now + WATCHDOG);
        assert_eq!(session.expired(now + PAINT_DEADLINE * 2, WATCHDOG), None);
        assert_eq!(
            session.expired(now + WATCHDOG, WATCHDOG),
            Some(Expired::Quiet)
        );
    }

    #[test]
    fn only_inputs_that_do_something_feed_the_watchdog() {
        let now = Instant::now();
        let later = now + Duration::from_secs(5);
        let mut session = open(now);
        session.painted(now);
        // A repeat of the press and the other keys coming up are ignored.
        session.hotkey(&pressed(HotkeyId::Open), later);
        session.hotkey(&HotkeySignal::Released(HotkeyId::Escape), later);
        session.hotkey(&HotkeySignal::Released(HotkeyId::Confirm), later);
        assert_eq!(session.deadline(WATCHDOG), now + WATCHDOG);
        // Confirm with nothing highlighted still shows someone is there.
        session.hotkey(&pressed(HotkeyId::Confirm), later);
        assert_eq!(session.deadline(WATCHDOG), later + WATCHDOG);
        let later = later + Duration::from_secs(5);
        session.pointer(moved(TOP), later);
        assert_eq!(session.deadline(WATCHDOG), later + WATCHDOG);

        // A tap counts; the hot key coming up again on a sticky wheel is
        // ignored.
        let mut session = open(now);
        session.painted(now);
        let tap = now + Duration::from_millis(100);
        session.hotkey(&HotkeySignal::Released(HotkeyId::Open), tap);
        assert_eq!(session.deadline(WATCHDOG), tap + WATCHDOG);
        session.hotkey(&HotkeySignal::Released(HotkeyId::Open), later);
        assert_eq!(session.deadline(WATCHDOG), tap + WATCHDOG);
    }

    #[test]
    fn debug_never_prints_what_the_user_typed() {
        let now = Instant::now();
        let mut session = open_with(&fixture::marked(), now);
        let front = Some(FrontApp {
            pid: 4,
            id: Some(format!("com.{MARKER}")),
        });
        session.front = front;
        session.pointer(moved(TOP), now);
        let decision = session.hotkey(&pressed(HotkeyId::Confirm), now);
        let Decision::Pick(pick) = &decision else {
            panic!("no pick");
        };
        assert_eq!(pick.run.name, MARKER);
        for text in [
            format!("{session:?}"),
            format!("{session:#?}"),
            format!("{decision:?}"),
            format!("{decision:#?}"),
            format!("{:?}", session.content()),
        ] {
            assert!(!text.contains(MARKER), "{text}");
        }
    }
}
