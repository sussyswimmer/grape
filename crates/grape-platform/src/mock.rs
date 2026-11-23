//! Scripted twins of the OS monitors for app-level tests. No OS needed.
//!
//! [`ScriptedMonitor`] replays a fixed list of [`DragSignal`]s from a worker
//! thread, in order. [`ScriptedHotkeys`] and [`ScriptedPointer`] do the same
//! for the Shortcuts wheel's hot keys and pointer, and remember what they
//! were asked to bind and when they were turned on. [`scripts`] has
//! ready-made sequences for the flows in `docs/INTERACTION_SPEC.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::keys::Chord;
use crate::shortcuts::{HotkeyId, HotkeyMonitor, HotkeySignal, PointerSignal, PointerWatch};
use crate::{DragMonitor, DragSignal, PlatformError};

/// How long the worker waits before retrying when the channel is full.
const RETRY_DELAY: Duration = Duration::from_millis(1);

/// A script and the worker thread that replays it.
#[derive(Debug)]
struct Replayer<T> {
    script: Vec<T>,
    interval: Duration,
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl<T: Clone + Send + 'static> Replayer<T> {
    fn new(script: Vec<T>) -> Self {
        Self {
            script,
            interval: Duration::ZERO,
            stop_flag: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    /// Starts the worker. With a `gate`, signals go out only while it is
    /// `true`.
    fn start(
        &mut self,
        tx: SyncSender<T>,
        gate: Option<Arc<AtomicBool>>,
    ) -> Result<(), PlatformError> {
        if self.worker.is_some() {
            return Err(PlatformError::AlreadyRunning);
        }
        let stop_flag = Arc::new(AtomicBool::new(false));
        self.stop_flag = Arc::clone(&stop_flag);
        let script = self.script.clone();
        let interval = self.interval;
        self.worker = Some(thread::spawn(move || {
            replay(&script, interval, &tx, &stop_flag, gate.as_deref());
        }));
        Ok(())
    }

    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            // A panic in the worker is a test bug; nothing to recover here.
            let _ = worker.join();
        }
    }
}

/// Replays a script of signals on a worker thread.
///
/// Unlike a real monitor, it never drops a signal: when the channel is full
/// it waits, so tests see the whole script. When the script is done the
/// worker drops its sender, so `rx.iter()` ends.
#[derive(Debug)]
pub struct ScriptedMonitor {
    replayer: Replayer<DragSignal>,
}

impl ScriptedMonitor {
    /// A monitor that sends `script` as fast as the receiver takes it.
    pub fn new(script: Vec<DragSignal>) -> Self {
        Self {
            replayer: Replayer::new(script),
        }
    }

    /// Waits `interval` before each signal, to imitate a real drag's pace.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.replayer.interval = interval;
        self
    }

    /// The signals this monitor sends.
    pub fn script(&self) -> &[DragSignal] {
        &self.replayer.script
    }
}

impl DragMonitor for ScriptedMonitor {
    fn start(&mut self, tx: SyncSender<DragSignal>) -> Result<(), PlatformError> {
        self.replayer.start(tx, None)
    }

    fn stop(&mut self) {
        self.replayer.stop();
    }
}

impl Drop for ScriptedMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The lock's contents even when a test thread panicked while holding it.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One `bind` call a [`ScriptedHotkeys`] got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindCall {
    /// The hot key.
    pub id: HotkeyId,
    /// The chords asked for. Empty is an unbind.
    pub chords: Vec<Chord>,
}

#[derive(Debug, Default)]
struct BindState {
    calls: Vec<BindCall>,
    bound: Vec<BindCall>,
}

/// What a [`ScriptedHotkeys`] was asked to bind. Take it before the monitor
/// is boxed and handed over; it stays good afterwards.
#[derive(Debug, Clone, Default)]
pub struct BindLog(Arc<Mutex<BindState>>);

impl BindLog {
    /// Every `bind` call so far, in order, refused ones included.
    pub fn calls(&self) -> Vec<BindCall> {
        locked(&self.0).calls.clone()
    }

    /// The chords `id` has now: what its last `bind` that worked left it.
    /// Empty when it has none, and once the monitor is stopped or dropped.
    pub fn bound(&self, id: HotkeyId) -> Vec<Chord> {
        locked(&self.0)
            .bound
            .iter()
            .find(|binding| binding.id == id)
            .map(|binding| binding.chords.clone())
            .unwrap_or_default()
    }
}

/// A [`HotkeyMonitor`] that replays a script of signals on a worker thread
/// and remembers its `bind` calls ([`binds`](Self::binds)).
///
/// Like [`ScriptedMonitor`] it never drops a signal, and its sender goes
/// when the script is done. It sends the whole script whatever is bound: a
/// test that wants an unbound key to stay silent leaves it out of the
/// script.
#[derive(Debug)]
pub struct ScriptedHotkeys {
    replayer: Replayer<HotkeySignal>,
    refused: Vec<Chord>,
    log: BindLog,
}

impl ScriptedHotkeys {
    /// What `bind` answers for a chord given to [`refusing`](Self::refusing).
    pub const REFUSED: PlatformError = PlatformError::Os {
        context: "ScriptedHotkeys::bind",
        code: -1,
    };

    /// A monitor that sends `script` as fast as the receiver takes it.
    pub fn new(script: Vec<HotkeySignal>) -> Self {
        Self {
            replayer: Replayer::new(script),
            refused: Vec::new(),
            log: BindLog::default(),
        }
    }

    /// Waits `interval` before each signal.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.replayer.interval = interval;
        self
    }

    /// Makes every `bind` that names `chord` fail with
    /// [`REFUSED`](Self::REFUSED), as when the OS won't give it up.
    #[must_use]
    pub fn refusing(mut self, chord: Chord) -> Self {
        self.refused.push(chord);
        self
    }

    /// The signals this monitor sends.
    pub fn script(&self) -> &[HotkeySignal] {
        &self.replayer.script
    }

    /// The record of this monitor's `bind` calls.
    pub fn binds(&self) -> BindLog {
        self.log.clone()
    }
}

impl HotkeyMonitor for ScriptedHotkeys {
    fn start(&mut self, tx: SyncSender<HotkeySignal>) -> Result<(), PlatformError> {
        self.replayer.start(tx, None)
    }

    fn bind(&mut self, id: HotkeyId, chords: &[Chord]) -> Result<(), PlatformError> {
        let call = BindCall {
            id,
            chords: chords.to_vec(),
        };
        let mut state = locked(&self.log.0);
        state.calls.push(call.clone());
        if chords.iter().any(|chord| self.refused.contains(chord)) {
            // A refused bind leaves the key with what it had.
            return Err(Self::REFUSED);
        }
        state.bound.retain(|binding| binding.id != id);
        if !chords.is_empty() {
            state.bound.push(call);
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.replayer.stop();
        locked(&self.log.0).bound.clear();
    }
}

impl Drop for ScriptedHotkeys {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The `set_active` calls a [`ScriptedPointer`] got. Take it before the
/// watch is boxed and handed over; it stays good afterwards.
#[derive(Debug, Clone, Default)]
pub struct ActiveLog(Arc<Mutex<Vec<bool>>>);

impl ActiveLog {
    /// Every `set_active` call so far, in order.
    pub fn calls(&self) -> Vec<bool> {
        locked(&self.0).clone()
    }
}

/// A [`PointerWatch`] that replays a script of signals on a worker thread
/// and remembers its `set_active` calls ([`activity`](Self::activity)).
///
/// As a real watch does, it sends only while it is on: the script waits
/// while the watch is off (and before the first `set_active(true)`) and
/// goes on from where it was. It sends what the script says and nothing
/// else, so the `Moved` a real watch sends at once belongs in the script.
/// It never drops a signal, and its sender goes when the script is done.
#[derive(Debug)]
pub struct ScriptedPointer {
    replayer: Replayer<PointerSignal>,
    active: Arc<AtomicBool>,
    log: ActiveLog,
}

impl ScriptedPointer {
    /// A watch that sends `script`, once on, as fast as the receiver takes
    /// it.
    pub fn new(script: Vec<PointerSignal>) -> Self {
        Self {
            replayer: Replayer::new(script),
            active: Arc::new(AtomicBool::new(false)),
            log: ActiveLog::default(),
        }
    }

    /// Waits `interval` before each signal.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.replayer.interval = interval;
        self
    }

    /// The signals this watch sends.
    pub fn script(&self) -> &[PointerSignal] {
        &self.replayer.script
    }

    /// The record of this watch's `set_active` calls.
    pub fn activity(&self) -> ActiveLog {
        self.log.clone()
    }
}

impl PointerWatch for ScriptedPointer {
    fn start(&mut self, tx: SyncSender<PointerSignal>) -> Result<(), PlatformError> {
        self.replayer.start(tx, Some(Arc::clone(&self.active)))
    }

    fn set_active(&mut self, on: bool) {
        locked(&self.log.0).push(on);
        self.active.store(on, Ordering::Release);
    }

    fn stop(&mut self) {
        self.replayer.stop();
    }
}

impl Drop for ScriptedPointer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One step of a Shortcuts wheel script: what the hot keys or the pointer
/// said, or time going by. One list keeps the two in the order a user would
/// cause them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptEvent {
    /// From the hot key monitor.
    Hotkey(HotkeySignal),
    /// From the pointer watch.
    Pointer(PointerSignal),
    /// Nothing happens for this long.
    Wait(Duration),
}

/// The hot key signals of a script, in order, for a [`ScriptedHotkeys`].
pub fn hotkey_signals(script: &[ScriptEvent]) -> Vec<HotkeySignal> {
    script
        .iter()
        .filter_map(|event| match event {
            ScriptEvent::Hotkey(signal) => Some(signal.clone()),
            ScriptEvent::Pointer(_) | ScriptEvent::Wait(_) => None,
        })
        .collect()
}

/// The pointer signals of a script, in order, for a [`ScriptedPointer`].
pub fn pointer_signals(script: &[ScriptEvent]) -> Vec<PointerSignal> {
    script
        .iter()
        .filter_map(|event| match event {
            ScriptEvent::Pointer(signal) => Some(*signal),
            ScriptEvent::Hotkey(_) | ScriptEvent::Wait(_) => None,
        })
        .collect()
}

/// Sends `script` in order. A full channel is waited for, never skipped;
/// while `gate` is there and `false`, nothing is sent.
fn replay<T: Clone>(
    script: &[T],
    interval: Duration,
    tx: &SyncSender<T>,
    stop_flag: &AtomicBool,
    gate: Option<&AtomicBool>,
) {
    for signal in script {
        if !interval.is_zero() {
            thread::sleep(interval);
        }
        let mut pending = signal.clone();
        loop {
            if stop_flag.load(Ordering::Acquire) {
                return;
            }
            if gate.is_some_and(|open| !open.load(Ordering::Acquire)) {
                thread::sleep(RETRY_DELAY);
                continue;
            }
            match tx.try_send(pending) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) => {
                    pending = back;
                    thread::sleep(RETRY_DELAY);
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

/// Ready-made signal sequences. Positions are arbitrary physical pixels.
pub mod scripts {
    use std::time::Duration;

    use super::ScriptEvent;
    use crate::shortcuts::{FrontApp, HotkeyId, HotkeySignal, PointerSignal, PressContext};
    use crate::{DragSignal, Modifiers};

    /// A drag that starts with no modifier held.
    const STARTED: DragSignal = DragSignal::Started {
        modifiers: Modifiers::NONE,
    };

    /// A drag with no key pressed: the wheel never opens and the OS handles
    /// the drop.
    pub fn plain_drag() -> Vec<DragSignal> {
        vec![
            STARTED,
            DragSignal::Moved { x: 400, y: 300 },
            DragSignal::Moved { x: 520, y: 340 },
            DragSignal::Ended,
        ]
    }

    /// Drag, tap Shift (down, then up), keep dragging, drop. The
    /// conversions wheel opens on key-down and stays open after key-up.
    pub fn shift_tap() -> Vec<DragSignal> {
        vec![
            STARTED,
            DragSignal::Moved { x: 400, y: 300 },
            DragSignal::Modifiers(Modifiers::SHIFT),
            DragSignal::Modifiers(Modifiers::NONE),
            DragSignal::Moved { x: 400, y: 220 },
            DragSignal::Ended,
        ]
    }

    /// Shift opens the conversions wheel, then Ctrl alone swaps it to tools
    /// at the same anchor (Windows default keys).
    pub fn key_swap() -> Vec<DragSignal> {
        vec![
            STARTED,
            DragSignal::Moved { x: 400, y: 300 },
            DragSignal::Modifiers(Modifiers::SHIFT),
            DragSignal::Modifiers(Modifiers::NONE),
            DragSignal::Moved { x: 430, y: 300 },
            DragSignal::Modifiers(Modifiers::CTRL),
            DragSignal::Modifiers(Modifiers::NONE),
            DragSignal::Moved { x: 400, y: 220 },
            DragSignal::Ended,
        ]
    }

    /// Shift is already held when the drag starts (Explorer's "move"), so
    /// no wheel opens. Releasing it and pressing it again during the drag
    /// does open the conversions wheel.
    pub fn shift_held_from_start() -> Vec<DragSignal> {
        vec![
            DragSignal::Started {
                modifiers: Modifiers::SHIFT,
            },
            DragSignal::Moved { x: 400, y: 300 },
            DragSignal::Moved { x: 430, y: 310 },
            DragSignal::Modifiers(Modifiers::NONE),
            DragSignal::Modifiers(Modifiers::SHIFT),
            DragSignal::Moved { x: 400, y: 220 },
            DragSignal::Ended,
        ]
    }

    /// Shift opens the wheel, then Esc closes it before the drop.
    pub fn escape() -> Vec<DragSignal> {
        vec![
            STARTED,
            DragSignal::Moved { x: 400, y: 300 },
            DragSignal::Modifiers(Modifiers::SHIFT),
            DragSignal::Escape,
            DragSignal::Moved { x: 520, y: 340 },
            DragSignal::Ended,
        ]
    }

    /// Shift and Ctrl held together: two modifiers mean "no mode", so no
    /// wheel opens.
    pub fn two_modifiers() -> Vec<DragSignal> {
        vec![
            STARTED,
            DragSignal::Moved { x: 400, y: 300 },
            DragSignal::Modifiers(Modifiers {
                shift: true,
                ctrl: true,
                ..Modifiers::NONE
            }),
            DragSignal::Modifiers(Modifiers::NONE),
            DragSignal::Ended,
        ]
    }

    // The Shortcuts wheel. No drag here: a hot key opens the wheel where
    // the cursor is, and the pointer watch reports from then on.

    /// Where the cursor is when the hot key goes down: the wheel's center.
    pub const WHEEL_AT: (i32, i32) = (400, 300);

    /// A point 80 above [`WHEEL_AT`]: on the ring at 100 % scale, over item
    /// 0 (item 1 in a wheel of three, whose item 0 is on the left).
    pub const ON_SEGMENT: (i32, i32) = (400, 220);

    /// The app in front in these scripts.
    pub fn front_app() -> FrontApp {
        FrontApp {
            pid: 501,
            id: Some("com.example.editor".to_owned()),
        }
    }

    /// `id` going down with the cursor at [`WHEEL_AT`], [`front_app`] in
    /// front and no mouse button held.
    pub fn press(id: HotkeyId) -> ScriptEvent {
        ScriptEvent::Hotkey(HotkeySignal::Pressed(
            id,
            PressContext {
                cursor: Some(WHEEL_AT),
                front: Some(front_app()),
                button_down: false,
            },
        ))
    }

    /// `id` coming up.
    pub fn release(id: HotkeyId) -> ScriptEvent {
        ScriptEvent::Hotkey(HotkeySignal::Released(id))
    }

    fn moved((x, y): (i32, i32)) -> ScriptEvent {
        ScriptEvent::Pointer(PointerSignal::Moved { x, y })
    }

    /// Tap the hot key (down, then up at once), so the wheel stays open;
    /// move onto a segment and click it.
    pub fn hotkey_tap_then_click() -> Vec<ScriptEvent> {
        let (x, y) = ON_SEGMENT;
        vec![
            press(HotkeyId::Open),
            moved(WHEEL_AT),
            release(HotkeyId::Open),
            moved(ON_SEGMENT),
            ScriptEvent::Pointer(PointerSignal::Down { x, y }),
            ScriptEvent::Pointer(PointerSignal::Up { x, y }),
        ]
    }

    /// Hold the hot key, move onto a segment, and let go well after a tap
    /// would have ended: the release picks the segment.
    pub fn hotkey_hold_and_release() -> Vec<ScriptEvent> {
        vec![
            press(HotkeyId::Open),
            moved(WHEEL_AT),
            moved(ON_SEGMENT),
            ScriptEvent::Wait(Duration::from_millis(500)),
            release(HotkeyId::Open),
        ]
    }

    /// Tap the hot key, move onto a segment, then press Esc: the wheel
    /// closes and nothing runs.
    pub fn hotkey_escape() -> Vec<ScriptEvent> {
        vec![
            press(HotkeyId::Open),
            moved(WHEEL_AT),
            release(HotkeyId::Open),
            moved(ON_SEGMENT),
            press(HotkeyId::Escape),
            release(HotkeyId::Escape),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyCode;
    use crate::Modifiers;
    use std::sync::mpsc;
    use std::time::Instant;

    fn all_scripts() -> Vec<Vec<DragSignal>> {
        vec![
            scripts::plain_drag(),
            scripts::shift_tap(),
            scripts::key_swap(),
            scripts::two_modifiers(),
            scripts::shift_held_from_start(),
            scripts::escape(),
        ]
    }

    #[test]
    fn replays_the_script_in_order_then_disconnects() {
        let script = scripts::key_swap();
        let mut monitor = ScriptedMonitor::new(script.clone());
        let (tx, rx) = mpsc::sync_channel(16);
        monitor.start(tx).unwrap();
        let received: Vec<DragSignal> = rx.iter().collect();
        assert_eq!(received, script);
        monitor.stop();
    }

    #[test]
    fn waits_instead_of_dropping_when_the_channel_is_full() {
        let script = scripts::shift_tap();
        let mut monitor = ScriptedMonitor::new(script.clone());
        let (tx, rx) = mpsc::sync_channel(1);
        monitor.start(tx).unwrap();
        thread::sleep(Duration::from_millis(20));
        let received: Vec<DragSignal> = rx.iter().collect();
        assert_eq!(received, script);
    }

    #[test]
    fn second_start_is_rejected() {
        let mut monitor = ScriptedMonitor::new(scripts::plain_drag());
        let (tx, _rx) = mpsc::sync_channel(16);
        monitor.start(tx.clone()).unwrap();
        assert_eq!(monitor.start(tx), Err(PlatformError::AlreadyRunning));
        monitor.stop();
    }

    #[test]
    fn stop_before_start_is_harmless() {
        let mut monitor = ScriptedMonitor::new(scripts::plain_drag());
        monitor.stop();
        monitor.stop();
        let (tx, rx) = mpsc::sync_channel(16);
        monitor.start(tx).unwrap();
        assert_eq!(rx.iter().count(), scripts::plain_drag().len());
    }

    #[test]
    fn stop_ends_a_slow_script_early() {
        let script = scripts::key_swap();
        let mut monitor =
            ScriptedMonitor::new(script.clone()).with_interval(Duration::from_millis(200));
        let (tx, rx) = mpsc::sync_channel(16);
        monitor.start(tx).unwrap();
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(first, DragSignal::Started { .. }));
        let begun = Instant::now();
        monitor.stop();
        assert!(begun.elapsed() < Duration::from_secs(1));
        let rest: Vec<DragSignal> = rx.iter().collect();
        assert!(1 + rest.len() < script.len());
    }

    #[test]
    fn can_restart_after_stop() {
        let mut monitor = ScriptedMonitor::new(scripts::plain_drag());
        let (tx, rx) = mpsc::sync_channel(16);
        monitor.start(tx).unwrap();
        assert_eq!(rx.iter().count(), 4);
        monitor.stop();
        let (tx, rx) = mpsc::sync_channel(16);
        monitor.start(tx).unwrap();
        assert_eq!(rx.iter().count(), 4);
    }

    #[test]
    fn every_script_starts_and_ends_a_drag() {
        for script in all_scripts() {
            assert!(matches!(script.first(), Some(DragSignal::Started { .. })));
            assert_eq!(script.last(), Some(&DragSignal::Ended));
            let starts = script
                .iter()
                .filter(|s| matches!(s, DragSignal::Started { .. }))
                .count();
            let ends = script.iter().filter(|s| **s == DragSignal::Ended).count();
            assert_eq!((starts, ends), (1, 1));
        }
    }

    #[test]
    fn scripts_hold_the_modifiers_they_describe() {
        let held = |script: Vec<DragSignal>| -> Vec<Modifiers> {
            script
                .into_iter()
                .filter_map(|s| match s {
                    DragSignal::Modifiers(m) if !m.is_empty() => Some(m),
                    _ => None,
                })
                .collect()
        };
        assert!(held(scripts::plain_drag()).is_empty());
        assert_eq!(held(scripts::shift_tap()), vec![Modifiers::SHIFT]);
        assert_eq!(
            held(scripts::key_swap()),
            vec![Modifiers::SHIFT, Modifiers::CTRL]
        );
        let two = held(scripts::two_modifiers());
        assert_eq!(two.len(), 1);
        assert_eq!(two.first().map(|m| m.count()), Some(2));
        assert_eq!(
            held(scripts::shift_held_from_start()),
            vec![Modifiers::SHIFT]
        );
        assert!(scripts::escape().contains(&DragSignal::Escape));
    }
    fn shortcut_scripts() -> Vec<Vec<ScriptEvent>> {
        vec![
            scripts::hotkey_tap_then_click(),
            scripts::hotkey_hold_and_release(),
            scripts::hotkey_escape(),
        ]
    }

    fn chord(key: KeyCode, mods: Modifiers) -> Chord {
        Chord { key, mods }
    }

    const CTRL_ALT: Modifiers = Modifiers {
        ctrl: true,
        alt: true,
        ..Modifiers::NONE
    };

    #[test]
    fn hotkeys_replay_the_script_in_order_then_disconnect() {
        let script = hotkey_signals(&scripts::hotkey_escape());
        let mut monitor = ScriptedHotkeys::new(script.clone());
        assert_eq!(monitor.script(), script);
        let (tx, rx) = mpsc::sync_channel(1);
        monitor.start(tx).unwrap();
        let received: Vec<HotkeySignal> = rx.iter().collect();
        assert_eq!(received, script);
        monitor.stop();
    }

    #[test]
    fn hotkeys_reject_a_second_start_and_restart_after_stop() {
        let script = hotkey_signals(&scripts::hotkey_tap_then_click());
        let mut monitor = ScriptedHotkeys::new(script.clone());
        monitor.stop();
        let (tx, rx) = mpsc::sync_channel(16);
        monitor.start(tx.clone()).unwrap();
        assert_eq!(monitor.start(tx), Err(PlatformError::AlreadyRunning));
        assert_eq!(rx.iter().count(), script.len());
        monitor.stop();
        monitor.stop();
        let (tx, rx) = mpsc::sync_channel(16);
        monitor.start(tx).unwrap();
        assert_eq!(rx.iter().count(), script.len());
    }

    #[test]
    fn hotkeys_stop_ends_a_slow_script_early() {
        let script = hotkey_signals(&scripts::hotkey_escape());
        let mut monitor =
            ScriptedHotkeys::new(script.clone()).with_interval(Duration::from_millis(200));
        let (tx, rx) = mpsc::sync_channel(16);
        monitor.start(tx).unwrap();
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(first, HotkeySignal::Pressed(HotkeyId::Open, _)));
        let begun = Instant::now();
        monitor.stop();
        assert!(begun.elapsed() < Duration::from_secs(1));
        assert!(1 + rx.iter().count() < script.len());
    }

    #[test]
    fn binds_are_remembered_in_order() {
        let open = chord(KeyCode::KeyG, CTRL_ALT);
        let escapes = chord(KeyCode::Escape, CTRL_ALT).with_modifier_subsets();
        let monitor = ScriptedHotkeys::new(Vec::new());
        let log = monitor.binds();
        assert!(log.calls().is_empty());
        assert!(log.bound(HotkeyId::Open).is_empty());

        // Binding needs no start, and the log outlives the box.
        let mut boxed: Box<dyn HotkeyMonitor> = Box::new(monitor);
        boxed.bind(HotkeyId::Open, &[open]).unwrap();
        boxed.bind(HotkeyId::Escape, &escapes).unwrap();
        assert_eq!(log.bound(HotkeyId::Open), vec![open]);
        assert_eq!(log.bound(HotkeyId::Escape), escapes);
        assert!(log.bound(HotkeyId::Confirm).is_empty());

        // A new bind replaces the old one; an empty one unbinds.
        let other = chord(KeyCode::F5, Modifiers::NONE);
        boxed.bind(HotkeyId::Open, &[other]).unwrap();
        boxed.bind(HotkeyId::Escape, &[]).unwrap();
        assert_eq!(log.bound(HotkeyId::Open), vec![other]);
        assert!(log.bound(HotkeyId::Escape).is_empty());

        let call = |id, chords: &[Chord]| BindCall {
            id,
            chords: chords.to_vec(),
        };
        assert_eq!(
            log.calls(),
            vec![
                call(HotkeyId::Open, &[open]),
                call(HotkeyId::Escape, &escapes),
                call(HotkeyId::Open, &[other]),
                call(HotkeyId::Escape, &[]),
            ]
        );

        // A stopped monitor holds no chord, but the calls stay on record.
        boxed.stop();
        assert!(log.bound(HotkeyId::Open).is_empty());
        boxed.bind(HotkeyId::Open, &[open]).unwrap();
        assert_eq!(log.bound(HotkeyId::Open), vec![open]);
        // So does a dropped one.
        drop(boxed);
        assert!(log.bound(HotkeyId::Open).is_empty());
        assert_eq!(log.calls().len(), 5);
    }

    #[test]
    fn a_refused_chord_fails_the_bind_and_keeps_the_old_one() {
        let old = chord(KeyCode::KeyG, CTRL_ALT);
        let taken = chord(KeyCode::Space, Modifiers::ALT);
        let mut monitor = ScriptedHotkeys::new(Vec::new()).refusing(taken);
        let log = monitor.binds();
        monitor.bind(HotkeyId::Open, &[old]).unwrap();
        assert_eq!(
            monitor.bind(HotkeyId::Open, &[taken]),
            Err(ScriptedHotkeys::REFUSED)
        );
        // Any refused chord in the list refuses all of it.
        assert_eq!(
            monitor.bind(HotkeyId::Confirm, &[old, taken]),
            Err(ScriptedHotkeys::REFUSED)
        );
        assert_eq!(log.bound(HotkeyId::Open), vec![old]);
        assert!(log.bound(HotkeyId::Confirm).is_empty());
        assert_eq!(log.calls().len(), 3);
        assert_eq!(log.calls()[1].chords, vec![taken]);
    }

    #[test]
    fn the_pointer_sends_nothing_until_it_is_on() {
        let script = pointer_signals(&scripts::hotkey_tap_then_click());
        let mut watch = ScriptedPointer::new(script.clone());
        assert_eq!(watch.script(), script);
        let log = watch.activity();
        let (tx, rx) = mpsc::sync_channel(16);
        watch.start(tx).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(30)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        assert!(log.calls().is_empty());
        watch.set_active(true);
        let received: Vec<PointerSignal> = rx.iter().collect();
        assert_eq!(received, script);
        watch.set_active(false);
        assert_eq!(log.calls(), vec![true, false]);
        watch.stop();
    }

    #[test]
    fn the_pointer_waits_while_it_is_off_and_goes_on_from_there() {
        let script = pointer_signals(&scripts::hotkey_tap_then_click());
        let mut watch = ScriptedPointer::new(script.clone());
        // Turned on before the start: the script begins at once.
        watch.set_active(true);
        // With no room in the channel, a signal only leaves while this
        // thread is waiting for it.
        let (tx, rx) = mpsc::sync_channel(0);
        watch.start(tx).unwrap();
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        watch.set_active(false);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        watch.set_active(true);
        let mut received = vec![first];
        received.extend(rx.iter());
        assert_eq!(received, script);
        assert_eq!(watch.activity().calls(), vec![true, false, true]);
    }

    #[test]
    fn the_pointer_rejects_a_second_start_and_stops_while_off() {
        let mut watch = ScriptedPointer::new(pointer_signals(&scripts::hotkey_escape()));
        watch.stop();
        let (tx, rx) = mpsc::sync_channel(16);
        watch.start(tx.clone()).unwrap();
        assert_eq!(watch.start(tx), Err(PlatformError::AlreadyRunning));
        // Never turned on: stop still ends the worker, and nothing was sent.
        let begun = Instant::now();
        watch.stop();
        assert!(begun.elapsed() < Duration::from_secs(1));
        assert_eq!(rx.iter().count(), 0);
        // The log outlives a boxed watch.
        let log = watch.activity();
        let mut boxed: Box<dyn PointerWatch> = Box::new(watch);
        boxed.set_active(true);
        boxed.set_active(true);
        drop(boxed);
        assert_eq!(log.calls(), vec![true, true]);
    }

    #[test]
    fn every_shortcut_script_opens_with_the_hot_key_at_the_cursor() {
        for script in shortcut_scripts() {
            let Some(ScriptEvent::Hotkey(HotkeySignal::Pressed(HotkeyId::Open, context))) =
                script.first()
            else {
                panic!("a script starts with the Open hot key going down");
            };
            assert_eq!(context.cursor, Some(scripts::WHEEL_AT));
            assert_eq!(context.front, Some(scripts::front_app()));
            assert!(!context.button_down);
            // The watch's first report is where the cursor already is.
            assert_eq!(
                pointer_signals(&script).first(),
                Some(&PointerSignal::Moved {
                    x: scripts::WHEEL_AT.0,
                    y: scripts::WHEEL_AT.1,
                })
            );
            // Each key that goes down comes up, and Open goes down once.
            let signals = hotkey_signals(&script);
            for id in [HotkeyId::Open, HotkeyId::Escape, HotkeyId::Confirm] {
                let downs = signals
                    .iter()
                    .filter(|s| matches!(s, HotkeySignal::Pressed(pressed, _) if *pressed == id))
                    .count();
                let ups = signals
                    .iter()
                    .filter(|s| **s == HotkeySignal::Released(id))
                    .count();
                assert_eq!(downs, ups);
                assert!(downs <= 1);
            }
        }
    }

    #[test]
    fn shortcut_scripts_do_what_they_say() {
        let (x, y) = scripts::ON_SEGMENT;
        // On the ring: between the wheel's inner (48) and outer (120) radius.
        assert_eq!((x - scripts::WHEEL_AT.0, y - scripts::WHEEL_AT.1), (0, -80));

        // A tap: nothing passes between the key going down and coming up.
        let tap = scripts::hotkey_tap_then_click();
        assert!(!tap.iter().any(|e| matches!(e, ScriptEvent::Wait(_))));
        assert_eq!(
            hotkey_signals(&tap).last(),
            Some(&HotkeySignal::Released(HotkeyId::Open))
        );
        assert_eq!(
            pointer_signals(&tap)[1..],
            [
                PointerSignal::Moved { x, y },
                PointerSignal::Down { x, y },
                PointerSignal::Up { x, y },
            ]
        );
        assert_eq!(
            tap.last(),
            Some(&ScriptEvent::Pointer(PointerSignal::Up { x, y }))
        );

        // A hold: the release comes last, longer after the press than a tap
        // lasts (300 ms), with the pointer already on the segment.
        let hold = scripts::hotkey_hold_and_release();
        assert_eq!(hold.last(), Some(&scripts::release(HotkeyId::Open)));
        let waited: Duration = hold
            .iter()
            .filter_map(|e| match e {
                ScriptEvent::Wait(time) => Some(*time),
                _ => None,
            })
            .sum();
        assert!(waited > Duration::from_millis(300));
        assert_eq!(
            pointer_signals(&hold).last(),
            Some(&PointerSignal::Moved { x, y })
        );
        assert!(!pointer_signals(&hold)
            .iter()
            .any(|s| matches!(s, PointerSignal::Down { .. } | PointerSignal::Up { .. })));

        // Escape: pressed after the tap, with no click anywhere.
        let escape = scripts::hotkey_escape();
        let keys = hotkey_signals(&escape);
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[1], HotkeySignal::Released(HotkeyId::Open));
        assert!(matches!(
            keys[2],
            HotkeySignal::Pressed(HotkeyId::Escape, _)
        ));
        assert_eq!(keys[3], HotkeySignal::Released(HotkeyId::Escape));
        assert!(!pointer_signals(&escape)
            .iter()
            .any(|s| matches!(s, PointerSignal::Down { .. })));
    }

    #[test]
    fn a_script_splits_into_its_two_channels_in_order() {
        let script = scripts::hotkey_tap_then_click();
        let keys = hotkey_signals(&script);
        let pointer = pointer_signals(&script);
        assert_eq!(keys.len() + pointer.len(), script.len());
        let mut keys = keys.into_iter();
        let mut pointer = pointer.into_iter();
        for event in script {
            match event {
                ScriptEvent::Hotkey(signal) => assert_eq!(keys.next(), Some(signal)),
                ScriptEvent::Pointer(signal) => assert_eq!(pointer.next(), Some(signal)),
                ScriptEvent::Wait(_) => {}
            }
        }
        assert_eq!((keys.next(), pointer.next()), (None, None));
    }
}
