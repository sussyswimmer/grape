//! The Shortcuts wheel (`docs/design/m10-shortcuts-wheel.md`): a wheel the
//! user opens with a hot key, whose segments are shortcuts made in
//! Settings.
//!
//! - [`model`] is the pure part: menus, shortcuts, steps, their limits and
//!   validation, which menu shows for which app, the text variables, the
//!   stored form and the Settings view.
//! - [`store`] reads and writes `shortcuts.json` in the app data folder.
//! - [`session`] is the open wheel's state, driven by the controller.
//! - [`run`] runs a picked shortcut's steps in order.
//! - This file is the wiring: [`ShortcutsState`], the `shortcuts_*`
//!   commands of the Settings window, the hot keys and the one run.
//!
//! Names, text, scripts, links, paths and app ids are the user's: none of
//! them is ever logged, and every type that holds one prints kinds and
//! counts only.
//!
//! **Hot keys.** Open is bound only while the wheel is on, Grape isn't
//! paused and the wheel is available here ([`open_chord`]). Escape and
//! Return are bound only while a wheel is open, once for every subset of
//! the hot key's modifiers ([`track_chords`]).
//!
//! **Locks.** `shared` (the menus and their file) is taken before `keys`
//! (the hot keys and the pointer). The controller's thread takes `keys`
//! alone. A bind waits for the main thread on macOS, so the main thread
//! never waits for either lock: Pause rebinds on a thread of its own, and
//! the exit only tries the lock.
//!
//! **Runs.** One at a time, each on a `"grape-shortcut"` thread. Every
//! [`Effect::RunShortcut`](crate::controller::Effect::RunShortcut) is
//! answered with [`Input::RunEnded`], whether it ran or not.

pub mod model;
pub mod run;
pub mod session;
pub mod store;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use grape_platform::keys::{Chord, KeyCode};
use grape_platform::shortcuts::{
    self as platform, HotkeyId, HotkeyMonitor, HotkeySignal, InputAccess, PickKind, Picked,
    PointerSignal, PointerWatch,
};
use grape_platform::{DisplayBounds, Modifiers, PlatformError};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager, State, WebviewWindow};

use crate::controller::{Input, THIS_OS};
use crate::events::{ToastShow, ToastTone};
use crate::toast::{self, Toaster};
use crate::wheel::WheelInputs;
use crate::GrapeState;
use model::{
    Binding, ChordWire, Config, Icon, Id, NewItem, OpensAt, ShortcutRun, ShortcutsView, Snapshot,
    StepEdit, StepKindId, Target,
};
use run::{RunToast, RunningChild, SystemActuator};
use store::Store;

/// What a hot key the OS won't give is answered with.
const HOTKEY_REFUSED: &str = "Grape couldn't use that shortcut. Try another.";
/// The same refusal when the switch, not the chord, was what changed.
const OS_REFUSED: &str = "macOS wouldn't give Grape that shortcut.";
const PICKER_FAILED: &str = "Couldn't open the picker.";
const ACCESS_FAILED: &str = "Couldn't ask for Accessibility access.";

/// How long the exit waits for a cancelled run: as long as it waits for
/// jobs (`jobs.rs`).
const QUIT_WAIT: Duration = Duration::from_secs(2);

/// The chord Open should be bound to right now, if any: the hot key, while
/// the wheel is on, Grape isn't paused and the wheel is available here.
fn open_chord(
    enabled: bool,
    paused: bool,
    available: bool,
    hotkey: Option<Chord>,
) -> Option<Chord> {
    hotkey.filter(|_| enabled && !paused && available)
}

/// The chords Escape and Confirm get while a wheel is open: Esc and Return
/// with every subset of the hot key's modifiers, since the user may still
/// hold some of them and the OS matches modifiers exactly.
fn track_chords(hotkey: Option<Chord>) -> (Vec<Chord>, Vec<Chord>) {
    let mods = hotkey.map_or(Modifiers::NONE, |chord| chord.mods);
    let subsets = |key| Chord { key, mods }.with_modifier_subsets();
    (subsets(KeyCode::Escape), subsets(KeyCode::Enter))
}

/// The hot keys and the pointer watch, and what Open is bound to.
struct Keys {
    monitor: Option<Box<dyn HotkeyMonitor>>,
    watch: Option<Box<dyn PointerWatch>>,
    /// The chord Open is bound to now.
    open: Option<Chord>,
    /// The wheel's hot key as the menus' file has it, bound or not.
    hotkey: Option<Chord>,
    /// The last try to bind Open failed, and it is still wanted.
    failed: bool,
}

impl Keys {
    fn new(monitor: Option<Box<dyn HotkeyMonitor>>, watch: Option<Box<dyn PointerWatch>>) -> Self {
        Self {
            monitor,
            watch,
            open: None,
            hotkey: None,
            failed: false,
        }
    }

    /// Starts both sources. Nothing is bound yet.
    fn start(
        &mut self,
        hotkeys: SyncSender<HotkeySignal>,
        pointer: SyncSender<PointerSignal>,
    ) -> Result<(), PlatformError> {
        let (Some(monitor), Some(watch)) = (self.monitor.as_mut(), self.watch.as_mut()) else {
            return Err(PlatformError::Unsupported);
        };
        monitor.start(hotkeys)?;
        watch.start(pointer)
    }

    /// What the view says of Open.
    fn binding(&self) -> Binding {
        match (self.open, self.failed) {
            (Some(_), _) => Binding::Bound,
            (None, true) => Binding::Failed,
            (None, false) => Binding::Off,
        }
    }

    /// Binds Open to `want`, or lets go of it. A refused chord leaves Open
    /// with what it had. `strict` is for a change the caller will then
    /// refuse, so nothing here changes either; otherwise the failure is
    /// remembered for the view.
    fn bind_open(&mut self, want: Option<Chord>, strict: bool) -> Result<(), PlatformError> {
        let Some(chord) = want else {
            self.failed = false;
            if self.open.take().is_some() {
                if let Some(monitor) = self.monitor.as_mut() {
                    // The monitor tries again at its next bind.
                    if let Err(err) = monitor.bind(HotkeyId::Open, &[]) {
                        log::error!("shortcuts: could not let go of the hot key: {err}");
                    }
                }
            }
            return Ok(());
        };
        if self.open == Some(chord) {
            return Ok(());
        }
        let bound = match self.monitor.as_mut() {
            Some(monitor) => monitor.bind(HotkeyId::Open, &[chord]),
            None => Err(PlatformError::Unsupported),
        };
        match bound {
            Ok(()) => {
                self.open = Some(chord);
                self.failed = false;
                Ok(())
            }
            Err(err) => {
                if !strict {
                    self.failed = self.open.is_none();
                }
                Err(err)
            }
        }
    }

    /// A wheel opened (`true`) or closed: binds or lets go of Escape and
    /// Confirm, and switches the pointer watch. Returns once the OS has
    /// done it.
    ///
    /// # Errors
    /// Only for `true`, when Escape couldn't be bound. Nothing is bound or
    /// switched on then.
    fn track(&mut self, on: bool) -> Result<(), PlatformError> {
        if !on {
            if let Some(watch) = self.watch.as_mut() {
                watch.set_active(false);
            }
        }
        let (escape, confirm) = if on {
            track_chords(self.hotkey)
        } else {
            (Vec::new(), Vec::new())
        };
        let Some(monitor) = self.monitor.as_mut() else {
            return if on {
                Err(PlatformError::Unsupported)
            } else {
                Ok(())
            };
        };
        match monitor.bind(HotkeyId::Escape, &escape) {
            Ok(()) => {}
            Err(err) if on => return Err(err),
            Err(err) => log::error!("shortcuts: could not let go of Esc: {err}"),
        }
        if let Err(err) = monitor.bind(HotkeyId::Confirm, &confirm) {
            if on {
                log::warn!("shortcuts: Return couldn't be bound: {err}");
            } else {
                log::error!("shortcuts: could not let go of Return: {err}");
            }
        }
        if on {
            if let Some(watch) = self.watch.as_mut() {
                watch.set_active(true);
            }
        }
        Ok(())
    }

    /// Stops both sources for good. `true` when there was one to stop.
    fn stop(&mut self) -> bool {
        self.open = None;
        self.failed = false;
        let watch = self.watch.take();
        let monitor = self.monitor.take();
        let any = watch.is_some() || monitor.is_some();
        if let Some(mut watch) = watch {
            watch.stop();
        }
        if let Some(mut monitor) = monitor {
            monitor.stop();
        }
        any
    }
}

/// Why a run is stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Tray → Pause Grape.
    Paused,
    /// Grape is going. No run starts after this.
    Quitting,
}

impl Why {
    const fn words(self) -> &'static str {
        match self {
            Why::Paused => "Grape was paused",
            Why::Quitting => "Grape is quitting",
        }
    }
}

/// The run that is going. No `Debug`: the name is the user's.
struct Current {
    /// The shortcut's name, for "Still running".
    name: String,
    /// Set to stop it.
    cancel: Arc<AtomicBool>,
}

/// What [`RunSlot::begin`] answers.
enum Begin {
    /// The slot is this run's now. The flag stops it.
    Started(Arc<AtomicBool>),
    /// Another run has it.
    Busy,
    /// Grape is quitting.
    Quitting,
}

/// The one run there may be, and how to stop it.
#[derive(Default)]
struct RunSlot {
    current: Mutex<Option<Current>>,
    /// Notified when the run ends.
    ended: Condvar,
    /// The program a step is running, for the kill.
    child: Arc<RunningChild>,
    /// Set by [`Why::Quitting`].
    quitting: AtomicBool,
}

impl RunSlot {
    fn lock(&self) -> MutexGuard<'_, Option<Current>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the slot for the shortcut `name`.
    fn begin(&self, name: &str) -> Begin {
        let mut current = self.lock();
        if self.quitting.load(Ordering::SeqCst) {
            return Begin::Quitting;
        }
        if current.is_some() {
            return Begin::Busy;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *current = Some(Current {
            name: name.to_owned(),
            cancel: Arc::clone(&cancel),
        });
        Begin::Started(cancel)
    }

    /// The run is over: frees the slot.
    fn finish(&self) {
        *self.lock() = None;
        self.ended.notify_all();
    }

    /// The running shortcut's name.
    fn name(&self) -> Option<String> {
        self.lock().as_ref().map(|current| current.name.clone())
    }

    /// Stops the run, if there is one: the flag first, so the runner calls
    /// the killed program's end a cancel, then the program and everything
    /// it started. Returns at once; `true` when a run was going.
    fn cancel(&self, why: Why) -> bool {
        let current = self.lock();
        if why == Why::Quitting {
            self.quitting.store(true, Ordering::SeqCst);
        }
        let Some(current) = current.as_ref() else {
            return false;
        };
        current.cancel.store(true, Ordering::SeqCst);
        self.child.kill();
        true
    }

    /// Waits up to `timeout` for the slot to be free. `true` when it is.
    fn wait(&self, timeout: Duration) -> bool {
        let (current, _) = self
            .ended
            .wait_timeout_while(self.lock(), timeout, |current| current.is_some())
            .unwrap_or_else(PoisonError::into_inner);
        current.is_none()
    }
}

/// A run's toast as the toast window takes it: the error look for a
/// failure, the info look for the rest, and nothing to reveal.
fn toast_show(id: u64, toast: RunToast) -> ToastShow {
    ToastShow {
        id,
        tone: if toast.error {
            ToastTone::Error
        } else {
            ToastTone::Info
        },
        title: toast.title,
        detail: toast.detail,
        progress: None,
        revealable: false,
    }
}

fn show(app: &AppHandle, toast: RunToast, anchor: (i32, i32), display: &DisplayBounds) {
    let Some(toaster) = app.try_state::<Toaster>() else {
        return;
    };
    toast::show(app, toast_show(toaster.next_id(), toast), anchor, display);
}

/// The menus, their file and what the load has to say.
struct Shared {
    config: Config,
    /// `None` when the wheel isn't available or the file's place is
    /// unknown: nothing is written then.
    store: Option<Store>,
    /// A sentence about the load, for the first view.
    note: Option<&'static str>,
}

impl Shared {
    /// Writes the menus to their file. Call with the change applied.
    fn save(&mut self) -> Result<(), &'static str> {
        let Some(store) = self.store.as_mut() else {
            log::error!("shortcuts: changed, but there's no file to save to");
            return Err(model::SAVE_FAILED);
        };
        store.save(&self.config).map_err(|err| {
            log::error!("shortcuts: could not save: {err}");
            model::SAVE_FAILED
        })
    }
}

/// The Shortcuts wheel's state, managed by Tauri.
pub struct ShortcutsState {
    /// `false` where the wheel isn't built, or its hot keys couldn't start.
    available: AtomicBool,
    shared: Mutex<Shared>,
    keys: Mutex<Keys>,
    /// Set while a picker is open, so a second one doesn't stack.
    picking: AtomicBool,
    run: RunSlot,
}

impl ShortcutsState {
    /// Makes the hot key monitor and the pointer watch and, where the
    /// wheel is available, loads the shortcuts. Call in setup, before the
    /// wheel starts. Never fails: without hot keys the wheel is reported
    /// as not available and nothing is read or written.
    pub fn load(app: &AppHandle) -> Self {
        let sources = match (platform::hotkey_monitor(), platform::pointer_watch()) {
            (Ok(monitor), Ok(watch)) => Some((monitor, watch)),
            (Err(PlatformError::Unsupported), _) | (_, Err(PlatformError::Unsupported)) => {
                log::info!("shortcuts: the Shortcuts wheel isn't available on this OS yet");
                None
            }
            (Err(err), _) | (_, Err(err)) => {
                log::error!("shortcuts: could not make the hot keys: {err}");
                None
            }
        };
        let available = model::available(THIS_OS) && sources.is_some();
        let (monitor, watch) = sources.filter(|_| available).unzip();
        let mut shared = Shared {
            config: Config::defaults(THIS_OS),
            store: None,
            note: None,
        };
        if available {
            let folder = app
                .path()
                .resolve(store::FILE, BaseDirectory::AppData)
                .map(|file| file.parent().map(std::path::Path::to_path_buf));
            match folder {
                Ok(Some(folder)) => {
                    let loaded = store::load_from(&folder, THIS_OS);
                    shared = Shared {
                        config: loaded.config,
                        store: Some(loaded.store),
                        note: loaded.note,
                    };
                }
                Ok(None) => log::error!(
                    "shortcuts: {} has no folder; using the defaults, and changes won't be saved",
                    store::FILE
                ),
                Err(err) => log::error!(
                    "shortcuts: can't tell where {} is ({err}); using the defaults, and changes won't be saved",
                    store::FILE
                ),
            }
        }
        Self {
            available: AtomicBool::new(available),
            shared: Mutex::new(shared),
            keys: Mutex::new(Keys::new(monitor, watch)),
            picking: AtomicBool::new(false),
            run: RunSlot::default(),
        }
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        // An edit is applied to a copy and put back whole, so a panic in an
        // earlier holder can't leave the menus half-changed.
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn keys(&self) -> MutexGuard<'_, Keys> {
        self.keys.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Binds Open as `config` and the Pause state want it. Call with
    /// `shared` locked, so binds land in the order of the changes.
    fn rebind(&self, app: &AppHandle, config: &Config, strict: bool) -> Result<(), PlatformError> {
        let paused = app
            .try_state::<GrapeState>()
            .is_some_and(|state| state.paused.load(Ordering::Acquire));
        let want = open_chord(config.enabled, paused, self.is_available(), config.hotkey);
        let mut keys = self.keys();
        let bound = keys.bind_open(want, strict);
        // A change that is about to be refused keeps the old hot key.
        if bound.is_ok() || !strict {
            keys.hotkey = config.hotkey;
        }
        bound
    }

    /// Sends the controller a copy of `config`, waiting for room: a
    /// dropped one would leave the wheel on old menus.
    fn push(app: &AppHandle, config: &Config) {
        if let Some(inputs) = app.try_state::<WheelInputs>() {
            inputs.send_waiting(Input::Shortcuts(Arc::new(Snapshot::of(config))));
        }
    }

    /// The view of `config`. Call with `shared` locked.
    fn view_of(&self, config: &Config, note: Option<&'static str>) -> ShortcutsView {
        let available = self.is_available();
        let binding = if available {
            self.keys().binding()
        } else {
            Binding::Off
        };
        let access = platform::input_access().into();
        let mut view = model::view(config, THIS_OS, binding, access, note.map(str::to_owned));
        // Also off on a Mac whose hot keys couldn't start.
        view.available = available;
        view
    }

    fn view(&self) -> ShortcutsView {
        let shared = self.shared();
        self.view_of(&shared.config, None)
    }

    /// Applies `edit` to the shortcuts: the hot key is bound first when it
    /// changed (a chord the OS refuses changes nothing), then the change
    /// takes effect, is saved, and goes to the wheel. If saving fails the
    /// change still applies, and the window is told. `what` is for the log.
    fn change(
        &self,
        app: &AppHandle,
        what: &'static str,
        edit: impl FnOnce(&mut Config) -> Result<Option<&'static str>, &'static str>,
    ) -> Result<ShortcutsView, String> {
        if !self.is_available() {
            return Err(model::UNAVAILABLE.to_owned());
        }
        let mut shared = self.shared();
        let mut new = shared.config.clone();
        let note = edit(&mut new)?;
        if new == shared.config {
            return Ok(self.view_of(&shared.config, note));
        }
        // Nothing the file couldn't hold is kept in memory either.
        model::stored_bytes(&new)?;
        let hotkey_changed = new.hotkey != shared.config.hotkey;
        if hotkey_changed || new.enabled != shared.config.enabled {
            if let Err(err) = self.rebind(app, &new, true) {
                log::warn!("shortcuts: the hot key wasn't bound ({err}); nothing changed");
                let why = if hotkey_changed {
                    HOTKEY_REFUSED
                } else {
                    OS_REFUSED
                };
                return Err(why.to_owned());
            }
        }
        shared.config = new;
        let saved = shared.save();
        if saved.is_ok() {
            log::info!("shortcuts: {what}; saved");
        }
        Self::push(app, &shared.config);
        let view = self.view_of(&shared.config, note);
        drop(shared);
        saved.map(|()| view).map_err(str::to_owned)
    }

    /// Opens the picker for `kind` on a blocking worker, owned by `window`
    /// where the OS takes an owner. `None` when it was closed, or when a
    /// picker is open already.
    async fn pick(&self, window: &WebviewWindow, kind: PickKind) -> Result<Option<Picked>, String> {
        if self.picking.swap(true, Ordering::AcqRel) {
            log::info!("shortcuts: a picker is already open");
            return Ok(None);
        }
        let owner = owner_of(window);
        let picked =
            tauri::async_runtime::spawn_blocking(move || platform::pick_target(kind, owner)).await;
        self.picking.store(false, Ordering::Release);
        match picked {
            Ok(Ok(Some(picked))) => Ok(Some(picked)),
            Ok(Ok(None)) => {
                log::info!("shortcuts: picker closed; nothing changed");
                Ok(None)
            }
            Ok(Err(err)) => {
                log::error!("shortcuts: could not open the picker: {err}");
                Err(PICKER_FAILED.to_owned())
            }
            Err(err) => {
                log::error!("shortcuts: the picker's worker failed: {err}");
                Err(PICKER_FAILED.to_owned())
            }
        }
    }
}

/// The window's raw handle, to own a picker on Windows.
#[cfg(target_os = "windows")]
fn owner_of(window: &WebviewWindow) -> Option<isize> {
    match window.hwnd() {
        Ok(hwnd) => Some(hwnd.0 as isize),
        Err(err) => {
            log::warn!("shortcuts: no window handle for the picker: {err}");
            None
        }
    }
}

/// Only Windows takes an owner for a picker.
#[cfg(not(target_os = "windows"))]
fn owner_of(_window: &WebviewWindow) -> Option<isize> {
    None
}

// ---------------------------------------------------------------------
// What the wheel's wiring calls.
// ---------------------------------------------------------------------

/// Starts the hot key monitor and the pointer watch, which send into
/// `hotkeys` and `pointer`. `false` when the wheel isn't available, or
/// when they couldn't start (it isn't available from then on).
pub fn start_sources(
    app: &AppHandle,
    hotkeys: SyncSender<HotkeySignal>,
    pointer: SyncSender<PointerSignal>,
) -> bool {
    let Some(state) = app.try_state::<ShortcutsState>() else {
        return false;
    };
    if !state.is_available() {
        return false;
    }
    let mut keys = state.keys();
    match keys.start(hotkeys, pointer) {
        Ok(()) => {
            log::info!("shortcuts: hot keys and pointer watch running");
            true
        }
        Err(err) => {
            log::error!("shortcuts: could not start the hot keys: {err}");
            keys.stop();
            state.available.store(false, Ordering::Release);
            false
        }
    }
}

/// Sends the controller the shortcuts as loaded and binds the hot key if
/// the wheel is on. Call once at start, after [`start_sources`], on the
/// thread that runs setup.
pub fn publish(app: &AppHandle) {
    let Some(state) = app.try_state::<ShortcutsState>() else {
        return;
    };
    if !state.is_available() {
        return;
    }
    let shared = state.shared();
    ShortcutsState::push(app, &shared.config);
    bind_as_set(&state, app, &shared.config);
}

/// Binds Open as things are now, and logs how that went.
fn bind_as_set(state: &ShortcutsState, app: &AppHandle, config: &Config) {
    match state.rebind(app, config, false) {
        Ok(()) => log::info!(
            "shortcuts: the hot key is {}",
            match state.keys().binding() {
                Binding::Bound => "bound",
                Binding::Off | Binding::Failed => "not bound",
            }
        ),
        Err(err) => log::warn!("shortcuts: the hot key wasn't bound: {err}"),
    }
}

/// Tray → Pause Grape was toggled: a pause stops the running shortcut and
/// lets go of the hot key, so a paused Grape never swallows the chord; a
/// resume binds it again. Returns at once: the bind happens on a thread of
/// its own, since the main thread mustn't wait for the hot keys.
pub fn set_paused(app: &AppHandle, paused: bool) {
    if paused {
        cancel_run(app, Why::Paused);
    }
    let Some(state) = app.try_state::<ShortcutsState>() else {
        return;
    };
    if !state.is_available() {
        return;
    }
    let worker = app.clone();
    let spawned = thread::Builder::new()
        .name("grape-shortcuts-bind".to_owned())
        .spawn(move || {
            let Some(state) = worker.try_state::<ShortcutsState>() else {
                return;
            };
            // Read under the lock: the last toggle is the one that holds.
            let shared = state.shared();
            bind_as_set(&state, &worker, &shared.config);
        });
    if let Err(err) = spawned {
        log::error!("shortcuts: could not start the thread that binds the hot key: {err}");
    }
}

/// A Shortcuts wheel opened (`true`) or closed: binds or lets go of Escape
/// and Return and switches the pointer watch, and returns once that is
/// done. `false` only when the wheel opened and Escape couldn't be bound:
/// the caller must close the wheel at once.
pub fn track(app: &AppHandle, on: bool) -> bool {
    let Some(state) = app.try_state::<ShortcutsState>() else {
        return !on;
    };
    let tracked = state.keys().track(on);
    match tracked {
        Ok(()) => true,
        Err(err) => {
            log::error!("shortcuts: Esc couldn't be bound ({err}); closing the wheel");
            false
        }
    }
}

/// Stops the hot key monitor and the pointer watch. Call on exit, on the
/// main thread. Never waits: a bind in flight holds the lock while it
/// waits for this very thread, and the OS lets go of the hot keys when
/// Grape exits anyway.
pub fn stop(app: &AppHandle) {
    let Some(state) = app.try_state::<ShortcutsState>() else {
        return;
    };
    let stopped = match state.keys.try_lock() {
        Ok(mut keys) => keys.stop(),
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().stop(),
        Err(TryLockError::WouldBlock) => {
            log::warn!("shortcuts: the hot keys were busy; left for the OS to let go of");
            false
        }
    };
    if stopped {
        log::info!("shortcuts: hot keys and pointer watch stopped");
    }
}

/// Runs a picked shortcut on a `"grape-shortcut"` thread, one at a time.
/// The thread sends [`Input::RunEnded`] when it ends. `Some` is that same
/// answer for a run that didn't start, for the caller to give the
/// controller itself.
pub fn start_run(app: &AppHandle, run: ShortcutRun) -> Option<Input> {
    let seq = run.seq;
    let Some(state) = app.try_state::<ShortcutsState>() else {
        log::warn!("shortcut {seq}: not run: the Shortcuts wheel isn't set up");
        return Some(Input::RunEnded);
    };
    let paused = app
        .try_state::<GrapeState>()
        .is_some_and(|state| state.paused.load(Ordering::Acquire));
    if paused {
        log::info!("shortcut {seq}: not run: Grape is paused");
        return Some(Input::RunEnded);
    }
    let cancel = match state.run.begin(&run.name) {
        Begin::Started(cancel) => cancel,
        Begin::Busy => {
            log::warn!("shortcut {seq}: not run: another one is still running");
            return Some(Input::RunEnded);
        }
        Begin::Quitting => {
            log::info!("shortcut {seq}: not run: Grape is quitting");
            return Some(Input::RunEnded);
        }
    };
    let worker = app.clone();
    let spawned = thread::Builder::new()
        .name("grape-shortcut".to_owned())
        .spawn(move || run_on_thread(&worker, &run, &cancel));
    match spawned {
        Ok(_) => None,
        Err(err) => {
            log::error!("shortcut {seq}: could not start its thread: {err}");
            state.run.finish();
            Some(Input::RunEnded)
        }
    }
}

/// Frees the runner and tells the controller, when the run's thread ends,
/// also if a step panicked: without the answer the hot key would never
/// open a wheel again.
struct Finished<'a>(&'a AppHandle);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        // Freed first, so the wheel the answer allows finds the slot free.
        if let Some(state) = self.0.try_state::<ShortcutsState>() {
            state.run.finish();
        }
        if let Some(inputs) = self.0.try_state::<WheelInputs>() {
            inputs.send_waiting(Input::RunEnded);
        }
    }
}

fn run_on_thread(app: &AppHandle, run: &ShortcutRun, cancel: &AtomicBool) {
    let _finished = Finished(app);
    let Some(state) = app.try_state::<ShortcutsState>() else {
        return;
    };
    let notify = {
        let app = app.clone();
        let name = run.name.clone();
        let (anchor, display) = (run.anchor, run.display);
        move |text: &str| show(&app, run::notice(&name, text), anchor, &display)
    };
    let actuator = SystemActuator::new(
        app.path().home_dir().ok(),
        Arc::clone(&state.run.child),
        notify,
    );
    let outcome = run::run_shortcut(run, &actuator, cancel);
    if let Some(toast) = run::toast_for(&run.name, &outcome) {
        show(app, toast, run.anchor, &run.display);
    }
}

/// The hot key went down while a shortcut runs: says which, near where
/// the wheel would have opened.
pub fn show_still_running(app: &AppHandle, anchor: (i32, i32), display: &DisplayBounds) {
    let Some(state) = app.try_state::<ShortcutsState>() else {
        return;
    };
    // `None` when it ended in the meantime: nothing to say then.
    if let Some(name) = state.run.name() {
        show(app, run::still_running(&name), anchor, display);
    }
}

/// Stops the running shortcut, if there is one: sets its cancel flag and
/// ends the program a step is running, with everything that started.
/// Returns at once. After [`Why::Quitting`] no run starts again.
pub fn cancel_run(app: &AppHandle, why: Why) {
    let Some(state) = app.try_state::<ShortcutsState>() else {
        return;
    };
    if state.run.cancel(why) {
        log::info!("shortcuts: stopping the running shortcut: {}", why.words());
    }
}

/// Waits up to [`QUIT_WAIT`] for the cancelled run to end. For
/// `RunEvent::Exit`, after [`cancel_run`].
pub fn wait_for_run(app: &AppHandle) {
    let Some(state) = app.try_state::<ShortcutsState>() else {
        return;
    };
    if state.run.name().is_none() {
        return;
    }
    let started = Instant::now();
    if state.run.wait(QUIT_WAIT) {
        log::info!(
            "shortcuts: waited {} ms for the cancelled shortcut to end",
            started.elapsed().as_millis()
        );
    } else {
        log::warn!(
            "shortcuts: a shortcut was still running after {} s; quitting anyway",
            QUIT_WAIT.as_secs()
        );
    }
}

// ---------------------------------------------------------------------
// The Settings window's commands. None of them runs a shortcut.
// ---------------------------------------------------------------------

/// The shortcuts as they are now. The first answer carries what the load
/// had to say.
#[tauri::command]
pub async fn shortcuts_get(state: State<'_, ShortcutsState>) -> Result<ShortcutsView, String> {
    let mut shared = state.shared();
    let note = shared.note.take();
    Ok(state.view_of(&shared.config, note))
}

/// Turns the wheel's hot key on or off. Answers once it is bound.
#[tauri::command]
pub async fn shortcuts_set_enabled(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    on: bool,
) -> Result<ShortcutsView, String> {
    let what = if on {
        "the wheel is on"
    } else {
        "the wheel is off"
    };
    state.change(&app, what, |config| {
        model::set_enabled(config, on).map(|()| None)
    })
}

/// Sets the chord that opens the wheel, and answers once it is bound.
/// `null` removes it and turns the wheel off.
#[tauri::command]
pub async fn shortcuts_set_hotkey(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    chord: Option<ChordWire>,
) -> Result<ShortcutsView, String> {
    state.change(&app, "the hot key changed", |config| {
        model::set_hotkey(config, chord.as_ref(), THIS_OS).map(|()| None)
    })
}

/// Sets where the wheel opens.
#[tauri::command]
pub async fn shortcuts_set_opens_at(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    at: OpensAt,
) -> Result<ShortcutsView, String> {
    state.change(&app, "where the wheel opens changed", |config| {
        model::set_opens_at(config, at).map(|()| None)
    })
}

/// Asks the OS to let Grape press keys: its own prompt the first time in a
/// launch, and its settings pane when a later press finds Grape still
/// without access.
#[tauri::command]
pub async fn shortcuts_request_access(
    state: State<'_, ShortcutsState>,
) -> Result<ShortcutsView, String> {
    if !state.is_available() {
        return Err(model::UNAVAILABLE.to_owned());
    }
    if matches!(platform::input_access(), InputAccess::Denied) {
        let open_settings = !run::first_access_request();
        // The settings pane is opened by a program this waits for.
        let asked = tauri::async_runtime::spawn_blocking(move || {
            platform::request_input_access(open_settings)
        })
        .await;
        match asked {
            Ok(Ok(())) => log::info!(
                "shortcuts: asked for input access{}",
                if open_settings {
                    ", and opened its settings"
                } else {
                    ""
                }
            ),
            Ok(Err(err)) => {
                log::warn!("shortcuts: couldn't ask for input access: {err}");
                return Err(ACCESS_FAILED.to_owned());
            }
            Err(err) => {
                log::error!("shortcuts: the worker that asks for input access failed: {err}");
                return Err(ACCESS_FAILED.to_owned());
            }
        }
    }
    Ok(state.view())
}

/// Opens the app picker and adds a menu for the picked app. Cancelling
/// changes nothing. An app with a menu, an app with no id and Grape itself
/// are refused.
#[tauri::command]
pub async fn shortcuts_add_menu(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, ShortcutsState>,
) -> Result<ShortcutsView, String> {
    if !state.is_available() {
        return Err(model::UNAVAILABLE.to_owned());
    }
    let Some(picked) = state.pick(&window, PickKind::App).await? else {
        return Ok(state.view());
    };
    let Some(identity) = picked.app else {
        log::info!("shortcuts: the picked app has no id; no menu added");
        return Err(model::APP_UNUSABLE.to_owned());
    };
    let own_id = app.config().identifier.clone();
    state.change(&app, "an app's menu added", |config| {
        model::add_app_menu(config, &identity.id, &identity.name, &own_id).map(|_| None)
    })
}

/// Removes an app's menu.
#[tauri::command]
pub async fn shortcuts_remove_menu(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    menu: Id,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a menu removed", |config| {
        model::remove_menu(config, menu).map(|()| None)
    })
}

/// Turns an app menu's Everywhere group on or off.
#[tauri::command]
pub async fn shortcuts_set_menu_everywhere(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    menu: Id,
    on: bool,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a menu's Everywhere group switched", |config| {
        model::set_menu_everywhere(config, menu, on)
    })
}

/// Adds a new shortcut or group to a menu, or a shortcut to a group.
#[tauri::command]
pub async fn shortcuts_add_item(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    menu: Id,
    group: Option<Id>,
    kind: NewItem,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a shortcut or group added", |config| {
        model::add_item(config, menu, group, kind).map(|_| None)
    })
}

/// Renames a shortcut or a group and sets its icon.
#[tauri::command]
pub async fn shortcuts_set_item(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    item: Id,
    name: String,
    icon: Icon,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a shortcut or group changed", |config| {
        model::set_item(config, item, &name, &icon).map(|()| None)
    })
}

/// Removes a shortcut, or a group and its shortcuts.
#[tauri::command]
pub async fn shortcuts_remove_item(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    item: Id,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a shortcut or group removed", |config| {
        model::remove_item(config, item).map(|()| None)
    })
}

/// Moves a shortcut or a group to place `to` of its own ring or group.
#[tauri::command]
pub async fn shortcuts_move_item(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    item: Id,
    to: usize,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a shortcut or group moved", |config| {
        model::move_item(config, item, to).map(|()| None)
    })
}

/// Adds a step of `kind`, with nothing chosen yet, to a shortcut.
#[tauri::command]
pub async fn shortcuts_add_step(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    item: Id,
    kind: StepKindId,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a step added", |config| {
        model::add_step(config, item, kind).map(|_| None)
    })
}

/// Gives a step new values.
#[tauri::command]
pub async fn shortcuts_set_step(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    step: Id,
    value: StepEdit,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a step changed", |config| {
        model::set_step(config, step, &value).map(|()| None)
    })
}

/// Removes a step.
#[tauri::command]
pub async fn shortcuts_remove_step(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    step: Id,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a step removed", |config| {
        model::remove_step(config, step).map(|()| None)
    })
}

/// Moves a step to place `to` of its own shortcut.
#[tauri::command]
pub async fn shortcuts_move_step(
    app: AppHandle,
    state: State<'_, ShortcutsState>,
    step: Id,
    to: usize,
) -> Result<ShortcutsView, String> {
    state.change(&app, "a step moved", |config| {
        model::move_step(config, step, to).map(|()| None)
    })
}

/// Opens the picker for an Open app or Open file step and sets what was
/// picked as its target. Cancelling changes nothing. The page never sends
/// a path.
#[tauri::command]
pub async fn shortcuts_pick_target(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, ShortcutsState>,
    step: Id,
) -> Result<ShortcutsView, String> {
    if !state.is_available() {
        return Err(model::UNAVAILABLE.to_owned());
    }
    let kind = model::pick_kind(&state.shared().config, step)?;
    let Some(picked) = state.pick(&window, kind).await? else {
        return Ok(state.view());
    };
    let target = Target::picked(kind, picked);
    state.change(&app, "a step's target picked", |config| {
        model::set_target(config, step, &target).map(|()| None)
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::mpsc;

    use grape_platform::mock::{ScriptedHotkeys, ScriptedPointer};
    use serde_json::Value;

    use super::run::{Failure, Outcome};
    use super::*;

    /// The commands of the Settings window that this file adds, in the
    /// order of the design note's table.
    const COMMANDS: [&str; 17] = [
        "shortcuts_get",
        "shortcuts_set_enabled",
        "shortcuts_set_hotkey",
        "shortcuts_set_opens_at",
        "shortcuts_request_access",
        "shortcuts_add_menu",
        "shortcuts_remove_menu",
        "shortcuts_set_menu_everywhere",
        "shortcuts_add_item",
        "shortcuts_set_item",
        "shortcuts_remove_item",
        "shortcuts_move_item",
        "shortcuts_add_step",
        "shortcuts_set_step",
        "shortcuts_remove_step",
        "shortcuts_move_step",
        "shortcuts_pick_target",
    ];

    /// Stands in for a name the user typed.
    const MARKER: &str = "zq-marker";

    fn chord(key: KeyCode, mods: Modifiers) -> Chord {
        Chord { key, mods }
    }

    const CTRL_ALT: Modifiers = Modifiers {
        ctrl: true,
        alt: true,
        ..Modifiers::NONE
    };

    fn hotkey() -> Chord {
        chord(KeyCode::KeyG, CTRL_ALT)
    }

    #[test]
    fn open_is_bound_only_while_on_unpaused_and_available() {
        let key = Some(hotkey());
        assert_eq!(open_chord(true, false, true, key), key);
        assert_eq!(open_chord(false, false, true, key), None);
        assert_eq!(open_chord(true, true, true, key), None);
        assert_eq!(open_chord(true, false, false, key), None);
        assert_eq!(open_chord(true, false, true, None), None);
        // Every other mix of the three switches binds nothing either.
        for bits in 0_u8..8 {
            let (enabled, paused, available) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
            let bound = open_chord(enabled, paused, available, key);
            assert_eq!(bound.is_some(), enabled && !paused && available, "{bits}");
            assert_eq!(open_chord(enabled, paused, available, None), None);
        }
    }

    #[test]
    fn esc_and_return_get_every_subset_of_the_hot_keys_modifiers() {
        let (escape, confirm) = track_chords(Some(hotkey()));
        let subsets = [
            Modifiers::NONE,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
            Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
            CTRL_ALT,
        ];
        assert_eq!(escape, subsets.map(|mods| chord(KeyCode::Escape, mods)));
        assert_eq!(confirm, subsets.map(|mods| chord(KeyCode::Enter, mods)));
    }

    #[test]
    fn a_bare_or_missing_hot_key_binds_bare_esc_and_return() {
        for hotkey in [None, Some(chord(KeyCode::F5, Modifiers::NONE))] {
            let (escape, confirm) = track_chords(hotkey);
            assert_eq!(escape, [chord(KeyCode::Escape, Modifiers::NONE)]);
            assert_eq!(confirm, [chord(KeyCode::Enter, Modifiers::NONE)]);
        }
    }

    #[test]
    fn four_modifiers_make_sixteen_chords_each() {
        let all = Modifiers {
            shift: true,
            ctrl: true,
            alt: true,
            meta: true,
        };
        let (escape, confirm) = track_chords(Some(chord(KeyCode::KeyK, all)));
        assert_eq!((escape.len(), confirm.len()), (16, 16));
        assert!(escape.iter().all(|chord| chord.key == KeyCode::Escape));
        assert!(confirm.iter().all(|chord| chord.key == KeyCode::Enter));
    }

    fn keys(monitor: ScriptedHotkeys, watch: ScriptedPointer) -> Keys {
        let mut keys = Keys::new(Some(Box::new(monitor)), Some(Box::new(watch)));
        keys.hotkey = Some(hotkey());
        keys
    }

    #[test]
    fn binding_open_follows_what_is_wanted() {
        let monitor = ScriptedHotkeys::new(Vec::new());
        let binds = monitor.binds();
        let mut keys = keys(monitor, ScriptedPointer::new(Vec::new()));
        assert_eq!(keys.binding(), Binding::Off);

        keys.bind_open(Some(hotkey()), false).unwrap();
        assert_eq!(keys.binding(), Binding::Bound);
        assert_eq!(binds.bound(HotkeyId::Open), [hotkey()]);
        // The same again asks the OS for nothing.
        keys.bind_open(Some(hotkey()), true).unwrap();
        assert_eq!(binds.calls().len(), 1);

        let other = chord(KeyCode::F5, Modifiers::NONE);
        keys.bind_open(Some(other), true).unwrap();
        assert_eq!(binds.bound(HotkeyId::Open), [other]);

        keys.bind_open(None, false).unwrap();
        assert_eq!(keys.binding(), Binding::Off);
        assert!(binds.bound(HotkeyId::Open).is_empty());
        // Nothing bound: nothing to let go of.
        keys.bind_open(None, false).unwrap();
        assert_eq!(binds.calls().len(), 3);
    }

    #[test]
    fn a_refused_chord_leaves_the_old_one_bound() {
        let taken = chord(KeyCode::F6, Modifiers::NONE);
        let monitor = ScriptedHotkeys::new(Vec::new()).refusing(taken);
        let binds = monitor.binds();
        let mut keys = keys(monitor, ScriptedPointer::new(Vec::new()));
        keys.bind_open(Some(hotkey()), false).unwrap();

        assert_eq!(
            keys.bind_open(Some(taken), true),
            Err(ScriptedHotkeys::REFUSED)
        );
        assert_eq!(keys.binding(), Binding::Bound);
        assert_eq!(keys.open, Some(hotkey()));
        assert_eq!(binds.bound(HotkeyId::Open), [hotkey()]);
    }

    #[test]
    fn a_refusal_outside_a_change_shows_as_failed_until_it_binds_or_is_let_go() {
        let taken = chord(KeyCode::F6, Modifiers::NONE);
        let monitor = ScriptedHotkeys::new(Vec::new()).refusing(taken);
        let mut keys = keys(monitor, ScriptedPointer::new(Vec::new()));

        // At launch or after a resume.
        assert!(keys.bind_open(Some(taken), false).is_err());
        assert_eq!(keys.binding(), Binding::Failed);
        // A change that is refused leaves that as it is.
        assert!(keys.bind_open(Some(taken), true).is_err());
        assert_eq!(keys.binding(), Binding::Failed);
        // Another chord binds.
        keys.bind_open(Some(hotkey()), true).unwrap();
        assert_eq!(keys.binding(), Binding::Bound);

        keys.bind_open(None, false).unwrap();
        assert!(keys.bind_open(Some(taken), false).is_err());
        assert_eq!(keys.binding(), Binding::Failed);
        // Paused or switched off: nothing is wanted, so nothing failed.
        keys.bind_open(None, false).unwrap();
        assert_eq!(keys.binding(), Binding::Off);
    }

    #[test]
    fn a_switch_refused_while_off_stays_off() {
        let monitor = ScriptedHotkeys::new(Vec::new()).refusing(hotkey());
        let mut keys = keys(monitor, ScriptedPointer::new(Vec::new()));
        assert!(keys.bind_open(Some(hotkey()), true).is_err());
        assert_eq!(keys.binding(), Binding::Off);
    }

    #[test]
    fn without_a_monitor_nothing_binds() {
        let mut keys = Keys::new(None, None);
        assert_eq!(
            keys.bind_open(Some(hotkey()), false),
            Err(PlatformError::Unsupported)
        );
        assert_eq!(keys.track(true), Err(PlatformError::Unsupported));
        assert_eq!(keys.track(false), Ok(()));
        let (hotkeys, _) = mpsc::sync_channel(1);
        let (pointer, _) = mpsc::sync_channel(1);
        assert_eq!(
            keys.start(hotkeys, pointer),
            Err(PlatformError::Unsupported)
        );
        assert!(!keys.stop());
    }

    #[test]
    fn tracking_binds_esc_and_return_then_lets_go_of_both() {
        let monitor = ScriptedHotkeys::new(Vec::new());
        let binds = monitor.binds();
        let watch = ScriptedPointer::new(Vec::new());
        let activity = watch.activity();
        let mut keys = keys(monitor, watch);
        let (escape, confirm) = track_chords(Some(hotkey()));

        keys.track(true).unwrap();
        assert_eq!(binds.bound(HotkeyId::Escape), escape);
        assert_eq!(binds.bound(HotkeyId::Confirm), confirm);
        assert_eq!(activity.calls(), [true]);

        keys.track(false).unwrap();
        assert!(binds.bound(HotkeyId::Escape).is_empty());
        assert!(binds.bound(HotkeyId::Confirm).is_empty());
        assert_eq!(activity.calls(), [true, false]);
        // Open was never touched.
        assert!(binds.calls().iter().all(|call| call.id != HotkeyId::Open));
    }

    #[test]
    fn tracking_follows_the_hot_key_of_the_moment() {
        let monitor = ScriptedHotkeys::new(Vec::new());
        let binds = monitor.binds();
        let mut keys = keys(monitor, ScriptedPointer::new(Vec::new()));
        keys.hotkey = Some(chord(KeyCode::F5, Modifiers::NONE));
        keys.track(true).unwrap();
        assert_eq!(
            binds.bound(HotkeyId::Escape),
            [chord(KeyCode::Escape, Modifiers::NONE)]
        );
    }

    #[test]
    fn a_wheel_whose_esc_cannot_be_bound_tracks_nothing() {
        let refused = chord(KeyCode::Escape, CTRL_ALT);
        let monitor = ScriptedHotkeys::new(Vec::new()).refusing(refused);
        let binds = monitor.binds();
        let watch = ScriptedPointer::new(Vec::new());
        let activity = watch.activity();
        let mut keys = keys(monitor, watch);

        assert_eq!(keys.track(true), Err(ScriptedHotkeys::REFUSED));
        assert!(binds.bound(HotkeyId::Escape).is_empty());
        assert!(binds.bound(HotkeyId::Confirm).is_empty());
        assert!(activity.calls().is_empty());
        // The close that follows lets go of what there is: nothing.
        keys.track(false).unwrap();
        assert_eq!(activity.calls(), [false]);
    }

    #[test]
    fn a_return_that_cannot_be_bound_still_opens_the_wheel() {
        let refused = chord(KeyCode::Enter, Modifiers::NONE);
        let monitor = ScriptedHotkeys::new(Vec::new()).refusing(refused);
        let binds = monitor.binds();
        let watch = ScriptedPointer::new(Vec::new());
        let activity = watch.activity();
        let mut keys = keys(monitor, watch);

        keys.track(true).unwrap();
        assert!(!binds.bound(HotkeyId::Escape).is_empty());
        assert!(binds.bound(HotkeyId::Confirm).is_empty());
        assert_eq!(activity.calls(), [true]);
    }

    #[test]
    fn stopping_lets_go_of_everything() {
        let monitor = ScriptedHotkeys::new(Vec::new());
        let binds = monitor.binds();
        let mut keys = keys(monitor, ScriptedPointer::new(Vec::new()));
        let (hotkeys, _hotkey_rx) = mpsc::sync_channel(4);
        let (pointer, _pointer_rx) = mpsc::sync_channel(4);
        keys.start(hotkeys, pointer).unwrap();
        keys.bind_open(Some(hotkey()), false).unwrap();
        keys.track(true).unwrap();

        assert!(keys.stop());
        assert_eq!(keys.binding(), Binding::Off);
        for id in [HotkeyId::Open, HotkeyId::Escape, HotkeyId::Confirm] {
            assert!(binds.bound(id).is_empty(), "{id:?}");
        }
        // Gone for good.
        assert!(!keys.stop());
        assert!(keys.bind_open(Some(hotkey()), false).is_err());
    }

    #[test]
    fn one_run_at_a_time() {
        let slot = RunSlot::default();
        assert_eq!(slot.name(), None);
        let Begin::Started(cancel) = slot.begin(MARKER) else {
            panic!("the first run starts");
        };
        assert_eq!(slot.name().as_deref(), Some(MARKER));
        assert!(matches!(slot.begin("another"), Begin::Busy));
        assert_eq!(slot.name().as_deref(), Some(MARKER));
        assert!(!cancel.load(Ordering::SeqCst));

        slot.finish();
        assert_eq!(slot.name(), None);
        assert!(matches!(slot.begin("another"), Begin::Started(_)));
    }

    #[test]
    fn a_cancel_sets_the_running_runs_flag_only() {
        let slot = RunSlot::default();
        assert!(!slot.cancel(Why::Paused));
        let Begin::Started(first) = slot.begin(MARKER) else {
            panic!("the first run starts");
        };
        assert!(slot.cancel(Why::Paused));
        assert!(first.load(Ordering::SeqCst));
        slot.finish();

        // A pause stops one run, not the next.
        let Begin::Started(second) = slot.begin(MARKER) else {
            panic!("a run starts after a pause");
        };
        assert!(!second.load(Ordering::SeqCst));
    }

    #[test]
    fn nothing_runs_once_grape_is_quitting() {
        let slot = RunSlot::default();
        let Begin::Started(cancel) = slot.begin(MARKER) else {
            panic!("the first run starts");
        };
        assert!(slot.cancel(Why::Quitting));
        assert!(cancel.load(Ordering::SeqCst));
        slot.finish();
        assert!(matches!(slot.begin(MARKER), Begin::Quitting));
        assert_eq!(slot.name(), None);

        // Also when nothing ran at the time.
        let idle = RunSlot::default();
        assert!(!idle.cancel(Why::Quitting));
        assert!(matches!(idle.begin(MARKER), Begin::Quitting));
    }

    #[test]
    fn the_exit_waits_for_the_run_but_not_for_long() {
        let slot = Arc::new(RunSlot::default());
        assert!(slot.wait(Duration::ZERO));
        assert!(matches!(slot.begin(MARKER), Begin::Started(_)));
        assert!(!slot.wait(Duration::from_millis(20)));

        let runner = Arc::clone(&slot);
        let ended = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            runner.finish();
        });
        assert!(slot.wait(Duration::from_secs(30)));
        ended.join().unwrap();
    }

    #[test]
    fn a_failed_run_is_an_error_toast_that_names_the_shortcut() {
        let outcome = Outcome::Failed {
            step: 0,
            kind: StepKindId::Keys,
            why: Failure::KeysHeld,
        };
        let toast = toast_show(7, run::toast_for(MARKER, &outcome).unwrap());
        assert_eq!(
            toast,
            ToastShow {
                id: 7,
                tone: ToastTone::Error,
                title: format!("Couldn't finish {MARKER}"),
                detail: Some("Let go of the keys first.".to_owned()),
                progress: None,
                revealable: false,
            }
        );
    }

    #[test]
    fn nothing_to_run_and_the_notices_are_info_toasts() {
        let nothing = toast_show(1, run::toast_for(MARKER, &Outcome::NothingToRun).unwrap());
        assert_eq!(nothing.tone, ToastTone::Info);
        assert_eq!(nothing.title, "Nothing to run yet");
        assert!(nothing.detail.is_some());

        let still = toast_show(2, run::still_running(MARKER));
        assert_eq!(still.tone, ToastTone::Info);
        assert_eq!(still.title, format!("Still running {MARKER}"));
        assert_eq!(still.detail, None);

        let notice = toast_show(3, run::notice(MARKER, "one\ntwo"));
        assert_eq!(notice.tone, ToastTone::Info);
        assert_eq!(notice.title, MARKER);
        assert_eq!(notice.detail.as_deref(), Some("one two"));

        for toast in [nothing, still, notice] {
            assert!(!toast.revealable);
            assert_eq!(toast.progress, None);
        }
    }

    #[test]
    fn a_run_that_finished_or_was_cancelled_shows_nothing() {
        assert_eq!(run::toast_for(MARKER, &Outcome::Finished), None);
        assert_eq!(run::toast_for(MARKER, &Outcome::Cancelled), None);
    }

    #[test]
    fn why_a_run_stopped_is_logged_in_fixed_words() {
        assert_eq!(Why::Paused.words(), "Grape was paused");
        assert_eq!(Why::Quitting.words(), "Grape is quitting");
    }

    /// Every quoted `settings_…` or `shortcuts_…` name in `text`, in order.
    fn window_commands(text: &str) -> Vec<&str> {
        text.split('"')
            .skip(1)
            .step_by(2)
            .filter(|name| name.starts_with("settings_") || name.starts_with("shortcuts_"))
            .collect()
    }

    /// The names `generate_handler!` gives for `module`, in order.
    fn handlers<'a>(lib: &'a str, module: &str) -> Vec<&'a str> {
        let prefix = format!("{module}::{module}_");
        lib.lines()
            .map(|line| line.trim().trim_end_matches(','))
            .filter(|line| line.starts_with(&prefix))
            .filter_map(|line| line.split("::").nth(1))
            .collect()
    }

    #[test]
    fn build_rs_lists_the_shortcuts_commands() {
        let listed = window_commands(include_str!("../../build.rs"));
        let shortcuts: Vec<&str> = listed
            .iter()
            .copied()
            .filter(|name| name.starts_with("shortcuts_"))
            .collect();
        assert_eq!(shortcuts, COMMANDS);
    }

    #[test]
    fn the_handler_takes_the_commands_build_rs_lists() {
        let listed = window_commands(include_str!("../../build.rs"));
        let lib = include_str!("../lib.rs");
        let mut handled = handlers(lib, "settings");
        handled.extend(handlers(lib, "shortcuts"));
        assert_eq!(handled, listed);
    }

    #[test]
    fn the_settings_window_gets_exactly_its_commands() {
        let listed = window_commands(include_str!("../../build.rs"));
        let wanted: Vec<String> = listed
            .iter()
            .map(|name| format!("allow-{}", name.replace('_', "-")))
            .collect();
        let settings: Value =
            serde_json::from_str(include_str!("../../capabilities/settings.json")).unwrap();
        assert_eq!(settings["windows"], serde_json::json!(["settings"]));
        assert_eq!(settings["permissions"], serde_json::json!(wanted));
    }

    #[test]
    fn no_other_window_gets_a_shortcuts_command() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).unwrap();
            if name == "settings.json" {
                seen += 1;
                continue;
            }
            assert!(!text.contains("allow-shortcuts-"), "{name}");
            assert!(!text.contains("allow-settings-"), "{name}");
        }
        assert_eq!(seen, 1);
    }
}
