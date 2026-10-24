//! A scripted [`DragMonitor`] for app-level tests. No OS needed.
//!
//! [`ScriptedMonitor`] replays a fixed list of [`DragSignal`]s from a worker
//! thread, in order. [`scripts`] has ready-made sequences for the flows in
//! `docs/INTERACTION_SPEC.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{DragMonitor, DragSignal, PlatformError};

/// How long the worker waits before retrying when the channel is full.
const RETRY_DELAY: Duration = Duration::from_millis(1);

/// Replays a script of signals on a worker thread.
///
/// Unlike a real monitor, it never drops a signal: when the channel is full
/// it waits, so tests see the whole script. When the script is done the
/// worker drops its sender, so `rx.iter()` ends.
#[derive(Debug)]
pub struct ScriptedMonitor {
    script: Vec<DragSignal>,
    interval: Duration,
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ScriptedMonitor {
    /// A monitor that sends `script` as fast as the receiver takes it.
    pub fn new(script: Vec<DragSignal>) -> Self {
        Self {
            script,
            interval: Duration::ZERO,
            stop_flag: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    /// Waits `interval` before each signal, to imitate a real drag's pace.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// The signals this monitor sends.
    pub fn script(&self) -> &[DragSignal] {
        &self.script
    }
}

impl DragMonitor for ScriptedMonitor {
    fn start(&mut self, tx: SyncSender<DragSignal>) -> Result<(), PlatformError> {
        if self.worker.is_some() {
            return Err(PlatformError::AlreadyRunning);
        }
        let stop_flag = Arc::new(AtomicBool::new(false));
        self.stop_flag = Arc::clone(&stop_flag);
        let script = self.script.clone();
        let interval = self.interval;
        self.worker = Some(thread::spawn(move || {
            replay(&script, interval, &tx, &stop_flag);
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

impl Drop for ScriptedMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn replay(
    script: &[DragSignal],
    interval: Duration,
    tx: &SyncSender<DragSignal>,
    stop_flag: &AtomicBool,
) {
    for &signal in script {
        if !interval.is_zero() {
            thread::sleep(interval);
        }
        let mut pending = signal;
        loop {
            if stop_flag.load(Ordering::Acquire) {
                return;
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
