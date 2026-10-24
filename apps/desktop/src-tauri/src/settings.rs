//! The user's settings (`docs/ARCHITECTURE.md` → "Settings (persisted)")
//! and the Settings window's commands.
//!
//! - [`AppSettings`] is loaded once in setup from `settings.json`
//!   (tauri-plugin-store, in the app data folder). Loading is total: a
//!   value that's missing, of the wrong type, a key this OS doesn't offer
//!   or a relative path falls back to its default, with a warning that
//!   names the key and never the file's contents. A `settings.json` that's
//!   there but can't be read or parsed is first renamed aside
//!   (`settings.unreadable.json`), because the store would write over it
//!   ([`settle_settings_file`]).
//! - Every change applies at once and is saved at once. Keys and sounds go
//!   to the wheel controller ([`Input::Configure`]), haptics to the flag
//!   the wheel reads on every highlight, and the folders to the next job
//!   ([`planning_settings`]). A chosen fallback folder that's gone is
//!   never recreated: planning uses the default until it's back.
//! - Each command answers with a [`SettingsView`] (the UI mirrors it in
//!   `ui/src/settings/model.ts`, and both sides test against the golden
//!   JSON in `ui/src/shared/fixtures/`), or with a short sentence the UI
//!   shows. The window never sends a path: folders are picked here, with
//!   the OS's folder picker.
//! - Launch at login is tauri-plugin-autostart. A release build turns it on
//!   at its first launch ([`first_launch_rule`]); a debug build never
//!   touches it on its own, so `pnpm tauri dev` doesn't register a dev
//!   binary at login.
//!
//! The model (loading, the key swap rule, the view, what to store) is pure
//! and tested here without Tauri.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use grape_core::{OutputLocation, Platform, Settings};
use grape_platform::file_actions;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager, State, WebviewWindow, Wry};
use tauri_plugin_autostart::AutoLaunchManager;
use tauri_plugin_store::{Store, StoreExt};

use crate::controller::{ControllerConfig, Input, Key, KeyMap, THIS_OS};
use crate::wheel::WheelInputs;

/// The store's file, in the app data folder (the store plugin's default).
pub const SETTINGS_FILE: &str = "settings.json";
/// An unreadable `settings.json` is renamed to this, plus `.json` (or
/// ` 2.json`, ` 3.json`… when that's taken), in the same folder.
const UNREADABLE_STEM: &str = "settings.unreadable";
/// How many `settings.unreadable` names to try before giving up.
const UNREADABLE_NAMES: u32 = 999;
/// Grape's own folder in Downloads: the default fallback folder.
const FALLBACK_FOLDER: &str = "Grape";

// Store keys, exactly as in docs/ARCHITECTURE.md → Settings.
const KEYS_CONVERSIONS: &str = "keys.conversions";
const KEYS_TOOLS: &str = "keys.tools";
const KEYS_ACTIONS: &str = "keys.actions";
const OUTPUT_LOCATION: &str = "output.location";
const OUTPUT_FALLBACK_DIR: &str = "output.fallback_dir";
const SOUNDS: &str = "sounds";
const HAPTICS: &str = "haptics";
const LAUNCH_AT_LOGIN: &str = "launch_at_login";
/// `output.location` for "next to the original".
const NEXT_TO_SOURCE: &str = "next_to_source";

// What the Settings window shows when a command fails.
const SAVE_FAILED: &str = "Couldn't save your settings.";
const KEY_NOT_OFFERED: &str = "That key can't open a wheel here.";
const NO_HAPTICS: &str = "Haptics are a Mac setting.";
const LAUNCH_FAILED: &str = "Couldn't change launch at login.";
const PICKER_FAILED: &str = "Couldn't open the folder picker.";
const NOT_A_FOLDER: &str = "Grape can't use that folder.";

/// The settings Grape follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSettings {
    /// Which key opens which wheel. Always three different keys.
    pub keys: KeyMap,
    /// Where outputs go.
    pub output: OutputLocation,
    /// The folder for outputs whose destination isn't writable. `None`
    /// means the default, `Downloads/Grape`.
    pub fallback_dir: Option<PathBuf>,
    /// The highlight tick.
    pub sounds: bool,
    /// The trackpad tick (macOS). Stored on Windows too, and ignored.
    pub haptics: bool,
    /// The user's choice, or `None` if nothing was ever stored.
    pub launch_at_login: Option<bool>,
}

impl AppSettings {
    /// The defaults on `platform` (DECISIONS D6 for the keys).
    pub fn defaults(platform: Platform) -> Self {
        Self {
            keys: default_keys(platform),
            output: OutputLocation::NextToSource,
            fallback_dir: None,
            sounds: true,
            haptics: true,
            launch_at_login: None,
        }
    }
}

/// The default keys on `platform`: Shift / Ctrl / Alt on Windows, Shift /
/// Command / Control on macOS.
pub const fn default_keys(platform: Platform) -> KeyMap {
    match platform {
        Platform::Windows => KeyMap::WINDOWS,
        Platform::MacOs => KeyMap::MACOS,
    }
}

/// The keys that may open a wheel on `platform`, in the order Settings
/// lists them. Never the Windows key: it opens the Start menu.
pub const fn offered_keys(platform: Platform) -> &'static [Key] {
    match platform {
        Platform::Windows => &[Key::Shift, Key::Ctrl, Key::Alt],
        Platform::MacOs => &[Key::Shift, Key::Ctrl, Key::Alt, Key::Meta],
    }
}

/// A key's id in the store and the view (as its serde form).
const fn key_id(key: Key) -> &'static str {
    match key {
        Key::Shift => "shift",
        Key::Ctrl => "ctrl",
        Key::Alt => "alt",
        Key::Meta => "meta",
    }
}

fn key_from_id(id: &str) -> Option<Key> {
    [Key::Shift, Key::Ctrl, Key::Alt, Key::Meta]
        .into_iter()
        .find(|&key| key_id(key) == id)
}

/// One of the three wheels, as the Settings window names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wheel {
    /// The conversions wheel.
    Conversions,
    /// The tools wheel.
    Tools,
    /// The file actions wheel.
    Actions,
}

impl Wheel {
    const ALL: [Wheel; 3] = [Wheel::Conversions, Wheel::Tools, Wheel::Actions];

    /// The wheel's name in a sentence, as Settings labels it.
    const fn title(self) -> &'static str {
        match self {
            Wheel::Conversions => "Conversions",
            Wheel::Tools => "Tools",
            Wheel::Actions => "File actions",
        }
    }

    const fn key_in(self, keys: KeyMap) -> Key {
        match self {
            Wheel::Conversions => keys.conversions,
            Wheel::Tools => keys.tools,
            Wheel::Actions => keys.actions,
        }
    }

    const fn set_in(self, keys: KeyMap, key: Key) -> KeyMap {
        let mut keys = keys;
        match self {
            Wheel::Conversions => keys.conversions = key,
            Wheel::Tools => keys.tools = key,
            Wheel::Actions => keys.actions = key,
        }
        keys
    }
}

/// `true` when the three wheels have three different keys.
fn distinct(keys: KeyMap) -> bool {
    keys.conversions != keys.tools && keys.tools != keys.actions && keys.conversions != keys.actions
}

/// The swap rule: `wheel` gets `key`, and if another wheel had `key`, that
/// wheel gets `wheel`'s old key. The note says what moved, e.g. "Ctrl now
/// opens Conversions, and Shift opens Tools.", and is `None` when nothing
/// had to swap.
///
/// # Errors
/// The sentence for the UI when `platform` doesn't offer `key`.
pub fn set_key(
    keys: KeyMap,
    wheel: Wheel,
    key: Key,
    platform: Platform,
) -> Result<(KeyMap, Option<String>), &'static str> {
    if !offered_keys(platform).contains(&key) {
        return Err(KEY_NOT_OFFERED);
    }
    let old = wheel.key_in(keys);
    if old == key {
        return Ok((keys, None));
    }
    let other = Wheel::ALL
        .into_iter()
        .find(|&other| other != wheel && other.key_in(keys) == key);
    let mut keys = wheel.set_in(keys, key);
    let note = other.map(|other| {
        keys = other.set_in(keys, old);
        format!(
            "{} now opens {}, and {} opens {}.",
            key.name_on(platform),
            wheel.title(),
            old.name_on(platform),
            other.title()
        )
    });
    Ok((keys, note))
}

/// What [`load`] found besides the values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// One line per stored value that fell back to its default, naming the
    /// key and why, never the value.
    pub warnings: Vec<String>,
    /// Keys with nothing stored yet (they use their defaults).
    pub missing: Vec<&'static str>,
}

/// Reads the settings for `platform` from `stored` (the store's key →
/// value lookup). Anything missing or unusable falls back to its default;
/// if the three keys aren't distinct after that, all three take the
/// defaults.
pub fn load(
    stored: impl Fn(&str) -> Option<Value>,
    platform: Platform,
) -> (AppSettings, LoadReport) {
    let defaults = AppSettings::defaults(platform);
    let mut report = LoadReport::default();
    let mut keys = defaults.keys;
    for (wheel, store_key) in [
        (Wheel::Conversions, KEYS_CONVERSIONS),
        (Wheel::Tools, KEYS_TOOLS),
        (Wheel::Actions, KEYS_ACTIONS),
    ] {
        if let Some(key) = read(&stored, &mut report, store_key, |value| {
            parse_key(value, platform)
        }) {
            keys = wheel.set_in(keys, key);
        }
    }
    if !distinct(keys) {
        report.warnings.push(format!(
            "{KEYS_CONVERSIONS}, {KEYS_TOOLS} and {KEYS_ACTIONS} give two wheels the same key; using the defaults for all three"
        ));
        keys = defaults.keys;
    }
    let output = read(&stored, &mut report, OUTPUT_LOCATION, parse_output);
    let fallback_dir = read(&stored, &mut report, OUTPUT_FALLBACK_DIR, parse_folder);
    let mut flag = |store_key| read(&stored, &mut report, store_key, parse_bool);
    let (sounds, haptics, launch_at_login) = (flag(SOUNDS), flag(HAPTICS), flag(LAUNCH_AT_LOGIN));
    let settings = AppSettings {
        keys,
        output: output.unwrap_or(defaults.output),
        fallback_dir,
        sounds: sounds.unwrap_or(defaults.sounds),
        haptics: haptics.unwrap_or(defaults.haptics),
        launch_at_login,
    };
    (settings, report)
}

/// The value stored under `key`, if there is one and `parse` takes it.
/// Notes a missing key or a warning in `report`.
fn read<T>(
    stored: &impl Fn(&str) -> Option<Value>,
    report: &mut LoadReport,
    key: &'static str,
    parse: impl FnOnce(&Value) -> Result<T, &'static str>,
) -> Option<T> {
    let Some(value) = stored(key) else {
        report.missing.push(key);
        return None;
    };
    match parse(&value) {
        Ok(parsed) => Some(parsed),
        Err(why) => {
            report
                .warnings
                .push(format!("{key} {why}; using the default"));
            None
        }
    }
}

fn parse_bool(value: &Value) -> Result<bool, &'static str> {
    value.as_bool().ok_or("isn't true or false")
}

fn parse_key(value: &Value, platform: Platform) -> Result<Key, &'static str> {
    let key = value
        .as_str()
        .and_then(key_from_id)
        .ok_or("isn't a key Grape knows")?;
    if offered_keys(platform).contains(&key) {
        Ok(key)
    } else {
        Err("names a key that can't open a wheel on this OS")
    }
}

fn parse_output(value: &Value) -> Result<OutputLocation, &'static str> {
    if value.as_str() == Some(NEXT_TO_SOURCE) {
        return Ok(OutputLocation::NextToSource);
    }
    parse_folder(value).map(OutputLocation::Folder)
}

fn parse_folder(value: &Value) -> Result<PathBuf, &'static str> {
    let path = Path::new(value.as_str().ok_or("isn't a folder path")?);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err("isn't an absolute folder path")
    }
}

/// A folder as a stored value. Folders come from the picker, which only
/// takes paths that are valid Unicode ([`usable_folder`]), so this never
/// changes one.
fn folder_value(path: &Path) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

/// What to write to the store going from `old` to `new`: `Some` sets the
/// key, `None` removes it. The three keys are written together whenever
/// one changes, so the store never holds half of a swap. Keys that didn't
/// change aren't touched, nor are keys this module doesn't know
/// (`terminal_app`, `features.v2`).
pub fn store_changes(old: &AppSettings, new: &AppSettings) -> Vec<(&'static str, Option<Value>)> {
    let mut changes = Vec::new();
    if old.keys != new.keys {
        for (wheel, store_key) in [
            (Wheel::Conversions, KEYS_CONVERSIONS),
            (Wheel::Tools, KEYS_TOOLS),
            (Wheel::Actions, KEYS_ACTIONS),
        ] {
            let id = key_id(wheel.key_in(new.keys));
            changes.push((store_key, Some(Value::from(id))));
        }
    }
    if old.output != new.output {
        let value = match &new.output {
            OutputLocation::NextToSource => Value::from(NEXT_TO_SOURCE),
            OutputLocation::Folder(folder) => folder_value(folder),
        };
        changes.push((OUTPUT_LOCATION, Some(value)));
    }
    if old.fallback_dir != new.fallback_dir {
        let value = new.fallback_dir.as_deref().map(folder_value);
        changes.push((OUTPUT_FALLBACK_DIR, value));
    }
    for (store_key, was, now) in [
        (SOUNDS, old.sounds, new.sounds),
        (HAPTICS, old.haptics, new.haptics),
    ] {
        if was != now {
            changes.push((store_key, Some(Value::Bool(now))));
        }
    }
    if old.launch_at_login != new.launch_at_login {
        changes.push((LAUNCH_AT_LOGIN, new.launch_at_login.map(Value::Bool)));
    }
    changes
}

/// What planning follows: the output location and the fallback folder
/// (`default_fallback` unless the user chose one). v1 tiers only.
///
/// The engines create the fallback folder when it's missing, and Grape
/// must never create a folder the user chose, so a chosen fallback is used
/// only while it's an existing folder. Otherwise this warns and plans with
/// `default_fallback`, Grape's own folder.
pub fn core_settings(settings: &AppSettings, default_fallback: &Path) -> Settings {
    let fallback = match settings.fallback_dir.as_deref() {
        Some(chosen) if chosen.is_dir() || chosen == default_fallback => chosen.to_path_buf(),
        Some(chosen) => {
            log::warn!(
                "settings: the fallback folder {} isn't there; using the default",
                chosen.display()
            );
            default_fallback.to_path_buf()
        }
        None => default_fallback.to_path_buf(),
    };
    Settings {
        output: settings.output.clone(),
        ..Settings::new(fallback)
    }
}

/// What the Settings window shows (`ui/src/settings/model.ts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    /// Which OS's keys and rows to show.
    pub platform: Platform,
    /// Which key opens which wheel.
    pub keys: KeyMap,
    /// The keys a wheel may use, in menu order, with their names.
    pub key_choices: Vec<KeyChoice>,
    /// Where outputs go.
    pub output: OutputView,
    /// The fallback folder.
    pub fallback_dir: FallbackView,
    /// The highlight tick.
    pub sounds: bool,
    /// The trackpad tick, or `None` on Windows (no haptics there).
    pub haptics: Option<bool>,
    /// Whether Grape is registered to launch at login, or `None` when that
    /// can't be read.
    pub launch_at_login: Option<bool>,
    /// What a key swap moved, right after one.
    pub note: Option<String>,
}

/// A key a wheel may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct KeyChoice {
    /// Its id.
    pub key: Key,
    /// Its name on this OS, e.g. "Option".
    pub name: &'static str,
}

/// Where outputs go, for the view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum OutputView {
    /// In each source's folder.
    NextToSource,
    /// In one folder.
    Folder {
        /// The folder, for display.
        path: String,
    },
}

/// The fallback folder, for the view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FallbackView {
    /// The folder, for display.
    pub path: String,
    /// `true` when it's the default (`Downloads/Grape`).
    pub is_default: bool,
}

/// The view of `settings` on `platform`. `launch_at_login` is the
/// autostart plugin's real state; `note` is set right after a key swap.
pub fn view(
    settings: &AppSettings,
    platform: Platform,
    default_fallback: &Path,
    launch_at_login: Option<bool>,
    note: Option<String>,
) -> SettingsView {
    let output = match &settings.output {
        OutputLocation::NextToSource => OutputView::NextToSource,
        OutputLocation::Folder(folder) => OutputView::Folder {
            path: folder.display().to_string(),
        },
    };
    let fallback = settings.fallback_dir.as_deref().unwrap_or(default_fallback);
    SettingsView {
        platform,
        keys: settings.keys,
        key_choices: offered_keys(platform)
            .iter()
            .map(|&key| KeyChoice {
                key,
                name: key.name_on(platform),
            })
            .collect(),
        output,
        fallback_dir: FallbackView {
            path: fallback.display().to_string(),
            is_default: fallback == default_fallback,
        },
        sounds: settings.sounds,
        haptics: matches!(platform, Platform::MacOs).then_some(settings.haptics),
        launch_at_login,
        note,
    }
}

/// `true` for a folder the settings can hold: an absolute path to an
/// existing folder whose name is valid Unicode (the store keeps text).
fn usable_folder(path: &Path) -> bool {
    path.is_absolute() && path.to_str().is_some() && path.is_dir()
}

/// `Downloads/Grape`. It's created only when a job actually uses it.
fn default_fallback_dir(app: &AppHandle) -> PathBuf {
    match app.path().download_dir() {
        Ok(downloads) => downloads.join(FALLBACK_FOLDER),
        Err(err) => {
            log::warn!("settings: no Downloads folder ({err}); the fallback is in the temp folder");
            std::env::temp_dir().join(FALLBACK_FOLDER)
        }
    }
}

/// The settings, managed by Tauri.
pub struct SettingsState {
    settings: Mutex<AppSettings>,
    /// `None` when the store couldn't be opened: changes still apply, but
    /// saving them fails.
    store: Option<Arc<Store<Wry>>>,
    /// `Downloads/Grape`, worked out once.
    default_fallback: PathBuf,
    /// `haptics`, for the wheel to read on every highlight.
    haptics: Arc<AtomicBool>,
    /// Set while a folder picker is open, so a second one doesn't stack.
    picking: AtomicBool,
}

impl SettingsState {
    /// Loads the settings from the store (call in setup, before the wheel
    /// starts) and, on a release build's first launch, turns launch at
    /// login on. Never fails: without a store Grape runs on the defaults.
    ///
    /// The store ignores a file it can't load and writes every store over
    /// its file when the app exits, so a `settings.json` that's there but
    /// unreadable is moved aside first, and if that fails there's no store
    /// at all.
    pub fn load(app: &AppHandle) -> Self {
        let default_fallback = default_fallback_dir(app);
        let file = match tauri_plugin_store::resolve_store_path(app, SETTINGS_FILE) {
            Ok(path) => settle_settings_file(&path, |from, to| fs::rename(from, to)),
            Err(err) => {
                log::error!("settings: can't tell where {SETTINGS_FILE} is ({err})");
                SettingsFile::Unknown
            }
        };
        let store = if file.store_allowed() {
            match app.store_builder(SETTINGS_FILE).disable_auto_save().build() {
                Ok(store) => Some(store),
                Err(err) => {
                    log::error!(
                        "settings: could not open {SETTINGS_FILE}: {err}; using the defaults, and changes won't be saved"
                    );
                    None
                }
            }
        } else {
            log::error!(
                "settings: no store this time; using the defaults, and changes won't be saved"
            );
            None
        };
        let (settings, report) = load(
            |key| store.as_ref().and_then(|store| store.get(key)),
            THIS_OS,
        );
        for line in &report.warnings {
            log::warn!("settings: {line}");
        }
        if !report.missing.is_empty() {
            log::info!(
                "settings: nothing stored for {} yet; using the defaults",
                report.missing.join(", ")
            );
        }
        let exe = std::env::current_exe();
        let facts = LaunchFacts {
            release: !cfg!(debug_assertions),
            platform: THIS_OS,
            missing: report.missing.contains(&LAUNCH_AT_LOGIN),
            has_store: store.is_some(),
            unreadable: file.was_unreadable(),
            exe: exe.as_deref().ok(),
        };
        let state = Self {
            haptics: Arc::new(AtomicBool::new(settings.haptics)),
            settings: Mutex::new(settings),
            store,
            default_fallback,
            picking: AtomicBool::new(false),
        };
        state.first_launch_at_login(app, &facts);
        state
    }

    /// The controller's settings: this OS's, with the user's keys and
    /// sounds.
    pub fn controller_config(&self) -> ControllerConfig {
        let settings = self.lock();
        ControllerConfig {
            keys: settings.keys,
            sounds: settings.sounds,
            ..ControllerConfig::for_this_os()
        }
    }

    /// The haptics flag the wheel reads on every highlight.
    pub fn haptics(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.haptics)
    }

    fn lock(&self) -> MutexGuard<'_, AppSettings> {
        // Every change replaces the whole value, so a panic in an earlier
        // holder can't leave it half-changed: keep using it.
        self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn view(&self, app: &AppHandle, note: Option<String>) -> SettingsView {
        let settings = self.lock().clone();
        view(
            &settings,
            THIS_OS,
            &self.default_fallback,
            launch_state(app),
            note,
        )
    }

    /// Applies `change` to the settings: the new value takes effect at once
    /// (the wheel, the next job), what changed is saved, and the window
    /// gets the new view. If saving fails the change still applies, and the
    /// window is told.
    fn change(
        &self,
        app: &AppHandle,
        change: impl FnOnce(&AppSettings) -> Result<(AppSettings, Option<String>), &'static str>,
    ) -> Result<SettingsView, String> {
        let mut settings = self.lock();
        let (new, note) = change(&settings)?;
        let changes = store_changes(&settings, &new);
        let wheel_changed = settings.keys != new.keys || settings.sounds != new.sounds;
        *settings = new;
        self.haptics.store(settings.haptics, Ordering::Release);
        if wheel_changed {
            if let Some(inputs) = app.try_state::<WheelInputs>() {
                inputs.send(Input::Configure {
                    keys: settings.keys,
                    sounds: settings.sounds,
                });
            }
        }
        let saved = self.save(&changes);
        let view = view(
            &settings,
            THIS_OS,
            &self.default_fallback,
            launch_state(app),
            note,
        );
        drop(settings);
        saved.map(|()| view).map_err(str::to_owned)
    }

    /// Writes `changes` to the store and saves it. Call with the settings
    /// locked, so saves land in the order the changes were made.
    fn save(&self, changes: &[(&'static str, Option<Value>)]) -> Result<(), &'static str> {
        if changes.is_empty() {
            return Ok(());
        }
        let keys: Vec<&str> = changes.iter().map(|(key, _)| *key).collect();
        let Some(store) = &self.store else {
            log::error!(
                "settings: {} changed but there's no store to save to",
                keys.join(", ")
            );
            return Err(SAVE_FAILED);
        };
        for (key, value) in changes {
            match value {
                Some(value) => store.set(*key, value.clone()),
                None => {
                    store.delete(key);
                }
            }
        }
        match store.save() {
            Ok(()) => {
                log::info!("settings: saved {}", keys.join(", "));
                Ok(())
            }
            Err(err) => {
                log::error!("settings: could not save {}: {err}", keys.join(", "));
                Err(SAVE_FAILED)
            }
        }
    }

    /// ARCHITECTURE's default for launch at login is on: when
    /// [`first_launch_rule`] says so, this turns it on and stores `true`.
    /// If that fails, nothing is stored and the next launch tries again.
    /// Otherwise autostart is left as it is, and the log says why.
    fn first_launch_at_login(&self, app: &AppHandle, facts: &LaunchFacts<'_>) {
        if let FirstLaunch::Leave(why) = first_launch_rule(facts) {
            log::info!("settings: leaving launch at login as it is: {why}");
            if facts.unreadable && facts.has_store {
                self.keep_launch_state(app);
            }
            return;
        }
        let mut settings = self.lock();
        if let Err(err) = set_autostart(app, true) {
            log::warn!("settings: could not turn launch at login on at the first launch: {err}");
            return;
        }
        log::info!("settings: first launch; launch at login is on");
        let old = settings.clone();
        settings.launch_at_login = Some(true);
        // A failed save is logged there; the next launch tries again.
        let _ = self.save(&store_changes(&old, &settings));
    }
}

impl SettingsState {
    /// After an unreadable `settings.json` was moved aside the store starts
    /// empty, so the next launch would read launch at login as never chosen
    /// and turn it on over the user's "off". Storing what autostart really
    /// is now keeps their choice. If that can't be read, "off" is stored:
    /// the stored value only stops the first-launch rule, and Settings
    /// always shows the real state.
    fn keep_launch_state(&self, app: &AppHandle) {
        let real = launch_state(app).unwrap_or_else(|| {
            log::warn!("settings: can't tell whether launch at login is on; storing off");
            false
        });
        let mut settings = self.lock();
        let old = settings.clone();
        settings.launch_at_login = Some(real);
        // A failed save is logged there.
        let _ = self.save(&store_changes(&old, &settings));
        log::info!(
            "settings: kept launch at login {} after moving the old file aside",
            if real { "on" } else { "off" }
        );
    }
}

/// What [`settle_settings_file`] found at `settings.json`, before the store
/// opens it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SettingsFile {
    /// Nothing there yet.
    Missing,
    /// A JSON object, which the store loads.
    Usable,
    /// It couldn't be read or parsed, so it was renamed to this path in
    /// the same folder. The store starts empty, and its next save writes a
    /// fresh file.
    MovedAside(PathBuf),
    /// It couldn't be read or parsed, nor renamed. There must be no store,
    /// so that nothing writes over it.
    Stuck,
    /// Where it is couldn't be worked out, so no store can open either.
    Unknown,
}

impl SettingsFile {
    /// `false` when a store could write over a file it couldn't load.
    fn store_allowed(&self) -> bool {
        !matches!(self, SettingsFile::Stuck | SettingsFile::Unknown)
    }

    /// `true` when the file was there but couldn't be read or parsed.
    fn was_unreadable(&self) -> bool {
        matches!(self, SettingsFile::MovedAside(_) | SettingsFile::Stuck)
    }
}

/// Checks the settings file at `path` before the store opens it. The store
/// ignores a file it can't load and saves over it at exit, so a file
/// that's there but can't be read or isn't a JSON object is renamed aside
/// first with `rename` ([`move_aside`]). Logs what happened, naming the
/// kind of problem and where in the file, never its contents.
fn settle_settings_file(
    path: &Path,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> SettingsFile {
    let problem = match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes) {
            Ok(_) => return SettingsFile::Usable,
            Err(err) => parse_problem(&err),
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => return SettingsFile::Missing,
        Err(err) => format!("couldn't be read ({err})"),
    };
    match move_aside(path, rename) {
        Ok(aside) => {
            let name = aside.file_name().unwrap_or_default().to_string_lossy();
            log::warn!(
                "settings: {SETTINGS_FILE} {problem}; renamed it to {name} and using the defaults"
            );
            SettingsFile::MovedAside(aside)
        }
        Err(err) => {
            log::error!(
                "settings: {SETTINGS_FILE} {problem}, and renaming it aside failed ({err}); leaving it as it is"
            );
            SettingsFile::Stuck
        }
    }
}

/// Why the settings file isn't usable: the kind of error and where.
/// serde_json's own message can quote the file (a top-level string, say),
/// so it's never used.
fn parse_problem(err: &serde_json::Error) -> String {
    format!(
        "isn't valid ({:?} error at line {}, column {})",
        err.classify(),
        err.line(),
        err.column()
    )
}

/// Renames `path` to `settings.unreadable.json` in its folder, or to
/// `settings.unreadable 2.json`, ` 3.json` and so on when that's taken, and
/// returns the new path. Never a name that's taken: `rename` replaces an
/// existing file. (Checking and renaming aren't one step, but only one
/// Grape runs, and nothing else writes in its data folder.)
fn move_aside(
    path: &Path,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let folder = path.parent().unwrap_or_else(|| Path::new(""));
    for number in 1..=UNREADABLE_NAMES {
        let name = if number == 1 {
            format!("{UNREADABLE_STEM}.json")
        } else {
            format!("{UNREADABLE_STEM} {number}.json")
        };
        let aside = folder.join(name);
        // `symlink_metadata`, so a link to nowhere takes its name too.
        match fs::symlink_metadata(&aside) {
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                rename(path, &aside)?;
                return Ok(aside);
            }
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("every {UNREADABLE_STEM} name is taken"),
    ))
}

/// What the first-launch rule for launch at login looks at.
#[derive(Debug, Clone, Copy)]
struct LaunchFacts<'a> {
    /// A release build. Debug builds never touch autostart on their own.
    release: bool,
    /// The OS Grape runs on.
    platform: Platform,
    /// Nothing at all is stored for `launch_at_login`. An unusable value
    /// doesn't count: the user may have turned it off.
    missing: bool,
    /// There's a store to remember the choice in.
    has_store: bool,
    /// `settings.json` was there at this launch but couldn't be read or
    /// parsed, so the choice may be in the file that was kept or moved
    /// aside.
    unreadable: bool,
    /// Where Grape runs from (`std::env::current_exe`), if that's known.
    exe: Option<&'a Path>,
}

/// What the first-launch rule decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstLaunch {
    /// Turn launch at login on and store `true`.
    TurnOn,
    /// Leave autostart alone and store nothing, for this reason.
    Leave(&'static str),
}

/// Whether this launch turns launch at login on by itself
/// (ARCHITECTURE's default is on): only in a release build, with nothing
/// stored for it, a store to remember it in, and a `settings.json` that
/// wasn't unreadable this time. On macOS, also not from a disk image
/// (`/Volumes/…`), a translocated copy (`…/AppTranslocation/…`) or an
/// unknown path: the login item would name a path that goes away. The
/// next launch, from Applications, tries again.
fn first_launch_rule(facts: &LaunchFacts<'_>) -> FirstLaunch {
    if !facts.missing {
        return FirstLaunch::Leave("a value is stored for it");
    }
    if !facts.release {
        return FirstLaunch::Leave("a debug build never changes it on its own");
    }
    if !facts.has_store {
        return FirstLaunch::Leave("there's no store to remember it in");
    }
    if facts.unreadable {
        return FirstLaunch::Leave("settings.json was unreadable at this launch");
    }
    if matches!(facts.platform, Platform::MacOs) {
        match facts.exe {
            None => return FirstLaunch::Leave("Grape can't tell where it runs from"),
            Some(exe) if transient_mac_location(exe) => {
                return FirstLaunch::Leave(
                    "Grape runs from a disk image or a translocated copy; the next launch from Applications tries again",
                );
            }
            Some(_) => {}
        }
    }
    FirstLaunch::TurnOn
}

/// `true` for a Mac app path that goes away: a mounted disk image
/// (`/Volumes/…`) or a copy Gatekeeper translocated
/// (`…/AppTranslocation/…`).
fn transient_mac_location(exe: &Path) -> bool {
    exe.starts_with("/Volumes")
        || exe
            .components()
            .any(|part| part.as_os_str() == "AppTranslocation")
}

/// Whether Grape is registered to launch at login, or `None` if the
/// autostart plugin can't tell. (`try_state` is what
/// `ManagerExt::autolaunch` reads, without its panic.)
fn launch_state(app: &AppHandle) -> Option<bool> {
    let manager = app.try_state::<AutoLaunchManager>()?;
    match manager.is_enabled() {
        Ok(on) => Some(on),
        Err(err) => {
            log::warn!("settings: can't tell whether launch at login is on: {err}");
            None
        }
    }
}

/// Registers Grape to launch at login, or removes it.
fn set_autostart(app: &AppHandle, on: bool) -> Result<(), String> {
    let Some(manager) = app.try_state::<AutoLaunchManager>() else {
        return Err("the autostart plugin isn't running".to_owned());
    };
    let result = if on {
        manager.enable()
    } else {
        manager.disable()
    };
    result.map_err(|err| err.to_string())
}

/// A switch in the Settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Toggle {
    /// The highlight tick.
    Sounds,
    /// The trackpad tick (macOS only).
    Haptics,
    /// Launch at login.
    LaunchAtLogin,
}

/// Which folder the picker chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderTarget {
    /// Where outputs go.
    Output,
    /// The fallback folder.
    Fallback,
}

impl FolderTarget {
    const fn picker_title(self) -> &'static str {
        match self {
            FolderTarget::Output => "Choose where Grape saves new files",
            FolderTarget::Fallback => "Choose a folder for when Grape can't save next to a file",
        }
    }
}

/// The settings as they are now.
#[tauri::command]
pub async fn settings_get(
    app: AppHandle,
    state: State<'_, SettingsState>,
) -> Result<SettingsView, String> {
    Ok(state.view(&app, None))
}

/// `key` opens `wheel` from the next drag, swapping with the wheel that
/// had it.
#[tauri::command]
pub async fn settings_set_key(
    app: AppHandle,
    state: State<'_, SettingsState>,
    wheel: Wheel,
    key: Key,
) -> Result<SettingsView, String> {
    state.change(&app, |old| {
        let (keys, note) = set_key(old.keys, wheel, key, THIS_OS)?;
        log::info!(
            "settings: {} opens {} now",
            key.name_on(THIS_OS),
            wheel.title()
        );
        let new = AppSettings {
            keys,
            ..old.clone()
        };
        Ok((new, note))
    })
}

/// Outputs go next to their originals again.
#[tauri::command]
pub async fn settings_set_output_next_to_source(
    app: AppHandle,
    state: State<'_, SettingsState>,
) -> Result<SettingsView, String> {
    state.change(&app, |old| {
        let new = AppSettings {
            output: OutputLocation::NextToSource,
            ..old.clone()
        };
        Ok((new, None))
    })
}

/// The fallback folder goes back to `Downloads/Grape`.
#[tauri::command]
pub async fn settings_use_default_fallback(
    app: AppHandle,
    state: State<'_, SettingsState>,
) -> Result<SettingsView, String> {
    state.change(&app, |old| {
        let new = AppSettings {
            fallback_dir: None,
            ..old.clone()
        };
        Ok((new, None))
    })
}

/// Turns a switch on or off. Launch at login registers or removes Grape
/// first, and changes nothing if that fails.
#[tauri::command]
pub async fn settings_set_toggle(
    app: AppHandle,
    state: State<'_, SettingsState>,
    name: Toggle,
    on: bool,
) -> Result<SettingsView, String> {
    let words = if on { "on" } else { "off" };
    state.change(&app, |old| {
        let mut new = old.clone();
        match name {
            Toggle::Sounds => new.sounds = on,
            Toggle::Haptics if matches!(THIS_OS, Platform::Windows) => return Err(NO_HAPTICS),
            Toggle::Haptics => new.haptics = on,
            Toggle::LaunchAtLogin => {
                if let Err(err) = set_autostart(&app, on) {
                    log::error!("settings: could not turn launch at login {words}: {err}");
                    return Err(LAUNCH_FAILED);
                }
                new.launch_at_login = Some(on);
            }
        }
        log::info!("settings: {name:?} {words}");
        Ok((new, None))
    })
}

/// Opens the folder picker for `target`, at the folder it has now. A
/// picked folder becomes the output folder or the fallback folder;
/// cancelling changes nothing.
///
/// The picker blocks, so it runs on a blocking worker, never the main
/// thread. On Windows it's modal to the Settings window (its HWND is the
/// owner); on macOS the panel runs on the main thread, which the worker
/// waits for.
#[tauri::command]
pub async fn settings_pick_folder(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, SettingsState>,
    target: FolderTarget,
) -> Result<SettingsView, String> {
    if state.picking.swap(true, Ordering::AcqRel) {
        log::info!("settings: a folder picker is already open");
        return Ok(state.view(&app, None));
    }
    let start = {
        let settings = state.lock();
        match target {
            FolderTarget::Output => match &settings.output {
                OutputLocation::Folder(folder) => Some(folder.clone()),
                OutputLocation::NextToSource => None,
            },
            FolderTarget::Fallback => Some(
                settings
                    .fallback_dir
                    .clone()
                    .unwrap_or_else(|| state.default_fallback.clone()),
            ),
        }
    };
    let owner = owner_of(&window);
    let picked = tauri::async_runtime::spawn_blocking(move || {
        file_actions::pick_folder(owner, target.picker_title(), start.as_deref())
    })
    .await;
    state.picking.store(false, Ordering::Release);

    let folder = match picked {
        Ok(Ok(Some(folder))) => folder,
        Ok(Ok(None)) => {
            log::info!("settings: folder picker closed; nothing changed");
            return Ok(state.view(&app, None));
        }
        Ok(Err(err)) => {
            log::error!("settings: could not open the folder picker: {err}");
            return Err(PICKER_FAILED.to_owned());
        }
        Err(err) => {
            log::error!("settings: the folder picker's worker failed: {err}");
            return Err(PICKER_FAILED.to_owned());
        }
    };
    if !usable_folder(&folder) {
        log::warn!(
            "settings: the picker returned {}, which isn't a folder Grape can use",
            folder.display()
        );
        return Err(NOT_A_FOLDER.to_owned());
    }
    log::info!("settings: {target:?} folder is {}", folder.display());
    state.change(&app, |old| {
        let mut new = old.clone();
        match target {
            FolderTarget::Output => new.output = OutputLocation::Folder(folder),
            FolderTarget::Fallback => new.fallback_dir = Some(folder),
        }
        Ok((new, None))
    })
}

/// The window's raw handle, to own the picker on Windows.
#[cfg(target_os = "windows")]
fn owner_of(window: &WebviewWindow) -> Option<isize> {
    match window.hwnd() {
        Ok(hwnd) => Some(hwnd.0 as isize),
        Err(err) => {
            log::warn!("settings: no window handle for the folder picker: {err}");
            None
        }
    }
}

/// Only Windows takes an owner for the picker.
#[cfg(not(target_os = "windows"))]
fn owner_of(_window: &WebviewWindow) -> Option<isize> {
    None
}

/// What the next job plans with: the user's output and fallback folders.
/// Falls back to the defaults if the settings aren't loaded (they always
/// are once setup ran).
///
/// [`core_settings`] checks the fallback folder on disk, which can be slow
/// on a network drive, so call this on a job thread, never the controller's,
/// and the settings are copied out of the lock first.
pub fn planning_settings(app: &AppHandle) -> Settings {
    match app.try_state::<SettingsState>() {
        Some(state) => {
            let settings = state.lock().clone();
            core_settings(&settings, &state.default_fallback)
        }
        None => {
            log::warn!("settings: not loaded; planning with the defaults");
            core_settings(&AppSettings::defaults(THIS_OS), &default_fallback_dir(app))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use serde_json::json;

    fn golden(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn json_of(view: &SettingsView) -> Value {
        serde_json::to_value(view).unwrap()
    }

    /// An absolute folder on the OS running the tests.
    fn abs(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\Users\max\{name}"))
        } else {
            PathBuf::from(format!("/Users/max/{name}"))
        }
    }

    fn abs_str(name: &str) -> String {
        abs(name).to_str().unwrap().to_owned()
    }

    fn from_map(
        map: HashMap<&'static str, Value>,
        platform: Platform,
    ) -> (AppSettings, LoadReport) {
        load(|key| map.get(key).cloned(), platform)
    }

    fn keys(conversions: Key, tools: Key, actions: Key) -> KeyMap {
        KeyMap {
            conversions,
            tools,
            actions,
        }
    }

    #[test]
    fn the_windows_view_matches_the_fixture() {
        let settings = AppSettings::defaults(Platform::Windows);
        let default_fallback = Path::new(r"C:\Users\max\Downloads\Grape");
        let view = view(
            &settings,
            Platform::Windows,
            default_fallback,
            Some(true),
            None,
        );
        assert_eq!(
            json_of(&view),
            golden(include_str!(
                "../../ui/src/shared/fixtures/settings-view-windows.json"
            ))
        );
    }

    #[test]
    fn the_macos_view_matches_the_fixture() {
        // The fixture's keys and note are Command chosen for Conversions on
        // the macOS defaults.
        let (keys, note) = set_key(
            KeyMap::MACOS,
            Wheel::Conversions,
            Key::Meta,
            Platform::MacOs,
        )
        .unwrap();
        let settings = AppSettings {
            keys,
            output: OutputLocation::Folder(PathBuf::from("/Users/max/Pictures/Grape out")),
            fallback_dir: Some(PathBuf::from("/Users/max/Desktop/Fallback")),
            sounds: false,
            haptics: true,
            launch_at_login: Some(false),
        };
        let default_fallback = Path::new("/Users/max/Downloads/Grape");
        let view = view(&settings, Platform::MacOs, default_fallback, None, note);
        assert_eq!(
            json_of(&view),
            golden(include_str!(
                "../../ui/src/shared/fixtures/settings-view-macos.json"
            ))
        );
    }

    #[test]
    fn key_ids_are_the_serde_ids() {
        for key in [Key::Shift, Key::Ctrl, Key::Alt, Key::Meta] {
            assert_eq!(serde_json::to_value(key).unwrap(), json!(key_id(key)));
            assert_eq!(key_from_id(key_id(key)), Some(key));
        }
        assert_eq!(key_from_id("Shift"), None);
        assert_eq!(key_from_id("win"), None);
    }

    #[test]
    fn the_defaults_follow_each_os() {
        let win = AppSettings::defaults(Platform::Windows);
        assert_eq!(win.keys, keys(Key::Shift, Key::Ctrl, Key::Alt));
        let mac = AppSettings::defaults(Platform::MacOs);
        assert_eq!(mac.keys, keys(Key::Shift, Key::Meta, Key::Ctrl));
        for defaults in [win, mac] {
            assert_eq!(defaults.output, OutputLocation::NextToSource);
            assert_eq!(defaults.fallback_dir, None);
            assert!(defaults.sounds && defaults.haptics);
            assert_eq!(defaults.launch_at_login, None);
        }
    }

    #[test]
    fn an_empty_store_loads_the_defaults_quietly() {
        for platform in [Platform::Windows, Platform::MacOs] {
            let (settings, report) = from_map(HashMap::new(), platform);
            assert_eq!(settings, AppSettings::defaults(platform));
            assert!(report.warnings.is_empty(), "{report:?}");
            assert_eq!(
                report.missing,
                [
                    KEYS_CONVERSIONS,
                    KEYS_TOOLS,
                    KEYS_ACTIONS,
                    OUTPUT_LOCATION,
                    OUTPUT_FALLBACK_DIR,
                    SOUNDS,
                    HAPTICS,
                    LAUNCH_AT_LOGIN
                ]
            );
        }
    }

    #[test]
    fn a_full_store_loads_every_value() {
        let map = HashMap::from([
            (KEYS_CONVERSIONS, json!("alt")),
            (KEYS_TOOLS, json!("meta")),
            (KEYS_ACTIONS, json!("shift")),
            (OUTPUT_LOCATION, json!(abs_str("Out"))),
            (OUTPUT_FALLBACK_DIR, json!(abs_str("Fallback"))),
            (SOUNDS, json!(false)),
            (HAPTICS, json!(false)),
            (LAUNCH_AT_LOGIN, json!(false)),
            // Not this slice's: ignored, and never rewritten.
            ("terminal_app", json!("iTerm2")),
            ("features.v2", json!(true)),
        ]);
        let (settings, report) = from_map(map, Platform::MacOs);
        assert_eq!(
            settings,
            AppSettings {
                keys: keys(Key::Alt, Key::Meta, Key::Shift),
                output: OutputLocation::Folder(abs("Out")),
                fallback_dir: Some(abs("Fallback")),
                sounds: false,
                haptics: false,
                launch_at_login: Some(false),
            }
        );
        assert_eq!(report, LoadReport::default());
    }

    #[test]
    fn next_to_source_loads_as_such() {
        let map = HashMap::from([(OUTPUT_LOCATION, json!("next_to_source"))]);
        let (settings, report) = from_map(map, Platform::Windows);
        assert_eq!(settings.output, OutputLocation::NextToSource);
        assert!(report.warnings.is_empty());
    }

    /// Loads one bad value and checks it fell back with one warning that
    /// names its key.
    fn falls_back(store_key: &'static str, value: Value, platform: Platform) -> String {
        let (settings, report) = from_map(HashMap::from([(store_key, value)]), platform);
        assert_eq!(settings, AppSettings::defaults(platform), "{store_key}");
        let [warning] = report.warnings.as_slice() else {
            panic!("{store_key}: {report:?}");
        };
        assert!(warning.starts_with(store_key), "{warning}");
        assert!(!report.missing.contains(&store_key));
        warning.clone()
    }

    #[test]
    fn values_of_the_wrong_type_fall_back() {
        for store_key in [SOUNDS, HAPTICS, LAUNCH_AT_LOGIN] {
            for value in [json!("true"), json!(1), json!(null), json!([true])] {
                falls_back(store_key, value, Platform::Windows);
            }
        }
        for store_key in [KEYS_CONVERSIONS, KEYS_TOOLS, KEYS_ACTIONS] {
            for value in [json!(1), json!(null), json!({"key": "shift"})] {
                falls_back(store_key, value, Platform::MacOs);
            }
        }
        for store_key in [OUTPUT_LOCATION, OUTPUT_FALLBACK_DIR] {
            for value in [json!(true), json!(null), json!(["/a"])] {
                falls_back(store_key, value, Platform::Windows);
            }
        }
    }

    #[test]
    fn unknown_key_names_fall_back() {
        for name in ["Shift", "control", "win", "", "cmd"] {
            let warning = falls_back(KEYS_TOOLS, json!(name), Platform::Windows);
            assert_eq!(
                warning,
                "keys.tools isn't a key Grape knows; using the default"
            );
        }
    }

    #[test]
    fn windows_never_loads_the_windows_key() {
        let warning = falls_back(KEYS_CONVERSIONS, json!("meta"), Platform::Windows);
        assert_eq!(
            warning,
            "keys.conversions names a key that can't open a wheel on this OS; using the default"
        );
    }

    #[test]
    fn a_warning_never_repeats_the_stored_value() {
        let secret = abs_str("Private Folder Name");
        let warning = falls_back(OUTPUT_LOCATION, json!(12), Platform::Windows);
        assert!(!warning.contains(&secret));
        let relative = "Private Folder Name";
        let warning = falls_back(OUTPUT_FALLBACK_DIR, json!(relative), Platform::Windows);
        assert!(!warning.contains(relative), "{warning}");
    }

    #[test]
    fn relative_paths_fall_back() {
        for value in ["Grape out", "./out", "", "out/../in"] {
            let warning = falls_back(OUTPUT_LOCATION, json!(value), Platform::MacOs);
            assert_eq!(
                warning,
                "output.location isn't an absolute folder path; using the default"
            );
            falls_back(OUTPUT_FALLBACK_DIR, json!(value), Platform::MacOs);
        }
    }

    #[test]
    fn keys_that_share_a_wheel_all_reset() {
        let map = HashMap::from([
            (KEYS_CONVERSIONS, json!("ctrl")),
            (KEYS_TOOLS, json!("ctrl")),
            (KEYS_ACTIONS, json!("shift")),
        ]);
        let (settings, report) = from_map(map, Platform::Windows);
        assert_eq!(settings.keys, KeyMap::WINDOWS);
        assert_eq!(
            report.warnings,
            ["keys.conversions, keys.tools and keys.actions give two wheels the same key; using the defaults for all three"]
        );

        // One stored key that matches another wheel's default clashes too.
        let map = HashMap::from([(KEYS_ACTIONS, json!("shift"))]);
        let (settings, report) = from_map(map, Platform::MacOs);
        assert_eq!(settings.keys, KeyMap::MACOS);
        assert_eq!(report.warnings.len(), 1);

        // A bad key's default clashing with a good one: still all reset.
        let map = HashMap::from([
            (KEYS_CONVERSIONS, json!("alt")),
            (KEYS_TOOLS, json!("shift")),
            (KEYS_ACTIONS, json!("meta")),
        ]);
        let (settings, report) = from_map(map, Platform::Windows);
        assert_eq!(settings.keys, KeyMap::WINDOWS);
        assert_eq!(report.warnings.len(), 2, "{report:?}");
    }

    #[test]
    fn one_bad_value_keeps_the_good_ones() {
        let map = HashMap::from([
            (SOUNDS, json!("loud")),
            (HAPTICS, json!(false)),
            (OUTPUT_LOCATION, json!(abs_str("Out"))),
        ]);
        let (settings, report) = from_map(map, Platform::MacOs);
        assert!(settings.sounds);
        assert!(!settings.haptics);
        assert_eq!(settings.output, OutputLocation::Folder(abs("Out")));
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn the_swap_rule_holds_for_every_pair_on_both_oses() {
        for platform in [Platform::Windows, Platform::MacOs] {
            let start = default_keys(platform);
            for wheel in Wheel::ALL {
                for &key in offered_keys(platform) {
                    let (after, note) = set_key(start, wheel, key, platform).unwrap();
                    assert!(distinct(after), "{platform:?} {wheel:?} {key:?}");
                    assert_eq!(wheel.key_in(after), key);
                    let old = wheel.key_in(start);
                    let other = Wheel::ALL
                        .into_iter()
                        .find(|&other| other != wheel && other.key_in(start) == key);
                    match other {
                        Some(other) => {
                            assert_eq!(other.key_in(after), old);
                            assert_eq!(
                                note.as_deref(),
                                Some(
                                    format!(
                                        "{} now opens {}, and {} opens {}.",
                                        key.name_on(platform),
                                        wheel.title(),
                                        old.name_on(platform),
                                        other.title()
                                    )
                                    .as_str()
                                )
                            );
                        }
                        None => {
                            assert_eq!(note, None);
                            // Only this wheel changed.
                            for third in Wheel::ALL.into_iter().filter(|&w| w != wheel) {
                                assert_eq!(third.key_in(after), third.key_in(start));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn swap_notes_name_both_keys_and_wheels() {
        let (after, note) = set_key(
            KeyMap::WINDOWS,
            Wheel::Conversions,
            Key::Ctrl,
            Platform::Windows,
        )
        .unwrap();
        assert_eq!(after, keys(Key::Ctrl, Key::Shift, Key::Alt));
        assert_eq!(
            note.as_deref(),
            Some("Ctrl now opens Conversions, and Shift opens Tools.")
        );

        let (after, note) = set_key(
            KeyMap::WINDOWS,
            Wheel::Actions,
            Key::Shift,
            Platform::Windows,
        )
        .unwrap();
        assert_eq!(after, keys(Key::Alt, Key::Ctrl, Key::Shift));
        assert_eq!(
            note.as_deref(),
            Some("Shift now opens File actions, and Alt opens Conversions.")
        );

        let (after, note) =
            set_key(KeyMap::MACOS, Wheel::Tools, Key::Ctrl, Platform::MacOs).unwrap();
        assert_eq!(after, keys(Key::Shift, Key::Ctrl, Key::Meta));
        assert_eq!(
            note.as_deref(),
            Some("Control now opens Tools, and Command opens File actions.")
        );
    }

    #[test]
    fn a_free_key_on_a_mac_moves_without_a_note() {
        let (after, note) =
            set_key(KeyMap::MACOS, Wheel::Actions, Key::Alt, Platform::MacOs).unwrap();
        assert_eq!(after, keys(Key::Shift, Key::Meta, Key::Alt));
        assert_eq!(note, None);
    }

    #[test]
    fn the_same_key_again_changes_nothing() {
        let (after, note) =
            set_key(KeyMap::WINDOWS, Wheel::Tools, Key::Ctrl, Platform::Windows).unwrap();
        assert_eq!(after, KeyMap::WINDOWS);
        assert_eq!(note, None);
    }

    #[test]
    fn windows_refuses_the_windows_key() {
        for wheel in Wheel::ALL {
            assert_eq!(
                set_key(KeyMap::WINDOWS, wheel, Key::Meta, Platform::Windows),
                Err("That key can't open a wheel here.")
            );
        }
        assert!(set_key(KeyMap::MACOS, Wheel::Tools, Key::Meta, Platform::MacOs).is_ok());
    }

    #[test]
    fn key_choices_follow_the_os() {
        let default_fallback = abs("Downloads");
        let names = |platform| {
            let view = view(
                &AppSettings::defaults(platform),
                platform,
                &default_fallback,
                None,
                None,
            );
            view.key_choices
                .iter()
                .map(|choice| (key_id(choice.key), choice.name))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(Platform::Windows),
            [("shift", "Shift"), ("ctrl", "Ctrl"), ("alt", "Alt")]
        );
        assert_eq!(
            names(Platform::MacOs),
            [
                ("shift", "Shift"),
                ("ctrl", "Control"),
                ("alt", "Option"),
                ("meta", "Command")
            ]
        );
    }

    #[test]
    fn haptics_show_on_macos_only() {
        let settings = AppSettings {
            haptics: false,
            ..AppSettings::defaults(Platform::MacOs)
        };
        let default_fallback = abs("Downloads");
        assert_eq!(
            view(&settings, Platform::MacOs, &default_fallback, None, None).haptics,
            Some(false)
        );
        assert_eq!(
            view(&settings, Platform::Windows, &default_fallback, None, None).haptics,
            None
        );
    }

    #[test]
    fn choosing_the_default_folder_reads_as_the_default() {
        let default_fallback = abs("Downloads");
        let settings = AppSettings {
            fallback_dir: Some(default_fallback.clone()),
            ..AppSettings::defaults(Platform::Windows)
        };
        let view = view(&settings, Platform::Windows, &default_fallback, None, None);
        assert!(view.fallback_dir.is_default);
    }

    #[test]
    fn nothing_changed_writes_nothing() {
        let settings = AppSettings::defaults(Platform::Windows);
        assert!(store_changes(&settings, &settings).is_empty());
    }

    #[test]
    fn a_swap_writes_all_three_keys() {
        let old = AppSettings::defaults(Platform::MacOs);
        let (keys, _) = set_key(old.keys, Wheel::Actions, Key::Alt, Platform::MacOs).unwrap();
        let new = AppSettings {
            keys,
            ..old.clone()
        };
        assert_eq!(
            store_changes(&old, &new),
            [
                (KEYS_CONVERSIONS, Some(json!("shift"))),
                (KEYS_TOOLS, Some(json!("meta"))),
                (KEYS_ACTIONS, Some(json!("alt"))),
            ]
        );
    }

    #[test]
    fn folders_are_written_as_text_and_the_default_is_removed() {
        let old = AppSettings::defaults(Platform::Windows);
        let new = AppSettings {
            output: OutputLocation::Folder(abs("Out")),
            fallback_dir: Some(abs("Fallback")),
            ..old.clone()
        };
        assert_eq!(
            store_changes(&old, &new),
            [
                (OUTPUT_LOCATION, Some(json!(abs_str("Out")))),
                (OUTPUT_FALLBACK_DIR, Some(json!(abs_str("Fallback")))),
            ]
        );
        assert_eq!(
            store_changes(&new, &old),
            [
                (OUTPUT_LOCATION, Some(json!("next_to_source"))),
                (OUTPUT_FALLBACK_DIR, None),
            ]
        );
    }

    #[test]
    fn switches_are_written_one_by_one() {
        let old = AppSettings::defaults(Platform::MacOs);
        let new = AppSettings {
            sounds: false,
            launch_at_login: Some(true),
            ..old.clone()
        };
        assert_eq!(
            store_changes(&old, &new),
            [
                (SOUNDS, Some(json!(false))),
                (LAUNCH_AT_LOGIN, Some(json!(true)))
            ]
        );
    }

    #[test]
    fn what_is_stored_loads_back_the_same() {
        for platform in [Platform::Windows, Platform::MacOs] {
            let old = AppSettings::defaults(platform);
            let (keys, _) = set_key(old.keys, Wheel::Tools, Key::Shift, platform).unwrap();
            let new = AppSettings {
                keys,
                output: OutputLocation::Folder(abs("Grape out")),
                fallback_dir: Some(abs("Fallback")),
                sounds: false,
                haptics: false,
                launch_at_login: Some(true),
            };
            let mut store: HashMap<&'static str, Value> = HashMap::new();
            for (key, value) in store_changes(&old, &new) {
                match value {
                    Some(value) => store.insert(key, value),
                    None => store.remove(key),
                };
            }
            let (loaded, report) = from_map(store, platform);
            assert_eq!(loaded, new);
            assert!(report.warnings.is_empty());
        }
    }

    /// A fresh, empty folder for one test, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("grape-settings-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, bytes).unwrap();
            path
        }

        /// The folder's entries, sorted.
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn planning_uses_the_chosen_folders() {
        let scratch = Scratch::new("planning");
        let default_fallback = scratch.0.join("Downloads").join("Grape");
        let settings = AppSettings::defaults(Platform::Windows);
        let core = core_settings(&settings, &default_fallback);
        assert_eq!(core, Settings::new(default_fallback.clone()));

        let fallback = scratch.0.join("Fallback");
        fs::create_dir(&fallback).unwrap();
        let settings = AppSettings {
            output: OutputLocation::Folder(abs("Out")),
            fallback_dir: Some(fallback.clone()),
            ..settings
        };
        let core = core_settings(&settings, &default_fallback);
        assert_eq!(core.output, OutputLocation::Folder(abs("Out")));
        assert_eq!(core.fallback_dir, fallback);
        assert_eq!(core.features, Settings::new(abs("x")).features);
    }

    #[test]
    fn a_chosen_fallback_that_is_gone_plans_with_the_default() {
        let scratch = Scratch::new("fallback-gone");
        let default_fallback = scratch.0.join("Downloads").join("Grape");
        let gone = scratch.0.join("Fallback");
        let settings = AppSettings {
            output: OutputLocation::Folder(abs("Out")),
            fallback_dir: Some(gone.clone()),
            ..AppSettings::defaults(Platform::Windows)
        };
        let core = core_settings(&settings, &default_fallback);
        assert_eq!(core.fallback_dir, default_fallback);
        assert_eq!(core.output, OutputLocation::Folder(abs("Out")));
        // A file where the folder was isn't a folder either.
        let file = scratch.write("Fallback", b"x");
        assert_eq!(
            core_settings(&settings, &default_fallback).fallback_dir,
            default_fallback
        );
        assert_eq!(file, gone);
        // Nothing was created.
        assert_eq!(scratch.names(), ["Fallback"]);
        assert!(gone.is_file());
    }

    #[test]
    fn choosing_grapes_own_folder_plans_with_it_before_it_exists() {
        let scratch = Scratch::new("fallback-own");
        let default_fallback = scratch.0.join("Downloads").join("Grape");
        let settings = AppSettings {
            fallback_dir: Some(default_fallback.clone()),
            ..AppSettings::defaults(Platform::MacOs)
        };
        assert_eq!(
            core_settings(&settings, &default_fallback).fallback_dir,
            default_fallback
        );
    }

    #[test]
    fn a_missing_settings_file_is_left_alone() {
        let scratch = Scratch::new("file-missing");
        let path = scratch.0.join(SETTINGS_FILE);
        let file = settle_settings_file(&path, |_, _| panic!("no rename"));
        assert_eq!(file, SettingsFile::Missing);
        assert!(file.store_allowed() && !file.was_unreadable());
        assert!(scratch.names().is_empty());
    }

    #[test]
    fn a_usable_settings_file_is_left_alone() {
        let scratch = Scratch::new("file-usable");
        for text in ["{}", r#"{"sounds": false, "keys.tools": 7}"#] {
            let path = scratch.write(SETTINGS_FILE, text.as_bytes());
            let file = settle_settings_file(&path, |_, _| panic!("no rename"));
            assert_eq!(file, SettingsFile::Usable, "{text}");
            assert!(file.store_allowed() && !file.was_unreadable());
            assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
            assert_eq!(scratch.names(), [SETTINGS_FILE]);
        }
    }

    #[test]
    fn an_invalid_settings_file_is_renamed_aside() {
        // Not JSON, cut short, and JSON that isn't an object.
        for (n, text) in [
            "not json",
            r#"{"sounds": tr"#,
            r#""a secret""#,
            "[1]",
            "null",
        ]
        .into_iter()
        .enumerate()
        {
            let scratch = Scratch::new(&format!("file-invalid-{n}"));
            let path = scratch.write(SETTINGS_FILE, text.as_bytes());
            let file = settle_settings_file(&path, |from, to| fs::rename(from, to));
            let aside = scratch.0.join("settings.unreadable.json");
            assert_eq!(file, SettingsFile::MovedAside(aside.clone()), "{text}");
            assert!(file.store_allowed() && file.was_unreadable());
            assert_eq!(fs::read(&aside).unwrap(), text.as_bytes());
            assert_eq!(scratch.names(), ["settings.unreadable.json"]);
        }
    }

    #[test]
    fn renaming_aside_never_takes_a_name_in_use() {
        let scratch = Scratch::new("file-numbered");
        scratch.write("settings.unreadable.json", b"first");
        scratch.write("settings.unreadable 2.json", b"second");
        let path = scratch.write(SETTINGS_FILE, b"third");
        let file = settle_settings_file(&path, |from, to| fs::rename(from, to));
        let aside = scratch.0.join("settings.unreadable 3.json");
        assert_eq!(file, SettingsFile::MovedAside(aside.clone()));
        assert_eq!(fs::read(&aside).unwrap(), b"third");
        assert_eq!(
            fs::read(scratch.0.join("settings.unreadable.json")).unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(scratch.0.join("settings.unreadable 2.json")).unwrap(),
            b"second"
        );
        assert_eq!(
            scratch.names(),
            [
                "settings.unreadable 2.json",
                "settings.unreadable 3.json",
                "settings.unreadable.json"
            ]
        );
    }

    #[test]
    fn a_settings_file_that_cannot_be_read_is_renamed_aside() {
        // A folder in its place can't be read as a file on any OS.
        let scratch = Scratch::new("file-unreadable");
        let path = scratch.0.join(SETTINGS_FILE);
        fs::create_dir(&path).unwrap();
        let file = settle_settings_file(&path, |from, to| fs::rename(from, to));
        let aside = scratch.0.join("settings.unreadable.json");
        assert_eq!(file, SettingsFile::MovedAside(aside.clone()));
        assert!(aside.is_dir());
        assert_eq!(scratch.names(), ["settings.unreadable.json"]);
    }

    #[test]
    fn a_failed_rename_leaves_the_file_and_allows_no_store() {
        let scratch = Scratch::new("file-stuck");
        let path = scratch.write(SETTINGS_FILE, b"not json");
        let file = settle_settings_file(&path, |_, _| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "test"))
        });
        assert_eq!(file, SettingsFile::Stuck);
        assert!(!file.store_allowed() && file.was_unreadable());
        assert_eq!(fs::read(&path).unwrap(), b"not json");
        assert_eq!(scratch.names(), [SETTINGS_FILE]);
        assert!(!SettingsFile::Unknown.store_allowed());
        assert!(!SettingsFile::Unknown.was_unreadable());
    }

    #[test]
    fn a_parse_problem_never_quotes_the_file() {
        let secret = r#""C:\\Users\\max\\Private Folder Name""#;
        let err = serde_json::from_str::<serde_json::Map<String, Value>>(secret).unwrap_err();
        // serde_json's own message does quote it.
        assert!(err.to_string().contains("Private Folder Name"), "{err}");
        let problem = parse_problem(&err);
        assert_eq!(problem, "isn't valid (Data error at line 1, column 37)");
        assert!(!problem.contains("Private"));

        let err = serde_json::from_str::<serde_json::Map<String, Value>>("{\n  \"so").unwrap_err();
        assert_eq!(
            parse_problem(&err),
            "isn't valid (Eof error at line 2, column 5)"
        );
    }

    /// The facts of a release build's first launch on `platform` from a
    /// normal place, which turns launch at login on.
    fn first_launch(platform: Platform, exe: Option<&Path>) -> LaunchFacts<'_> {
        LaunchFacts {
            release: true,
            platform,
            missing: true,
            has_store: true,
            unreadable: false,
            exe,
        }
    }

    fn left(decision: FirstLaunch) -> bool {
        matches!(decision, FirstLaunch::Leave(_))
    }

    #[test]
    fn a_first_launch_turns_launch_at_login_on() {
        let win = Path::new(r"C:\Program Files\Grape\Grape.exe");
        for exe in [Some(win), None] {
            let facts = first_launch(Platform::Windows, exe);
            assert_eq!(first_launch_rule(&facts), FirstLaunch::TurnOn);
        }
        for exe in [
            "/Applications/Grape.app/Contents/MacOS/Grape",
            "/Users/max/Applications/Grape.app/Contents/MacOS/Grape",
            "/Users/max/Volumes/Grape.app/Contents/MacOS/Grape",
        ] {
            let facts = first_launch(Platform::MacOs, Some(Path::new(exe)));
            assert_eq!(first_launch_rule(&facts), FirstLaunch::TurnOn, "{exe}");
        }
    }

    #[test]
    fn launch_at_login_is_left_alone_unless_nothing_is_stored() {
        for platform in [Platform::Windows, Platform::MacOs] {
            let exe = Path::new("/Applications/Grape.app/Contents/MacOS/Grape");
            let base = first_launch(platform, Some(exe));
            for facts in [
                LaunchFacts {
                    missing: false,
                    ..base
                },
                LaunchFacts {
                    release: false,
                    ..base
                },
                LaunchFacts {
                    has_store: false,
                    ..base
                },
                LaunchFacts {
                    unreadable: true,
                    ..base
                },
            ] {
                assert!(left(first_launch_rule(&facts)), "{facts:?}");
            }
        }
    }

    #[test]
    fn an_unusable_stored_value_is_not_a_first_launch() {
        let map = HashMap::from([(LAUNCH_AT_LOGIN, json!("off"))]);
        let (settings, report) = from_map(map, Platform::Windows);
        assert_eq!(settings.launch_at_login, None);
        let facts = LaunchFacts {
            missing: report.missing.contains(&LAUNCH_AT_LOGIN),
            ..first_launch(Platform::Windows, None)
        };
        assert!(left(first_launch_rule(&facts)));

        let (_, report) = from_map(HashMap::new(), Platform::Windows);
        let facts = LaunchFacts {
            missing: report.missing.contains(&LAUNCH_AT_LOGIN),
            ..first_launch(Platform::Windows, None)
        };
        assert_eq!(first_launch_rule(&facts), FirstLaunch::TurnOn);
    }

    #[test]
    fn a_mac_leaves_launch_at_login_alone_from_a_place_that_goes_away() {
        for exe in [
            "/Volumes/Grape/Grape.app/Contents/MacOS/Grape",
            "/Volumes/Backup/Applications/Grape.app/Contents/MacOS/Grape",
            "/private/var/folders/xy/abc/T/AppTranslocation/1A2B/d/Grape.app/Contents/MacOS/Grape",
            "/var/folders/xy/abc/T/AppTranslocation/1A2B/d/Grape.app/Contents/MacOS/Grape",
        ] {
            let facts = first_launch(Platform::MacOs, Some(Path::new(exe)));
            assert!(left(first_launch_rule(&facts)), "{exe}");
            // Windows doesn't look at the path.
            let facts = first_launch(Platform::Windows, Some(Path::new(exe)));
            assert_eq!(first_launch_rule(&facts), FirstLaunch::TurnOn, "{exe}");
        }
        let facts = first_launch(Platform::MacOs, None);
        assert!(left(first_launch_rule(&facts)));
    }

    #[test]
    fn only_existing_absolute_folders_are_usable() {
        let temp = std::env::temp_dir();
        assert!(usable_folder(&temp));
        assert!(!usable_folder(Path::new("relative")));
        assert!(!usable_folder(
            &temp.join("grape-settings-test-no-such-folder")
        ));
        let file = temp.join(format!("grape-settings-test-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        assert!(!usable_folder(&file));
        std::fs::remove_file(&file).unwrap();
    }
}
