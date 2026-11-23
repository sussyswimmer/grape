//! Runs a picked shortcut's steps in order (`docs/design/m10-shortcuts-wheel.md` §9).
//!
//! - [`Actuator`] is everything a run asks of the world outside, one
//!   method per need. [`SystemActuator`] is the real one; tests use a
//!   recording fake.
//! - [`run_steps`] is the sequencing and nothing else: which call follows
//!   which, the checks before the first step that presses keys, the waits,
//!   and where a run stops. It reads no clock and never logs.
//! - [`run_shortcut`] is what the `"grape-shortcut"` thread calls: it times
//!   [`run_steps`] and writes the log line.
//! - [`toast_for`] words an [`Outcome`] for the toast.
//!
//! A step holds what the user typed or picked. None of it reaches an
//! [`Outcome`], a [`Failure`] or a log line: those carry step kinds, places
//! and fixed words only. The one exception is the toast, which names the
//! shortcut.

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use grape_platform::keys::Chord;
use grape_platform::shortcuts::{
    self as platform, InputAccess, LocalTime, OpenTarget, Placement, Program, ProgramRequest,
    SystemControl, SystemPlan,
};
use grape_platform::{Modifiers, PlatformError};

use super::model::{
    date_text, expand_text, needs_clipboard, runnable, time_text, ClipboardAction, ShortcutRun,
    StepKind, StepKindId,
};

/// One slice of a wait. A cancel is noticed within one.
const SLICE: Duration = Duration::from_millis(20);
/// How long the user has to let go of the modifier keys.
const KEYS_WAIT: Duration = Duration::from_millis(500);
/// How long the app the user was in has to come back to the front.
const FRONT_WAIT: Duration = Duration::from_millis(400);
/// How long a Copy has to reach the clipboard.
const COPY_WAIT: Duration = Duration::from_millis(500);
/// The longest a program may run.
const PROGRAM_LIMIT: Duration = Duration::from_secs(60);
/// How often a running program is looked at.
const PROGRAM_POLL: Duration = Duration::from_millis(10);
/// How long a program has to end by itself once its group was asked to:
/// a little longer than the grace `kill_group` gives on the Mac. Elsewhere
/// `kill_group` does nothing, so there is nothing to wait for.
const END_GRACE: Duration = Duration::from_millis(if cfg!(target_os = "macos") { 600 } else { 0 });

/// How a program a step ran ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramEnd {
    /// It exited with 0.
    Success,
    /// It exited with something else, or a signal ended it.
    Failed,
    /// It couldn't be started.
    NotStarted,
    /// It was still running at the time limit and was ended.
    TimedOut,
    /// The cancel flag was set, and it was ended.
    Cancelled,
}

/// Everything a run needs from outside. Each method is one thing; the
/// order they are called in is [`run_steps`]' business.
pub trait Actuator {
    /// Whether Grape may press keys and move windows.
    fn input_access(&self) -> InputAccess;
    /// Asks the OS for input access. The real one asks once per launch.
    fn request_access(&self);
    /// The modifier keys held right now.
    fn held_modifiers(&self) -> Modifiers;
    /// Waits up to `timeout` for the app `pid` to be in front.
    fn ensure_front(&self, pid: i32, timeout: Duration) -> bool;
    /// Presses `chord` from its place on the keyboard.
    ///
    /// # Errors
    /// When the key events couldn't be made.
    fn send_chord(&self, chord: Chord) -> Result<(), PlatformError>;
    /// Presses the key that types `ch`, with `mods`.
    ///
    /// # Errors
    /// As [`send_chord`](Self::send_chord), and when no key types `ch`.
    fn send_char_chord(&self, ch: char, mods: Modifiers) -> Result<(), PlatformError>;
    /// Types `text`, ending early without an error once `cancel` is set.
    ///
    /// # Errors
    /// As [`send_chord`](Self::send_chord).
    fn type_text(&self, text: &str, cancel: &AtomicBool) -> Result<(), PlatformError>;
    /// The text on the clipboard, if it holds any.
    ///
    /// # Errors
    /// When the clipboard couldn't be read.
    fn clipboard_text(&self) -> Result<Option<String>, PlatformError>;
    /// A number that moves whenever the clipboard changes.
    ///
    /// # Errors
    /// When the clipboard couldn't be read.
    fn clipboard_change_count(&self) -> Result<i64, PlatformError>;
    /// Empties the clipboard.
    ///
    /// # Errors
    /// When the clipboard couldn't be changed.
    fn clear_clipboard(&self) -> Result<(), PlatformError>;
    /// Opens an app, a file, a folder or a link, and waits for the answer.
    ///
    /// # Errors
    /// When it couldn't be opened.
    fn open_target(&self, target: OpenTarget<'_>) -> Result<(), PlatformError>;
    /// The program that does `request` here.
    ///
    /// # Errors
    /// Where this OS has none.
    fn program_for(&self, request: ProgramRequest<'_>) -> Result<Program, PlatformError>;
    /// How `control` is done here.
    ///
    /// # Errors
    /// Where this OS can't.
    fn system_plan(&self, control: SystemControl) -> Result<SystemPlan, PlatformError>;
    /// Moves the focused window of the app `pid`.
    ///
    /// # Errors
    /// When there is no window to move, or it refused.
    fn place_window(&self, pid: i32, placement: Placement) -> Result<(), PlatformError>;
    /// Now, on the user's clock.
    fn local_date_time(&self) -> Option<LocalTime>;
    /// Runs `program` until it ends, `cancel` is set or `limit` has passed.
    fn run_program(&self, program: &Program, cancel: &AtomicBool, limit: Duration) -> ProgramEnd;
    /// Shows a Notify step's text in Grape's toast.
    fn notify(&self, text: &str);
    /// Waits.
    fn sleep(&self, time: Duration);
}

/// Why a run stopped. Holds nothing the user typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Grape isn't allowed to press keys.
    NeedsAccess,
    /// A modifier key was still held.
    KeysHeld,
    /// The app the user was in didn't come back to the front.
    NotInFront,
    /// No app was in front when the wheel opened.
    NoFrontApp,
    /// The OS call the step makes failed.
    Platform(PlatformError),
    /// The step's program couldn't be started.
    NotStarted,
    /// The step's program ended with an error.
    ExitedWithError,
    /// The step's program ran past the time limit.
    TimedOut,
}

/// A few fixed words for the log.
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsAccess => f.write_str("no input access"),
            Self::KeysHeld => f.write_str("modifier keys still held"),
            Self::NotInFront => f.write_str("the app didn't come to the front"),
            Self::NoFrontApp => f.write_str("no app was in front"),
            // A platform error's own words name a call and a code.
            Self::Platform(err) => write!(f, "{err}"),
            Self::NotStarted => f.write_str("the program didn't start"),
            Self::ExitedWithError => f.write_str("the program ended with an error"),
            Self::TimedOut => f.write_str("the program ran too long"),
        }
    }
}

/// How a run ended. Holds nothing the user typed, so it is safe to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every step with something to do ran.
    Finished,
    /// No step had anything to do.
    NothingToRun,
    /// The cancel flag stopped it (Pause or Quit).
    Cancelled,
    /// A step failed, and the steps after it didn't run.
    Failed {
        /// The step's place in the shortcut, from 0.
        step: usize,
        /// The step's kind.
        kind: StepKindId,
        /// What went wrong.
        why: Failure,
    },
}

/// Why a step ended early.
enum Stop {
    Cancelled,
    Failed(Failure),
}

impl From<PlatformError> for Stop {
    fn from(err: PlatformError) -> Self {
        Self::Failed(Failure::Platform(err))
    }
}

impl From<Failure> for Stop {
    fn from(why: Failure) -> Self {
        Self::Failed(why)
    }
}

/// One run in progress.
struct Sequence<'a> {
    run: &'a ShortcutRun,
    actuator: &'a dyn Actuator,
    cancel: &'a AtomicBool,
    /// The front app's pid, once the checks before the first key press
    /// passed.
    front: Option<i32>,
}

impl Sequence<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Stops here once the run is cancelled. Called before every call
    /// that might go to the main thread, which Quit may be waiting on.
    fn live(&self) -> Result<(), Stop> {
        if self.cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    /// The checks before the first step that presses keys, types, copies,
    /// pastes or moves a window. They run once; later steps go straight
    /// through. Returns the front app's pid.
    fn gate(&mut self) -> Result<i32, Stop> {
        if let Some(pid) = self.front {
            return Ok(pid);
        }
        self.live()?;
        if self.actuator.input_access() == InputAccess::Denied {
            self.live()?;
            self.actuator.request_access();
            return Err(Failure::NeedsAccess.into());
        }
        // A modifier still held from the hot key would join every key
        // Grape presses.
        let mut waited = Duration::ZERO;
        loop {
            self.live()?;
            if self.actuator.held_modifiers() == Modifiers::NONE {
                break;
            }
            if waited >= KEYS_WAIT {
                return Err(Failure::KeysHeld.into());
            }
            self.live()?;
            self.actuator.sleep(SLICE);
            waited += SLICE;
        }
        let Some(front) = &self.run.front else {
            return Err(Failure::NoFrontApp.into());
        };
        self.live()?;
        if !self.actuator.ensure_front(front.pid, FRONT_WAIT) {
            return Err(Failure::NotInFront.into());
        }
        self.front = Some(front.pid);
        Ok(front.pid)
    }

    fn step(&mut self, kind: &StepKind) -> Result<(), Stop> {
        match kind {
            StepKind::Keys { chord } => {
                let Some(chord) = chord else {
                    return Ok(());
                };
                self.gate()?;
                self.live()?;
                self.actuator.send_chord(*chord)?;
            }
            StepKind::TypeText { text } => self.type_text(text)?,
            StepKind::Clipboard { action } => self.clipboard(*action)?,
            StepKind::OpenApp {
                path, bundle_id, ..
            } => {
                let Some(path) = path else {
                    return Ok(());
                };
                self.live()?;
                self.actuator.open_target(OpenTarget::App {
                    path,
                    id: bundle_id.as_deref(),
                })?;
            }
            StepKind::OpenFile { path, .. } => {
                let Some(path) = path else {
                    return Ok(());
                };
                self.live()?;
                self.actuator.open_target(OpenTarget::Path(path))?;
            }
            StepKind::OpenUrl { url } => {
                self.live()?;
                self.actuator.open_target(OpenTarget::Url(url))?;
            }
            StepKind::AppleShortcut { name } => {
                self.program_for(ProgramRequest::AppleShortcut(name))?;
            }
            StepKind::AppleScript { source } => {
                self.program_for(ProgramRequest::AppleScript(source))?;
            }
            StepKind::Shell { source } => self.program_for(ProgramRequest::Shell(source))?,
            StepKind::KeyboardMaestro { name } => {
                self.program_for(ProgramRequest::KeyboardMaestro(name))?;
            }
            StepKind::Window { placement } => {
                let pid = self.gate()?;
                self.live()?;
                self.actuator.place_window(pid, (*placement).into())?;
            }
            StepKind::System { control } => {
                self.live()?;
                match self.actuator.system_plan((*control).into())? {
                    SystemPlan::Program(program) => self.program(&program)?,
                    SystemPlan::CharChord { ch, mods } => {
                        self.gate()?;
                        self.live()?;
                        self.actuator.send_char_chord(ch, mods)?;
                    }
                }
            }
            StepKind::Notify { text } => {
                self.live()?;
                self.actuator.notify(text);
            }
            StepKind::Speak { text } => self.program_for(ProgramRequest::Speak(text))?,
            StepKind::Delay { ms } => {
                let mut left = Duration::from_millis(u64::from(*ms));
                while !left.is_zero() {
                    self.live()?;
                    let slice = left.min(SLICE);
                    self.actuator.sleep(slice);
                    left -= slice;
                }
            }
        }
        Ok(())
    }

    /// Fills in the variables and types the result. The clipboard is read
    /// only when the text asks for it.
    fn type_text(&mut self, text: &str) -> Result<(), Stop> {
        self.gate()?;
        self.live()?;
        let now = self.actuator.local_date_time();
        let clipboard = if needs_clipboard(text) {
            self.live()?;
            self.actuator.clipboard_text()?
        } else {
            None
        };
        let typed = expand_text(
            text,
            now.map(date_text).as_deref(),
            now.map(time_text).as_deref(),
            clipboard.as_deref(),
        );
        if typed.is_empty() {
            return Ok(());
        }
        self.live()?;
        self.actuator.type_text(&typed, self.cancel)?;
        // A cancel ends the typing early without an error.
        self.live()
    }

    fn clipboard(&mut self, action: ClipboardAction) -> Result<(), Stop> {
        match action {
            ClipboardAction::Copy => {
                self.gate()?;
                self.live()?;
                let before = self.actuator.clipboard_change_count()?;
                self.live()?;
                self.actuator.send_char_chord('c', Modifiers::META)?;
                // The app in front copies in its own time: the next step
                // must find what it copied. A Copy that never lands (nothing
                // was selected) isn't a failure.
                let mut waited = Duration::ZERO;
                while waited < COPY_WAIT {
                    self.live()?;
                    self.actuator.sleep(SLICE);
                    waited += SLICE;
                    self.live()?;
                    if self.actuator.clipboard_change_count()? != before {
                        break;
                    }
                }
            }
            ClipboardAction::Paste => {
                self.gate()?;
                self.live()?;
                self.actuator.send_char_chord('v', Modifiers::META)?;
            }
            ClipboardAction::Clear => {
                self.live()?;
                self.actuator.clear_clipboard()?;
            }
        }
        Ok(())
    }

    fn program_for(&self, request: ProgramRequest<'_>) -> Result<(), Stop> {
        self.live()?;
        let program = self.actuator.program_for(request)?;
        self.program(&program)
    }

    fn program(&self, program: &Program) -> Result<(), Stop> {
        self.live()?;
        match self
            .actuator
            .run_program(program, self.cancel, PROGRAM_LIMIT)
        {
            ProgramEnd::Success => Ok(()),
            ProgramEnd::Failed => Err(Failure::ExitedWithError.into()),
            ProgramEnd::NotStarted => Err(Failure::NotStarted.into()),
            ProgramEnd::TimedOut => Err(Failure::TimedOut.into()),
            ProgramEnd::Cancelled => Err(Stop::Cancelled),
        }
    }
}

/// [`run_steps`], also noting the kind of every step it started.
fn sequence(
    run: &ShortcutRun,
    actuator: &dyn Actuator,
    cancel: &AtomicBool,
    ran: &mut Vec<StepKindId>,
) -> Outcome {
    if !runnable(&run.steps) {
        return Outcome::NothingToRun;
    }
    let mut state = Sequence {
        run,
        actuator,
        cancel,
        front: None,
    };
    for (index, step) in run.steps.iter().enumerate() {
        if !step.kind.is_set() {
            continue;
        }
        if state.cancelled() {
            return Outcome::Cancelled;
        }
        let kind = step.kind.id();
        ran.push(kind);
        match state.step(&step.kind) {
            Ok(()) => {}
            // A cancel ends a program and cuts a wait short, which can look
            // like a failure from here.
            Err(Stop::Cancelled) => return Outcome::Cancelled,
            Err(Stop::Failed(_)) if state.cancelled() => return Outcome::Cancelled,
            Err(Stop::Failed(why)) => {
                return Outcome::Failed {
                    step: index,
                    kind,
                    why,
                }
            }
        }
    }
    Outcome::Finished
}

/// Runs `run`'s steps in order through `actuator`, as the design note's §9
/// has it.
///
/// - Steps with nothing chosen or typed yet are skipped; when that leaves
///   nothing, the answer is [`Outcome::NothingToRun`] and nothing is called.
/// - Before the first step that presses keys, types, copies, pastes or
///   moves a window: input access, then up to 500 ms for the modifier keys
///   to be let go, then up to 400 ms for the app the user was in to be in
///   front. Once per run.
/// - The first failure stops the run.
/// - `cancel` is checked before every step, before every call inside one
///   and in every slice of a wait.
///
/// Pure sequencing: no clock, no log, no thread.
// Only the tests call it so far: the app goes through `run_shortcut`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn run_steps(run: &ShortcutRun, actuator: &dyn Actuator, cancel: &AtomicBool) -> Outcome {
    sequence(run, actuator, cancel, &mut Vec::new())
}

/// Runs `run` as [`run_steps`] does, and logs which kinds of step ran, how
/// long it took and how it ended. This is the one call the
/// `"grape-shortcut"` thread makes; the answer goes to [`toast_for`].
pub fn run_shortcut(run: &ShortcutRun, actuator: &dyn Actuator, cancel: &AtomicBool) -> Outcome {
    let started = Instant::now();
    let mut ran = Vec::new();
    let outcome = sequence(run, actuator, cancel, &mut ran);
    let line = log_line(run.seq, &ran, started.elapsed().as_millis(), &outcome);
    match outcome {
        Outcome::Failed { .. } => log::warn!("{line}"),
        _ => log::info!("{line}"),
    }
    outcome
}

/// A run's log line: the wheel it came from, the kinds of the steps that
/// were started, the time and the ending. It takes no text, so it can't
/// repeat any.
pub fn log_line(seq: u64, ran: &[StepKindId], elapsed_ms: u128, outcome: &Outcome) -> String {
    let kinds: Vec<&str> = ran.iter().map(|kind| kind.id()).collect();
    let kinds = kinds.join(", ");
    let ending = match outcome {
        Outcome::Finished => "finished".to_owned(),
        Outcome::NothingToRun => "nothing to run".to_owned(),
        Outcome::Cancelled => "cancelled".to_owned(),
        Outcome::Failed { step, kind, why } => {
            format!("step {} ({}) failed: {why}", step + 1, kind.id())
        }
    };
    format!("shortcut {seq}: ran [{kinds}] in {elapsed_ms} ms: {ending}")
}

// ---------------------------------------------------------------------
// The toast.
// ---------------------------------------------------------------------

const NOTHING_TO_RUN: &str = "Nothing to run yet";
const NOTHING_TO_RUN_DETAIL: &str = "Add a step to it in Settings.";
const NEEDS_ACCESS: &str =
    "Grape needs Accessibility access to press keys. Turn it on in System Settings, then try again.";
const KEYS_HELD: &str = "Let go of the keys first.";
const NOT_IN_FRONT: &str = "The app you were in didn't come back to the front.";
const NO_FRONT_APP: &str = "Click the app you want first.";
const NOT_HERE: &str = "That step doesn't work on this computer yet.";
const TIMED_OUT: &str = "A step was still going after a minute, so Grape stopped it.";

/// What a toast about a run says. `Debug` prints neither line: the title
/// names the shortcut.
#[derive(Clone, PartialEq, Eq)]
pub struct RunToast {
    /// The main line.
    pub title: String,
    /// The second line, if any.
    pub detail: Option<String>,
    /// `true` for the error look, `false` for the info look.
    pub error: bool,
}

impl fmt::Debug for RunToast {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunToast")
            .field("detail", &self.detail.is_some())
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// The toast for a run of the shortcut `name` that ended with `outcome`.
/// `None` when there is nothing to say: it finished, or was cancelled.
pub fn toast_for(name: &str, outcome: &Outcome) -> Option<RunToast> {
    match outcome {
        Outcome::Finished | Outcome::Cancelled => None,
        Outcome::NothingToRun => Some(RunToast {
            title: NOTHING_TO_RUN.to_owned(),
            detail: Some(NOTHING_TO_RUN_DETAIL.to_owned()),
            error: false,
        }),
        Outcome::Failed { kind, why, .. } => Some(RunToast {
            title: format!("Couldn't finish {name}"),
            detail: Some(sentence(*kind, why).to_owned()),
            error: true,
        }),
    }
}

/// The toast for a press of the hot key while the shortcut `name` is still
/// running.
pub fn still_running(name: &str) -> RunToast {
    RunToast {
        title: format!("Still running {name}"),
        detail: None,
        error: false,
    }
}

/// The toast a Notify step of the shortcut `name` shows: the shortcut's
/// name over the step's text, on one line.
pub fn notice(name: &str, text: &str) -> RunToast {
    let line: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    RunToast {
        title: name.to_owned(),
        detail: Some(line),
        error: false,
    }
}

/// One plain sentence for a step of `kind` that failed for `why`.
fn sentence(kind: StepKindId, why: &Failure) -> &'static str {
    match why {
        Failure::NeedsAccess => NEEDS_ACCESS,
        Failure::KeysHeld => KEYS_HELD,
        Failure::NotInFront => NOT_IN_FRONT,
        Failure::NoFrontApp => NO_FRONT_APP,
        Failure::Platform(PlatformError::Unsupported) => NOT_HERE,
        Failure::TimedOut => TIMED_OUT,
        Failure::Platform(_) | Failure::NotStarted | Failure::ExitedWithError => match kind {
            StepKindId::Keys => "Grape couldn't press those keys.",
            StepKindId::TypeText => "Grape couldn't type that text.",
            StepKindId::Clipboard => "Grape couldn't use the clipboard.",
            StepKindId::OpenApp => "That app wouldn't open.",
            StepKindId::OpenFile => "That file or folder wouldn't open. It may have moved.",
            StepKindId::OpenUrl => "That link wouldn't open.",
            StepKindId::AppleShortcut => "That Apple Shortcut didn't run. Check its name.",
            StepKindId::AppleScript => "The AppleScript stopped with an error.",
            StepKindId::Shell => "The shell script stopped with an error.",
            StepKindId::KeyboardMaestro => {
                "Keyboard Maestro didn't run that macro. Check its name."
            }
            StepKindId::Window => "That window wouldn't move.",
            StepKindId::System => "The system wouldn't do that.",
            StepKindId::Speak => "Grape couldn't say that.",
            StepKindId::Notify | StepKindId::Delay => "A step didn't work.",
        },
    }
}

// ---------------------------------------------------------------------
// The real actuator.
// ---------------------------------------------------------------------

/// Set once Grape has asked for input access in this launch.
static ACCESS_ASKED: AtomicBool = AtomicBool::new(false);

/// `true` the first time it is called with `flag`, `false` ever after.
fn first_time(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::SeqCst)
}

/// `true` when Grape hasn't asked for input access in this launch yet,
/// and counts this as the asking. A run and the Settings button share the
/// one flag, so the system's prompt comes once whichever asks first.
pub fn first_access_request() -> bool {
    first_time(&ACCESS_ASKED)
}

/// The program a step is running right now, if any, for whoever cancels
/// the run. One is shared by the app's state and the [`SystemActuator`].
#[derive(Debug, Default)]
pub struct RunningChild(Mutex<Option<u32>>);

impl RunningChild {
    /// None running.
    // Only the tests call it so far: the app's state uses `Default`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ends the running program, and everything it started. Set the cancel
    /// flag first: the run then stops instead of calling it a failure.
    /// Does nothing when no program is running. Returns at once.
    pub fn kill(&self) {
        if let Some(pid) = *self.lock() {
            platform::kill_group(pid);
        }
    }

    /// The running program's pid.
    // Only the tests read it so far.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pid(&self) -> Option<u32> {
        *self.lock()
    }

    // A pid is held only while its process hasn't been waited for, and the
    // wait happens under this lock. So `kill` never signals a group whose
    // id went to another process.
    fn lock(&self) -> MutexGuard<'_, Option<u32>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Looks whether `child` has ended, and forgets it when it has.
    fn poll(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
        let mut pid = self.lock();
        let status = child.try_wait();
        if matches!(status, Ok(Some(_))) {
            *pid = None;
        }
        status
    }

    /// Ends `child` and its group: asked first, killed after
    /// [`END_GRACE`]. Returns once it is gone.
    fn end(&self, child: &mut Child) {
        platform::kill_group(child.id());
        let asked = Instant::now();
        while asked.elapsed() < END_GRACE {
            if !matches!(self.poll(child), Ok(None)) {
                break;
            }
            thread::sleep(PROGRAM_POLL);
        }
        let mut pid = self.lock();
        // Both fail only when it is gone already.
        let _ = child.kill();
        let _ = child.wait();
        *pid = None;
    }
}

/// The [`Actuator`] that does things: `grape_platform::shortcuts` for the
/// OS, `std::process` for programs.
pub struct SystemActuator {
    home: Option<PathBuf>,
    child: Arc<RunningChild>,
    notify: Box<dyn Fn(&str) + Send + Sync>,
}

impl SystemActuator {
    /// - `home` is the user's home folder, where programs run. With none,
    ///   no program starts.
    /// - `child` is where the running program's pid is kept for
    ///   [`RunningChild::kill`].
    /// - `notify` shows a Notify step's text. It is called on the run's
    ///   thread.
    pub fn new(
        home: Option<PathBuf>,
        child: Arc<RunningChild>,
        notify: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            home,
            child,
            notify: Box::new(notify),
        }
    }
}

/// Puts the program in a process group of its own, so it and everything it
/// starts can be ended together and a signal to Grape's group passes it by.
#[cfg(unix)]
fn own_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn own_group(_command: &mut Command) {}

/// Writes `text` to the program's standard input and closes it, on a
/// thread of its own: a shell reads its script as it goes, so a long one
/// would otherwise hold the run past its time limit. `false` when the
/// thread couldn't start.
fn feed(mut pipe: ChildStdin, text: String) -> bool {
    thread::Builder::new()
        .name("grape-shortcut-stdin".to_owned())
        .spawn(move || {
            // A program that ends before it read everything closes the
            // pipe. Its exit status says how it went.
            let _ = pipe.write_all(text.as_bytes());
        })
        .is_ok()
}

impl Actuator for SystemActuator {
    fn input_access(&self) -> InputAccess {
        platform::input_access()
    }

    fn request_access(&self) {
        if !first_access_request() {
            return;
        }
        if let Err(err) = platform::request_input_access(false) {
            log::warn!("shortcut: couldn't ask for input access: {err}");
        }
    }

    fn held_modifiers(&self) -> Modifiers {
        platform::held_modifiers()
    }

    fn ensure_front(&self, pid: i32, timeout: Duration) -> bool {
        platform::ensure_front(pid, timeout)
    }

    fn send_chord(&self, chord: Chord) -> Result<(), PlatformError> {
        platform::send_chord(chord)
    }

    fn send_char_chord(&self, ch: char, mods: Modifiers) -> Result<(), PlatformError> {
        platform::send_char_chord(ch, mods)
    }

    fn type_text(&self, text: &str, cancel: &AtomicBool) -> Result<(), PlatformError> {
        platform::type_text(text, cancel)
    }

    fn clipboard_text(&self) -> Result<Option<String>, PlatformError> {
        platform::clipboard_text()
    }

    fn clipboard_change_count(&self) -> Result<i64, PlatformError> {
        platform::clipboard_change_count()
    }

    fn clear_clipboard(&self) -> Result<(), PlatformError> {
        platform::clear_clipboard()
    }

    fn open_target(&self, target: OpenTarget<'_>) -> Result<(), PlatformError> {
        platform::open_target(target)
    }

    fn program_for(&self, request: ProgramRequest<'_>) -> Result<Program, PlatformError> {
        platform::program_for(request)
    }

    fn system_plan(&self, control: SystemControl) -> Result<SystemPlan, PlatformError> {
        platform::system_plan(control)
    }

    fn place_window(&self, pid: i32, placement: Placement) -> Result<(), PlatformError> {
        platform::place_window(pid, placement)
    }

    fn local_date_time(&self) -> Option<LocalTime> {
        platform::local_date_time()
    }

    fn run_program(&self, program: &Program, cancel: &AtomicBool, limit: Duration) -> ProgramEnd {
        let Some(home) = &self.home else {
            log::warn!("shortcut: no home folder to run a program in");
            return ProgramEnd::NotStarted;
        };
        let mut command = Command::new(&program.program);
        command
            .args(&program.args)
            .current_dir(home)
            .stdin(if program.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            // What a program prints is never read.
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        own_group(&mut command);
        let mut child = {
            // Under the lock, so a cancel never finds a program that has
            // started and no pid.
            let mut pid = self.child.lock();
            match command.spawn() {
                Ok(child) => {
                    *pid = Some(child.id());
                    child
                }
                Err(err) => {
                    // The kind only: the error's own words can hold a path.
                    log::warn!("shortcut: a program didn't start: {}", err.kind());
                    return ProgramEnd::NotStarted;
                }
            }
        };
        if let (Some(text), Some(pipe)) = (&program.stdin, child.stdin.take()) {
            if !feed(pipe, text.clone()) {
                log::warn!("shortcut: no thread to hand a program its input");
                self.child.end(&mut child);
                return ProgramEnd::NotStarted;
            }
        }
        let started = Instant::now();
        loop {
            if cancel.load(Ordering::SeqCst) {
                self.child.end(&mut child);
                return ProgramEnd::Cancelled;
            }
            match self.child.poll(&mut child) {
                Ok(Some(status)) if status.success() => return ProgramEnd::Success,
                Ok(Some(_)) => return ProgramEnd::Failed,
                Ok(None) => {}
                Err(err) => {
                    log::warn!("shortcut: lost track of a program: {}", err.kind());
                    self.child.end(&mut child);
                    return ProgramEnd::Failed;
                }
            }
            if started.elapsed() >= limit {
                self.child.end(&mut child);
                return ProgramEnd::TimedOut;
            }
            thread::sleep(PROGRAM_POLL);
        }
    }

    fn notify(&self, text: &str) {
        (self.notify)(text);
    }

    fn sleep(&self, time: Duration) {
        thread::sleep(time);
    }
}

/// A recording [`Actuator`] for tests: it notes every call, answers from a
/// script and touches nothing.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use super::*;

    /// What was asked to open.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Opened {
        App { path: PathBuf, id: Option<String> },
        Path(PathBuf),
        Url(String),
    }

    /// What a program was asked for.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Asked {
        AppleScript(String),
        Shell(String),
        AppleShortcut(String),
        KeyboardMaestro(String),
        Speak(String),
    }

    /// One call, with what it was given.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Call {
        InputAccess,
        RequestAccess,
        HeldModifiers,
        EnsureFront(i32, Duration),
        SendChord(Chord),
        SendCharChord(char, Modifiers),
        TypeText(String),
        ClipboardText,
        ChangeCount,
        ClearClipboard,
        Open(Opened),
        ProgramFor(Asked),
        SystemPlan(SystemControl),
        PlaceWindow(i32, Placement),
        LocalDateTime,
        RunProgram(Program, Duration),
        Notify(String),
        Sleep(Duration),
    }

    /// A call that can fail.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Fallible {
        SendChord,
        SendCharChord,
        TypeText,
        ClipboardText,
        ChangeCount,
        ClearClipboard,
        Open,
        ProgramFor,
        SystemPlan,
        PlaceWindow,
    }

    /// The error a call told to fail gives.
    pub(crate) const BROKEN: PlatformError = PlatformError::Os {
        context: "fake",
        code: 5,
    };

    /// The program the fake gives for a request: the kind as its path, the
    /// text on its standard input.
    pub(crate) fn program(kind: &str, text: &str) -> Program {
        Program {
            program: PathBuf::from(format!("/fake/{kind}")),
            args: Vec::new(),
            stdin: Some(text.to_owned()),
        }
    }

    pub(crate) struct FakeActuator {
        calls: RefCell<Vec<Call>>,
        flag: AtomicBool,
        cancel_after: Cell<Option<usize>>,
        /// Input access. Granted at first.
        pub(crate) access: Cell<InputAccess>,
        /// What the next reads of the modifiers say, in order.
        pub(crate) held: RefCell<VecDeque<Modifiers>>,
        /// What they say once `held` is used up. None at first.
        pub(crate) held_after: Cell<Modifiers>,
        /// Whether the app comes to the front. It does at first.
        pub(crate) front: Cell<bool>,
        /// What the next reads of the change count say, in order. The last
        /// one repeats.
        pub(crate) counts: RefCell<VecDeque<i64>>,
        /// The clipboard's text. None at first.
        pub(crate) clipboard: RefCell<Option<String>>,
        /// The clock. Unknown at first.
        pub(crate) now: Cell<Option<LocalTime>>,
        /// How every program ends. Well at first.
        pub(crate) program_end: Cell<ProgramEnd>,
        /// The call that fails with [`BROKEN`], and the one that says the
        /// OS can't.
        pub(crate) broken: Cell<Option<Fallible>>,
        pub(crate) unsupported: Cell<Option<Fallible>>,
    }

    impl FakeActuator {
        pub(crate) fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                flag: AtomicBool::new(false),
                cancel_after: Cell::new(None),
                access: Cell::new(InputAccess::Granted),
                held: RefCell::new(VecDeque::new()),
                held_after: Cell::new(Modifiers::NONE),
                front: Cell::new(true),
                counts: RefCell::new(VecDeque::from([7])),
                clipboard: RefCell::new(None),
                now: Cell::new(None),
                program_end: Cell::new(ProgramEnd::Success),
                broken: Cell::new(None),
                unsupported: Cell::new(None),
            }
        }

        /// The cancel flag to hand to the runner.
        pub(crate) fn cancel(&self) -> &AtomicBool {
            &self.flag
        }

        /// Sets the cancel flag during the call that makes `calls` of them.
        pub(crate) fn cancel_after(&self, calls: usize) {
            self.cancel_after.set(Some(calls));
        }

        /// Every call so far, in order.
        pub(crate) fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        fn note(&self, call: Call) {
            let mut calls = self.calls.borrow_mut();
            calls.push(call);
            if self.cancel_after.get() == Some(calls.len()) {
                self.flag.store(true, Ordering::SeqCst);
            }
        }

        fn answer(&self, call: Fallible) -> Result<(), PlatformError> {
            if self.broken.get() == Some(call) {
                Err(BROKEN)
            } else if self.unsupported.get() == Some(call) {
                Err(PlatformError::Unsupported)
            } else {
                Ok(())
            }
        }
    }

    impl Actuator for FakeActuator {
        fn input_access(&self) -> InputAccess {
            self.note(Call::InputAccess);
            self.access.get()
        }

        fn request_access(&self) {
            self.note(Call::RequestAccess);
        }

        fn held_modifiers(&self) -> Modifiers {
            self.note(Call::HeldModifiers);
            self.held
                .borrow_mut()
                .pop_front()
                .unwrap_or(self.held_after.get())
        }

        fn ensure_front(&self, pid: i32, timeout: Duration) -> bool {
            self.note(Call::EnsureFront(pid, timeout));
            self.front.get()
        }

        fn send_chord(&self, chord: Chord) -> Result<(), PlatformError> {
            self.note(Call::SendChord(chord));
            self.answer(Fallible::SendChord)
        }

        fn send_char_chord(&self, ch: char, mods: Modifiers) -> Result<(), PlatformError> {
            self.note(Call::SendCharChord(ch, mods));
            self.answer(Fallible::SendCharChord)
        }

        fn type_text(&self, text: &str, _cancel: &AtomicBool) -> Result<(), PlatformError> {
            self.note(Call::TypeText(text.to_owned()));
            self.answer(Fallible::TypeText)
        }

        fn clipboard_text(&self) -> Result<Option<String>, PlatformError> {
            self.note(Call::ClipboardText);
            self.answer(Fallible::ClipboardText)?;
            Ok(self.clipboard.borrow().clone())
        }

        fn clipboard_change_count(&self) -> Result<i64, PlatformError> {
            self.note(Call::ChangeCount);
            self.answer(Fallible::ChangeCount)?;
            let mut counts = self.counts.borrow_mut();
            let count = match counts.len() {
                0 => 0,
                1 => counts[0],
                _ => counts.pop_front().unwrap_or_default(),
            };
            Ok(count)
        }

        fn clear_clipboard(&self) -> Result<(), PlatformError> {
            self.note(Call::ClearClipboard);
            self.answer(Fallible::ClearClipboard)
        }

        fn open_target(&self, target: OpenTarget<'_>) -> Result<(), PlatformError> {
            self.note(Call::Open(match target {
                OpenTarget::App { path, id } => Opened::App {
                    path: path.to_path_buf(),
                    id: id.map(str::to_owned),
                },
                OpenTarget::Path(path) => Opened::Path(path.to_path_buf()),
                OpenTarget::Url(url) => Opened::Url(url.to_owned()),
            }));
            self.answer(Fallible::Open)
        }

        fn program_for(&self, request: ProgramRequest<'_>) -> Result<Program, PlatformError> {
            let (asked, made) = match request {
                ProgramRequest::AppleScript(text) => (
                    Asked::AppleScript(text.to_owned()),
                    program("appleScript", text),
                ),
                ProgramRequest::Shell(text) => {
                    (Asked::Shell(text.to_owned()), program("shell", text))
                }
                ProgramRequest::AppleShortcut(text) => (
                    Asked::AppleShortcut(text.to_owned()),
                    program("appleShortcut", text),
                ),
                ProgramRequest::KeyboardMaestro(text) => (
                    Asked::KeyboardMaestro(text.to_owned()),
                    program("keyboardMaestro", text),
                ),
                ProgramRequest::Speak(text) => {
                    (Asked::Speak(text.to_owned()), program("speak", text))
                }
            };
            self.note(Call::ProgramFor(asked));
            self.answer(Fallible::ProgramFor)?;
            Ok(made)
        }

        /// Lock screen and Screenshot are chords, as on the Mac; the rest
        /// are programs.
        fn system_plan(&self, control: SystemControl) -> Result<SystemPlan, PlatformError> {
            self.note(Call::SystemPlan(control));
            self.answer(Fallible::SystemPlan)?;
            Ok(match control {
                SystemControl::LockScreen => SystemPlan::CharChord {
                    ch: 'q',
                    mods: Modifiers {
                        ctrl: true,
                        meta: true,
                        ..Modifiers::NONE
                    },
                },
                SystemControl::ScreenshotArea => SystemPlan::CharChord {
                    ch: '4',
                    mods: Modifiers {
                        shift: true,
                        meta: true,
                        ..Modifiers::NONE
                    },
                },
                _ => SystemPlan::Program(program("system", "")),
            })
        }

        fn place_window(&self, pid: i32, placement: Placement) -> Result<(), PlatformError> {
            self.note(Call::PlaceWindow(pid, placement));
            self.answer(Fallible::PlaceWindow)
        }

        fn local_date_time(&self) -> Option<LocalTime> {
            self.note(Call::LocalDateTime);
            self.now.get()
        }

        fn run_program(
            &self,
            program: &Program,
            _cancel: &AtomicBool,
            limit: Duration,
        ) -> ProgramEnd {
            self.note(Call::RunProgram(program.clone(), limit));
            self.program_end.get()
        }

        fn notify(&self, text: &str) {
            self.note(Call::Notify(text.to_owned()));
        }

        fn sleep(&self, time: Duration) {
            self.note(Call::Sleep(time));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use grape_platform::keys::KeyCode;
    use grape_platform::shortcuts::FrontApp;
    use grape_platform::{DisplayBounds, Rect};

    use super::fake::{program, Asked, Call, FakeActuator, Fallible, Opened, BROKEN};
    use super::*;
    use crate::shortcuts::model::{ControlId, PlacementId, Step};

    /// Something no log line and no printed outcome may ever hold.
    const MARKER: &str = "s3cret-marker";
    const PID: i32 = 501;
    const MS: fn(u64) -> Duration = Duration::from_millis;

    const SAVE: Chord = Chord {
        key: KeyCode::KeyS,
        mods: Modifiers::META,
    };

    fn run_of(kinds: Vec<StepKind>) -> ShortcutRun {
        let steps = kinds
            .into_iter()
            .zip(1..)
            .map(|(kind, id)| Step { id, kind })
            .collect();
        ShortcutRun {
            seq: 7,
            name: "Tidy up".to_owned(),
            steps,
            front: Some(FrontApp {
                pid: PID,
                id: Some("com.example.notes".to_owned()),
            }),
            anchor: (400, 300),
            display: DisplayBounds {
                monitor: Rect {
                    x: 0,
                    y: 0,
                    width: 1440,
                    height: 900,
                },
                work_area: Rect {
                    x: 0,
                    y: 25,
                    width: 1440,
                    height: 875,
                },
                scale: 1.0,
            },
        }
    }

    fn keys() -> StepKind {
        StepKind::Keys { chord: Some(SAVE) }
    }

    fn typed(text: &str) -> StepKind {
        StepKind::TypeText {
            text: text.to_owned(),
        }
    }

    fn clipboard(action: ClipboardAction) -> StepKind {
        StepKind::Clipboard { action }
    }

    fn url(url: &str) -> StepKind {
        StepKind::OpenUrl {
            url: url.to_owned(),
        }
    }

    fn shell(source: &str) -> StepKind {
        StepKind::Shell {
            source: source.to_owned(),
        }
    }

    fn notify(text: &str) -> StepKind {
        StepKind::Notify {
            text: text.to_owned(),
        }
    }

    fn delay(ms: u32) -> StepKind {
        StepKind::Delay { ms }
    }

    fn window(placement: PlacementId) -> StepKind {
        StepKind::Window { placement }
    }

    fn system(control: ControlId) -> StepKind {
        StepKind::System { control }
    }

    /// The calls of the checks before the first key press, when they pass.
    fn gate() -> Vec<Call> {
        vec![
            Call::InputAccess,
            Call::HeldModifiers,
            Call::EnsureFront(PID, MS(400)),
        ]
    }

    fn gate_then(calls: Vec<Call>) -> Vec<Call> {
        let mut all = gate();
        all.extend(calls);
        all
    }

    fn failed(step: usize, kind: StepKindId, why: Failure) -> Outcome {
        Outcome::Failed { step, kind, why }
    }

    /// Runs `kinds` with a fresh fake and expects it to finish.
    fn finished(kinds: Vec<StepKind>) -> Vec<Call> {
        let fake = FakeActuator::new();
        assert_eq!(
            run_steps(&run_of(kinds), &fake, fake.cancel()),
            Outcome::Finished
        );
        fake.calls()
    }

    // --- Each kind of step. ------------------------------------------

    #[test]
    fn keys_presses_the_chord_after_the_checks() {
        assert_eq!(
            finished(vec![keys()]),
            gate_then(vec![Call::SendChord(SAVE)])
        );
    }

    #[test]
    fn type_text_types_plain_text_without_reading_the_clipboard() {
        assert_eq!(
            finished(vec![typed("Hello {name}\n")]),
            gate_then(vec![
                Call::LocalDateTime,
                Call::TypeText("Hello {name}\n".to_owned())
            ])
        );
    }

    #[test]
    fn type_text_fills_in_the_date_the_time_and_the_clipboard() {
        let fake = FakeActuator::new();
        fake.now.set(Some(LocalTime {
            year: 2026,
            month: 11,
            day: 23,
            hour: 14,
            minute: 5,
        }));
        *fake.clipboard.borrow_mut() = Some("two\nlines".to_owned());
        let run = run_of(vec![typed("{date} {time}: {clipboard}")]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Finished);
        assert_eq!(
            fake.calls(),
            gate_then(vec![
                Call::LocalDateTime,
                Call::ClipboardText,
                Call::TypeText("2026-11-23 14:05: two lines".to_owned()),
            ])
        );
    }

    #[test]
    fn type_text_keeps_the_date_as_typed_when_the_clock_is_unknown() {
        assert_eq!(
            finished(vec![typed("{date}")]),
            gate_then(vec![
                Call::LocalDateTime,
                Call::TypeText("{date}".to_owned())
            ])
        );
    }

    #[test]
    fn type_text_types_nothing_when_an_empty_clipboard_is_all_there_is() {
        assert_eq!(
            finished(vec![typed("{clipboard}"), notify("after")]),
            gate_then(vec![
                Call::LocalDateTime,
                Call::ClipboardText,
                Call::Notify("after".to_owned()),
            ])
        );
    }

    #[test]
    fn copy_presses_the_chord_and_waits_for_the_clipboard_to_change() {
        let fake = FakeActuator::new();
        // The count before, two looks that find it unchanged, then the new one.
        *fake.counts.borrow_mut() = [7, 7, 7, 8].into();
        let run = run_of(vec![clipboard(ClipboardAction::Copy), notify("copied")]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Finished);
        assert_eq!(
            fake.calls(),
            gate_then(vec![
                Call::ChangeCount,
                Call::SendCharChord('c', Modifiers::META),
                Call::Sleep(MS(20)),
                Call::ChangeCount,
                Call::Sleep(MS(20)),
                Call::ChangeCount,
                Call::Sleep(MS(20)),
                Call::ChangeCount,
                Call::Notify("copied".to_owned()),
            ])
        );
    }

    #[test]
    fn a_copy_that_never_lands_waits_half_a_second_and_goes_on() {
        let calls = finished(vec![clipboard(ClipboardAction::Copy), notify("after")]);
        let sleeps: Vec<&Call> = calls
            .iter()
            .filter(|call| matches!(call, Call::Sleep(_)))
            .collect();
        assert_eq!(sleeps.len(), 25);
        assert!(sleeps.iter().all(|call| **call == Call::Sleep(MS(20))));
        // One look before the chord and one after every slice.
        let looks = calls
            .iter()
            .filter(|call| **call == Call::ChangeCount)
            .count();
        assert_eq!(looks, 26);
        assert_eq!(calls.last(), Some(&Call::Notify("after".to_owned())));
    }

    #[test]
    fn paste_presses_its_chord_and_clear_needs_no_checks() {
        assert_eq!(
            finished(vec![clipboard(ClipboardAction::Paste)]),
            gate_then(vec![Call::SendCharChord('v', Modifiers::META)])
        );
        assert_eq!(
            finished(vec![clipboard(ClipboardAction::Clear)]),
            vec![Call::ClearClipboard]
        );
    }

    #[test]
    fn the_open_steps_open_their_targets_without_the_checks() {
        let app = PathBuf::from("/Applications/Notes.app");
        let file = PathBuf::from("/Users/max/todo.txt");
        let calls = finished(vec![
            StepKind::OpenApp {
                path: Some(app.clone()),
                bundle_id: Some("com.example.notes".to_owned()),
                name: Some("Notes".to_owned()),
            },
            StepKind::OpenApp {
                path: Some(app.clone()),
                bundle_id: None,
                name: Some("Notes".to_owned()),
            },
            StepKind::OpenFile {
                path: Some(file.clone()),
                folder: false,
            },
            url("https://example.com/a?b=c"),
        ]);
        assert_eq!(
            calls,
            vec![
                Call::Open(Opened::App {
                    path: app.clone(),
                    id: Some("com.example.notes".to_owned()),
                }),
                Call::Open(Opened::App {
                    path: app,
                    id: None
                }),
                Call::Open(Opened::Path(file)),
                Call::Open(Opened::Url("https://example.com/a?b=c".to_owned())),
            ]
        );
    }

    #[test]
    fn the_program_steps_ask_for_their_program_and_run_it_for_a_minute_at_most() {
        let minute = Duration::from_secs(60);
        let calls = finished(vec![
            StepKind::AppleShortcut {
                name: "Morning".to_owned(),
            },
            StepKind::AppleScript {
                source: "beep".to_owned(),
            },
            shell("true"),
            StepKind::KeyboardMaestro {
                name: "Tidy".to_owned(),
            },
            StepKind::Speak {
                text: "hello".to_owned(),
            },
        ]);
        assert_eq!(
            calls,
            vec![
                Call::ProgramFor(Asked::AppleShortcut("Morning".to_owned())),
                Call::RunProgram(program("appleShortcut", "Morning"), minute),
                Call::ProgramFor(Asked::AppleScript("beep".to_owned())),
                Call::RunProgram(program("appleScript", "beep"), minute),
                Call::ProgramFor(Asked::Shell("true".to_owned())),
                Call::RunProgram(program("shell", "true"), minute),
                Call::ProgramFor(Asked::KeyboardMaestro("Tidy".to_owned())),
                Call::RunProgram(program("keyboardMaestro", "Tidy"), minute),
                Call::ProgramFor(Asked::Speak("hello".to_owned())),
                Call::RunProgram(program("speak", "hello"), minute),
            ]
        );
    }

    #[test]
    fn window_moves_the_front_apps_window_after_the_checks() {
        assert_eq!(
            finished(vec![window(PlacementId::RightHalf)]),
            gate_then(vec![Call::PlaceWindow(PID, Placement::RightHalf)])
        );
    }

    #[test]
    fn a_system_step_runs_its_program_without_the_checks() {
        assert_eq!(
            finished(vec![system(ControlId::ToggleMute)]),
            vec![
                Call::SystemPlan(SystemControl::ToggleMute),
                Call::RunProgram(program("system", ""), Duration::from_secs(60)),
            ]
        );
    }

    #[test]
    fn a_system_step_that_is_a_chord_gets_the_checks_first() {
        let lock = Modifiers {
            ctrl: true,
            meta: true,
            ..Modifiers::NONE
        };
        let mut expected = vec![Call::SystemPlan(SystemControl::LockScreen)];
        expected.extend(gate());
        expected.push(Call::SendCharChord('q', lock));
        assert_eq!(finished(vec![system(ControlId::LockScreen)]), expected);
    }

    #[test]
    fn notify_shows_its_text_and_delay_sleeps_in_slices() {
        assert_eq!(
            finished(vec![notify("Done.\nReally."), delay(50), delay(20)]),
            vec![
                Call::Notify("Done.\nReally.".to_owned()),
                Call::Sleep(MS(20)),
                Call::Sleep(MS(20)),
                Call::Sleep(MS(10)),
                Call::Sleep(MS(20)),
            ]
        );
    }

    #[test]
    fn steps_run_in_the_order_they_are_in() {
        let calls = finished(vec![
            url("https://example.com/"),
            delay(20),
            keys(),
            notify("saved"),
        ]);
        let mut expected = vec![
            Call::Open(Opened::Url("https://example.com/".to_owned())),
            Call::Sleep(MS(20)),
        ];
        expected.extend(gate());
        expected.extend([Call::SendChord(SAVE), Call::Notify("saved".to_owned())]);
        assert_eq!(calls, expected);
    }

    // --- Steps with nothing to do. -----------------------------------

    #[test]
    fn unset_steps_are_skipped() {
        let calls = finished(vec![
            StepKind::Keys { chord: None },
            typed(""),
            StepKind::OpenApp {
                path: None,
                bundle_id: None,
                name: None,
            },
            StepKind::OpenFile {
                path: None,
                folder: false,
            },
            url(""),
            StepKind::AppleShortcut {
                name: String::new(),
            },
            StepKind::AppleScript {
                source: String::new(),
            },
            shell(""),
            StepKind::KeyboardMaestro {
                name: String::new(),
            },
            notify(""),
            StepKind::Speak {
                text: String::new(),
            },
            notify("the only one"),
        ]);
        assert_eq!(calls, vec![Call::Notify("the only one".to_owned())]);
    }

    #[test]
    fn nothing_to_run_calls_nothing() {
        for kinds in [
            Vec::new(),
            vec![StepKind::Keys { chord: None }, typed(""), url("")],
        ] {
            let fake = FakeActuator::new();
            assert_eq!(
                run_steps(&run_of(kinds), &fake, fake.cancel()),
                Outcome::NothingToRun
            );
            assert_eq!(fake.calls(), Vec::new());
        }
    }

    // --- The checks before the first key press. ----------------------

    #[test]
    fn the_checks_run_once_for_a_run() {
        let calls = finished(vec![
            keys(),
            typed("a"),
            clipboard(ClipboardAction::Paste),
            window(PlacementId::Fill),
            system(ControlId::ScreenshotArea),
        ]);
        let shot = Modifiers {
            shift: true,
            meta: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            calls,
            gate_then(vec![
                Call::SendChord(SAVE),
                Call::LocalDateTime,
                Call::TypeText("a".to_owned()),
                Call::SendCharChord('v', Modifiers::META),
                Call::PlaceWindow(PID, Placement::Fill),
                Call::SystemPlan(SystemControl::ScreenshotArea),
                Call::SendCharChord('4', shot),
            ])
        );
    }

    #[test]
    fn steps_that_press_nothing_never_run_the_checks() {
        let calls = finished(vec![
            clipboard(ClipboardAction::Clear),
            url("mailto:a@example.com"),
            shell("true"),
            system(ControlId::SleepDisplay),
            notify("n"),
            delay(10),
        ]);
        for call in gate() {
            assert!(!calls.contains(&call), "{call:?}");
        }
    }

    #[test]
    fn a_run_with_no_front_app_still_does_what_presses_nothing() {
        let fake = FakeActuator::new();
        let mut run = run_of(vec![notify("n"), shell("true")]);
        run.front = None;
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Finished);
    }

    #[test]
    fn where_no_access_is_needed_the_checks_go_on() {
        let fake = FakeActuator::new();
        fake.access.set(InputAccess::NotNeeded);
        let run = run_of(vec![keys()]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Finished);
        assert_eq!(fake.calls(), gate_then(vec![Call::SendChord(SAVE)]));
    }

    #[test]
    fn without_access_grape_asks_for_it_and_presses_nothing() {
        let fake = FakeActuator::new();
        fake.access.set(InputAccess::Denied);
        let run = run_of(vec![notify("first"), keys(), notify("never")]);
        assert_eq!(
            run_steps(&run, &fake, fake.cancel()),
            failed(1, StepKindId::Keys, Failure::NeedsAccess)
        );
        assert_eq!(
            fake.calls(),
            vec![
                Call::Notify("first".to_owned()),
                Call::InputAccess,
                Call::RequestAccess,
            ]
        );
    }

    #[test]
    fn keys_let_go_in_time_let_the_run_go_on() {
        let fake = FakeActuator::new();
        fake.held
            .borrow_mut()
            .extend([Modifiers::CTRL, Modifiers::ALT]);
        let run = run_of(vec![keys()]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Finished);
        assert_eq!(
            fake.calls(),
            vec![
                Call::InputAccess,
                Call::HeldModifiers,
                Call::Sleep(MS(20)),
                Call::HeldModifiers,
                Call::Sleep(MS(20)),
                Call::HeldModifiers,
                Call::EnsureFront(PID, MS(400)),
                Call::SendChord(SAVE),
            ]
        );
    }

    #[test]
    fn keys_held_for_half_a_second_stop_the_run() {
        let fake = FakeActuator::new();
        fake.held_after.set(Modifiers::SHIFT);
        let run = run_of(vec![typed("a")]);
        assert_eq!(
            run_steps(&run, &fake, fake.cancel()),
            failed(0, StepKindId::TypeText, Failure::KeysHeld)
        );
        let calls = fake.calls();
        let slept: Duration = calls
            .iter()
            .map(|call| match call {
                Call::Sleep(time) => *time,
                _ => Duration::ZERO,
            })
            .sum();
        assert_eq!(slept, MS(500));
        // The last look comes after the last slice.
        assert_eq!(calls.last(), Some(&Call::HeldModifiers));
        assert!(!calls
            .iter()
            .any(|call| matches!(call, Call::EnsureFront(..) | Call::TypeText(_))));
    }

    #[test]
    fn no_front_app_stops_a_step_that_presses_keys() {
        let fake = FakeActuator::new();
        let mut run = run_of(vec![clipboard(ClipboardAction::Paste)]);
        run.front = None;
        assert_eq!(
            run_steps(&run, &fake, fake.cancel()),
            failed(0, StepKindId::Clipboard, Failure::NoFrontApp)
        );
        assert_eq!(fake.calls(), vec![Call::InputAccess, Call::HeldModifiers]);
    }

    #[test]
    fn an_app_that_stays_behind_stops_the_run() {
        let fake = FakeActuator::new();
        fake.front.set(false);
        let run = run_of(vec![window(PlacementId::Center)]);
        assert_eq!(
            run_steps(&run, &fake, fake.cancel()),
            failed(0, StepKindId::Window, Failure::NotInFront)
        );
        assert_eq!(fake.calls(), gate());
    }

    #[test]
    fn checks_that_failed_are_made_again_by_the_next_run_only() {
        // One run stops at its first failure, so the checks can't repeat
        // inside it; a new run starts them over.
        let fake = FakeActuator::new();
        fake.front.set(false);
        let run = run_of(vec![keys(), keys()]);
        let _ = run_steps(&run, &fake, fake.cancel());
        fake.front.set(true);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Finished);
        let fronts = fake
            .calls()
            .iter()
            .filter(|call| matches!(call, Call::EnsureFront(..)))
            .count();
        assert_eq!(fronts, 2);
    }

    // --- Failures. ---------------------------------------------------

    #[test]
    fn the_first_failure_stops_the_rest() {
        let fake = FakeActuator::new();
        fake.broken.set(Some(Fallible::Open));
        let run = run_of(vec![
            notify("one"),
            url("https://example.com/"),
            notify("three"),
            keys(),
        ]);
        assert_eq!(
            run_steps(&run, &fake, fake.cancel()),
            failed(1, StepKindId::OpenUrl, Failure::Platform(BROKEN))
        );
        assert_eq!(
            fake.calls(),
            vec![
                Call::Notify("one".to_owned()),
                Call::Open(Opened::Url("https://example.com/".to_owned())),
            ]
        );
    }

    #[test]
    fn a_failed_steps_place_counts_the_skipped_ones() {
        let fake = FakeActuator::new();
        fake.broken.set(Some(Fallible::ClearClipboard));
        let run = run_of(vec![
            typed(""),
            url(""),
            clipboard(ClipboardAction::Clear),
            notify("never"),
        ]);
        assert_eq!(
            run_steps(&run, &fake, fake.cancel()),
            failed(2, StepKindId::Clipboard, Failure::Platform(BROKEN))
        );
    }

    #[test]
    fn every_call_that_fails_fails_its_step() {
        let cases: Vec<(Fallible, StepKind, StepKindId)> = vec![
            (Fallible::SendChord, keys(), StepKindId::Keys),
            (
                Fallible::SendCharChord,
                clipboard(ClipboardAction::Copy),
                StepKindId::Clipboard,
            ),
            (
                Fallible::SendCharChord,
                clipboard(ClipboardAction::Paste),
                StepKindId::Clipboard,
            ),
            (
                Fallible::SendCharChord,
                system(ControlId::LockScreen),
                StepKindId::System,
            ),
            (Fallible::TypeText, typed("a"), StepKindId::TypeText),
            (
                Fallible::ClipboardText,
                typed("{clipboard}"),
                StepKindId::TypeText,
            ),
            (
                Fallible::ChangeCount,
                clipboard(ClipboardAction::Copy),
                StepKindId::Clipboard,
            ),
            (
                Fallible::ClearClipboard,
                clipboard(ClipboardAction::Clear),
                StepKindId::Clipboard,
            ),
            (Fallible::Open, url("x:y"), StepKindId::OpenUrl),
            (Fallible::ProgramFor, shell("true"), StepKindId::Shell),
            (
                Fallible::SystemPlan,
                system(ControlId::VolumeUp),
                StepKindId::System,
            ),
            (
                Fallible::PlaceWindow,
                window(PlacementId::TopLeft),
                StepKindId::Window,
            ),
        ];
        for (call, kind, id) in cases {
            for (unsupported, err) in [(false, BROKEN), (true, PlatformError::Unsupported)] {
                let fake = FakeActuator::new();
                if unsupported {
                    fake.unsupported.set(Some(call));
                } else {
                    fake.broken.set(Some(call));
                }
                let run = run_of(vec![kind.clone(), notify("never")]);
                assert_eq!(
                    run_steps(&run, &fake, fake.cancel()),
                    failed(0, id, Failure::Platform(err)),
                    "{call:?}"
                );
                assert!(!fake.calls().contains(&Call::Notify("never".to_owned())));
            }
        }
    }

    #[test]
    fn a_text_whose_clipboard_cant_be_read_is_not_typed() {
        let fake = FakeActuator::new();
        fake.broken.set(Some(Fallible::ClipboardText));
        let run = run_of(vec![typed("rm {clipboard}")]);
        let _ = run_steps(&run, &fake, fake.cancel());
        assert!(!fake
            .calls()
            .iter()
            .any(|call| matches!(call, Call::TypeText(_))));
    }

    #[test]
    fn a_copy_whose_chord_fails_doesnt_wait() {
        let fake = FakeActuator::new();
        fake.broken.set(Some(Fallible::SendCharChord));
        let run = run_of(vec![clipboard(ClipboardAction::Copy)]);
        let _ = run_steps(&run, &fake, fake.cancel());
        assert_eq!(
            fake.calls(),
            gate_then(vec![
                Call::ChangeCount,
                Call::SendCharChord('c', Modifiers::META)
            ])
        );
    }

    #[test]
    fn how_a_program_ends_is_how_its_step_ends() {
        let cases = [
            (ProgramEnd::Failed, Some(Failure::ExitedWithError)),
            (ProgramEnd::NotStarted, Some(Failure::NotStarted)),
            (ProgramEnd::TimedOut, Some(Failure::TimedOut)),
            (ProgramEnd::Cancelled, None),
        ];
        for (end, why) in cases {
            for (kind, id) in [
                (shell("exit 3"), StepKindId::Shell),
                (system(ControlId::ToggleDarkMode), StepKindId::System),
            ] {
                let fake = FakeActuator::new();
                fake.program_end.set(end);
                let run = run_of(vec![kind, notify("never")]);
                let expected = match &why {
                    Some(why) => failed(0, id, why.clone()),
                    None => Outcome::Cancelled,
                };
                assert_eq!(run_steps(&run, &fake, fake.cancel()), expected, "{end:?}");
                assert!(!fake.calls().contains(&Call::Notify("never".to_owned())));
            }
        }
    }

    // --- Cancel. -----------------------------------------------------

    #[test]
    fn a_run_cancelled_before_it_starts_calls_nothing() {
        let fake = FakeActuator::new();
        fake.cancel().store(true, Ordering::SeqCst);
        let run = run_of(vec![keys(), notify("never")]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Cancelled);
        assert_eq!(fake.calls(), Vec::new());
    }

    #[test]
    fn a_cancel_between_steps_stops_before_the_next() {
        let fake = FakeActuator::new();
        fake.cancel_after(1);
        let run = run_of(vec![notify("one"), notify("two"), keys()]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Cancelled);
        assert_eq!(fake.calls(), vec![Call::Notify("one".to_owned())]);
    }

    #[test]
    fn a_cancel_during_a_delay_ends_it_within_a_slice() {
        let fake = FakeActuator::new();
        fake.cancel_after(3);
        let run = run_of(vec![delay(60_000), notify("never")]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Cancelled);
        assert_eq!(fake.calls(), vec![Call::Sleep(MS(20)); 3]);
    }

    #[test]
    fn a_cancel_during_the_copy_wait_ends_it() {
        // In a slice of the wait.
        let fake = FakeActuator::new();
        fake.cancel_after(gate().len() + 3);
        let run = run_of(vec![clipboard(ClipboardAction::Copy), notify("never")]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Cancelled);
        assert_eq!(
            fake.calls(),
            gate_then(vec![
                Call::ChangeCount,
                Call::SendCharChord('c', Modifiers::META),
                Call::Sleep(MS(20)),
            ])
        );

        // In a look at the count.
        let fake = FakeActuator::new();
        fake.cancel_after(gate().len() + 4);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Cancelled);
        assert_eq!(fake.calls().len(), gate().len() + 4);
        assert_eq!(fake.calls().last(), Some(&Call::ChangeCount));
    }

    #[test]
    fn a_cancel_while_keys_are_held_ends_the_wait() {
        let fake = FakeActuator::new();
        fake.held_after.set(Modifiers::CTRL);
        fake.cancel_after(5);
        let run = run_of(vec![keys()]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Cancelled);
        assert_eq!(
            fake.calls(),
            vec![
                Call::InputAccess,
                Call::HeldModifiers,
                Call::Sleep(MS(20)),
                Call::HeldModifiers,
                Call::Sleep(MS(20)),
            ]
        );
    }

    #[test]
    fn no_call_follows_a_cancel_inside_a_step() {
        // The cancel lands in each call of this run in turn. Whatever the
        // call, it is the last one.
        let kinds = vec![
            typed("{clipboard}"),
            clipboard(ClipboardAction::Copy),
            system(ControlId::LockScreen),
            window(PlacementId::Fill),
            shell("true"),
        ];
        let fresh = || {
            let fake = FakeActuator::new();
            *fake.clipboard.borrow_mut() = Some("c".to_owned());
            fake
        };
        let whole = fresh();
        assert_eq!(
            run_steps(&run_of(kinds.clone()), &whole, whole.cancel()),
            Outcome::Finished
        );
        let all = whole.calls().len();
        for at in 1..=all {
            let fake = fresh();
            fake.cancel_after(at);
            let outcome = run_steps(&run_of(kinds.clone()), &fake, fake.cancel());
            assert_eq!(fake.calls().len(), at, "cancel in call {at}");
            // Only a cancel in the very last call comes too late to matter.
            if at < all {
                assert_eq!(outcome, Outcome::Cancelled, "cancel in call {at}");
            }
        }
    }

    #[test]
    fn typing_cut_short_by_a_cancel_is_a_cancelled_run() {
        let fake = FakeActuator::new();
        fake.cancel_after(gate().len() + 2);
        let run = run_of(vec![typed("a long text")]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Cancelled);
        assert_eq!(
            fake.calls().last(),
            Some(&Call::TypeText("a long text".to_owned()))
        );
    }

    #[test]
    fn a_failure_after_a_cancel_is_a_cancel() {
        // Quit kills the program: it then ends with an error, or the app
        // never comes to the front.
        let fake = FakeActuator::new();
        fake.program_end.set(ProgramEnd::Failed);
        fake.cancel_after(2);
        let run = run_of(vec![shell("sleep 100")]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Cancelled);

        let fake = FakeActuator::new();
        fake.front.set(false);
        fake.cancel_after(gate().len());
        let run = run_of(vec![keys()]);
        assert_eq!(run_steps(&run, &fake, fake.cancel()), Outcome::Cancelled);
    }

    // --- Words. ------------------------------------------------------

    fn every_failure() -> Vec<Failure> {
        vec![
            Failure::NeedsAccess,
            Failure::KeysHeld,
            Failure::NotInFront,
            Failure::NoFrontApp,
            Failure::Platform(PlatformError::Unsupported),
            Failure::Platform(BROKEN),
            Failure::Platform(PlatformError::WrongThread),
            Failure::NotStarted,
            Failure::ExitedWithError,
            Failure::TimedOut,
        ]
    }

    #[test]
    fn a_finished_or_cancelled_run_shows_nothing() {
        assert_eq!(toast_for("Tidy up", &Outcome::Finished), None);
        assert_eq!(toast_for("Tidy up", &Outcome::Cancelled), None);
    }

    #[test]
    fn nothing_to_run_is_an_info_toast() {
        assert_eq!(
            toast_for("Tidy up", &Outcome::NothingToRun),
            Some(RunToast {
                title: "Nothing to run yet".to_owned(),
                detail: Some("Add a step to it in Settings.".to_owned()),
                error: false,
            })
        );
    }

    #[test]
    fn a_failure_names_the_shortcut_and_gives_one_reason() {
        let toast = |kind, why| toast_for("Tidy up", &failed(0, kind, why));
        assert_eq!(
            toast(StepKindId::Keys, Failure::KeysHeld),
            Some(RunToast {
                title: "Couldn't finish Tidy up".to_owned(),
                detail: Some("Let go of the keys first.".to_owned()),
                error: true,
            })
        );
        let detail = |kind, why| toast(kind, why).and_then(|toast| toast.detail);
        assert_eq!(
            detail(StepKindId::Clipboard, Failure::NeedsAccess).as_deref(),
            Some(
                "Grape needs Accessibility access to press keys. \
                 Turn it on in System Settings, then try again."
            )
        );
        assert_eq!(
            detail(StepKindId::Window, Failure::NoFrontApp).as_deref(),
            Some("Click the app you want first.")
        );
        assert_eq!(
            detail(StepKindId::Window, Failure::NotInFront).as_deref(),
            Some("The app you were in didn't come back to the front.")
        );
        assert_eq!(
            detail(
                StepKindId::Shell,
                Failure::Platform(PlatformError::Unsupported)
            )
            .as_deref(),
            Some("That step doesn't work on this computer yet.")
        );
        assert_eq!(
            detail(StepKindId::Shell, Failure::ExitedWithError).as_deref(),
            Some("The shell script stopped with an error.")
        );
        assert_eq!(
            detail(StepKindId::AppleScript, Failure::TimedOut).as_deref(),
            Some("A step was still going after a minute, so Grape stopped it.")
        );
        assert_eq!(
            detail(StepKindId::OpenApp, Failure::Platform(BROKEN)).as_deref(),
            Some("That app wouldn't open.")
        );
    }

    #[test]
    fn every_failure_of_every_kind_has_a_sentence() {
        for kind in StepKindId::ALL {
            for why in every_failure() {
                let words = sentence(kind, &why);
                assert!(words.len() > 10, "{kind:?} {why:?}");
                assert!(words.ends_with('.'), "{words}");
                assert!(
                    words.chars().next().is_some_and(char::is_uppercase),
                    "{words}"
                );
                // The platform's own words (a call's name, a code) are for
                // the log.
                assert!(!words.contains("OS error"), "{words}");
            }
        }
    }

    #[test]
    fn the_other_toasts() {
        assert_eq!(
            still_running("Tidy up"),
            RunToast {
                title: "Still running Tidy up".to_owned(),
                detail: None,
                error: false,
            }
        );
        assert_eq!(
            notice("Tidy up", "All done.\n\tSee you."),
            RunToast {
                title: "Tidy up".to_owned(),
                detail: Some("All done.  See you.".to_owned()),
                error: false,
            }
        );
    }

    #[test]
    fn the_log_line_says_kinds_time_and_ending() {
        let ran = [StepKindId::OpenUrl, StepKindId::Keys, StepKindId::Shell];
        assert_eq!(
            log_line(7, &ran, 532, &Outcome::Finished),
            "shortcut 7: ran [openUrl, keys, shell] in 532 ms: finished"
        );
        assert_eq!(
            log_line(8, &[], 0, &Outcome::NothingToRun),
            "shortcut 8: ran [] in 0 ms: nothing to run"
        );
        assert_eq!(
            log_line(9, &ran[..1], 41, &Outcome::Cancelled),
            "shortcut 9: ran [openUrl] in 41 ms: cancelled"
        );
        assert_eq!(
            log_line(
                7,
                &ran,
                60_012,
                &failed(4, StepKindId::Shell, Failure::TimedOut)
            ),
            "shortcut 7: ran [openUrl, keys, shell] in 60012 ms: \
             step 5 (shell) failed: the program ran too long"
        );
        assert_eq!(
            log_line(
                7,
                &ran[..2],
                3,
                &failed(1, StepKindId::Keys, Failure::Platform(BROKEN))
            ),
            "shortcut 7: ran [openUrl, keys] in 3 ms: \
             step 2 (keys) failed: fake failed (OS error 0x00000005)"
        );
    }

    /// Scripts a fake before a run.
    type SetUp = Box<dyn Fn(&FakeActuator)>;

    /// A run whose every text, name, path and id holds the marker.
    fn marked_run() -> ShortcutRun {
        let text = || format!("t {MARKER}");
        let path = || Some(PathBuf::from(format!("/Users/max/{MARKER}")));
        let mut run = run_of(vec![
            typed(&format!("{MARKER} {{clipboard}}")),
            clipboard(ClipboardAction::Copy),
            StepKind::OpenApp {
                path: path(),
                bundle_id: Some(format!("com.example.{MARKER}")),
                name: Some(text()),
            },
            StepKind::OpenFile {
                path: path(),
                folder: true,
            },
            url(&format!("https://example.com/{MARKER}")),
            StepKind::AppleShortcut { name: text() },
            StepKind::AppleScript { source: text() },
            shell(&text()),
            StepKind::KeyboardMaestro { name: text() },
            notify(&text()),
            StepKind::Speak { text: text() },
            keys(),
            window(PlacementId::LeftHalf),
            system(ControlId::LockScreen),
            delay(20),
        ]);
        run.name = format!("n {MARKER}");
        run.front = Some(FrontApp {
            pid: PID,
            id: Some(format!("com.example.{MARKER}")),
        });
        run
    }

    #[test]
    fn nothing_typed_reaches_an_outcome_or_a_log_line() {
        let run = marked_run();
        let mut printed = Vec::new();
        let mut note = |outcome: &Outcome, ran: &[StepKindId]| {
            printed.push(format!("{outcome:?}"));
            printed.push(format!("{outcome:#?}"));
            printed.push(log_line(run.seq, ran, 12, outcome));
            if let Some(toast) = toast_for(&run.name, outcome) {
                printed.push(format!("{toast:?} {toast:#?}"));
                // The detail is one of Grape's own sentences.
                printed.push(toast.detail.unwrap_or_default());
            }
        };
        let fresh = || {
            let fake = FakeActuator::new();
            *fake.clipboard.borrow_mut() = Some(format!("c {MARKER}"));
            fake
        };

        // It finishes; a cancel lands in every call in turn.
        let mut ran = Vec::new();
        let fake = fresh();
        let outcome = sequence(&run, &fake, fake.cancel(), &mut ran);
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(ran.len(), run.steps.len());
        note(&outcome, &ran);
        for at in 1..fake.calls().len() {
            let fake = fresh();
            fake.cancel_after(at);
            let mut ran = Vec::new();
            let outcome = sequence(&run, &fake, fake.cancel(), &mut ran);
            note(&outcome, &ran);
        }

        // Every call fails in turn, both ways, and so do the checks and
        // the programs.
        let fallible = [
            Fallible::SendChord,
            Fallible::SendCharChord,
            Fallible::TypeText,
            Fallible::ClipboardText,
            Fallible::ChangeCount,
            Fallible::ClearClipboard,
            Fallible::Open,
            Fallible::ProgramFor,
            Fallible::SystemPlan,
            Fallible::PlaceWindow,
        ];
        let mut set_ups: Vec<SetUp> = vec![
            Box::new(|fake| fake.access.set(InputAccess::Denied)),
            Box::new(|fake| fake.held_after.set(Modifiers::META)),
            Box::new(|fake| fake.front.set(false)),
            Box::new(|fake| fake.program_end.set(ProgramEnd::Failed)),
            Box::new(|fake| fake.program_end.set(ProgramEnd::NotStarted)),
            Box::new(|fake| fake.program_end.set(ProgramEnd::TimedOut)),
            Box::new(|fake| fake.program_end.set(ProgramEnd::Cancelled)),
        ];
        for call in fallible {
            set_ups.push(Box::new(move |fake| fake.broken.set(Some(call))));
            set_ups.push(Box::new(move |fake| fake.unsupported.set(Some(call))));
        }
        let mut failures = 0;
        for set_up in &set_ups {
            let fake = fresh();
            set_up(&fake);
            let mut ran = Vec::new();
            let outcome = sequence(&run, &fake, fake.cancel(), &mut ran);
            failures += usize::from(matches!(outcome, Outcome::Failed { .. }));
            note(&outcome, &ran);
        }
        // Clear isn't in this run and a cancelled program isn't a failure.
        assert_eq!(failures, set_ups.len() - 3);

        // With no front app, and with nothing to run.
        let mut alone = run.clone();
        alone.front = None;
        let fake = fresh();
        note(&run_steps(&alone, &fake, fake.cancel()), &[]);
        alone.steps.clear();
        note(&run_steps(&alone, &fake, fake.cancel()), &[]);

        assert!(printed.len() > 100);
        for line in &printed {
            assert!(!line.contains(MARKER), "{line}");
        }
        // The fake did see the marker, so the check means something.
        let fake = fresh();
        let _ = run_steps(&run, &fake, fake.cancel());
        assert!(format!("{:?}", fake.calls()).contains(MARKER));
    }

    #[test]
    fn a_toast_prints_neither_line() {
        let toast = notice(&format!("n {MARKER}"), &format!("t {MARKER}"));
        assert_eq!(
            format!("{toast:?}"),
            "RunToast { detail: true, error: false, .. }"
        );
        assert!(!format!("{:?}", still_running(MARKER)).contains(MARKER));
    }

    // --- The real actuator. ------------------------------------------

    #[test]
    fn grape_asks_for_access_once() {
        let asked = AtomicBool::new(false);
        assert!(first_time(&asked));
        assert!(!first_time(&asked));
        assert!(!first_time(&asked));
    }

    #[test]
    fn no_program_running_means_nothing_to_kill() {
        let child = RunningChild::new();
        assert_eq!(child.pid(), None);
        child.kill();
        assert_eq!(child.pid(), None);
    }

    #[test]
    fn the_real_actuator_hands_a_notice_to_its_closure() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let actuator = {
            let seen = Arc::clone(&seen);
            SystemActuator::new(None, Arc::new(RunningChild::new()), move |text: &str| {
                seen.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(text.to_owned());
            })
        };
        actuator.notify("hello");
        assert_eq!(
            *seen.lock().unwrap_or_else(PoisonError::into_inner),
            vec!["hello".to_owned()]
        );
    }

    #[test]
    fn with_no_home_folder_no_program_starts() {
        let child = Arc::new(RunningChild::new());
        let actuator = SystemActuator::new(None, Arc::clone(&child), |_: &str| {});
        let program = Program {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "exit 0".into()],
            stdin: None,
        };
        let cancel = AtomicBool::new(false);
        assert_eq!(
            actuator.run_program(&program, &cancel, Duration::from_secs(5)),
            ProgramEnd::NotStarted
        );
        assert_eq!(child.pid(), None);
    }

    /// Real programs: `/bin/sh` only, in the temp folder.
    #[cfg(unix)]
    mod programs {
        use super::*;

        const LONG: Duration = Duration::from_secs(20);

        fn actuator() -> (SystemActuator, Arc<RunningChild>) {
            let child = Arc::new(RunningChild::new());
            let actuator =
                SystemActuator::new(Some(std::env::temp_dir()), Arc::clone(&child), |_: &str| {});
            (actuator, child)
        }

        fn sh(args: &[&str], stdin: Option<&str>) -> Program {
            Program {
                program: PathBuf::from("/bin/sh"),
                args: args.iter().map(Into::into).collect(),
                stdin: stdin.map(str::to_owned),
            }
        }

        fn end_of(program: &Program, limit: Duration) -> ProgramEnd {
            let (actuator, child) = actuator();
            let cancel = AtomicBool::new(false);
            let end = actuator.run_program(program, &cancel, limit);
            assert_eq!(child.pid(), None);
            end
        }

        #[test]
        fn exit_zero_is_success_and_anything_else_a_failure() {
            assert_eq!(
                end_of(&sh(&["-c", "exit 0"], None), LONG),
                ProgramEnd::Success
            );
            assert_eq!(
                end_of(&sh(&["-c", "exit 3"], None), LONG),
                ProgramEnd::Failed
            );
            assert_eq!(
                end_of(&sh(&["-c", "kill -9 $$"], None), LONG),
                ProgramEnd::Failed
            );
        }

        #[test]
        fn a_program_that_isnt_there_doesnt_start() {
            let program = Program {
                program: PathBuf::from("/nonexistent/grape-test-program"),
                args: Vec::new(),
                stdin: Some("exit 0".to_owned()),
            };
            assert_eq!(end_of(&program, LONG), ProgramEnd::NotStarted);
        }

        #[test]
        fn the_script_arrives_on_standard_input_and_the_pipe_is_closed() {
            assert_eq!(
                end_of(&sh(&["-s"], Some("exit 0\n")), LONG),
                ProgramEnd::Success
            );
            assert_eq!(
                end_of(&sh(&["-s"], Some("exit 4\n")), LONG),
                ProgramEnd::Failed
            );
            // `cat` only ends when the pipe is closed.
            assert_eq!(
                end_of(&sh(&["-c", "test \"$(cat)\" = 'a b'"], Some("a b")), LONG),
                ProgramEnd::Success
            );
        }

        #[test]
        fn with_no_script_standard_input_is_empty() {
            assert_eq!(
                end_of(&sh(&["-c", "test -z \"$(cat)\""], None), LONG),
                ProgramEnd::Success
            );
        }

        #[test]
        fn it_runs_in_the_home_folder_given() {
            let home = std::env::temp_dir();
            let script = "test \"$(pwd -P)\" = \"$(cd \"$1\" && pwd -P)\"";
            let mut program = sh(&["-c", script, "sh"], None);
            program.args.push(home.into_os_string());
            assert_eq!(end_of(&program, LONG), ProgramEnd::Success);
        }

        #[test]
        fn it_runs_in_a_process_group_of_its_own() {
            // In its own group, the shell's pid is its group's id. A machine
            // with no `ps` can't tell, and the test then says nothing.
            let script = "command -v ps >/dev/null 2>&1 || exit 0; \
                          test \"$(ps -o pgid= -p $$ | tr -d ' ')\" = \"$$\"";
            assert_eq!(
                end_of(&sh(&["-c", script], None), LONG),
                ProgramEnd::Success
            );
        }

        #[test]
        fn what_it_prints_goes_nowhere() {
            let script = "echo out; echo err >&2; exit 0";
            assert_eq!(
                end_of(&sh(&["-c", script], None), LONG),
                ProgramEnd::Success
            );
        }

        #[test]
        fn a_program_past_its_limit_is_ended() {
            let started = Instant::now();
            assert_eq!(
                end_of(&sh(&["-c", "exec sleep 30"], None), MS(150)),
                ProgramEnd::TimedOut
            );
            assert!(started.elapsed() < Duration::from_secs(10));
        }

        #[test]
        fn a_script_too_long_for_the_pipe_doesnt_hold_the_run() {
            // The shell stops reading at the sleep, so the rest never fits
            // in the pipe.
            let script = format!("exec sleep 30\n{}", "# padding\n".repeat(100_000));
            let started = Instant::now();
            assert_eq!(
                end_of(&sh(&["-s"], Some(&script)), MS(150)),
                ProgramEnd::TimedOut
            );
            assert!(started.elapsed() < Duration::from_secs(10));
        }

        #[test]
        fn a_cancel_ends_the_program_and_its_pid_is_known_meanwhile() {
            let (actuator, child) = actuator();
            let cancel = Arc::new(AtomicBool::new(false));
            let canceller = {
                let (child, cancel) = (Arc::clone(&child), Arc::clone(&cancel));
                thread::spawn(move || {
                    let started = Instant::now();
                    let mut seen = None;
                    while seen.is_none() && started.elapsed() < LONG {
                        seen = child.pid();
                        thread::sleep(MS(2));
                    }
                    // As the app's cancel does: the flag, then the kill.
                    cancel.store(true, Ordering::SeqCst);
                    child.kill();
                    seen
                })
            };
            let started = Instant::now();
            let end = actuator.run_program(&sh(&["-c", "exec sleep 30"], None), &cancel, LONG);
            assert_eq!(end, ProgramEnd::Cancelled);
            assert!(started.elapsed() < Duration::from_secs(10));
            let seen = canceller.join().ok().flatten();
            assert!(seen.is_some_and(|pid| pid > 1));
            assert_eq!(child.pid(), None);
        }

        #[test]
        fn a_run_already_cancelled_ends_its_program_at_once() {
            let (actuator, child) = actuator();
            let cancel = AtomicBool::new(true);
            let started = Instant::now();
            let end = actuator.run_program(&sh(&["-c", "exec sleep 30"], None), &cancel, LONG);
            assert_eq!(end, ProgramEnd::Cancelled);
            assert!(started.elapsed() < Duration::from_secs(10));
            assert_eq!(child.pid(), None);
        }

        #[test]
        fn a_real_script_runs_through_the_whole_runner() {
            struct Shell(SystemActuator);
            // Only what a Shell step needs is real here.
            impl Actuator for Shell {
                fn input_access(&self) -> InputAccess {
                    InputAccess::NotNeeded
                }
                fn request_access(&self) {}
                fn held_modifiers(&self) -> Modifiers {
                    Modifiers::NONE
                }
                fn ensure_front(&self, _pid: i32, _timeout: Duration) -> bool {
                    false
                }
                fn send_chord(&self, _chord: Chord) -> Result<(), PlatformError> {
                    Err(PlatformError::Unsupported)
                }
                fn send_char_chord(
                    &self,
                    _ch: char,
                    _mods: Modifiers,
                ) -> Result<(), PlatformError> {
                    Err(PlatformError::Unsupported)
                }
                fn type_text(
                    &self,
                    _text: &str,
                    _cancel: &AtomicBool,
                ) -> Result<(), PlatformError> {
                    Err(PlatformError::Unsupported)
                }
                fn clipboard_text(&self) -> Result<Option<String>, PlatformError> {
                    Err(PlatformError::Unsupported)
                }
                fn clipboard_change_count(&self) -> Result<i64, PlatformError> {
                    Err(PlatformError::Unsupported)
                }
                fn clear_clipboard(&self) -> Result<(), PlatformError> {
                    Err(PlatformError::Unsupported)
                }
                fn open_target(&self, _target: OpenTarget<'_>) -> Result<(), PlatformError> {
                    Err(PlatformError::Unsupported)
                }
                fn program_for(
                    &self,
                    request: ProgramRequest<'_>,
                ) -> Result<Program, PlatformError> {
                    match request {
                        ProgramRequest::Shell(source) => Ok(sh(&["-s"], Some(source))),
                        _ => Err(PlatformError::Unsupported),
                    }
                }
                fn system_plan(
                    &self,
                    _control: SystemControl,
                ) -> Result<SystemPlan, PlatformError> {
                    Err(PlatformError::Unsupported)
                }
                fn place_window(
                    &self,
                    _pid: i32,
                    _placement: Placement,
                ) -> Result<(), PlatformError> {
                    Err(PlatformError::Unsupported)
                }
                fn local_date_time(&self) -> Option<LocalTime> {
                    None
                }
                fn run_program(
                    &self,
                    program: &Program,
                    cancel: &AtomicBool,
                    limit: Duration,
                ) -> ProgramEnd {
                    self.0.run_program(program, cancel, limit)
                }
                fn notify(&self, text: &str) {
                    self.0.notify(text);
                }
                fn sleep(&self, time: Duration) {
                    self.0.sleep(time);
                }
            }

            let (actuator, _child) = actuator();
            let actuator = Shell(actuator);
            let cancel = AtomicBool::new(false);
            let run = run_of(vec![
                shell("true\n"),
                delay(10),
                shell("exit 2\n"),
                delay(10),
            ]);
            assert_eq!(
                run_shortcut(&run, &actuator, &cancel),
                failed(2, StepKindId::Shell, Failure::ExitedWithError)
            );
        }
    }
}
