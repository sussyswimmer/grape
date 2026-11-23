# M10 design: the Shortcuts wheel

Status: design for `DECISIONS.md` D47. macOS first; Windows gets stubs now and the real thing later. Nothing here has run on a Mac.

This note is the contract the code is written against, the way `m1-drag-wheel.md` is for the drag wheels. Behavior lives in `INTERACTION_SPEC.md` §12; what was learned from the reference app lives in `RADIAL_FINDINGS.md`. Where this note and those disagree, they win and this note gets fixed.

## 1. What it is

A fourth wheel that needs no drag. The user presses a global hotkey, the wheel opens at the cursor, and each segment is a **shortcut** the user made in Settings. A shortcut is a name, an icon and one or more **steps** (press a key combination, type text, open something, run a script, move a window, …), or it is a **group** that opens a second ring of shortcuts. Which **menu** shows depends on the app in front: an app's own menu if it has one, otherwise the **Everywhere** menu.

Words: *Shortcuts wheel*, *shortcut*, *step*, *group*, *menu*, *Everywhere*. These are Grape's words. Don't borrow the reference app's names for things.

Out of scope for this change (listed in ROADMAP M10 as unchecked items): AI steps and an online preset library (both need the network: golden rule 3), opening the wheel from a hot corner, a shake, a mouse button or a bare modifier, an app switcher, visual effects, appearance options, preset files, media keys, and everything on Windows.

## 2. Where the code goes

| Piece | Where | Notes |
|---|---|---|
| Keys and chords (pure) | `crates/grape-platform/src/keys.rs` | `KeyCode`, `Chord`. No serde (the crate has none). |
| Portable entry points | `crates/grape-platform/src/shortcuts.rs` | Same shape as `file_actions.rs`: `pub fn` → `imp::`. |
| macOS | `crates/grape-platform/src/macos/{hotkey.rs,input.rs,shortcuts.rs}` | Carbon hot keys, CGEvent posting, AX window moves, pickers. |
| macOS pure rules | `crates/grape-platform/src/mac_rules.rs` | Key code table, Carbon and CGEvent flag masks, program plans. Tested on every OS. |
| Windows, other | `windows/mod.rs`, `unsupported.rs` | Every new `imp::` function returns `PlatformError::Unsupported`. |
| Scripted twins | `crates/grape-platform/src/mock.rs` | Hotkey and pointer scripts for the controller's tests. |
| Model (pure) | `apps/desktop/src-tauri/src/shortcuts/model.rs` | Menus, shortcuts, steps, limits, validation, menu choice, text variables. |
| File | `apps/desktop/src-tauri/src/shortcuts/store.rs` | `shortcuts.json`: load (total), save (atomic), move aside. |
| Wheel session (pure) | `apps/desktop/src-tauri/src/shortcuts/session.rs` | The state machine in §5. Driven by `controller.rs`. |
| Runner | `apps/desktop/src-tauri/src/shortcuts/run.rs` | Runs steps in order behind an `Actuator` trait. |
| Commands, state, wiring | `apps/desktop/src-tauri/src/shortcuts/mod.rs` | `ShortcutsState`, the `shortcuts_*` commands, the hotkey binding. |
| Overlay | `apps/desktop/ui/src/wheel/*`, `shared/events.ts` | A new content kind. Rust still owns the hit test. |
| Settings | `apps/desktop/ui/src/settings/shortcuts-{model,view}.ts` | A new section in the Settings window. |

`grape-core` doesn't change. The wheel's geometry and hit test are reused as they are: they only take a count.

## 3. Keys (`grape-platform::keys`)

```rust
/// A key that isn't a modifier, named by its W3C `KeyboardEvent.code`.
pub enum KeyCode { KeyA..KeyZ, Digit0..Digit9, F1..F20, Space, Enter, Tab, Escape,
    Backspace, Delete, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, Home, End, PageUp, PageDown,
    Minus, Equal, BracketLeft, BracketRight, Backslash, Semicolon, Quote, Comma, Period, Slash, Backquote }

impl KeyCode {
    pub const ALL: &'static [KeyCode];
    pub fn id(self) -> &'static str;              // "KeyA", "Digit4", "F5", "ArrowUp"
    pub fn from_id(id: &str) -> Option<KeyCode>;  // exact match only
    pub fn label(self) -> &'static str;           // "A", "4", "F5", "↑", "Space", "Return"…
}

/// A key with the modifiers held with it.
pub struct Chord { pub key: KeyCode, pub mods: Modifiers }

pub enum HotkeyProblem { NeedsModifier, Reserved }
impl Chord {
    /// Why this chord can't open the wheel, if it can't.
    pub fn hotkey_problem(self, mac: bool) -> Option<HotkeyProblem>;
    /// "Control+Option+Space" on the Mac, "Ctrl+Alt+Space" elsewhere. Order: ctrl, alt, shift, meta, key.
    pub fn label(self, mac: bool) -> String;
}
```

- A hotkey needs Control, Option/Alt or Command/Meta, unless its key is F1–F20. Shift alone isn't enough (it would eat capital letters).
- Reserved on the Mac (`mac_rules::reserved_hotkey`): Command with Q, W, Tab, Space, C, V, X, A, Z or S, with or without Shift. They belong to every app.
- A **Keys step** may be any chord, modifiers or none.
- The default hotkey is Control+Option+Space. The feature is **off** until the user turns it on, so an update never grabs a key by itself.

## 4. Platform entry points (`grape-platform::shortcuts`)

All return `Result<_, PlatformError>` unless noted. Off macOS every one is `Unsupported` (and `input_access()` is `NotNeeded`).

```rust
pub enum HotkeyId { Open, Escape, Confirm }
pub struct FrontApp { pub pid: i32, pub id: String }        // bundle id; never logged
pub struct PressContext { pub cursor: Option<(i32, i32)>, pub front: Option<FrontApp> }
pub enum HotkeySignal { Pressed(HotkeyId, PressContext), Released(HotkeyId) }

pub trait HotkeyMonitor: Send {
    fn start(&mut self, tx: SyncSender<HotkeySignal>) -> Result<(), PlatformError>;
    /// Bind `id` to `chord`, or unbind it with `None`. Rebinding replaces.
    fn bind(&mut self, id: HotkeyId, chord: Option<Chord>) -> Result<(), PlatformError>;
    fn stop(&mut self);
}
pub fn hotkey_monitor() -> Result<Box<dyn HotkeyMonitor>, PlatformError>;

pub enum PointerSignal { Moved { x: i32, y: i32 }, Down { x: i32, y: i32 }, Up { x: i32, y: i32 } }
pub trait PointerWatch: Send {
    fn start(&mut self, tx: SyncSender<PointerSignal>) -> Result<(), PlatformError>;
    /// Report the pointer and the primary button while on. Off costs nothing.
    fn set_active(&mut self, on: bool);
    fn stop(&mut self);
}
pub fn pointer_watch() -> Result<Box<dyn PointerWatch>, PlatformError>;

pub enum InputAccess { Granted, Denied, NotNeeded }
pub fn input_access() -> InputAccess;                 // infallible
pub fn request_input_access() -> Result<(), PlatformError>;   // the system's own prompt, then its settings pane
pub fn held_modifiers() -> Modifiers;                 // infallible; NONE where unknown

pub fn send_chord(chord: Chord) -> Result<(), PlatformError>;
pub fn type_text(text: &str) -> Result<(), PlatformError>;
pub fn clipboard_text() -> Result<Option<String>, PlatformError>;
pub fn clear_clipboard() -> Result<(), PlatformError>;

pub enum OpenTarget<'a> { App { path: &'a Path, id: Option<&'a str> }, Path(&'a Path), Url(&'a str) }
pub fn open_target(target: OpenTarget<'_>) -> Result<(), PlatformError>;

pub struct Program { pub program: PathBuf, pub args: Vec<OsString> }
pub enum ProgramRequest<'a> { AppleScript(&'a str), Shell(&'a str), AppleShortcut(&'a str), KeyboardMaestro(&'a str), Speak(&'a str) }
/// The program that does this here. The app runs it; nothing goes through a shell except `Shell` itself.
pub fn program_for(request: ProgramRequest<'_>) -> Result<Program, PlatformError>;

pub enum SystemControl { SleepDisplay, LockScreen, Screensaver, MissionControl, ToggleMute, VolumeUp, VolumeDown, ToggleDarkMode, ScreenshotArea }
pub enum SystemPlan { Program(Program), Chord(Chord) }
pub fn system_plan(control: SystemControl) -> Result<SystemPlan, PlatformError>;

pub enum Placement { LeftHalf, RightHalf, TopHalf, BottomHalf, Fill, Center, TopLeft, TopRight, BottomLeft, BottomRight }
/// Pure: where `placement` puts a window of `size` on a display's `work_area`.
pub fn placement_frame(placement: Placement, work_area: Rect, size: (i32, i32)) -> Rect;
/// Move the focused window of `pid` (needs input access).
pub fn place_window(pid: i32, placement: Placement) -> Result<(), PlatformError>;

/// Bring `pid` to the front if Grape took the front, and wait up to `timeout` for it. True when it is in front.
pub fn ensure_front(pid: i32, timeout: Duration) -> bool;

pub enum PickKind { App, File, Folder }
pub struct Picked { pub path: PathBuf, pub app: Option<AppIdentity> }
pub struct AppIdentity { pub id: String, pub name: String }
pub fn pick_target(kind: PickKind, owner: Option<isize>) -> Result<Option<Picked>, PlatformError>;
```

### macOS notes for the implementer

- **Hot keys: Carbon** (`RegisterEventHotKey`), declared by hand in `macos/hotkey.rs` with `#[link(name = "Carbon", kind = "framework")]`. No new crate, no permission. One `InstallEventHandler` on `GetApplicationEventTarget()` for class `'keyb'` kinds 5 (pressed) and 6 (released); read the id with `GetEventParameter(event, '----', 'hkid', …)`. Everything Carbon runs on the main thread (`dispatch2`), and the handler only reads the cursor and the front app and `try_send`s. Carbon modifier masks: Command `0x0100`, Shift `0x0200`, Option `0x0800`, Control `0x1000`. Register failure (the chord is taken) → `PlatformError::Os`.
- **Escape and Confirm** are hot keys too (Escape `53`, Return `36`, no modifiers), bound only while a Shortcuts wheel is open. That is how Grape reads Esc without a key monitor. While bound they are Grape's: the app in front doesn't see them.
- **The front app** is read in the pressed handler, on the main thread, before anything of Grape's shows: `NSWorkspace.frontmostApplication` → pid and `bundleIdentifier`. Grape itself in front → `None`.
- **The pointer** comes from polling `NSEvent.mouseLocation` and `NSEvent.pressedMouseButtons` on a `"grape-pointer"` thread every 8 ms while active (the drag poller's own trick; no permission), flipped with `mac_rules::cursor_to_grape`.
- **Input access** is Accessibility: `AXIsProcessTrusted` / `AXIsProcessTrustedWithOptions` with `kAXTrustedCheckOptionPrompt`, declared by hand against `ApplicationServices`. `request_input_access` shows the system prompt and then opens `x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility` with `/usr/bin/open`.
- **Posting keys:** `objc2-core-graphics`' `CGEvent` (features added to the manifest, no new package): key down and key up with the chord's flags, posted at the HID tap. Flag masks: Shift `0x20000`, Control `0x40000`, Option `0x80000`, Command `0x100000`. `type_text` sends the text 20 UTF-16 units at a time with `CGEventKeyboardSetUnicodeString`; `\n` is Return and `\t` is Tab.
- **Windows of other apps:** `AXUIElementCreateApplication(pid)` → `"AXFocusedWindow"` → read `"AXPosition"`/`"AXSize"` → `placement_frame` on the display holding the window's center → set both. AX points have a top-left origin, the same as Grape's.
- **Programs** (`mac_rules`, each a pure builder with tests): AppleScript `/usr/bin/osascript -e <source>`; Shell `/bin/zsh -c <source>`; Apple Shortcut `/usr/bin/shortcuts run <name>`; Keyboard Maestro `/usr/bin/osascript -e 'tell application "Keyboard Maestro Engine" to do script "<name, escaped>"'`; Speak `/usr/bin/say <text>` (a text starting with `-` gets a leading space). Open: `/usr/bin/open -b <id>` or `-a <path>`, `/usr/bin/open <path>`, `/usr/bin/open <url>`.
- **System controls:** Sleep display `/usr/bin/pmset displaysleepnow`; Screensaver `open -a ScreenSaverEngine`; Mission Control `open -a "Mission Control"`; mute and volume through one-line `osascript` (`set volume …`); dark mode through `System Events`; Lock screen is the chord Control+Command+Q; Screenshot area is Command+Shift+4.
- **`ensure_front`:** if Grape is active (a click on the overlay activates an accessory app), hand activation back with the existing `hand_back`, then poll `frontmostApplication` every 10 ms until it is `pid` or the time is up.
- **Pickers:** `NSOpenPanel`, as `pick_folder` does. Apps start in `/Applications` and allow only `.app`; the identity is `NSBundle`'s `bundleIdentifier` and the bundle's display name.
- `Info.plist` gains `NSAppleEventsUsageDescription` (AppleScript steps that talk to other apps), in Grape's words.

## 5. The wheel session (`shortcuts/session.rs`, driven by `controller.rs`)

New controller inputs: `Input::Hotkey(HotkeySignal)`, `Input::Pointer(PointerSignal)`, `Input::Shortcuts(Arc<Snapshot>)` (the enabled flag, placement and menus, sent at start and after every change).

New effects: `Effect::Track(bool)` (start or stop the pointer watch, and bind or unbind Escape and Confirm), `Effect::GiveBack(Option<i32>)` (after the overlay hides: hand the front back to this pid if Grape took it), `Effect::RunShortcut(ShortcutRun)`. `Open`, `ShowOverlay`, `Highlight`, `Close` and `HideOverlay` are reused, so the tick and the haptic come for free.

New state: `State::Shortcuts(…)`, holding `seq`, the display, the anchor, the front app, the trail of open groups, the current ring's items, the highlight, `sticky`, whether a button-down was seen, the press time and the last event time.

| In state | Input | Result |
|---|---|---|
| Idle, on, not paused | `Pressed(Open, ctx)` with a cursor and a display | choose the menu for `ctx.front`; anchor = the cursor (or the work area's center), clamped; `Track(true)`, `Open`, `ShowOverlay`; → Shortcuts, not sticky |
| Idle | `Pressed(Open, …)` while off, paused, or with no cursor or display | nothing (a log line) |
| Dragging, Open, Dismissed | any hotkey or pointer input | ignored: a drag always wins |
| Shortcuts | `Pointer::Moved` | hit test → `Highlight` on change |
| Shortcuts | `Pointer::Down` | remember that a press was seen |
| Shortcuts | `Pointer::Up` after a seen press | on a segment → **pick** it; inside the dead zone → back one group, or cancel at the root; anywhere else → cancel |
| Shortcuts, not sticky | `Released(Open)` | highlight → **pick** it; no highlight and under `TAP` (300 ms) since the press → sticky; otherwise cancel |
| Shortcuts, sticky | `Pressed(Open, …)` | cancel |
| Shortcuts | `Pressed(Confirm, …)` | highlight → **pick** it; otherwise nothing |
| Shortcuts | `Pressed(Escape, …)` | close, reason `escape` |
| Shortcuts | `DragSignal::Started` | cancel, then handle the drag as from Idle |
| Shortcuts | `SetPaused(true)` | close, reason `paused` |
| Shortcuts | 30 s with no input | close, reason `watchdog` |

- **Pick, a shortcut with steps:** `Close { reason: picked }`, `HideOverlay`, `Track(false)`, `GiveBack(front pid)`, `RunShortcut`. → Idle.
- **Pick, a group:** push it on the trail, redraw with the same `seq` (an update: no animation, highlight cleared), become sticky.
- **Cancel / close:** `Close`, `HideOverlay`, `Track(false)`, `GiveBack(front pid)`. → Idle.
- **An empty menu** opens with no segments and shows the "nothing here yet" message. It closes like any other.
- A shortcut edit while a wheel is open doesn't redraw it (as `Configure` doesn't).
- Hit testing is `grape_core::hit_test` on logical offsets from the anchor, exactly as for drags.

`ShortcutRun { seq, name, steps, front: Option<FrontApp>, anchor, display }`.

## 6. The overlay contract

`wheel://open` keeps its keys. `mode` gains `"shortcuts"` (an app-side enum; `grape-core`'s `WheelMode` is untouched). `content` gains one kind:

```json
{"kind":"shortcuts","dense":false,"trail":["Edit"],"segments":[
  {"index":0,"center":1.5707963267948966,"start":3.141592653589793,"end":0.0,"labelX":0.0,"labelY":-84.0,
   "title":"Save","icon":{"kind":"emoji","value":"💾"},"group":false}]}
```

- `icon` is `{"kind":"none"}`, `{"kind":"emoji","value":"…"}` or `{"kind":"glyph","value":"<id>"}`. Glyph ids come from one list shared by Rust and the UI (`GLYPHS`): Grape's own 24×24 stroke drawings.
- `segments` may be empty: the page then shows the empty message. `trail` is the open groups' names, outermost first.
- `title` is the shortcut's name as typed (1–24 characters; Rust enforces it). The page fits it with the existing `wrapLines`.
- `wheel://close` gains the reason `"picked"`.
- The page draws the icon above the label, a small mark on group segments, and, in a group with nothing hovered, the group's name in the center. It handles no input: Rust reads the pointer.
- New fixtures: `wheel-open-shortcuts.json`, `wheel-open-shortcuts-empty.json`, `wheel-close-picked.json`.

## 7. The model and `shortcuts.json`

Limits: 12 shortcuts in a ring (`MAX_ITEMS`), groups hold shortcuts with steps only (two levels), 16 steps (`MAX_STEPS`), names 1–24 characters, text fields 4 000 characters, scripts 20 000, URLs 2 048, a delay 10 ms–60 s, 40 menus.

```json
{"version":1,"enabled":false,"hotkey":{"key":"Space","shift":false,"ctrl":true,"alt":true,"meta":false},
 "placement":"cursor","nextId":20,
 "menus":[{"id":1,"scope":{"kind":"everywhere"},"withEverywhere":false,"items":[
   {"id":2,"name":"Copy","icon":{"kind":"glyph","value":"clipboard"},"kind":"steps","steps":[
     {"id":3,"kind":"keys","chord":{"key":"KeyC","shift":false,"ctrl":false,"alt":false,"meta":true}}]}]}]}
```

Step kinds (stored form): `keys {chord|null}`, `typeText {text}`, `clipboard {action: copy|paste|clear}`, `openApp {path|null, bundleId|null, name|null}`, `openFile {path|null, folder}`, `openUrl {url}`, `appleShortcut {name}`, `appleScript {source}`, `shell {source}`, `keyboardMaestro {name}`, `window {placement}`, `system {control}`, `notify {text}`, `speak {text}`, `delay {ms}`.

- **Menu choice:** the menu whose scope is the front app's id, else Everywhere. An app menu with `withEverywhere` on ends with one extra group, "Everywhere", holding the Everywhere menu's shortcuts (its groups flattened away: two levels only); it is left out when that would pass 12.
- **Text variables** in `typeText`: `{date}` (2026-11-23), `{time}` (14:05), `{clipboard}`. Unknown braces stay as typed. Expansion is a pure function of the text and the three values.
- **URLs** must be `scheme:rest` with a lowercase scheme; `file:`, `javascript:` and `data:` are refused (use the file step); no control characters.
- **Starter set:** the first time the file is made, Everywhere gets Copy, Paste, Undo, Screenshot, Mute and Lock screen, so the first press shows a working wheel.
- **Loading is total.** A missing file → defaults. A file that isn't this shape, or has a newer `version` → moved aside as `shortcuts.unreadable.json` (the settings file's rule) and defaults. A bad shortcut or step inside a good file is dropped by itself, and the log counts what was dropped by kind. Nothing a user typed is ever logged: not names, text, scripts, URLs or paths.
- **Saving** writes `shortcuts.json.tmp` and renames it over the file, after every change. A failed save still applies the change and says "Couldn't save your shortcuts."
- The file is its own, not a key in `settings.json`: a broken one never takes the other with it.

## 8. Settings commands and view

All granted to the Settings window only, each answering `Result<ShortcutsView, String>`. The page never sends a path: apps, files and folders are picked on the Rust side. It does send what the user typed (names, text, scripts, URLs); D47 says so.

`shortcuts_get {}`, `shortcuts_set_enabled {on}`, `shortcuts_set_hotkey {chord|null}`, `shortcuts_set_placement {placement}`, `shortcuts_request_access {}`, `shortcuts_add_menu {}` (opens the app picker), `shortcuts_remove_menu {menu}`, `shortcuts_set_menu_everywhere {menu, on}`, `shortcuts_add_item {menu, group|null, kind}`, `shortcuts_set_item {menu, item, name, icon}`, `shortcuts_remove_item {menu, item}`, `shortcuts_move_item {menu, item, to}`, `shortcuts_add_step {menu, item, kind}`, `shortcuts_set_step {menu, item, step, value}`, `shortcuts_remove_step {menu, item, step}`, `shortcuts_move_step {menu, item, step, to}`, `shortcuts_pick_target {menu, item, step}`.

```json
{"available":true,"enabled":false,
 "hotkey":{"chord":{"key":"Space","shift":false,"ctrl":true,"alt":true,"meta":false},"label":"Control+Option+Space"},
 "access":"denied","placement":"cursor","note":null,
 "limits":{"items":12,"steps":16,"name":24,"text":4000,"script":20000,"url":2048,"menus":40},
 "glyphs":["key","text", "…"],
 "menus":[{"id":1,"scope":{"kind":"everywhere"},"withEverywhere":false,"items":[
   {"id":2,"name":"Copy","icon":{"kind":"glyph","value":"clipboard"},"kind":"steps","steps":[
     {"id":3,"kind":"keys","chord":{"key":"KeyC","shift":false,"ctrl":false,"alt":false,"meta":true},"label":"Command+C"}]}]}]}
```

- `hotkey` is `null` when none is set. `access` is `"granted"`, `"denied"` or `"notNeeded"`.
- An app menu's scope is `{"kind":"app","name":"Safari"}`: the name only. `openApp` and `openFile` steps carry `name` (the display name, or `null` until picked) and never a path.
- `set_step`'s `value` carries only that kind's typed fields (`chord`, `text`, `action`, `url`, `name`, `source`, `placement`, `control`, `ms`). It can't change a step's kind or a picked target.
- On Windows for now `available` is `false`, and the section says the wheel is on the Mac first.
- Fixtures: `shortcuts-view-macos.json`, `shortcuts-view-windows.json`.

## 9. Running a shortcut (`shortcuts/run.rs`)

- One run at a time, on a `"grape-shortcut"` thread. A pick while one is running shows "Still running {name}" and runs nothing.
- Steps run in order. The first failure stops the run and shows an error toast near the wheel's anchor: "Couldn't finish {name}" with one plain sentence.
- Before the first step that presses keys, types, pastes or moves a window: check `input_access()` (denied → stop, ask once through `request_input_access`, and say "Grape needs Accessibility access to press keys. Turn it on in System Settings, then try again."), wait up to 500 ms for the hotkey's own modifiers to be let go (`held_modifiers`), and `ensure_front(front pid, 400 ms)`.
- `clipboard copy` is Command+C, `paste` is Command+V, `clear` empties the pasteboard. `notify` is Grape's own toast. `delay` sleeps in 20 ms slices so a cancel is quick.
- Programs run with no shell (except the Shell step's own), stdin, stdout and stderr closed, in the home folder, for at most 60 s. A non-zero exit is a failure. Output is never read or logged.
- Pause and Quit cancel the run between steps and kill a running program.
- A finished run shows nothing unless a step says so.
- The log says which step *kinds* ran and how long, never their contents.

## 10. Order of work

1. `grape-platform`: `keys.rs`, `shortcuts.rs`, stubs, mock scripts, `mac_rules` tables (compiles on every OS, tests on Linux).
2. In parallel: the macOS implementation; the app's `model.rs` and `store.rs`; the event types and fixtures.
3. The session, the controller glue, the runner, the commands and capabilities.
4. The overlay drawing and the Settings section.
5. Docs: D47, `RADIAL_FINDINGS.md`, `INTERACTION_SPEC.md` §12, `ARCHITECTURE.md`, `PLATFORM_MACOS.md`, `TESTING.md`, `ROADMAP.md` M10, `PRODUCT.md`, `README.md`, the CLAUDE.md files.

Checked from Linux: `cargo test` for the pure parts, the Mac and Windows clippy cross-checks in `START_HERE.md`, the UI's typecheck and tests. CI's macOS jobs compile and link the Carbon and AX declarations. Everything a person has to see gets a `mac:` line in `TESTING.md`.
