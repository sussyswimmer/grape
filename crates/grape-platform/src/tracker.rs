//! The candidate-drag heuristic, as a pure state machine.
//!
//! The OS layer feeds it raw input (button down/up, pointer moves, and polled
//! modifier, Esc and button state) and sends whatever [`DragSignal`]s it
//! returns. No OS calls here, so the rules are unit-tested on every OS.
//!
//! A drag starts when the primary button is down and the pointer has moved
//! past the OS drag threshold (`docs/INTERACTION_SPEC.md` §2), but only if
//! the press was *armed*: on Windows that means it landed on Explorer or the
//! Desktop (`docs/PLATFORM_WINDOWS.md` → "Is it a file drag?").

use crate::{DragSignal, Modifiers};

/// Tracks one primary-button press and decides whether it became a drag.
#[derive(Debug, Clone)]
pub struct DragTracker {
    threshold_x: i32,
    threshold_y: i32,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No button down.
    Idle,
    /// The button is down but the pointer hasn't moved far enough yet.
    /// `armed` is false when the press started somewhere Grape ignores.
    Pressed { x: i32, y: i32, armed: bool },
    /// A drag is in progress. `modifiers` is the last set reported, and
    /// `escape` whether Esc was down at the last poll.
    Dragging { modifiers: Modifiers, escape: bool },
}

impl DragTracker {
    /// A tracker that starts a drag once the pointer moves more than
    /// `threshold_x` / `threshold_y` pixels from the press point (Windows:
    /// `SM_CXDRAG` / `SM_CYDRAG`). Negative thresholds count as 0.
    pub fn new(threshold_x: i32, threshold_y: i32) -> Self {
        Self {
            threshold_x: threshold_x.max(0),
            threshold_y: threshold_y.max(0),
            state: State::Idle,
        }
    }

    /// `true` while a drag is in progress.
    pub fn is_dragging(&self) -> bool {
        matches!(self.state, State::Dragging { .. })
    }

    /// `true` while the button is down, armed or not. The OS layer polls
    /// modifiers only while this holds.
    pub fn is_pressed(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    /// The primary button went down at (`x`, `y`). `armed` says whether a
    /// drag from here may open the wheel. Returns [`DragSignal::Ended`] if a
    /// drag was still open (its button-up was missed).
    pub fn button_down(&mut self, x: i32, y: i32, armed: bool) -> Option<DragSignal> {
        let ended = self.is_dragging().then_some(DragSignal::Ended);
        self.state = State::Pressed { x, y, armed };
        ended
    }

    /// The pointer moved to (`x`, `y`) with the button down. `held` is the
    /// modifier set right now; it's only used if this move starts the drag.
    ///
    /// Returns `[Started, Moved]` when this move starts a drag, `[Moved]`
    /// during a drag, and nothing otherwise.
    pub fn moved(&mut self, x: i32, y: i32, held: Modifiers) -> Vec<DragSignal> {
        match self.state {
            State::Idle => Vec::new(),
            State::Pressed {
                x: x0,
                y: y0,
                armed,
            } => {
                let far = (x - x0).abs() > self.threshold_x || (y - y0).abs() > self.threshold_y;
                if !(armed && far) {
                    return Vec::new();
                }
                self.state = State::Dragging {
                    modifiers: held,
                    escape: false,
                };
                vec![
                    DragSignal::Started { modifiers: held },
                    DragSignal::Moved { x, y },
                ]
            }
            State::Dragging { .. } => vec![DragSignal::Moved { x, y }],
        }
    }

    /// The primary button went up. Returns [`DragSignal::Ended`] if a drag
    /// was in progress.
    pub fn button_up(&mut self) -> Option<DragSignal> {
        let ended = self.is_dragging().then_some(DragSignal::Ended);
        self.state = State::Idle;
        ended
    }

    /// A poll of the keyboard and mouse during a press: the modifiers held,
    /// whether Esc is down, and whether the primary button is still down.
    ///
    /// During a drag it reports modifier changes, Esc going down (once per
    /// press), and [`DragSignal::Ended`] if the button turns out to be up
    /// (a missed button-up). Outside a drag it only resets a stale press.
    pub fn poll(&mut self, held: Modifiers, escape: bool, button_down: bool) -> Vec<DragSignal> {
        if !button_down {
            return self.button_up().into_iter().collect();
        }
        let State::Dragging {
            modifiers,
            escape: was_escape,
        } = self.state
        else {
            return Vec::new();
        };
        let mut signals = Vec::new();
        if held != modifiers {
            signals.push(DragSignal::Modifiers(held));
        }
        if escape && !was_escape {
            signals.push(DragSignal::Escape);
        }
        self.state = State::Dragging {
            modifiers: held,
            escape,
        };
        signals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;

    fn started(modifiers: Modifiers) -> DragSignal {
        DragSignal::Started { modifiers }
    }

    #[test]
    fn small_moves_stay_a_click() {
        let mut t = DragTracker::new(4, 4);
        assert_eq!(t.button_down(100, 100, true), None);
        assert!(t.moved(104, 96, NONE).is_empty());
        assert!(t.is_pressed());
        assert!(!t.is_dragging());
        assert_eq!(t.button_up(), None);
        assert!(!t.is_pressed());
    }

    #[test]
    fn crossing_the_threshold_starts_a_drag() {
        let mut t = DragTracker::new(4, 4);
        t.button_down(100, 100, true);
        assert_eq!(
            t.moved(105, 100, NONE),
            vec![started(NONE), DragSignal::Moved { x: 105, y: 100 }]
        );
        assert!(t.is_dragging());
        assert_eq!(
            t.moved(130, 90, NONE),
            vec![DragSignal::Moved { x: 130, y: 90 }]
        );
        assert_eq!(t.button_up(), Some(DragSignal::Ended));
        assert!(!t.is_dragging());
    }

    #[test]
    fn the_vertical_threshold_counts_too() {
        let mut t = DragTracker::new(10, 2);
        t.button_down(0, 0, true);
        assert!(t.moved(0, 2, NONE).is_empty());
        assert_eq!(t.moved(0, -3, NONE).first(), Some(&started(NONE)));
    }

    #[test]
    fn an_unarmed_press_never_starts_a_drag() {
        let mut t = DragTracker::new(4, 4);
        t.button_down(100, 100, false);
        assert!(t.moved(300, 300, Modifiers::SHIFT).is_empty());
        assert!(t.poll(Modifiers::SHIFT, true, true).is_empty());
        assert_eq!(t.button_up(), None);
    }

    #[test]
    fn modifiers_held_at_the_start_are_reported_as_the_baseline() {
        let mut t = DragTracker::new(4, 4);
        t.button_down(0, 0, true);
        let signals = t.moved(20, 0, Modifiers::SHIFT);
        assert_eq!(signals.first(), Some(&started(Modifiers::SHIFT)));
        // Still held: not a change, so nothing to report.
        assert!(t.poll(Modifiers::SHIFT, false, true).is_empty());
        assert_eq!(t.poll(NONE, false, true), vec![DragSignal::Modifiers(NONE)]);
        assert_eq!(
            t.poll(Modifiers::SHIFT, false, true),
            vec![DragSignal::Modifiers(Modifiers::SHIFT)]
        );
    }

    #[test]
    fn modifier_changes_are_reported_once() {
        let mut t = DragTracker::new(4, 4);
        t.button_down(0, 0, true);
        t.moved(20, 0, NONE);
        assert_eq!(
            t.poll(Modifiers::CTRL, false, true),
            vec![DragSignal::Modifiers(Modifiers::CTRL)]
        );
        assert!(t.poll(Modifiers::CTRL, false, true).is_empty());
    }

    #[test]
    fn modifiers_before_the_drag_are_not_reported() {
        let mut t = DragTracker::new(4, 4);
        t.button_down(0, 0, true);
        assert!(t.poll(Modifiers::SHIFT, true, true).is_empty());
    }

    #[test]
    fn escape_is_reported_once_per_press() {
        let mut t = DragTracker::new(4, 4);
        t.button_down(0, 0, true);
        t.moved(20, 0, NONE);
        assert_eq!(t.poll(NONE, true, true), vec![DragSignal::Escape]);
        assert!(t.poll(NONE, true, true).is_empty());
        assert!(t.poll(NONE, false, true).is_empty());
        assert_eq!(t.poll(NONE, true, true), vec![DragSignal::Escape]);
    }

    #[test]
    fn a_modifier_change_and_escape_in_one_poll() {
        let mut t = DragTracker::new(4, 4);
        t.button_down(0, 0, true);
        t.moved(20, 0, NONE);
        assert_eq!(
            t.poll(Modifiers::ALT, true, true),
            vec![DragSignal::Modifiers(Modifiers::ALT), DragSignal::Escape]
        );
    }

    #[test]
    fn a_missed_button_up_is_caught_by_the_poll() {
        let mut t = DragTracker::new(4, 4);
        t.button_down(0, 0, true);
        t.moved(20, 0, NONE);
        assert_eq!(t.poll(NONE, false, false), vec![DragSignal::Ended]);
        assert!(!t.is_pressed());
        // A stale press that never became a drag just resets.
        t.button_down(0, 0, true);
        assert!(t.poll(NONE, false, false).is_empty());
        assert!(!t.is_pressed());
    }

    #[test]
    fn a_new_press_during_a_drag_ends_it() {
        let mut t = DragTracker::new(4, 4);
        t.button_down(0, 0, true);
        t.moved(20, 0, NONE);
        assert_eq!(t.button_down(50, 50, true), Some(DragSignal::Ended));
        assert!(!t.is_dragging());
        assert!(t.is_pressed());
    }

    #[test]
    fn moves_without_a_press_are_ignored() {
        let mut t = DragTracker::new(4, 4);
        assert!(t.moved(500, 500, Modifiers::SHIFT).is_empty());
        assert!(t.poll(Modifiers::SHIFT, true, true).is_empty());
        assert_eq!(t.button_up(), None);
    }

    #[test]
    fn negative_thresholds_act_as_zero() {
        let mut t = DragTracker::new(-3, -3);
        t.button_down(0, 0, true);
        assert!(t.moved(0, 0, NONE).is_empty());
        assert_eq!(t.moved(1, 0, NONE).first(), Some(&started(NONE)));
    }
}
