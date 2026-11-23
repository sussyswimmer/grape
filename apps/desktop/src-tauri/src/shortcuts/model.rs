//! The Shortcuts wheel's model (`docs/design/m10-shortcuts-wheel.md` §7 and
//! §8). Pure: no Tauri, no disk, no clock and no OS calls.
//!
//! - A [`Config`] holds the menus. A [`Menu`] is for every app or for one;
//!   it holds [`Item`]s, each a shortcut with [`Step`]s or a group of
//!   shortcuts (two levels, never more).
//! - Ids are made here and never stored: [`load`] numbers everything from
//!   1, and each edit takes the next number. The page works by id.
//! - Every edit is a function on a `&mut Config` that answers with a short
//!   sentence when it refuses, and then leaves the config as it was.
//! - [`load`] is total. A file it can't use at all is reported as one to
//!   move aside; a part it can't use is left out and counted.
//! - [`Snapshot`] is the copy the wheel reads, [`menu_for`] picks its ring
//!   and [`ShortcutRun`] is what a pick hands to the runner.
//! - [`view`] is what Settings shows: names only, never a path or an app's
//!   id.
//!
//! Lengths are UTF-16 code units, as the page's `maxlength` counts them.
//!
//! Names, text, scripts, links, paths and app ids are the user's. Every
//! type here that holds one has a hand-written `Debug` that prints kinds
//! and counts only, and no refusal or report repeats what was typed.

use std::fmt;
use std::path::{Path, PathBuf};

use grape_core::Platform;
use grape_platform::keys::{Chord, HotkeyProblem, KeyCode};
use grape_platform::shortcuts::{
    FrontApp, InputAccess, LocalTime, PickKind, Picked, Placement, SystemControl,
};
use grape_platform::{DisplayBounds, Modifiers};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::events::{UiIcon, GLYPHS};

/// Shortcuts in one ring, and in one group.
pub const MAX_ITEMS: usize = 12;
/// Steps in one shortcut.
pub const MAX_STEPS: usize = 16;
/// A shortcut's or group's name, trimmed.
pub const MAX_NAME: usize = 24;
/// An emoji icon's text.
pub const MAX_ICON: usize = 16;
/// A text field: typed text, a notice, speech, and the name of an Apple
/// Shortcut or a Keyboard Maestro macro.
pub const MAX_TEXT: usize = 4_000;
/// A script.
pub const MAX_SCRIPT: usize = 20_000;
/// A link.
pub const MAX_URL: usize = 2_048;
/// The shortest delay, in milliseconds.
pub const MIN_DELAY_MS: u32 = 10;
/// The longest delay, in milliseconds.
pub const MAX_DELAY_MS: u32 = 60_000;
/// Menus in all, Everywhere among them.
pub const MAX_MENUS: usize = 40;
/// The largest file Grape writes, in bytes.
pub const MAX_FILE_BYTES: usize = 1 << 20;
/// The largest file Grape reads, in bytes.
pub const MAX_LOAD_BYTES: usize = 2 << 20;
/// An app's id or display name.
const MAX_APP_FIELD: usize = 255;
/// A picked path.
const MAX_PATH: usize = 4_096;
/// The file format this build reads and writes.
const VERSION: u64 = 1;

/// The hot key a fresh config has: Control+Option+G.
pub const DEFAULT_HOTKEY: Chord = Chord {
    key: KeyCode::KeyG,
    mods: Modifiers {
        shift: false,
        ctrl: true,
        alt: true,
        meta: false,
    },
};

/// The name of the group an app's menu gets for the Everywhere menu.
pub const EVERYWHERE_NAME: &str = "Everywhere";
/// That group's glyph.
const EVERYWHERE_GLYPH: &str = "stack";
const NEW_SHORTCUT: &str = "New shortcut";
const NEW_GROUP: &str = "New group";

// What a refused change says. One sentence each, and none repeats what the
// user typed.
/// Every changing command says this where the wheel isn't built yet.
pub const UNAVAILABLE: &str = "The Shortcuts wheel is on the Mac first.";
/// A change that would make the file too large.
pub const TOO_BIG: &str = "That's more than Grape can save. Remove something first.";
/// A save that failed. The change still applied.
pub const SAVE_FAILED: &str = "Couldn't save your shortcuts.";
/// The view's note after a load that left parts of the file out.
pub const LEFT_OUT: &str = "Some shortcuts couldn't be read and were left out.";
/// The view's note after a load that couldn't use the file at all.
pub const STARTED_FRESH: &str =
    "Grape couldn't read your shortcuts and started fresh. The old file is kept.";
/// The view's note when an app's menu is too full for the Everywhere group.
pub const EVERYWHERE_DID_NOT_FIT: &str = "Everywhere didn't fit in this menu.";
const NO_HOTKEY: &str = "Choose a key combination for the wheel first.";
const UNKNOWN_KEY: &str = "Grape doesn't know that key.";
const HOTKEY_NEEDS_MODIFIER_MAC: &str = "Add Control, Option or Command, or use a function key.";
const HOTKEY_NEEDS_MODIFIER: &str = "Add Ctrl, Alt or Win, or use a function key.";
const HOTKEY_RESERVED: &str = "Every app already uses that one. Try another.";
const HOTKEY_IN_STEP: &str = "One of your shortcuts presses that. Try another.";
const MENU_GONE: &str = "That menu isn't there anymore.";
const ITEM_GONE: &str = "That shortcut isn't there anymore.";
const GROUP_GONE: &str = "That group isn't there anymore.";
const STEP_GONE: &str = "That step isn't there anymore.";
const APP_HAS_MENU: &str = "That app already has a menu.";
const APP_IS_GRAPE: &str = "Grape can't have a menu of its own.";
/// An app Grape can't tell apart (it has no id), or whose id or name it
/// can't keep.
pub const APP_UNUSABLE: &str = "Grape can't make a menu for that app.";
const MENUS_FULL: &str = "Grape keeps 40 menus at most.";
const EVERYWHERE_STAYS: &str = "The Everywhere menu can't be removed.";
const EVERYWHERE_SWITCH: &str = "Only an app's menu can add Everywhere.";
const RING_FULL: &str = "A ring holds 12 shortcuts at most.";
const GROUP_IN_GROUP: &str = "A group can't hold another group.";
const NAME_LENGTH: &str = "A name needs 1 to 24 characters.";
const NAME_CONTROLS: &str = "A name can't have line breaks.";
const ICON_EMOJI: &str = "An icon is 1 to 16 characters with no spaces.";
const ICON_GLYPH: &str = "Grape doesn't have that icon.";
const CANT_MOVE: &str = "It can't move there.";
const GROUP_NO_STEPS: &str = "A group has no steps of its own.";
const STEPS_FULL: &str = "A shortcut holds 16 steps at most.";
const KIND_MISMATCH: &str = "That step is another kind now.";
const KEYS_IS_HOTKEY: &str = "That one opens the wheel. Pick another.";
const TEXT_TOO_LONG: &str = "That's too long. Text can be 4,000 characters.";
const TEXT_CONTROLS: &str = "Grape can't keep some of those characters.";
const SCRIPT_TOO_LONG: &str = "That's too long. A script can be 20,000 characters.";
const URL_TOO_LONG: &str = "That's too long. A link can be 2,048 characters.";
const URL_SHAPE: &str = "A link starts with its kind, like https: or mailto:, and has no spaces.";
const URL_SCHEME: &str = "Grape doesn't open that kind of link.";
const TARGET_NAME: &str = "That name can't start with a dash or have line breaks.";
const DELAY_RANGE: &str = "A delay is 10 milliseconds to 60 seconds.";
const TARGET_UNUSABLE: &str = "Grape can't use that one.";

/// What names a menu, a shortcut or a step in the view and the commands.
pub type Id = u64;

/// `true` where the Shortcuts wheel is built: the Mac, for now.
pub const fn available(platform: Platform) -> bool {
    matches!(platform, Platform::MacOs)
}

/// A length in UTF-16 code units.
fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `true` for a character no one-line field may hold: a control character
/// or a Unicode line or paragraph separator.
fn breaks_a_line(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

/// Where the wheel opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OpensAt {
    /// Under the cursor.
    Cursor,
    /// In the middle of the display the cursor is on.
    Center,
}

/// A chord as the page and the file write it: the key's id and four flags.
/// `Debug` prints nothing of it: the page chose the key's text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChordWire {
    /// A [`KeyCode`] id, like `"KeyG"`.
    pub key: String,
    /// Shift.
    pub shift: bool,
    /// Control.
    pub ctrl: bool,
    /// Option, or Alt.
    pub alt: bool,
    /// Command, or the Windows key.
    pub meta: bool,
}

impl ChordWire {
    /// The chord, or `None` when the key's id names no key.
    pub fn chord(&self) -> Option<Chord> {
        Some(Chord {
            key: KeyCode::from_id(&self.key)?,
            mods: Modifiers {
                shift: self.shift,
                ctrl: self.ctrl,
                alt: self.alt,
                meta: self.meta,
            },
        })
    }
}

impl From<Chord> for ChordWire {
    fn from(chord: Chord) -> Self {
        Self {
            key: chord.key.id().to_owned(),
            shift: chord.mods.shift,
            ctrl: chord.mods.ctrl,
            alt: chord.mods.alt,
            meta: chord.mods.meta,
        }
    }
}

impl fmt::Debug for ChordWire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChordWire").finish_non_exhaustive()
    }
}

/// A shortcut's icon, written the same way in the file, in the view and in
/// `shortcuts_set_item`. `Debug` prints the kind only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", from = "IconWire")]
pub enum Icon {
    /// No icon: the name alone.
    None,
    /// Text drawn as the icon, usually one emoji.
    Emoji {
        /// The text: 1 to [`MAX_ICON`] units, no whitespace.
        value: String,
    },
    /// One of Grape's own drawings.
    Glyph {
        /// An id from [`GLYPHS`].
        value: String,
    },
}

/// How an [`Icon`] is read. serde lets a variant with no fields carry
/// unknown ones, so "none" is written here with an empty set of them.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum IconWire {
    None {},
    Emoji { value: String },
    Glyph { value: String },
}

impl From<IconWire> for Icon {
    fn from(wire: IconWire) -> Self {
        match wire {
            IconWire::None {} => Icon::None,
            IconWire::Emoji { value } => Icon::Emoji { value },
            IconWire::Glyph { value } => Icon::Glyph { value },
        }
    }
}

impl Icon {
    /// One of Grape's drawings, by id.
    pub fn glyph(id: &str) -> Self {
        Icon::Glyph {
            value: id.to_owned(),
        }
    }

    /// The icon as the overlay takes it.
    pub fn to_ui(&self) -> UiIcon {
        match self {
            Icon::None => UiIcon::None,
            Icon::Emoji { value } => UiIcon::Emoji {
                value: value.clone(),
            },
            Icon::Glyph { value } => UiIcon::Glyph {
                value: value.clone(),
            },
        }
    }
}

impl fmt::Debug for Icon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Icon::None => "None",
            Icon::Emoji { .. } => "Emoji",
            Icon::Glyph { .. } => "Glyph",
        })
    }
}

/// What a Clipboard step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClipboardAction {
    /// Copies the selection.
    Copy,
    /// Pastes.
    Paste,
    /// Empties the clipboard.
    Clear,
}

/// Where a Window step puts the window: [`Placement`], with the ids the
/// file and the page use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlacementId {
    /// The left half.
    LeftHalf,
    /// The right half.
    RightHalf,
    /// The top half.
    TopHalf,
    /// The bottom half.
    BottomHalf,
    /// The whole work area.
    Fill,
    /// The middle, at the window's own size.
    Center,
    /// The top-left quarter.
    TopLeft,
    /// The top-right quarter.
    TopRight,
    /// The bottom-left quarter.
    BottomLeft,
    /// The bottom-right quarter.
    BottomRight,
}

impl From<PlacementId> for Placement {
    fn from(id: PlacementId) -> Self {
        match id {
            PlacementId::LeftHalf => Placement::LeftHalf,
            PlacementId::RightHalf => Placement::RightHalf,
            PlacementId::TopHalf => Placement::TopHalf,
            PlacementId::BottomHalf => Placement::BottomHalf,
            PlacementId::Fill => Placement::Fill,
            PlacementId::Center => Placement::Center,
            PlacementId::TopLeft => Placement::TopLeft,
            PlacementId::TopRight => Placement::TopRight,
            PlacementId::BottomLeft => Placement::BottomLeft,
            PlacementId::BottomRight => Placement::BottomRight,
        }
    }
}

/// What a System step does: [`SystemControl`], with the ids the file and
/// the page use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlId {
    /// Turns the displays off.
    SleepDisplay,
    /// Locks the screen.
    LockScreen,
    /// Starts the screen saver.
    Screensaver,
    /// Shows every window.
    MissionControl,
    /// Mutes the sound, or brings it back.
    ToggleMute,
    /// Turns the sound up a step.
    VolumeUp,
    /// Turns the sound down a step.
    VolumeDown,
    /// Switches between the light and the dark appearance.
    ToggleDarkMode,
    /// Starts a screenshot of an area.
    ScreenshotArea,
}

impl From<ControlId> for SystemControl {
    fn from(id: ControlId) -> Self {
        match id {
            ControlId::SleepDisplay => SystemControl::SleepDisplay,
            ControlId::LockScreen => SystemControl::LockScreen,
            ControlId::Screensaver => SystemControl::Screensaver,
            ControlId::MissionControl => SystemControl::MissionControl,
            ControlId::ToggleMute => SystemControl::ToggleMute,
            ControlId::VolumeUp => SystemControl::VolumeUp,
            ControlId::VolumeDown => SystemControl::VolumeDown,
            ControlId::ToggleDarkMode => SystemControl::ToggleDarkMode,
            ControlId::ScreenshotArea => SystemControl::ScreenshotArea,
        }
    }
}

/// A step's kind, by the id the file and the page use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepKindId {
    /// Presses a key combination.
    Keys,
    /// Types text.
    TypeText,
    /// Copies, pastes or empties the clipboard.
    Clipboard,
    /// Opens an app.
    OpenApp,
    /// Opens a file or a folder.
    OpenFile,
    /// Opens a link.
    OpenUrl,
    /// Runs an Apple Shortcut by name.
    AppleShortcut,
    /// Runs AppleScript.
    AppleScript,
    /// Runs a shell script.
    Shell,
    /// Runs a Keyboard Maestro macro by name.
    KeyboardMaestro,
    /// Moves the window in front.
    Window,
    /// Something the system does.
    System,
    /// Shows a notice.
    Notify,
    /// Speaks text.
    Speak,
    /// Waits.
    Delay,
}

impl StepKindId {
    /// Every kind, in the order Settings offers them.
    pub const ALL: [StepKindId; 15] = [
        StepKindId::Keys,
        StepKindId::TypeText,
        StepKindId::Clipboard,
        StepKindId::OpenApp,
        StepKindId::OpenFile,
        StepKindId::OpenUrl,
        StepKindId::AppleShortcut,
        StepKindId::AppleScript,
        StepKindId::Shell,
        StepKindId::KeyboardMaestro,
        StepKindId::Window,
        StepKindId::System,
        StepKindId::Notify,
        StepKindId::Speak,
        StepKindId::Delay,
    ];

    /// The id, as serde writes it. For the log.
    pub const fn id(self) -> &'static str {
        match self {
            StepKindId::Keys => "keys",
            StepKindId::TypeText => "typeText",
            StepKindId::Clipboard => "clipboard",
            StepKindId::OpenApp => "openApp",
            StepKindId::OpenFile => "openFile",
            StepKindId::OpenUrl => "openUrl",
            StepKindId::AppleShortcut => "appleShortcut",
            StepKindId::AppleScript => "appleScript",
            StepKindId::Shell => "shell",
            StepKindId::KeyboardMaestro => "keyboardMaestro",
            StepKindId::Window => "window",
            StepKindId::System => "system",
            StepKindId::Notify => "notify",
            StepKindId::Speak => "speak",
            StepKindId::Delay => "delay",
        }
    }
}

/// What a step does. Every value here passed the checks of
/// [`set_step`]. `Debug` prints the kind only.
#[derive(Clone, PartialEq, Eq)]
pub enum StepKind {
    /// Presses a chord. `None` until one is chosen.
    Keys {
        /// The chord, never the wheel's own hot key.
        chord: Option<Chord>,
    },
    /// Types text, with `{date}`, `{time}` and `{clipboard}` filled in
    /// ([`expand_text`]).
    TypeText {
        /// The text. `\n` and `\t` are its only control characters.
        text: String,
    },
    /// Copies, pastes or empties the clipboard.
    Clipboard {
        /// Which.
        action: ClipboardAction,
    },
    /// Opens an app. All three are `None` until one is picked.
    OpenApp {
        /// Where the app is.
        path: Option<PathBuf>,
        /// Its id, when it has one.
        bundle_id: Option<String>,
        /// Its display name.
        name: Option<String>,
    },
    /// Opens a file or a folder. `None` until one is picked.
    OpenFile {
        /// Where it is.
        path: Option<PathBuf>,
        /// `true` when a folder was picked.
        folder: bool,
    },
    /// Opens a link. Empty until one is typed.
    OpenUrl {
        /// The link.
        url: String,
    },
    /// Runs an Apple Shortcut. Empty until a name is typed.
    AppleShortcut {
        /// The Shortcut's name.
        name: String,
    },
    /// Runs AppleScript.
    AppleScript {
        /// The script.
        source: String,
    },
    /// Runs a shell script.
    Shell {
        /// The script.
        source: String,
    },
    /// Runs a Keyboard Maestro macro. Empty until a name is typed.
    KeyboardMaestro {
        /// The macro's name.
        name: String,
    },
    /// Moves the window in front.
    Window {
        /// Where to.
        placement: PlacementId,
    },
    /// Something the system does.
    System {
        /// What.
        control: ControlId,
    },
    /// Shows a notice.
    Notify {
        /// What it says.
        text: String,
    },
    /// Speaks text.
    Speak {
        /// What to say.
        text: String,
    },
    /// Waits.
    Delay {
        /// For how long: [`MIN_DELAY_MS`] to [`MAX_DELAY_MS`].
        ms: u32,
    },
}

impl StepKind {
    /// Which kind this is.
    pub const fn id(&self) -> StepKindId {
        match self {
            StepKind::Keys { .. } => StepKindId::Keys,
            StepKind::TypeText { .. } => StepKindId::TypeText,
            StepKind::Clipboard { .. } => StepKindId::Clipboard,
            StepKind::OpenApp { .. } => StepKindId::OpenApp,
            StepKind::OpenFile { .. } => StepKindId::OpenFile,
            StepKind::OpenUrl { .. } => StepKindId::OpenUrl,
            StepKind::AppleShortcut { .. } => StepKindId::AppleShortcut,
            StepKind::AppleScript { .. } => StepKindId::AppleScript,
            StepKind::Shell { .. } => StepKindId::Shell,
            StepKind::KeyboardMaestro { .. } => StepKindId::KeyboardMaestro,
            StepKind::Window { .. } => StepKindId::Window,
            StepKind::System { .. } => StepKindId::System,
            StepKind::Notify { .. } => StepKindId::Notify,
            StepKind::Speak { .. } => StepKindId::Speak,
            StepKind::Delay { .. } => StepKindId::Delay,
        }
    }

    /// What a step of `kind` holds when it's added: nothing chosen yet.
    pub fn fresh(kind: StepKindId) -> Self {
        match kind {
            StepKindId::Keys => StepKind::Keys { chord: None },
            StepKindId::TypeText => StepKind::TypeText {
                text: String::new(),
            },
            StepKindId::Clipboard => StepKind::Clipboard {
                action: ClipboardAction::Copy,
            },
            StepKindId::OpenApp => StepKind::OpenApp {
                path: None,
                bundle_id: None,
                name: None,
            },
            StepKindId::OpenFile => StepKind::OpenFile {
                path: None,
                folder: false,
            },
            StepKindId::OpenUrl => StepKind::OpenUrl { url: String::new() },
            StepKindId::AppleShortcut => StepKind::AppleShortcut {
                name: String::new(),
            },
            StepKindId::AppleScript => StepKind::AppleScript {
                source: String::new(),
            },
            StepKindId::Shell => StepKind::Shell {
                source: String::new(),
            },
            StepKindId::KeyboardMaestro => StepKind::KeyboardMaestro {
                name: String::new(),
            },
            StepKindId::Window => StepKind::Window {
                placement: PlacementId::LeftHalf,
            },
            StepKindId::System => StepKind::System {
                control: ControlId::MissionControl,
            },
            StepKindId::Notify => StepKind::Notify {
                text: String::new(),
            },
            StepKindId::Speak => StepKind::Speak {
                text: String::new(),
            },
            StepKindId::Delay => StepKind::Delay { ms: 500 },
        }
    }

    /// `false` for a step with nothing chosen or typed yet. The runner
    /// skips those.
    pub fn is_set(&self) -> bool {
        match self {
            StepKind::Keys { chord } => chord.is_some(),
            StepKind::OpenApp { path, .. } | StepKind::OpenFile { path, .. } => path.is_some(),
            StepKind::TypeText { text } | StepKind::Notify { text } | StepKind::Speak { text } => {
                !text.is_empty()
            }
            StepKind::OpenUrl { url } => !url.is_empty(),
            StepKind::AppleShortcut { name } | StepKind::KeyboardMaestro { name } => {
                !name.is_empty()
            }
            StepKind::AppleScript { source } | StepKind::Shell { source } => !source.is_empty(),
            StepKind::Clipboard { .. }
            | StepKind::Window { .. }
            | StepKind::System { .. }
            | StepKind::Delay { .. } => true,
        }
    }
}

impl fmt::Debug for StepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.id(), f)
    }
}

/// One step of a shortcut.
#[derive(Clone, PartialEq, Eq)]
pub struct Step {
    /// Its id.
    pub id: Id,
    /// What it does.
    pub kind: StepKind,
}

impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Step")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish()
    }
}

/// `true` when any of `steps` has something to do.
pub fn runnable(steps: &[Step]) -> bool {
    steps.iter().any(|step| step.kind.is_set())
}

/// What a segment holds.
#[derive(Clone, PartialEq, Eq)]
pub enum ItemKind {
    /// A shortcut: steps run in order. None yet is fine.
    Steps(Vec<Step>),
    /// A group: a second ring of shortcuts. Never another group.
    Group(Vec<Item>),
}

impl fmt::Debug for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemKind::Steps(steps) => f.debug_tuple("Steps").field(steps).finish(),
            ItemKind::Group(items) => f.debug_tuple("Group").field(&items.len()).finish(),
        }
    }
}

/// A segment of a ring: a shortcut or a group.
#[derive(Clone, PartialEq, Eq)]
pub struct Item {
    /// Its id.
    pub id: Id,
    /// Its name: 1 to [`MAX_NAME`] units, trimmed, one line.
    pub name: String,
    /// Its icon.
    pub icon: Icon,
    /// Its steps, or its shortcuts.
    pub kind: ItemKind,
}

impl fmt::Debug for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Item")
            .field("id", &self.id)
            .field("icon", &self.icon)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Which apps a menu is for.
#[derive(Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every app that has no menu of its own.
    Everywhere,
    /// One app.
    App {
        /// The app's id.
        bundle_id: String,
        /// Its display name, for Settings.
        name: String,
    },
}

impl fmt::Debug for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Scope::Everywhere => "Everywhere",
            Scope::App { .. } => "App",
        })
    }
}

/// One ring of shortcuts, for every app or for one.
#[derive(Clone, PartialEq, Eq)]
pub struct Menu {
    /// Its id.
    pub id: Id,
    /// Which apps it's for.
    pub scope: Scope,
    /// An app's menu only: end the ring with a group holding the
    /// Everywhere menu's shortcuts.
    pub with_everywhere: bool,
    /// Its segments, at most [`MAX_ITEMS`].
    pub items: Vec<Item>,
}

impl fmt::Debug for Menu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Menu")
            .field("id", &self.id)
            .field("scope", &self.scope)
            .field("with_everywhere", &self.with_everywhere)
            .field("items", &self.items.len())
            .finish()
    }
}

/// Everything the user set up for the Shortcuts wheel.
///
/// There is always exactly one Everywhere menu, no app has two menus, and
/// `enabled` is never `true` without a hot key. Build one with
/// [`Config::defaults`] or [`load`], and change it with the edits below.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// Whether the hot key opens the wheel. Off until the user turns it
    /// on.
    pub enabled: bool,
    /// The chord that opens the wheel.
    pub hotkey: Option<Chord>,
    /// Where it opens.
    pub opens_at: OpensAt,
    /// The menus, at most [`MAX_MENUS`].
    pub menus: Vec<Menu>,
    /// The next id to hand out.
    next_id: Id,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (items, steps) = count(&self.menus);
        f.debug_struct("Config")
            .field("enabled", &self.enabled)
            .field("hotkey", &self.hotkey.is_some())
            .field("opens_at", &self.opens_at)
            .field("menus", &self.menus.len())
            .field("items", &items)
            .field("steps", &steps)
            .finish()
    }
}

/// How many segments (groups and their shortcuts both) and steps `menus`
/// hold.
fn count(menus: &[Menu]) -> (usize, usize) {
    let (mut items, mut steps) = (0, 0);
    for item in menus.iter().flat_map(|menu| &menu.items) {
        items += 1;
        match &item.kind {
            ItemKind::Steps(own) => steps += own.len(),
            ItemKind::Group(children) => {
                items += children.len();
                steps += steps_in(children).count();
            }
        }
    }
    (items, steps)
}

/// The steps of the shortcuts in `ring`, not looking inside groups.
fn steps_in(ring: &[Item]) -> impl Iterator<Item = &Step> {
    ring.iter().flat_map(|item| match &item.kind {
        ItemKind::Steps(steps) => steps.as_slice(),
        ItemKind::Group(_) => &[],
    })
}

fn take_id(next: &mut Id) -> Id {
    let id = *next;
    *next += 1;
    id
}

fn number_item(item: &mut Item, next: &mut Id) {
    item.id = take_id(next);
    match &mut item.kind {
        ItemKind::Steps(steps) => {
            for step in steps {
                step.id = take_id(next);
            }
        }
        ItemKind::Group(children) => {
            for child in children {
                number_item(child, next);
            }
        }
    }
}

/// A one-step shortcut for the starter set. Ids come from `renumber`.
fn starter(name: &str, glyph: &str, kind: StepKind) -> Item {
    Item {
        id: 0,
        name: name.to_owned(),
        icon: Icon::glyph(glyph),
        kind: ItemKind::Steps(vec![Step { id: 0, kind }]),
    }
}

/// What the Everywhere menu holds before the user changes anything on a
/// Mac: clipboard and system steps only, so they're right on every
/// keyboard layout.
fn starter_set() -> Vec<Item> {
    let clipboard = |action| StepKind::Clipboard { action };
    let system = |control| StepKind::System { control };
    vec![
        starter("Copy", "clipboard", clipboard(ClipboardAction::Copy)),
        starter("Paste", "text", clipboard(ClipboardAction::Paste)),
        starter("Screenshot", "camera", system(ControlId::ScreenshotArea)),
        starter(
            "Mission Control",
            "window",
            system(ControlId::MissionControl),
        ),
        starter("Mute", "speaker", system(ControlId::ToggleMute)),
        starter("Lock screen", "lock", system(ControlId::LockScreen)),
    ]
}

impl Config {
    /// What there is before a file exists: the feature off, the default
    /// hot key, and one Everywhere menu. On a Mac it holds the starter
    /// set; elsewhere it's empty.
    pub fn defaults(platform: Platform) -> Self {
        let items = if available(platform) {
            starter_set()
        } else {
            Vec::new()
        };
        Self::assembled(
            false,
            Some(DEFAULT_HOTKEY),
            OpensAt::Cursor,
            vec![(Scope::Everywhere, false, items)],
        )
    }

    /// A config of these parts, numbered from 1 in reading order.
    fn assembled(
        enabled: bool,
        hotkey: Option<Chord>,
        opens_at: OpensAt,
        menus: Vec<(Scope, bool, Vec<Item>)>,
    ) -> Self {
        let mut config = Self {
            enabled,
            hotkey,
            opens_at,
            menus: menus
                .into_iter()
                .map(|(scope, with_everywhere, items)| Menu {
                    id: 0,
                    scope,
                    with_everywhere,
                    items,
                })
                .collect(),
            next_id: 1,
        };
        config.renumber();
        config
    }

    /// Numbers every menu, segment and step from 1, in reading order.
    fn renumber(&mut self) {
        let mut next = 1;
        for menu in &mut self.menus {
            menu.id = take_id(&mut next);
            for item in &mut menu.items {
                number_item(item, &mut next);
            }
        }
        self.next_id = next;
    }

    /// The Everywhere menu's segments.
    pub fn everywhere(&self) -> &[Item] {
        everywhere_of(&self.menus)
    }

    /// Every step of every shortcut.
    fn steps(&self) -> Vec<&Step> {
        let mut all = Vec::new();
        for menu in &self.menus {
            all.extend(steps_in(&menu.items));
            for item in &menu.items {
                if let ItemKind::Group(children) = &item.kind {
                    all.extend(steps_in(children));
                }
            }
        }
        all
    }

    fn item_mut(&mut self, id: Id) -> Option<&mut Item> {
        for item in self.menus.iter_mut().flat_map(|menu| &mut menu.items) {
            if item.id == id {
                return Some(item);
            }
            if let ItemKind::Group(children) = &mut item.kind {
                if let Some(child) = children.iter_mut().find(|child| child.id == id) {
                    return Some(child);
                }
            }
        }
        None
    }

    /// The ring or group that holds segment `id`, and its place there.
    fn ring_of_mut(&mut self, id: Id) -> Option<(&mut Vec<Item>, usize)> {
        for menu in &mut self.menus {
            if let Some(at) = menu.items.iter().position(|item| item.id == id) {
                return Some((&mut menu.items, at));
            }
            for item in &mut menu.items {
                if let ItemKind::Group(children) = &mut item.kind {
                    if let Some(at) = children.iter().position(|child| child.id == id) {
                        return Some((children, at));
                    }
                }
            }
        }
        None
    }

    /// The steps that hold step `id`, and its place there.
    fn steps_of_mut(&mut self, id: Id) -> Option<(&mut Vec<Step>, usize)> {
        for item in self.menus.iter_mut().flat_map(|menu| &mut menu.items) {
            match &mut item.kind {
                ItemKind::Steps(steps) => {
                    if let Some(at) = steps.iter().position(|step| step.id == id) {
                        return Some((steps, at));
                    }
                }
                ItemKind::Group(children) => {
                    for child in children {
                        if let ItemKind::Steps(steps) = &mut child.kind {
                            if let Some(at) = steps.iter().position(|step| step.id == id) {
                                return Some((steps, at));
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

/// The Everywhere menu's segments among `menus`.
fn everywhere_of(menus: &[Menu]) -> &[Item] {
    menus
        .iter()
        .find(|menu| menu.scope == Scope::Everywhere)
        .map_or(&[], |menu| menu.items.as_slice())
}

// ---------------------------------------------------------------------
// What a field may hold. The edits and the loader share these.
// ---------------------------------------------------------------------

/// A shortcut's or group's name, trimmed.
fn checked_name(name: &str) -> Result<String, &'static str> {
    let name = name.trim();
    if name.chars().any(breaks_a_line) {
        return Err(NAME_CONTROLS);
    }
    if !(1..=MAX_NAME).contains(&units(name)) {
        return Err(NAME_LENGTH);
    }
    Ok(name.to_owned())
}

/// An icon, with an emoji's text trimmed.
fn checked_icon(icon: &Icon) -> Result<Icon, &'static str> {
    match icon {
        Icon::None => Ok(Icon::None),
        Icon::Emoji { value } => {
            let value = value.trim();
            let whole = value
                .chars()
                .all(|c| !c.is_whitespace() && !breaks_a_line(c));
            if !whole || !(1..=MAX_ICON).contains(&units(value)) {
                return Err(ICON_EMOJI);
            }
            Ok(Icon::Emoji {
                value: value.to_owned(),
            })
        }
        Icon::Glyph { value } => {
            if !GLYPHS.contains(&value.as_str()) {
                return Err(ICON_GLYPH);
            }
            Ok(icon.clone())
        }
    }
}

/// Text of several lines: typed text, a notice, speech or a script.
/// `\r\n` becomes `\n`; `\n` and `\t` are the only control characters it
/// may hold.
fn checked_text(text: &str, limit: usize, too_long: &'static str) -> Result<String, &'static str> {
    let text = text.replace("\r\n", "\n");
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(TEXT_CONTROLS);
    }
    if units(&text) > limit {
        return Err(too_long);
    }
    Ok(text)
}

/// The schemes Grape won't open: the page never sends a path, and the
/// other two open nothing useful.
const REFUSED_SCHEMES: [&str; 3] = ["file", "javascript", "data"];

/// A link, trimmed. Empty is "none yet". This checks the shape only: any
/// other scheme opens whatever app registered it.
fn checked_url(url: &str) -> Result<String, &'static str> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(String::new());
    }
    if units(url) > MAX_URL {
        return Err(URL_TOO_LONG);
    }
    if url
        .chars()
        .any(|c| c <= '\u{20}' || ('\u{7f}'..='\u{9f}').contains(&c))
    {
        return Err(URL_SHAPE);
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err(URL_SHAPE);
    };
    let mut letters = scheme.bytes();
    let starts = letters.next().is_some_and(|b| b.is_ascii_lowercase());
    let continues = letters
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'.' | b'-'));
    if !starts || !continues || scheme.len() > 32 || rest.is_empty() {
        return Err(URL_SHAPE);
    }
    if REFUSED_SCHEMES.contains(&scheme) {
        return Err(URL_SCHEME);
    }
    Ok(url.to_owned())
}

/// The name of an Apple Shortcut or a Keyboard Maestro macro, trimmed.
/// Empty is "none yet". It becomes a program's argument, so it can't look
/// like an option.
fn checked_target_name(name: &str) -> Result<String, &'static str> {
    let name = name.trim();
    if name.starts_with('-') || name.chars().any(breaks_a_line) {
        return Err(TARGET_NAME);
    }
    if units(name) > MAX_TEXT {
        return Err(TEXT_TOO_LONG);
    }
    Ok(name.to_owned())
}

fn checked_delay(ms: u32) -> Result<u32, &'static str> {
    if (MIN_DELAY_MS..=MAX_DELAY_MS).contains(&ms) {
        Ok(ms)
    } else {
        Err(DELAY_RANGE)
    }
}

/// A Keys step's chord: any chord but the wheel's own hot key.
fn checked_keys(
    chord: Option<&ChordWire>,
    hotkey: Option<Chord>,
) -> Result<Option<Chord>, &'static str> {
    let Some(wire) = chord else {
        return Ok(None);
    };
    let chord = wire.chord().ok_or(UNKNOWN_KEY)?;
    if hotkey == Some(chord) {
        return Err(KEYS_IS_HOTKEY);
    }
    Ok(Some(chord))
}

/// `true` for an app's id or display name Grape can keep.
fn app_field_ok(text: &str) -> bool {
    (1..=MAX_APP_FIELD).contains(&units(text)) && !text.chars().any(breaks_a_line)
}

/// `true` for a path Grape can keep: absolute on a Mac or a PC, one line.
/// Judged from the text, so it's the same answer on every OS.
fn path_ok(path: &str) -> bool {
    let bytes = path.as_bytes();
    let rooted = bytes.first() == Some(&b'/')
        || path.starts_with(r"\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'));
    rooted && units(path) <= MAX_PATH && !path.chars().any(breaks_a_line)
}

/// An Open app step's three values: none of them, or at least a path and
/// a name.
fn checked_app(
    path: Option<&str>,
    bundle_id: Option<&str>,
    name: Option<&str>,
) -> Result<StepKind, &'static str> {
    match (path, name) {
        (None, None) if bundle_id.is_none() => Ok(StepKind::fresh(StepKindId::OpenApp)),
        (Some(path), Some(name))
            if path_ok(path) && app_field_ok(name) && bundle_id.is_none_or(app_field_ok) =>
        {
            Ok(StepKind::OpenApp {
                path: Some(PathBuf::from(path)),
                bundle_id: bundle_id.map(str::to_owned),
                name: Some(name.to_owned()),
            })
        }
        _ => Err(TARGET_UNUSABLE),
    }
}

fn checked_file(path: Option<&str>, folder: bool) -> Result<StepKind, &'static str> {
    match path {
        None => Ok(StepKind::OpenFile {
            path: None,
            folder: false,
        }),
        Some(path) if path_ok(path) => Ok(StepKind::OpenFile {
            path: Some(PathBuf::from(path)),
            folder,
        }),
        Some(_) => Err(TARGET_UNUSABLE),
    }
}

// ---------------------------------------------------------------------
// Edits. Each checks everything first, so a refusal changes nothing.
// ---------------------------------------------------------------------

/// Turns the wheel's hot key on or off. On is refused while there's no
/// hot key.
pub fn set_enabled(config: &mut Config, on: bool) -> Result<(), &'static str> {
    if on && config.hotkey.is_none() {
        return Err(NO_HOTKEY);
    }
    config.enabled = on;
    Ok(())
}

/// Sets the chord that opens the wheel. `None` removes it and turns the
/// feature off. Refused for a chord that can't be a hot key on `platform`
/// ([`Chord::hotkey_problem`]) and for one a Keys step presses.
pub fn set_hotkey(
    config: &mut Config,
    chord: Option<&ChordWire>,
    platform: Platform,
) -> Result<(), &'static str> {
    let Some(wire) = chord else {
        config.hotkey = None;
        config.enabled = false;
        return Ok(());
    };
    let chord = wire.chord().ok_or(UNKNOWN_KEY)?;
    let mac = matches!(platform, Platform::MacOs);
    match chord.hotkey_problem(mac) {
        Some(HotkeyProblem::NeedsModifier) if mac => return Err(HOTKEY_NEEDS_MODIFIER_MAC),
        Some(HotkeyProblem::NeedsModifier) => return Err(HOTKEY_NEEDS_MODIFIER),
        Some(HotkeyProblem::Reserved) => return Err(HOTKEY_RESERVED),
        None => {}
    }
    let pressed =
        |step: &Step| matches!(step.kind, StepKind::Keys { chord: Some(own) } if own == chord);
    if config.steps().into_iter().any(pressed) {
        return Err(HOTKEY_IN_STEP);
    }
    config.hotkey = Some(chord);
    Ok(())
}

/// Sets where the wheel opens. Never refused.
pub fn set_opens_at(config: &mut Config, at: OpensAt) -> Result<(), &'static str> {
    config.opens_at = at;
    Ok(())
}

/// Adds an empty menu for the app with this id and display name, and
/// answers with the menu's id. Refused for an app that has a menu, for
/// Grape itself (`own_id`) and past [`MAX_MENUS`].
pub fn add_app_menu(
    config: &mut Config,
    bundle_id: &str,
    name: &str,
    own_id: &str,
) -> Result<Id, &'static str> {
    let name = name.trim();
    if !app_field_ok(bundle_id) || !app_field_ok(name) {
        return Err(APP_UNUSABLE);
    }
    if bundle_id == own_id {
        return Err(APP_IS_GRAPE);
    }
    let taken =
        |menu: &Menu| matches!(&menu.scope, Scope::App { bundle_id: id, .. } if id == bundle_id);
    if config.menus.iter().any(taken) {
        return Err(APP_HAS_MENU);
    }
    if config.menus.len() >= MAX_MENUS {
        return Err(MENUS_FULL);
    }
    let id = take_id(&mut config.next_id);
    config.menus.push(Menu {
        id,
        scope: Scope::App {
            bundle_id: bundle_id.to_owned(),
            name: name.to_owned(),
        },
        with_everywhere: false,
        items: Vec::new(),
    });
    Ok(id)
}

/// Removes an app's menu and everything in it. The Everywhere menu stays.
pub fn remove_menu(config: &mut Config, menu: Id) -> Result<(), &'static str> {
    let at = config
        .menus
        .iter()
        .position(|own| own.id == menu)
        .ok_or(MENU_GONE)?;
    if config.menus[at].scope == Scope::Everywhere {
        return Err(EVERYWHERE_STAYS);
    }
    config.menus.remove(at);
    Ok(())
}

/// Turns an app menu's Everywhere group on or off. Answers with a note
/// for the view when it's on but the menu is too full to show it.
pub fn set_menu_everywhere(
    config: &mut Config,
    menu: Id,
    on: bool,
) -> Result<Option<&'static str>, &'static str> {
    let menu = config
        .menus
        .iter_mut()
        .find(|own| own.id == menu)
        .ok_or(MENU_GONE)?;
    if menu.scope == Scope::Everywhere {
        return Err(EVERYWHERE_SWITCH);
    }
    menu.with_everywhere = on;
    let fits = menu.items.len() < MAX_ITEMS;
    Ok((on && !fits).then_some(EVERYWHERE_DID_NOT_FIT))
}

/// What `shortcuts_add_item` adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NewItem {
    /// A shortcut, with no steps yet.
    Steps,
    /// A group, with no shortcuts yet.
    Group,
}

/// Adds "New shortcut" or "New group" at the end of a menu's ring, or of
/// one of its groups, and answers with its id. Refused in a full ring and
/// for a group inside a group.
pub fn add_item(
    config: &mut Config,
    menu: Id,
    group: Option<Id>,
    kind: NewItem,
) -> Result<Id, &'static str> {
    let menu = config
        .menus
        .iter_mut()
        .find(|own| own.id == menu)
        .ok_or(MENU_GONE)?;
    let ring = match group {
        None => &mut menu.items,
        Some(group) => {
            let found = menu.items.iter_mut().find(|item| item.id == group);
            let Some(ItemKind::Group(children)) = found.map(|item| &mut item.kind) else {
                return Err(GROUP_GONE);
            };
            if kind == NewItem::Group {
                return Err(GROUP_IN_GROUP);
            }
            children
        }
    };
    if ring.len() >= MAX_ITEMS {
        return Err(RING_FULL);
    }
    let id = take_id(&mut config.next_id);
    let (name, kind) = match kind {
        NewItem::Steps => (NEW_SHORTCUT, ItemKind::Steps(Vec::new())),
        NewItem::Group => (NEW_GROUP, ItemKind::Group(Vec::new())),
    };
    ring.push(Item {
        id,
        name: name.to_owned(),
        icon: Icon::None,
        kind,
    });
    Ok(id)
}

/// Renames a shortcut or a group and sets its icon. The name is trimmed.
pub fn set_item(
    config: &mut Config,
    item: Id,
    name: &str,
    icon: &Icon,
) -> Result<(), &'static str> {
    let item = config.item_mut(item).ok_or(ITEM_GONE)?;
    let name = checked_name(name)?;
    let icon = checked_icon(icon)?;
    item.name = name;
    item.icon = icon;
    Ok(())
}

/// Removes a shortcut, or a group and its shortcuts.
pub fn remove_item(config: &mut Config, item: Id) -> Result<(), &'static str> {
    let (ring, at) = config.ring_of_mut(item).ok_or(ITEM_GONE)?;
    ring.remove(at);
    Ok(())
}

/// Moves a shortcut or a group to place `to` (from 0) of the ring or group
/// it's in. Refused out of range.
pub fn move_item(config: &mut Config, item: Id, to: usize) -> Result<(), &'static str> {
    let (ring, at) = config.ring_of_mut(item).ok_or(ITEM_GONE)?;
    if to >= ring.len() {
        return Err(CANT_MOVE);
    }
    let moved = ring.remove(at);
    ring.insert(to, moved);
    Ok(())
}

/// Adds a step of `kind`, with nothing chosen yet, at the end of a
/// shortcut, and answers with its id. Refused for a group and past
/// [`MAX_STEPS`].
pub fn add_step(config: &mut Config, item: Id, kind: StepKindId) -> Result<Id, &'static str> {
    let next = config.next_id;
    let item = config.item_mut(item).ok_or(ITEM_GONE)?;
    let ItemKind::Steps(steps) = &mut item.kind else {
        return Err(GROUP_NO_STEPS);
    };
    if steps.len() >= MAX_STEPS {
        return Err(STEPS_FULL);
    }
    steps.push(Step {
        id: next,
        kind: StepKind::fresh(kind),
    });
    config.next_id = next + 1;
    Ok(next)
}

/// What `shortcuts_set_step` sends: a step's new values. There is none for
/// an Open app or Open file step: [`set_target`] sets those, after a pick
/// on the Rust side. `Debug` prints the kind only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum StepEdit {
    /// A Keys step's chord, or `None` for none yet.
    Keys {
        /// The chord.
        chord: Option<ChordWire>,
    },
    /// A Type text step's text.
    TypeText {
        /// The text.
        text: String,
    },
    /// What a Clipboard step does.
    Clipboard {
        /// Which.
        action: ClipboardAction,
    },
    /// An Open link step's link.
    OpenUrl {
        /// The link.
        url: String,
    },
    /// An Apple Shortcut step's name.
    AppleShortcut {
        /// The name.
        name: String,
    },
    /// An AppleScript step's script.
    AppleScript {
        /// The script.
        source: String,
    },
    /// A Shell step's script.
    Shell {
        /// The script.
        source: String,
    },
    /// A Keyboard Maestro step's macro name.
    KeyboardMaestro {
        /// The name.
        name: String,
    },
    /// Where a Window step puts the window.
    Window {
        /// Where.
        placement: PlacementId,
    },
    /// What a System step does.
    System {
        /// What.
        control: ControlId,
    },
    /// A Notify step's text.
    Notify {
        /// The text.
        text: String,
    },
    /// A Speak step's text.
    Speak {
        /// The text.
        text: String,
    },
    /// A Delay step's length.
    Delay {
        /// Milliseconds.
        ms: u32,
    },
}

impl StepEdit {
    /// Which kind of step this is for.
    pub const fn id(&self) -> StepKindId {
        match self {
            StepEdit::Keys { .. } => StepKindId::Keys,
            StepEdit::TypeText { .. } => StepKindId::TypeText,
            StepEdit::Clipboard { .. } => StepKindId::Clipboard,
            StepEdit::OpenUrl { .. } => StepKindId::OpenUrl,
            StepEdit::AppleShortcut { .. } => StepKindId::AppleShortcut,
            StepEdit::AppleScript { .. } => StepKindId::AppleScript,
            StepEdit::Shell { .. } => StepKindId::Shell,
            StepEdit::KeyboardMaestro { .. } => StepKindId::KeyboardMaestro,
            StepEdit::Window { .. } => StepKindId::Window,
            StepEdit::System { .. } => StepKindId::System,
            StepEdit::Notify { .. } => StepKindId::Notify,
            StepEdit::Speak { .. } => StepKindId::Speak,
            StepEdit::Delay { .. } => StepKindId::Delay,
        }
    }

    /// The step this edit makes, once every value passed its check.
    fn checked(&self, hotkey: Option<Chord>) -> Result<StepKind, &'static str> {
        Ok(match self {
            StepEdit::Keys { chord } => StepKind::Keys {
                chord: checked_keys(chord.as_ref(), hotkey)?,
            },
            StepEdit::TypeText { text } => StepKind::TypeText {
                text: checked_text(text, MAX_TEXT, TEXT_TOO_LONG)?,
            },
            StepEdit::Clipboard { action } => StepKind::Clipboard { action: *action },
            StepEdit::OpenUrl { url } => StepKind::OpenUrl {
                url: checked_url(url)?,
            },
            StepEdit::AppleShortcut { name } => StepKind::AppleShortcut {
                name: checked_target_name(name)?,
            },
            StepEdit::AppleScript { source } => StepKind::AppleScript {
                source: checked_text(source, MAX_SCRIPT, SCRIPT_TOO_LONG)?,
            },
            StepEdit::Shell { source } => StepKind::Shell {
                source: checked_text(source, MAX_SCRIPT, SCRIPT_TOO_LONG)?,
            },
            StepEdit::KeyboardMaestro { name } => StepKind::KeyboardMaestro {
                name: checked_target_name(name)?,
            },
            StepEdit::Window { placement } => StepKind::Window {
                placement: *placement,
            },
            StepEdit::System { control } => StepKind::System { control: *control },
            StepEdit::Notify { text } => StepKind::Notify {
                text: checked_text(text, MAX_TEXT, TEXT_TOO_LONG)?,
            },
            StepEdit::Speak { text } => StepKind::Speak {
                text: checked_text(text, MAX_TEXT, TEXT_TOO_LONG)?,
            },
            StepEdit::Delay { ms } => StepKind::Delay {
                ms: checked_delay(*ms)?,
            },
        })
    }
}

impl fmt::Debug for StepEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StepEdit").field(&self.id()).finish()
    }
}

/// Gives a step new values. Refused when the edit is for another kind of
/// step than the stored one, and when a value fails its check; text is
/// stored as the check cleaned it (trimmed names and links, `\r\n` as
/// `\n`).
pub fn set_step(config: &mut Config, step: Id, edit: &StepEdit) -> Result<(), &'static str> {
    let hotkey = config.hotkey;
    let (steps, at) = config.steps_of_mut(step).ok_or(STEP_GONE)?;
    if steps[at].kind.id() != edit.id() {
        return Err(KIND_MISMATCH);
    }
    steps[at].kind = edit.checked(hotkey)?;
    Ok(())
}

/// Removes a step.
pub fn remove_step(config: &mut Config, step: Id) -> Result<(), &'static str> {
    let (steps, at) = config.steps_of_mut(step).ok_or(STEP_GONE)?;
    steps.remove(at);
    Ok(())
}

/// Moves a step to place `to` (from 0) of its own shortcut. Refused out of
/// range.
pub fn move_step(config: &mut Config, step: Id, to: usize) -> Result<(), &'static str> {
    let (steps, at) = config.steps_of_mut(step).ok_or(STEP_GONE)?;
    if to >= steps.len() {
        return Err(CANT_MOVE);
    }
    let moved = steps.remove(at);
    steps.insert(to, moved);
    Ok(())
}

/// What the user picked for an Open app or Open file step, on the Rust
/// side. `Debug` prints the kind only.
#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    /// An app.
    App {
        /// Where it is.
        path: PathBuf,
        /// Its id, when it has one.
        bundle_id: Option<String>,
        /// Its display name.
        name: String,
    },
    /// A file or a folder.
    File {
        /// Where it is.
        path: PathBuf,
        /// `true` for a folder.
        folder: bool,
    },
}

impl Target {
    /// What the picker of `kind` answered with, as a target. An app with
    /// no identity is named after its file.
    pub fn picked(kind: PickKind, picked: Picked) -> Self {
        match kind {
            PickKind::App => {
                let (bundle_id, name) = match picked.app {
                    Some(app) => (Some(app.id), app.name),
                    None => (None, file_stem(&picked.path)),
                };
                Target::App {
                    path: picked.path,
                    bundle_id,
                    name,
                }
            }
            PickKind::FileOrFolder => Target::File {
                path: picked.path,
                folder: picked.folder,
            },
        }
    }
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Target::App { .. } => "App",
            Target::File { .. } => "File",
        })
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Sets what an Open app or Open file step opens. Refused for another kind
/// of step, for an app on a file step or the other way round, and for a
/// path or name Grape can't keep (the file is text).
pub fn set_target(config: &mut Config, step: Id, target: &Target) -> Result<(), &'static str> {
    let (steps, at) = config.steps_of_mut(step).ok_or(STEP_GONE)?;
    let kind = match (&steps[at].kind, target) {
        (
            StepKind::OpenApp { .. },
            Target::App {
                path,
                bundle_id,
                name,
            },
        ) => {
            let path = path.to_str().ok_or(TARGET_UNUSABLE)?;
            checked_app(Some(path), bundle_id.as_deref(), Some(name.trim()))?
        }
        (StepKind::OpenFile { .. }, Target::File { path, folder }) => {
            let path = path.to_str().ok_or(TARGET_UNUSABLE)?;
            checked_file(Some(path), *folder)?
        }
        _ => return Err(KIND_MISMATCH),
    };
    steps[at].kind = kind;
    Ok(())
}

/// Which picker step `step` gets: the app picker for an Open app step, the
/// file and folder one for an Open file step. Refused for any other kind
/// of step, since only those two have a target.
pub fn pick_kind(config: &Config, step: Id) -> Result<PickKind, &'static str> {
    let kind = config
        .steps()
        .into_iter()
        .find(|own| own.id == step)
        .map(|own| own.kind.id())
        .ok_or(STEP_GONE)?;
    match kind {
        StepKindId::OpenApp => Ok(PickKind::App),
        StepKindId::OpenFile => Ok(PickKind::FileOrFolder),
        _ => Err(KIND_MISMATCH),
    }
}

// ---------------------------------------------------------------------
// What the wheel reads.
// ---------------------------------------------------------------------

/// The copy of the config the wheel's controller holds. `Debug` prints
/// counts only.
#[derive(Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Whether the hot key opens the wheel.
    pub enabled: bool,
    /// Where it opens.
    pub opens_at: OpensAt,
    /// The menus.
    pub menus: Vec<Menu>,
    /// The chord that opens it.
    pub hotkey: Option<Chord>,
}

impl Snapshot {
    /// A copy of `config` as it is now.
    pub fn of(config: &Config) -> Self {
        Self {
            enabled: config.enabled,
            opens_at: config.opens_at,
            menus: config.menus.clone(),
            hotkey: config.hotkey,
        }
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (items, steps) = count(&self.menus);
        f.debug_struct("Snapshot")
            .field("enabled", &self.enabled)
            .field("opens_at", &self.opens_at)
            .field("hotkey", &self.hotkey.is_some())
            .field("menus", &self.menus.len())
            .field("items", &items)
            .field("steps", &steps)
            .finish()
    }
}

/// What a segment of an open wheel holds.
#[derive(Clone, PartialEq, Eq)]
pub enum RingKind {
    /// A shortcut's steps.
    Steps(Vec<Step>),
    /// A group's shortcuts: the second ring.
    Group(Vec<RingItem>),
}

/// A segment of an open wheel. `Debug` prints kinds and counts only.
#[derive(Clone, PartialEq, Eq)]
pub struct RingItem {
    /// Its name, as typed.
    pub name: String,
    /// Its icon.
    pub icon: Icon,
    /// Its steps, or its shortcuts.
    pub kind: RingKind,
}

impl RingItem {
    /// `true` for a group.
    pub fn is_group(&self) -> bool {
        matches!(self.kind, RingKind::Group(_))
    }

    fn of(item: &Item) -> Self {
        Self {
            name: item.name.clone(),
            icon: item.icon.clone(),
            kind: match &item.kind {
                ItemKind::Steps(steps) => RingKind::Steps(steps.clone()),
                ItemKind::Group(children) => RingKind::Group(shortcuts_of(children)),
            },
        }
    }
}

impl fmt::Debug for RingItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = f.debug_struct("RingItem");
        out.field("icon", &self.icon);
        match &self.kind {
            RingKind::Steps(steps) => out.field("steps", steps),
            RingKind::Group(items) => out.field("group", &items.len()),
        };
        out.finish_non_exhaustive()
    }
}

/// The shortcuts in `items`, groups left out: what a second ring may hold.
fn shortcuts_of(items: &[Item]) -> Vec<RingItem> {
    items
        .iter()
        .filter(|item| matches!(item.kind, ItemKind::Steps(_)))
        .map(RingItem::of)
        .collect()
}

/// The ring to show over the app in front: its own menu if it has one,
/// else the Everywhere menu (also for an app with no id, and for none).
///
/// An app's menu with the Everywhere switch on ends with one more group,
/// "Everywhere", holding the Everywhere menu's shortcuts without its
/// groups. It's left out when the ring is full already.
pub fn menu_for(snapshot: &Snapshot, front_app_id: Option<&str>) -> Vec<RingItem> {
    let everywhere = everywhere_of(&snapshot.menus);
    let own = front_app_id.and_then(|front| {
        snapshot
            .menus
            .iter()
            .find(|menu| matches!(&menu.scope, Scope::App { bundle_id, .. } if bundle_id == front))
    });
    let Some(menu) = own else {
        return everywhere.iter().map(RingItem::of).collect();
    };
    let mut ring: Vec<RingItem> = menu.items.iter().map(RingItem::of).collect();
    if menu.with_everywhere && ring.len() < MAX_ITEMS {
        ring.push(RingItem {
            name: EVERYWHERE_NAME.to_owned(),
            icon: Icon::glyph(EVERYWHERE_GLYPH),
            kind: RingKind::Group(shortcuts_of(everywhere)),
        });
    }
    ring
}

/// A picked shortcut, for the runner. `Debug` prints the step kinds and
/// the front app's pid, never the name or what a step holds.
#[derive(Clone, PartialEq)]
pub struct ShortcutRun {
    /// The wheel it came from.
    pub seq: u64,
    /// The shortcut's name, for the toast.
    pub name: String,
    /// Its steps, in order.
    pub steps: Vec<Step>,
    /// The app that was in front when the wheel opened.
    pub front: Option<FrontApp>,
    /// The wheel's center, in global coordinates (to place the toast).
    pub anchor: (i32, i32),
    /// The display the wheel was on.
    pub display: DisplayBounds,
}

impl fmt::Debug for ShortcutRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShortcutRun")
            .field("seq", &self.seq)
            .field("steps", &self.steps)
            .field("front", &self.front)
            .field("anchor", &self.anchor)
            .field("display", &self.display)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------
// Text variables.
// ---------------------------------------------------------------------

const DATE: &str = "{date}";
const TIME: &str = "{time}";
const CLIPBOARD: &str = "{clipboard}";

/// `true` when typing `text` reads the clipboard.
pub fn needs_clipboard(text: &str) -> bool {
    text.contains(CLIPBOARD)
}

/// `{date}`'s value: `2026-11-23`.
pub fn date_text(now: LocalTime) -> String {
    format!("{:04}-{:02}-{:02}", now.year, now.month, now.day)
}

/// `{time}`'s value: `14:05`.
pub fn time_text(now: LocalTime) -> String {
    format!("{:02}:{:02}", now.hour, now.minute)
}

/// A Type text step's text with `{date}`, `{time}` and `{clipboard}`
/// filled in, in one pass: a value that holds a variable isn't filled in
/// again. Other braces stay as typed, and so do `{date}` and `{time}`
/// when theirs is `None` (the clock is unknown). No `clipboard` fills in
/// nothing.
///
/// The clipboard's text is typed, not pasted, so every control character
/// is taken out of it, a line break or tab becoming one space: copied
/// text typed into a terminal must not run as commands. It's cut so the
/// whole stays within [`MAX_TEXT`], each `{clipboard}` getting the same
/// share. The date and the time are never cut.
pub fn expand_text(
    text: &str,
    date: Option<&str>,
    time: Option<&str>,
    clipboard: Option<&str>,
) -> String {
    // `None` is a place for the clipboard.
    let mut pieces: Vec<Option<&str>> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('{') {
        let (before, tail) = rest.split_at(at);
        pieces.push(Some(before));
        if let Some(after) = tail.strip_prefix(CLIPBOARD) {
            pieces.push(None);
            rest = after;
        } else if let (Some(after), Some(date)) = (tail.strip_prefix(DATE), date) {
            pieces.push(Some(date));
            rest = after;
        } else if let (Some(after), Some(time)) = (tail.strip_prefix(TIME), time) {
            pieces.push(Some(time));
            rest = after;
        } else {
            let (brace, after) = tail.split_at(1);
            pieces.push(Some(brace));
            rest = after;
        }
    }
    pieces.push(Some(rest));

    let fixed: usize = pieces.iter().flatten().map(|piece| units(piece)).sum();
    let places = pieces.iter().filter(|piece| piece.is_none()).count();
    let pasted = match places {
        0 => String::new(),
        _ => clipboard_value(
            clipboard.unwrap_or_default(),
            MAX_TEXT.saturating_sub(fixed) / places,
        ),
    };
    pieces
        .into_iter()
        .map(|piece| piece.unwrap_or(&pasted))
        .collect()
}

/// The clipboard's text made safe to type, within `budget` units.
fn clipboard_value(raw: &str, budget: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in raw.chars() {
        let c = match c {
            '\r' | '\n' | '\t' | '\u{2028}' | '\u{2029}' => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        used += c.len_utf16();
        if used > budget {
            break;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------
// The stored form: `shortcuts.json`. Ids aren't in it.
// ---------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum StoredScope {
    Everywhere {},
    App { bundle_id: String, name: String },
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum StoredStep {
    Keys {
        chord: Option<ChordWire>,
    },
    TypeText {
        text: String,
    },
    Clipboard {
        action: ClipboardAction,
    },
    OpenApp {
        path: Option<String>,
        bundle_id: Option<String>,
        name: Option<String>,
    },
    OpenFile {
        path: Option<String>,
        folder: bool,
    },
    OpenUrl {
        url: String,
    },
    AppleShortcut {
        name: String,
    },
    AppleScript {
        source: String,
    },
    Shell {
        source: String,
    },
    KeyboardMaestro {
        name: String,
    },
    Window {
        placement: PlacementId,
    },
    System {
        control: ControlId,
    },
    Notify {
        text: String,
    },
    Speak {
        text: String,
    },
    Delay {
        ms: u32,
    },
}

impl StoredStep {
    fn of(kind: &StepKind) -> Self {
        let text_of = |path: &Option<PathBuf>| {
            // Every path here came from text, so this never loses one.
            path.as_deref().and_then(Path::to_str).map(str::to_owned)
        };
        match kind {
            StepKind::Keys { chord } => StoredStep::Keys {
                chord: chord.map(ChordWire::from),
            },
            StepKind::TypeText { text } => StoredStep::TypeText { text: text.clone() },
            StepKind::Clipboard { action } => StoredStep::Clipboard { action: *action },
            StepKind::OpenApp {
                path,
                bundle_id,
                name,
            } => StoredStep::OpenApp {
                path: text_of(path),
                bundle_id: bundle_id.clone(),
                name: name.clone(),
            },
            StepKind::OpenFile { path, folder } => StoredStep::OpenFile {
                path: text_of(path),
                folder: *folder,
            },
            StepKind::OpenUrl { url } => StoredStep::OpenUrl { url: url.clone() },
            StepKind::AppleShortcut { name } => StoredStep::AppleShortcut { name: name.clone() },
            StepKind::AppleScript { source } => StoredStep::AppleScript {
                source: source.clone(),
            },
            StepKind::Shell { source } => StoredStep::Shell {
                source: source.clone(),
            },
            StepKind::KeyboardMaestro { name } => {
                StoredStep::KeyboardMaestro { name: name.clone() }
            }
            StepKind::Window { placement } => StoredStep::Window {
                placement: *placement,
            },
            StepKind::System { control } => StoredStep::System { control: *control },
            StepKind::Notify { text } => StoredStep::Notify { text: text.clone() },
            StepKind::Speak { text } => StoredStep::Speak { text: text.clone() },
            StepKind::Delay { ms } => StoredStep::Delay { ms: *ms },
        }
    }

    /// The step this holds, if every value passes the check an edit
    /// makes.
    fn checked(&self, hotkey: Option<Chord>) -> Result<StepKind, &'static str> {
        let edit = match self {
            StoredStep::OpenApp {
                path,
                bundle_id,
                name,
            } => return checked_app(path.as_deref(), bundle_id.as_deref(), name.as_deref()),
            StoredStep::OpenFile { path, folder } => return checked_file(path.as_deref(), *folder),
            StoredStep::Keys { chord } => StepEdit::Keys {
                chord: chord.clone(),
            },
            StoredStep::TypeText { text } => StepEdit::TypeText { text: text.clone() },
            StoredStep::Clipboard { action } => StepEdit::Clipboard { action: *action },
            StoredStep::OpenUrl { url } => StepEdit::OpenUrl { url: url.clone() },
            StoredStep::AppleShortcut { name } => StepEdit::AppleShortcut { name: name.clone() },
            StoredStep::AppleScript { source } => StepEdit::AppleScript {
                source: source.clone(),
            },
            StoredStep::Shell { source } => StepEdit::Shell {
                source: source.clone(),
            },
            StoredStep::KeyboardMaestro { name } => {
                StepEdit::KeyboardMaestro { name: name.clone() }
            }
            StoredStep::Window { placement } => StepEdit::Window {
                placement: *placement,
            },
            StoredStep::System { control } => StepEdit::System { control: *control },
            StoredStep::Notify { text } => StepEdit::Notify { text: text.clone() },
            StoredStep::Speak { text } => StepEdit::Speak { text: text.clone() },
            StoredStep::Delay { ms } => StepEdit::Delay { ms: *ms },
        };
        edit.checked(hotkey)
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum StoredBody {
    Steps { steps: Vec<StoredStep> },
    Group { items: Vec<StoredItem> },
}

#[derive(Serialize)]
struct StoredItem {
    name: String,
    icon: Icon,
    #[serde(flatten)]
    body: StoredBody,
}

impl StoredItem {
    fn of(item: &Item) -> Self {
        Self {
            name: item.name.clone(),
            icon: item.icon.clone(),
            body: match &item.kind {
                ItemKind::Steps(steps) => StoredBody::Steps {
                    steps: steps
                        .iter()
                        .map(|step| StoredStep::of(&step.kind))
                        .collect(),
                },
                ItemKind::Group(children) => StoredBody::Group {
                    items: children.iter().map(StoredItem::of).collect(),
                },
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredMenu {
    scope: StoredScope,
    with_everywhere: bool,
    items: Vec<StoredItem>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredFile {
    version: u64,
    enabled: bool,
    hotkey: Option<ChordWire>,
    opens_at: OpensAt,
    menus: Vec<StoredMenu>,
}

/// What `shortcuts.json` holds for `config`: one line of JSON and a
/// newline. Refused past [`MAX_FILE_BYTES`], so check a change with this
/// before applying it.
pub fn stored_bytes(config: &Config) -> Result<Vec<u8>, &'static str> {
    let file = StoredFile {
        version: VERSION,
        enabled: config.enabled,
        hotkey: config.hotkey.map(ChordWire::from),
        opens_at: config.opens_at,
        menus: config
            .menus
            .iter()
            .map(|menu| StoredMenu {
                scope: match &menu.scope {
                    Scope::Everywhere => StoredScope::Everywhere {},
                    Scope::App { bundle_id, name } => StoredScope::App {
                        bundle_id: bundle_id.clone(),
                        name: name.clone(),
                    },
                },
                with_everywhere: menu.with_everywhere,
                items: menu.items.iter().map(StoredItem::of).collect(),
            })
            .collect(),
    };
    // Plain structs with text keys: this can't fail, but the answer is the
    // same if it ever does.
    let mut bytes = serde_json::to_vec(&file).map_err(|_| TOO_BIG)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FILE_BYTES {
        return Err(TOO_BIG);
    }
    Ok(bytes)
}

/// Why a file can't be used at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// Over [`MAX_LOAD_BYTES`].
    TooLarge,
    /// Not JSON.
    NotJson,
    /// JSON, but not an object.
    NotAnObject,
    /// No `version` this build knows as a number.
    NoVersion,
    /// A `version` from a newer Grape.
    Newer,
    /// No `menus` list.
    NoMenus,
}

impl Unreadable {
    /// The reason in a fixed word or two, for the log.
    pub const fn word(self) -> &'static str {
        match self {
            Unreadable::TooLarge => "too large",
            Unreadable::NotJson => "not JSON",
            Unreadable::NotAnObject => "not an object",
            Unreadable::NoVersion => "no version",
            Unreadable::Newer => "a newer version",
            Unreadable::NoMenus => "no menus",
        }
    }
}

/// Where JSON stopped making sense. serde_json's own message can quote the
/// file, so only its class and place are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseAt {
    /// The error's class: `"Syntax"`, `"Data"`, `"Eof"` or `"Io"`.
    pub class: &'static str,
    /// The line, from 1.
    pub line: usize,
    /// The column, from 1.
    pub column: usize,
}

/// How many parts of a file a load left out, by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drops {
    /// Top-level values that fell back (`enabled`, `hotkey`, `opensAt`),
    /// and top-level keys this build doesn't know.
    pub settings: u32,
    /// Menus that couldn't be read.
    pub menus: u32,
    /// A second Everywhere menu, or a second menu for one app.
    pub duplicate_menus: u32,
    /// Menus past [`MAX_MENUS`].
    pub menus_over: u32,
    /// Shortcuts and groups that couldn't be read.
    pub items: u32,
    /// Shortcuts left out whole because of one of their steps, or because
    /// they had more than [`MAX_STEPS`].
    pub for_steps: u32,
    /// Groups inside a group.
    pub nested_groups: u32,
    /// Shortcuts and groups past [`MAX_ITEMS`] in their ring.
    pub items_over: u32,
}

impl Drops {
    /// How many menus, shortcuts and groups were left out.
    pub const fn left_out(&self) -> u32 {
        self.menus
            + self.duplicate_menus
            + self.menus_over
            + self.items
            + self.for_steps
            + self.nested_groups
            + self.items_over
    }
}

/// What [`load`] found besides the config. It holds counts and fixed words
/// only, so it's safe to log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// What was left out of a file that was otherwise used.
    pub drops: Drops,
    /// Why the file couldn't be used at all, if it couldn't.
    pub unreadable: Option<Unreadable>,
    /// Where its JSON broke, for [`Unreadable::NotJson`].
    pub parse: Option<ParseAt>,
}

impl LoadReport {
    /// `true` when the file was used but something in it was left out or
    /// fell back: saving would lose it, so the file is copied aside
    /// first.
    pub const fn dropped(&self) -> bool {
        self.drops.settings + self.drops.left_out() > 0
    }

    /// `true` when the file can't be used at all and must be moved aside.
    pub const fn move_aside(&self) -> bool {
        self.unreadable.is_some()
    }

    /// The sentence for the view's note after this load, if it needs
    /// one.
    pub const fn note(&self) -> Option<&'static str> {
        if self.unreadable.is_some() {
            Some(STARTED_FRESH)
        } else if self.drops.left_out() > 0 {
            Some(LEFT_OUT)
        } else {
            None
        }
    }
}

/// The report as a log line's tail: counts and fixed words.
impl fmt::Display for LoadReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(why) = self.unreadable {
            f.write_str(why.word())?;
            if let Some(at) = self.parse {
                write!(
                    f,
                    " ({} error at line {}, column {})",
                    at.class, at.line, at.column
                )?;
            }
            return Ok(());
        }
        let d = self.drops;
        write!(
            f,
            "left out {} settings, {} menus ({} twice, {} past the limit), {} shortcuts \
             ({} for a step, {} groups in a group, {} past the limit)",
            d.settings,
            d.menus + d.duplicate_menus + d.menus_over,
            d.duplicate_menus,
            d.menus_over,
            d.items + d.for_steps + d.nested_groups + d.items_over,
            d.for_steps,
            d.nested_groups,
            d.items_over,
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MenuShell {
    scope: StoredScope,
    with_everywhere: bool,
    items: Vec<Value>,
}

/// A segment with its steps or shortcuts still unread, so one bad one
/// doesn't take its neighbours along.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum ItemShell {
    Steps {
        name: String,
        icon: Icon,
        steps: Vec<Value>,
    },
    Group {
        name: String,
        icon: Icon,
        items: Vec<Value>,
    },
}

/// The keys a file has at the top.
const TOP_KEYS: [&str; 5] = ["version", "enabled", "hotkey", "opensAt", "menus"];

/// Reads a `shortcuts.json` for `platform`. Total: it always answers with
/// a usable config.
///
/// A file that's too large, isn't a JSON object, has no `version` this
/// build reads or no `menus` list is not used at all: the answer is
/// [`Config::defaults`], and the report says to move the file aside.
/// Otherwise each part is read by itself. `enabled`, `hotkey` and
/// `opensAt` each fall back alone; a hot key that can't be one becomes
/// none, with the feature off. A menu or segment that can't be read
/// (unknown fields are refused) is left out alone. One bad step leaves its
/// whole shortcut out: running the rest would do something else. A second
/// Everywhere menu, a second menu for one app, a group in a group and
/// whatever is past a limit are left out, and a missing Everywhere menu is
/// added empty, first. The report counts all of it.
pub fn load(bytes: &[u8], platform: Platform) -> (Config, LoadReport) {
    let mut report = LoadReport::default();
    let (mut top, menus) = match read_top(bytes) {
        Ok(parts) => parts,
        Err((why, parse)) => {
            report.unreadable = Some(why);
            report.parse = parse;
            return (Config::defaults(platform), report);
        }
    };
    let drops = &mut report.drops;
    drops.settings += count_u32(
        top.keys()
            .filter(|key| !TOP_KEYS.contains(&key.as_str()))
            .count(),
    );

    let mac = matches!(platform, Platform::MacOs);
    let hotkey = match top.remove("hotkey") {
        None => Some(DEFAULT_HOTKEY),
        Some(Value::Null) => None,
        Some(value) => {
            let chord = ChordWire::deserialize(&value)
                .ok()
                .and_then(|wire| wire.chord())
                .filter(|chord| chord.hotkey_problem(mac).is_none());
            if chord.is_none() {
                drops.settings += 1;
            }
            chord
        }
    };
    let enabled = match top.remove("enabled") {
        None => false,
        Some(Value::Bool(on)) if hotkey.is_some() || !on => on,
        // Not a switch, or on with no hot key to open the wheel.
        Some(_) => {
            drops.settings += 1;
            false
        }
    };
    let opens_at = match top.remove("opensAt") {
        None => OpensAt::Cursor,
        Some(value) => OpensAt::deserialize(&value).unwrap_or_else(|_| {
            drops.settings += 1;
            OpensAt::Cursor
        }),
    };

    let mut kept: Vec<(Scope, bool, Vec<Item>)> = Vec::new();
    for value in &menus {
        let Ok(shell) = MenuShell::deserialize(value) else {
            drops.menus += 1;
            continue;
        };
        let scope = match shell.scope {
            StoredScope::Everywhere {} => Scope::Everywhere,
            StoredScope::App { bundle_id, name } => {
                let name = name.trim();
                if !app_field_ok(&bundle_id) || !app_field_ok(name) {
                    drops.menus += 1;
                    continue;
                }
                Scope::App {
                    bundle_id,
                    name: name.to_owned(),
                }
            }
        };
        let same_scope = |other: &Scope| match (other, &scope) {
            (Scope::Everywhere, Scope::Everywhere) => true,
            (Scope::App { bundle_id: a, .. }, Scope::App { bundle_id: b, .. }) => a == b,
            _ => false,
        };
        if kept.iter().any(|(other, ..)| same_scope(other)) {
            drops.duplicate_menus += 1;
            continue;
        }
        let is_app = scope != Scope::Everywhere;
        let apps = kept
            .iter()
            .filter(|(other, ..)| *other != Scope::Everywhere)
            .count();
        // One place is always the Everywhere menu's.
        if is_app && apps >= MAX_MENUS - 1 {
            drops.menus_over += 1;
            continue;
        }
        let items = read_ring(&shell.items, false, hotkey, drops);
        kept.push((scope, shell.with_everywhere && is_app, items));
    }
    if !kept.iter().any(|(scope, ..)| *scope == Scope::Everywhere) {
        kept.insert(0, (Scope::Everywhere, false, Vec::new()));
    }
    (Config::assembled(enabled, hotkey, opens_at, kept), report)
}

fn count_u32(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Why a file can't be used, and where its JSON broke if it did.
type Unusable = (Unreadable, Option<ParseAt>);

/// The file's top-level object without `menus`, and its `menus` list, or
/// why the file can't be used.
fn read_top(bytes: &[u8]) -> Result<(Map<String, Value>, Vec<Value>), Unusable> {
    if bytes.len() > MAX_LOAD_BYTES {
        return Err((Unreadable::TooLarge, None));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| (Unreadable::NotJson, Some(parse_at(&err))))?;
    let Value::Object(mut top) = value else {
        return Err((Unreadable::NotAnObject, None));
    };
    match top.get("version").and_then(Value::as_u64) {
        Some(VERSION) => {}
        Some(version) if version > VERSION => return Err((Unreadable::Newer, None)),
        _ => return Err((Unreadable::NoVersion, None)),
    }
    let Some(Value::Array(menus)) = top.remove("menus") else {
        return Err((Unreadable::NoMenus, None));
    };
    Ok((top, menus))
}

fn parse_at(err: &serde_json::Error) -> ParseAt {
    use serde_json::error::Category;
    ParseAt {
        class: match err.classify() {
            Category::Io => "Io",
            Category::Syntax => "Syntax",
            Category::Data => "Data",
            Category::Eof => "Eof",
        },
        line: err.line(),
        column: err.column(),
    }
}

/// Reads the segments of a ring, or of a group when `in_group`, each by
/// itself. Ids are 0 until the config is numbered.
fn read_ring(
    values: &[Value],
    in_group: bool,
    hotkey: Option<Chord>,
    drops: &mut Drops,
) -> Vec<Item> {
    let mut ring = Vec::new();
    for value in values {
        let Ok(shell) = ItemShell::deserialize(value) else {
            drops.items += 1;
            continue;
        };
        let (name, icon) = match &shell {
            ItemShell::Steps { name, icon, .. } | ItemShell::Group { name, icon, .. } => {
                (checked_name(name), checked_icon(icon))
            }
        };
        let (Ok(name), Ok(icon)) = (name, icon) else {
            drops.items += 1;
            continue;
        };
        let kind = match &shell {
            ItemShell::Steps { steps, .. } => match read_steps(steps, hotkey) {
                Some(steps) => ItemKind::Steps(steps),
                None => {
                    drops.for_steps += 1;
                    continue;
                }
            },
            ItemShell::Group { .. } if in_group => {
                drops.nested_groups += 1;
                continue;
            }
            ItemShell::Group { items, .. } => {
                ItemKind::Group(read_ring(items, true, hotkey, drops))
            }
        };
        if ring.len() >= MAX_ITEMS {
            drops.items_over += 1;
            continue;
        }
        ring.push(Item {
            id: 0,
            name,
            icon,
            kind,
        });
    }
    ring
}

/// A shortcut's steps, or `None` when one of them can't be used or there
/// are too many.
fn read_steps(values: &[Value], hotkey: Option<Chord>) -> Option<Vec<Step>> {
    if values.len() > MAX_STEPS {
        return None;
    }
    values
        .iter()
        .map(|value| {
            let kind = StoredStep::deserialize(value).ok()?.checked(hotkey).ok()?;
            Some(Step { id: 0, kind })
        })
        .collect()
}

// ---------------------------------------------------------------------
// The Settings view.
// ---------------------------------------------------------------------

/// Whether the wheel's hot key is Grape's right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Binding {
    /// Switched off, no hot key, or not available here.
    Off,
    /// The OS gave Grape the chord.
    Bound,
    /// The OS wouldn't.
    Failed,
}

/// Whether Grape may press keys and move windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    /// It may.
    Granted,
    /// The user hasn't allowed it.
    Denied,
    /// This OS doesn't ask.
    NotNeeded,
}

impl From<InputAccess> for Access {
    fn from(access: InputAccess) -> Self {
        match access {
            InputAccess::Granted => Access::Granted,
            InputAccess::Denied => Access::Denied,
            InputAccess::NotNeeded => Access::NotNeeded,
        }
    }
}

/// The wheel's hot key, for the view.
#[derive(Clone, Serialize)]
pub struct HotkeyView {
    /// The chord.
    pub chord: ChordWire,
    /// How this OS writes it, like "Control+Option+G".
    pub label: String,
}

/// The limits, so the page's fields agree with the checks here.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitsView {
    /// [`MAX_ITEMS`].
    pub items: usize,
    /// [`MAX_STEPS`].
    pub steps: usize,
    /// [`MAX_NAME`].
    pub name: usize,
    /// [`MAX_ICON`].
    pub icon: usize,
    /// [`MAX_TEXT`].
    pub text: usize,
    /// [`MAX_SCRIPT`].
    pub script: usize,
    /// [`MAX_URL`].
    pub url: usize,
    /// [`MAX_MENUS`].
    pub menus: usize,
    /// [`MIN_DELAY_MS`].
    pub delay_min_ms: u32,
    /// [`MAX_DELAY_MS`].
    pub delay_max_ms: u32,
}

/// A key a chord may use.
#[derive(Clone, Copy, Serialize)]
pub struct KeyView {
    /// Its id, like `"KeyA"`.
    pub id: &'static str,
    /// Its name on a US keyboard, like `"A"`.
    pub label: &'static str,
}

/// What this OS calls the four modifiers.
#[derive(Clone, Copy, Serialize)]
pub struct ModifierNames {
    /// Shift.
    pub shift: &'static str,
    /// Control, or Ctrl.
    pub ctrl: &'static str,
    /// Option, or Alt.
    pub alt: &'static str,
    /// Command, or Win.
    pub meta: &'static str,
}

/// Which apps a menu is for, in the view: an app by name only.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ScopeView {
    /// Every app with no menu of its own.
    Everywhere,
    /// One app.
    App {
        /// Its display name.
        name: String,
    },
}

/// A step's values in the view: the stored ones, but a picked app, file or
/// folder by name only, and a chord with its label.
#[derive(Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum StepBodyView {
    /// A Keys step.
    Keys {
        /// The chord, or `null` until one is chosen.
        chord: Option<ChordWire>,
        /// How this OS writes it, or `null`.
        label: Option<String>,
    },
    /// A Type text step.
    TypeText {
        /// The text.
        text: String,
    },
    /// A Clipboard step.
    Clipboard {
        /// What it does.
        action: ClipboardAction,
    },
    /// An Open app step.
    OpenApp {
        /// The app's display name, or `null` until one is picked.
        name: Option<String>,
    },
    /// An Open file step.
    OpenFile {
        /// The file's or folder's name, or `null` until one is picked.
        name: Option<String>,
        /// `true` when a folder was picked.
        folder: bool,
    },
    /// An Open link step.
    OpenUrl {
        /// The link.
        url: String,
    },
    /// An Apple Shortcut step.
    AppleShortcut {
        /// The Shortcut's name.
        name: String,
    },
    /// An AppleScript step.
    AppleScript {
        /// The script.
        source: String,
    },
    /// A Shell step.
    Shell {
        /// The script.
        source: String,
    },
    /// A Keyboard Maestro step.
    KeyboardMaestro {
        /// The macro's name.
        name: String,
    },
    /// A Window step.
    Window {
        /// Where the window goes.
        placement: PlacementId,
    },
    /// A System step.
    System {
        /// What the system does.
        control: ControlId,
    },
    /// A Notify step.
    Notify {
        /// What it says.
        text: String,
    },
    /// A Speak step.
    Speak {
        /// What to say.
        text: String,
    },
    /// A Delay step.
    Delay {
        /// Milliseconds.
        ms: u32,
    },
}

/// A step, in the view.
#[derive(Clone, Serialize)]
pub struct StepView {
    /// Its id.
    pub id: Id,
    /// Its kind and values.
    #[serde(flatten)]
    pub body: StepBodyView,
}

/// A shortcut's steps or a group's shortcuts, in the view.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ItemBodyView {
    /// A shortcut.
    Steps {
        /// Its steps.
        steps: Vec<StepView>,
    },
    /// A group.
    Group {
        /// Its shortcuts.
        items: Vec<ItemView>,
    },
}

/// A shortcut or a group, in the view.
#[derive(Clone, Serialize)]
pub struct ItemView {
    /// Its id.
    pub id: Id,
    /// Its name.
    pub name: String,
    /// Its icon.
    pub icon: Icon,
    /// Its steps, or its shortcuts.
    #[serde(flatten)]
    pub body: ItemBodyView,
}

/// A menu, in the view.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MenuView {
    /// Its id.
    pub id: Id,
    /// Which apps it's for.
    pub scope: ScopeView,
    /// Whether it ends with the Everywhere group.
    pub with_everywhere: bool,
    /// Its segments.
    pub items: Vec<ItemView>,
}

/// What the Settings window shows of the Shortcuts wheel
/// (`ui/src/settings/shortcuts-model.ts`). It carries names, never a path
/// or an app's id. `Debug` prints counts only.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutsView {
    /// `false` where the wheel isn't built yet.
    pub available: bool,
    /// Whether the hot key opens the wheel.
    pub enabled: bool,
    /// The hot key, or `null` when none is set.
    pub hotkey: Option<HotkeyView>,
    /// Whether the hot key is Grape's right now.
    pub binding: Binding,
    /// Whether Grape may press keys.
    pub access: Access,
    /// Where the wheel opens.
    pub opens_at: OpensAt,
    /// A sentence about the change that made this view, if it needs one.
    pub note: Option<String>,
    /// The limits.
    pub limits: LimitsView,
    /// Every key a chord may use, in menu order.
    pub keys: Vec<KeyView>,
    /// What this OS calls the modifiers.
    pub modifiers: ModifierNames,
    /// Every glyph id, in menu order.
    pub glyphs: &'static [&'static str],
    /// The menus.
    pub menus: Vec<MenuView>,
}

impl fmt::Debug for ShortcutsView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShortcutsView")
            .field("available", &self.available)
            .field("enabled", &self.enabled)
            .field("binding", &self.binding)
            .field("access", &self.access)
            .field("note", &self.note.is_some())
            .field("menus", &self.menus.len())
            .finish_non_exhaustive()
    }
}

/// The limits the view carries.
const LIMITS: LimitsView = LimitsView {
    items: MAX_ITEMS,
    steps: MAX_STEPS,
    name: MAX_NAME,
    icon: MAX_ICON,
    text: MAX_TEXT,
    script: MAX_SCRIPT,
    url: MAX_URL,
    menus: MAX_MENUS,
    delay_min_ms: MIN_DELAY_MS,
    delay_max_ms: MAX_DELAY_MS,
};

/// The modifiers' names, as [`Chord::label`] writes them.
const fn modifier_names(mac: bool) -> ModifierNames {
    if mac {
        ModifierNames {
            shift: "Shift",
            ctrl: "Control",
            alt: "Option",
            meta: "Command",
        }
    } else {
        ModifierNames {
            shift: "Shift",
            ctrl: "Ctrl",
            alt: "Alt",
            meta: "Win",
        }
    }
}

/// A picked file's or folder's name, for display. A root has none, so it
/// shows as it is.
fn display_name(path: &Path) -> String {
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.display().to_string(),
    }
}

fn step_view(step: &Step, mac: bool) -> StepView {
    let body = match &step.kind {
        StepKind::Keys { chord } => StepBodyView::Keys {
            chord: chord.map(ChordWire::from),
            label: chord.map(|chord| chord.label(mac)),
        },
        StepKind::TypeText { text } => StepBodyView::TypeText { text: text.clone() },
        StepKind::Clipboard { action } => StepBodyView::Clipboard { action: *action },
        StepKind::OpenApp { name, .. } => StepBodyView::OpenApp { name: name.clone() },
        StepKind::OpenFile { path, folder } => StepBodyView::OpenFile {
            name: path.as_deref().map(display_name),
            folder: *folder,
        },
        StepKind::OpenUrl { url } => StepBodyView::OpenUrl { url: url.clone() },
        StepKind::AppleShortcut { name } => StepBodyView::AppleShortcut { name: name.clone() },
        StepKind::AppleScript { source } => StepBodyView::AppleScript {
            source: source.clone(),
        },
        StepKind::Shell { source } => StepBodyView::Shell {
            source: source.clone(),
        },
        StepKind::KeyboardMaestro { name } => StepBodyView::KeyboardMaestro { name: name.clone() },
        StepKind::Window { placement } => StepBodyView::Window {
            placement: *placement,
        },
        StepKind::System { control } => StepBodyView::System { control: *control },
        StepKind::Notify { text } => StepBodyView::Notify { text: text.clone() },
        StepKind::Speak { text } => StepBodyView::Speak { text: text.clone() },
        StepKind::Delay { ms } => StepBodyView::Delay { ms: *ms },
    };
    StepView { id: step.id, body }
}

fn item_view(item: &Item, mac: bool) -> ItemView {
    ItemView {
        id: item.id,
        name: item.name.clone(),
        icon: item.icon.clone(),
        body: match &item.kind {
            ItemKind::Steps(steps) => ItemBodyView::Steps {
                steps: steps.iter().map(|step| step_view(step, mac)).collect(),
            },
            ItemKind::Group(children) => ItemBodyView::Group {
                items: children.iter().map(|child| item_view(child, mac)).collect(),
            },
        },
    }
}

/// The view of `config` on `platform`. `binding` and `access` are what the
/// OS says right now; `note` is a sentence about the change that led
/// here.
pub fn view(
    config: &Config,
    platform: Platform,
    binding: Binding,
    access: Access,
    note: Option<String>,
) -> ShortcutsView {
    let mac = matches!(platform, Platform::MacOs);
    ShortcutsView {
        available: available(platform),
        enabled: config.enabled,
        hotkey: config.hotkey.map(|chord| HotkeyView {
            chord: chord.into(),
            label: chord.label(mac),
        }),
        binding,
        access,
        opens_at: config.opens_at,
        note,
        limits: LIMITS,
        keys: KeyCode::ALL
            .iter()
            .map(|key| KeyView {
                id: key.id(),
                label: key.label(),
            })
            .collect(),
        modifiers: modifier_names(mac),
        glyphs: &GLYPHS,
        menus: config
            .menus
            .iter()
            .map(|menu| MenuView {
                id: menu.id,
                scope: match &menu.scope {
                    Scope::Everywhere => ScopeView::Everywhere,
                    Scope::App { name, .. } => ScopeView::App { name: name.clone() },
                },
                with_everywhere: menu.with_everywhere,
                items: menu.items.iter().map(|item| item_view(item, mac)).collect(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grape_platform::shortcuts::AppIdentity;
    use grape_platform::Rect;
    use serde_json::json;

    const MAC: Platform = Platform::MacOs;
    const WIN: Platform = Platform::Windows;
    /// Stands in for anything the user typed or picked.
    const MARKER: &str = "zq-marker";
    const OWN_ID: &str = "app.grape.test";

    fn golden(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn json_of<T: Serialize>(value: &T) -> Value {
        serde_json::to_value(value).unwrap()
    }

    /// What a fixture file holds: the value on one line, then a newline.
    fn line<T: Serialize>(value: &T) -> String {
        let mut text = serde_json::to_string(value).unwrap();
        text.push('\n');
        text
    }

    fn wire(key: &str, shift: bool, ctrl: bool, alt: bool, meta: bool) -> ChordWire {
        ChordWire {
            key: key.to_owned(),
            shift,
            ctrl,
            alt,
            meta,
        }
    }

    fn emoji(value: &str) -> Icon {
        Icon::Emoji {
            value: value.to_owned(),
        }
    }

    /// A config with one empty Everywhere menu, and that menu's id.
    fn empty() -> (Config, Id) {
        let config = Config::defaults(WIN);
        let id = config.menus[0].id;
        (config, id)
    }

    /// A new shortcut in `menu` with one step of `kind`: (shortcut, step).
    fn shortcut_with(config: &mut Config, menu: Id, kind: StepKindId) -> (Id, Id) {
        let item = add_item(config, menu, None, NewItem::Steps).unwrap();
        let step = add_step(config, item, kind).unwrap();
        (item, step)
    }

    fn item(config: &Config, id: Id) -> &Item {
        config
            .menus
            .iter()
            .flat_map(|menu| &menu.items)
            .flat_map(|item| {
                let children = match &item.kind {
                    ItemKind::Group(children) => children.as_slice(),
                    ItemKind::Steps(_) => &[],
                };
                std::iter::once(item).chain(children)
            })
            .find(|item| item.id == id)
            .unwrap()
    }

    fn steps_of(config: &Config, item_id: Id) -> &[Step] {
        match &item(config, item_id).kind {
            ItemKind::Steps(steps) => steps,
            ItemKind::Group(_) => panic!("a group"),
        }
    }

    fn children_of(config: &Config, item_id: Id) -> &[Item] {
        match &item(config, item_id).kind {
            ItemKind::Group(children) => children,
            ItemKind::Steps(_) => panic!("a shortcut"),
        }
    }

    fn step(config: &Config, id: Id) -> &StepKind {
        let all = config.steps();
        &all.into_iter().find(|step| step.id == id).unwrap().kind
    }

    fn names(items: &[Item]) -> Vec<&str> {
        items.iter().map(|item| item.name.as_str()).collect()
    }

    /// One plain sentence that repeats nothing typed.
    fn assert_sentence(why: &str) {
        assert!(why.ends_with('.'), "{why}");
        assert!(why.is_ascii() && !why.contains('\n'), "{why}");
        assert!(why.len() <= 80, "{why}");
        assert!(!why.contains(MARKER), "{why}");
    }

    /// Runs an edit that must be refused, checks it changed nothing, and
    /// answers with its sentence.
    fn refused<T: fmt::Debug>(
        config: &mut Config,
        edit: impl FnOnce(&mut Config) -> Result<T, &'static str>,
    ) -> &'static str {
        let before = config.clone();
        let why = edit(config).unwrap_err();
        assert!(*config == before, "a refused edit changed the config");
        assert_sentence(why);
        why
    }

    /// A config with every kind of step, a group, an app's menu and `mark`
    /// in every value a user types or picks.
    fn rich(mark: &str) -> Config {
        let (mut config, everywhere) = empty();
        let app = add_app_menu(
            &mut config,
            &format!("com.example.{mark}"),
            &format!("App {mark}"),
            OWN_ID,
        )
        .unwrap();
        set_menu_everywhere(&mut config, app, true).unwrap();

        let all = add_item(&mut config, everywhere, None, NewItem::Steps).unwrap();
        set_item(&mut config, all, &format!("n {mark}"), &emoji(mark)).unwrap();
        for kind in StepKindId::ALL {
            let id = add_step(&mut config, all, kind).unwrap();
            let edit = match kind {
                StepKindId::Keys => StepEdit::Keys {
                    chord: Some(wire("KeyK", true, false, false, true)),
                },
                StepKindId::TypeText => StepEdit::TypeText {
                    text: format!("t {mark} {{date}}\n\tend"),
                },
                StepKindId::Clipboard => StepEdit::Clipboard {
                    action: ClipboardAction::Paste,
                },
                StepKindId::OpenUrl => StepEdit::OpenUrl {
                    url: format!("https://example.com/{mark}"),
                },
                StepKindId::AppleShortcut => StepEdit::AppleShortcut {
                    name: format!("s {mark}"),
                },
                StepKindId::AppleScript => StepEdit::AppleScript {
                    source: format!("display dialog \"{mark}\""),
                },
                StepKindId::Shell => StepEdit::Shell {
                    source: format!("echo {mark}\nexit 0"),
                },
                StepKindId::KeyboardMaestro => StepEdit::KeyboardMaestro {
                    name: format!("k {mark}"),
                },
                StepKindId::Window => StepEdit::Window {
                    placement: PlacementId::TopRight,
                },
                StepKindId::System => StepEdit::System {
                    control: ControlId::VolumeUp,
                },
                StepKindId::Notify => StepEdit::Notify {
                    text: format!("n {mark}"),
                },
                StepKindId::Speak => StepEdit::Speak {
                    text: format!("say {mark}"),
                },
                StepKindId::Delay => StepEdit::Delay { ms: 250 },
                StepKindId::OpenApp => {
                    let target = Target::App {
                        path: PathBuf::from(format!("/Applications/{mark}.app")),
                        bundle_id: Some(format!("com.example.{mark}.target")),
                        name: format!("Target {mark}"),
                    };
                    set_target(&mut config, id, &target).unwrap();
                    continue;
                }
                StepKindId::OpenFile => {
                    let target = Target::File {
                        path: PathBuf::from(format!("/Users/max/{mark}/notes {mark}.txt")),
                        folder: false,
                    };
                    set_target(&mut config, id, &target).unwrap();
                    continue;
                }
            };
            set_step(&mut config, id, &edit).unwrap();
        }

        let group = add_item(&mut config, everywhere, None, NewItem::Group).unwrap();
        set_item(
            &mut config,
            group,
            &format!("g {mark}"),
            &Icon::glyph("star"),
        )
        .unwrap();
        let child = add_item(&mut config, everywhere, Some(group), NewItem::Steps).unwrap();
        set_item(&mut config, child, &format!("c {mark}"), &Icon::None).unwrap();
        let typed = add_step(&mut config, child, StepKindId::TypeText).unwrap();
        let text = format!("{mark} {{clipboard}}");
        set_step(&mut config, typed, &StepEdit::TypeText { text }).unwrap();
        add_item(&mut config, everywhere, None, NewItem::Steps).unwrap();

        let own = add_item(&mut config, app, None, NewItem::Steps).unwrap();
        set_item(&mut config, own, &format!("a {mark}"), &Icon::glyph("bolt")).unwrap();
        let folder = add_step(&mut config, own, StepKindId::OpenFile).unwrap();
        let target = Target::File {
            path: PathBuf::from(format!("/Users/max/{mark}")),
            folder: true,
        };
        set_target(&mut config, folder, &target).unwrap();
        config
    }

    fn renumbered(config: &Config) -> Config {
        let mut config = config.clone();
        config.renumber();
        config
    }

    // What a file holds, for the loader's tests.

    fn default_hotkey() -> Value {
        json!({"key": "KeyG", "shift": false, "ctrl": true, "alt": true, "meta": false})
    }

    fn file(menus: Value) -> Value {
        json!({
            "version": 1,
            "enabled": false,
            "hotkey": default_hotkey(),
            "opensAt": "cursor",
            "menus": menus,
        })
    }

    fn everywhere(items: Value) -> Value {
        json!({"scope": {"kind": "everywhere"}, "withEverywhere": false, "items": items})
    }

    fn app(id: &str, items: Value) -> Value {
        json!({
            "scope": {"kind": "app", "bundleId": id, "name": "App"},
            "withEverywhere": false,
            "items": items,
        })
    }

    fn shortcut(name: &str, steps: Value) -> Value {
        json!({"name": name, "icon": {"kind": "none"}, "kind": "steps", "steps": steps})
    }

    fn group(name: &str, items: Value) -> Value {
        json!({"name": name, "icon": {"kind": "none"}, "kind": "group", "items": items})
    }

    fn copy_step() -> Value {
        json!({"kind": "clipboard", "action": "copy"})
    }

    fn loaded(value: &Value) -> (Config, LoadReport) {
        load(&serde_json::to_vec(value).unwrap(), MAC)
    }

    /// Loads a file whose Everywhere menu holds `items`.
    fn loaded_ring(items: Value) -> (Config, LoadReport) {
        loaded(&file(json!([everywhere(items)])))
    }

    fn clean() -> LoadReport {
        LoadReport::default()
    }

    fn drops(change: impl FnOnce(&mut Drops)) -> LoadReport {
        let mut report = LoadReport::default();
        change(&mut report.drops);
        report
    }

    // ----- fixtures -----

    #[test]
    fn the_macos_view_matches_the_fixture() {
        // The starter set, before the user turned anything on.
        let view = view(
            &Config::defaults(MAC),
            MAC,
            Binding::Off,
            Access::Denied,
            None,
        );
        let fixture = include_str!("../../../ui/src/shared/fixtures/shortcuts-view-macos.json");
        assert_eq!(json_of(&view), golden(fixture));
        assert_eq!(line(&view), fixture);
    }

    #[test]
    fn the_windows_view_matches_the_fixture() {
        let view = view(
            &Config::defaults(WIN),
            WIN,
            Binding::Off,
            Access::NotNeeded,
            None,
        );
        let fixture = include_str!("../../../ui/src/shared/fixtures/shortcuts-view-windows.json");
        assert_eq!(json_of(&view), golden(fixture));
        assert_eq!(line(&view), fixture);
        assert_eq!(json_of(&view)["available"], json!(false));
        assert_eq!(
            json_of(&view)["menus"],
            json!([{"id": 1, "scope": {"kind": "everywhere"}, "withEverywhere": false, "items": []}])
        );
    }

    /// Every `shortcuts-step-*.json` and the edit it holds.
    fn step_fixtures() -> Vec<(&'static str, StepEdit)> {
        vec![
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-keys.json"),
                StepEdit::Keys {
                    chord: Some(wire("KeyK", true, false, false, true)),
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-keys-unset.json"),
                StepEdit::Keys { chord: None },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-type-text.json"),
                StepEdit::TypeText {
                    text: "Sent {date} at {time}\nMax".to_owned(),
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-clipboard.json"),
                StepEdit::Clipboard {
                    action: ClipboardAction::Paste,
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-open-url.json"),
                StepEdit::OpenUrl {
                    url: "https://example.com/inbox".to_owned(),
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-apple-shortcut.json"),
                StepEdit::AppleShortcut {
                    name: "Start focus".to_owned(),
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-apple-script.json"),
                StepEdit::AppleScript {
                    source: "tell application \"Finder\" to activate".to_owned(),
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-shell.json"),
                StepEdit::Shell {
                    source: "echo hello".to_owned(),
                },
            ),
            (
                include_str!(
                    "../../../ui/src/shared/fixtures/shortcuts-step-keyboard-maestro.json"
                ),
                StepEdit::KeyboardMaestro {
                    name: "Tidy desktop".to_owned(),
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-window.json"),
                StepEdit::Window {
                    placement: PlacementId::RightHalf,
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-system.json"),
                StepEdit::System {
                    control: ControlId::ToggleMute,
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-notify.json"),
                StepEdit::Notify {
                    text: "Break time".to_owned(),
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-speak.json"),
                StepEdit::Speak {
                    text: "Stand up".to_owned(),
                },
            ),
            (
                include_str!("../../../ui/src/shared/fixtures/shortcuts-step-delay.json"),
                StepEdit::Delay { ms: 250 },
            ),
        ]
    }

    #[test]
    fn step_edits_match_the_fixtures() {
        let fixtures = step_fixtures();
        for (fixture, edit) in &fixtures {
            let read: StepEdit = serde_json::from_str(fixture).unwrap();
            assert!(read == *edit, "{edit:?}");
            assert_eq!(line(edit), *fixture, "{edit:?}");
            // And each is a value its step takes.
            let (mut config, menu) = empty();
            let (_, step) = shortcut_with(&mut config, menu, edit.id());
            set_step(&mut config, step, edit).unwrap();
        }
        // Every kind an edit exists for has a fixture.
        for kind in StepKindId::ALL {
            let has = fixtures.iter().any(|(_, edit)| edit.id() == kind);
            let picked = matches!(kind, StepKindId::OpenApp | StepKindId::OpenFile);
            assert_eq!(has, !picked, "{kind:?}");
        }
    }

    #[test]
    fn a_step_edit_refuses_what_it_does_not_know() {
        for text in [
            r#"{"kind":"teleport"}"#,
            r#"{"kind":"delay"}"#,
            r#"{"kind":"delay","ms":250,"extra":1}"#,
            r#"{"kind":"delay","ms":-1}"#,
            r#"{"kind":"delay","ms":"250"}"#,
            r#"{"kind":"clipboard","action":"cut"}"#,
            r#"{"kind":"window","placement":"upsideDown"}"#,
            r#"{"kind":"system","control":"shutDown"}"#,
            r#"{"kind":"keys","chord":{"key":"KeyK","shift":true,"ctrl":false,"alt":false}}"#,
            r#"{"kind":"keys","chord":{"key":"KeyK","shift":true,"ctrl":false,"alt":false,"meta":true,"fn":true}}"#,
            // Targets are picked on the Rust side, never sent.
            r#"{"kind":"openApp","path":"/Applications/Safari.app","bundleId":null,"name":"Safari"}"#,
            r#"{"kind":"openFile","path":"/etc/hosts","folder":false}"#,
            r#"{"ms":250}"#,
            r#""delay""#,
        ] {
            assert!(serde_json::from_str::<StepEdit>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_command_ids_are_the_serde_ids() {
        for kind in StepKindId::ALL {
            assert_eq!(json_of(&kind), json!(kind.id()));
            assert_eq!(
                serde_json::from_value::<StepKindId>(json!(kind.id())).unwrap(),
                kind
            );
            assert_eq!(StepKind::fresh(kind).id(), kind);
            let once = StepKindId::ALL.iter().filter(|other| **other == kind);
            assert_eq!(once.count(), 1);
        }
        assert_eq!(json_of(&OpensAt::Cursor), json!("cursor"));
        assert_eq!(json_of(&OpensAt::Center), json!("center"));
        assert_eq!(json_of(&NewItem::Steps), json!("steps"));
        assert_eq!(json_of(&NewItem::Group), json!("group"));
        assert_eq!(
            json_of(&[Binding::Off, Binding::Bound, Binding::Failed]),
            json!(["off", "bound", "failed"])
        );
        assert_eq!(
            json_of(&[
                Access::from(InputAccess::Granted),
                Access::from(InputAccess::Denied),
                Access::from(InputAccess::NotNeeded),
            ]),
            json!(["granted", "denied", "notNeeded"])
        );
        assert_eq!(
            json_of(&[
                ClipboardAction::Copy,
                ClipboardAction::Paste,
                ClipboardAction::Clear
            ]),
            json!(["copy", "paste", "clear"])
        );
    }

    #[test]
    fn placements_and_controls_name_the_platforms_own() {
        let placements = [
            ("leftHalf", Placement::LeftHalf),
            ("rightHalf", Placement::RightHalf),
            ("topHalf", Placement::TopHalf),
            ("bottomHalf", Placement::BottomHalf),
            ("fill", Placement::Fill),
            ("center", Placement::Center),
            ("topLeft", Placement::TopLeft),
            ("topRight", Placement::TopRight),
            ("bottomLeft", Placement::BottomLeft),
            ("bottomRight", Placement::BottomRight),
        ];
        for (id, placement) in placements {
            let read: PlacementId = serde_json::from_value(json!(id)).unwrap();
            assert_eq!(Placement::from(read), placement, "{id}");
            assert_eq!(json_of(&read), json!(id));
        }
        let controls = [
            ("sleepDisplay", SystemControl::SleepDisplay),
            ("lockScreen", SystemControl::LockScreen),
            ("screensaver", SystemControl::Screensaver),
            ("missionControl", SystemControl::MissionControl),
            ("toggleMute", SystemControl::ToggleMute),
            ("volumeUp", SystemControl::VolumeUp),
            ("volumeDown", SystemControl::VolumeDown),
            ("toggleDarkMode", SystemControl::ToggleDarkMode),
            ("screenshotArea", SystemControl::ScreenshotArea),
        ];
        for (id, control) in controls {
            let read: ControlId = serde_json::from_value(json!(id)).unwrap();
            assert_eq!(SystemControl::from(read), control, "{id}");
            assert_eq!(json_of(&read), json!(id));
        }
    }

    // ----- defaults -----

    #[test]
    fn the_limits_are_the_ones_the_page_is_told() {
        assert_eq!(
            json_of(&LIMITS),
            json!({
                "items": 12, "steps": 16, "name": 24, "icon": 16, "text": 4000,
                "script": 20000, "url": 2048, "menus": 40, "delayMinMs": 10, "delayMaxMs": 60000,
            })
        );
        assert_eq!(MAX_FILE_BYTES, 1024 * 1024);
        assert_eq!(MAX_LOAD_BYTES, 2 * 1024 * 1024);
        // The sentences that name a limit name the right one.
        assert!(RING_FULL.contains("12"));
        assert!(STEPS_FULL.contains("16"));
        assert!(NAME_LENGTH.contains("24"));
        assert!(ICON_EMOJI.contains("16"));
        assert!(TEXT_TOO_LONG.contains("4,000"));
        assert!(SCRIPT_TOO_LONG.contains("20,000"));
        assert!(URL_TOO_LONG.contains("2,048"));
        assert!(MENUS_FULL.contains("40"));
        assert!(DELAY_RANGE.contains("10 milliseconds") && DELAY_RANGE.contains("60 seconds"));
    }

    #[test]
    fn a_mac_starts_with_the_starter_set_switched_off() {
        let config = Config::defaults(MAC);
        assert!(!config.enabled);
        assert_eq!(config.hotkey, Some(DEFAULT_HOTKEY));
        assert_eq!(DEFAULT_HOTKEY.label(true), "Control+Option+G");
        assert_eq!(DEFAULT_HOTKEY.hotkey_problem(true), None);
        assert_eq!(config.opens_at, OpensAt::Cursor);
        assert_eq!(config.menus.len(), 1);
        assert!(config.menus[0].scope == Scope::Everywhere);
        assert_eq!(
            names(config.everywhere()),
            [
                "Copy",
                "Paste",
                "Screenshot",
                "Mission Control",
                "Mute",
                "Lock screen"
            ]
        );
        // Clipboard and system steps only: right on every keyboard layout.
        for step in config.steps() {
            assert!(matches!(
                step.kind.id(),
                StepKindId::Clipboard | StepKindId::System
            ));
            assert!(step.kind.is_set());
        }
        // Numbered from 1 in reading order, as the view shows them.
        assert_eq!(config.menus[0].id, 1);
        let ids: Vec<Id> = config
            .everywhere()
            .iter()
            .flat_map(|item| [item.id, steps_of(&config, item.id)[0].id])
            .collect();
        assert_eq!(ids, (2..=13).collect::<Vec<Id>>());
        assert_eq!(config.next_id, 14);
        // It passes its own checks: what it stores loads back the same.
        let (again, report) = load(&stored_bytes(&config).unwrap(), MAC);
        assert!(again == config);
        assert_eq!(report, clean());
    }

    #[test]
    fn other_systems_start_with_an_empty_menu() {
        let config = Config::defaults(WIN);
        assert!(!config.enabled);
        assert_eq!(config.hotkey, Some(DEFAULT_HOTKEY));
        assert_eq!(config.menus.len(), 1);
        assert!(config.menus[0].scope == Scope::Everywhere);
        assert!(config.everywhere().is_empty());
        assert!(available(MAC) && !available(WIN));
        assert_sentence(UNAVAILABLE);
    }

    #[test]
    fn the_stored_form_is_one_line_in_this_order() {
        let mut config = Config::defaults(MAC);
        let ring = &mut config.menus[0].items;
        ring.truncate(1);
        let text = String::from_utf8(stored_bytes(&config).unwrap()).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"version":1,"enabled":false,"#,
                r#""hotkey":{"key":"KeyG","shift":false,"ctrl":true,"alt":true,"meta":false},"#,
                r#""opensAt":"cursor","#,
                r#""menus":[{"scope":{"kind":"everywhere"},"withEverywhere":false,"items":["#,
                r#"{"name":"Copy","icon":{"kind":"glyph","value":"clipboard"},"kind":"steps","steps":["#,
                r#"{"kind":"clipboard","action":"copy"}]}]}]}"#,
                "\n"
            )
        );
    }

    // ----- names and icons -----

    #[test]
    fn names_are_counted_in_utf16_units() {
        let (mut config, menu) = empty();
        let id = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
        let accepted = [
            "a".to_owned(),
            "a".repeat(24),
            // 12 emoji are 24 units and 48 bytes.
            "😀".repeat(12),
            "字".repeat(24),
            "é".repeat(24),
            "Tidy up".to_owned(),
        ];
        for name in &accepted {
            set_item(&mut config, id, name, &Icon::None).unwrap();
            assert_eq!(item(&config, id).name, *name);
        }
        let too_long = [
            "a".repeat(25),
            format!("{}a", "😀".repeat(12)),
            "😀".repeat(13),
            "字".repeat(25),
        ];
        for name in &too_long {
            let why = refused(&mut config, |c| set_item(c, id, name, &Icon::None));
            assert_eq!(why, NAME_LENGTH, "{} units", units(name));
        }
        for name in ["", " ", "\t\n", "\u{3000}"] {
            let why = refused(&mut config, |c| set_item(c, id, name, &Icon::None));
            assert_eq!(why, NAME_LENGTH);
        }
        for name in [
            "a\nb",
            "a\tb",
            "a\rb",
            "a\u{0}b",
            "a\u{7f}",
            "a\u{85}b",
            "a\u{2028}b",
        ] {
            let why = refused(&mut config, |c| set_item(c, id, name, &Icon::None));
            assert_eq!(why, NAME_CONTROLS);
        }
    }

    #[test]
    fn names_are_trimmed_before_they_are_counted() {
        let (mut config, menu) = empty();
        let id = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
        let padded = format!("  {}\n", "a".repeat(24));
        set_item(&mut config, id, &padded, &Icon::None).unwrap();
        assert_eq!(item(&config, id).name, "a".repeat(24));
        set_item(&mut config, id, " two words ", &Icon::None).unwrap();
        assert_eq!(item(&config, id).name, "two words");
    }

    #[test]
    fn an_emoji_icon_is_one_to_sixteen_units_with_no_spaces() {
        let (mut config, menu) = empty();
        let id = add_item(&mut config, menu, None, NewItem::Group).unwrap();
        // A family is one drawing and 11 units.
        let family = "👨\u{200d}👩\u{200d}👧\u{200d}👦";
        assert_eq!(units(family), 11);
        for value in ["a", "💾", family, &"😀".repeat(8), &"a".repeat(16), "ab"] {
            set_item(&mut config, id, "Name", &emoji(value)).unwrap();
            assert!(item(&config, id).icon == emoji(value));
        }
        set_item(&mut config, id, "Name", &emoji(" 💾\n")).unwrap();
        assert!(item(&config, id).icon == emoji("💾"));
        for value in [
            "",
            " ",
            "a b",
            "a\u{a0}b",
            "a\nb",
            "a\u{0}",
            &"a".repeat(17),
            &format!("{}a", "😀".repeat(8)),
        ] {
            let why = refused(&mut config, |c| set_item(c, id, "Other", &emoji(value)));
            assert_eq!(why, ICON_EMOJI);
        }
        // A refused icon doesn't rename either.
        assert_eq!(item(&config, id).name, "Name");
    }

    #[test]
    fn a_glyph_icon_is_one_of_grapes_own() {
        let (mut config, menu) = empty();
        let id = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
        for glyph in GLYPHS {
            set_item(&mut config, id, "Name", &Icon::glyph(glyph)).unwrap();
            assert!(item(&config, id).icon == Icon::glyph(glyph));
            assert!(
                item(&config, id).icon.to_ui()
                    == UiIcon::Glyph {
                        value: glyph.to_owned()
                    }
            );
        }
        assert!(GLYPHS.contains(&EVERYWHERE_GLYPH));
        for glyph in ["", "Key", "key ", "sparkle", MARKER] {
            let why = refused(&mut config, |c| {
                set_item(c, id, "Name", &Icon::glyph(glyph))
            });
            assert_eq!(why, ICON_GLYPH);
        }
        set_item(&mut config, id, "Name", &Icon::None).unwrap();
        assert!(item(&config, id).icon.to_ui() == UiIcon::None);
        assert!(
            emoji("💾").to_ui()
                == UiIcon::Emoji {
                    value: "💾".to_owned()
                }
        );
        let why = refused(&mut config, |c| set_item(c, 999, "Name", &Icon::None));
        assert_eq!(why, ITEM_GONE);
    }

    #[test]
    fn an_icon_reads_and_writes_as_the_overlay_does() {
        for (icon, text) in [
            (Icon::None, r#"{"kind":"none"}"#),
            (emoji("💾"), r#"{"kind":"emoji","value":"💾"}"#),
            (Icon::glyph("key"), r#"{"kind":"glyph","value":"key"}"#),
        ] {
            assert_eq!(serde_json::to_string(&icon).unwrap(), text);
            assert_eq!(serde_json::to_string(&icon.to_ui()).unwrap(), text);
            assert!(serde_json::from_str::<Icon>(text).unwrap() == icon);
        }
        for text in [
            r#"{"kind":"emoji"}"#,
            r#"{"kind":"none","value":"a"}"#,
            r#"{"kind":"emoji","value":"a","size":2}"#,
            r#"{"kind":"image","value":"a"}"#,
            r#"{"value":"a"}"#,
            r#""none""#,
        ] {
            assert!(serde_json::from_str::<Icon>(text).is_err(), "{text}");
        }
    }

    // ----- the switch, the hot key, where it opens -----

    #[test]
    fn the_wheel_turns_on_only_with_a_hot_key() {
        let (mut config, _) = empty();
        set_enabled(&mut config, true).unwrap();
        assert!(config.enabled);
        set_enabled(&mut config, false).unwrap();
        assert!(!config.enabled);
        set_hotkey(&mut config, None, MAC).unwrap();
        assert_eq!(config.hotkey, None);
        assert_eq!(refused(&mut config, |c| set_enabled(c, true)), NO_HOTKEY);
        // Off is always fine.
        set_enabled(&mut config, false).unwrap();
    }

    #[test]
    fn removing_the_hot_key_turns_the_wheel_off() {
        let (mut config, _) = empty();
        set_enabled(&mut config, true).unwrap();
        set_hotkey(&mut config, None, MAC).unwrap();
        assert!(!config.enabled && config.hotkey.is_none());
        // A new one doesn't turn it back on by itself.
        set_hotkey(
            &mut config,
            Some(&wire("F5", false, false, false, false)),
            MAC,
        )
        .unwrap();
        assert!(!config.enabled);
        assert_eq!(
            config.hotkey.map(|chord| chord.label(true)).as_deref(),
            Some("F5")
        );
        // And changing it while on keeps it on.
        set_enabled(&mut config, true).unwrap();
        set_hotkey(
            &mut config,
            Some(&wire("KeyJ", true, true, false, false)),
            MAC,
        )
        .unwrap();
        assert!(config.enabled);
        assert_eq!(
            config.hotkey.map(|chord| chord.label(true)).as_deref(),
            Some("Control+Shift+J")
        );
    }

    #[test]
    fn a_hot_key_that_cannot_be_one_is_refused() {
        let (mut config, menu) = empty();
        let cases = [
            (
                wire("KeyG", false, false, false, false),
                MAC,
                HOTKEY_NEEDS_MODIFIER_MAC,
            ),
            (
                wire("KeyG", true, false, false, false),
                MAC,
                HOTKEY_NEEDS_MODIFIER_MAC,
            ),
            (
                wire("KeyG", true, false, false, false),
                WIN,
                HOTKEY_NEEDS_MODIFIER,
            ),
            (
                wire("KeyQ", false, false, false, true),
                MAC,
                HOTKEY_RESERVED,
            ),
            (wire("KeyS", true, false, false, true), MAC, HOTKEY_RESERVED),
            (
                wire("Space", false, false, false, true),
                MAC,
                HOTKEY_RESERVED,
            ),
            (wire("keyg", false, true, false, false), MAC, UNKNOWN_KEY),
            (wire("", false, true, false, false), MAC, UNKNOWN_KEY),
            (wire(MARKER, false, true, false, false), MAC, UNKNOWN_KEY),
        ];
        for (chord, platform, expected) in &cases {
            let why = refused(&mut config, |c| set_hotkey(c, Some(chord), *platform));
            assert_eq!(why, *expected);
        }
        // Nothing is reserved off the Mac, and Control makes Command+Q fine.
        set_hotkey(
            &mut config,
            Some(&wire("KeyQ", false, false, false, true)),
            WIN,
        )
        .unwrap();
        set_hotkey(
            &mut config,
            Some(&wire("KeyQ", false, true, false, true)),
            MAC,
        )
        .unwrap();

        // A chord one of the shortcuts presses can't open the wheel too.
        let (_, step) = shortcut_with(&mut config, menu, StepKindId::Keys);
        let pressed = wire("KeyP", false, true, true, false);
        let edit = StepEdit::Keys {
            chord: Some(pressed.clone()),
        };
        set_step(&mut config, step, &edit).unwrap();
        let why = refused(&mut config, |c| set_hotkey(c, Some(&pressed), MAC));
        assert_eq!(why, HOTKEY_IN_STEP);
        // The same in a group.
        let group = add_item(&mut config, menu, None, NewItem::Group).unwrap();
        let child = add_item(&mut config, menu, Some(group), NewItem::Steps).unwrap();
        let inner = add_step(&mut config, child, StepKindId::Keys).unwrap();
        let other = wire("KeyO", false, true, true, false);
        let edit = StepEdit::Keys {
            chord: Some(other.clone()),
        };
        set_step(&mut config, inner, &edit).unwrap();
        let why = refused(&mut config, |c| set_hotkey(c, Some(&other), MAC));
        assert_eq!(why, HOTKEY_IN_STEP);
    }

    #[test]
    fn where_the_wheel_opens_is_set_as_asked() {
        let (mut config, _) = empty();
        set_opens_at(&mut config, OpensAt::Center).unwrap();
        assert_eq!(config.opens_at, OpensAt::Center);
        set_opens_at(&mut config, OpensAt::Cursor).unwrap();
        assert_eq!(config.opens_at, OpensAt::Cursor);
    }

    // ----- menus -----

    #[test]
    fn an_app_gets_one_menu() {
        let (mut config, everywhere) = empty();
        let safari = add_app_menu(&mut config, "com.apple.Safari", " Safari ", OWN_ID).unwrap();
        assert_eq!(config.menus.len(), 2);
        let menu = &config.menus[1];
        assert_eq!(menu.id, safari);
        assert!(!menu.with_everywhere && menu.items.is_empty());
        assert!(
            menu.scope
                == Scope::App {
                    bundle_id: "com.apple.Safari".to_owned(),
                    name: "Safari".to_owned(),
                }
        );
        assert_ne!(safari, everywhere);

        let again = |c: &mut Config| add_app_menu(c, "com.apple.Safari", "Other name", OWN_ID);
        assert_eq!(refused(&mut config, again), APP_HAS_MENU);
        let own = |c: &mut Config| add_app_menu(c, OWN_ID, "Grape", OWN_ID);
        assert_eq!(refused(&mut config, own), APP_IS_GRAPE);
        let long = "a".repeat(256);
        for (id, name) in [
            ("", "App"),
            ("com.example.app", ""),
            ("com.example.app", "  "),
            ("com.example\n.app", "App"),
            ("com.example.app", "A\u{0}pp"),
            (long.as_str(), "App"),
            ("com.example.app", long.as_str()),
        ] {
            let bad = |c: &mut Config| add_app_menu(c, id, name, OWN_ID);
            assert_eq!(refused(&mut config, bad), APP_UNUSABLE);
        }
        add_app_menu(&mut config, &"a".repeat(255), &"字".repeat(255), OWN_ID).unwrap();
    }

    #[test]
    fn there_are_forty_menus_at_most() {
        let (mut config, _) = empty();
        for n in 1..MAX_MENUS {
            add_app_menu(&mut config, &format!("com.example.app{n}"), "App", OWN_ID).unwrap();
        }
        assert_eq!(config.menus.len(), MAX_MENUS);
        let one_more = |c: &mut Config| add_app_menu(c, "com.example.last", "App", OWN_ID);
        assert_eq!(refused(&mut config, one_more), MENUS_FULL);
        // Room again after one goes.
        let last = config.menus[MAX_MENUS - 1].id;
        remove_menu(&mut config, last).unwrap();
        add_app_menu(&mut config, "com.example.last", "App", OWN_ID).unwrap();
    }

    #[test]
    fn an_apps_menu_can_go_but_everywhere_stays() {
        let (mut config, everywhere) = empty();
        let app = add_app_menu(&mut config, "com.example.app", "App", OWN_ID).unwrap();
        shortcut_with(&mut config, app, StepKindId::Delay);
        assert_eq!(
            refused(&mut config, |c| remove_menu(c, everywhere)),
            EVERYWHERE_STAYS
        );
        assert_eq!(refused(&mut config, |c| remove_menu(c, 999)), MENU_GONE);
        remove_menu(&mut config, app).unwrap();
        assert_eq!(config.menus.len(), 1);
        assert_eq!(refused(&mut config, |c| remove_menu(c, app)), MENU_GONE);
        // The app can have a menu again, with a new id.
        let again = add_app_menu(&mut config, "com.example.app", "App", OWN_ID).unwrap();
        assert_ne!(again, app);
    }

    #[test]
    fn the_everywhere_switch_says_when_the_group_does_not_fit() {
        let (mut config, everywhere) = empty();
        let app = add_app_menu(&mut config, "com.example.app", "App", OWN_ID).unwrap();
        for _ in 0..MAX_ITEMS - 1 {
            add_item(&mut config, app, None, NewItem::Steps).unwrap();
        }
        // Eleven and the group make twelve.
        assert_eq!(set_menu_everywhere(&mut config, app, true), Ok(None));
        assert!(config.menus[1].with_everywhere);
        assert_eq!(set_menu_everywhere(&mut config, app, false), Ok(None));
        assert!(!config.menus[1].with_everywhere);
        add_item(&mut config, app, None, NewItem::Steps).unwrap();
        // Twelve leave no room: the switch is on, with a note.
        assert_eq!(
            set_menu_everywhere(&mut config, app, true),
            Ok(Some(EVERYWHERE_DID_NOT_FIT))
        );
        assert!(config.menus[1].with_everywhere);
        assert_eq!(set_menu_everywhere(&mut config, app, false), Ok(None));
        assert_sentence(EVERYWHERE_DID_NOT_FIT);

        let on_everywhere = |c: &mut Config| set_menu_everywhere(c, everywhere, true);
        assert_eq!(refused(&mut config, on_everywhere), EVERYWHERE_SWITCH);
        let gone = |c: &mut Config| set_menu_everywhere(c, 999, true);
        assert_eq!(refused(&mut config, gone), MENU_GONE);
    }

    // ----- shortcuts and groups -----

    #[test]
    fn a_ring_holds_twelve() {
        let (mut config, menu) = empty();
        let mut ids = Vec::new();
        for n in 0..MAX_ITEMS {
            let kind = if n % 2 == 0 {
                NewItem::Steps
            } else {
                NewItem::Group
            };
            ids.push(add_item(&mut config, menu, None, kind).unwrap());
        }
        assert_eq!(config.everywhere().len(), 12);
        assert_eq!(item(&config, ids[0]).name, "New shortcut");
        assert_eq!(item(&config, ids[1]).name, "New group");
        assert!(item(&config, ids[0]).icon == Icon::None);
        assert!(steps_of(&config, ids[0]).is_empty());
        assert!(children_of(&config, ids[1]).is_empty());
        for kind in [NewItem::Steps, NewItem::Group] {
            let why = refused(&mut config, |c| add_item(c, menu, None, kind));
            assert_eq!(why, RING_FULL);
        }
        // A full ring's groups still take shortcuts, twelve each.
        let group = ids[1];
        for _ in 0..MAX_ITEMS {
            add_item(&mut config, menu, Some(group), NewItem::Steps).unwrap();
        }
        assert_eq!(children_of(&config, group).len(), 12);
        let why = refused(&mut config, |c| {
            add_item(c, menu, Some(group), NewItem::Steps)
        });
        assert_eq!(why, RING_FULL);
        // Every id is its own.
        let mut all: Vec<Id> = ids.clone();
        all.extend(children_of(&config, group).iter().map(|child| child.id));
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 24);
    }

    #[test]
    fn a_group_holds_shortcuts_only() {
        let (mut config, menu) = empty();
        let group = add_item(&mut config, menu, None, NewItem::Group).unwrap();
        let shortcut = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
        let child = add_item(&mut config, menu, Some(group), NewItem::Steps).unwrap();
        assert_eq!(names(children_of(&config, group)), ["New shortcut"]);

        let nested = |c: &mut Config| add_item(c, menu, Some(group), NewItem::Group);
        assert_eq!(refused(&mut config, nested), GROUP_IN_GROUP);
        // Not a group, a group's own shortcut, another menu's group, nothing.
        let app = add_app_menu(&mut config, "com.example.app", "App", OWN_ID).unwrap();
        for (in_menu, in_group) in [(menu, shortcut), (menu, child), (app, group), (menu, 999)] {
            let lost = |c: &mut Config| add_item(c, in_menu, Some(in_group), NewItem::Steps);
            assert_eq!(refused(&mut config, lost), GROUP_GONE);
        }
        let no_menu = |c: &mut Config| add_item(c, 999, None, NewItem::Steps);
        assert_eq!(refused(&mut config, no_menu), MENU_GONE);

        // Steps belong to shortcuts.
        let on_group = |c: &mut Config| add_step(c, group, StepKindId::Delay);
        assert_eq!(refused(&mut config, on_group), GROUP_NO_STEPS);
        add_step(&mut config, child, StepKindId::Delay).unwrap();
    }

    #[test]
    fn removing_a_group_takes_its_shortcuts() {
        let (mut config, menu) = empty();
        let group = add_item(&mut config, menu, None, NewItem::Group).unwrap();
        let child = add_item(&mut config, menu, Some(group), NewItem::Steps).unwrap();
        let other = add_item(&mut config, menu, Some(group), NewItem::Steps).unwrap();
        let (kept, _) = shortcut_with(&mut config, menu, StepKindId::Delay);
        remove_item(&mut config, child).unwrap();
        assert_eq!(children_of(&config, group).len(), 1);
        assert_eq!(refused(&mut config, |c| remove_item(c, child)), ITEM_GONE);
        remove_item(&mut config, group).unwrap();
        assert_eq!(config.everywhere().len(), 1);
        assert_eq!(config.everywhere()[0].id, kept);
        assert_eq!(refused(&mut config, |c| remove_item(c, other)), ITEM_GONE);
        assert_eq!(refused(&mut config, |c| remove_item(c, 999)), ITEM_GONE);
    }

    #[test]
    fn a_move_is_a_remove_and_an_insert_in_its_own_ring() {
        for count in 1..=5_usize {
            for from in 0..count {
                for to in 0..count {
                    let (mut config, menu) = empty();
                    let group = add_item(&mut config, menu, None, NewItem::Group).unwrap();
                    let mut top = vec![group];
                    let mut inner = Vec::new();
                    for _ in 0..count {
                        inner.push(
                            add_item(&mut config, menu, Some(group), NewItem::Steps).unwrap(),
                        );
                    }
                    for _ in 1..count {
                        top.push(add_item(&mut config, menu, None, NewItem::Steps).unwrap());
                    }
                    let ids = |items: &[Item]| items.iter().map(|i| i.id).collect::<Vec<_>>();

                    move_item(&mut config, top[from], to).unwrap();
                    let moved = top.remove(from);
                    top.insert(to, moved);
                    assert_eq!(ids(config.everywhere()), top);
                    // The group's own ring is untouched, and moves alone.
                    assert_eq!(ids(children_of(&config, group)), inner);
                    move_item(&mut config, inner[from], to).unwrap();
                    let moved = inner.remove(from);
                    inner.insert(to, moved);
                    assert_eq!(ids(children_of(&config, group)), inner);
                    assert_eq!(ids(config.everywhere()), top);

                    let past = |c: &mut Config| move_item(c, top[0], count);
                    assert_eq!(refused(&mut config, past), CANT_MOVE);
                    let past = |c: &mut Config| move_item(c, inner[0], count);
                    assert_eq!(refused(&mut config, past), CANT_MOVE);
                }
            }
        }
        let (mut config, _) = empty();
        assert_eq!(refused(&mut config, |c| move_item(c, 999, 0)), ITEM_GONE);
    }

    // ----- steps -----

    #[test]
    fn a_shortcut_holds_sixteen_steps() {
        let (mut config, menu) = empty();
        let shortcut = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
        for n in 0..MAX_STEPS {
            let kind = StepKindId::ALL[n % StepKindId::ALL.len()];
            let id = add_step(&mut config, shortcut, kind).unwrap();
            assert_eq!(steps_of(&config, shortcut)[n].id, id);
            assert!(*step(&config, id) == StepKind::fresh(kind));
        }
        let more = |c: &mut Config| add_step(c, shortcut, StepKindId::Delay);
        assert_eq!(refused(&mut config, more), STEPS_FULL);
        let gone = |c: &mut Config| add_step(c, 999, StepKindId::Delay);
        assert_eq!(refused(&mut config, gone), ITEM_GONE);
        let first = steps_of(&config, shortcut)[0].id;
        remove_step(&mut config, first).unwrap();
        add_step(&mut config, shortcut, StepKindId::Delay).unwrap();
        assert_eq!(refused(&mut config, |c| remove_step(c, first)), STEP_GONE);
    }

    #[test]
    fn a_new_step_has_nothing_chosen() {
        let unset = [
            StepKindId::Keys,
            StepKindId::TypeText,
            StepKindId::OpenApp,
            StepKindId::OpenFile,
            StepKindId::OpenUrl,
            StepKindId::AppleShortcut,
            StepKindId::AppleScript,
            StepKindId::Shell,
            StepKindId::KeyboardMaestro,
            StepKindId::Notify,
            StepKindId::Speak,
        ];
        for kind in StepKindId::ALL {
            let fresh = StepKind::fresh(kind);
            assert_eq!(fresh.is_set(), !unset.contains(&kind), "{kind:?}");
            // And it's a stored state: it loads back as it is.
            let (mut config, menu) = empty();
            shortcut_with(&mut config, menu, kind);
            let (again, report) = load(&stored_bytes(&config).unwrap(), MAC);
            assert!(again == renumbered(&config), "{kind:?}");
            assert_eq!(report, clean(), "{kind:?}");
        }
        assert!(
            matches!(StepKind::fresh(StepKindId::Delay), StepKind::Delay { ms } if checked_delay(ms).is_ok())
        );
        assert!(!runnable(&[]));
        let fresh = |id, kind| Step {
            id,
            kind: StepKind::fresh(kind),
        };
        assert!(!runnable(&[
            fresh(1, StepKindId::Keys),
            fresh(2, StepKindId::Shell)
        ]));
        assert!(runnable(&[
            fresh(1, StepKindId::Keys),
            fresh(2, StepKindId::Delay)
        ]));
    }

    #[test]
    fn a_step_takes_only_an_edit_of_its_own_kind() {
        let fixtures = step_fixtures();
        for kind in StepKindId::ALL {
            let (mut config, menu) = empty();
            let (_, id) = shortcut_with(&mut config, menu, kind);
            for (_, edit) in &fixtures {
                if edit.id() == kind {
                    set_step(&mut config, id, edit).unwrap();
                } else {
                    let why = refused(&mut config, |c| set_step(c, id, edit));
                    assert_eq!(why, KIND_MISMATCH, "{kind:?} with {edit:?}");
                }
            }
        }
        let (mut config, _) = empty();
        let edit = StepEdit::Delay { ms: 250 };
        assert_eq!(refused(&mut config, |c| set_step(c, 999, &edit)), STEP_GONE);
    }

    /// A step of `edit`'s kind, and what `edit` stores in it or why not.
    fn edited(edit: &StepEdit) -> Result<StepKind, &'static str> {
        let (mut config, menu) = empty();
        let (_, id) = shortcut_with(&mut config, menu, edit.id());
        match set_step(&mut config, id, edit) {
            Ok(()) => Ok(step(&config, id).clone()),
            Err(_) => Err(refused(&mut config, |c| set_step(c, id, edit))),
        }
    }

    #[test]
    fn text_is_at_most_four_thousand_units() {
        let texts = |text: &str| {
            [
                StepEdit::TypeText {
                    text: text.to_owned(),
                },
                StepEdit::Notify {
                    text: text.to_owned(),
                },
                StepEdit::Speak {
                    text: text.to_owned(),
                },
            ]
        };
        for text in [
            "a".repeat(4_000),
            "😀".repeat(2_000),
            "字".repeat(4_000),
            String::new(),
        ] {
            for edit in texts(&text) {
                assert!(edited(&edit).is_ok(), "{} units", units(&text));
            }
        }
        for text in [
            "a".repeat(4_001),
            format!("{}a", "😀".repeat(2_000)),
            "😀".repeat(2_001),
        ] {
            for edit in texts(&text) {
                assert_eq!(edited(&edit), Err(TEXT_TOO_LONG));
            }
        }
        // A Windows line break is one unit once stored.
        let lines = "a\r\n".repeat(2_000);
        let stored = edited(&StepEdit::TypeText { text: lines }).unwrap();
        assert!(
            stored
                == StepKind::TypeText {
                    text: "a\n".repeat(2_000)
                }
        );
    }

    #[test]
    fn text_keeps_line_breaks_and_tabs_and_no_other_control() {
        let stored = edited(&StepEdit::TypeText {
            text: "a\tb\nc\r\nd {x} é 😀\u{2028}".to_owned(),
        });
        assert!(
            stored
                == Ok(StepKind::TypeText {
                    text: "a\tb\nc\nd {x} é 😀\u{2028}".to_owned()
                })
        );
        for bad in [
            "a\rb",
            "a\u{0}b",
            "\u{1b}[0m",
            "a\u{7f}",
            "a\u{85}b",
            "a\u{8}",
            "a\r",
        ] {
            for edit in [
                StepEdit::TypeText {
                    text: bad.to_owned(),
                },
                StepEdit::Notify {
                    text: bad.to_owned(),
                },
                StepEdit::Speak {
                    text: bad.to_owned(),
                },
                StepEdit::AppleScript {
                    source: bad.to_owned(),
                },
                StepEdit::Shell {
                    source: bad.to_owned(),
                },
            ] {
                assert_eq!(edited(&edit), Err(TEXT_CONTROLS), "{bad:?}");
            }
        }
    }

    #[test]
    fn a_script_is_at_most_twenty_thousand_units() {
        let scripts = |source: &str| {
            [
                StepEdit::AppleScript {
                    source: source.to_owned(),
                },
                StepEdit::Shell {
                    source: source.to_owned(),
                },
            ]
        };
        for source in [
            "a".repeat(20_000),
            "😀".repeat(10_000),
            "echo hi\n\tdone\r\n".to_owned(),
        ] {
            for edit in scripts(&source) {
                assert!(edited(&edit).is_ok());
            }
        }
        for source in ["a".repeat(20_001), "😀".repeat(10_001)] {
            for edit in scripts(&source) {
                assert_eq!(edited(&edit), Err(SCRIPT_TOO_LONG));
            }
        }
        let stored = edited(&StepEdit::Shell {
            source: "a\r\nb".to_owned(),
        });
        assert!(
            stored
                == Ok(StepKind::Shell {
                    source: "a\nb".to_owned()
                })
        );
    }

    fn url(text: &str) -> Result<StepKind, &'static str> {
        edited(&StepEdit::OpenUrl {
            url: text.to_owned(),
        })
    }

    #[test]
    fn a_link_has_a_scheme_and_no_spaces() {
        let longest_scheme = format!("a{}:x", "b".repeat(31));
        for good in [
            "https://example.com/a?b=c#d",
            "http://localhost:8080",
            "mailto:max@example.com",
            "x:y",
            "a+b.c-d:e",
            "h2://x",
            "obsidian://open?vault=Notes",
            "tel:+15550100",
            "https://example.com/字/😀",
            "https://example.com/%20",
            "datax:1",
            "files://x",
            longest_scheme.as_str(),
        ] {
            assert!(
                url(good)
                    == Ok(StepKind::OpenUrl {
                        url: good.to_owned()
                    }),
                "{good}"
            );
        }
        // None yet, and what's around a link is trimmed.
        assert!(url("") == Ok(StepKind::OpenUrl { url: String::new() }));
        assert!(url("   ") == Ok(StepKind::OpenUrl { url: String::new() }));
        assert!(
            url(" https://example.com\n")
                == Ok(StepKind::OpenUrl {
                    url: "https://example.com".to_owned()
                })
        );
        let too_long_scheme = format!("a{}:x", "b".repeat(32));
        for bad in [
            "example.com",
            "https",
            "https:",
            ":x",
            "HTTPS://example.com",
            "Https://example.com",
            "1http://x",
            "+a:x",
            "a_b:x",
            "a b:x",
            "https://example.com/a b",
            "https://example.com/a\tb",
            "https://example.com/a\nb",
            "https://example.com/\u{0}",
            "https://example.com/\u{7f}",
            "https://example.com/\u{9f}",
            "https://exam\u{85}ple.com",
            "/Users/max/notes.txt",
            too_long_scheme.as_str(),
        ] {
            assert_eq!(url(bad), Err(URL_SHAPE), "{bad:?}");
        }
        for refused in [
            "file:///etc/hosts",
            "file:x",
            "javascript:alert(1)",
            "data:text/html,hi",
        ] {
            assert_eq!(url(refused), Err(URL_SCHEME), "{refused}");
        }
    }

    #[test]
    fn a_link_is_at_most_2048_units() {
        let base = "https://example.com/";
        let fits = format!("{base}{}", "a".repeat(MAX_URL - base.len()));
        assert_eq!(units(&fits), 2_048);
        assert!(url(&fits).is_ok());
        assert_eq!(url(&format!("{fits}a")), Err(URL_TOO_LONG));
        // Counted in units: 1 014 emoji are 2 028 of them.
        let emoji_fits = format!("{base}{}", "😀".repeat(1_014));
        assert_eq!(units(&emoji_fits), 2_048);
        assert!(url(&emoji_fits).is_ok());
        assert_eq!(
            url(&format!("{base}{}", "😀".repeat(1_015))),
            Err(URL_TOO_LONG)
        );
        // Trimmed first.
        assert!(url(&format!("  {fits}  ")).is_ok());
    }

    #[test]
    fn a_shortcut_or_macro_name_cannot_look_like_an_option() {
        let named = |name: &str| {
            [
                StepEdit::AppleShortcut {
                    name: name.to_owned(),
                },
                StepEdit::KeyboardMaestro {
                    name: name.to_owned(),
                },
            ]
        };
        for good in ["Start focus", "a-b", "字 😀", "x", ""] {
            for edit in named(good) {
                assert!(edited(&edit).is_ok(), "{good}");
            }
        }
        let [stored, _] = named("  Start focus \n").map(|edit| edited(&edit));
        assert!(
            stored
                == Ok(StepKind::AppleShortcut {
                    name: "Start focus".to_owned()
                })
        );
        for bad in [
            "-v",
            "--help",
            " -x",
            "a\nb",
            "a\tb",
            "a\u{0}",
            "a\u{2029}b",
        ] {
            for edit in named(bad) {
                assert_eq!(edited(&edit), Err(TARGET_NAME), "{bad:?}");
            }
        }
        for edit in named(&"a".repeat(4_000)) {
            assert!(edited(&edit).is_ok());
        }
        for edit in named(&"a".repeat(4_001)) {
            assert_eq!(edited(&edit), Err(TEXT_TOO_LONG));
        }
    }

    #[test]
    fn a_delay_is_ten_milliseconds_to_a_minute() {
        for ms in [10, 11, 250, 59_999, 60_000] {
            assert!(edited(&StepEdit::Delay { ms }) == Ok(StepKind::Delay { ms }));
        }
        for ms in [0, 1, 9, 60_001, u32::MAX] {
            assert_eq!(edited(&StepEdit::Delay { ms }), Err(DELAY_RANGE));
        }
    }

    #[test]
    fn a_keys_step_takes_any_chord_but_the_wheels_own() {
        let keys = |chord| StepEdit::Keys { chord: Some(chord) };
        // No modifier, Shift alone and a reserved chord are all fine here.
        for chord in [
            wire("KeyA", false, false, false, false),
            wire("KeyA", true, false, false, false),
            wire("KeyQ", false, false, false, true),
            wire("Escape", false, false, false, false),
            wire("F20", true, true, true, true),
        ] {
            let stored = edited(&keys(chord.clone())).unwrap();
            assert!(
                stored
                    == StepKind::Keys {
                        chord: chord.chord()
                    }
            );
            assert!(stored.is_set());
        }
        assert!(edited(&StepEdit::Keys { chord: None }) == Ok(StepKind::Keys { chord: None }));
        assert_eq!(edited(&keys(DEFAULT_HOTKEY.into())), Err(KEYS_IS_HOTKEY));
        assert_eq!(
            edited(&keys(wire("KeyAA", false, false, false, false))),
            Err(UNKNOWN_KEY)
        );
        assert_eq!(
            edited(&keys(wire(MARKER, false, false, false, false))),
            Err(UNKNOWN_KEY)
        );

        // The same key with other modifiers isn't the hot key, and with no
        // hot key set nothing is.
        let near = wire("KeyG", true, true, true, false);
        assert!(edited(&keys(near)).is_ok());
        let (mut config, menu) = empty();
        set_hotkey(&mut config, None, MAC).unwrap();
        let (_, id) = shortcut_with(&mut config, menu, StepKindId::Keys);
        set_step(&mut config, id, &keys(DEFAULT_HOTKEY.into())).unwrap();
        // Which then can't become the hot key again.
        let back = ChordWire::from(DEFAULT_HOTKEY);
        let why = refused(&mut config, |c| set_hotkey(c, Some(&back), MAC));
        assert_eq!(why, HOTKEY_IN_STEP);
    }

    #[test]
    fn a_chord_goes_to_the_wire_and_back() {
        for key in KeyCode::ALL {
            for chord in (Chord {
                key: *key,
                mods: Modifiers {
                    shift: true,
                    ctrl: true,
                    alt: true,
                    meta: true,
                },
            })
            .with_modifier_subsets()
            {
                assert_eq!(ChordWire::from(chord).chord(), Some(chord));
            }
        }
        let text = r#"{"key":"KeyG","shift":false,"ctrl":true,"alt":true,"meta":false}"#;
        assert_eq!(
            serde_json::to_string(&ChordWire::from(DEFAULT_HOTKEY)).unwrap(),
            text
        );
        let read: ChordWire = serde_json::from_str(text).unwrap();
        assert_eq!(read.chord(), Some(DEFAULT_HOTKEY));
    }

    #[test]
    fn steps_move_inside_their_own_shortcut() {
        for count in 1..=4_usize {
            for from in 0..count {
                for to in 0..count {
                    let (mut config, menu) = empty();
                    let shortcut = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
                    let (_, other) = shortcut_with(&mut config, menu, StepKindId::Delay);
                    let mut ids: Vec<Id> = (0..count)
                        .map(|_| add_step(&mut config, shortcut, StepKindId::Delay).unwrap())
                        .collect();
                    move_step(&mut config, ids[from], to).unwrap();
                    let moved = ids.remove(from);
                    ids.insert(to, moved);
                    let now: Vec<Id> = steps_of(&config, shortcut).iter().map(|s| s.id).collect();
                    assert_eq!(now, ids);
                    let past = |c: &mut Config| move_step(c, ids[0], count);
                    assert_eq!(refused(&mut config, past), CANT_MOVE);
                    // The other shortcut's one step has one place.
                    assert_eq!(refused(&mut config, |c| move_step(c, other, 1)), CANT_MOVE);
                    move_step(&mut config, other, 0).unwrap();
                }
            }
        }
        let (mut config, _) = empty();
        assert_eq!(refused(&mut config, |c| move_step(c, 999, 0)), STEP_GONE);
    }

    // ----- picked targets -----

    fn app_target(path: &str, bundle_id: Option<&str>, name: &str) -> Target {
        Target::App {
            path: PathBuf::from(path),
            bundle_id: bundle_id.map(str::to_owned),
            name: name.to_owned(),
        }
    }

    fn file_target(path: &str, folder: bool) -> Target {
        Target::File {
            path: PathBuf::from(path),
            folder,
        }
    }

    #[test]
    fn a_pick_sets_an_open_steps_target() {
        let (mut config, menu) = empty();
        let (_, app) = shortcut_with(&mut config, menu, StepKindId::OpenApp);
        let (_, file) = shortcut_with(&mut config, menu, StepKindId::OpenFile);
        let (_, delay) = shortcut_with(&mut config, menu, StepKindId::Delay);

        let safari = app_target(
            "/Applications/Safari.app",
            Some("com.apple.Safari"),
            " Safari ",
        );
        set_target(&mut config, app, &safari).unwrap();
        assert!(
            *step(&config, app)
                == StepKind::OpenApp {
                    path: Some(PathBuf::from("/Applications/Safari.app")),
                    bundle_id: Some("com.apple.Safari".to_owned()),
                    name: Some("Safari".to_owned()),
                }
        );
        assert!(step(&config, app).is_set());
        // An app with no id opens by its path alone.
        set_target(
            &mut config,
            app,
            &app_target("/Applications/Old.app", None, "Old"),
        )
        .unwrap();
        assert!(matches!(
            step(&config, app),
            StepKind::OpenApp {
                bundle_id: None,
                ..
            }
        ));

        for (path, folder) in [
            ("/Users/max/notes.txt", false),
            ("/Users/max/Invoices", true),
            ("/", true),
        ] {
            set_target(&mut config, file, &file_target(path, folder)).unwrap();
            assert!(
                *step(&config, file)
                    == StepKind::OpenFile {
                        path: Some(PathBuf::from(path)),
                        folder,
                    }
            );
        }
        for path in [
            r"C:\Users\max\notes.txt",
            r"\\server\share\notes.txt",
            "D:/notes.txt",
        ] {
            set_target(&mut config, file, &file_target(path, false)).unwrap();
        }

        // The wrong kind of pick, or of step.
        let why = refused(&mut config, |c| {
            set_target(c, app, &file_target("/a", false))
        });
        assert_eq!(why, KIND_MISMATCH);
        let why = refused(&mut config, |c| set_target(c, file, &safari));
        assert_eq!(why, KIND_MISMATCH);
        let why = refused(&mut config, |c| set_target(c, delay, &safari));
        assert_eq!(why, KIND_MISMATCH);
        let why = refused(&mut config, |c| set_target(c, 999, &safari));
        assert_eq!(why, STEP_GONE);
    }

    #[test]
    fn each_open_step_gets_its_own_picker() {
        let (mut config, menu) = empty();
        let (_, app) = shortcut_with(&mut config, menu, StepKindId::OpenApp);
        let (_, file) = shortcut_with(&mut config, menu, StepKindId::OpenFile);
        let group = add_item(&mut config, menu, None, NewItem::Group).unwrap();
        let inner = add_item(&mut config, menu, Some(group), NewItem::Steps).unwrap();
        let nested = add_step(&mut config, inner, StepKindId::OpenFile).unwrap();
        assert_eq!(pick_kind(&config, app), Ok(PickKind::App));
        assert_eq!(pick_kind(&config, file), Ok(PickKind::FileOrFolder));
        assert_eq!(pick_kind(&config, nested), Ok(PickKind::FileOrFolder));
        assert_eq!(pick_kind(&config, u64::MAX), Err(STEP_GONE));
        let others = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
        for kind in StepKindId::ALL {
            if matches!(kind, StepKindId::OpenApp | StepKindId::OpenFile) {
                continue;
            }
            let step = add_step(&mut config, others, kind).unwrap();
            assert_eq!(pick_kind(&config, step), Err(KIND_MISMATCH), "{kind:?}");
        }
    }

    #[test]
    fn a_target_grape_cannot_keep_is_refused() {
        let (mut config, menu) = empty();
        let (_, app) = shortcut_with(&mut config, menu, StepKindId::OpenApp);
        let (_, file) = shortcut_with(&mut config, menu, StepKindId::OpenFile);
        let long = "a".repeat(256);
        for target in [
            app_target("Applications/Safari.app", None, "Safari"),
            app_target("", None, "Safari"),
            app_target("/Applications/Safari.app", None, ""),
            app_target("/Applications/Safari.app", None, "Saf\nari"),
            app_target("/Applications/Safari.app", Some(""), "Safari"),
            app_target("/Applications/Safari.app", Some("com.\u{0}"), "Safari"),
            app_target("/Applications/Safari.app", Some(&long), "Safari"),
            app_target("/Applications/Safari.app", None, &long),
            app_target("/Applications/Saf\nari.app", None, "Safari"),
        ] {
            let why = refused(&mut config, |c| set_target(c, app, &target));
            assert_eq!(why, TARGET_UNUSABLE);
        }
        let too_long = format!("/{}", "a".repeat(MAX_PATH));
        for path in [
            "notes.txt",
            "",
            "./notes.txt",
            "~/notes.txt",
            "C:notes.txt",
            "/a\u{0}b",
            "/a\nb",
            &too_long,
        ] {
            let why = refused(&mut config, |c| {
                set_target(c, file, &file_target(path, false))
            });
            assert_eq!(why, TARGET_UNUSABLE, "{path:?}");
        }
        let fits = format!("/{}", "a".repeat(MAX_PATH - 1));
        set_target(&mut config, file, &file_target(&fits, false)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_text_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let (mut config, menu) = empty();
        let (_, file) = shortcut_with(&mut config, menu, StepKindId::OpenFile);
        let (_, app) = shortcut_with(&mut config, menu, StepKindId::OpenApp);
        let path = PathBuf::from(OsStr::from_bytes(b"/Users/max/\xff.txt"));
        let target = Target::File {
            path: path.clone(),
            folder: false,
        };
        assert_eq!(
            refused(&mut config, |c| set_target(c, file, &target)),
            TARGET_UNUSABLE
        );
        let target = Target::App {
            path,
            bundle_id: None,
            name: "App".to_owned(),
        };
        assert_eq!(
            refused(&mut config, |c| set_target(c, app, &target)),
            TARGET_UNUSABLE
        );
    }

    #[test]
    fn a_pickers_answer_becomes_a_target() {
        let picked = Picked {
            path: PathBuf::from("/Applications/Safari.app"),
            folder: true,
            app: Some(AppIdentity {
                id: "com.apple.Safari".to_owned(),
                name: "Safari".to_owned(),
            }),
        };
        assert!(
            Target::picked(PickKind::App, picked)
                == app_target(
                    "/Applications/Safari.app",
                    Some("com.apple.Safari"),
                    "Safari"
                )
        );
        // An app that couldn't be identified is named after its file.
        let picked = Picked {
            path: PathBuf::from("/Applications/Old Tool.app"),
            folder: true,
            app: None,
        };
        assert!(
            Target::picked(PickKind::App, picked)
                == app_target("/Applications/Old Tool.app", None, "Old Tool")
        );
        let picked = Picked {
            path: PathBuf::from("/Users/max/Invoices"),
            folder: true,
            app: None,
        };
        assert!(
            Target::picked(PickKind::FileOrFolder, picked)
                == file_target("/Users/max/Invoices", true)
        );
    }

    // ----- the ring for the app in front -----

    /// The names in a ring, a group as `name[child, …]`.
    fn ring_names(ring: &[RingItem]) -> Vec<String> {
        ring.iter()
            .map(|item| match &item.kind {
                RingKind::Steps(_) => item.name.clone(),
                RingKind::Group(children) => {
                    format!("{}[{}]", item.name, ring_names(children).join(","))
                }
            })
            .collect()
    }

    /// Everywhere holds E1, a group EG with E2 in it, and E3; Safari's
    /// menu holds S1 and a group SG with S2.
    fn two_menus() -> (Config, Id) {
        let (mut config, everywhere) = empty();
        let named = |config: &mut Config, menu, group, kind, name: &str| {
            let id = add_item(config, menu, group, kind).unwrap();
            set_item(config, id, name, &Icon::None).unwrap();
            id
        };
        let e1 = named(&mut config, everywhere, None, NewItem::Steps, "E1");
        add_step(&mut config, e1, StepKindId::Delay).unwrap();
        let eg = named(&mut config, everywhere, None, NewItem::Group, "EG");
        named(&mut config, everywhere, Some(eg), NewItem::Steps, "E2");
        named(&mut config, everywhere, None, NewItem::Steps, "E3");
        let safari = add_app_menu(&mut config, "com.apple.Safari", "Safari", OWN_ID).unwrap();
        let s1 = named(&mut config, safari, None, NewItem::Steps, "S1");
        add_step(&mut config, s1, StepKindId::Clipboard).unwrap();
        let sg = named(&mut config, safari, None, NewItem::Group, "SG");
        named(&mut config, safari, Some(sg), NewItem::Steps, "S2");
        (config, safari)
    }

    #[test]
    fn the_menu_is_the_front_apps_or_everywhere() {
        let (config, _) = two_menus();
        let snapshot = Snapshot::of(&config);
        let everywhere = ["E1", "EG[E2]", "E3"];
        assert_eq!(ring_names(&menu_for(&snapshot, None)), everywhere);
        assert_eq!(
            ring_names(&menu_for(&snapshot, Some("com.apple.Notes"))),
            everywhere
        );
        assert_eq!(ring_names(&menu_for(&snapshot, Some(""))), everywhere);
        // An id is matched whole and as written.
        assert_eq!(
            ring_names(&menu_for(&snapshot, Some("com.apple.Safari.helper"))),
            everywhere
        );
        assert_eq!(
            ring_names(&menu_for(&snapshot, Some("com.apple.safari"))),
            everywhere
        );
        let own = menu_for(&snapshot, Some("com.apple.Safari"));
        assert_eq!(ring_names(&own), ["S1", "SG[S2]"]);
        // A segment carries its steps to run.
        let RingKind::Steps(steps) = &own[0].kind else {
            panic!("a group");
        };
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].kind.id(), StepKindId::Clipboard);
        assert!(!own[0].is_group() && own[1].is_group());
    }

    #[test]
    fn an_apps_menu_can_end_with_the_everywhere_group() {
        let (mut config, safari) = two_menus();
        set_menu_everywhere(&mut config, safari, true).unwrap();
        let ring = menu_for(&Snapshot::of(&config), Some("com.apple.Safari"));
        // Everywhere's own group is left out: two levels only.
        assert_eq!(ring_names(&ring), ["S1", "SG[S2]", "Everywhere[E1,E3]"]);
        let last = &ring[2];
        assert_eq!(last.name, EVERYWHERE_NAME);
        assert!(last.icon == Icon::glyph("stack"));
        // The switch does nothing to the Everywhere ring itself.
        let ring = menu_for(&Snapshot::of(&config), None);
        assert_eq!(ring_names(&ring), ["E1", "EG[E2]", "E3"]);
    }

    #[test]
    fn the_everywhere_group_is_left_out_of_a_full_ring() {
        let (mut config, safari) = two_menus();
        set_menu_everywhere(&mut config, safari, true).unwrap();
        while config.menus[1].items.len() < MAX_ITEMS - 1 {
            add_item(&mut config, safari, None, NewItem::Steps).unwrap();
        }
        // Eleven of its own: the group is the twelfth.
        let ring = menu_for(&Snapshot::of(&config), Some("com.apple.Safari"));
        assert_eq!(ring.len(), 12);
        assert_eq!(ring[11].name, EVERYWHERE_NAME);
        add_item(&mut config, safari, None, NewItem::Steps).unwrap();
        let ring = menu_for(&Snapshot::of(&config), Some("com.apple.Safari"));
        assert_eq!(ring.len(), 12);
        assert!(ring.iter().all(|item| item.name != EVERYWHERE_NAME));
    }

    #[test]
    fn the_everywhere_group_holds_twelve_at_most() {
        let (mut config, everywhere) = empty();
        for n in 0..MAX_ITEMS {
            let kind = if n == 0 {
                NewItem::Group
            } else {
                NewItem::Steps
            };
            add_item(&mut config, everywhere, None, kind).unwrap();
        }
        let app = add_app_menu(&mut config, "com.example.app", "App", OWN_ID).unwrap();
        set_menu_everywhere(&mut config, app, true).unwrap();
        let ring = menu_for(&Snapshot::of(&config), Some("com.example.app"));
        // An empty menu of its own shows the group alone.
        assert_eq!(ring.len(), 1);
        let RingKind::Group(children) = &ring[0].kind else {
            panic!("a shortcut");
        };
        assert_eq!(children.len(), 11);
        assert!(children.iter().all(|child| !child.is_group()));
    }

    #[test]
    fn a_snapshot_is_a_copy() {
        let (mut config, safari) = two_menus();
        set_enabled(&mut config, true).unwrap();
        set_opens_at(&mut config, OpensAt::Center).unwrap();
        let snapshot = Snapshot::of(&config);
        assert!(snapshot.enabled);
        assert_eq!(snapshot.opens_at, OpensAt::Center);
        assert_eq!(snapshot.hotkey, Some(DEFAULT_HOTKEY));
        assert!(snapshot.menus == config.menus);
        remove_menu(&mut config, safari).unwrap();
        assert_eq!(snapshot.menus.len(), 2);
        assert_eq!(
            ring_names(&menu_for(&snapshot, Some("com.apple.Safari"))),
            ["S1", "SG[S2]"]
        );
    }

    // ----- text variables -----

    #[test]
    fn variables_are_filled_in_once() {
        let date = Some("2026-11-23");
        let time = Some("14:05");
        let cases = [
            ("", None, ""),
            ("plain", None, "plain"),
            ("{date}", None, "2026-11-23"),
            ("{time}", None, "14:05"),
            ("at {time} on {date}.", None, "at 14:05 on 2026-11-23."),
            ("{date}{date}", None, "2026-11-232026-11-23"),
            ("[{clipboard}]", Some("copied"), "[copied]"),
            ("[{clipboard}]", None, "[]"),
            ("[{clipboard}]", Some(""), "[]"),
            // One pass: a value that holds a variable stays as it is.
            (
                "{clipboard} {date}",
                Some("{date} {time} {clipboard}"),
                "{date} {time} {clipboard} 2026-11-23",
            ),
            // Other braces stay as typed.
            (
                "{Date} {dat} {date } { date} {} {",
                None,
                "{Date} {dat} {date } { date} {} {",
            ),
            ("{{date}}", None, "{2026-11-23}"),
            ("{da{date}te}", None, "{da2026-11-23te}"),
            ("{clipboard", Some("x"), "{clipboard"),
            ("}{", None, "}{"),
            ("a {unknown} b", None, "a {unknown} b"),
            ("字 {date} 😀", None, "字 2026-11-23 😀"),
        ];
        for (text, clipboard, expected) in cases {
            assert_eq!(expand_text(text, date, time, clipboard), expected, "{text}");
        }
    }

    #[test]
    fn an_unknown_clock_leaves_its_variables_as_typed() {
        assert_eq!(
            expand_text("{date} {time} {clipboard}", None, None, Some("c")),
            "{date} {time} c"
        );
        assert_eq!(
            expand_text("{date} {time}", Some("D"), None, None),
            "D {time}"
        );
        assert_eq!(
            expand_text("{date} {time}", None, Some("T"), None),
            "{date} T"
        );
    }

    #[test]
    fn the_clock_is_written_with_leading_zeros() {
        let now = LocalTime {
            year: 2026,
            month: 11,
            day: 23,
            hour: 14,
            minute: 5,
        };
        assert_eq!(date_text(now), "2026-11-23");
        assert_eq!(time_text(now), "14:05");
        let early = LocalTime {
            year: 987,
            month: 1,
            day: 2,
            hour: 0,
            minute: 0,
        };
        assert_eq!(date_text(early), "0987-01-02");
        assert_eq!(time_text(early), "00:00");
    }

    #[test]
    fn the_clipboard_is_read_only_for_its_variable() {
        assert!(needs_clipboard("{clipboard}"));
        assert!(needs_clipboard("a {clipboard} b {clipboard}"));
        for text in [
            "",
            "clipboard",
            "{Clipboard}",
            "{clipboard",
            "{date} {time}",
            "{ clipboard }",
        ] {
            assert!(!needs_clipboard(text), "{text}");
        }
    }

    #[test]
    fn clipboard_text_cannot_press_return() {
        let copied = "ls\nrm -rf x\r\ny\tz\u{0}\u{1b}[0m\u{7f}\u{85}\u{2028}end\u{2029}";
        assert_eq!(
            expand_text("{clipboard}", None, None, Some(copied)),
            "ls rm -rf x  y z[0m end "
        );
        let out = expand_text("$ {clipboard}", None, None, Some("a\n\n\nb"));
        assert_eq!(out, "$ a   b");
        assert!(!out.chars().any(char::is_control));
        // The text the user typed keeps its own line breaks.
        assert_eq!(
            expand_text("a\n{clipboard}\n", None, None, Some("b\nc")),
            "a\nb c\n"
        );
    }

    #[test]
    fn the_clipboard_is_cut_to_keep_the_whole_within_the_limit() {
        let long = "x".repeat(10_000);
        let out = expand_text("{clipboard}", None, None, Some(&long));
        assert_eq!(units(&out), 4_000);
        let out = expand_text("abc {clipboard} def", None, None, Some(&long));
        assert_eq!(units(&out), 4_000);
        assert!(out.starts_with("abc xxx") && out.ends_with("x def"));
        // Never through the middle of a character.
        let faces = "😀".repeat(3_000);
        let out = expand_text("a{clipboard}", None, None, Some(&faces));
        assert_eq!(out, format!("a{}", "😀".repeat(1_999)));
        assert_eq!(units(&out), 3_999);
        // Every place gets the same share.
        let out = expand_text(
            "{clipboard}|{clipboard}|{clipboard}",
            None,
            None,
            Some(&long),
        );
        assert_eq!(out, [&long[..1_332]; 3].join("|"));
        // The date counts toward the whole.
        let out = expand_text("{date}{clipboard}", Some("2026-11-23"), None, Some(&long));
        assert_eq!(units(&out), 4_000);
        assert!(out.starts_with("2026-11-23x"));
        // A short one isn't cut, and a full text leaves it no room.
        assert_eq!(
            expand_text("{clipboard}", None, None, Some("short")),
            "short"
        );
        let full = format!("{}{{clipboard}}", "a".repeat(4_000 - 11));
        assert_eq!(units(&full), 4_000);
        let out = expand_text(&full, None, None, Some(&long));
        assert_eq!(units(&out), 4_000);
        let over = "{date}".repeat(600);
        let out = expand_text(
            &format!("{over}{{clipboard}}"),
            Some("2026-11-23"),
            None,
            Some(&long),
        );
        assert_eq!(out, "2026-11-23".repeat(600));
    }

    // ----- loading -----

    #[test]
    fn a_file_that_is_not_a_shortcuts_file_is_moved_aside() {
        let cases: [(&[u8], Unreadable); 20] = [
            (b"", Unreadable::NotJson),
            (b"   \n", Unreadable::NotJson),
            (b"not json", Unreadable::NotJson),
            (b"{\"version\":1,\"menus\":[", Unreadable::NotJson),
            (
                b"{\"version\":1,\"menus\":[]} trailing",
                Unreadable::NotJson,
            ),
            (b"\xff\xfe{}", Unreadable::NotJson),
            (b"[]", Unreadable::NotAnObject),
            (b"[{\"version\":1,\"menus\":[]}]", Unreadable::NotAnObject),
            (b"\"version\"", Unreadable::NotAnObject),
            (b"7", Unreadable::NotAnObject),
            (b"null", Unreadable::NotAnObject),
            (b"{}", Unreadable::NoVersion),
            (b"{\"menus\":[]}", Unreadable::NoVersion),
            (b"{\"version\":\"1\",\"menus\":[]}", Unreadable::NoVersion),
            (b"{\"version\":1.5,\"menus\":[]}", Unreadable::NoVersion),
            (b"{\"version\":0,\"menus\":[]}", Unreadable::NoVersion),
            (b"{\"version\":-1,\"menus\":[]}", Unreadable::NoVersion),
            (b"{\"version\":2,\"menus\":[]}", Unreadable::Newer),
            (b"{\"version\":1}", Unreadable::NoMenus),
            (b"{\"version\":1,\"menus\":{}}", Unreadable::NoMenus),
        ];
        for (bytes, why) in cases {
            for platform in [MAC, WIN] {
                let (config, report) = load(bytes, platform);
                let text = String::from_utf8_lossy(bytes);
                assert_eq!(report.unreadable, Some(why), "{text}");
                assert!(report.move_aside() && !report.dropped(), "{text}");
                assert_eq!(report.parse.is_some(), why == Unreadable::NotJson, "{text}");
                assert_eq!(report.note(), Some(STARTED_FRESH));
                assert!(config == Config::defaults(platform), "{text}");
            }
        }
        // A newer file's menus aren't even looked at.
        let newer = json!({"version": 99, "menus": [everywhere(json!([]))], "enabled": true});
        assert_eq!(loaded(&newer).1.unreadable, Some(Unreadable::Newer));
        assert_sentence(STARTED_FRESH);
        assert_sentence(LEFT_OUT);
    }

    #[test]
    fn a_file_over_two_mebibytes_is_moved_aside_unread() {
        let mut bytes = serde_json::to_vec(&file(json!([everywhere(json!([]))]))).unwrap();
        bytes.resize(MAX_LOAD_BYTES, b' ');
        let (config, report) = load(&bytes, MAC);
        assert_eq!(report, clean());
        assert!(config.everywhere().is_empty());
        bytes.push(b' ');
        let (config, report) = load(&bytes, MAC);
        assert_eq!(report.unreadable, Some(Unreadable::TooLarge));
        assert_eq!(report.parse, None);
        assert!(report.move_aside());
        assert!(config == Config::defaults(MAC));
    }

    #[test]
    fn a_parse_problem_is_a_class_and_a_place() {
        let (_, report) = load(b"{\n  \"version\": 1,\n  \"menus\": [}", MAC);
        assert_eq!(
            report.parse,
            Some(ParseAt {
                class: "Syntax",
                line: 3,
                column: 13
            })
        );
        assert_eq!(
            report.to_string(),
            "not JSON (Syntax error at line 3, column 13)"
        );
        let (_, report) = load(b"{\"version\": 1", MAC);
        assert_eq!(
            report.to_string(),
            "not JSON (Eof error at line 1, column 13)"
        );
        let (_, report) = load(b"{\"version\":3,\"menus\":[]}", MAC);
        assert_eq!(report.to_string(), "a newer version");
        for why in [
            Unreadable::TooLarge,
            Unreadable::NotJson,
            Unreadable::NotAnObject,
            Unreadable::NoVersion,
            Unreadable::Newer,
            Unreadable::NoMenus,
        ] {
            assert!(!why.word().is_empty() && why.word().is_ascii());
        }
    }

    #[test]
    fn the_smallest_file_loads_with_the_defaults() {
        let (config, report) = loaded(&json!({"version": 1, "menus": []}));
        assert_eq!(report, clean());
        assert!(!report.dropped() && !report.move_aside() && report.note().is_none());
        assert!(!config.enabled);
        assert_eq!(config.hotkey, Some(DEFAULT_HOTKEY));
        assert_eq!(config.opens_at, OpensAt::Cursor);
        // The Everywhere menu is always there. No starter set: the file is
        // the user's.
        assert_eq!(config.menus.len(), 1);
        assert!(config.menus[0].scope == Scope::Everywhere);
        assert!(config.everywhere().is_empty());
        assert_eq!(config.menus[0].id, 1);
    }

    #[test]
    fn the_top_values_each_fall_back_alone() {
        let with = |key: &str, value: Value| {
            let mut top = file(json!([everywhere(json!([shortcut("Kept", json!([]))]))]));
            top["enabled"] = json!(true);
            top["opensAt"] = json!("center");
            top["hotkey"] =
                json!({"key": "F5", "shift": false, "ctrl": false, "alt": false, "meta": false});
            top[key] = value;
            let (config, report) = loaded(&top);
            assert_eq!(names(config.everywhere()), ["Kept"]);
            (config, report)
        };
        let f5 = wire("F5", false, false, false, false).chord();

        let (config, report) = with("opensAt", json!("center"));
        assert_eq!(report, clean());
        assert!(config.enabled && config.hotkey == f5 && config.opens_at == OpensAt::Center);

        for bad in [json!("corner"), json!(1), json!(null), json!(["cursor"])] {
            let (config, report) = with("opensAt", bad);
            assert_eq!(report, drops(|d| d.settings = 1));
            assert!(config.enabled && config.hotkey == f5 && config.opens_at == OpensAt::Cursor);
            // Nothing a shortcut was left out, so no note; but a save would
            // lose the value, so the file is kept first.
            assert!(report.dropped() && report.note().is_none());
        }
        for bad in [json!("yes"), json!(1), json!(null)] {
            let (config, report) = with("enabled", bad);
            assert_eq!(report, drops(|d| d.settings = 1));
            assert!(!config.enabled && config.hotkey == f5 && config.opens_at == OpensAt::Center);
        }
        // A hot key that can't be one becomes none, and the wheel goes off.
        for bad in [
            json!({"key": "KeyZZ", "shift": false, "ctrl": true, "alt": false, "meta": false}),
            json!({"key": "KeyG", "shift": true, "ctrl": false, "alt": false, "meta": false}),
            json!({"key": "KeyQ", "shift": false, "ctrl": false, "alt": false, "meta": true}),
            json!({"key": "KeyG", "ctrl": true}),
            json!({"key": "KeyG", "shift": false, "ctrl": true, "alt": true, "meta": false, "fn": true}),
            json!("Control+Option+G"),
            json!(7),
        ] {
            let (config, report) = with("hotkey", bad);
            assert_eq!(report.drops.left_out(), 0);
            assert_eq!(report.drops.settings, 2, "the hot key, and on without one");
            assert!(!config.enabled && config.hotkey.is_none());
            assert_eq!(config.opens_at, OpensAt::Center);
        }
        // No hot key by choice is fine when the wheel is off.
        let mut top = file(json!([]));
        top["hotkey"] = json!(null);
        let (config, report) = loaded(&top);
        assert_eq!(report, clean());
        assert!(!config.enabled && config.hotkey.is_none());
        // On with none isn't.
        top["enabled"] = json!(true);
        let (config, report) = loaded(&top);
        assert_eq!(report, drops(|d| d.settings = 1));
        assert!(!config.enabled && config.hotkey.is_none());
    }

    #[test]
    fn a_hot_key_is_judged_by_the_os_that_loads_it() {
        let mut top = file(json!([]));
        top["hotkey"] =
            json!({"key": "KeyQ", "shift": false, "ctrl": false, "alt": false, "meta": true});
        let bytes = serde_json::to_vec(&top).unwrap();
        assert!(load(&bytes, MAC).0.hotkey.is_none());
        assert!(load(&bytes, WIN).0.hotkey.is_some());
    }

    #[test]
    fn a_top_level_key_grape_does_not_know_is_counted() {
        let mut top = file(json!([]));
        top["theme"] = json!(MARKER);
        top["extra"] = json!({"a": 1});
        let (_, report) = loaded(&top);
        assert_eq!(report, drops(|d| d.settings = 2));
        assert!(report.dropped() && report.note().is_none());
    }

    #[test]
    fn a_menu_that_cannot_be_read_is_left_out_alone() {
        let kept = || everywhere(json!([shortcut("Kept", json!([copy_step()]))]));
        let long = "a".repeat(256);
        let bad_menus = [
            json!("everywhere"),
            json!(null),
            json!([]),
            json!({}),
            json!({"scope": {"kind": "everywhere"}, "items": []}),
            json!({"scope": {"kind": "everywhere"}, "withEverywhere": false}),
            json!({"withEverywhere": false, "items": []}),
            json!({"scope": {"kind": "everywhere"}, "withEverywhere": "no", "items": []}),
            json!({"scope": {"kind": "everywhere"}, "withEverywhere": false, "items": {}}),
            json!({"scope": {"kind": "everywhere"}, "withEverywhere": false, "items": [], "color": "red"}),
            json!({"scope": {"kind": "everywhere", "name": "x"}, "withEverywhere": false, "items": []}),
            json!({"scope": {"kind": "window"}, "withEverywhere": false, "items": []}),
            json!({"scope": "everywhere", "withEverywhere": false, "items": []}),
            json!({"scope": {"kind": "app", "name": "App"}, "withEverywhere": false, "items": []}),
            json!({"scope": {"kind": "app", "bundleId": "com.x"}, "withEverywhere": false, "items": []}),
            json!({"scope": {"kind": "app", "bundleId": "", "name": "App"}, "withEverywhere": false, "items": []}),
            json!({"scope": {"kind": "app", "bundleId": "com.x", "name": " "}, "withEverywhere": false, "items": []}),
            json!({"scope": {"kind": "app", "bundleId": "com\n.x", "name": "App"}, "withEverywhere": false, "items": []}),
            json!({"scope": {"kind": "app", "bundleId": long, "name": "App"}, "withEverywhere": false, "items": []}),
            json!({"scope": {"kind": "app", "bundleId": "com.x", "name": "App", "path": "/x"}, "withEverywhere": false, "items": []}),
        ];
        for bad in bad_menus {
            let (config, report) = loaded(&file(json!([
                bad,
                kept(),
                app("com.example.app", json!([]))
            ])));
            assert_eq!(report, drops(|d| d.menus = 1), "{bad}");
            assert_eq!(config.menus.len(), 2, "{bad}");
            assert_eq!(names(config.everywhere()), ["Kept"]);
            assert!(report.dropped());
            assert_eq!(report.note(), Some(LEFT_OUT));
        }
    }

    #[test]
    fn a_second_menu_for_the_same_apps_is_left_out() {
        let first = everywhere(json!([shortcut("First", json!([]))]));
        let second = everywhere(json!([shortcut("Second", json!([]))]));
        let (config, report) = loaded(&file(json!([first, second])));
        assert_eq!(report, drops(|d| d.duplicate_menus = 1));
        assert_eq!(config.menus.len(), 1);
        assert_eq!(names(config.everywhere()), ["First"]);

        let a = app("com.example.a", json!([shortcut("A1", json!([]))]));
        let again = app("com.example.a", json!([shortcut("A2", json!([]))]));
        let b = app("com.example.b", json!([]));
        let (config, report) = loaded(&file(json!([a, b, again])));
        assert_eq!(report, drops(|d| d.duplicate_menus = 1));
        // Everywhere was missing: it's added empty, first.
        assert_eq!(config.menus.len(), 3);
        assert!(config.menus[0].scope == Scope::Everywhere && config.menus[0].items.is_empty());
        assert_eq!(names(&config.menus[1].items), ["A1"]);
        assert_eq!(config.menus[0].id, 1);
        assert_eq!(config.menus[1].id, 2);
    }

    #[test]
    fn menus_keep_their_order_and_everywhere_never_adds_itself() {
        let mut own = everywhere(json!([]));
        own["withEverywhere"] = json!(true);
        let mut with = app("com.example.a", json!([]));
        with["withEverywhere"] = json!(true);
        with["scope"]["name"] = json!("  Padded ");
        let (config, report) = loaded(&file(json!([with, own, app("com.example.b", json!([]))])));
        assert_eq!(report, clean());
        assert_eq!(config.menus.len(), 3);
        assert!(config.menus[0].with_everywhere);
        assert!(
            config.menus[0].scope
                == Scope::App {
                    bundle_id: "com.example.a".to_owned(),
                    name: "Padded".to_owned(),
                }
        );
        assert!(config.menus[1].scope == Scope::Everywhere && !config.menus[1].with_everywhere);
        assert!(!config.menus[2].with_everywhere);
    }

    #[test]
    fn menus_past_forty_are_left_out() {
        let apps = |count: usize| -> Vec<Value> {
            (0..count)
                .map(|n| app(&format!("com.example.app{n}"), json!([])))
                .collect()
        };
        // Everywhere and 39 apps fit, wherever Everywhere is.
        let mut menus = apps(39);
        menus.push(everywhere(json!([shortcut("Kept", json!([]))])));
        let (config, report) = loaded(&file(json!(menus)));
        assert_eq!(report, clean());
        assert_eq!(config.menus.len(), MAX_MENUS);
        // One app more doesn't, and Everywhere still has its place.
        let mut menus = apps(41);
        menus.push(everywhere(json!([shortcut("Kept", json!([]))])));
        let (config, report) = loaded(&file(json!(menus)));
        assert_eq!(report, drops(|d| d.menus_over = 2));
        assert_eq!(config.menus.len(), MAX_MENUS);
        assert_eq!(names(config.everywhere()), ["Kept"]);
        // With none in the file, the empty one makes forty.
        let (config, report) = loaded(&file(json!(apps(40))));
        assert_eq!(report, drops(|d| d.menus_over = 1));
        assert_eq!(config.menus.len(), MAX_MENUS);
        assert!(config.menus[0].scope == Scope::Everywhere);
        // And the app picker's limit agrees.
        let mut config = config;
        let more = |c: &mut Config| add_app_menu(c, "com.example.more", "App", OWN_ID);
        assert_eq!(refused(&mut config, more), MENUS_FULL);
    }

    #[test]
    fn one_bad_step_leaves_its_whole_shortcut_out() {
        let long_text = "a".repeat(4_001);
        let long_script = "a".repeat(20_001);
        let bad_steps = [
            json!({"kind": "teleport"}),
            json!({}),
            json!("delay"),
            json!(7),
            json!(null),
            json!([]),
            json!({"kind": "clipboard", "action": "cut"}),
            json!({"kind": "clipboard", "action": "copy", "extra": 1}),
            json!({"kind": "clipboard"}),
            json!({"kind": "typeText", "text": "a\u{0}b"}),
            json!({"kind": "typeText", "text": "a\rb"}),
            json!({"kind": "typeText", "text": 5}),
            json!({"kind": "typeText", "text": long_text}),
            json!({"kind": "typeText"}),
            json!({"kind": "notify", "text": "a\u{1b}"}),
            json!({"kind": "speak", "text": long_text}),
            json!({"kind": "keys", "chord": {"key": "KeyQQ", "shift": false, "ctrl": false, "alt": false, "meta": false}}),
            json!({"kind": "keys", "chord": {"key": "KeyA", "shift": false, "ctrl": false, "alt": false}}),
            json!({"kind": "keys", "chord": "KeyA"}),
            // The wheel's own hot key.
            json!({"kind": "keys", "chord": default_hotkey()}),
            json!({"kind": "openUrl", "url": "file:///etc/hosts"}),
            json!({"kind": "openUrl", "url": "no scheme"}),
            json!({"kind": "openUrl"}),
            json!({"kind": "appleShortcut", "name": "-x"}),
            json!({"kind": "keyboardMaestro", "name": "a\nb"}),
            json!({"kind": "appleScript", "source": long_script}),
            json!({"kind": "shell", "source": "a\u{1b}[0m"}),
            json!({"kind": "shell", "text": "echo"}),
            json!({"kind": "window", "placement": "upsideDown"}),
            json!({"kind": "window"}),
            json!({"kind": "system", "control": "shutDown"}),
            json!({"kind": "delay", "ms": 9}),
            json!({"kind": "delay", "ms": 60_001}),
            json!({"kind": "delay", "ms": -1}),
            json!({"kind": "delay", "ms": 1.5}),
            json!({"kind": "delay", "ms": "10"}),
            json!({"kind": "openApp", "path": "Safari.app", "bundleId": null, "name": "Safari"}),
            json!({"kind": "openApp", "path": null, "bundleId": "com.apple.Safari", "name": null}),
            json!({"kind": "openApp", "path": null, "bundleId": null, "name": "Safari"}),
            json!({"kind": "openApp", "path": "/Applications/Safari.app", "bundleId": null, "name": null}),
            json!({"kind": "openApp", "path": "/Applications/Safari.app", "bundleId": "", "name": "Safari"}),
            json!({"kind": "openApp", "path": "/Applications/Safari.app", "bundleId": null, "name": "Safari", "args": []}),
            json!({"kind": "openFile", "path": "notes.txt", "folder": false}),
            json!({"kind": "openFile", "path": "/a\nb", "folder": false}),
            json!({"kind": "openFile", "path": null}),
            json!({"kind": "openFile", "path": 7, "folder": false}),
        ];
        for bad in bad_steps {
            for place in 0..3 {
                let mut steps = vec![copy_step(), json!({"kind": "delay", "ms": 250})];
                steps.insert(place, bad.clone());
                let (config, report) = loaded_ring(json!([
                    shortcut("Before", json!([copy_step()])),
                    shortcut("Broken", json!(steps)),
                    shortcut("After", json!([])),
                ]));
                assert_eq!(report, drops(|d| d.for_steps = 1), "{bad}");
                assert_eq!(names(config.everywhere()), ["Before", "After"], "{bad}");
                assert_eq!(report.note(), Some(LEFT_OUT));
            }
        }
    }

    #[test]
    fn every_stored_step_loads() {
        let steps = json!([
            {"kind": "keys", "chord": null},
            {"kind": "keys", "chord": {"key": "KeyK", "shift": true, "ctrl": false, "alt": false, "meta": true}},
            {"kind": "typeText", "text": "Hi {date}\r\n\tthere"},
            {"kind": "clipboard", "action": "clear"},
            {"kind": "openApp", "path": null, "bundleId": null, "name": null},
            {"kind": "openApp", "path": "/Applications/Safari.app", "bundleId": "com.apple.Safari", "name": "Safari"},
            {"kind": "openFile", "path": null, "folder": false},
            {"kind": "openFile", "path": "/Users/max/Invoices", "folder": true},
            {"kind": "openUrl", "url": ""},
            {"kind": "openUrl", "url": "https://example.com"},
            {"kind": "appleShortcut", "name": "Start focus"},
            {"kind": "appleScript", "source": "beep"},
            {"kind": "shell", "source": "echo hi"},
            {"kind": "keyboardMaestro", "name": ""},
            {"kind": "window", "placement": "bottomRight"},
            {"kind": "system", "control": "toggleDarkMode"},
        ]);
        let more = json!([
            {"kind": "notify", "text": "Done"},
            {"kind": "speak", "text": "Done"},
            {"kind": "delay", "ms": 10},
            // A step may leave a `null` value out.
            {"kind": "keys"},
            {"kind": "openApp"},
            {"kind": "openFile", "folder": false},
        ]);
        let (config, report) = loaded_ring(json!([shortcut("All", steps), shortcut("More", more)]));
        assert_eq!(report, clean());
        let all = config.steps();
        assert_eq!(all.len(), 22);
        let mut kinds: Vec<StepKindId> = all.iter().map(|step| step.kind.id()).collect();
        kinds.dedup();
        kinds.sort_by_key(|kind| StepKindId::ALL.iter().position(|other| other == kind));
        kinds.dedup();
        assert_eq!(kinds, StepKindId::ALL);
        // Text is stored as an edit would store it.
        assert!(
            all[2].kind
                == StepKind::TypeText {
                    text: "Hi {date}\n\tthere".to_owned()
                }
        );
        assert!(!all[0].kind.is_set() && all[1].kind.is_set());
        assert!(all[19].kind == StepKind::Keys { chord: None });
        assert!(all[20].kind == StepKind::fresh(StepKindId::OpenApp));
    }

    #[test]
    fn a_shortcut_with_too_many_steps_is_left_out_whole() {
        let steps = |count: usize| json!(vec![copy_step(); count]);
        let (config, report) = loaded_ring(json!([
            shortcut("Sixteen", steps(16)),
            shortcut("Seventeen", steps(17)),
            shortcut("None", steps(0)),
        ]));
        assert_eq!(report, drops(|d| d.for_steps = 1));
        assert_eq!(names(config.everywhere()), ["Sixteen", "None"]);
        assert_eq!(steps_of(&config, config.everywhere()[0].id).len(), 16);
    }

    #[test]
    fn a_shortcut_that_cannot_be_read_is_left_out_alone() {
        let name_25 = "a".repeat(25);
        let bad_items = [
            json!("Copy"),
            json!(null),
            json!([]),
            json!({}),
            json!({"icon": {"kind": "none"}, "kind": "steps", "steps": []}),
            json!({"name": "A", "kind": "steps", "steps": []}),
            json!({"name": "A", "icon": {"kind": "none"}, "steps": []}),
            json!({"name": "A", "icon": {"kind": "none"}, "kind": "steps"}),
            json!({"name": "A", "icon": {"kind": "none"}, "kind": "steps", "items": []}),
            json!({"name": "A", "icon": {"kind": "none"}, "kind": "group", "steps": []}),
            json!({"name": "A", "icon": {"kind": "none"}, "kind": "other", "steps": []}),
            json!({"name": "A", "icon": {"kind": "none"}, "kind": "steps", "steps": [], "id": 4}),
            json!({"name": "A", "icon": {"kind": "none"}, "kind": "steps", "steps": {}}),
            json!({"name": "", "icon": {"kind": "none"}, "kind": "steps", "steps": []}),
            json!({"name": "  ", "icon": {"kind": "none"}, "kind": "steps", "steps": []}),
            json!({"name": name_25, "icon": {"kind": "none"}, "kind": "steps", "steps": []}),
            json!({"name": "a\nb", "icon": {"kind": "none"}, "kind": "steps", "steps": []}),
            json!({"name": 7, "icon": {"kind": "none"}, "kind": "steps", "steps": []}),
            json!({"name": "A", "icon": {"kind": "glyph", "value": "sparkle"}, "kind": "steps", "steps": []}),
            json!({"name": "A", "icon": {"kind": "emoji", "value": "a b"}, "kind": "steps", "steps": []}),
            json!({"name": "A", "icon": {"kind": "emoji", "value": ""}, "kind": "group", "items": []}),
            json!({"name": "A", "icon": {"kind": "emoji"}, "kind": "steps", "steps": []}),
            json!({"name": "A", "icon": "none", "kind": "steps", "steps": []}),
        ];
        for bad in bad_items {
            // In a ring and in a group.
            let (config, report) = loaded_ring(json!([
                shortcut("Before", json!([])),
                bad,
                group(
                    "Group",
                    json!([shortcut("In", json!([])), bad, shortcut("Too", json!([]))])
                ),
            ]));
            assert_eq!(report, drops(|d| d.items = 2), "{bad}");
            assert_eq!(names(config.everywhere()), ["Before", "Group"], "{bad}");
            assert_eq!(
                names(children_of(&config, config.everywhere()[1].id)),
                ["In", "Too"]
            );
        }
    }

    #[test]
    fn names_and_icons_load_trimmed() {
        let (config, report) = loaded_ring(json!([
            {"name": "  Copy ", "icon": {"kind": "emoji", "value": " 💾 "}, "kind": "steps", "steps": []},
        ]));
        assert_eq!(report, clean());
        assert_eq!(config.everywhere()[0].name, "Copy");
        assert!(config.everywhere()[0].icon == emoji("💾"));
    }

    #[test]
    fn a_group_in_a_group_is_left_out() {
        let inner = group("Inner", json!([shortcut("Deep", json!([copy_step()]))]));
        let (config, report) = loaded_ring(json!([
            group(
                "Outer",
                json!([
                    shortcut("First", json!([])),
                    inner,
                    shortcut("Last", json!([]))
                ])
            ),
            shortcut("Beside", json!([])),
        ]));
        assert_eq!(report, drops(|d| d.nested_groups = 1));
        assert_eq!(names(config.everywhere()), ["Outer", "Beside"]);
        assert_eq!(
            names(children_of(&config, config.everywhere()[0].id)),
            ["First", "Last"]
        );
        // An empty group is fine.
        let (config, report) = loaded_ring(json!([group("Empty", json!([]))]));
        assert_eq!(report, clean());
        assert!(children_of(&config, config.everywhere()[0].id).is_empty());
    }

    #[test]
    fn a_ring_and_a_group_load_twelve_at_most() {
        let many = |count: usize| -> Vec<Value> {
            (0..count)
                .map(|n| shortcut(&format!("S{n}"), json!([])))
                .collect()
        };
        let (config, report) = loaded_ring(json!(many(12)));
        assert_eq!(report, clean());
        assert_eq!(config.everywhere().len(), 12);

        let mut ring = many(14);
        ring[0] = group("Group", json!(many(13)));
        // One that can't be read doesn't use up a place.
        ring.insert(1, json!({"name": ""}));
        let (config, report) = loaded_ring(json!(ring));
        assert_eq!(
            report,
            drops(|d| {
                d.items_over = 3;
                d.items = 1
            })
        );
        assert_eq!(config.everywhere().len(), 12);
        assert_eq!(config.everywhere()[11].name, "S11");
        let children = children_of(&config, config.everywhere()[0].id);
        assert_eq!(children.len(), 12);
        assert_eq!(children[11].name, "S11");
    }

    #[test]
    fn loading_numbers_everything_in_reading_order() {
        let (config, report) = loaded(&file(json!([
            everywhere(json!([
                shortcut("A", json!([copy_step(), copy_step()])),
                group("G", json!([shortcut("B", json!([copy_step()]))])),
            ])),
            app("com.example.app", json!([shortcut("C", json!([]))])),
        ])));
        assert_eq!(report, clean());
        let a = &config.everywhere()[0];
        let g = &config.everywhere()[1];
        let b = &children_of(&config, g.id)[0];
        assert_eq!(config.menus[0].id, 1);
        assert_eq!(a.id, 2);
        assert_eq!(
            steps_of(&config, a.id)
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            [3, 4]
        );
        assert_eq!(g.id, 5);
        assert_eq!(b.id, 6);
        assert_eq!(steps_of(&config, b.id)[0].id, 7);
        assert_eq!(config.menus[1].id, 8);
        assert_eq!(config.menus[1].items[0].id, 9);
        // And the next thing made gets the next number.
        let mut config = config;
        assert_eq!(add_item(&mut config, 1, None, NewItem::Steps), Ok(10));
        assert_eq!(add_step(&mut config, 10, StepKindId::Delay), Ok(11));
        assert_eq!(
            add_app_menu(&mut config, "com.example.b", "B", OWN_ID),
            Ok(12)
        );
    }

    #[test]
    fn what_is_saved_loads_back_the_same() {
        let mut config = rich(MARKER);
        set_enabled(&mut config, true).unwrap();
        set_opens_at(&mut config, OpensAt::Center).unwrap();
        let hotkey = wire("Backquote", false, true, false, true);
        set_hotkey(&mut config, Some(&hotkey), MAC).unwrap();
        // Ids that edits left out of order.
        let first = config.everywhere()[0].id;
        move_item(&mut config, first, 2).unwrap();
        let steps: Vec<Id> = steps_of(&config, first).iter().map(|s| s.id).collect();
        move_step(&mut config, steps[0], 5).unwrap();
        remove_step(&mut config, steps[1]).unwrap();

        let bytes = stored_bytes(&config).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
        let (again, report) = load(&bytes, MAC);
        assert_eq!(report, clean());
        assert!(again == renumbered(&config));
        assert!(again != config, "ids aren't stored");
        // And saving that changes nothing.
        assert_eq!(stored_bytes(&again).unwrap(), bytes);
        // No id is in the file.
        let stored: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!stored.to_string().contains("\"id\""));

        for platform in [MAC, WIN] {
            let config = Config::defaults(platform);
            let (again, report) = load(&stored_bytes(&config).unwrap(), platform);
            assert_eq!(report, clean());
            assert!(again == config);
        }
        let mut none = Config::defaults(MAC);
        set_hotkey(&mut none, None, MAC).unwrap();
        let (again, report) = load(&stored_bytes(&none).unwrap(), MAC);
        assert_eq!(report, clean());
        assert!(again == none);
    }

    #[test]
    fn a_change_past_the_file_limit_is_refused() {
        let (mut config, menu) = empty();
        let shell = |source: String| StepEdit::Shell { source };
        let mut id = 0;
        let mut last = Ok(Vec::new());
        'full: for _ in 0..MAX_ITEMS {
            let shortcut = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
            for _ in 0..MAX_STEPS {
                id = add_step(&mut config, shortcut, StepKindId::Shell).unwrap();
                set_step(&mut config, id, &shell("a".repeat(MAX_SCRIPT))).unwrap();
                last = stored_bytes(&config);
                if last.is_err() {
                    break 'full;
                }
            }
        }
        assert_eq!(last, Err(TOO_BIG));
        // Shorten the last script until the file is exactly a mebibyte.
        set_step(&mut config, id, &shell(String::new())).unwrap();
        let room = MAX_FILE_BYTES - stored_bytes(&config).unwrap().len();
        assert!(room < MAX_SCRIPT);
        set_step(&mut config, id, &shell("a".repeat(room))).unwrap();
        assert_eq!(stored_bytes(&config).unwrap().len(), MAX_FILE_BYTES);
        set_step(&mut config, id, &shell("a".repeat(room + 1))).unwrap();
        assert_eq!(stored_bytes(&config), Err(TOO_BIG));
        // Counted in bytes, not units.
        set_step(&mut config, id, &shell("é".repeat(room / 2 + 1))).unwrap();
        assert_eq!(stored_bytes(&config), Err(TOO_BIG));
        assert_sentence(TOO_BIG);
        assert_sentence(SAVE_FAILED);
        assert!(stored_bytes(&Config::defaults(MAC)).unwrap().len() < 2_000);
    }

    // ----- the view -----

    #[test]
    fn the_view_shows_every_step_by_name_only() {
        let config = rich("X");
        let view = json_of(&view(&config, MAC, Binding::Bound, Access::Granted, None));
        let all = &view["menus"][0]["items"][0];
        assert_eq!(all["name"], json!("n X"));
        assert_eq!(all["icon"], json!({"kind": "emoji", "value": "X"}));
        assert_eq!(all["kind"], json!("steps"));
        let first = all["steps"][0]["id"].as_u64().unwrap();
        let expected = json!([
            {"kind": "keys",
             "chord": {"key": "KeyK", "shift": true, "ctrl": false, "alt": false, "meta": true},
             "label": "Shift+Command+K"},
            {"kind": "typeText", "text": "t X {date}\n\tend"},
            {"kind": "clipboard", "action": "paste"},
            {"kind": "openApp", "name": "Target X"},
            {"kind": "openFile", "name": "notes X.txt", "folder": false},
            {"kind": "openUrl", "url": "https://example.com/X"},
            {"kind": "appleShortcut", "name": "s X"},
            {"kind": "appleScript", "source": "display dialog \"X\""},
            {"kind": "shell", "source": "echo X\nexit 0"},
            {"kind": "keyboardMaestro", "name": "k X"},
            {"kind": "window", "placement": "topRight"},
            {"kind": "system", "control": "volumeUp"},
            {"kind": "notify", "text": "n X"},
            {"kind": "speak", "text": "say X"},
            {"kind": "delay", "ms": 250},
        ]);
        let mut expected = expected.as_array().unwrap().clone();
        for (n, step) in expected.iter_mut().enumerate() {
            step["id"] = json!(first + n as u64);
        }
        assert_eq!(all["steps"], json!(expected));

        let group = &view["menus"][0]["items"][1];
        assert_eq!(group["kind"], json!("group"));
        assert_eq!(group["icon"], json!({"kind": "glyph", "value": "star"}));
        assert_eq!(group["items"][0]["name"], json!("c X"));
        assert_eq!(group["items"][0]["kind"], json!("steps"));
        assert!(group.get("steps").is_none());

        let app = &view["menus"][1];
        assert_eq!(app["scope"], json!({"kind": "app", "name": "App X"}));
        assert_eq!(app["withEverywhere"], json!(true));
        assert_eq!(
            app["items"][0]["steps"][0],
            json!({"id": app["items"][0]["steps"][0]["id"], "kind": "openFile", "name": "X", "folder": true})
        );
        assert_eq!(view["binding"], json!("bound"));
        assert_eq!(view["access"], json!("granted"));
    }

    #[test]
    fn the_view_never_carries_a_path_or_an_app_id() {
        // Only the path and the ids carry the marker here.
        let (mut config, menu) = empty();
        add_app_menu(&mut config, &format!("com.{MARKER}"), "App", OWN_ID).unwrap();
        let (_, app) = shortcut_with(&mut config, menu, StepKindId::OpenApp);
        let target = app_target(
            &format!("/{MARKER}/Safari.app"),
            Some(&format!("id.{MARKER}")),
            "Safari",
        );
        set_target(&mut config, app, &target).unwrap();
        let (_, file) = shortcut_with(&mut config, menu, StepKindId::OpenFile);
        set_target(
            &mut config,
            file,
            &file_target(&format!("/Users/{MARKER}/notes.txt"), false),
        )
        .unwrap();
        for platform in [MAC, WIN] {
            let text = line(&view(&config, platform, Binding::Off, Access::Denied, None));
            assert!(!text.contains(MARKER));
            assert!(!text.contains("bundleId") && !text.contains("\"path\""));
            assert!(text.contains(r#""name":"notes.txt""#) && text.contains(r#""name":"Safari""#));
        }
        // Unpicked steps say so.
        let (mut config, menu) = empty();
        shortcut_with(&mut config, menu, StepKindId::OpenApp);
        shortcut_with(&mut config, menu, StepKindId::OpenFile);
        shortcut_with(&mut config, menu, StepKindId::Keys);
        let view = json_of(&view(&config, MAC, Binding::Off, Access::Denied, None));
        let items = &view["menus"][0]["items"];
        assert_eq!(
            items[0]["steps"][0],
            json!({"id": 3, "kind": "openApp", "name": null})
        );
        assert_eq!(
            items[1]["steps"][0],
            json!({"id": 5, "kind": "openFile", "name": null, "folder": false})
        );
        assert_eq!(
            items[2]["steps"][0],
            json!({"id": 7, "kind": "keys", "chord": null, "label": null})
        );
        // A root has no name of its own.
        assert_eq!(display_name(Path::new("/")), "/");
        assert_eq!(display_name(Path::new("/Users/max/Invoices/")), "Invoices");
    }

    #[test]
    fn the_view_names_keys_and_modifiers_for_its_os() {
        let (mut config, menu) = empty();
        let (_, keys) = shortcut_with(&mut config, menu, StepKindId::Keys);
        let all_four = wire("KeyK", true, true, true, true);
        set_step(
            &mut config,
            keys,
            &StepEdit::Keys {
                chord: Some(all_four.clone()),
            },
        )
        .unwrap();
        for (platform, mac) in [(MAC, true), (WIN, false)] {
            let shown = view(
                &config,
                platform,
                Binding::Off,
                Access::NotNeeded,
                Some("A note.".to_owned()),
            );
            assert_eq!(shown.available, mac);
            let names = shown.modifiers;
            // The names are the ones a chord's label is written with.
            let label = all_four.chord().unwrap().label(mac);
            assert_eq!(
                label,
                format!(
                    "{}+{}+{}+{}+K",
                    names.ctrl, names.alt, names.shift, names.meta
                )
            );
            let json = json_of(&shown);
            assert_eq!(
                json["menus"][0]["items"][0]["steps"][0]["label"],
                json!(label)
            );
            assert_eq!(json["hotkey"]["label"], json!(DEFAULT_HOTKEY.label(mac)));
            assert_eq!(json["hotkey"]["chord"], default_hotkey());
            assert_eq!(json["note"], json!("A note."));
            assert_eq!(json["glyphs"], json!(GLYPHS));
            let keys = json["keys"].as_array().unwrap();
            assert_eq!(keys.len(), KeyCode::ALL.len());
            for (shown, key) in keys.iter().zip(KeyCode::ALL) {
                assert_eq!(*shown, json!({"id": key.id(), "label": key.label()}));
            }
        }
        assert_eq!(
            json_of(&modifier_names(true)),
            json!({"shift": "Shift", "ctrl": "Control", "alt": "Option", "meta": "Command"})
        );
        set_hotkey(&mut config, None, MAC).unwrap();
        let json = json_of(&view(&config, MAC, Binding::Off, Access::Denied, None));
        assert_eq!(json["hotkey"], json!(null));
        assert_eq!(json["note"], json!(null));
        assert_eq!(json["enabled"], json!(false));
    }

    // ----- nothing typed is printed -----

    #[test]
    fn debug_never_prints_what_the_user_typed() {
        let config = rich(MARKER);
        let snapshot = Snapshot::of(&config);
        let shortcut = config.everywhere()[0].clone();
        let steps = steps_of(&config, shortcut.id).to_vec();
        let run = ShortcutRun {
            seq: 7,
            name: format!("n {MARKER}"),
            steps: steps.clone(),
            front: Some(FrontApp {
                pid: 501,
                id: Some(format!("com.example.{MARKER}")),
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
        };
        let ring = menu_for(&snapshot, Some(&format!("com.example.{MARKER}")));
        let shown = view(
            &config,
            MAC,
            Binding::Bound,
            Access::Granted,
            Some(MARKER.to_owned()),
        );
        let edits: Vec<StepEdit> = step_fixtures()
            .into_iter()
            .map(|(_, edit)| edit)
            .chain([
                StepEdit::TypeText {
                    text: MARKER.to_owned(),
                },
                StepEdit::Shell {
                    source: MARKER.to_owned(),
                },
                StepEdit::Keys {
                    chord: Some(wire(MARKER, false, false, false, false)),
                },
            ])
            .collect();
        let targets = [
            app_target(&format!("/{MARKER}.app"), Some(MARKER), MARKER),
            file_target(&format!("/{MARKER}"), true),
        ];
        let printed = [
            format!("{config:?}"),
            format!("{config:#?}"),
            format!("{snapshot:?}"),
            format!("{snapshot:#?}"),
            format!("{shortcut:?}"),
            format!("{shortcut:#?}"),
            format!("{:?}", config.everywhere()),
            format!("{:?}", config.menus),
            format!("{steps:?}"),
            format!("{steps:#?}"),
            format!("{run:?}"),
            format!("{run:#?}"),
            format!("{ring:?}"),
            format!("{ring:#?}"),
            format!("{shown:?}"),
            format!("{edits:?}"),
            format!("{targets:?}"),
            format!("{:?}", wire(MARKER, true, true, true, true)),
            format!("{:?}", emoji(MARKER)),
            format!("{:?}", Icon::glyph(MARKER)),
        ];
        // The sample does hold the marker, everywhere a user could put it.
        let stored = String::from_utf8(stored_bytes(&config).unwrap()).unwrap();
        assert!(
            stored.matches(MARKER).count() >= 20,
            "{}",
            stored.matches(MARKER).count()
        );
        for text in &printed {
            assert!(!text.contains(MARKER), "{text}");
            assert!(!text.contains("zq"), "{text}");
        }
        // What they do print is kinds and counts.
        assert_eq!(
            printed[0],
            "Config { enabled: false, hotkey: true, opens_at: Cursor, menus: 2, items: 5, steps: 17 }"
        );
        assert_eq!(
            format!("{:?}", steps[1]),
            format!("Step {{ id: {}, kind: TypeText }}", steps[1].id)
        );
        assert!(
            printed[10].starts_with("ShortcutRun { seq: 7, steps: [Step { id: "),
            "{}",
            printed[10]
        );
        assert!(printed[10].contains("front: Some(FrontApp { pid: 501, .. }), anchor: (400, 300)"));
        assert_eq!(format!("{:?}", edits[0]), "StepEdit(Keys)");
        assert_eq!(format!("{targets:?}"), "[App, File]");
        assert_eq!(
            format!("{:?}", config.menus[1]),
            format!(
                "Menu {{ id: {}, scope: App, with_everywhere: true, items: 1 }}",
                config.menus[1].id
            )
        );
    }

    #[test]
    fn a_load_report_never_repeats_the_file() {
        let secret = format!("{MARKER} secret");
        let bad_name = json!({"name": secret, "icon": {"kind": "none"}, "kind": "steps", "steps": [], "x": secret});
        let top = json!({
            "version": 1,
            "enabled": secret,
            "hotkey": {"key": secret, "shift": false, "ctrl": true, "alt": false, "meta": false},
            "opensAt": secret,
            MARKER: secret,
            "menus": [
                secret,
                {"scope": {"kind": "app", "bundleId": secret, "name": secret}, "withEverywhere": false, "items": [], MARKER: 1},
                everywhere(json!([
                    bad_name,
                    shortcut(&"x".repeat(30), json!([])),
                    shortcut("Steps", json!([{"kind": "shell", "source": format!("{secret}\u{0}")}])),
                    shortcut("Kind", json!([{"kind": secret}])),
                    group("G", json!([group(MARKER, json!([]))])),
                ])),
                everywhere(json!([shortcut(MARKER, json!([]))])),
            ],
        });
        let (config, report) = loaded(&top);
        assert_eq!(
            report.drops,
            Drops {
                settings: 4,
                menus: 2,
                duplicate_menus: 1,
                menus_over: 0,
                items: 2,
                for_steps: 2,
                nested_groups: 1,
                items_over: 0,
            }
        );
        assert_eq!(report.drops.left_out(), 8);
        assert_eq!(names(config.everywhere()), ["G"]);
        let said = format!("{report} {report:?} {report:#?} {:?}", report.note());
        assert!(!said.contains(MARKER) && !said.contains("secret"), "{said}");
        assert_eq!(
            report.to_string(),
            "left out 4 settings, 3 menus (1 twice, 0 past the limit), 5 shortcuts \
             (2 for a step, 1 groups in a group, 0 past the limit)"
        );

        // Broken JSON whose own error message would quote the file.
        let text = format!("{{\"version\": 1, \"menus\": [], \"{MARKER}\": {MARKER}}}");
        let err = serde_json::from_str::<Value>(&text).unwrap_err();
        let (_, report) = load(text.as_bytes(), MAC);
        assert_eq!(report.unreadable, Some(Unreadable::NotJson));
        let said = format!("{report} {report:?}");
        assert!(!said.contains(MARKER), "{said}");
        assert_eq!(
            report.parse.map(|at| (at.line, at.column)),
            Some((err.line(), err.column()))
        );
        let text = format!("\"{MARKER}\"");
        let (_, report) = load(text.as_bytes(), MAC);
        assert!(!format!("{report} {report:?}").contains(MARKER));
    }

    #[test]
    fn no_refusal_repeats_what_was_typed() {
        // Each of these was typed or picked; the sentence back holds none
        // of it.
        let mark = |text: &str| format!("{MARKER}{text}");
        let (mut config, menu) = empty();
        let shortcut = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
        let step_of = |config: &mut Config, kind| add_step(config, shortcut, kind).unwrap();
        let text = step_of(&mut config, StepKindId::TypeText);
        let link = step_of(&mut config, StepKindId::OpenUrl);
        let named = step_of(&mut config, StepKindId::AppleShortcut);
        let script = step_of(&mut config, StepKindId::Shell);
        let keys = step_of(&mut config, StepKindId::Keys);
        let file = step_of(&mut config, StepKindId::OpenFile);
        let app = step_of(&mut config, StepKindId::OpenApp);
        let edits = [
            (
                text,
                StepEdit::TypeText {
                    text: mark("\u{0}"),
                },
            ),
            (
                text,
                StepEdit::TypeText {
                    text: mark(&"a".repeat(4_000)),
                },
            ),
            (link, StepEdit::OpenUrl { url: mark(" x") }),
            (
                link,
                StepEdit::OpenUrl {
                    url: format!("file:///{MARKER}"),
                },
            ),
            (
                link,
                StepEdit::OpenUrl {
                    url: format!("https://{}", mark(&"a".repeat(2_048))),
                },
            ),
            (
                named,
                StepEdit::AppleShortcut {
                    name: format!("-{MARKER}"),
                },
            ),
            (
                script,
                StepEdit::Shell {
                    source: mark(&"a".repeat(20_000)),
                },
            ),
            (
                keys,
                StepEdit::Keys {
                    chord: Some(wire(MARKER, false, false, false, false)),
                },
            ),
            (
                keys,
                StepEdit::TypeText {
                    text: MARKER.to_owned(),
                },
            ),
        ];
        for (id, edit) in &edits {
            refused(&mut config, |c| set_step(c, *id, edit));
        }
        refused(&mut config, |c| {
            set_item(c, shortcut, &mark("\nx"), &Icon::None)
        });
        refused(&mut config, |c| {
            set_item(c, shortcut, &mark(&"a".repeat(24)), &Icon::None)
        });
        refused(&mut config, |c| {
            set_item(c, shortcut, "Name", &emoji(&mark(" x")))
        });
        refused(&mut config, |c| {
            set_item(c, shortcut, "Name", &Icon::glyph(MARKER))
        });
        refused(&mut config, |c| {
            add_app_menu(c, &mark("\n"), MARKER, OWN_ID)
        });
        refused(&mut config, |c| add_app_menu(c, OWN_ID, MARKER, OWN_ID));
        refused(&mut config, |c| {
            set_target(c, file, &file_target(MARKER, false))
        });
        refused(&mut config, |c| {
            set_target(c, app, &app_target(MARKER, Some(MARKER), MARKER))
        });
        refused(&mut config, |c| {
            set_hotkey(c, Some(&wire(MARKER, false, true, false, false)), MAC)
        });
        // Every sentence there is passes the same check.
        for why in [
            UNAVAILABLE,
            TOO_BIG,
            SAVE_FAILED,
            LEFT_OUT,
            STARTED_FRESH,
            EVERYWHERE_DID_NOT_FIT,
            NO_HOTKEY,
            UNKNOWN_KEY,
            HOTKEY_NEEDS_MODIFIER_MAC,
            HOTKEY_NEEDS_MODIFIER,
            HOTKEY_RESERVED,
            HOTKEY_IN_STEP,
            MENU_GONE,
            ITEM_GONE,
            GROUP_GONE,
            STEP_GONE,
            APP_HAS_MENU,
            APP_IS_GRAPE,
            APP_UNUSABLE,
            MENUS_FULL,
            EVERYWHERE_STAYS,
            EVERYWHERE_SWITCH,
            RING_FULL,
            GROUP_IN_GROUP,
            NAME_LENGTH,
            NAME_CONTROLS,
            ICON_EMOJI,
            ICON_GLYPH,
            CANT_MOVE,
            GROUP_NO_STEPS,
            STEPS_FULL,
            KIND_MISMATCH,
            KEYS_IS_HOTKEY,
            TEXT_TOO_LONG,
            TEXT_CONTROLS,
            SCRIPT_TOO_LONG,
            URL_TOO_LONG,
            URL_SHAPE,
            URL_SCHEME,
            TARGET_NAME,
            DELAY_RANGE,
            TARGET_UNUSABLE,
        ] {
            assert_sentence(why);
        }
    }
}
